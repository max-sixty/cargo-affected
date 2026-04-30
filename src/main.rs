//! cargo-affected: Run only the tests affected by your changes.
//!
//! Uses LLVM coverage data to map each test to the source files it touches,
//! then queries git for changed files to select which tests to rerun.

mod collect;
mod coverage;
mod db;
mod fingerprint;
mod project;
mod run;
mod selection;
#[cfg(unix)]
mod shim;
mod status;

use anyhow::Result;
use clap::{Parser, Subcommand};

/// Run only the tests affected by your changes.
#[derive(Parser)]
#[command(
    name = "cargo-affected",
    bin_name = "cargo affected",
    version,
    disable_help_subcommand = true,
    arg_required_else_help = true,
)]
struct Cli {
    /// Print extra output: pipeline internals during `collect`, every
    /// selected test name during `run`/`status`. Accepted at any position.
    #[arg(short, long, global = true)]
    verbose: bool,
    #[command(subcommand)]
    action: Action,
}

#[derive(Subcommand)]
enum Action {
    /// Collect coverage data for all tests and store in the database.
    Collect {
        /// Re-collect coverage only for tests affected by changes since the
        /// last collect, leaving rows for unaffected tests in place. Errors
        /// out if there's no prior collect for the current environment, or
        /// if any stored collect_sha is no longer reachable from HEAD.
        #[arg(long)]
        diff: bool,
        /// Collect against a dirty working tree. Stored line numbers reflect
        /// the working-tree files cargo compiled, but they're filed under
        /// `HEAD`'s sha — later diffs against `HEAD` will be out of phase
        /// and selection will silently mis-target. Use only for throwaway runs.
        #[arg(long)]
        allow_dirty: bool,
        /// Extra args forwarded to `cargo nextest run`. Separate with `--`.
        #[arg(last = true)]
        nextest_args: Vec<String>,
    },
    /// Run only tests affected by current git changes.
    Run {
        /// Run all tests, skipping coverage-based selection.
        #[arg(long)]
        all: bool,
        /// Extra args forwarded to `cargo nextest run`. Separate with `--`.
        #[arg(last = true)]
        nextest_args: Vec<String>,
    },
    /// Show stored coverage data and what would run for current changes.
    Status,
    /// Clear stored coverage data from target/affected/coverage.db.
    Clean,
}

fn clean() -> Result<()> {
    let project = project::find_project_root()?;
    let path = db::db_path(&project.workspace_root);
    if !path.exists() {
        eprintln!("no coverage database found");
        return Ok(());
    }
    // Clear via SQL rather than unlinking: the open + write lock waits out any
    // concurrent `collect`, so we can't silently orphan a mid-flight commit.
    let mut db = db::Db::open(&project.workspace_root)?;
    db.clear()?;
    eprintln!("cleared {}", path.display());
    Ok(())
}

fn main() {
    let mut argv: Vec<String> = std::env::args().collect();

    // `runner-shim` is the hidden per-test coverage runner invoked by cargo/nextest
    // via CARGO_TARGET_<TRIPLE>_RUNNER. Dispatch before clap — its trailing args
    // include `--exact`/`--list`/etc. which clap would interpret if we let it.
    // Unix-only — non-Unix targets fall through to clap and hit the platform
    // check in `run_action`.
    #[cfg(unix)]
    if argv.get(1).map(String::as_str) == Some("runner-shim") {
        shim::run(&argv[2..]);
    }

    // Cargo invokes us as `cargo-affected affected <args>`; strip the redundant
    // slot so clap sees a flat command rather than needing a wrapper subcommand.
    if argv.get(1).map(String::as_str) == Some("affected") {
        argv.remove(1);
    }

    let cli = Cli::parse_from(argv);

    let exit_code = match run_action(cli.action, cli.verbose) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("Error: {e:#}");
            1
        }
    };
    std::process::exit(exit_code);
}

fn run_action(action: Action, verbose: bool) -> Result<i32> {
    // The coverage pipeline relies on the unix-only runner shim (see `shim.rs`).
    // `status` and `clean` don't spawn it, so leave them working everywhere.
    #[cfg(not(unix))]
    if matches!(action, Action::Collect { .. } | Action::Run { .. }) {
        eprintln!("cargo-affected: mac & linux only at this stage — see README.");
        return Ok(1);
    }

    match action {
        Action::Collect {
            diff,
            allow_dirty,
            nextest_args,
        } => collect::collect(diff, verbose, allow_dirty, &nextest_args),
        Action::Run { all, nextest_args } => run::run(all, verbose, &nextest_args),
        Action::Status => {
            status::status(verbose)?;
            Ok(0)
        }
        Action::Clean => {
            clean()?;
            Ok(0)
        }
    }
}
