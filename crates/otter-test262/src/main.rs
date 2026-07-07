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

use std::collections::{HashSet, VecDeque};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{ExitCode, ExitStatus};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, atomic::AtomicUsize};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use indicatif::{ProgressBar, ProgressStyle};
use serde::{Deserialize, Serialize};

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
/// Default number of tests handled by one worker process before the
/// process exits and releases all process-local allocator / cage state.
const PROCESS_CHUNK_SIZE: usize = 40;
/// Default worker RSS soft ceiling. A worker that crosses this after
/// recording a test exits successfully; the parent requeues the rest of
/// the chunk on a fresh process.
const DEFAULT_WORKER_SOFT_RSS_BYTES: u64 = 1024 * 1024 * 1024;
/// Default ceiling for parent-side worker processes when `--jobs` is
/// omitted. Full core-count process fan-out can align several memory-heavy
/// tests and harm interactivity on developer machines.
const MAX_PROCESS_WORKERS: usize = 8;
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
    /// Internal process-isolation worker. Not a stable CLI surface.
    #[command(hide = true)]
    Worker(WorkerArgs),
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

    /// Number of worker processes. `0` (default) uses a conservative
    /// fraction of logical cores for a single-shard run and `1` for a
    /// multi-shard run.
    #[arg(long, default_value_t = 0)]
    jobs: usize,

    /// Tests assigned to one worker-process invocation.
    /// Smaller chunks make crashes easier to localise; larger chunks
    /// reduce process-spawn overhead.
    #[arg(long, default_value_t = PROCESS_CHUNK_SIZE)]
    process_chunk_size: usize,

    /// Worker resident-memory soft ceiling in bytes. `0` disables.
    /// Defaults to `OTTER_TEST262_WORKER_SOFT_RSS_BYTES` if set, else
    /// 1 GiB. When crossed after a test result is flushed, the worker
    /// exits cleanly and the parent requeues the remaining chunk on a
    /// fresh process.
    #[arg(long)]
    worker_soft_rss_bytes: Option<u64>,
}

#[derive(Parser, Debug)]
struct WorkerArgs {
    /// Absolute test path list shared by the parent.
    #[arg(long)]
    paths_file: PathBuf,

    /// JSON-lines result file written by this worker.
    #[arg(long)]
    out_file: PathBuf,

    /// First test index, inclusive.
    #[arg(long)]
    start: usize,

    /// Last test index, exclusive.
    #[arg(long)]
    end: usize,

    /// Per-test wall-clock timeout in milliseconds.
    #[arg(long)]
    timeout_ms: u64,

    /// Per-test heap cap in bytes.
    #[arg(long)]
    max_heap_bytes: u64,

    /// Optional `test262_config.toml` path.
    #[arg(long)]
    config: Option<PathBuf>,

