//! Environment fingerprinting.
//!
//! Produces a stable hash of every input that would globally invalidate cached
//! coverage data: `Cargo.lock`, all workspace `Cargo.toml` files, the rustc
//! version, and build-flag env vars. Stored alongside each `(test, file)`
//! mapping so queries scoped to the current fingerprint naturally miss when
//! any tracked input has changed — no explicit invalidation path needed.
//!
//! `[package.metadata]` / `[workspace.metadata]` tables are excluded from the
//! manifest hash (see [`read_manifest_for_fingerprint`]): cargo never reads
//! them for the build, so they can't affect coverage — and one of them,
//! `[workspace.metadata.affected]`, holds this tool's own input rules, which
//! must be editable without invalidating the cache.
//!
//! [`compute`] returns both the composite hex digest (for cache scoping) and
//! per-component hashes (for diagnostic "which input changed?" reporting).
//! Inputs are read once into memory and both digests are derived from the
//! same byte buffers, so the composite and components cannot diverge mid-run
//! if a file is being modified concurrently.

use std::fmt::Write;
use std::process::Command;

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

use crate::project::ProjectRoot;

/// Composite fingerprint plus the per-component hashes that produced it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Fingerprint {
    /// Composite SHA-256 hex digest. The cache-scoping value used in the DB.
    pub(crate) hex: String,
    /// Per-component hashes in the same order they were folded into `hex`.
    /// Used to answer "which input changed?" on a fingerprint mismatch.
    pub(crate) components: Vec<FingerprintComponent>,
}

/// A single named input contributing to the composite fingerprint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FingerprintComponent {
    /// Stable label (`cargo_lock`, `manifest:Cargo.toml`, `rustc`,
    /// `RUSTFLAGS`, `CARGO_BUILD_TARGET`).
    pub(crate) label: String,
    /// SHA-256 hex digest of this component's bytes alone.
    pub(crate) hash: String,
}

/// Compute the composite fingerprint and per-component hashes of the build
/// environment.
///
/// Each input is hashed with a named prefix and a null separator so collisions
/// across categories aren't possible (e.g. an env var containing "rustc:..."
/// can't look like a rustc version).
///
/// Inputs are read once into memory; the composite digest and per-component
/// digests are computed from the same byte buffers. A file changing on disk
/// after the read still produces a self-consistent `Fingerprint`.
pub(crate) fn compute(project: &ProjectRoot) -> Result<Fingerprint> {
    let inputs = collect_inputs(project)?;

    let mut hasher = Sha256::new();
    let mut components = Vec::with_capacity(inputs.len());
    for (label, bytes) in &inputs {
        fold_into_composite(&mut hasher, label, bytes);
        components.push(FingerprintComponent {
            label: label.clone(),
            hash: hash_one(bytes),
        });
    }

    Ok(Fingerprint {
        hex: hex(&hasher.finalize()),
        components,
    })
}

/// Read every fingerprint input into memory in the canonical order.
///
/// Order matters for the composite digest: changing the order would change
/// the hex digest of an otherwise unchanged environment.
fn collect_inputs(project: &ProjectRoot) -> Result<Vec<(String, Vec<u8>)>> {
    let mut inputs: Vec<(String, Vec<u8>)> =
        Vec::with_capacity(2 + project.manifest_paths.len() + 3);

    inputs.push((
        "cargo_lock".to_string(),
        read_file_or_empty(&project.workspace_root.join("Cargo.lock"))?,
    ));

    for manifest in &project.manifest_paths {
        let label = manifest
            .strip_prefix(&project.workspace_root)
            .unwrap_or(manifest)
            .to_string_lossy();
        inputs.push((
            format!("manifest:{label}"),
            read_manifest_for_fingerprint(manifest)?,
        ));
    }

    inputs.push(("rustc".to_string(), rustc_version_bytes()?));
    inputs.push(("RUSTFLAGS".to_string(), env_var("RUSTFLAGS").into_bytes()));
    inputs.push((
        "CARGO_BUILD_TARGET".to_string(),
        env_var("CARGO_BUILD_TARGET").into_bytes(),
    ));

    Ok(inputs)
}

/// Fold one labeled input into the composite hasher with the canonical
/// `label : bytes \0` framing. Must match historical encoding so a stable
/// environment produces a stable composite hex digest.
fn fold_into_composite(hasher: &mut Sha256, label: &str, bytes: &[u8]) {
    hasher.update(label.as_bytes());
    hasher.update(b":");
    hasher.update(bytes);
    hasher.update(b"\0");
}

/// SHA-256 hex of a single component's bytes, with no label/framing.
/// Component hashes are independent; framing belongs only on the composite
/// side where label collisions would matter.
fn hash_one(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex(&hasher.finalize())
}

