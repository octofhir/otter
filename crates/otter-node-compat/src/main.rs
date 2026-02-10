use clap::{ArgAction, Parser, Subcommand};
use colored::*;
use indicatif::{ProgressBar, ProgressStyle};
use std::path::PathBuf;
use std::time::Instant;

use otter_node_compat::{
    PersistedReport, TestOutcome, TestReport, compare, config::NodeCompatConfig,
    runner::NodeCompatRunner,
};

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

#[derive(Parser, Debug)]
#[command(name = "node-compat")]
#[command(about = "Run Node.js compatibility tests against Otter VM")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Path to test files directory (flat: tests live directly in this dir)
    #[arg(
        long,
        default_value = "tests/node-compat/node/test/parallel",
        global = true
    )]
    test_dir: PathBuf,

    /// Path to harness directory
    #[arg(long, default_value = "tests/node-compat/harness", global = true)]
    harness_dir: PathBuf,

    /// Run tests for a specific module only
    #[arg(short, long, global = true)]
    module: Option<String>,

    /// Filter tests by path pattern
    #[arg(short, long, global = true)]
    filter: Option<String>,

    /// Output results as JSON
    #[arg(long, global = true)]
    json: bool,

    /// Verbosity level: -v colored, -vv names, -vvv output
    #[arg(short, long, action = ArgAction::Count, global = true)]
    verbose: u8,

    /// Maximum number of tests to run
    #[arg(short = 'n', long, global = true)]
    max_tests: Option<usize>,

    /// Only list tests without running them
    #[arg(long, global = true)]
    list_only: bool,

    /// Timeout in seconds per test
    #[arg(long, global = true)]
    timeout: Option<u64>,

    /// Path to config file
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    /// Save results to a JSON file
    #[arg(long, global = true)]
    save: Option<Option<PathBuf>>,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Run node compatibility tests (default)
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
    /// List available modules and test counts
    Status,
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn main() {
    const STACK_SIZE: usize = 64 * 1024 * 1024;
    let builder = std::thread::Builder::new()
        .name("node-compat-main".into())
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
    let cli = Cli::parse();

    match cli.command {
        Some(Commands::Compare { base, current }) => {
            run_compare(&base, &current);
        }
        Some(Commands::Status) => {
            run_status(&cli);
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
        Ok(comparison) => {
            comparison.print();
            if comparison.has_regressions() {
                std::process::exit(1);
            }
        }
        Err(e) => {
            eprintln!("{}: {}", "Error".red().bold(), e);
            std::process::exit(1);
        }
    }
}

// ---------------------------------------------------------------------------
// Status command
// ---------------------------------------------------------------------------

fn run_status(cli: &Cli) {
    let config = NodeCompatConfig::load_or_default(cli.config.as_deref());
    let runner = NodeCompatRunner::new(&cli.test_dir, &cli.harness_dir, config);

    let modules = runner.list_modules();
    if modules.is_empty() {
        println!("No test modules found in {}", cli.test_dir.display());
        println!("Run test fetching first (see justfile).");
        return;
    }

    println!("{}", "=== Available Modules ===".bold().cyan());
    for module in &modules {
        let count = runner.list_tests_for_module(module).len();
        println!("  {:<16} {} tests", module, count);
    }

    // Show latest report if available
    let report_path = PathBuf::from("tests/node-compat/reports/latest.json");
    if report_path.exists() {
        if let Ok(report) = PersistedReport::load(&report_path) {
            println!();
            println!("{}", "=== Latest Results ===".bold().cyan());
            println!(
                "  Pass rate: {:.1}% ({}/{})",
                report.summary.pass_rate, report.summary.passed, report.summary.total,
            );
            println!("  Run at: {}", report.timestamp);
        }
    }
}

// ---------------------------------------------------------------------------
// Run command
// ---------------------------------------------------------------------------

async fn run_tests(cli: Cli) {
    let config = NodeCompatConfig::load_or_default(cli.config.as_deref());
    let save_results = cli.save.is_some();
    let save_path = cli.save.as_ref().and_then(|opt| opt.clone()).or_else(|| {
        if save_results {
            Some(PathBuf::from("tests/node-compat/reports/latest.json"))
        } else {
            None
        }
    });

    let timeout_secs = cli.timeout.unwrap_or(config.timeout_secs);
    let timeout = Some(std::time::Duration::from_secs(timeout_secs));

    if !cli.json {
        eprintln!("{}", "Otter Node.js Compatibility Runner".bold().cyan());
        eprintln!("Test directory: {}", cli.test_dir.display());
    }

    let mut runner = NodeCompatRunner::new(&cli.test_dir, &cli.harness_dir, config);

    // Collect tests
    let tests: Vec<(String, PathBuf)> = if let Some(ref module) = cli.module {
        let paths = runner.list_tests_for_module(module);
        paths.into_iter().map(|p| (module.clone(), p)).collect()
    } else {
        runner.list_all_tests()
    };

    // Apply filter
    let tests: Vec<(String, PathBuf)> = if let Some(ref filter) = cli.filter {
        tests
            .into_iter()
            .filter(|(_, p)| p.to_string_lossy().contains(filter.as_str()))
            .collect()
    } else {
        tests
    };

    // Limit
    let tests: Vec<(String, PathBuf)> = if let Some(max) = cli.max_tests {
        tests.into_iter().take(max).collect()
    } else {
        tests
    };

    let test_count = tests.len();

    if cli.list_only {
        for (module, path) in &tests {
            println!("[{}] {}", module, path.display());
        }
        println!("\nTotal: {} tests", test_count);
        return;
    }

    if !cli.json {
        eprintln!("Found {} test files", test_count);
        if let Some(ref module) = cli.module {
            eprintln!("Module: {}", module);
        }
    }

    // Progress bar
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
    let mut all_results = Vec::with_capacity(test_count);
    let mut passed = 0usize;
    let mut failed = 0usize;
    let mut skipped = 0usize;

    for (module, test_path) in &tests {
        let result = runner.run_test(module, test_path, timeout).await;

        // Verbosity output
        if !cli.json {
            match cli.verbose {
                0 => {} // progress bar
                1 => {
                    let ch = match result.outcome {
                        TestOutcome::Pass => ".".green(),
                        TestOutcome::Fail => "F".red(),
                        TestOutcome::Skip => "S".yellow(),
                        TestOutcome::Timeout => "T".magenta(),
                        TestOutcome::Crash => "!".red().bold(),
                    };
                    eprint!("{}", ch);
                    if all_results.len() % 80 == 79 {
                        eprintln!();
                    }
                }
                2 => {
                    let status = match result.outcome {
                        TestOutcome::Pass => "PASS".green(),
                        TestOutcome::Fail => "FAIL".red(),
                        TestOutcome::Skip => "SKIP".yellow(),
                        TestOutcome::Timeout => "TIME".magenta(),
                        TestOutcome::Crash => "CRASH".red().bold(),
                    };
                    eprintln!("[{}] {} ({}ms)", status, result.path, result.duration_ms,);
                }
                _ => {
                    let status = match result.outcome {
                        TestOutcome::Pass => "PASS".green(),
                        TestOutcome::Fail => "FAIL".red(),
                        TestOutcome::Skip => "SKIP".yellow(),
                        TestOutcome::Timeout => "TIME".magenta(),
                        TestOutcome::Crash => "CRASH".red().bold(),
                    };
                    eprintln!("[{}] {} ({}ms)", status, result.path, result.duration_ms,);
                    if let Some(ref err) = result.error {
                        eprintln!("  Error: {}", err);
                    }
                }
            }
        }

        match result.outcome {
            TestOutcome::Pass => passed += 1,
            TestOutcome::Fail => failed += 1,
            TestOutcome::Skip => skipped += 1,
            _ => {}
        }

        all_results.push(result);

        if let Some(ref pb) = pb {
            pb.inc(1);
            pb.set_message(format!(
                "Pass: {} Fail: {} Skip: {}",
                passed, failed, skipped
            ));
        }
    }

    // Finish progress
    if let Some(ref pb) = pb {
        pb.finish_and_clear();
    }
    if cli.verbose == 1 && !cli.json {
        eprintln!();
    }

    let run_duration = run_start.elapsed();
    let report = TestReport::from_results(&all_results);

    if cli.json {
        match report.to_json() {
            Ok(json) => println!("{}", json),
            Err(e) => eprintln!("Failed to generate JSON: {}", e),
        }
    } else {
        report.print_summary();
    }

    // Save results if requested
    if let Some(save_path) = save_path {
        let persisted = PersistedReport {
            timestamp: chrono::Utc::now().to_rfc3339(),
            otter_version: env!("CARGO_PKG_VERSION").to_string(),
            duration_secs: run_duration.as_secs_f64(),
            summary: report,
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

        // Timestamped copy
        if let Some(parent) = save_path.parent() {
            let ts = chrono::Utc::now().format("%Y%m%d_%H%M%S");
            let timestamped = parent.join(format!("run_{}.json", ts));
            let _ = persisted.save(&timestamped);
        }
    }
}
