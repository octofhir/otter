//! Otter CLI - A fast TypeScript/JavaScript runtime.
//!
//! VM-based JavaScript execution with pluggable builtins.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use tracing_subscriber::filter::EnvFilter;

// Use otter-engine as the single entry point
use otter_engine::{CapabilitiesBuilder, EngineBuilder, PropertyKey, Value};

mod commands;
mod config;

#[derive(Parser)]
#[command(
    name = "otter",
    version,
    about = "A fast TypeScript/JavaScript runtime",
    long_about = "Otter is a secure, fast TypeScript/JavaScript runtime powered by a custom VM."
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Script file to run (shorthand for `otter run <file>`)
    #[arg(value_name = "FILE")]
    file: Option<PathBuf>,

    /// Evaluate argument as a script
    #[arg(short = 'e', long = "eval")]
    eval: Option<String>,

    /// Verbose output
    #[arg(short, long, global = true)]
    verbose: bool,

    /// Config file path
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    /// Allow all permissions
    #[arg(long = "allow-all", short = 'A', global = true)]
    allow_all: bool,

    /// Allow network access
    #[arg(long = "allow-net", global = true)]
    allow_net: bool,

    /// Allow file system read
    #[arg(long = "allow-read", global = true)]
    allow_read: bool,

    /// Allow file system write
    #[arg(long = "allow-write", global = true)]
    allow_write: bool,

    /// Allow environment variable access
    #[arg(long = "allow-env", global = true)]
    allow_env: bool,

    /// Allow subprocess execution
    #[arg(long = "allow-run", global = true)]
    allow_run: bool,

    /// Execution timeout in seconds (0 = no timeout)
    #[arg(long, global = true, default_value = "30")]
    timeout: u64,

    /// Show profiling information (memory usage)
    #[arg(long, global = true)]
    profile: bool,
}

#[derive(Subcommand)]
enum Commands {
    /// Run a JavaScript/TypeScript file
    Run {
        /// The script file to run
        file: PathBuf,
    },
    /// Interactive REPL
    Repl,
    /// Run test files
    Test {
        /// Test files or directories (defaults to current directory)
        #[arg(default_value = ".")]
        paths: Vec<PathBuf>,

        /// Filter tests by name pattern
        #[arg(long, short = 'f')]
        filter: Option<String>,
    },
    /// Type check without running
    Check {
        /// Files to check
        files: Vec<PathBuf>,
    },
    /// Build/bundle a project
    Build {
        /// Entry point file
        entry: PathBuf,

        /// Output file
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
    /// Install dependencies
    Install {
        /// Packages to install (if not provided, installs from package.json)
        #[arg()]
        packages: Vec<String>,

        /// Save as dev dependency
        #[arg(long, short = 'D')]
        save_dev: bool,
    },
    /// Add a package
    Add {
        /// Package name (optionally with version: package@version)
        package: String,

        /// Save as dev dependency
        #[arg(long, short = 'D')]
        dev: bool,
    },
    /// Remove a package
    Remove {
        /// Package name to remove
        package: String,
    },
    /// Execute a package binary
    Exec {
        /// Package binary to run
        command: String,

        /// Arguments to pass
        #[arg(trailing_var_arg = true)]
        args: Vec<String>,
    },
    /// Initialize a new project
    Init,
    /// Show runtime information
    Info {
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("warn".parse()?))
        .init();

    let cli = Cli::parse();

    // Handle --eval flag
    if let Some(ref code) = cli.eval {
        return run_code(code, "<eval>", &cli).await;
    }

    // Handle direct file argument (otter script.js)
    if let Some(ref file) = cli.file {
        return run_file(file, &cli).await;
    }

    match &cli.command {
        Some(Commands::Run { file }) => run_file(file, &cli).await,
        Some(Commands::Repl) => run_repl(&cli).await,
        Some(Commands::Test { paths, filter }) => run_tests(paths, filter.as_deref(), &cli).await,
        Some(Commands::Check { files }) => {
            // Type checking stub - tsgo integration pending
            println!("Type checking: {:?}", files);
            println!("Note: Type checking integration (tsgo) is being ported to the new VM.");
            Ok(())
        }
        Some(Commands::Build { entry, output }) => {
            // Build stub - bundling pending
            println!("Building: {}", entry.display());
            if let Some(out) = output {
                println!("Output: {}", out.display());
            }
            println!("Note: Bundling is being ported to the new VM.");
            Ok(())
        }
        Some(Commands::Install { packages, save_dev }) => {
            commands::install::run(packages, *save_dev).await
        }
        Some(Commands::Add { package, dev }) => commands::add::run(package, *dev).await,
        Some(Commands::Remove { package }) => commands::remove::run(package).await,
        Some(Commands::Exec { command, args }) => commands::exec::run(command, args).await,
        Some(Commands::Init) => {
            commands::init::run()?;
            Ok(())
        }
        Some(Commands::Info { json }) => {
            commands::info::run(*json);
            Ok(())
        }
        None => {
            use clap::CommandFactory;
            Cli::command().print_help()?;
            println!();
            Ok(())
        }
    }
}

/// Build capabilities from CLI flags
fn build_capabilities(cli: &Cli) -> otter_engine::Capabilities {
    if cli.allow_all {
        CapabilitiesBuilder::new()
            .allow_net_all()
            .allow_read_all()
            .allow_write_all()
            .allow_env_all()
            .build()
    } else {
        let mut builder = CapabilitiesBuilder::new();
        if cli.allow_net {
            builder = builder.allow_net_all();
        }
        if cli.allow_read {
            builder = builder.allow_read_all();
        }
        if cli.allow_write {
            builder = builder.allow_write_all();
        }
        if cli.allow_env {
            builder = builder.allow_env_all();
        }
        builder.build()
    }
}

/// Run a JavaScript file
async fn run_file(path: &PathBuf, cli: &Cli) -> Result<()> {
    let source = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read file: {}", path.display()))?;

    let source_url = path.to_string_lossy();
    run_code(&source, &source_url, cli).await
}

/// Run JavaScript code using EngineBuilder
async fn run_code(source: &str, _source_url: &str, cli: &Cli) -> Result<()> {
    use std::sync::atomic::Ordering;
    use std::time::Instant;
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, RefreshKind, System};