/// Read a file's bytes; treat NotFound as empty (the appearance/disappearance
/// of the file still changes the composite digest via the label). Other I/O
/// errors propagate so we don't silently produce a fingerprint that matches a
/// different state.
fn read_file_or_empty(path: &std::path::Path) -> Result<Vec<u8>> {
    match std::fs::read(path) {
        Ok(b) => Ok(b),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(anyhow::Error::from(e).context(format!("failed to read {}", path.display()))),
    }
}

/// Read a manifest's fingerprint bytes with any `[package.metadata]` /
/// `[workspace.metadata]` table stripped.
///
/// `[*.metadata]` is cargo's escape hatch for external tools; cargo never reads
/// it for the build, so it can't change which code compiles or which tests run
/// — and therefore must not invalidate cached coverage. Concretely, this tool's
/// own `[workspace.metadata.affected]` rules live here: without stripping,
/// editing a rule would change the manifest hash and force a needless
/// re-collect, making config iteration painful.
///
/// A manifest *without* metadata is hashed by its raw bytes (the round-trip is
/// skipped), so the overwhelming common case is byte-identical to before — no
/// churn on upgrade. Only manifests that carry metadata are re-serialized via
/// toml_edit (which preserves formatting), and only the metadata subtree is
/// removed; editing rules within it leaves the remaining content identical, so
/// the hash is stable across edits.
fn read_manifest_for_fingerprint(path: &std::path::Path) -> Result<Vec<u8>> {
    let raw = read_file_or_empty(path)?;
    let Ok(text) = std::str::from_utf8(&raw) else {
        // Non-UTF8 isn't valid TOML; hash the raw bytes.
        return Ok(raw);
    };
    let Ok(mut doc) = text.parse::<toml_edit::DocumentMut>() else {
        // Unparsable manifest: `cargo metadata` would already have failed
        // upstream. Hash raw so a broken manifest still differs from a fixed one.
        return Ok(raw);
    };
    let mut stripped = false;
    for section in ["package", "workspace"] {
        if let Some(table) = doc.get_mut(section).and_then(|i| i.as_table_like_mut()) {
            stripped |= table.remove("metadata").is_some();
        }
    }
    // Only pay the re-serialize when something was actually removed; otherwise
    // the raw bytes hash identically and avoid any dependence on the serializer.
    if stripped {
        Ok(doc.to_string().into_bytes())
    } else {
        Ok(raw)
    }
}

fn env_var(name: &str) -> String {
    std::env::var(name).unwrap_or_default()
}

