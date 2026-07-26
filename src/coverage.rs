//! Parse LLVM coverage data into the source-line ranges a single test touched.
//!
//! The work is split across two inputs joined by mangled function name, because
//! only one of them varies per test:
//!
//! 1. **The binary's coverage map** — where each instrumented function lives in
//!    the source. Fixed by the binary, so [`build_function_map`] derives it
//!    once per binary per collect from `llvm-cov export`.
//! 2. **One test's profile** — which functions that test executed.
//!    [`executed_functions`] reads it from `llvm-profdata merge --text`, which
//!    lists only the functions the test actually reached.
//!
//! [`hit_ranges`] joins the two. Splitting them this way is what keeps collect
//! affordable: `llvm-cov export` describes the *whole* binary — 34 MB of JSON
//! for a 63,000-function test binary — and costs the same whether the test hit
//! two functions or two thousand. Running it per test meant regenerating and
//! reparsing that description for every one of a suite's thousands of tests.
//! The per-test profile is ~400 KB and lists exactly the hit functions.
//!
//! ## What the split costs in precision
//!
//! Per the LLVM coverage exporter, `data[i].functions[j]` looks like:
//!
//! ```json
//! {
//!   "name": "_RNvMNtCs…",
//!   "count": <total execution count for the function>,
//!   "filenames": ["/abs/path/to/source.rs", ...],
//!   "regions": [
//!     [line_start, col_start, line_end, col_end, count, file_id, ...],
//!     ...
//!   ]
//! }
//! ```
//!
//! A per-test export can narrow a function to the extent its *executed*
//! regions cover: `min(line_start)` / `max(line_end)` across regions with
//! `count > 0`. The per-binary map has no counts, so it can only offer each
//! function's full extent across all its regions.
//!
//! Those are almost always the same span. rustc emits a region for the
//! function's signature line and one for its closing brace, both carrying the
//! function's entry counter, so a function that ran at all normally ran the
//! two regions that set the min and the max.
//!
//! The exception is a function that *terminates* rather than returns — one
//! ending in a failing `assert!`, a `panic!`, a `process::exit`. Its closing
//! brace never executes, so a per-test export stops the extent at the last
//! line the test reached while the map carries it to the end of the function.
//! Measured across worktrunk's 1,407 unit tests: 2 of 34,175 stored ranges
//! widen, by 7 source lines in total, and a change to any of those lines
//! selects exactly one extra test — the `#[should_panic]` test for the
//! function that widened.
//!
//! So the map is a *superset*, never a subset: a full extent can only be wider
//! than an executed extent, and a wider range overlaps more diff hunks. The
//! direction is what matters — over-selection costs a test run, under-selection
//! silently skips the test that would have caught the regression.
//!
//! Note that `--ignore-filename-regex` shrinks `data[].files[]` but leaves
//! `data[].functions[]` untouched, so [`build_function_map`] applies the
//! project-root filter itself using `strip_prefix`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{bail, Context, Result};
use camino::Utf8PathBuf;
use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
pub struct CoverageExport {
    pub data: Vec<CoverageData>,
}

#[derive(Deserialize)]
pub struct CoverageData {
    #[serde(default)]
    pub functions: Vec<CoverageFunction>,
}

#[derive(Deserialize)]
pub struct CoverageFunction {
    pub name: String,
    #[serde(default)]
    pub filenames: Vec<String>,
    #[serde(default)]
    pub regions: Vec<CoverageRegion>,
}

/// Sentinel `line_end` value marking a row as a "crate-root sentinel" — a
/// row that overlaps any hunk in the file by construction. Used to model
/// implicit dependencies cargo's function-level coverage can't observe
/// (e.g., `mod foo;` or `use ...;` in a crate root). Detection at query
/// time relies on exact-value equality, so all sentinel-creators must use
/// this constant via [`HitRange::sentinel`].
pub const CRATE_ROOT_SENTINEL_END: i64 = i64::MAX;

/// A region tuple as emitted by `llvm-cov export`. Only the source extent and
/// the file it belongs to are used; the columns, the execution count (always
/// zero — the map is exported against an empty profile) and the trailing
/// fields (`expanded_file_id`, `kind`, optional extras for newer LLVM) are
/// accepted but ignored.
#[derive(Debug)]
pub struct CoverageRegion {
    pub line_start: i64,
    pub line_end: i64,
    pub file_id: usize,
}

