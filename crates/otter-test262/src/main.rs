use clap::{ArgAction, Parser, Subcommand};
use colored::*;
use indicatif::{ProgressBar, ProgressStyle};
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, RefreshKind, System};
use tracing_subscriber::filter::EnvFilter;

use otter_test262::{
    PersistedReport, RunSummary, Test262Runner, TestOutcome, TestReport, compare,
    config::Test262Config, editions, parallel::ParallelConfig,
};

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

#[derive(Parser, Debug)]
#[command(name = "test262")]
#[command(about = "Run Test262 conformance tests against Otter VM")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    // ---- Flags shared with `run` so bare `test262 --filter foo` still works ----
    /// Path to test262 directory
    #[arg(short, long, default_value = "tests/test262", global = true)]
    test_dir: PathBuf,

    /// Filter tests by path pattern
    #[arg(short, long, global = true)]
    filter: Option<String>,

    /// Run only tests in this subdirectory (e.g., "language/expressions")
    #[arg(short = 'd', long, global = true)]
    subdir: Option<String>,

    /// Output results as JSON
    #[arg(long, global = true)]
    json: bool,

    /// Verbosity level: -v colored, -vv names, -vvv output
    #[arg(short, long, action = ArgAction::Count, global = true)]
    verbose: u8,

    /// Number of tests to skip from the beginning
    #[arg(short = 's', long, global = true)]
    skip: Option<usize>,

    /// Maximum number of tests to run
    #[arg(short = 'n', long, global = true)]
    max_tests: Option<usize>,

    /// Only list tests without running them
    #[arg(long, global = true)]
    list_only: bool,

    /// Show memory usage statistics
    #[arg(long, global = true)]
    memory_stats: bool,

    /// Timeout in seconds for each test
    #[arg(long, global = true)]
    timeout: Option<u64>,

    /// Path to config file (default: test262_config.toml)
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    /// Save results to a JSON file
    #[arg(long, global = true)]
    save: Option<Option<PathBuf>>,

    /// Specific test files to run
    #[arg(value_name = "FILES", global = true)]
    files: Vec<String>,

    /// Dump debug snapshot on timeout
    #[arg(long, global = true, default_value = "false")]
    dump_on_timeout: bool,

    /// File path for timeout dumps (default: stderr)
    #[arg(long, global = true)]
    dump_file: Option<PathBuf>,

    /// Number of instructions to keep in ring buffer (default: 100)
    #[arg(long, global = true, default_value = "100")]
    dump_buffer_size: usize,

    /// Number of parallel workers (default: num_cpus; 1 = sequential)
    #[arg(short = 'j', long = "jobs", global = true)]
    jobs: Option<usize>,

    /// Path for JSONL result log (one JSON object per test result per line)
    #[arg(long, global = true)]
    log: Option<PathBuf>,

    /// Append to the JSONL log instead of truncating at run start
    #[arg(long, global = true)]
    log_append: bool,

    /// Enable full execution trace for tests
    #[arg(long, global = true)]
    trace: bool,

    /// File path for trace output (default: test262-trace.txt)
    #[arg(long, global = true)]
    trace_file: Option<PathBuf>,

    /// Filter trace by module/function pattern (regex)
    #[arg(long, global = true)]
    trace_filter: Option<String>,

    /// Trace only failing tests
    #[arg(long, global = true)]
    trace_failures_only: bool,

    /// Trace only timeout tests
    #[arg(long, global = true)]
    trace_timeouts_only: bool,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Run test262 tests (default)
    Run,
    /// Compare two saved result files
    Compare {
        /// Base (older) result file
        #[arg(long)]
        base: PathBuf,
        /// New (current) result file
        #[arg(long, alias = "new")]
        current: PathBuf,
    },
}

// ---------------------------------------------------------------------------
// Memory tracker
// ---------------------------------------------------------------------------

struct MemoryTracker {
    system: System,
    pid: Pid,
    peak_memory_kib: u64,
    initial_memory_kib: u64,
}