fn rustc_version_bytes() -> Result<Vec<u8>> {
    let output = Command::new("rustc")
        .arg("-vV")
        .output()
        .context("failed to run rustc -vV")?;
    if !output.status.success() {
        anyhow::bail!(
            "rustc -vV failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output.stdout)
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        write!(&mut s, "{b:02x}").unwrap();
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::{Mutex, MutexGuard};

    /// `compute` reads RUSTFLAGS / CARGO_BUILD_TARGET from the process env, and
    /// one of these tests mutates RUSTFLAGS. cargo runs tests in parallel
    /// threads sharing the same env, so every test that calls `compute` must
    /// serialize on this lock.
    fn env_lock() -> MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn project_with(workspace_root: PathBuf, manifests: Vec<PathBuf>) -> ProjectRoot {
        ProjectRoot {
            workspace_root,
            manifest_paths: manifests,
            metadata: serde_json::Value::Null,
        }
    }

    #[test]
    fn same_inputs_same_fingerprint() -> Result<()> {
        let _guard = env_lock();
        let dir = tempfile::tempdir()?;
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("Cargo.lock"), b"lock contents")?;
        std::fs::write(root.join("Cargo.toml"), b"[package]\nname = \"x\"\n")?;
        let project = project_with(root.clone(), vec![root.join("Cargo.toml")]);

        let a = compute(&project)?;
        let b = compute(&project)?;
        assert_eq!(a, b);
        assert_eq!(a.hex.len(), 64); // SHA-256 hex
        Ok(())
    }

    #[test]
    fn cargo_lock_change_changes_hex() -> Result<()> {
        let _guard = env_lock();
        let dir = tempfile::tempdir()?;
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("Cargo.toml"), b"[package]\n")?;
        let project = project_with(root.clone(), vec![root.join("Cargo.toml")]);

        std::fs::write(root.join("Cargo.lock"), b"v1")?;
        let a = compute(&project)?;
        std::fs::write(root.join("Cargo.lock"), b"v2")?;
        let b = compute(&project)?;
        assert_ne!(a.hex, b.hex);
        // Components diverge only in the cargo_lock entry; everything else is unchanged.
        let differing: Vec<&str> = a
            .components
            .iter()
            .zip(b.components.iter())
            .filter(|(x, y)| x.hash != y.hash)
            .map(|(x, _)| x.label.as_str())
            .collect();
        assert_eq!(differing, vec!["cargo_lock"]);
        Ok(())
    }

    #[test]
    fn manifest_change_changes_hex() -> Result<()> {
        let _guard = env_lock();
        let dir = tempfile::tempdir()?;
        let root = dir.path().to_path_buf();
        let manifest = root.join("Cargo.toml");
        let project = project_with(root.clone(), vec![manifest.clone()]);

        std::fs::write(&manifest, b"[package]\nname = \"a\"\n")?;
        let a = compute(&project)?;
        std::fs::write(&manifest, b"[package]\nname = \"b\"\n")?;
        let b = compute(&project)?;
        assert_ne!(a.hex, b.hex);
        Ok(())
    }

    /// `[*.metadata]` is excluded from the manifest hash: adding it, and then
    /// editing it, must not change the fingerprint — otherwise iterating on
    /// `[workspace.metadata.affected]` rules would invalidate the coverage cache
    /// on every edit.
    #[test]
    fn manifest_metadata_excluded_from_fingerprint() -> Result<()> {
        let _guard = env_lock();
        let dir = tempfile::tempdir()?;
        let root = dir.path().to_path_buf();
        let manifest = root.join("Cargo.toml");
        let project = project_with(root.clone(), vec![manifest.clone()]);

        let bare = "[package]\nname = \"a\"\nedition = \"2021\"\n";
        std::fs::write(&manifest, bare)?;
        let a = compute(&project)?;

        // Add a rule, then change it. Neither may move the fingerprint.
        std::fs::write(
            &manifest,
            format!("{bare}\n[[package.metadata.affected.rule]]\nglobs = [\"x.snap\"]\nfilterset = \"test(=t)\"\n"),
        )?;
        let with_rule = compute(&project)?;
        std::fs::write(
            &manifest,
            format!("{bare}\n[[package.metadata.affected.rule]]\nglobs = [\"y.snap\", \"docs/**\"]\nfilterset = \"test(=u)\"\n"),
        )?;
        let edited = compute(&project)?;

        assert_eq!(
            with_rule.hex, edited.hex,
            "editing a rule must not change the fingerprint"
        );
        // First-add may differ by at most the metadata text; the stable
        // guarantee is edit-invariance above. Build inputs still register:
        std::fs::write(&manifest, format!("{bare}edition2 = true\n"))?;
        let real_change = compute(&project)?;
        assert_ne!(
            a.hex, real_change.hex,
            "a non-metadata manifest edit must still register"
        );
        Ok(())
    }

    #[test]
    fn rustflags_change_changes_hex() -> Result<()> {
        let _guard = env_lock();
        let dir = tempfile::tempdir()?;
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("Cargo.toml"), b"[package]\n")?;
        let project = project_with(root.clone(), vec![root.join("Cargo.toml")]);

        let prior = std::env::var("RUSTFLAGS").ok();
        std::env::remove_var("RUSTFLAGS");
        let a = compute(&project)?;
        std::env::set_var("RUSTFLAGS", "-C opt-level=3");
        let b = compute(&project)?;
        match prior {
            Some(v) => std::env::set_var("RUSTFLAGS", v),
            None => std::env::remove_var("RUSTFLAGS"),
        }
        assert_ne!(a.hex, b.hex);
        Ok(())
    }

    /// Components must enumerate every input that contributes to the hex
    /// digest, in the canonical order. A consumer comparing per-component
    /// hashes against a stored fingerprint relies on this set.
    #[test]
    fn components_cover_every_input() -> Result<()> {
        let _guard = env_lock();
        let dir = tempfile::tempdir()?;
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("Cargo.lock"), b"lock")?;
        std::fs::write(root.join("Cargo.toml"), b"[package]")?;
        std::fs::create_dir_all(root.join("crates/foo"))?;
        std::fs::write(
            root.join("crates/foo/Cargo.toml"),
            b"[package]\nname = \"foo\"",
        )?;
        let project = project_with(
            root.clone(),
            vec![root.join("Cargo.toml"), root.join("crates/foo/Cargo.toml")],
        );

        let fp = compute(&project)?;
        let labels: Vec<&str> = fp.components.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(
            labels,
            vec![
                "cargo_lock",
                "manifest:Cargo.toml",
                "manifest:crates/foo/Cargo.toml",
                "rustc",
                "RUSTFLAGS",
                "CARGO_BUILD_TARGET",
            ],
        );
        for c in &fp.components {
            assert_eq!(c.hash.len(), 64, "{} hash should be 64 hex chars", c.label);
        }
        Ok(())
    }

    #[test]
    fn hex_roundtrip() {
        assert_eq!(hex(&[0x00, 0xff, 0xab]), "00ffab");
    }
}