    // Start timing
    let start_time = Instant::now();

    // Initialize memory tracking if profiling is enabled
    let mut profiler = if cli.profile {
        let pid = Pid::from_u32(std::process::id());
        let mut sys = System::new_with_specifics(
            RefreshKind::nothing().with_processes(ProcessRefreshKind::everything()),
        );
        sys.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[pid]),
            true,
            ProcessRefreshKind::everything(),
        );
        let initial_rss = sys.process(pid).map(|p| p.memory()).unwrap_or(0);
        let initial_virt = sys.process(pid).map(|p| p.virtual_memory()).unwrap_or(0);
        Some((sys, pid, initial_rss, initial_virt, start_time))
    } else {
        None
    };

    let caps = build_capabilities(cli);

    // Create engine with builtins (EngineBuilder handles all setup)
    let mut engine = EngineBuilder::new()
        .capabilities(caps)
        // TODO: Fix HTTP extension with Rust intrinsics
        // .with_http() // Enable Otter.serve()
        .build();

    // Set up timeout if specified (0 = no timeout)
    let timeout_handle = if cli.timeout > 0 {
        let interrupt_flag = engine.interrupt_flag();
        let timeout_secs = cli.timeout;
        Some(tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(timeout_secs)).await;
            interrupt_flag.store(true, Ordering::Relaxed);
        }))
    } else {
        None
    };

    // Execute code
    let result = engine.eval(source).await;

    // Cancel timeout task if still running
    if let Some(handle) = timeout_handle {
        handle.abort();
    }

    match result {
        Ok(value) => {
            // Print result if it's not undefined
            if !value.is_undefined() {
                println!("{}", format_value(&value));
            }

            // Print detailed profiling stats
            if let Some((ref mut sys, pid, initial_rss, initial_virt, start)) = profiler {
                let elapsed = start.elapsed();
                sys.refresh_processes_specifics(
                    ProcessesToUpdate::Some(&[pid]),
                    true,
                    ProcessRefreshKind::everything(),
                );

                let current_rss = sys.process(pid).map(|p| p.memory()).unwrap_or(0);
                let current_virt = sys.process(pid).map(|p| p.virtual_memory()).unwrap_or(0);

                println!();
                println!("╭─────────────────────────────────────╮");
                println!("│       Otter Profiling Report        │");
                println!("├─────────────────────────────────────┤");
                println!("│ Execution Time                      │");
                println!(
                    "│   Total:     {:>10.2} ms          │",
                    elapsed.as_secs_f64() * 1000.0
                );
                println!("├─────────────────────────────────────┤");
                println!("│ Memory Usage (RSS)                  │");
                println!(
                    "│   Initial:   {:>10.2} MB          │",
                    initial_rss as f64 / 1024.0 / 1024.0
                );
                println!(
                    "│   Current:   {:>10.2} MB          │",
                    current_rss as f64 / 1024.0 / 1024.0
                );
                println!(
                    "│   Delta:     {:>+10.2} MB          │",
                    (current_rss as i64 - initial_rss as i64) as f64 / 1024.0 / 1024.0
                );
                println!("├─────────────────────────────────────┤");
                println!("│ Virtual Memory                      │");
                println!(
                    "│   Initial:   {:>10.2} MB          │",
                    initial_virt as f64 / 1024.0 / 1024.0
                );
                println!(
                    "│   Current:   {:>10.2} MB          │",
                    current_virt as f64 / 1024.0 / 1024.0
                );
                println!("╰─────────────────────────────────────╯");
            }

            Ok(())
        }
        Err(e) => {
            let err_str = e.to_string();
            if err_str.contains("interrupted") || err_str.contains("Interrupted") {
                Err(anyhow::anyhow!(
                    "Execution timed out after {} seconds",
                    cli.timeout
                ))
            } else {
                Err(anyhow::anyhow!("{}", e))
            }
        }
    }
}