impl<'de> Deserialize<'de> for CoverageRegion {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let raw: Vec<serde_json::Value> = Vec::deserialize(d)?;
        let get_u64 = |i: usize| -> std::result::Result<u64, D::Error> {
            raw.get(i)
                .and_then(|v| v.as_u64())
                .ok_or_else(|| serde::de::Error::custom(format!("region missing field {i}")))
        };
        Ok(CoverageRegion {
            line_start: get_u64(0)? as i64,
            line_end: get_u64(2)? as i64,
            file_id: get_u64(5)? as usize,
        })
    }
}

/// One hit function range for some test, in source coordinates.
///
/// `line_start..=line_end` is the inclusive line span covered by the function's
/// hit regions in `file`. Sources are stored relative to the project root so
/// they line up with `git diff` output.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct HitRange {
    pub file: Utf8PathBuf,
    pub line_start: i64,
    pub line_end: i64,
}

impl HitRange {
    /// Build a sentinel range for `file` — line 1 through
    /// [`CRATE_ROOT_SENTINEL_END`]. Stored alongside real function ranges
    /// to model an implicit "any hunk in this file selects this test" link
    /// the function-level coverage can't observe directly.
    pub fn sentinel(file: Utf8PathBuf) -> Self {
        Self {
            file,
            line_start: 1,
            line_end: CRATE_ROOT_SENTINEL_END,
        }
    }
}

/// Where every instrumented function of one binary lives in the project's
/// source, keyed by mangled name.
///
/// A name maps to one range per source file it spans — normally exactly one;
/// more only when macro expansion pulls a function's regions across files.
/// Functions outside the project root (stdlib, dependencies) are absent
/// entirely, which is what keeps the map two orders of magnitude smaller than
/// the export it comes from.
pub type FunctionMap = BTreeMap<String, Vec<HitRange>>;

/// Convert a relative source path into a `Utf8PathBuf` whose string form uses
/// forward slashes on every platform.
///
/// llvm-cov and cargo on Windows emit paths with `\` separators, while git
/// diff always uses `/`. The DB stores file paths as opaque strings and
/// looks them up by exact-match — so anything destined for `test_regions`
/// must be normalised to git's separator or selection silently misses on
/// Windows. Returns `None` if the path isn't valid UTF-8 (paths originating
/// from cargo/llvm-cov already are, but the call shape is still fallible).
pub fn to_db_relative(path: &Path) -> Option<Utf8PathBuf> {
    let utf8 = Utf8PathBuf::try_from(path.to_path_buf()).ok()?;
    if cfg!(windows) && utf8.as_str().contains('\\') {
        Some(Utf8PathBuf::from(utf8.as_str().replace('\\', "/")))
    } else {
        Some(utf8)
    }
}

/// Build one binary's [`FunctionMap`] from its `llvm-cov export` JSON.
///
/// `canonical_root` must already be canonicalized. Functions outside it
/// (stdlib, dependencies) are dropped: `--ignore-filename-regex` shrinks
/// `files[]` but leaves `functions[]` intact, so the filter is applied here.
/// Multiple monomorphizations of a generic that land on the same source extent
/// collapse to one range.
pub fn build_function_map(json: &str, canonical_root: &Path) -> Result<FunctionMap> {
    let export: CoverageExport =
        serde_json::from_str(json).context("failed to parse llvm-cov export JSON")?;

    // A function may span multiple file_ids via macro expansion. The vast
    // majority span exactly one — a small Vec with linear scan beats a
    // HashMap for the typical n=1 case, and this runs across every function
    // in the binary.
    let mut per_file: Vec<(usize, i64, i64)> = Vec::new();
    let mut map = FunctionMap::new();
    for data in &export.data {
        for func in &data.functions {
            per_file.clear();
            for region in &func.regions {
                if let Some(entry) = per_file.iter_mut().find(|(id, _, _)| *id == region.file_id) {
                    entry.1 = entry.1.min(region.line_start);
                    entry.2 = entry.2.max(region.line_end);
                } else {
                    per_file.push((region.file_id, region.line_start, region.line_end));
                }
            }

            for &(file_id, start, end) in &per_file {
                let Some(filename) = func.filenames.get(file_id) else {
                    continue;
                };
                let path = Path::new(filename);
                let Ok(rel) = path.strip_prefix(canonical_root) else {
                    continue;
                };
                let Some(utf8) = to_db_relative(rel) else {
                    continue;
                };
                let range = HitRange {
                    file: utf8,
                    line_start: start,
                    line_end: end,
                };
                let ranges = map.entry(func.name.clone()).or_default();
                if !ranges.contains(&range) {
                    ranges.push(range);
                }
            }
        }
    }
    Ok(map)
}

