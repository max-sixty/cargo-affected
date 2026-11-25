//! LLVM profile persistence modes. Continuous mode maps relocated counters
//! into a raw profile, so terminating a process need not run exit handlers.

use std::ffi::OsString;
use std::path::Path;
use std::process::Command;

use anyhow::{bail, Result};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum ProfileMode {
    #[default]
    Exit,
    Continuous,
}

impl ProfileMode {
    pub(crate) fn from_flag(continuous: bool) -> Self {
        if continuous {
            Self::Continuous
        } else {
            Self::Exit
        }
    }

    pub(crate) fn check_platform(self) -> Result<()> {
        if self == Self::Continuous && !cfg!(target_os = "linux") {
            bail!("continuous coverage is currently validated only on native Linux");
        }
        Ok(())
    }

    pub(crate) fn fingerprint(self) -> &'static [u8] {
        match self {
            Self::Exit => b"executable-maps-v2",
            Self::Continuous => b"executable-maps-v2-continuous",
        }
    }

    pub(crate) fn runner(self) -> &'static str {
        match self {
            Self::Exit => "runner-shim",
            Self::Continuous => "runner-shim-continuous",
        }
    }

    pub(crate) fn profile_name(self) -> &'static str {
        match self {
            Self::Exit => "%p-%m.profraw",
            Self::Continuous => "%p-%m%c.profraw",
        }
    }

    fn args(self) -> &'static [&'static str] {
        match self {
            Self::Exit => &["-C", "instrument-coverage"],
            Self::Continuous => &[
                "-C",
                "instrument-coverage",
                "-C",
                "llvm-args=-runtime-counter-relocation",
            ],
        }
    }
}

/// Cargo gives encoded flags precedence. Append instrumentation to the
/// actual effective flags, preserving argument boundaries and caller flags.
pub(crate) struct InstrumentFlags {
    plain: OsString,
    encoded: Option<OsString>,
}

impl InstrumentFlags {
    pub(crate) fn new(mode: ProfileMode) -> Self {
        let mut plain = std::env::var_os("RUSTFLAGS").unwrap_or_default();
        for flag in mode.args() {
            if !plain.is_empty() {
                plain.push(" ");
            }
            plain.push(flag);
        }
        let encoded = std::env::var_os("CARGO_ENCODED_RUSTFLAGS").map(|mut flags| {
            for flag in mode.args() {
                if !flags.is_empty() {
                    flags.push("\x1f");
                }
                flags.push(flag);
            }
            flags
        });
        Self { plain, encoded }
    }

    pub(crate) fn apply(&self, cmd: &mut Command) {
        cmd.env("RUSTFLAGS", &self.plain);
        if let Some(encoded) = &self.encoded {
            cmd.env("CARGO_ENCODED_RUSTFLAGS", encoded);
        }
    }
}

impl ProfileMode {
    /// Refuse to silently accept a runtime that ignores continuous mode.
    /// Build a tiny independent, dependency-free Cargo target with the same
    /// toolchain/flags and verify its counters after forced termination.
    /// Temporary build outputs are removed on both success and failure.
    pub(crate) fn verify(
        self,
        workspace: &Path,
        state: &Path,
        target: &str,
        flags: &InstrumentFlags,
        llvm_profdata: &Path,
    ) -> Result<()> {
        if self != Self::Continuous {
            return Ok(());
        }
        use anyhow::Context;
        use std::io::{BufRead, BufReader};
        use std::process::Stdio;

        let probe = tempfile::Builder::new()
            .prefix("continuous-probe-")
            .tempdir_in(state)?;
        let manifest = probe.path().join("Cargo.toml");
        let build_dir = probe.path().join("target");
        std::fs::create_dir(probe.path().join("src"))?;
        std::fs::write(&manifest, "[package]\nname = \"affected-continuous-probe\"\nversion = \"0.0.0\"\nedition = \"2021\"\n[workspace]\n")?;
        std::fs::write(
            probe.path().join("src/main.rs"),
            r#"
#[inline(never)]
fn continuous_probe_hit() -> u32 { std::hint::black_box(42) }
fn main() {
    println!("{}", continuous_probe_hit());
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).unwrap();
}
"#,
        )?;
        let mut build = Command::new("cargo");
        build
            .args(["build", "--offline", "--manifest-path"])
            .arg(&manifest)
            .arg("--target-dir")
            .arg(&build_dir)
            .arg("--target")
            .arg(target)
            .current_dir(workspace)
            .env(
                "LLVM_PROFILE_FILE",
                probe.path().join("build-%p-%m.profraw"),
            );
        flags.apply(&mut build);
        let output = build
            .output()
            .context("building continuous-profile capability probe")?;
        if !output.status.success() {
            bail!(
                "continuous profile probe build failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let binary = build_dir.join(target).join("debug").join(format!(
            "affected-continuous-probe{}",
            std::env::consts::EXE_SUFFIX
        ));
        let raw = probe.path().join("raw");
        std::fs::create_dir(&raw)?;
        let mut child = Command::new(binary)
            .current_dir(probe.path())
            .env("LLVM_PROFILE_FILE", raw.join(self.profile_name()))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("starting continuous-profile capability probe")?;
        let stdout = child.stdout.take().context("probe stdout missing")?;
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let reader = std::thread::spawn(move || {
            let mut ready = String::new();
            let result = BufReader::new(stdout).read_line(&mut ready).map(|_| ready);
            let _ = sender.send(result);
        });
        let ready = receiver.recv_timeout(std::time::Duration::from_secs(15));
        // Never let normal process exit flush the counters being tested.
        let killed = child.kill();
        let outcome = child
            .wait_with_output()
            .context("reaping continuous-profile capability probe")?;
        reader
            .join()
            .map_err(|_| anyhow::anyhow!("profile probe reader panicked"))?;
        let ready =
            ready.context("timed out waiting for continuous-profile capability probe")??;
        killed.context("terminating continuous-profile capability probe")?;
        if ready.trim() != "42" || outcome.status.success() {
            bail!("continuous profile probe did not reach its kill checkpoint");
        }
        let files = std::fs::read_dir(&raw)?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<std::io::Result<Vec<_>>>()?;
        if files.is_empty() {
            bail!(
                "LLVM continuous profiling did not persist counters before termination: {}",
                String::from_utf8_lossy(&outcome.stderr)
            );
        }
        let text = probe.path().join("profile.txt");
        let merged = Command::new(llvm_profdata)
            .args(["merge", "--sparse", "--text", "--failure-mode=any"])
            .args(files)
            .arg("-o")
            .arg(&text)
            .output()?;
        if !merged.status.success() {
            bail!(
                "continuous profile probe cannot be merged: {}",
                String::from_utf8_lossy(&merged.stderr)
            );
        }
        let profile = std::fs::read_to_string(text)?;
        if !crate::coverage::executed_functions(&profile)?
            .iter()
            .any(|name| name.contains("continuous_probe_hit"))
        {
            bail!("continuous profile probe lost its executed function counter");
        }
        eprintln!("verified continuous LLVM profile persistence after process termination");
        Ok(())
    }
}