    /// Worker resident-memory soft ceiling in bytes. `0` disables.
    #[arg(long, default_value_t = DEFAULT_WORKER_SOFT_RSS_BYTES)]
    worker_soft_rss_bytes: u64,
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
        Command::Worker(args) => worker(&repo_root, args),
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
    let worker_soft_rss_bytes = args.worker_soft_rss_bytes.unwrap_or_else(|| {
        std::env::var("OTTER_TEST262_WORKER_SOFT_RSS_BYTES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_WORKER_SOFT_RSS_BYTES)
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

    let jobs = resolve_jobs(args.jobs, shard.total as usize);

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

    execute_process_isolated(
        repo_root,
        &paths,
        &tests,
        args.config.as_deref(),
        timeout_ms,
        max_heap_bytes,
        args.output.as_deref(),
        args.cursor.as_deref(),
        args.resume,
        jobs,
        args.process_chunk_size.max(1),
        worker_soft_rss_bytes,
    )
}

fn resolve_jobs(explicit: usize, shard_total: usize) -> usize {
    if explicit > 0 {
        return explicit;
    }
    if shard_total > 1 {
        return 1;
    }
    let cores = thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(1);
    (cores / 2).clamp(1, MAX_PROCESS_WORKERS)
}

fn init_test262_cage(jobs: usize, max_heap_bytes: u64) -> Result<()> {
    if otter_runtime::otter_gc::cage_size() != 0 {
        return Ok(());
    }
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
    Ok(())
}

#[derive(Debug, Serialize, Deserialize)]
struct WorkerLine {
    idx: usize,
    result: TestResult,
}

struct ProgressState {
    done: AtomicUsize,
    completed: Mutex<Vec<bool>>,
    contiguous: AtomicUsize,
    cursor: Option<PathBuf>,
    resume_offset: usize,
}

impl ProgressState {
    fn new(len: usize, cursor: Option<&Path>, resume_offset: usize) -> Self {
        Self {
            done: AtomicUsize::new(0),
            completed: Mutex::new(vec![false; len]),
            contiguous: AtomicUsize::new(0),
            cursor: cursor.map(Path::to_path_buf),
            resume_offset,
        }
    }

    fn record(&self, idx: usize) {
        self.done.fetch_add(1, Ordering::Relaxed);
        let mut completed = self.completed.lock().expect("progress state poisoned");
        if let Some(done) = completed.get_mut(idx) {
            *done = true;
        }
        let mut next = self.contiguous.load(Ordering::Relaxed);
        while completed.get(next).copied().unwrap_or(false) {
            next += 1;
        }
        self.contiguous.store(next, Ordering::Relaxed);
        if let Some(path) = &self.cursor {
            write_cursor(path, self.resume_offset + next);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn execute_process_isolated(
    repo_root: &Path,
    paths: &CorpusPaths,
    tests: &[PathBuf],
    config_path: Option<&Path>,
    timeout_ms: u64,
    max_heap_bytes: u64,
    output: Option<&Path>,
    cursor: Option<&Path>,
    resume_offset: usize,
    jobs: usize,
    chunk_size: usize,
    worker_soft_rss_bytes: u64,
) -> Result<ExitCode> {
    let temp = tempfile::Builder::new()
        .prefix("otter-test262-")
        .tempdir()
        .context("failed to create process-isolation temp dir")?;
    let paths_file = temp.path().join("paths.txt");
    {
        let mut out = String::new();
        for path in tests {
            out.push_str(&path.display().to_string());
            out.push('\n');
        }
        std::fs::write(&paths_file, out).context("failed to write worker path list")?;
    }

    let config_path = config_path.map(|path| {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            repo_root.join(path)
        }
    });
    let exe = std::env::current_exe().context("failed to resolve current executable")?;
    let queue: Arc<Mutex<VecDeque<(usize, usize)>>> = Arc::new(Mutex::new(
        (0..tests.len())
            .step_by(chunk_size)
            .map(|lo| (lo, (lo + chunk_size).min(tests.len())))
            .collect(),
    ));
    let slots: Arc<Vec<Mutex<Option<TestResult>>>> =
        Arc::new((0..tests.len()).map(|_| Mutex::new(None)).collect());
    let tests = Arc::new(tests.to_vec());
    let paths = Arc::new(paths.clone());
    let progress = Arc::new(ProgressState::new(tests.len(), cursor, resume_offset));

    let pb = ProgressBar::new(tests.len() as u64);
    pb.set_style(
        ProgressStyle::default_bar()
            .template("{spinner:.green} [{bar:40.cyan/blue}] {pos}/{len} ({percent}%) | {msg}")
            .expect("progress bar template should compile")
            .progress_chars("#>-"),
    );
    pb.set_message("process");

    let interrupted = Arc::new(AtomicBool::new(false));
    let _ = ctrlc_install(Arc::clone(&interrupted));
    let start = Instant::now();
    eprintln!(
        "process isolation: {} tests, {} worker(s), chunk size {}",
        tests.len(),
        jobs,
        chunk_size
    );

    let handles = (0..jobs)
        .map(|worker_id| {
            let queue = Arc::clone(&queue);
            let slots = Arc::clone(&slots);
            let tests = Arc::clone(&tests);
            let paths = Arc::clone(&paths);
            let progress = Arc::clone(&progress);
            let interrupted = Arc::clone(&interrupted);
            let pb = pb.clone();
            let exe = exe.clone();
            let repo_root = repo_root.to_path_buf();
            let paths_file = paths_file.clone();
            let config_path = config_path.clone();
            let temp_path = temp.path().to_path_buf();
            thread::spawn(move || {
                process_parent_worker_loop(
                    worker_id,
                    &queue,
                    &slots,
                    &tests,
                    &paths,
                    &progress,
                    &interrupted,
                    &pb,
                    &exe,
                    &repo_root,
                    &paths_file,
                    config_path.as_deref(),
                    &temp_path,
                    timeout_ms,
                    max_heap_bytes,
                    worker_soft_rss_bytes,
                );
            })
        })
        .collect::<Vec<_>>();
    for handle in handles {
        let _ = handle.join();
    }
    pb.finish_and_clear();

    let mut results = Vec::with_capacity(tests.len());
    for (idx, path) in tests.iter().enumerate() {
        let result = slots[idx]
            .lock()
            .expect("worker result slot poisoned")
            .take()
            .unwrap_or_else(|| {
                synthetic_result(
                    &paths,
                    path,
                    Outcome::Crash {
                        panic: "not executed by process worker".to_string(),
                    },
                )
            });
        results.push(result);
    }
    results.sort_by(|a, b| a.path.cmp(&b.path));

    write_timings_if_requested(&results);

    let elapsed = start.elapsed();
    let baseline = build_baseline(&paths, &results);
    print_summary(&baseline, elapsed);
    if let Some(json_path) = output {
        write_baseline(json_path, &baseline)?;
    }
    if interrupted.load(Ordering::Relaxed) {
        write_partial_baseline(output, &paths, &results);
        return Ok(ExitCode::from(130));
    }
    if baseline.totals.crashed > 0 {
        return Ok(ExitCode::from(1));
    }
    Ok(ExitCode::SUCCESS)
}

#[allow(clippy::too_many_arguments)]
fn process_parent_worker_loop(
    worker_id: usize,
    queue: &Mutex<VecDeque<(usize, usize)>>,
    slots: &[Mutex<Option<TestResult>>],
    tests: &[PathBuf],
    paths: &CorpusPaths,
    progress: &ProgressState,
    interrupted: &AtomicBool,
    pb: &ProgressBar,
    exe: &Path,
    repo_root: &Path,
    paths_file: &Path,
    config_path: Option<&Path>,
    temp_path: &Path,
    timeout_ms: u64,
    max_heap_bytes: u64,
    worker_soft_rss_bytes: u64,
) {
    loop {
        if interrupted.load(Ordering::Relaxed) {
            return;
        }
        let Some((lo, hi)) = queue.lock().expect("worker queue poisoned").pop_front() else {
            return;
        };
        let out_file = temp_path.join(format!("worker-{worker_id}-{lo}-{hi}.jsonl"));
        let stdout_file = temp_path.join(format!("worker-{worker_id}-{lo}-{hi}.stdout"));
        let stderr_file = temp_path.join(format!("worker-{worker_id}-{lo}-{hi}.stderr"));
        let worker_status = run_worker_child(
            exe,
            repo_root,
            paths_file,
            &out_file,
            &stdout_file,
            &stderr_file,
            lo,
            hi,
            timeout_ms,
            max_heap_bytes,
            worker_soft_rss_bytes,
            config_path,
        );

        let recorded = read_worker_lines(&out_file, slots, pb, progress);
        let _ = std::fs::remove_file(&out_file);
        let _ = std::fs::remove_file(&stdout_file);
        let _ = std::fs::remove_file(&stderr_file);
        if let Some(first_missing) = (lo..hi).find(|idx| !recorded.contains(idx)) {
            if !worker_status.timed_out
                && matches!(&worker_status.status, Some(status) if status.success())
            {
                eprintln!(
                    "worker {worker_id}: retired after {} result(s); requeueing [{first_missing}, {hi})",
                    recorded.len()
                );
                queue
                    .lock()
                    .expect("worker queue poisoned")
                    .push_back((first_missing, hi));
                continue;
            }
            let outcome = if worker_status.timed_out {
                Outcome::Timeout { ms: timeout_ms }
            } else {
                Outcome::Crash {
                    panic: worker_status.failure_reason(),
                }
            };
            if store_worker_result(
                slots,
                first_missing,
                synthetic_result(paths, &tests[first_missing], outcome),
                pb,
                progress,
            ) {
                eprintln!(
                    "worker {worker_id}: isolated {} at index {}",
                    relative_to(&paths.test_dir, &tests[first_missing]),
                    first_missing
                );
            }
            if first_missing + 1 < hi {
                queue
                    .lock()
                    .expect("worker queue poisoned")
                    .push_back((first_missing + 1, hi));
            }
        }
    }
}

struct WorkerRunStatus {
    status: Option<ExitStatus>,
    timed_out: bool,
    stderr_tail: String,
    stdout_tail: String,
}

impl WorkerRunStatus {
    fn failure_reason(&self) -> String {
        let mut reason = self.status.map_or_else(
            || "worker spawn failed".to_string(),
            |status| format!("worker exited before reporting this test ({status})"),
        );
        if !self.stderr_tail.is_empty() {
            reason.push_str("; stderr: ");
            reason.push_str(&self.stderr_tail);
        }
        if !self.stdout_tail.is_empty() {
            reason.push_str("; stdout: ");
            reason.push_str(&self.stdout_tail);
        }
        reason
    }
}

#[allow(clippy::too_many_arguments)]
fn run_worker_child(
    exe: &Path,
    repo_root: &Path,
    paths_file: &Path,
    out_file: &Path,
    stdout_file: &Path,
    stderr_file: &Path,
    start: usize,
    end: usize,
    timeout_ms: u64,
    max_heap_bytes: u64,
    worker_soft_rss_bytes: u64,
    config_path: Option<&Path>,
) -> WorkerRunStatus {
    let mut cmd = std::process::Command::new(exe);
    let stdout = File::create(stdout_file).ok();
    let stderr = File::create(stderr_file).ok();
    cmd.current_dir(repo_root)
        .arg("--repo-root")
        .arg(repo_root)
        .arg("worker")
        .arg("--paths-file")
        .arg(paths_file)
        .arg("--out-file")
        .arg(out_file)
        .arg("--start")
        .arg(start.to_string())
        .arg("--end")
        .arg(end.to_string())
        .arg("--timeout-ms")
        .arg(timeout_ms.to_string())
        .arg("--max-heap-bytes")
        .arg(max_heap_bytes.to_string())
        .arg("--worker-soft-rss-bytes")
        .arg(worker_soft_rss_bytes.to_string());
    if let Some(stdout) = stdout {
        cmd.stdout(stdout);
    }
    if let Some(stderr) = stderr {
        cmd.stderr(stderr);
    }
    if let Some(config_path) = config_path {
        cmd.arg("--config").arg(config_path);
    }
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(err) => {
            eprintln!("worker spawn failed for [{start}, {end}): {err}");
            return WorkerRunStatus {
                status: None,
                timed_out: false,
                stderr_tail: String::new(),
                stdout_tail: String::new(),
            };
        }
    };

    let startup_stall = Duration::from_millis(timeout_ms.saturating_add(5_000).max(10_000));
    let progress_stall = Duration::from_millis(timeout_ms.saturating_add(2_000).max(5_000));
    let mut stall = startup_stall;
    let mut deadline = Instant::now() + stall;
    let mut last_len = 0;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return WorkerRunStatus {
                    status: Some(status),
                    timed_out: false,
                    stderr_tail: tail_file(stderr_file),
                    stdout_tail: tail_file(stdout_file),
                };
            }
            Ok(None) => {
                let len = std::fs::metadata(out_file).map(|m| m.len()).unwrap_or(0);
                if len > last_len {
                    last_len = len;
                    stall = progress_stall;
                    deadline = Instant::now() + stall;
                }
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return WorkerRunStatus {
                        status: None,
                        timed_out: true,
                        stderr_tail: tail_file(stderr_file),
                        stdout_tail: tail_file(stdout_file),
                    };
                }
                thread::sleep(Duration::from_millis(50));
            }
            Err(err) => {
                eprintln!("worker wait failed for [{start}, {end}): {err}");
                return WorkerRunStatus {
                    status: None,
                    timed_out: false,
                    stderr_tail: tail_file(stderr_file),
                    stdout_tail: tail_file(stdout_file),
                };
            }
        }
    }
}