/// Run interactive REPL
async fn run_repl(cli: &Cli) -> Result<()> {
    use std::io::{self, BufRead, Write};

    println!("Otter {} - TypeScript Runtime", env!("CARGO_PKG_VERSION"));
    println!("Type .help for help, .exit to exit\n");

    let caps = build_capabilities(cli);
    let mut engine = EngineBuilder::new().capabilities(caps).with_http().build();

    let stdin = io::stdin();
    let mut stdout = io::stdout();

    let mut multiline_buffer = String::new();
    let mut in_multiline = false;

    loop {
        // Print prompt
        let prompt = if in_multiline { "...> " } else { "otter> " };
        print!("{}", prompt);
        stdout.flush()?;

        // Read line
        let mut line = String::new();
        match stdin.lock().read_line(&mut line) {
            Ok(0) => break, // EOF
            Ok(_) => {}
            Err(e) => {
                eprintln!("Error reading input: {}", e);
                break;
            }
        }

        let line = line.trim_end();

        // Handle empty input
        if line.is_empty() && !in_multiline {
            continue;
        }

        // Handle REPL commands
        if line.starts_with('.') && !in_multiline {
            match line {
                ".exit" | ".quit" | ".q" => break,
                ".help" | ".h" => {
                    print_repl_help();
                    continue;
                }
                ".clear" | ".cls" => {
                    print!("\x1B[2J\x1B[1;1H");
                    stdout.flush()?;
                    continue;
                }
                ".multiline" | ".m" => {
                    in_multiline = true;
                    println!("Entering multiline mode. Type .end to execute, .cancel to abort.");
                    continue;
                }
                ".end" => {
                    if in_multiline {
                        let code = std::mem::take(&mut multiline_buffer);
                        in_multiline = false;
                        eval_repl_line(&mut engine, &code).await;
                    }
                    continue;
                }
                ".cancel" => {
                    multiline_buffer.clear();
                    in_multiline = false;
                    println!("Multiline input cancelled.");
                    continue;
                }
                _ => {
                    println!(
                        "Unknown command: {}. Type .help for available commands.",
                        line
                    );
                    continue;
                }
            }
        }

        // Handle multiline mode
        if in_multiline {
            multiline_buffer.push_str(line);
            multiline_buffer.push('\n');
            continue;
        }

        // Evaluate single line
        eval_repl_line(&mut engine, line).await;
    }

    println!("\nGoodbye!");
    Ok(())
}