/// Names of the functions a test executed, read from the text form of its
/// merged profile (`llvm-profdata merge --text`).
///
/// The format is a flat token stream — function name, hash, counter count,
/// then that many counter values — with `#`-prefixed labels, blank separators
/// and leading `:`-prefixed file flags carrying no data. A function counts as
/// executed when any of its counters is non-zero.
///
/// Parsing is strict: a record whose fields don't come out as numbers is an
/// error rather than a skipped function. A silently short parse would hand the
/// test fewer ranges than it earned, and *under*-selection is the one failure
/// this tool can't detect downstream.
pub fn executed_functions(proftext: &str) -> Result<Vec<&str>> {
    let mut tokens = proftext
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .peekable();
    while tokens.peek().is_some_and(|line| line.starts_with(':')) {
        tokens.next();
    }

    let mut executed = Vec::new();
    while let Some(name) = tokens.next() {
        let number = |tokens: &mut std::iter::Peekable<_>, field: &str| -> Result<u64> {
            let token: &str = Iterator::next(tokens)
                .with_context(|| format!("profile record {name} ended before its {field}"))?;
            token
                .parse()
                .with_context(|| format!("profile record {name} has a non-numeric {field}"))
        };
        let _hash = number(&mut tokens, "hash")?;
        let counters = number(&mut tokens, "counter count")?;
        let mut hit = false;
        for _ in 0..counters {
            hit |= number(&mut tokens, "counter value")? > 0;
        }
        if hit {
            executed.push(name);
        }
    }
    if executed.is_empty() {
        bail!("profile lists no executed functions");
    }
    Ok(executed)
}

