//! `otter-test262` CLI entry point.
//!
//! Slice 101 ships the skeleton: corpus traversal, `--dry-run`, and
//! a refusal-to-launch check on the `vendor/test262` submodule.
//! Real metadata parsing / per-test execution / reports land with
//! slices 102 → 105.
//!
//! Spec links:
//! - <https://tc39.es/ecma262/>
//! - <https://github.com/tc39/test262/blob/main/INTERPRETING.md>
//! - `docs/new-engine/tasks/100-test262-conformance.md`

#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use otter_test262::runner::{CorpusError, ensure_corpus_present, list_tests};

/// Default per-test timeout in milliseconds (slice 103 wires the
/// real watchdog; slice 101 only stores the value).
const DEFAULT_TIMEOUT_MS: u64 = 30_000;

/// Default per-test heap cap (512 MiB).
const DEFAULT_MAX_HEAP_BYTES: u64 = 512 * 1024 * 1024;

#[derive(Parser, Debug)]
#[command(
    name = "otter-test262",
    about = "Test262 conformance runner for the new-engine Otter stack.",
    long_about = "Drives the tc39/test262 corpus through the active otter-runtime / \
                  otter-vm stack and publishes a versioned baseline. Slice 101 only \
                  wires the skeleton — see docs/new-engine/tasks/100-test262-conformance.md."
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
    /// Run the corpus (slice 101: only `--dry-run` is wired).
    Run(RunArgs),
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
    /// anything. The only mode wired in slice 101.
    #[arg(long)]
    dry_run: bool,

    /// Print the histogram of `features:` tokens encountered in the
    /// corpus (lands in slice 102).
    #[arg(long)]
    collect_features: bool,
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
        Command::Diff(args) => {
            // Slice 104 wires the real diff. Slice 101 surfaces
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

    let _timeout_ms = args.timeout.unwrap_or_else(|| {
        std::env::var("OTTER_TEST262_TIMEOUT_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_TIMEOUT_MS)
    });
    let _max_heap_bytes = args.max_heap_bytes.unwrap_or_else(|| {
        std::env::var("OTTER_TEST262_HEAP_BYTES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_MAX_HEAP_BYTES)
    });

    if args.collect_features {
        eprintln!("error: --collect-features lands in slice 102.");
        return Ok(ExitCode::from(2));
    }

    if !args.dry_run {
        eprintln!(
            "error: only --dry-run is wired in slice 101. Real execution lands with slice 103."
        );
        return Ok(ExitCode::from(2));
    }

    let tests = list_tests(&paths, args.filter.as_deref());
    println!("total: {}", tests.len());
    if let Some(_path) = args.output {
        eprintln!("note: --output lands with slice 104; nothing was written.");
    }
    Ok(ExitCode::SUCCESS)
}