async fn eval_repl_line(engine: &mut otter_engine::Otter, line: &str) {
    match engine.eval(line).await {
        Ok(value) => {
            if !value.is_undefined() {
                println!("{}", format_value(&value));
            }
        }
        Err(e) => {
            eprintln!("error: {}", e);
        }
    }
}

fn print_repl_help() {
    println!("REPL Commands:");
    println!("  .help, .h      Show this help message");
    println!("  .exit, .q      Exit the REPL");
    println!("  .clear, .cls   Clear the screen");
    println!("  .multiline, .m Enter multiline mode");
    println!("  .end           Execute multiline input");
    println!("  .cancel        Cancel multiline input");
    println!();
    println!("You can type any JavaScript or TypeScript expression.");
}

/// Run test files
async fn run_tests(paths: &[PathBuf], filter: Option<&str>, cli: &Cli) -> Result<()> {
    let test_files = find_test_files(paths)?;

    if test_files.is_empty() {
        println!("No test files found.");
        return Ok(());
    }

    println!("Running {} test file(s)...\n", test_files.len());

    let mut total_passed = 0;
    let mut total_failed = 0;
    let mut total_skipped = 0;

    for file in &test_files {
        match run_test_file(file, filter, cli).await {
            Ok((passed, failed, skipped)) => {
                total_passed += passed;
                total_failed += failed;
                total_skipped += skipped;
            }
            Err(e) => {
                eprintln!("Error running {}: {}", file.display(), e);
                total_failed += 1;
            }
        }
    }

    println!();
    if total_failed > 0 {
        println!(
            "Result: {} passed, {} failed, {} skipped",
            total_passed, total_failed, total_skipped
        );
        std::process::exit(1);
    } else {
        println!("Result: {} passed, {} skipped", total_passed, total_skipped);
    }

    Ok(())
}

fn find_test_files(paths: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();

    for path in paths {
        if path.is_file() {
            if is_test_file(path) {
                files.push(path.clone());
            }
        } else if path.is_dir() {
            find_test_files_in_dir(path, &mut files)?;
        }
    }

    files.sort();
    Ok(files)
}

fn find_test_files_in_dir(dir: &PathBuf, files: &mut Vec<PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();

        // Skip node_modules and hidden directories
        if path.is_dir() {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name == "node_modules" || name.starts_with('.') {
                continue;
            }
            find_test_files_in_dir(&path, files)?;
        } else if is_test_file(&path) {
            files.push(path);
        }
    }

    Ok(())
}

fn is_test_file(path: &std::path::Path) -> bool {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");

    // Match patterns like *.test.ts, *.spec.ts, *_test.ts
    name.ends_with(".test.ts")
        || name.ends_with(".test.js")
        || name.ends_with(".spec.ts")
        || name.ends_with(".spec.js")
        || name.ends_with("_test.ts")
        || name.ends_with("_test.js")
}