/// Join a test's executed functions against its binary's [`FunctionMap`].
///
/// Names absent from the map are the ones outside the project root — the bulk
/// of any real profile, since dependencies are instrumented too.
pub fn hit_ranges<'a>(
    executed: impl IntoIterator<Item = &'a str>,
    map: &FunctionMap,
) -> BTreeSet<HitRange> {
    let mut ranges = BTreeSet::new();
    for name in executed {
        if let Some(function) = map.get(name) {
            ranges.extend(function.iter().cloned());
        }
    }
    ranges
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build the export JSON for a function, JSON-escaping the interpolated
    /// paths: on Windows a literal `C:\Users\…` would otherwise produce
    /// invalid `\U` escapes.
    fn function_json(name: &str, filenames: &[&std::path::Path], regions: &str) -> String {
        let names: Vec<String> = filenames
            .iter()
            .map(|p| serde_json::to_string(&p.display().to_string()).unwrap())
            .collect();
        format!(
            r#"{{"name": "{name}", "count": 0, "filenames": [{}], "regions": [{regions}]}}"#,
            names.join(", "),
        )
    }

    fn export_json(functions: &[String]) -> String {
        format!(r#"{{"data": [{{"functions": [{}]}}]}}"#, functions.join(", "))
    }

    /// A project with two source files: the map records each function's full
    /// extent per file, and functions outside the project root are dropped.
    #[test]
    fn builds_function_extents_per_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "").unwrap();
        std::fs::write(root.join("src/utils.rs"), "").unwrap();

        let canon = root.canonicalize().unwrap();
        let lib = root.join("src/lib.rs").canonicalize().unwrap();
        let utils = root.join("src/utils.rs").canonicalize().unwrap();

        let json = export_json(&[
            // Two regions, the second reaching further down the file.
            function_json("adds", &[&lib], "[10, 0, 12, 0, 0, 0, 0, 0], [11, 0, 15, 0, 0, 0, 0, 0]"),
            function_json("greets", &[&utils], "[5, 0, 7, 0, 0, 0, 0, 0]"),
            function_json(
                "std_io",
                &[std::path::Path::new("/rustc/abc/library/std/src/io.rs")],
                "[1, 0, 5, 0, 0, 0, 0, 0]",
            ),
        ]);

        let map = build_function_map(&json, &canon).unwrap();
        assert_eq!(map.len(), 2, "the stdlib function is filtered out: {map:?}");
        assert_eq!(
            map["adds"],
            vec![HitRange {
                file: Utf8PathBuf::from("src/lib.rs"),
                line_start: 10,
                line_end: 15,
            }],
        );
        assert_eq!(
            map["greets"],
            vec![HitRange {
                file: Utf8PathBuf::from("src/utils.rs"),
                line_start: 5,
                line_end: 7,
            }],
        );
    }

    /// A region carrying `count: 0` still contributes its extent — the map is
    /// exported against an empty profile, so *every* count is zero and the
    /// counts a per-test profile would supply live in the profile instead.
    #[test]
    fn zero_count_regions_still_define_the_extent() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "").unwrap();
        let canon = root.canonicalize().unwrap();
        let lib = root.join("src/lib.rs").canonicalize().unwrap();

        let json = export_json(&[function_json(
            "early_return",
            &[&lib],
            "[20, 0, 25, 0, 0, 0, 0, 0], [22, 0, 30, 0, 0, 0, 0, 0]",
        )]);
        let map = build_function_map(&json, &canon).unwrap();
        assert_eq!(map["early_return"][0].line_start, 20);
        assert_eq!(map["early_return"][0].line_end, 30);
    }

    /// Two monomorphizations of one generic land on the same source extent and
    /// collapse to a single range.
    #[test]
    fn dedupes_generic_monomorphizations() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "").unwrap();
        let canon = root.canonicalize().unwrap();
        let lib = root.join("src/lib.rs").canonicalize().unwrap();

        // Distinct mangled names, same extent — as `Vec<u8>` and `Vec<i32>`
        // instantiations of one function would be.
        let json = export_json(&[
            function_json("id$u8", &[&lib], "[1, 0, 5, 0, 0, 0, 0, 0]"),
            function_json("id$i32", &[&lib], "[1, 0, 5, 0, 0, 0, 0, 0]"),
        ]);
        let map = build_function_map(&json, &canon).unwrap();
        let ranges = hit_ranges(["id$u8", "id$i32"], &map);
        assert_eq!(ranges.len(), 1);
    }

    /// The `--text` profile shape llvm-profdata emits: labelled sections, one
    /// blank-separated record per function. Only functions with a non-zero
    /// counter come back.
    #[test]
    fn reads_executed_functions_from_proftext() {
        let proftext = "\
_RNvC5probe3add
# Func Hash:
5576428231601949022
# Num Counters:
3
# Counter Values:
8
0
2

_RNvC5probe12never_called
# Func Hash:
123
# Num Counters:
2
# Counter Values:
0
0
";
        assert_eq!(executed_functions(proftext).unwrap(), ["_RNvC5probe3add"]);
    }

    /// A leading `:`-prefixed flag line describes the file, not a function.
    #[test]
    fn skips_leading_profile_flags() {
        let proftext = ":ir\n:entry_first\nfunc\n7\n1\n4\n";
        assert_eq!(executed_functions(proftext).unwrap(), ["func"]);
    }

    /// A record that runs out mid-way, or carries a field the grammar says is
    /// a number and isn't, is an error — never a quietly dropped function.
    #[test]
    fn malformed_records_are_errors() {
        let truncated = executed_functions("func\n7\n3\n1\n2\n").unwrap_err();
        assert!(
            truncated.to_string().contains("counter value"),
            "unexpected error: {truncated:#}",
        );
        let unparseable = executed_functions("func\nnot-a-hash\n1\n1\n").unwrap_err();
        assert!(
            unparseable.to_string().contains("non-numeric hash"),
            "unexpected error: {unparseable:#}",
        );
        // An unknown trailing section (a future LLVM addition) desyncs the
        // grammar and surfaces as a parse error naming the token it tripped
        // on, rather than as a short read.
        let unknown = executed_functions("func\n7\n1\n4\n$deadbeef\n").unwrap_err();
        assert!(
            unknown.to_string().contains("$deadbeef"),
            "unexpected error: {unknown:#}",
        );
    }

    /// A profile with no executed function at all means the parse found
    /// nothing, not that the test ran nothing — every test executes at least
    /// its own body.
    #[test]
    fn empty_profile_is_an_error() {
        assert!(executed_functions("").is_err());
    }

    #[test]
    fn sentinel_uses_the_canonical_end_value() {
        let r = HitRange::sentinel(Utf8PathBuf::from("src/lib.rs"));
        assert_eq!(r.line_start, 1);
        assert_eq!(r.line_end, CRATE_ROOT_SENTINEL_END);
        assert_eq!(r.line_end, i64::MAX);
    }
}
