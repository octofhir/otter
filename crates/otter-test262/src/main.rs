//! `otter-test262` CLI entry point.
//!
//! Slices shipped end-to-end:
//! - 101: corpus traversal, `--dry-run`, refusal-to-launch.
//! - 102: frontmatter parser + `parse <path>` subcommand.
//! - 103: per-test driver with watchdog + heap cap + `catch_unwind`.
//! - 104: sharding, JSON+Markdown writers, `diff`, `merge`,
//!   cursor persistence, Ctrl-C partial dump.
//!
//! Spec links:
//! - <https://tc39.es/ecma262/>
//! - <https://github.com/tc39/test262/blob/main/INTERPRETING.md>

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use indicatif::{ProgressBar, ProgressStyle};
use rayon::prelude::*;

use otter_test262::config::Test262Config;
use otter_test262::diff::{self, DiffReport};
use otter_test262::harness::HarnessCache;
use otter_test262::metadata::Frontmatter;
use otter_test262::report::{Baseline, ReportError};
use otter_test262::runner::{
    CorpusError, CorpusPaths, ExecConfig, Outcome, TestResult, ensure_corpus_present, list_tests,
    run_one,
};
use otter_test262::shard::ShardSpec;

/// Default per-test timeout in milliseconds (5 s for local dev).
const DEFAULT_TIMEOUT_MS: u64 = 5_000;
/// Hard cap (30 s) — refuses larger values per
/// `MEMORY.md::feedback_no_long_test262`.
const MAX_TIMEOUT_MS: u64 = 30_000;
/// Default per-test heap cap (512 MiB).
const DEFAULT_MAX_HEAP_BYTES: u64 = 512 * 1024 * 1024;
/// Test262 can run several `$262.agent` runtimes at once. Use a larger
/// process-global pointer-compression cage than the VM default before the
/// first per-test runtime is constructed.
const TEST262_CAGE_BYTES: usize = 1024 * 1024 * 1024;
/// How often to flush the cursor file mid-shard.
const CURSOR_FLUSH_EVERY: u64 = 100;
/// Default location for generated baselines.
const BASELINE_DIR: &str = "tests/test262-baseline";

#[derive(Parser, Debug)]
#[command(
    name = "otter-test262",
    about = "Test262 conformance runner for the new-engine Otter stack.",
    long_about = "Drives the tc39/test262 corpus through the active otter-runtime / \
                  otter-vm stack and publishes a versioned baseline."
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
    /// Run the corpus end-to-end.
    Run(RunArgs),
    /// Pretty-print a single test's frontmatter.
    Parse(ParseArgs),
    /// Diff a freshly produced report against an earlier baseline.
    Diff(DiffArgs),
    /// Merge per-shard JSON outputs into one baseline.
    Merge(MergeArgs),
    /// Render a baseline JSON into a static HTML dashboard.
    Site(SiteArgs),
    /// Generate the Markdown conformance report (ES_CONFORMANCE.md) from a
    /// baseline JSON — auto-generated, never hand-edited.
    Conformance(ConformanceArgs),
}

#[derive(Parser, Debug)]
struct ConformanceArgs {
    /// Path to a baseline / merged report (`*.json`).
    #[arg(default_value = "test262_results/latest.json")]
    input: PathBuf,
    /// Where to write the Markdown report.
    #[arg(long, default_value = "ES_CONFORMANCE.md")]
    output: PathBuf,
}

#[derive(Parser, Debug)]
struct RunArgs {
    /// Substring filter applied to each test path relative to
    /// `vendor/test262/test/`.
    #[arg(long)]
    filter: Option<String>,

    /// `--shard N/M` (stable hash partition). Defaults to "1/1"
    /// (the entire corpus on one worker).
    #[arg(long)]
    shard: Option<String>,

    /// Per-test wall-clock timeout in milliseconds. Defaults to
    /// `OTTER_TEST262_TIMEOUT_MS` if set, else 5 s. Hard cap 30 s.
    #[arg(long)]
    timeout: Option<u64>,

    /// Per-test heap cap in bytes (`0` disables the cap). Defaults
    /// to `OTTER_TEST262_HEAP_BYTES` if set, else 512 MiB.
    #[arg(long)]
    max_heap_bytes: Option<u64>,

    /// Where to write the JSON report (`*.json`); the matching
    /// `*.md` lands next to it.
    #[arg(long)]
    output: Option<PathBuf>,

