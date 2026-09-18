//! Declarative input→test rules from `[workspace.metadata.affected]`.
//!
//! cargo-affected selects tests by Rust-line coverage overlap, so a change to a
//! non-Rust input a test reads at runtime — an insta `.snap`, a doc `.md`, a
//! template, an `include_str!` target — maps to no coverage row and selects no
//! test (see README, "Non-Rust sources"). The `[[workspace.metadata.affected.rule]]`
//! tables (or `[[package.metadata.affected.rule]]` for a single-crate project)
//! pair input globs with a nextest filterset; when a changed path matches a
//! rule's globs, that rule's tests are force-selected. The filterset is resolved
//! with `cargo nextest list -E`, so it speaks the full nextest filter language.
//!
//! The rules ride in the manifest cargo-affected already loads via `cargo
//! metadata`, so there's no extra file to read. The trade-off is that the
//! manifest is fingerprinted: editing a rule changes the coverage-cache key, so
//! the next run re-collects. No rules → the tool behaves exactly as before, with
//! no extra `nextest list` invocation.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context, Result};
use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::Deserialize;
use serde_json::Value;

use crate::collect::nextest_list;
use crate::db::TestId;
use crate::project::ProjectRoot;

/// Where rules live, for user-facing error messages. The `*` shorthand covers
/// both locations — `[workspace.metadata.affected]` and the single-crate
/// `[package.metadata.affected]` fallback — since these messages fire equally
/// for rules loaded from either, and a hardcoded `workspace` would send a
/// single-crate user grepping for a table their `Cargo.toml` doesn't contain.
const TABLE: &str = "[*.metadata.affected]";

/// Parsed `affected` metadata table. An absent table deserializes to `Default`
/// (no rules), preserving the no-config invariant.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AffectedConfig {
    /// `[[..metadata.affected.rule]]` array-of-tables. Renamed so the TOML key
    /// reads as a single rule per table while the field stays plural.
    #[serde(default, rename = "rule")]
    rules: Vec<InputRule>,
}

/// One `[[rule]]`: input globs paired with the nextest filterset whose tests
/// to run when any changed path matches.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct InputRule {
    globs: Vec<String>,
    filterset: String,
}

/// A rule whose globs are compiled into a matcher, ready to test changed paths.
#[derive(Debug)]
pub(crate) struct CompiledRule {
    matcher: GlobSet,
    filterset: String,
}

impl AffectedConfig {
    /// Read the `affected` table from the already-loaded `cargo metadata` JSON.
    ///
    /// Prefers `[workspace.metadata.affected]`; falls back to the root package's
    /// `[package.metadata.affected]` (single-crate projects have no `[workspace]`
    /// section). An absent table yields no rules — not an error. A malformed
    /// table is a hard error: a silently-dropped rule would reopen the exact gap
    /// it exists to close.
    pub(crate) fn from_metadata(metadata: &Value, workspace_root: &Path) -> Result<Self> {
        let table = metadata
            .get("metadata")
            .and_then(|m| m.get("affected"))
            .or_else(|| root_package_metadata_affected(metadata, workspace_root));
        match table {
            Some(value) => serde_json::from_value(value.clone())
                .with_context(|| format!("failed to parse {TABLE} in Cargo.toml")),
            None => Ok(Self::default()),
        }
    }

    /// Compile every rule's globs into a `GlobSet`. A malformed glob is a hard
    /// error naming the offending pattern.
    pub(crate) fn compile(&self) -> Result<Vec<CompiledRule>> {
        self.rules.iter().map(InputRule::compile).collect()
    }
}

/// The `metadata.affected` value of the package whose manifest is the workspace
/// root's `Cargo.toml` — i.e. the root package's `[package.metadata.affected]`.
fn root_package_metadata_affected<'a>(
    metadata: &'a Value,
    workspace_root: &Path,
) -> Option<&'a Value> {
    let root_manifest = workspace_root.join("Cargo.toml");
    metadata
        .get("packages")?
        .as_array()?
        .iter()
        .find_map(|pkg| {
            let manifest = pkg.get("manifest_path")?.as_str()?;
            if Path::new(manifest) == root_manifest {
                pkg.get("metadata")?.get("affected")
            } else {
                None
            }
        })
}

impl InputRule {
    fn compile(&self) -> Result<CompiledRule> {
        let mut builder = GlobSetBuilder::new();
        for g in &self.globs {
            builder.add(Glob::new(g).with_context(|| format!("invalid glob {g:?} in {TABLE}"))?);
        }
        Ok(CompiledRule {
            matcher: builder
                .build()
                .with_context(|| format!("failed to build glob matcher for {TABLE}"))?,
            filterset: self.filterset.clone(),
        })
    }
}

/// Load the `affected` rules for `project`, and if any, resolve them against
/// `changed_paths` — every path that differs from any reachable `collect_sha`
/// or from HEAD, unioned by [`changed_paths_since`](crate::selection::changed_paths_since)
/// in [`crate::plan::plan`]. Returns the path→tests map
/// [`compute`](crate::selection::compute) folds in.
///
/// The no-rules fast path returns an empty map without invoking nextest, so a
/// project without the table still pays nothing for the expensive half. It no
/// longer skips computing the changed paths — those moved up to `plan`, which
/// needs them anyway to tell "nothing changed" from "nothing covers what
/// changed" — but that's one `git diff --name-only` per reachable sha against
/// a `nextest list` that has to build the test binaries.
pub(crate) fn config_rule_hits(
    project: &ProjectRoot,
    build_args: &[String],
    changed_paths: &BTreeSet<String>,
) -> Result<BTreeMap<String, BTreeSet<TestId>>> {
    let rules =
        AffectedConfig::from_metadata(&project.metadata, &project.workspace_root)?.compile()?;
    if rules.is_empty() {
        return Ok(BTreeMap::new());
    }
    resolve_config_hits(&project.workspace_root, build_args, &rules, changed_paths)
}

