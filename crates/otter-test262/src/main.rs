use clap::Parser;
use colored::*;
use std::collections::HashMap;
use std::path::PathBuf;
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, RefreshKind, System};
use tracing_subscriber::filter::EnvFilter;

use otter_test262::{FeatureReport, Test262Runner, TestOutcome, TestReport, report::FailureInfo};

#[derive(Parser, Debug)]
#[command(name = "test262")]
#[command(about = "Run Test262 conformance tests against Otter VM")]
struct Args {
    /// Path to test262 directory
    #[arg(short, long, default_value = "tests/test262")]
    test_dir: PathBuf,

    /// Filter tests by path pattern
    #[arg(short, long)]
    filter: Option<String>,

    /// Run only tests in this subdirectory (e.g., "language/expressions")
    #[arg(short = 'd', long)]
    subdir: Option<String>,

    /// Output results as JSON
    #[arg(long)]
    json: bool,

    /// Show verbose output
    #[arg(short, long)]
    verbose: bool,

    /// Maximum number of tests to run
    #[arg(short = 'n', long)]
    max_tests: Option<usize>,

    /// Only list tests without running them
    #[arg(long)]
    list_only: bool,

    /// Show memory usage statistics
    #[arg(long)]
    memory_stats: bool,
}

/// Memory statistics tracker
struct MemoryTracker {
    system: System,
    pid: Pid,
    peak_memory_bytes: u64,
    initial_memory_bytes: u64,
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
            peak_memory_bytes: initial,
            initial_memory_bytes: initial,
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
            if current > self.peak_memory_bytes {
                self.peak_memory_bytes = current;
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
            .map(|p| p.memory() as f64 / 1024.0 / 1024.0)
            .unwrap_or(0.0)
    }

    fn peak_memory_mb(&self) -> f64 {
        self.peak_memory_bytes as f64 / 1024.0 / 1024.0
    }

    fn initial_memory_mb(&self) -> f64 {
        self.initial_memory_bytes as f64 / 1024.0 / 1024.0
    }

    fn memory_increase_mb(&mut self) -> f64 {
        self.current_memory_mb() - self.initial_memory_mb()
    }
}

struct RunSummary {
    total: usize,
    passed: usize,
    failed: usize,
    skipped: usize,
    timeout: usize,
    crashed: usize,
    by_feature: HashMap<String, FeatureReport>,
    failures: Vec<FailureInfo>,
    max_failures: usize,
}

impl RunSummary {
    fn new(max_failures: usize) -> Self {
        Self {
            total: 0,
            passed: 0,
            failed: 0,
            skipped: 0,
            timeout: 0,
            crashed: 0,
            by_feature: HashMap::new(),
            failures: Vec::new(),
            max_failures,
        }
    }

    fn record(&mut self, result: &otter_test262::TestResult) {
        self.total += 1;
        match result.outcome {
            TestOutcome::Pass => self.passed += 1,
            TestOutcome::Fail => {
                self.failed += 1;
                if self.failures.len() < self.max_failures {
                    self.failures.push(FailureInfo {
                        path: result.path.clone(),
                        error: result.error.clone().unwrap_or_default(),
                    });
                }
            }
            TestOutcome::Skip => self.skipped += 1,
            TestOutcome::Timeout => self.timeout += 1,
            TestOutcome::Crash => self.crashed += 1,
        }

        for feature in &result.features {
            let feature_report = self.by_feature.entry(feature.clone()).or_default();
            feature_report.total += 1;
            match result.outcome {
                TestOutcome::Pass => feature_report.passed += 1,
                TestOutcome::Fail => feature_report.failed += 1,
                TestOutcome::Skip => feature_report.skipped += 1,
                _ => {}
            }
        }
    }

    fn to_report(self) -> TestReport {
        let run_count = self.passed + self.failed + self.timeout + self.crashed;
        let pass_rate = if run_count > 0 {
            (self.passed as f64 / run_count as f64) * 100.0
        } else {
            0.0
        };

        TestReport {
            total: self.total,
            passed: self.passed,
            failed: self.failed,
            skipped: self.skipped,
            timeout: self.timeout,
            crashed: self.crashed,
            pass_rate,
            by_feature: self.by_feature,
            failures: self.failures,
        }
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("warn".parse().unwrap()))
        .init();

