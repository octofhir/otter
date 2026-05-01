//! `otter-test262` CLI entry point.
//!
//! Slice 101 ships the skeleton (corpus traversal, `--dry-run`, and
//! a refusal-to-launch check on the `vendor/test262` submodule).
//! Slice 102 adds the YAML frontmatter parser, the `parse`
//! subcommand, and the `--collect-features` histogram. Real
//! per-test execution / reports / sharding land with slices 103 →
//! 105.
//!
//! Spec links:
//! - <https://tc39.es/ecma262/>
//! - <https://github.com/tc39/test262/blob/main/INTERPRETING.md>
//! - `docs/new-engine/tasks/100-test262-conformance.md`

#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use indicatif::{ProgressBar, ProgressStyle};

use otter_test262::config::Test262Config;
use otter_test262::harness::HarnessCache;
use otter_test262::metadata::Frontmatter;
use otter_test262::runner::{
    CorpusError, ExecConfig, Outcome, ensure_corpus_present, list_tests, run_one,
};

/// Default per-test timeout in milliseconds. 30 s is the absolute
/// upper bound documented in `MEMORY.md::feedback_no_long_test262`;
/// the runner picks the lower 5 s for ordinary local development.
const DEFAULT_TIMEOUT_MS: u64 = 5_000;
/// Hard cap — the runner refuses values larger than this unless the
/// caller explicitly raises it via `OTTER_TEST262_TIMEOUT_MS`. Keeps
/// accidental 60-s timeouts from creeping in via CLI typos.
const MAX_TIMEOUT_MS: u64 = 30_000;

/// Default per-test heap cap (512 MiB).
const DEFAULT_MAX_HEAP_BYTES: u64 = 512 * 1024 * 1024;

#[derive(Parser, Debug)]
#[command(
    name = "otter-test262",
    about = "Test262 conformance runner for the new-engine Otter stack.",
    long_about = "Drives the tc39/test262 corpus through the active otter-runtime / \
                  otter-vm stack and publishes a versioned baseline. See \
                  docs/new-engine/tasks/100-test262-conformance.md."
)]
struct Cli {
    /// Path to the repository root. Defaults to the current
    /// working directory; the runner anchors `vendor/test262/` and
    /// `test262_config.toml` against it.
    #[arg(long, global = true)]
    repo_root: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run the corpus (slice 101: `--dry-run`; slice 102:
    /// `--collect-features`; real execution lands in slice 103).
    Run(RunArgs),
    /// Pretty-print a single test's frontmatter (slice 102).
    Parse(ParseArgs),
    /// Diff a freshly produced report against an earlier baseline
    /// (lands in slice 104).
    Diff(DiffArgs),
}

#[derive(Parser, Debug)]
struct RunArgs {
    /// Substring filter applied to each test path relative to
    /// `vendor/test262/test/`.
    #[arg(long)]
    filter: Option<String>,

    /// `--shard N/M` (lands in slice 104).
    #[arg(long)]
    shard: Option<String>,

    /// Per-test wall-clock timeout in milliseconds. Defaults to
    /// `OTTER_TEST262_TIMEOUT_MS` if set, else 30 s.
    #[arg(long)]
    timeout: Option<u64>,

    /// Per-test heap cap in bytes (`0` disables the cap). Defaults
    /// to `OTTER_TEST262_HEAP_BYTES` if set, else 512 MiB.
    #[arg(long)]
    max_heap_bytes: Option<u64>,

    /// Where to write the JSON report (lands in slice 104).
    #[arg(long)]
    output: Option<PathBuf>,

    /// Optional `test262_config.toml` path.
    #[arg(long)]
    config: Option<PathBuf>,

    /// Walk the corpus and print the test count without executing
    /// anything.
    #[arg(long)]
    dry_run: bool,
}

#[derive(Parser, Debug)]
struct ParseArgs {
    /// Path to a single Test262 test file.
    path: PathBuf,
}

#[derive(Parser, Debug)]
struct DiffArgs {
    /// Path to the previous baseline (`*.json`).
    previous: PathBuf,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match dispatch(cli) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::from(2)
        }
    }
}

fn dispatch(cli: Cli) -> Result<ExitCode> {
    let repo_root = cli
        .repo_root
        .clone()
        .unwrap_or_else(|| std::env::current_dir().expect("cwd should be readable"));
    match cli.command {
        Command::Run(args) => run(&repo_root, args),
        Command::Parse(args) => parse(args),
        Command::Diff(args) => {
            // Slice 104 wires the real diff. Earlier slices surface
            // an actionable stub so users see the gate.
            let _ = args;
            eprintln!("error: `diff` subcommand lands in slice 104.");
            Ok(ExitCode::from(2))
        }
    }
}

