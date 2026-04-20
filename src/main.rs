//! cargo-difftest: Run only the tests affected by your changes.
//!
//! Uses LLVM coverage data to map each test to the source files it touches,
//! then queries git for changed files to select which tests to rerun.

mod collect;
mod coverage;
mod db;
mod project;
mod run;
mod status;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

/// Run only the tests affected by your changes.
///
/// Cargo subcommand that uses LLVM coverage to determine which tests cover
/// which source files, then reruns only the tests touching changed files.
#[derive(Parser)]
#[command(name = "cargo-difftest", bin_name = "cargo-difftest")]
struct Cli {
    /// When invoked as `cargo difftest`, cargo passes "difftest" as the first arg.
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
    },
    /// Show stored coverage data and what would run for current changes.
    Status {
        /// Compare against a git ref (commit, branch, tag) instead of working tree changes.
        /// Uses three-dot diff (`<ref>...HEAD`) to find changes since diverging from the ref.
        #[arg(long)]
        diff_base: Option<String>,
    },
    /// Delete the .difftest.db coverage database.
    Clean,
}

fn clean() -> Result<()> {
    let project = project::find_project_root()?;
    let db_path = project.workspace_root.join(".difftest.db");
    if db_path.exists() {
        std::fs::remove_file(&db_path)
            .with_context(|| format!("failed to delete {}", db_path.display()))?;
        eprintln!("deleted {}", db_path.display());
    } else {
        eprintln!("no .difftest.db found");
    }
    Ok(())
}

fn main() {
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
        Action::Collect { diff_base } => {
            collect::collect(diff_base.as_deref())?;
            Ok(0)
        }
        Action::Run { diff_base, all } => run::run(diff_base.as_deref(), all),
        Action::Status { diff_base } => {
            status::status(diff_base.as_deref())?;
            Ok(0)
        }
        Action::Clean => {
            clean()?;
            Ok(0)
        }
    }
}