    let args = Args::parse();

    println!("{}", "Otter Test262 Runner".bold().cyan());
    println!("Test directory: {}", args.test_dir.display());

    // Initialize memory tracking if requested
    let mut memory_tracker = if args.memory_stats {
        let tracker = MemoryTracker::new();
        println!("Initial memory: {:.2} MB", tracker.initial_memory_mb());
        Some(tracker)
    } else {
        None
    };

    // Create runner
    let mut runner = Test262Runner::new(&args.test_dir);

    if let Some(ref filter) = args.filter {
        runner = runner.with_filter(filter.clone());
        println!("Filter: {}", filter);
    }

    // List-only mode
    if args.list_only {
        let tests = if let Some(ref subdir) = args.subdir {
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

    // Run tests
    println!("\nRunning tests...");
    use std::io::Write;
    std::io::stdout().flush().unwrap();

    let mut tests = if let Some(ref subdir) = args.subdir {
        println!("Subdirectory: {}", subdir);
        runner.list_tests_dir(subdir)
    } else {
        runner.list_tests()
    };


    if let Some(max) = args.max_tests {
        tests.truncate(max);
    }

    let mut summary = RunSummary::new(10);

    for path in tests {
        let result = runner.run_test(&path).await;

        match result.outcome {
            TestOutcome::Fail | TestOutcome::Crash => {
                println!(
                    "\n{}: {} - {:?}",
                    "FAIL".red().bold(),
                    result.path,
                    result.error
                );
            }
            _ => {}
        }

        summary.record(&result);

        if summary.total % 100 == 0 {
            use std::io::Write;
            print!(".");
            std::io::stdout().flush().unwrap();
        }

        if let Some(ref mut tracker) = memory_tracker {
            if summary.total % 100 == 0 {
                tracker.update();
            }
        }
    }

    println!(); // Newline after progress dots

    if let Some(ref mut tracker) = memory_tracker {
        tracker.update();
    }

    let report = summary.to_report();

    if args.json {
        // Output JSON
        match report.to_json() {
            Ok(json) => println!("{}", json),
            Err(e) => eprintln!("Failed to generate JSON: {}", e),
        }
    } else {
        // Print summary
        report.print_summary();

        // Print detailed failures if verbose
        if args.verbose && !report.failures.is_empty() {
            println!("\n{}", "=== All Failures ===".bold().red());
            for failure in &report.failures {
                println!("{}: {}", failure.path.yellow(), failure.error);
            }
        }
    }

    // Print memory statistics
    if let Some(ref mut tracker) = memory_tracker {
        println!();
        println!("╭─────────────────────────────────────╮");
        println!("│       Otter Profiling Report        │");
        println!("├─────────────────────────────────────┤");
        println!("│ Execution Statistics                │");
        println!("│   Total Tests: {:>10}           │", report.total);
        println!("│   Passed:      {:>10}           │", report.passed);
        println!("│   Failed:      {:>10}           │", report.failed);
        println!("│   Pass Rate:   {:>10.2}%          │", report.pass_rate);
        println!("├─────────────────────────────────────┤");
        println!("│ Memory Usage Metrics                │");
        println!(
            "│   Initial:     {:>10.2} MB       │",
            tracker.initial_memory_mb()
        );
        println!(
            "│   Peak:        {:>10.2} MB       │",
            tracker.peak_memory_mb()
        );
        println!(
            "│   Current:     {:>10.2} MB       │",
            tracker.current_memory_mb()
        );
        println!(
            "│   Increase:    {:>10.2} MB       │",
            tracker.memory_increase_mb()
        );
        println!("╰─────────────────────────────────────╯");
    }

    // Exit with error code if there were failures
    if report.failed > 0 {
        std::process::exit(1);
    }
}