fn run(repo_root: &std::path::Path, args: RunArgs) -> Result<ExitCode> {
    let paths = match ensure_corpus_present(repo_root) {
        Ok(paths) => paths,
        Err(CorpusError::Missing { ref root }) | Err(CorpusError::Empty { ref root }) => {
            eprintln!(
                "error: vendor/test262 is not initialised at {}",
                root.display()
            );
            eprintln!("       run: git submodule update --init --recursive vendor/test262");
            return Ok(ExitCode::from(2));
        }
        Err(other) => return Err(other).context("failed to locate test262 corpus"),
    };

    let timeout_ms = args.timeout.unwrap_or_else(|| {
        std::env::var("OTTER_TEST262_TIMEOUT_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_TIMEOUT_MS)
    });
    if timeout_ms > MAX_TIMEOUT_MS {
        eprintln!(
            "error: --timeout {timeout_ms} ms exceeds the {MAX_TIMEOUT_MS} ms cap — \
             see MEMORY.md::feedback_no_long_test262."
        );
        return Ok(ExitCode::from(2));
    }
    let max_heap_bytes = args.max_heap_bytes.unwrap_or_else(|| {
        std::env::var("OTTER_TEST262_HEAP_BYTES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_MAX_HEAP_BYTES)
    });

    let config = Test262Config::load_or_default(args.config.as_deref());

    let tests = list_tests(&paths, args.filter.as_deref());
    if args.dry_run {
        println!("total: {}", tests.len());
        return Ok(ExitCode::SUCCESS);
    }

    if let Some(_path) = args.output {
        eprintln!("note: --output lands with slice 104; nothing is written this slice.");
    }

    execute_in_process(&paths, &tests, &config, timeout_ms, max_heap_bytes)
}

/// Drive every test in `tests` through [`run_one`] in-process and
/// print a roll-up summary. Slice 104 swaps this for the
/// process-isolated worker model + JSON / Markdown writers.
fn execute_in_process(
    paths: &otter_test262::runner::CorpusPaths,
    tests: &[PathBuf],
    config: &Test262Config,
    timeout_ms: u64,
    max_heap_bytes: u64,
) -> Result<ExitCode> {
    let mut harness = HarnessCache::new(&paths.harness_dir);
    if let Err(err) = harness.prewarm() {
        eprintln!("error: failed to prewarm harness: {err}");
        return Ok(ExitCode::from(2));
    }

    let exec = ExecConfig {
        timeout: Duration::from_millis(timeout_ms),
        max_heap_bytes,
        config: config.clone(),
    };

    let pb = ProgressBar::new(tests.len() as u64);
    pb.set_style(
        ProgressStyle::default_bar()
            .template("{spinner:.green} [{bar:40.cyan/blue}] {pos}/{len} ({percent}%) | {msg}")
            .expect("progress bar template should compile")
            .progress_chars("#>-"),
    );
    pb.set_message("starting");

    let mut totals = Totals::default();
    let start = Instant::now();
    for path in tests {
        let result = run_one(path, paths, &mut harness, &exec);
        totals.record(&result.outcome);
        pb.set_message(format!(
            "P{} F{} S{} T{} O{} C{}",
            totals.pass, totals.fail, totals.skipped, totals.timeout, totals.oom, totals.crash
        ));
        pb.inc(1);
    }
    pb.finish_and_clear();

    let elapsed = start.elapsed();
    println!(
        "test262: {} tests, {} pass, {} fail, {} skip, {} timeout, {} OOM, {} crash in {:.1}s",
        totals.total,
        totals.pass,
        totals.fail,
        totals.skipped,
        totals.timeout,
        totals.oom,
        totals.crash,
        elapsed.as_secs_f64()
    );

    // Slice 105 wires the regression gate; for now exit code is
    // non-zero only on crash so a hard-failed run still surfaces.
    if totals.crash > 0 {
        return Ok(ExitCode::from(1));
    }
    Ok(ExitCode::SUCCESS)
}

#[derive(Debug, Default)]
struct Totals {
    total: u64,
    pass: u64,
    fail: u64,
    skipped: u64,
    timeout: u64,
    oom: u64,
    crash: u64,
}

impl Totals {
    fn record(&mut self, outcome: &Outcome) {
        self.total += 1;
        match outcome {
            Outcome::Pass => self.pass += 1,
            Outcome::Fail { .. } => self.fail += 1,
            Outcome::Skipped { .. } => self.skipped += 1,
            Outcome::Timeout { .. } => self.timeout += 1,
            Outcome::OutOfMemory { .. } => self.oom += 1,
            Outcome::Crash { .. } => self.crash += 1,
        }
    }
}

/// Implementation of `parse <path>` — read one test, parse the
/// frontmatter, pretty-print as JSON.
fn parse(args: ParseArgs) -> Result<ExitCode> {
    let source = std::fs::read_to_string(&args.path)
        .with_context(|| format!("failed to read {}", args.path.display()))?;
    let fm = match Frontmatter::parse(&source) {
        Ok(fm) => fm,
        Err(err) => {
            eprintln!("error: {err}");
            return Ok(ExitCode::from(1));
        }
    };
    let json = serde_json::to_string_pretty(&fm).context("failed to serialise frontmatter")?;
    println!("{json}");
    Ok(ExitCode::SUCCESS)
}