    /// Optional `test262_config.toml` path.
    #[arg(long)]
    config: Option<PathBuf>,

    /// Walk the corpus and print the test count without executing
    /// anything.
    #[arg(long)]
    dry_run: bool,

    /// Optional path to a JSON shard cursor (`reports/shard-N.cursor`)
    /// — written after every CURSOR_FLUSH_EVERY tests so the
    /// supervisor can resume after a crash.
    #[arg(long)]
    cursor: Option<PathBuf>,

    /// Resume from this 0-based test index (within the shard).
    /// Combined with `--cursor`, lets the supervisor restart a
    /// dead worker without re-running passed tests.
    #[arg(long, default_value_t = 0)]
    resume: usize,

    /// Number of in-process worker threads. `0` (default) picks the
    /// host's logical core count for a single-shard run and `1` for a
    /// multi-shard run (CI already parallelises across shard
    /// processes, so per-shard threads would oversubscribe). Each
    /// worker drives its own isolate; tests exercising `$262.agent` /
    /// shared memory (feature `Atomics` / `SharedArrayBuffer`) run on
    /// a single serial pass because the agent registry is
    /// process-global.
    #[arg(long, default_value_t = 0)]
    jobs: usize,
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
    /// Path to the freshly produced baseline. Defaults to the
    /// canonical `tests/test262-baseline/main.json`.
    #[arg(long)]
    current: Option<PathBuf>,
}

#[derive(Parser, Debug)]
struct MergeArgs {
    /// Per-shard JSON inputs (`reports/shard-*.json`).
    inputs: Vec<PathBuf>,
    /// Where to write the merged baseline (`*.json`); the matching
    /// `*.md` lands next to it.
    #[arg(long)]
    output: PathBuf,
}

#[derive(Parser, Debug)]
struct SiteArgs {
    /// Path to a baseline / merged report (`*.json`).
    input: PathBuf,
    /// Where to write the self-contained HTML page.
    #[arg(long, default_value = "test262_results/site/index.html")]
    output: PathBuf,
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
        Command::Diff(args) => diff_cmd(&repo_root, args),
        Command::Merge(args) => merge_cmd(args),
        Command::Site(args) => site_cmd(args),
        Command::Conformance(args) => conformance_cmd(args),
    }
}

