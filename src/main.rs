//! cargo-difftest: Run only the tests affected by your changes.
//!
//! Uses LLVM coverage data to map each test to the source files it touches,
//! then queries git for changed files to select which tests to rerun.

mod collect;
mod coverage;
mod db;
mod fingerprint;
mod project;
mod run;
mod shim;
mod status;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

/// Run only the tests affected by your changes.
#[derive(Parser)]
#[command(name = "cargo-difftest", bin_name = "cargo-difftest")]
struct Cli {
    #[command(subcommand)]
    command: CargoSubcommand,
}

#[derive(Subcommand)]
enum CargoSubcommand {
    /// The actual difftest subcommand (invoked as `cargo difftest <action>`).
    Difftest {
        #[command(subcommand)]
        action: Action,
    },
}

#[derive(Subcommand)]
enum Action {
    /// Collect coverage data for all tests and store in the database.
    Collect {
        /// Only re-collect tests covering files changed since this git ref.
        /// Discovers new tests too. Much faster than a full collection.
        #[arg(long)]
        diff_base: Option<String>,
        /// Extra args forwarded to `cargo nextest run`. Separate with `--`.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        nextest_args: Vec<String>,
    },
    /// Run only tests affected by current git changes.
    Run {
        /// Compare against a git ref (commit, branch, tag) instead of working tree changes.
        /// Uses three-dot diff (`<ref>...HEAD`) to find changes since diverging from the ref.
        #[arg(long)]
        diff_base: Option<String>,
        /// Run all tests, skipping coverage-based selection.
        #[arg(long)]
        all: bool,
        /// List every selected test name before running. Default prints only a count.
        #[arg(short, long)]
        verbose: bool,
    },
    /// Show stored coverage data and what would run for current changes.
    Status {
        /// Compare against a git ref (commit, branch, tag) instead of working tree changes.
        /// Uses three-dot diff (`<ref>...HEAD`) to find changes since diverging from the ref.
        #[arg(long)]
        diff_base: Option<String>,
        /// List every selected test name. Default prints only a count.
        #[arg(short, long)]
        verbose: bool,
    },
    /// Delete the target/difftest/coverage.db coverage database.
    Clean,
}

fn clean() -> Result<()> {
    let project = project::find_project_root()?;
    let path = db::db_path(&project.workspace_root);
    if path.exists() {
        std::fs::remove_file(&path)
            .with_context(|| format!("failed to delete {}", path.display()))?;
        eprintln!("deleted {}", path.display());
    } else {
        eprintln!("no coverage database found");
    }
    Ok(())
}

fn main() {
    // `runner-shim` is the hidden per-test coverage runner invoked by cargo/nextest
    // via CARGO_TARGET_<TRIPLE>_RUNNER. Dispatch before clap — its trailing args
    // include `--exact`/`--list`/etc. which clap would interpret if we let it.
    let argv: Vec<String> = std::env::args().collect();
    if argv.get(1).map(String::as_str) == Some("runner-shim") {
        shim::run(&argv[2..]);
    }

    let cli = Cli::parse();
    let CargoSubcommand::Difftest { action } = cli.command;

    let exit_code = match run_action(action) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("Error: {e:#}");
            1
        }
    };
    std::process::exit(exit_code);
}

fn run_action(action: Action) -> Result<i32> {
    match action {
        Action::Collect { diff_base, nextest_args } => {
            collect::collect(diff_base.as_deref(), &nextest_args)
        }
        Action::Run {
            diff_base,
            all,
            verbose,
        } => run::run(diff_base.as_deref(), all, verbose),
        Action::Status { diff_base, verbose } => {
            status::status(diff_base.as_deref(), verbose)?;
            Ok(0)
        }
        Action::Clean => {
            clean()?;
            Ok(0)
        }
    }
}