fn read_worker_lines(
    out_file: &Path,
    slots: &[Mutex<Option<TestResult>>],
    pb: &ProgressBar,
    progress: &ProgressState,
) -> HashSet<usize> {
    let mut recorded = HashSet::new();
    let Ok(text) = std::fs::read_to_string(out_file) else {
        return recorded;
    };
    for line in text.lines() {
        let Ok(row) = serde_json::from_str::<WorkerLine>(line) else {
            continue;
        };
        recorded.insert(row.idx);
        store_worker_result(slots, row.idx, row.result, pb, progress);
    }
    recorded
}

fn store_worker_result(
    slots: &[Mutex<Option<TestResult>>],
    idx: usize,
    result: TestResult,
    pb: &ProgressBar,
    progress: &ProgressState,
) -> bool {
    let Some(slot) = slots.get(idx) else {
        return false;
    };
    let mut slot = slot.lock().expect("worker result slot poisoned");
    if slot.is_some() {
        return false;
    }
    record_progress(pb, &result.outcome);
    *slot = Some(result);
    pb.inc(1);
    progress.record(idx);
    true
}

fn synthetic_result(paths: &CorpusPaths, path: &Path, outcome: Outcome) -> TestResult {
    TestResult {
        path: relative_to(&paths.test_dir, path),
        esid: None,
        features: Vec::new(),
        outcome,
        wall_ms: 0,
    }
}