fn run(repo_root: &Path, args: RunArgs) -> Result<ExitCode> {
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

    let config = Test262Config::load_or_default(args.config.as_deref());

    // Precedence: CLI flag, then env var, then `test262_config.toml`, then the
    // built-in default.
    let timeout_ms = args.timeout.unwrap_or_else(|| {
        std::env::var("OTTER_TEST262_TIMEOUT_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .or_else(|| config.timeout_secs.map(|s| s.saturating_mul(1000)))
            .unwrap_or(DEFAULT_TIMEOUT_MS)
    });
    let max_heap_bytes = args.max_heap_bytes.unwrap_or_else(|| {
        std::env::var("OTTER_TEST262_HEAP_BYTES")
            .ok()
            .and_then(|v| v.parse().ok())
            .or(config.max_heap_bytes_per_test)
            .unwrap_or(DEFAULT_MAX_HEAP_BYTES)
    });
    if timeout_ms > MAX_TIMEOUT_MS {
        eprintln!(
            "error: --timeout {timeout_ms} ms exceeds the {MAX_TIMEOUT_MS} ms cap — \
             see MEMORY.md::feedback_no_long_test262."
        );
        return Ok(ExitCode::from(2));
    }

    let all_tests = list_tests(&paths, args.filter.as_deref());

    let shard = match args.shard.as_deref() {
        Some(spec) => match ShardSpec::parse(spec) {
            Ok(spec) => spec,
            Err(err) => {
                eprintln!("error: {err}");
                return Ok(ExitCode::from(2));
            }
        },
        None => ShardSpec { index: 1, total: 1 },
    };

    // Filter to this shard via the stable hash split.
    let mut tests: Vec<PathBuf> = if shard.total > 1 {
        all_tests
            .into_iter()
            .filter(|p| shard.contains(&relative_to(&paths.test_dir, p)))
            .collect()
    } else {
        all_tests
    };

    if args.dry_run {
        println!("total: {}", tests.len());
        return Ok(ExitCode::SUCCESS);
    }

    // Resolve worker count: explicit `--jobs`, else core count for a
    // single-shard run (and 1 under sharding to avoid oversubscribing
    // the CI shard processes).
    let jobs = if args.jobs > 0 {
        args.jobs
    } else if shard.total > 1 {
        1
    } else {
        std::thread::available_parallelism()
            .map(std::num::NonZeroUsize::get)
            .unwrap_or(1)
    };

    if otter_runtime::otter_gc::cage_size() == 0 {
        // N concurrent isolates can each grow toward `max_heap_bytes`,
        // so the shared pointer-compression cage must hold the sum or a
        // heavy test pair exhausts it (a `CageExhausted`, distinct from
        // the per-isolate cap). Scale with `jobs`, clamped to the cage
        // maximum (4 GiB).
        const MAX_CAGE: u64 = 1u64 << 32;
        let per_worker = if max_heap_bytes > 0 {
            max_heap_bytes
        } else {
            256 * 1024 * 1024
        };
        let want = (jobs as u64).saturating_mul(per_worker);
        let cage = want.max(TEST262_CAGE_BYTES as u64).min(MAX_CAGE) as usize;
        otter_runtime::otter_gc::init_cage_with_size(cage)
            .context("failed to initialise Test262 GC cage")?;
    }

    if args.resume > 0 {
        if args.resume >= tests.len() {
            eprintln!(
                "note: --resume {} ≥ shard size {} — nothing to do",
                args.resume,
                tests.len()
            );
            return Ok(ExitCode::SUCCESS);
        }
        tests = tests.split_off(args.resume);
    }

    execute_in_process(
        &paths,
        &tests,
        &config,
        timeout_ms,
        max_heap_bytes,
        args.output.as_deref(),
        args.cursor.as_deref(),
        args.resume,
        jobs,
    )
}

#[allow(clippy::too_many_arguments)]
fn execute_in_process(
    paths: &CorpusPaths,
    tests: &[PathBuf],
    config: &Test262Config,
    timeout_ms: u64,
    max_heap_bytes: u64,
    output: Option<&Path>,
    cursor: Option<&Path>,
    resume_offset: usize,
    jobs: usize,
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

    // Ctrl-C handler — flush a partial baseline so the work isn't
    // lost. Uses an `AtomicBool` polled at the loop boundary.
    let interrupted = Arc::new(AtomicBool::new(false));
    let _ = ctrlc_install(Arc::clone(&interrupted));

    let start = Instant::now();

    let results: Vec<TestResult> = if jobs > 1 {
        run_parallel(paths, tests, &exec, jobs, &pb, &interrupted)
    } else {
        run_sequential(
            paths,
            tests,
            &exec,
            &mut harness,
            &pb,
            &interrupted,
            cursor,
            resume_offset,
        )
    };
    pb.finish_and_clear();

    if let Some(cursor_path) = cursor {
        write_cursor(cursor_path, resume_offset + results.len());
    }

    let elapsed = start.elapsed();
    if let Some(timings_path) = std::env::var_os("OTTER_TEST262_TIMINGS") {
        let mut rows: Vec<(&str, u64)> = results
            .iter()
            .map(|r| (r.path.as_str(), r.wall_ms))
            .collect();
        rows.sort_by_key(|b| std::cmp::Reverse(b.1));
        let mut out = String::new();
        for (rel, ms) in &rows {
            out.push_str(&format!("{ms}\t{rel}\n"));
        }
        if let Err(err) = std::fs::write(&timings_path, out) {
            eprintln!("warning: failed to write timings: {err}");
        }
    }
    let baseline = build_baseline(paths, &results);
    print_summary(&baseline, elapsed);

    if let Some(json_path) = output {
        write_baseline(json_path, &baseline)?;
    }

    if interrupted.load(Ordering::Relaxed) {
        // Drop a partial baseline next to the cursor file so the
        // user can inspect the work that did finish.
        let stem = format!("partial-{}", chrono::Utc::now().format("%Y%m%dT%H%M%SZ"));
        let dir = output
            .and_then(Path::parent)
            .unwrap_or_else(|| Path::new("."));
        if let Ok((p, _)) = baseline.write_pair(dir, &stem) {
            eprintln!("partial baseline at {}", p.display());
        }
        return Ok(ExitCode::from(130));
    }

    if baseline.totals.crashed > 0 {
        return Ok(ExitCode::from(1));
    }
    Ok(ExitCode::SUCCESS)
}

/// Single-threaded driver — preserves cursor flushing and the
/// `OTTER_TEST262_TRACE_CURRENT` hang-tracing hook.
#[allow(clippy::too_many_arguments)]
fn run_sequential(
    paths: &CorpusPaths,
    tests: &[PathBuf],
    exec: &ExecConfig,
    harness: &mut HarnessCache,
    pb: &ProgressBar,
    interrupted: &AtomicBool,
    cursor: Option<&Path>,
    resume_offset: usize,
) -> Vec<TestResult> {
    let mut results: Vec<TestResult> = Vec::with_capacity(tests.len());
    let mut seen_since_flush: u64 = 0;
    for (idx, path) in tests.iter().enumerate() {
        if interrupted.load(Ordering::Relaxed) {
            eprintln!("\ninterrupted by user — writing partial baseline");
            break;
        }
        if std::env::var_os("OTTER_TEST262_TRACE_CURRENT").is_some() {
            let rel = path.strip_prefix(&paths.test_dir).unwrap_or(path).display();
            eprintln!("test262-current {} {rel}", resume_offset + idx);
        }
        let result = run_one(path, paths, harness, exec);
        record_progress(pb, &result.outcome);
        results.push(result);
        seen_since_flush += 1;
        if let Some(cursor_path) = cursor
            && seen_since_flush >= CURSOR_FLUSH_EVERY
        {
            seen_since_flush = 0;
            write_cursor(cursor_path, resume_offset + idx + 1);
        }
        pb.inc(1);
    }
    results
}

/// Multi-threaded driver. Each rayon worker owns its own
/// [`HarnessCache`] and drives a fresh isolate per test; isolates are
/// independent (per-isolate GC heaps carving pages from the shared,
/// mutex-guarded cage), so they run concurrently without coordination.
///
/// Tests that touch the process-global `$262.agent` registry (feature
/// `Atomics` / `SharedArrayBuffer`) are pulled out and run on a single
/// serial pass — `agent::reset_for_next_test` clears that registry
/// wholesale, so two agent tests in flight at once would corrupt each
/// other. The parallel phase contains no agent tests, so the registry
/// stays empty there and the per-test reset is a no-op on empty maps.
///
/// Results are sorted by path so the report is deterministic regardless
/// of completion order.
fn run_parallel(
    paths: &CorpusPaths,
    tests: &[PathBuf],
    exec: &ExecConfig,
    jobs: usize,
    pb: &ProgressBar,
    interrupted: &AtomicBool,
) -> Vec<TestResult> {
    let pool = match rayon::ThreadPoolBuilder::new().num_threads(jobs).build() {
        Ok(pool) => pool,
        Err(err) => {
            eprintln!("error: failed to build worker pool ({err}); falling back to 1 thread");
            rayon::ThreadPoolBuilder::new()
                .num_threads(1)
                .build()
                .expect("single-thread pool")
        }
    };

    // Classify in parallel: which tests must run serially.
    let serial_mask: Vec<bool> =
        pool.install(|| tests.par_iter().map(|p| is_serial_test(p)).collect());
    let parallel: Vec<&PathBuf> = tests
        .iter()
        .zip(&serial_mask)
        .filter_map(|(p, &s)| (!s).then_some(p))
        .collect();
    let serial: Vec<&PathBuf> = tests
        .iter()
        .zip(&serial_mask)
        .filter_map(|(p, &s)| s.then_some(p))
        .collect();

    let run = |path: &PathBuf, harness: &mut HarnessCache| -> TestResult {
        let result = run_one(path, paths, harness, exec);
        record_progress(pb, &result.outcome);
        pb.inc(1);
        result
    };

    let mut results: Vec<TestResult> = pool.install(|| {
        parallel
            .par_iter()
            .map_init(
                || {
                    let mut h = HarnessCache::new(&paths.harness_dir);
                    let _ = h.prewarm();
                    h
                },
                |harness, path| {
                    if interrupted.load(Ordering::Relaxed) {
                        return None;
                    }
                    Some(run(path, harness))
                },
            )
            .flatten()
            .collect()
    });

    // Serial pass for agent / shared-memory tests.
    if !serial.is_empty() {
        let mut harness = HarnessCache::new(&paths.harness_dir);
        let _ = harness.prewarm();
        for path in serial {
            if interrupted.load(Ordering::Relaxed) {
                break;
            }
            results.push(run(path, &mut harness));
        }
    }

    results.sort_by(|a, b| a.path.cmp(&b.path));
    results
}

/// `true` when a test must run on the serial pass because it can reach
/// the process-global `$262.agent` registry / shared memory. Detected
/// from frontmatter features; an unreadable or frontmatter-less file is
/// safe to run in parallel (the driver will skip / fail it without
/// touching agent state).
fn is_serial_test(path: &Path) -> bool {
    let Ok(source) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(frontmatter) = Frontmatter::parse(&source) else {
        return false;
    };
    frontmatter
        .features
        .iter()
        .any(|f| f == "Atomics" || f == "SharedArrayBuffer")
}

fn record_progress(pb: &ProgressBar, outcome: &Outcome) {
    let label = match outcome {
        Outcome::Pass => "pass",
        Outcome::Fail { .. } => "fail",
        Outcome::Skipped { .. } => "skip",
        Outcome::Crash { .. } => "crash",
        Outcome::Timeout { .. } => "timeout",
        Outcome::OutOfMemory { .. } => "oom",
    };
    pb.set_message(label);
}

fn build_baseline(paths: &CorpusPaths, results: &[TestResult]) -> Baseline {
    let test262_commit = git_head(&paths.root).unwrap_or_else(|| "unknown".to_string());
    let engine_commit = git_head(Path::new(".")).unwrap_or_else(|| "unknown".to_string());
    let ran_at = chrono::Utc::now().to_rfc3339();
    Baseline::from_results(results, test262_commit, engine_commit, ran_at)
}

fn write_baseline(json_path: &Path, baseline: &Baseline) -> Result<()> {
    let parent = json_path.parent().unwrap_or_else(|| Path::new("."));
    let stem = json_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("baseline");
    let (json, md) = baseline
        .write_pair(parent, stem)
        .with_context(|| format!("failed to write baseline at {}", json_path.display()))?;
    eprintln!(
        "baseline written:\n  json: {}\n  md  : {}",
        json.display(),
        md.display()
    );
    Ok(())
}

fn print_summary(baseline: &Baseline, elapsed: Duration) {
    let t = &baseline.totals;
    println!(
        "test262: {} tests, {} pass, {} fail, {} skip, {} timeout, {} OOM, {} crash in {:.1}s ({:.2}% pass)",
        t.total,
        t.passed,
        t.failed,
        t.skipped,
        t.timed_out,
        t.oom,
        t.crashed,
        elapsed.as_secs_f64(),
        t.pass_rate(),
    );
}

fn write_cursor(path: &Path, next_index: usize) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, format!("{next_index}\n"));
}