async fn run_test_file(
    path: &PathBuf,
    filter: Option<&str>,
    cli: &Cli,
) -> Result<(usize, usize, usize)> {
    println!("  {}", path.display());

    let source = std::fs::read_to_string(path)?;
    let caps = build_capabilities(cli);

    let mut engine = EngineBuilder::new().capabilities(caps).with_http().build();

    // Inject test framework
    let filter_json = match filter {
        Some(f) => format!("\"{}\"", f),
        None => "null".to_string(),
    };

    let test_harness = format!(
        r#"
globalThis.__otter_tests = [];
globalThis.__otter_results = {{ passed: 0, failed: 0, skipped: 0 }};
globalThis.__otter_filter = {filter_json};

globalThis.describe = function(name, fn) {{
    fn();
}};

globalThis.it = globalThis.test = function(name, fn) {{
    const filter = globalThis.__otter_filter;
    if (filter && !name.includes(filter)) {{
        globalThis.__otter_results.skipped++;
        return;
    }}
    globalThis.__otter_tests.push({{ name, fn }});
}};

globalThis.expect = function(actual) {{
    return {{
        toBe: function(expected) {{
            if (actual !== expected) {{
                throw new Error("Expected " + JSON.stringify(expected) + " but got " + JSON.stringify(actual));
            }}
        }},
        toEqual: function(expected) {{
            if (JSON.stringify(actual) !== JSON.stringify(expected)) {{
                throw new Error("Expected " + JSON.stringify(expected) + " but got " + JSON.stringify(actual));
            }}
        }},
        toBeTruthy: function() {{
            if (!actual) {{
                throw new Error("Expected truthy but got " + JSON.stringify(actual));
            }}
        }},
        toBeFalsy: function() {{
            if (actual) {{
                throw new Error("Expected falsy but got " + JSON.stringify(actual));
            }}
        }},
        toThrow: function(message) {{
            let threw = false;
            try {{
                if (typeof actual === 'function') actual();
            }} catch (e) {{
                threw = true;
                if (message && !e.message.includes(message)) {{
                    throw new Error("Expected error containing '" + message + "' but got '" + e.message + "'");
                }}
            }}
            if (!threw) {{
                throw new Error("Expected function to throw");
            }}
        }},
        not: {{
            toBe: function(expected) {{
                if (actual === expected) {{
                    throw new Error("Expected not to be " + JSON.stringify(expected));
                }}
            }},
            toEqual: function(expected) {{
                if (JSON.stringify(actual) === JSON.stringify(expected)) {{
                    throw new Error("Expected not to equal " + JSON.stringify(expected));
                }}
            }},
        }}
    }};
}};

// Load test file
{source}

// Run tests synchronously for now
for (const test of globalThis.__otter_tests) {{
    try {{
        test.fn();
        globalThis.__otter_results.passed++;
        console.log("    ✓ " + test.name);
    }} catch (e) {{
        globalThis.__otter_results.failed++;
        console.log("    ✗ " + test.name);
        console.log("      " + e.message);
    }}
}}

globalThis.__otter_results;
"#
    );

    match engine.eval(&test_harness).await {
        Ok(result) => {
            // Try to extract results from the returned object
            if let Some(obj) = result.as_object() {
                let passed = obj
                    .get(&PropertyKey::string("passed"))
                    .and_then(|v| v.as_int32())
                    .unwrap_or(0) as usize;
                let failed = obj
                    .get(&PropertyKey::string("failed"))
                    .and_then(|v| v.as_int32())
                    .unwrap_or(0) as usize;
                let skipped = obj
                    .get(&PropertyKey::string("skipped"))
                    .and_then(|v| v.as_int32())
                    .unwrap_or(0) as usize;
                Ok((passed, failed, skipped))
            } else {
                Ok((0, 1, 0))
            }
        }
        Err(e) => {
            eprintln!("    Error: {}", e);
            Ok((0, 1, 0))
        }
    }
}

/// Format a Value for display
fn format_value(value: &Value) -> String {
    if value.is_undefined() {
        return "undefined".to_string();
    }

    if value.is_null() {
        return "null".to_string();
    }

    if let Some(b) = value.as_boolean() {
        return b.to_string();
    }

    if let Some(n) = value.as_number() {
        if n.is_nan() {
            return "NaN".to_string();
        }
        if n.is_infinite() {
            return if n.is_sign_positive() {
                "Infinity"
            } else {
                "-Infinity"
            }
            .to_string();
        }
        if n.fract() == 0.0 && n.abs() < 1e15 {
            return format!("{}", n as i64);
        }
        return format!("{}", n);
    }

    if let Some(s) = value.as_string() {
        return format!("'{}'", s.as_str());
    }

    if let Some(obj) = value.as_object() {
        if obj.is_array() {
            let len = obj
                .get(&PropertyKey::string("length"))
                .and_then(|v| v.as_int32())
                .unwrap_or(0);
            return format!("[Array({})]", len);
        }
        return "[object Object]".to_string();
    }

    if value.is_function() {
        return "[Function]".to_string();
    }

    "[unknown]".to_string()
}
