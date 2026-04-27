//! Environment fingerprinting.
//!
//! Produces a stable hash of every input that would globally invalidate cached
//! coverage data: `Cargo.lock`, all workspace `Cargo.toml` files, the rustc
//! version, and build-flag env vars. Stored alongside each `(test, file)`
//! mapping so queries scoped to the current fingerprint naturally miss when
//! any tracked input has changed — no explicit invalidation path needed.

use std::fmt::Write;
use std::process::Command;

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

use crate::project::ProjectRoot;

/// Compute a hex-encoded SHA-256 fingerprint of the build environment.
///
/// Each input is hashed with a named prefix and a null separator so collisions
/// across categories aren't possible (e.g. an env var containing "rustc:..."
/// can't look like a rustc version).
pub fn compute(project: &ProjectRoot) -> Result<String> {
    let mut hasher = Sha256::new();

    hash_file(
        &mut hasher,
        "cargo_lock",
        &project.workspace_root.join("Cargo.lock"),
    )?;

    for manifest in &project.manifest_paths {
        let label = manifest
            .strip_prefix(&project.workspace_root)
            .unwrap_or(manifest)
            .to_string_lossy();
        hash_file(&mut hasher, &format!("manifest:{label}"), manifest)?;
    }

    hash_named(&mut hasher, "rustc", &rustc_version_bytes()?);
    hash_named(&mut hasher, "RUSTFLAGS", env_var("RUSTFLAGS").as_bytes());
    hash_named(
        &mut hasher,
        "CARGO_BUILD_TARGET",
        env_var("CARGO_BUILD_TARGET").as_bytes(),
    );

    Ok(hex(&hasher.finalize()))
}

/// Hash a category label and its bytes with a null separator.
fn hash_named(hasher: &mut Sha256, label: &str, bytes: &[u8]) {
    hasher.update(label.as_bytes());
    hasher.update(b":");
    hasher.update(bytes);
    hasher.update(b"\0");
}

/// Hash a file's contents under a category label. Missing files hash as empty —
/// their appearance/disappearance still changes the fingerprint via the label.
/// Other I/O errors (permission denied, etc.) bubble up so we don't silently
/// produce a fingerprint that matches a different state.
fn hash_file(hasher: &mut Sha256, label: &str, path: &std::path::Path) -> Result<()> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => {
            return Err(anyhow::Error::from(e).context(format!("failed to read {}", path.display())))
        }
    };
    hash_named(hasher, label, &bytes);
    Ok(())
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
    fn same_inputs_same_hash() -> Result<()> {
        let _guard = env_lock();
        let dir = tempfile::tempdir()?;
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("Cargo.lock"), b"lock contents")?;
        std::fs::write(root.join("Cargo.toml"), b"[package]\nname = \"x\"\n")?;
        let project = project_with(root.clone(), vec![root.join("Cargo.toml")]);

        let a = compute(&project)?;
        let b = compute(&project)?;
        assert_eq!(a, b);
        assert_eq!(a.len(), 64); // SHA-256 hex
        Ok(())
    }

    #[test]
    fn cargo_lock_change_changes_hash() -> Result<()> {
        let _guard = env_lock();
        let dir = tempfile::tempdir()?;
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("Cargo.toml"), b"[package]\n")?;
        let project = project_with(root.clone(), vec![root.join("Cargo.toml")]);

        std::fs::write(root.join("Cargo.lock"), b"v1")?;
        let a = compute(&project)?;
        std::fs::write(root.join("Cargo.lock"), b"v2")?;
        let b = compute(&project)?;
        assert_ne!(a, b);
        Ok(())
    }

    #[test]
    fn manifest_change_changes_hash() -> Result<()> {
        let _guard = env_lock();
        let dir = tempfile::tempdir()?;
        let root = dir.path().to_path_buf();
        let manifest = root.join("Cargo.toml");
        let project = project_with(root.clone(), vec![manifest.clone()]);

        std::fs::write(&manifest, b"[package]\nname = \"a\"\n")?;
        let a = compute(&project)?;
        std::fs::write(&manifest, b"[package]\nname = \"b\"\n")?;
        let b = compute(&project)?;
        assert_ne!(a, b);
        Ok(())
    }

    #[test]
    fn rustflags_change_changes_hash() -> Result<()> {
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
        assert_ne!(a, b);
        Ok(())
    }

    #[test]
    fn hex_roundtrip() {
        assert_eq!(hex(&[0x00, 0xff, 0xab]), "00ffab");
    }
}