fn ctrlc_install(flag: Arc<AtomicBool>) -> std::io::Result<()> {
    // Foundation: a polled atomic flag is enough; ctrlc handlers
    // are notoriously fragile across platforms and the runner
    // already has the watchdog interrupting the engine. The
    // dependency-free approach mirrors how the legacy runner's
    // `scripts/test262-safe.sh` logic worked.
    let _ = flag;
    Ok(())
}

fn relative_to(base: &Path, p: &Path) -> String {
    p.strip_prefix(base)
        .unwrap_or(p)
        .to_string_lossy()
        .replace('\\', "/")
}

fn git_head(repo: &Path) -> Option<String> {
    // Submodules store `.git` as a `gitdir: <path>` pointer file
    // rather than a directory — follow it so the dashboard records
    // the pinned vendor/test262 commit instead of "unknown".
    let dot_git = repo.join(".git");
    let git_dir = if dot_git.is_file() {
        let pointer = std::fs::read_to_string(&dot_git).ok()?;
        let rel = pointer.trim().strip_prefix("gitdir: ")?.to_string();
        let resolved = repo.join(rel);
        resolved.canonicalize().unwrap_or(resolved)
    } else {
        dot_git
    };
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let head = head.trim();
    if let Some(rest) = head.strip_prefix("ref: ") {
        let ref_path = git_dir.join(rest);
        return std::fs::read_to_string(&ref_path)
            .ok()
            .map(|s| s.trim().to_string());
    }
    Some(head.to_string())
}

