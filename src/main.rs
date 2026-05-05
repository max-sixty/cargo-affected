//! cargo-affected: Run only the tests affected by your changes.
//!
//! Uses LLVM coverage data to map each test to the source files it touches,
//! then queries git for changed files to select which tests to rerun.

mod collect;
mod coverage;
mod db;
mod fingerprint;
mod project;
mod report;
mod run;
mod selection;
#[cfg(unix)]
mod shim;
mod status;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};

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
        /// Extra args forwarded to `cargo nextest run`. Must be preceded by
        /// `--` (e.g. `cargo affected collect -- --features foo`); otherwise
        /// clap rejects unknown flags rather than risk swallowing one of
        /// cargo-affected's own (e.g. `--verbose`).
        #[arg(last = true)]
        nextest_args: Vec<String>,
    },
    /// Run only tests affected by current git changes.
    Run {
        /// Run all tests, skipping coverage-based selection.
        #[arg(long)]
        all: bool,
        /// Write a structured JSON diagnostic report to PATH after
        /// selection but before invoking nextest. The report names
        /// the cache status, which inputs differ from any cached
        /// snapshot (on miss), and per-file/per-test selection
        /// reasoning. See docs/report-json.md for the schema.
        #[arg(long, value_name = "PATH")]
        report_json: Option<PathBuf>,
        /// Detail level for the report's selection section. `summary`
        /// keeps per-file aggregate counts only (bounded);
        /// `full` adds per-test reason vectors (can be megabytes on
        /// large test suites).
        #[arg(long, value_enum, default_value_t = ReportDetail::Summary)]
        report_detail: ReportDetail,
        /// Extra args forwarded to `cargo nextest run`. Must be preceded by
        /// `--` (e.g. `cargo affected run -- --features foo`); otherwise
        /// clap rejects unknown flags rather than risk swallowing one of
        /// cargo-affected's own (e.g. `--verbose`).
        #[arg(last = true)]
        nextest_args: Vec<String>,
    },
    /// Show stored coverage data and what would run for current changes.
    Status {
        /// Write a structured JSON diagnostic report to PATH. See
        /// docs/report-json.md for the schema.
        #[arg(long, value_name = "PATH")]
        report_json: Option<PathBuf>,
        /// Detail level for the report's selection section.
        #[arg(long, value_enum, default_value_t = ReportDetail::Summary)]
        report_detail: ReportDetail,
    },
    /// Clear stored coverage data from target/affected/coverage.db.
    Clean,
}

/// Mirrors `selection::DiagnosticDetail` at the CLI surface so clap can
/// derive `ValueEnum` without importing into the selection module.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum ReportDetail {
    /// Per-file aggregate counters only.
    Summary,
    /// Per-test reason vectors plus per-file aggregates.
    Full,
}

impl From<ReportDetail> for selection::DiagnosticDetail {
    fn from(d: ReportDetail) -> Self {
        match d {
            ReportDetail::Summary => Self::Summary,
            ReportDetail::Full => Self::Full,
        }
    }
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
        Action::Run {
            all,
            report_json,
            report_detail,
            nextest_args,
        } => run::run(
            all,
            verbose,
            report_json.as_deref(),
            report_detail.into(),
            &nextest_args,
        ),
        Action::Status {
            report_json,
            report_detail,
        } => {
            status::status(verbose, report_json.as_deref(), report_detail.into())?;
            Ok(0)
        }
        Action::Clean => {
            clean()?;
            Ok(0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    /// `cargo affected` is invoked by cargo as `cargo-affected affected …`;
    /// `main` strips the redundant `affected` slot before clap, so tests
    /// inject the post-strip argv directly.
    fn parse(args: &[&str]) -> Cli {
        let mut argv = vec!["cargo-affected"];
        argv.extend(args);
        Cli::parse_from(argv)
    }

    /// `try_parse` variant for the error-case tests below — `Cli::parse_from`
    /// would `process::exit` on a clap error and abort the test run.
    fn try_parse(args: &[&str]) -> Result<Cli, clap::Error> {
        let mut argv = vec!["cargo-affected"];
        argv.extend(args);
        Cli::try_parse_from(argv)
    }

    #[test]
    fn run_rejects_unknown_flags_without_double_dash() {
        // Strict parsing: anything clap doesn't recognise must come after `--`.
        // The looser "sweep trailing args into nextest_args" behavior would
        // silently steal cargo-affected's own flags (e.g. `--verbose`).
        let err = try_parse(&["run", "--features", "shell-integration-tests"])
            .err()
            .expect("expected clap to reject unknown flag without --");
        assert_eq!(err.kind(), clap::error::ErrorKind::UnknownArgument);
    }

    #[test]
    fn collect_rejects_unknown_flags_without_double_dash() {
        let err = try_parse(&["collect", "--features", "foo"])
            .err()
            .expect("expected clap to reject unknown flag without --");
        assert_eq!(err.kind(), clap::error::ErrorKind::UnknownArgument);
    }

    #[test]
    fn run_accepts_explicit_double_dash_separator() {
        // Args after `--` are forwarded verbatim; the `--` itself is consumed
        // by the parser.
        let cli = parse(&["run", "--", "--features", "foo"]);
        let Action::Run { nextest_args, .. } = cli.action else {
            panic!("expected Run");
        };
        assert_eq!(nextest_args, vec!["--features", "foo"]);
    }

    #[test]
    fn collect_accepts_explicit_double_dash_separator() {
        let cli = parse(&["collect", "--", "--features", "foo"]);
        let Action::Collect {
            diff,
            allow_dirty,
            nextest_args,
        } = cli.action
        else {
            panic!("expected Collect");
        };
        assert!(!diff);
        assert!(!allow_dirty);
        assert_eq!(nextest_args, vec!["--features", "foo"]);
    }

    #[test]
    fn run_keeps_global_verbose_for_cargo_affected() {
        // Without `--`, `--verbose` is unambiguously cargo-affected's global
        // flag — the strict parse stops nextest from stealing it.
        let cli = parse(&["run", "--verbose"]);
        assert!(cli.verbose);
        let Action::Run { nextest_args, .. } = cli.action else {
            panic!("expected Run");
        };
        assert!(nextest_args.is_empty());
    }

    #[test]
    fn run_forwards_verbose_to_nextest_after_double_dash() {
        // If you really want nextest to see `--verbose`, put it after `--`.
        let cli = parse(&["run", "--", "--verbose"]);
        assert!(!cli.verbose);
        let Action::Run { nextest_args, .. } = cli.action else {
            panic!("expected Run");
        };
        assert_eq!(nextest_args, vec!["--verbose"]);
    }

    #[test]
    fn global_verbose_before_subcommand() {
        let cli = parse(&["--verbose", "run"]);
        assert!(cli.verbose);
        let Action::Run { nextest_args, .. } = cli.action else {
            panic!("expected Run");
        };
        assert!(nextest_args.is_empty());
    }

    #[test]
    fn known_run_flags_parse_before_double_dash() {
        // Subcommand-owned flags still parse normally before `--`.
        let cli = parse(&["run", "--all", "--", "--features", "foo"]);
        let Action::Run { all, nextest_args, .. } = cli.action else {
            panic!("expected Run");
        };
        assert!(all);
        assert_eq!(nextest_args, vec!["--features", "foo"]);
    }
}