fn worker(repo_root: &Path, args: WorkerArgs) -> Result<ExitCode> {
    let paths = ensure_corpus_present(repo_root).context("failed to locate test262 corpus")?;
    let config = Test262Config::load_or_default(args.config.as_deref());
    init_test262_cage(1, args.max_heap_bytes)?;
    let mut harness = HarnessCache::new(&paths.harness_dir);
    if let Err(err) = harness.prewarm() {
        eprintln!("error: failed to prewarm harness: {err}");
        return Ok(ExitCode::from(2));
    }
    let exec = ExecConfig {
        timeout: Duration::from_millis(args.timeout_ms),
        max_heap_bytes: args.max_heap_bytes,
        config,
    };
    let test_paths = std::fs::read_to_string(&args.paths_file)
        .with_context(|| format!("failed to read {}", args.paths_file.display()))?;
    let test_paths = test_paths.lines().map(PathBuf::from).collect::<Vec<_>>();
    let mut out = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&args.out_file)
        .with_context(|| format!("failed to open {}", args.out_file.display()))?;
    let end = args.end.min(test_paths.len());
    for (idx, test_path) in test_paths.iter().enumerate().take(end).skip(args.start) {
        if std::env::var_os("OTTER_TEST262_TRACE_CURRENT").is_some() {
            let rel = test_path
                .strip_prefix(&paths.test_dir)
                .unwrap_or(test_path)
                .display();
            eprintln!("test262-current {idx} {rel}");
        }
        let result = run_one(test_path, &paths, &mut harness, &exec);
        let row = WorkerLine { idx, result };
        let line = serde_json::to_string(&row).context("failed to serialize worker result")?;
        use std::io::Write as _;
        writeln!(out, "{line}").context("failed to write worker result")?;
        out.flush().context("failed to flush worker result")?;
        if should_retire_worker(args.worker_soft_rss_bytes) {
            return Ok(ExitCode::SUCCESS);
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn should_retire_worker(soft_rss_bytes: u64) -> bool {
    if soft_rss_bytes == 0 {
        return false;
    }
    let Some(rss) = current_rss_bytes() else {
        return false;
    };
    if rss <= soft_rss_bytes {
        return false;
    }
    eprintln!(
        "worker soft-retire: rss={} soft_cap={}",
        format_bytes(rss),
        format_bytes(soft_rss_bytes)
    );
    true
}

fn current_rss_bytes() -> Option<u64> {
    let pid = sysinfo::get_current_pid().ok()?;
    let mut system = sysinfo::System::new();
    system.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);
    system.process(pid).map(sysinfo::Process::memory)
}

fn format_bytes(bytes: u64) -> String {
    const MIB: u64 = 1024 * 1024;
    if bytes >= MIB {
        format!("{}MiB", bytes / MIB)
    } else {
        format!("{bytes}B")
    }
}

fn write_timings_if_requested(results: &[TestResult]) {
    let Some(timings_path) = std::env::var_os("OTTER_TEST262_TIMINGS") else {
        return;
    };
    let mut rows: Vec<(&str, u64)> = results
        .iter()
        .map(|result| (result.path.as_str(), result.wall_ms))
        .collect();
    rows.sort_by_key(|row| std::cmp::Reverse(row.1));
    let mut out = String::new();
    for (rel, ms) in &rows {
        out.push_str(&format!("{ms}\t{rel}\n"));
    }
    if let Err(err) = std::fs::write(&timings_path, out) {
        eprintln!("warning: failed to write timings: {err}");
    }
}

fn write_partial_baseline(output: Option<&Path>, paths: &CorpusPaths, results: &[TestResult]) {
    let stem = format!("partial-{}", chrono::Utc::now().format("%Y%m%dT%H%M%SZ"));
    let dir = output
        .and_then(Path::parent)
        .unwrap_or_else(|| Path::new("."));
    let baseline = build_baseline(paths, results);
    if let Ok((json, _)) = baseline.write_pair(dir, &stem) {
        eprintln!("partial baseline at {}", json.display());
    }
}

fn tail_file(path: &Path) -> String {
    let Ok(text) = std::fs::read_to_string(path) else {
        return String::new();
    };
    let text = text.trim();
    if text.is_empty() {
        return String::new();
    }
    let mut lines = text.lines().rev().take(5).collect::<Vec<_>>();
    lines.reverse();
    let joined = lines.join(" | ");
    if joined.chars().count() > 800 {
        format!("{}...", joined.chars().take(800).collect::<String>())
    } else {
        joined
    }
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
    ctrlc::set_handler(move || {
        flag.store(true, Ordering::SeqCst);
    })
    .map_err(std::io::Error::other)
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
