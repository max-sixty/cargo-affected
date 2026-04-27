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

/// A region tuple as emitted by `llvm-cov export`. Only the leading six
/// fields are used here; trailing fields (`expanded_file_id`, `kind`,
/// optional extras for newer LLVM) are accepted but ignored.
#[derive(Debug)]
pub struct CoverageRegion {
    pub line_start: u32,
    pub line_end: u32,
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
            line_start: get_u64(0)? as u32,
            line_end: get_u64(2)? as u32,
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
    pub line_start: u32,
    pub line_end: u32,
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
    let mut per_file: Vec<(usize, u32, u32)> = Vec::new();
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
                let Ok(utf8) = Utf8PathBuf::try_from(rel.to_path_buf()) else {
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
}