impl MemoryTracker {
    fn new() -> Self {
        let pid = Pid::from_u32(std::process::id());
        let mut system = System::new_with_specifics(
            RefreshKind::nothing().with_processes(ProcessRefreshKind::everything()),
        );
        system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[pid]),
            true,
            ProcessRefreshKind::everything(),
        );

        let initial = system.process(pid).map(|p| p.memory()).unwrap_or(0);

        Self {
            system,
            pid,
            peak_memory_kib: initial,
            initial_memory_kib: initial,
        }
    }

    fn update(&mut self) {
        self.system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[self.pid]),
            true,
            ProcessRefreshKind::everything(),
        );
        if let Some(process) = self.system.process(self.pid) {
            let current = process.memory();
            if current > self.peak_memory_kib {
                self.peak_memory_kib = current;
            }
        }
    }

    fn current_memory_mb(&mut self) -> f64 {
        self.system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[self.pid]),
            true,
            ProcessRefreshKind::everything(),
        );
        self.system
            .process(self.pid)
            .map(|p| p.memory() as f64 / 1024.0)
            .unwrap_or(0.0)
    }

    fn peak_memory_mb(&self) -> f64 {
        self.peak_memory_kib as f64 / 1024.0
    }

    fn initial_memory_mb(&self) -> f64 {
        self.initial_memory_kib as f64 / 1024.0
    }

    fn memory_increase_mb(&mut self) -> f64 {
        self.current_memory_mb() - self.initial_memory_mb()
    }
}

// RunSummary is defined in otter_test262::report and re-exported from the library.

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn main() {
    // Force UTC timezone for consistent test results (avoids failures due to local/historical offsets)
    // SAFETY: This is safe because it's the very first thing called in main, before any threads are spawned.
    unsafe {
        std::env::set_var("TZ", "UTC");
    }

    // Suppress default panic output — panics are caught by catch_unwind
    // in the runner and reported as Crash/Fail outcomes.
    // std::panic::set_hook(Box::new(|_| {}));

    const STACK_SIZE: usize = 64 * 1024 * 1024; // 64 MB
    let builder = std::thread::Builder::new()
        .name("test262-main".into())
        .stack_size(STACK_SIZE);
    let handler = builder
        .spawn(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap()
                .block_on(async_main())
        })
        .expect("failed to spawn main thread");
    match handler.join() {
        Ok(()) => {}
        Err(e) => {
            let msg = if let Some(s) = e.downcast_ref::<String>() {
                s.clone()
            } else if let Some(s) = e.downcast_ref::<&str>() {
                s.to_string()
            } else {
                "unknown panic".to_string()
            };
            eprintln!("Test runner thread panicked: {}", msg);
            std::process::exit(2);
        }
    }
}

async fn async_main() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("warn".parse().unwrap()))
        .init();

    let cli = Cli::parse();

    match cli.command {
        Some(Commands::Compare { base, current }) => {
            run_compare(&base, &current);
        }
        Some(Commands::Run) | None => {
            run_tests(cli).await;
        }
    }
}

// ---------------------------------------------------------------------------
// Compare command
// ---------------------------------------------------------------------------

fn run_compare(base: &std::path::Path, current: &std::path::Path) {
    match compare::compare_files(base, current) {
        Ok(comparison) => comparison.print(),
        Err(e) => {
            eprintln!("{}: {}", "Error".red().bold(), e);
            std::process::exit(1);
        }
    }
}

// ---------------------------------------------------------------------------
// Run command
// ---------------------------------------------------------------------------