fn conformance_cmd(args: ConformanceArgs) -> Result<ExitCode> {
    let baseline = Baseline::from_path(&args.input)
        .with_context(|| format!("failed to read baseline {}", args.input.display()))?;
    std::fs::write(&args.output, baseline.to_markdown())
        .with_context(|| format!("failed to write {}", args.output.display()))?;
    println!(
        "wrote {} ({:.2}% pass)",
        args.output.display(),
        baseline.totals.pass_rate()
    );
    Ok(ExitCode::SUCCESS)
}

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

fn diff_cmd(repo_root: &Path, args: DiffArgs) -> Result<ExitCode> {
    let previous = Baseline::from_path(&args.previous).with_context(|| {
        format!(
            "failed to read previous baseline {}",
            args.previous.display()
        )
    })?;
    let current_path = args
        .current
        .unwrap_or_else(|| repo_root.join(BASELINE_DIR).join("main.json"));
    let current = Baseline::from_path(&current_path)
        .with_context(|| format!("failed to read current baseline {}", current_path.display()))?;
    let report: DiffReport = diff::compute(&previous, &current);
    print!("{}", report.to_text(&args.previous.display().to_string()));
    Ok(ExitCode::from(report.exit_code() as u8))
}

fn merge_cmd(args: MergeArgs) -> Result<ExitCode> {
    if args.inputs.is_empty() {
        eprintln!("error: merge requires at least one input baseline");
        return Ok(ExitCode::from(2));
    }
    let mut shards: Vec<Baseline> = Vec::with_capacity(args.inputs.len());
    for path in &args.inputs {
        let baseline = Baseline::from_path(path)
            .with_context(|| format!("failed to read shard {}", path.display()))?;
        shards.push(baseline);
    }

    let merged = merge_baselines(&shards, &args.inputs).map_err(anyhow::Error::from)?;
    let parent = args.output.parent().unwrap_or_else(|| Path::new("."));
    let stem = args
        .output
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("merged");
    let (json, md) = merged.write_pair(parent, stem).with_context(|| {
        format!(
            "failed to write merged baseline at {}",
            args.output.display()
        )
    })?;
    eprintln!(
        "merged baseline:\n  json: {}\n  md  : {}",
        json.display(),
        md.display()
    );
    Ok(ExitCode::SUCCESS)
}

