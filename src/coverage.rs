//! Parse LLVM coverage JSON (`llvm-cov export`) to extract covered source files.
//!
//! We only need the file paths from each test's coverage report, not line-level
//! detail. The JSON structure is:
//!
//! ```json
//! {
//!   "data": [{
//!     "files": [{
//!       "filename": "/absolute/path/to/source.rs",
//!       "summary": { "lines": { "count": 100, "covered": 80 } }
//!     }]
//!   }]
//! }
//! ```

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
    pub files: Vec<CoverageFile>,
}

#[derive(Deserialize)]
pub struct CoverageFile {
    pub filename: String,
    pub summary: Option<CoverageSummary>,
}

#[derive(Deserialize)]
pub struct CoverageSummary {
    pub lines: Option<LineSummary>,
}

#[derive(Deserialize)]
pub struct LineSummary {
    pub covered: u64,
}

/// Extract covered source file paths from `llvm-cov export` JSON output.
///
/// Returns paths relative to `project_root`. Files outside the project root
/// (stdlib, dependencies) are excluded.
pub fn extract_covered_files(json: &str, project_root: &Path) -> Result<BTreeSet<Utf8PathBuf>> {
    let export: CoverageExport =
        serde_json::from_str(json).context("failed to parse llvm-cov export JSON")?;

    let root = project_root
        .canonicalize()
        .context("failed to canonicalize project root")?;

    let mut files = BTreeSet::new();
    for data in &export.data {
        for file in &data.files {
            // Only include files where the test actually executed lines.
            // Crate roots (lib.rs/main.rs) are added separately as implicit deps.
            let covered = file
                .summary
                .as_ref()
                .and_then(|s| s.lines.as_ref())
                .map(|l| l.covered)
                .unwrap_or(0);
            if covered == 0 {
                continue;
            }

            let path = Path::new(&file.filename);
            // Only include files within the project root, skip deps/stdlib.
            if let Ok(rel) = path.strip_prefix(&root) {
                if let Ok(utf8) = Utf8PathBuf::try_from(rel.to_path_buf()) {
                    files.insert(utf8);
                }
            }
        }
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_covered_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "").unwrap();
        std::fs::write(root.join("src/utils.rs"), "").unwrap();

        let abs_lib = root.join("src/lib.rs").canonicalize().unwrap();
        let abs_utils = root.join("src/utils.rs").canonicalize().unwrap();

        let json = format!(
            r#"{{
            "data": [{{
                "files": [
                    {{"filename": "{}", "summary": {{"lines": {{"covered": 5}}}}}},
                    {{"filename": "{}", "summary": {{"lines": {{"covered": 0}}}}}},
                    {{"filename": "/rustc/abc123/library/std/src/io/mod.rs", "summary": {{"lines": {{"covered": 10}}}}}}
                ]
            }}]
        }}"#,
            abs_lib.display(),
            abs_utils.display()
        );

        let files = extract_covered_files(&json, root).unwrap();
        // lib.rs included (covered > 0, within project root)
        assert_eq!(files.len(), 1);
        assert!(files.contains(&Utf8PathBuf::from("src/lib.rs")));
        // utils.rs excluded (covered == 0), stdlib excluded (outside project root)
    }
}
