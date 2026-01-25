//! Test262 runner CLI

use clap::Parser;
use colored::*;
use std::path::PathBuf;

use otter_test262::{Test262Runner, TestReport};

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
}

fn main() {
    let args = Args::parse();

    println!("{}", "Otter Test262 Runner".bold().cyan());
    println!("Test directory: {}", args.test_dir.display());

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

    // Run tests
    println!("\nRunning tests...");

    let results = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let results_clone = results.clone();
    let total_tests = std::sync::atomic::AtomicUsize::new(0);
    let passed_tests = std::sync::atomic::AtomicUsize::new(0);

    let callback = move |result: otter_test262::TestResult| {
        let total = total_tests.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;

        match result.outcome {
            otter_test262::TestOutcome::Pass => {
                passed_tests.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            otter_test262::TestOutcome::Fail | otter_test262::TestOutcome::Crash => {
                // Print failures immediately
                println!(
                    "\n{}: {} - {:?}",
                    "FAIL".red().bold(),
                    result.path,
                    result.error
                );
            }
            _ => {}
        }

        // Simple progress indication every 100 tests
        if total % 100 == 0 {
            use std::io::Write;
            print!(".");
            std::io::stdout().flush().unwrap();
        }

        results_clone.lock().unwrap().push(result);
    };

    if let Some(ref subdir) = args.subdir {
        println!("Subdirectory: {}", subdir);
        runner.run_dir_with_callback(subdir, callback);
    } else {
        runner.run_all_with_callback(callback);
    };

    println!(); // Newline after progress dots

    let results = std::sync::Arc::try_unwrap(results)
        .unwrap()
        .into_inner()
        .unwrap();

    // Limit results if needed
    let results: Vec<_> = if let Some(max) = args.max_tests {
        results.into_iter().take(max).collect()
    } else {
        results
    };

    // Generate report
    let report = TestReport::from_results(&results);

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

    // Exit with error code if there were failures
    if report.failed > 0 {
        std::process::exit(1);
    }
}