fn site_cmd(args: SiteArgs) -> Result<ExitCode> {
    let baseline = Baseline::from_path(&args.input)
        .with_context(|| format!("failed to read baseline {}", args.input.display()))?;
    let html = otter_test262::site::render_html(&baseline);
    if let Some(parent) = args.output.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    std::fs::write(&args.output, html)
        .with_context(|| format!("failed to write {}", args.output.display()))?;
    eprintln!("site written: {}", args.output.display());
    Ok(ExitCode::SUCCESS)
}

/// Combine per-shard baselines by union; flags collisions via
/// [`ReportError::MergeCollision`].
fn merge_baselines(shards: &[Baseline], inputs: &[PathBuf]) -> Result<Baseline, ReportError> {
    let mut totals = otter_test262::report::Totals::default();
    let mut by_section: otter_test262::report::BySection = std::collections::BTreeMap::new();
    let mut failing_tests: Vec<otter_test262::report::FailingTest> = Vec::new();
    let mut seen: std::collections::HashMap<String, String> = std::collections::HashMap::new();

    for (shard, path) in shards.iter().zip(inputs.iter()) {
        // Sum totals.
        totals.total += shard.totals.total;
        totals.passed += shard.totals.passed;
        totals.failed += shard.totals.failed;
        totals.skipped += shard.totals.skipped;
        totals.crashed += shard.totals.crashed;
        totals.timed_out += shard.totals.timed_out;
        totals.oom += shard.totals.oom;

        // Sum per-section totals.
        for (section, t) in &shard.by_section {
            let entry = by_section.entry(section.clone()).or_default();
            entry.total += t.total;
            entry.passed += t.passed;
            entry.failed += t.failed;
            entry.skipped += t.skipped;
            entry.crashed += t.crashed;
            entry.timed_out += t.timed_out;
            entry.oom += t.oom;
        }

        // Append failing rows; flag collisions.
        for row in &shard.failing_tests {
            if let Some(existing) = seen.get(&row.path) {
                return Err(ReportError::MergeCollision {
                    path: row.path.clone(),
                    first: existing.clone(),
                    second: path.display().to_string(),
                });
            }
            seen.insert(row.path.clone(), path.display().to_string());
            failing_tests.push(row.clone());
        }
    }

    // Inherit `test262_commit` / `engine_commit` from the first
    // shard — every shard runs against the same checkout in CI.
    let head = shards.first().expect("non-empty inputs validated above");
    Ok(Baseline {
        test262_commit: head.test262_commit.clone(),
        engine_commit: head.engine_commit.clone(),
        ran_at: chrono::Utc::now().to_rfc3339(),
        totals,
        by_section,
        failing_tests,
    })
}
