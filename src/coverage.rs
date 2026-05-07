//! Parse LLVM coverage JSON (`llvm-cov export`) to extract per-function line
//! ranges hit by a single test.
//!
//! Per the LLVM coverage exporter, `data[i].functions[j]` looks like:
//!
//! ```json
//! {
//!   "name": "...",
//!   "count": <total execution count for the function>,
//!   "filenames": ["/abs/path/to/source.rs", ...],
//!   "regions": [
//!     [line_start, col_start, line_end, col_end, count, file_id, ...],
//!     ...
//!   ]
//! }
//! ```
//!
//! For each function with `count > 0`, we walk its regions, group by file, and
//! emit `(file, min(line_start), max(line_end))` covering the hit extent in
//! that file. Multiple monomorphizations of a generic function collapse to the
//! same `(file, line_start, line_end)` tuple downstream via the dedupe set.
//!
//! Note that `--ignore-filename-regex` shrinks `data[].files[]` but leaves
//! `data[].functions[]` untouched, so we apply the project-root filter
//! ourselves here using `strip_prefix`.

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::{Context, Result};
use camino::Utf8PathBuf;
use serde::Deserialize;

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
    #[serde(default)]
    pub count: u64,
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

/// A region tuple as emitted by `llvm-cov export`. Only the leading six
/// fields are used here; trailing fields (`expanded_file_id`, `kind`,
/// optional extras for newer LLVM) are accepted but ignored.
#[derive(Debug)]
pub struct CoverageRegion {
    pub line_start: i64,
    pub line_end: i64,
    pub count: u64,
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
            count: get_u64(4)?,
            file_id: get_u64(5)? as usize,
        })
    }
}

/// One hit function range for some test, in source coordinates.
///
/// `line_start..=line_end` is the inclusive line span covered by the function's
/// hit regions in `file`. Sources are stored relative to the project root so
/// they line up with `git diff` output.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
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

/// Extract per-function hit line ranges from `llvm-cov export` JSON output.
///
/// `canonical_root` must already be canonicalized — this function is called
/// once per test, so canonicalizing here would be a syscall per test.
/// Functions outside the project root (stdlib, dependencies) are excluded;
/// `--ignore-filename-regex` shrinks `files[]` but leaves `functions[]`
/// intact, so we re-filter here. Multiple monomorphizations of the same
/// generic that hit the same source extent dedupe.
pub fn extract_hit_ranges(json: &str, canonical_root: &Path) -> Result<BTreeSet<HitRange>> {
    let export: CoverageExport =
        serde_json::from_str(json).context("failed to parse llvm-cov export JSON")?;

    // A function may span multiple file_ids via macro expansion. The vast
    // majority span exactly one — a small Vec with linear scan beats a
    // HashMap for the typical n=1 case, and this runs across thousands of
    // functions per test.
    let mut per_file: Vec<(usize, i64, i64)> = Vec::new();
    let mut ranges = BTreeSet::new();
    for data in &export.data {
        for func in &data.functions {
            if func.count == 0 {
                continue;
            }
            per_file.clear();
            for region in &func.regions {
                if region.count == 0 {
                    continue;
                }
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
                ranges.insert(HitRange {
                    file: utf8,
                    line_start: start,
                    line_end: end,
                });
            }
        }
    }
    Ok(ranges)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_function_ranges_per_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "").unwrap();
        std::fs::write(root.join("src/utils.rs"), "").unwrap();

        let canon = root.canonicalize().unwrap();
        let abs_lib = root.join("src/lib.rs").canonicalize().unwrap();
        let abs_utils = root.join("src/utils.rs").canonicalize().unwrap();

        // Two functions in lib.rs, one in utils.rs, plus a stdlib hit that
        // must be filtered out, plus an unhit function that must be ignored.
        let json = format!(
            r#"{{
            "data": [{{
                "functions": [
                    {{
                        "count": 1,
                        "filenames": ["{abs_lib}"],
                        "regions": [
                            [10, 0, 12, 0, 5, 0, 0, 0],
                            [11, 0, 15, 0, 3, 0, 0, 0]
                        ]
                    }},
                    {{
                        "count": 1,
                        "filenames": ["{abs_lib}"],
                        "regions": [
                            [20, 0, 25, 0, 1, 0, 0, 0],
                            [22, 0, 23, 0, 0, 0, 0, 0]
                        ]
                    }},
                    {{
                        "count": 1,
                        "filenames": ["{abs_utils}"],
                        "regions": [[5, 0, 7, 0, 2, 0, 0, 0]]
                    }},
                    {{
                        "count": 0,
                        "filenames": ["{abs_lib}"],
                        "regions": [[100, 0, 200, 0, 0, 0, 0, 0]]
                    }},
                    {{
                        "count": 1,
                        "filenames": ["/rustc/abc/library/std/src/io.rs"],
                        "regions": [[1, 0, 5, 0, 1, 0, 0, 0]]
                    }}
                ]
            }}]
        }}"#,
            abs_lib = abs_lib.display(),
            abs_utils = abs_utils.display(),
        );

        let ranges = extract_hit_ranges(&json, &canon).unwrap();
        let expected: BTreeSet<HitRange> = [
            HitRange {
                file: Utf8PathBuf::from("src/lib.rs"),
                line_start: 10,
                line_end: 15,
            },
            // Second lib.rs function: only the count>0 region [20,25] contributes.
            HitRange {
                file: Utf8PathBuf::from("src/lib.rs"),
                line_start: 20,
                line_end: 25,
            },
            HitRange {
                file: Utf8PathBuf::from("src/utils.rs"),
                line_start: 5,
                line_end: 7,
            },
        ]
        .into_iter()
        .collect();
        assert_eq!(ranges, expected);
    }

    #[test]
    fn dedupes_generic_monomorphizations() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "").unwrap();
        let canon = root.canonicalize().unwrap();
        let abs = root.join("src/lib.rs").canonicalize().unwrap();

        // Same source extent emitted by two monomorphizations.
        let json = format!(
            r#"{{
            "data": [{{
                "functions": [
                    {{
                        "count": 1,
                        "filenames": ["{abs}"],
                        "regions": [[1, 0, 5, 0, 1, 0, 0, 0]]
                    }},
                    {{
                        "count": 1,
                        "filenames": ["{abs}"],
                        "regions": [[1, 0, 5, 0, 1, 0, 0, 0]]
                    }}
                ]
            }}]
        }}"#,
            abs = abs.display(),
        );
        let ranges = extract_hit_ranges(&json, &canon).unwrap();
        assert_eq!(ranges.len(), 1);
    }

    #[test]
    fn sentinel_uses_the_canonical_end_value() {
        let r = HitRange::sentinel(Utf8PathBuf::from("src/lib.rs"));
        assert_eq!(r.line_start, 1);
        assert_eq!(r.line_end, CRATE_ROOT_SENTINEL_END);
        assert_eq!(r.line_end, i64::MAX);
    }
}