/// Resolve compiled rules against the changed paths, returning a map from each
/// changed path that matched a rule to the tests that rule selects.
///
/// For each rule with at least one matching changed path, `cargo nextest list
/// -E <filterset>` resolves the filterset to concrete tests — using the same
/// build flags as the run, so the listing matches what nextest will build.
/// Keying on the changed path lets the JSON report attribute the selection to
/// the file that triggered it. Rules with no matching path cost nothing (no
/// nextest invocation), so a Rust-only diff is byte-for-byte the prior
/// behavior plus one cheap glob check per changed path.
///
/// A rule whose filterset resolves to zero tests after matching is surfaced as
/// a warning rather than swallowed: a typo'd filterset would otherwise silently
/// reopen the gap it exists to close.
pub(crate) fn resolve_config_hits(
    project_root: &Path,
    build_args: &[String],
    rules: &[CompiledRule],
    changed_paths: &BTreeSet<String>,
) -> Result<BTreeMap<String, BTreeSet<TestId>>> {
    let mut out: BTreeMap<String, BTreeSet<TestId>> = BTreeMap::new();
    for rule in rules {
        let matched: Vec<&String> = changed_paths
            .iter()
            .filter(|p| rule.matcher.is_match(p.as_str()))
            .collect();
        if matched.is_empty() {
            continue;
        }
        let listing = nextest_list(project_root, None, None, build_args, Some(&rule.filterset))
            .with_context(|| {
                format!(
                    "failed to resolve {TABLE} filterset {:?} \
                     (check it is a valid nextest filter expression)",
                    rule.filterset
                )
            })?;
        let tests: BTreeSet<TestId> = listing.tests.into_iter().collect();
        if tests.is_empty() {
            eprintln!(
                "warning: {TABLE} rule matched {} but its filterset ({:?}) \
                 selected no tests — those input changes may go untested",
                matched
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
                rule.filterset,
            );
            continue;
        }
        for p in matched {
            out.entry(p.clone())
                .or_default()
                .extend(tests.iter().cloned());
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `[workspace.metadata.affected]` shape, as `cargo metadata` surfaces it.
    fn workspace_meta(rules: Value) -> Value {
        serde_json::json!({ "metadata": { "affected": { "rule": rules } }, "packages": [] })
    }

    #[test]
    fn absent_table_is_empty_config() {
        let meta = serde_json::json!({ "metadata": null, "packages": [] });
        let cfg = AffectedConfig::from_metadata(&meta, Path::new("/ws")).unwrap();
        assert!(cfg.rules.is_empty());
        assert!(cfg.compile().unwrap().is_empty());
    }

    #[test]
    fn parses_workspace_metadata_rules() {
        let meta = workspace_meta(serde_json::json!([
            { "globs": ["**/*.snap"], "filterset": "binary_id(=pkg::integration) & test(/help/)" },
            { "globs": ["README.md", "docs/**/*.md"], "filterset": "test(/sync/)" },
        ]));
        let cfg = AffectedConfig::from_metadata(&meta, Path::new("/ws")).unwrap();
        assert_eq!(cfg.rules.len(), 2);
        assert_eq!(
            cfg.rules[0].filterset,
            "binary_id(=pkg::integration) & test(/help/)"
        );
        assert_eq!(cfg.rules[1].globs, vec!["README.md", "docs/**/*.md"]);
    }

    #[test]
    fn falls_back_to_root_package_metadata() {
        // No [workspace.metadata]; rules live in the root package's
        // [package.metadata.affected] (single-crate layout).
        let meta = serde_json::json!({
            "metadata": null,
            "packages": [{
                "manifest_path": "/ws/Cargo.toml",
                "metadata": { "affected": { "rule": [
                    { "globs": ["x.snap"], "filterset": "test(=t)" }
                ] } },
            }],
        });
        let cfg = AffectedConfig::from_metadata(&meta, Path::new("/ws")).unwrap();
        assert_eq!(cfg.rules.len(), 1);
        assert_eq!(cfg.rules[0].globs, vec!["x.snap"]);
    }

    #[test]
    fn malformed_table_errors() {
        // `filterset` misspelled — deny_unknown_fields makes this a hard error
        // rather than a silently-empty rule.
        let meta = workspace_meta(serde_json::json!([
            { "globs": ["x"], "filterse": "y" }
        ]));
        let err = AffectedConfig::from_metadata(&meta, Path::new("/ws"))
            .expect_err("typo'd key must error");
        assert!(format!("{err:#}").contains("Cargo.toml"));
    }

    #[test]
    fn invalid_glob_errors() {
        let cfg = AffectedConfig {
            rules: vec![InputRule {
                globs: vec!["a[".to_string()], // unclosed character class
                filterset: "test(/x/)".to_string(),
            }],
        };
        let err = cfg.compile().expect_err("invalid glob must error");
        assert!(format!("{err:#}").contains("invalid glob"));
    }

    #[test]
    fn compiled_rule_matches_globs() {
        let cfg = AffectedConfig {
            rules: vec![InputRule {
                globs: vec!["**/*.snap".to_string(), "docs/**/*.md".to_string()],
                filterset: "test(/x/)".to_string(),
            }],
        };
        let compiled = cfg.compile().unwrap();
        let m = &compiled[0].matcher;
        assert!(m.is_match("tests/integration/snapshots/help.snap"));
        assert!(m.is_match("docs/content/faq.md"));
        assert!(!m.is_match("src/lib.rs"));
        assert!(!m.is_match("README.md"));
    }
}