async fn run_tests(cli: Cli) {
    let config = Test262Config::load_or_default(cli.config.as_deref());
    let save_results = cli.save.is_some();
    let save_path = cli.save.as_ref().and_then(|opt| opt.clone()).or_else(|| {
        if save_results {
            Some(PathBuf::from("test262_results/latest.json"))
        } else {
            None
        }
    });

    if !cli.json {
        eprintln!("{}", "Otter Test262 Runner".bold().cyan());
        eprintln!("Test directory: {}", cli.test_dir.display());
    }

    // Initialize memory tracking if requested
    let mut memory_tracker = if cli.memory_stats {
        let tracker = MemoryTracker::new();
        eprintln!("Initial memory: {:.2} MB", tracker.initial_memory_mb());
        Some(tracker)
    } else {
        None
    };

    // Create runner with config
    let mut runner = Test262Runner::new(
        &cli.test_dir,
        cli.dump_on_timeout,
        cli.dump_file.clone(),
        cli.dump_buffer_size,
        cli.trace,
        cli.trace_file.clone(),
        cli.trace_filter.clone(),
        cli.trace_failures_only,
        cli.trace_timeouts_only,
    );

    // Apply skip features from config
    if !config.skip_features.is_empty() {
        runner = runner.with_skip_features(config.skip_features.clone());
    }

    if let Some(ref filter) = cli.filter {
        runner = runner.with_filter(filter.clone());
        if !cli.json {
            eprintln!("Filter: {}", filter);
        }
    }

    // List-only mode
    if cli.list_only {
        let tests = if let Some(ref subdir) = cli.subdir {
            runner.list_tests_dir(subdir)
        } else {
            runner.list_tests()
        };

        for test in &tests {
            println!("{}", test.display());
        }
        println!("\nTotal: {} tests", tests.len());
        return;
    }

    // Collect tests
    let mut tests = if !cli.files.is_empty() {
        if !cli.json {
            eprintln!("Running {} specific test files", cli.files.len());
        }
        cli.files.iter().map(PathBuf::from).collect()
    } else if let Some(ref subdir) = cli.subdir {
        if !cli.json {
            eprintln!("Subdirectory: {}", subdir);
        }
        runner.list_tests_dir(subdir)
    } else {
        runner.list_tests()
    };

    if let Some(skip) = cli.skip {
        if skip < tests.len() {
            tests = tests.split_off(skip);
        } else {
            tests.clear();
        }
    }

    if let Some(max) = cli.max_tests {
        tests.truncate(max);
    }

    let test_count = tests.len();
    if !cli.json {
        if let Some(skip) = cli.skip {
            eprintln!("Skipping first {} tests", skip);
        }
        eprintln!("Found {} test files", test_count);
    }

    // Set up progress bar (hidden in JSON mode or high verbosity)
    let show_progress = !cli.json && cli.verbose < 2;
    let pb = if show_progress {
        let pb = ProgressBar::new(test_count as u64);
        pb.set_style(
            ProgressStyle::default_bar()
                .template("{spinner:.green} [{bar:40.cyan/blue}] {pos}/{len} ({percent}%) | {msg}")
                .unwrap()
                .progress_chars("#>-"),
        );
        pb.set_message("starting...");
        Some(pb)
    } else {
        None
    };

    let run_start = Instant::now();
    let max_failures = if cli.json { 50000 } else { 200 };

    // Always enforce a timeout to prevent hangs. Default: 10s per test.
    let timeout = Some(std::time::Duration::from_secs(
        cli.timeout.or(config.timeout_secs).unwrap_or(10),
    ));

    // -------------------------------------------------------------------------
    // Parallel path — dispatch to worker threads when -j > 1
    // -------------------------------------------------------------------------
    let num_jobs = cli.jobs.unwrap_or_else(num_cpus::get).max(1);
    if num_jobs > 1 {
        if !cli.json {
            eprintln!("Running with {} parallel workers", num_jobs);
        }
        let par_config = Arc::new(ParallelConfig {
            test_dir: cli.test_dir.clone(),
            skip_features: config.skip_features.clone(),
            timeout,
            ignored_tests: config.ignored_tests.clone(),
            known_panics: config.known_panics.clone(),
            max_failures,
            save_results,
            verbose: cli.verbose,
            json_mode: cli.json,
            log_path: cli.log.clone(),
            log_append: cli.log_append,
        });
        let pb_for_par = pb.clone();
        let summary = tokio::task::spawn_blocking(move || {
            otter_test262::parallel::run_parallel(tests, par_config, num_jobs, pb_for_par)
        })
        .await
        .expect("parallel runner panicked");

        let run_duration = run_start.elapsed();
        let by_edition = summary.by_edition.clone();
        let all_results = if save_results {
            summary.all_results.clone()
        } else {
            Vec::new()
        };
        let report = summary.into_report();

        // Report
        if cli.json {
            match report.to_json() {
                Ok(json) => println!("{}", json),
                Err(e) => eprintln!("Failed to generate JSON: {}", e),
            }
        } else {
            report.print_summary();
            if !by_edition.is_empty() {
                editions::print_edition_table(&by_edition);
            }
            if cli.verbose >= 1 && !report.failures.is_empty() {
                println!();
                println!("{}", "=== All Failures ===".bold().red());
                for failure in &report.failures {
                    println!(
                        "{} ({}) - {}",
                        failure.path.yellow(),
                        failure.mode,
                        failure.error
                    );
                }
            }
        }

        // Save results if requested
        if let Some(save_path) = save_path {
            let persisted = PersistedReport {
                timestamp: chrono::Utc::now().to_rfc3339(),
                otter_version: env!("CARGO_PKG_VERSION").to_string(),
                test262_commit: None,
                duration_secs: run_duration.as_secs_f64(),
                summary: report.clone(),
                results: all_results,
            };
            match persisted.save(&save_path) {
                Ok(()) => {
                    if !cli.json {
                        eprintln!("Results saved to {}", save_path.display());
                    }
                }
                Err(e) => eprintln!("Failed to save results: {}", e),
            }
            if let Some(parent) = save_path.parent() {
                let ts = chrono::Utc::now().format("%Y%m%d_%H%M%S");
                let timestamped = parent.join(format!("run_{}.json", ts));
                let _ = persisted.save(&timestamped);
            }
        }

        if report.failed > 0 {
            std::process::exit(1);
        }
        return;
    }

    // -------------------------------------------------------------------------
    // Sequential path (single worker, existing behaviour)
    // -------------------------------------------------------------------------

    // Open JSONL log if requested
    let mut log_writer: Option<BufWriter<std::fs::File>> = cli.log.as_ref().and_then(|p| {
        if let Some(parent) = p.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(!cli.log_append)
            .append(cli.log_append)
            .open(p)
            .ok()
            .map(BufWriter::new)
    });

    let mut summary = RunSummary::new(max_failures);

    for path in tests {
        // Check ignored/known-panic via config
        let path_str = path.to_string_lossy();
        if config.is_ignored(&path_str) || config.is_known_panic(&path_str) {
            continue;
        }

        let results = runner.run_test_all_modes(&path, timeout).await;

        for result in &results {
            // Verbosity output
            if !cli.json {
                match cli.verbose {
                    0 => {} // progress bar handles it
                    1 => {
                        // Colored single-character indicators
                        let ch = match result.outcome {
                            TestOutcome::Pass => ".".green(),
                            TestOutcome::Fail => "F".red(),
                            TestOutcome::Skip => "S".yellow(),
                            TestOutcome::Timeout => "T".magenta(),
                            TestOutcome::Crash => "!".red().bold(),
                        };
                        eprint!("{}", ch);
                        if summary.total % 80 == 79 {
                            eprintln!();
                        }
                    }
                    2 => {
                        // Print test name and result
                        let status = match result.outcome {
                            TestOutcome::Pass => "PASS".green(),
                            TestOutcome::Fail => "FAIL".red(),
                            TestOutcome::Skip => "SKIP".yellow(),
                            TestOutcome::Timeout => "TIME".magenta(),
                            TestOutcome::Crash => "CRASH".red().bold(),
                        };
                        eprintln!(
                            "[{}] {} ({}) {:?}",
                            status,
                            result.path,
                            result.mode,
                            result.duration()
                        );
                    }
                    _ => {
                        // vvv: Print test name, result, and captured output
                        let status = match result.outcome {
                            TestOutcome::Pass => "PASS".green(),
                            TestOutcome::Fail => "FAIL".red(),
                            TestOutcome::Skip => "SKIP".yellow(),
                            TestOutcome::Timeout => "TIME".magenta(),
                            TestOutcome::Crash => "CRASH".red().bold(),
                        };
                        eprintln!(
                            "[{}] {} ({}) {:?}",
                            status,
                            result.path,
                            result.mode,
                            result.duration()
                        );
                        if let Some(ref err) = result.error {
                            eprintln!("  Error: {}", err);
                        }
                        let output = runner.harness_state().print_output();
                        if !output.is_empty() {
                            eprintln!("  Output:");
                            for line in &output {
                                eprintln!("    {}", line);
                            }
                        }
                    }
                }
            }

            // JSONL log
            if let Some(ref mut writer) = log_writer {
                if let Ok(line) = serde_json::to_string(result) {
                    let _ = writeln!(writer, "{}", line);
                }
            }

            summary.record(result, save_results);
        }

        // Update progress bar
        if let Some(ref pb) = pb {
            pb.inc(1);
            pb.set_message(format!(
                "Pass: {} Fail: {} Skip: {}",
                summary.passed, summary.failed, summary.skipped
            ));
        }

        // Update memory tracker
        if let Some(ref mut tracker) = memory_tracker
            && summary.total.is_multiple_of(100)
        {
            tracker.update();
        }

        // Incremental save every 500 tests to survive crashes
        if save_results && summary.total.is_multiple_of(500) {
            if let Some(ref save_path) = save_path {
                let interim_report = TestReport {
                    total: summary.total,
                    passed: summary.passed,
                    failed: summary.failed,
                    skipped: summary.skipped,
                    timeout: summary.timeout,
                    crashed: summary.crashed,
                    pass_rate: {
                        let run =
                            summary.passed + summary.failed + summary.timeout + summary.crashed;
                        if run > 0 {
                            (summary.passed as f64 / run as f64) * 100.0
                        } else {
                            0.0
                        }
                    },
                    by_feature: summary.by_feature.clone(),
                    failures: summary.failures.clone(),
                };
                let persisted = PersistedReport {
                    timestamp: chrono::Utc::now().to_rfc3339(),
                    otter_version: env!("CARGO_PKG_VERSION").to_string(),
                    test262_commit: None,
                    duration_secs: run_start.elapsed().as_secs_f64(),
                    summary: interim_report,
                    results: summary.all_results.clone(),
                };
                let _ = persisted.save(save_path);
            }
        }
    }

    // Flush JSONL log
    if let Some(ref mut writer) = log_writer {
        let _ = writer.flush();
    }

    // Finish progress
    if let Some(ref pb) = pb {
        pb.finish_and_clear();
    }
    if cli.verbose == 1 && !cli.json {
        eprintln!();
    }

    let run_duration = run_start.elapsed();

    if let Some(ref mut tracker) = memory_tracker {
        tracker.update();
    }

    let by_edition = summary.by_edition.clone();
    let all_results = if save_results {
        summary.all_results.clone()
    } else {
        Vec::new()
    };
    let report = summary.into_report();

    if cli.json {
        match report.to_json() {
            Ok(json) => println!("{}", json),
            Err(e) => eprintln!("Failed to generate JSON: {}", e),
        }
    } else {
        report.print_summary();

        // Print edition table
        if !by_edition.is_empty() {
            editions::print_edition_table(&by_edition);
        }

        // Print detailed failures if verbose
        if cli.verbose >= 1 && !report.failures.is_empty() {
            println!();
            println!("{}", "=== All Failures ===".bold().red());
            for failure in &report.failures {
                println!(
                    "{} ({}) - {}",
                    failure.path.yellow(),
                    failure.mode,
                    failure.error
                );
            }
        }
    }

    // Memory statistics
    if let Some(ref mut tracker) = memory_tracker {
        println!();
        println!("{}", "=== Memory Profile ===".bold().cyan());
        println!("Initial:  {:.2} MB", tracker.initial_memory_mb());
        println!("Peak:     {:.2} MB", tracker.peak_memory_mb());
        println!("Current:  {:.2} MB", tracker.current_memory_mb());
        println!("Increase: {:.2} MB", tracker.memory_increase_mb());
    }

    // Save results if requested
    if let Some(save_path) = save_path {
        let persisted = PersistedReport {
            timestamp: chrono::Utc::now().to_rfc3339(),
            otter_version: env!("CARGO_PKG_VERSION").to_string(),
            test262_commit: None,
            duration_secs: run_duration.as_secs_f64(),
            summary: report.clone(),
            results: all_results,
        };

        match persisted.save(&save_path) {
            Ok(()) => {
                if !cli.json {
                    eprintln!("Results saved to {}", save_path.display());
                }
            }
            Err(e) => eprintln!("Failed to save results: {}", e),
        }

        // Also save a timestamped copy
        if let Some(parent) = save_path.parent() {
            let ts = chrono::Utc::now().format("%Y%m%d_%H%M%S");
            let timestamped = parent.join(format!("run_{}.json", ts));
            if let Err(e) = persisted.save(&timestamped) {
                eprintln!("Failed to save timestamped results: {}", e);
            }
        }
    }

    // Exit with error code if there were failures
    if report.failed > 0 {
        std::process::exit(1);
    }
}
