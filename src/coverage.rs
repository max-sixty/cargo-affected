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
pub(crate) struct CoverageExport {
    pub(crate) data: Vec<CoverageData>,
}

#[derive(Deserialize)]
pub(crate) struct CoverageData {
    #[serde(default)]
    pub(crate) functions: Vec<CoverageFunction>,
}

#[derive(Deserialize)]
pub(crate) struct CoverageFunction {
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) filenames: Vec<String>,
    #[serde(default)]
    pub(crate) regions: Vec<CoverageRegion>,
}

/// Sentinel `line_end` value marking a row as a "crate-root sentinel" — a
/// row that overlaps any hunk in the file by construction. Used to model
/// implicit dependencies cargo's function-level coverage can't observe
/// (e.g., `mod foo;` or `use ...;` in a crate root). Detection at query
/// time relies on exact-value equality, so all sentinel-creators must use
/// this constant via [`HitRange::sentinel`].
pub(crate) const CRATE_ROOT_SENTINEL_END: i64 = i64::MAX;

/// A region tuple as emitted by `llvm-cov export`. Only the source extent and
/// the file it belongs to are used; the columns, the execution count (always
/// zero — the map is exported against an empty profile) and the trailing
/// fields (`expanded_file_id`, `kind`, optional extras for newer LLVM) are
/// accepted but ignored.
#[derive(Debug)]
pub(crate) struct CoverageRegion {
    pub(crate) line_start: i64,
    pub(crate) line_end: i64,
    pub(crate) file_id: usize,
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
pub(crate) struct HitRange {
    pub(crate) file: Utf8PathBuf,
    pub(crate) line_start: i64,
    pub(crate) line_end: i64,
}

impl HitRange {
    /// Build a sentinel range for `file` — line 1 through
    /// [`CRATE_ROOT_SENTINEL_END`]. Stored alongside real function ranges
    /// to model an implicit "any hunk in this file selects this test" link
    /// the function-level coverage can't observe directly.
    pub(crate) fn sentinel(file: Utf8PathBuf) -> Self {
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
/// the export it comes from. That absence is the normal case, not an error:
/// most of what a test executes is dependency code.
pub(crate) type FunctionMap = BTreeMap<String, Vec<HitRange>>;

/// A binary's identity at the moment its [`FunctionMap`] was exported.
///
/// The filename cannot serve: cargo's `-C extra-filename` hash is derived from
/// the unit's *metadata* — package id, profile, features, rustc version,
/// RUSTFLAGS, target — not from its contents. Edit a source file, rebuild, and
/// the test binary keeps the same name while every line number in it may have
/// moved. A map keyed only by filename would therefore be reapplied to a
/// different build without anything noticing, and the ranges would be filed
/// against the wrong lines: silent *under*-selection, the one failure this
/// tool can't detect downstream.
///
/// Length plus modification time distinguishes builds at the cost of one
/// `stat`. It is not a content hash — hashing a 67 MB binary per test would
/// cost more than the extraction it guards — so it detects a rebuild rather
/// than proving byte equality. That is the right granularity: cargo rewrites
/// the file on every relink.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct BinaryStamp {
    len: u64,
    modified_secs: u64,
    modified_nanos: u32,
}

impl BinaryStamp {
    /// Stat `binary` and record what identifies this build of it.
    ///
    /// An unreadable or timestamp-less binary is an error rather than a
    /// stamp that compares equal to everything.
    pub(crate) fn of(binary: &Path) -> Result<Self> {
        let meta = std::fs::metadata(binary)
            .with_context(|| format!("failed to stat {}", binary.display()))?;
        let modified = meta
            .modified()
            .with_context(|| format!("no modification time for {}", binary.display()))?
            .duration_since(std::time::UNIX_EPOCH)
            .with_context(|| format!("modification time of {} predates 1970", binary.display()))?;
        Ok(Self {
            len: meta.len(),
            modified_secs: modified.as_secs(),
            modified_nanos: modified.subsec_nanos(),
        })
    }
}

/// One binary's function map as it travels from `collect` to the runner shim:
/// the map itself, plus the stamp of the binary it describes.
///
/// The shim re-stamps the binary it was handed and refuses a map that doesn't
/// match, so a rebuild between `collect`'s listing pass and the run surfaces
/// as a skipped test rather than as coverage filed against stale lines. See
/// [`BinaryStamp`] for why the filename can't carry that guarantee.
#[derive(Serialize, Deserialize)]
pub(crate) struct BinaryFunctionMap {
    pub(crate) binary: BinaryStamp,
    pub(crate) functions: FunctionMap,
}

/// Convert a relative source path into a `Utf8PathBuf` whose string form uses
/// forward slashes on every platform.
///
/// llvm-cov and cargo on Windows emit paths with `\` separators, while git
/// diff always uses `/`. The DB stores file paths as opaque strings and
/// looks them up by exact-match — so anything destined for `test_regions`
/// must be normalised to git's separator or selection silently misses on
/// Windows. Returns `None` if the path isn't valid UTF-8 (paths originating
/// from cargo/llvm-cov already are, but the call shape is still fallible).
pub(crate) fn to_db_relative(path: &Path) -> Option<Utf8PathBuf> {
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
pub(crate) fn build_function_map(json: &str, canonical_root: &Path) -> Result<FunctionMap> {
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
/// The grammar covers exactly the record shape `-C instrument-coverage`
/// produces: a flat token stream of function name, hash, counter count, then
/// that many counter values, with `#`-prefixed labels, blank separators and
/// leading `:`-prefixed file flags carrying no data. A function counts as
/// executed when any of its counters is non-zero.
///
/// LLVM's writer has two other per-record sections this doesn't model — MC/DC
/// bitmaps (`$`-prefixed) and value-profile data — neither of which coverage
/// instrumentation emits on stable. If one ever appears the token stream
/// desyncs, and the error names it rather than blaming the next function's
/// name.
///
/// Parsing is strict throughout: a record whose fields don't come out as
/// numbers is an error rather than a skipped function. A silently short parse
/// would hand the test fewer ranges than it earned, and *under*-selection is
/// the one failure this tool can't detect downstream.
pub(crate) fn executed_functions(proftext: &str) -> Result<Vec<&str>> {
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
        if let Some(marker) = name.strip_prefix('$') {
            bail!(
                "profile carries an MC/DC bitmap section (`${marker}`), which \
                 this parser doesn't model; coverage instrumentation alone \
                 doesn't emit one"
            );
        }
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

/// Join a test's executed functions against its binary's [`FunctionMap`],
/// refusing a join that matched nothing.
///
/// Individual misses are the normal case and carry no signal: most of what any
/// test executes is dependency code, which the map deliberately omits. *Every*
/// name missing is a different statement — a test always executes at least its
/// own body, which is project source and so is in the map. So an all-miss join
/// means the two sides don't describe the same binary: a symbol-name form we
/// don't recognise, a project root that isn't a prefix of the recorded paths,
/// a profile from somewhere else.
///
/// The check lives here rather than at the call site because the empty set is
/// a plausible-looking value: returned as a successful result it becomes a
/// test with no ranges, `collect` folds in that test's crate-root sentinels,
/// the row count looks healthy, and the stored coverage quietly degrades to
/// "only crate-root edits select anything" — under-selection across the whole
/// binary, from a collect that exited 0.
pub(crate) fn hit_ranges(executed: &[&str], map: &FunctionMap) -> Result<BTreeSet<HitRange>> {
    let mut ranges = BTreeSet::new();
    for name in executed {
        if let Some(function) = map.get(*name) {
            ranges.extend(function.iter().cloned());
        }
    }
    if ranges.is_empty() {
        bail!(
            "none of the {} functions this test executed appear among the {} \
             in its binary's function map — the profile and the map don't \
             describe the same binary",
            executed.len(),
            map.len(),
        );
    }
    Ok(ranges)
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
        format!(
            r#"{{"data": [{{"functions": [{}]}}]}}"#,
            functions.join(", ")
        )
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
            function_json(
                "adds",
                &[&lib],
                "[10, 0, 12, 0, 0, 0, 0, 0], [11, 0, 15, 0, 0, 0, 0, 0]",
            ),
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
        let ranges = hit_ranges(&["id$u8", "id$i32"], &map).unwrap();
        assert_eq!(ranges.len(), 1);
    }

    /// A name the map doesn't carry is ordinary — dependencies are
    /// instrumented too, and the map holds only project functions — so a join
    /// that matched *something* keeps going.
    #[test]
    fn unmatched_names_are_not_an_error() {
        let map: FunctionMap = [(
            "mine".to_string(),
            vec![HitRange {
                file: Utf8PathBuf::from("src/lib.rs"),
                line_start: 1,
                line_end: 4,
            }],
        )]
        .into_iter()
        .collect();
        let ranges = hit_ranges(&["serde::de", "mine", "regex::exec"], &map).unwrap();
        assert_eq!(ranges.len(), 1);
    }

    /// *Every* name missing is the failure this tool cannot afford to record
    /// as success: a test always executes its own body, so an all-miss join
    /// means the profile and the map describe different binaries. Returned as
    /// an empty-but-successful set it would reach the DB as a test with only
    /// crate-root sentinels, and every edit outside a crate root would then
    /// select nothing.
    #[test]
    fn a_join_that_matches_nothing_is_an_error() {
        let map: FunctionMap = [(
            "mine".to_string(),
            vec![HitRange {
                file: Utf8PathBuf::from("src/lib.rs"),
                line_start: 1,
                line_end: 4,
            }],
        )]
        .into_iter()
        .collect();
        let err = hit_ranges(&["serde::de", "regex::exec"], &map).unwrap_err();
        assert!(
            err.to_string().contains("don't describe the same binary"),
            "unexpected error: {err:#}",
        );
        // An empty map is the same failure seen from the other side.
        assert!(hit_ranges(&["mine"], &FunctionMap::new()).is_err());
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
        let unparsable = executed_functions("func\nnot-a-hash\n1\n1\n").unwrap_err();
        assert!(
            unparsable.to_string().contains("non-numeric hash"),
            "unexpected error: {unparsable:#}",
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

    /// The stamp exists because the filename can't do this job: cargo's hash
    /// suffix tracks build metadata, so a rebuilt binary keeps its name while
    /// its line numbers move. Rewriting a file at the same path must therefore
    /// produce a different stamp.
    #[test]
    fn a_rewritten_binary_gets_a_different_stamp() {
        let tmp = tempfile::tempdir().unwrap();
        let binary = tmp.path().join("integration-abc123");
        std::fs::write(&binary, b"first build").unwrap();
        let before = BinaryStamp::of(&binary).unwrap();
        assert_eq!(
            before,
            BinaryStamp::of(&binary).unwrap(),
            "stable when untouched"
        );

        std::fs::write(&binary, b"second build, a different length").unwrap();
        assert_ne!(before, BinaryStamp::of(&binary).unwrap());
    }

    /// A binary that isn't there can't be stamped — an error, never a stamp
    /// that would compare equal to some other missing binary's.
    #[test]
    fn stamping_a_missing_binary_errors() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(BinaryStamp::of(&tmp.path().join("nope")).is_err());
    }

    #[test]
    fn sentinel_uses_the_canonical_end_value() {
        let r = HitRange::sentinel(Utf8PathBuf::from("src/lib.rs"));
        assert_eq!(r.line_start, 1);
        assert_eq!(r.line_end, CRATE_ROOT_SENTINEL_END);
        assert_eq!(r.line_end, i64::MAX);
    }
}
