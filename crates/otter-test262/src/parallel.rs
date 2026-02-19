//! Parallel Test262 runner.
//!
//! Distributes test execution across N worker threads, each owning its own
//! Otter engine instance.  Communication uses bounded crossbeam channels for
//! backpressure.  Each worker creates a `new_multi_thread(worker_threads=1)`
//! tokio runtime so that the timeout watchdog (spawned via `tokio::spawn`)
//! can preempt the synchronous JS eval from a separate OS thread.

use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use colored::Colorize;
use crossbeam_channel::bounded;
use indicatif::ProgressBar;

use crate::report::RunSummary;
use crate::runner::{Test262Runner, TestOutcome, TestResult};

/// Shared configuration for all parallel workers.
///
/// Wrapped in `Arc` so workers can reference it without cloning every field.
pub struct ParallelConfig {
    /// Path to the test262 directory (for building harness cache and running tests).
    pub test_dir: PathBuf,
    /// Features whose tests should be skipped.
    pub skip_features: Vec<String>,
    /// Per-test timeout (cooperative via interrupt flag).
    pub timeout: Option<Duration>,
    /// Test path patterns (substring) to skip silently.
    pub ignored_tests: Vec<String>,
    /// Test path patterns known to panic — skipped with a warning.
    pub known_panics: Vec<String>,
    /// Maximum failure details to keep in the `RunSummary`.
    pub max_failures: usize,
    /// Whether to accumulate all results in `RunSummary::all_results`.
    pub save_results: bool,
    /// Verbosity level (mirrors the CLI `-v` count).
    pub verbose: u8,
    /// Suppress non-JSON output when true.
    pub json_mode: bool,
    /// Optional path for the JSONL result log.
    pub log_path: Option<PathBuf>,
    /// Append to the log file instead of truncating at run start.
    pub log_append: bool,
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Run `tests` in parallel across `num_jobs` worker threads.
///
/// Blocks until all tests complete.  Should be called from a blocking context
/// (e.g. via `tokio::task::spawn_blocking` or `block_in_place`).
pub fn run_parallel(
    tests: Vec<PathBuf>,
    config: Arc<ParallelConfig>,
    num_jobs: usize,
    pb: Option<ProgressBar>,
) -> RunSummary {
    // Bounded channels provide backpressure so the job queue never grows huge.
    let (job_tx, job_rx) = bounded::<PathBuf>(num_jobs * 4);
    let (result_tx, result_rx) = bounded::<Vec<TestResult>>(num_jobs * 8);

    // -------------------------------------------------------------------------
    // Spawn N worker threads — each with a 64 MB stack and its own tokio rt.
    // -------------------------------------------------------------------------
    let mut handles = Vec::with_capacity(num_jobs);
    for i in 0..num_jobs {
        let job_rx = job_rx.clone();
        let result_tx = result_tx.clone();
        let cfg = Arc::clone(&config);

        let handle = std::thread::Builder::new()
            .name(format!("test262-worker-{i}"))
            .stack_size(64 * 1024 * 1024)
            .spawn(move || worker_main(i, job_rx, result_tx, cfg))
            .unwrap_or_else(|e| panic!("failed to spawn worker thread {i}: {e}"));

        handles.push(handle);
    }

    // Drop the main thread's copy of result_tx — the channel closes when all
    // workers finish and drop their own copies.
    drop(result_tx);

    // -------------------------------------------------------------------------
    // Job sender — a lightweight thread that just feeds paths into the channel.
    // -------------------------------------------------------------------------
    let send_handle = std::thread::spawn(move || {
        for path in tests {
            if job_tx.send(path).is_err() {
                break; // result collector disconnected
            }
        }
        // Drop job_tx → workers' job_rx closes → workers exit their loops.
    });

    // -------------------------------------------------------------------------
    // Open JSONL log file if requested (truncate or append per flag).
    // -------------------------------------------------------------------------
    let mut log_writer: Option<BufWriter<std::fs::File>> = config.log_path.as_ref().and_then(|p| {
        if let Some(parent) = p.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(!config.log_append)
            .append(config.log_append)
            .open(p)
            .ok()
            .map(BufWriter::new)
    });

    // -------------------------------------------------------------------------
    // Collect results on the calling thread.
    // -------------------------------------------------------------------------
    let mut summary = RunSummary::new(config.max_failures);

    for results in &result_rx {
        for result in &results {
            // JSONL log
            if let Some(ref mut writer) = log_writer {
                if let Ok(line) = serde_json::to_string(result) {
                    let _ = writeln!(writer, "{}", line);
                }
            }

            // Verbose per-result output
            if !config.json_mode {
                match config.verbose {
                    0 => {}
                    1 => {
                        let ch = match result.outcome {
                            TestOutcome::Pass => ".".green().to_string(),
                            TestOutcome::Fail => "F".red().to_string(),
                            TestOutcome::Skip => "S".yellow().to_string(),
                            TestOutcome::Timeout => "T".magenta().to_string(),
                            TestOutcome::Crash => "!".red().bold().to_string(),
                        };
                        eprint!("{}", ch);
                        if summary.total % 80 == 79 {
                            eprintln!();
                        }
                    }
                    _ => {
                        let status = match result.outcome {
                            TestOutcome::Pass => "PASS".green().to_string(),
                            TestOutcome::Fail => "FAIL".red().to_string(),
                            TestOutcome::Skip => "SKIP".yellow().to_string(),
                            TestOutcome::Timeout => "TIME".magenta().to_string(),
                            TestOutcome::Crash => "CRASH".red().bold().to_string(),
                        };
                        eprintln!(
                            "[{}] {} ({}) {}ms",
                            status, result.path, result.mode, result.duration_ms,
                        );
                        if let Some(ref err) = result.error {
                            eprintln!("  Error: {}", err);
                        }
                    }
                }
            }

            summary.record(result, config.save_results);
        }

        // Update progress bar once per test-file batch.
        if let Some(ref pb) = pb {
            pb.inc(1);
            pb.set_message(format!(
                "Pass: {} Fail: {} Skip: {} [{}j]",
                summary.passed, summary.failed, summary.skipped, num_jobs
            ));
        }
    }

    // Flush log
    if let Some(ref mut writer) = log_writer {
        let _ = writer.flush();
    }

    if config.verbose == 1 && !config.json_mode {
        eprintln!();
    }

    if let Some(pb) = pb {
        pb.finish_and_clear();
    }

    // Wait for all threads to finish cleanly.
    let _ = send_handle.join();
    for h in handles {
        // Worker panics are already caught by catch_unwind inside run_with_timeout.
        // If a worker thread itself panics (e.g. engine rebuild failed), log it.
        if let Err(e) = h.join() {
            let msg = if let Some(s) = e.downcast_ref::<&str>() {
                (*s).to_string()
            } else if let Some(s) = e.downcast_ref::<String>() {
                s.clone()
            } else {
                "unknown".to_string()
            };
            eprintln!("worker thread panicked: {}", msg);
        }
    }

    summary
}

// ---------------------------------------------------------------------------
// Per-worker logic
// ---------------------------------------------------------------------------

fn worker_main(
    _id: usize,
    job_rx: crossbeam_channel::Receiver<PathBuf>,
    result_tx: crossbeam_channel::Sender<Vec<TestResult>>,
    cfg: Arc<ParallelConfig>,
) {
    // Keep each worker fully thread-confined. Timeout watchdog is handled by
    // a dedicated std thread inside Test262Runner, so Tokio can stay single-threaded.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("worker tokio runtime");

    let mut runner = Test262Runner::new(
        &cfg.test_dir,
        false, // dump disabled in parallel (avoids file contention)
        None,
        100,
        false, // tracing disabled in parallel
        None,
        None,
        false,
        false,
    );
    if !cfg.skip_features.is_empty() {
        runner = runner.with_skip_features(cfg.skip_features.clone());
    }

    for path in &job_rx {
        let path_str = path.to_string_lossy();
        if cfg
            .ignored_tests
            .iter()
            .any(|p| path_str.contains(p.as_str()))
            || cfg
                .known_panics
                .iter()
                .any(|p| path_str.contains(p.as_str()))
        {
            continue;
        }

        let results = rt.block_on(runner.run_test_all_modes(&path, cfg.timeout));

        if result_tx.send(results).is_err() {
            break; // main thread disconnected
        }
    }
}
