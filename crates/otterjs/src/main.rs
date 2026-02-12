//! Otter CLI - A fast TypeScript/JavaScript runtime.
//!
//! VM-based JavaScript execution with pluggable builtins.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use tracing_subscriber::filter::EnvFilter;

// Use otter-engine as the single entry point
use otter_engine::{CapabilitiesBuilder, EngineBuilder, NodeApiProfile, PropertyKey, Value};

mod commands;
mod config;
mod watch;

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

    /// Evaluate argument as a script (silent, use -p to print result)
    #[arg(short = 'e', long = "eval")]
    eval: Option<String>,

    /// Evaluate argument as a script and print the result
    #[arg(short = 'p', long = "print")]
    print: Option<String>,

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

    /// Node.js API profile (`none`, `safe-core`, `full`)
    #[arg(long = "node-api", value_enum, global = true, default_value_t = NodeApiMode::Full)]
    node_api: NodeApiMode,

    /// Execution timeout in seconds (0 = no timeout)
    #[arg(long, global = true, default_value = "30")]
    timeout: u64,

    /// Show profiling information (memory usage)
    #[arg(long, global = true)]
    profile: bool,

    /// Dump debug snapshot on timeout (default: true)
    #[arg(long, global = true, default_value = "true")]
    dump_on_timeout: bool,

    /// File path for timeout dumps (default: stderr)
    #[arg(long, global = true)]
    dump_file: Option<PathBuf>,

    /// Number of instructions to keep in ring buffer (default: 100)
    #[arg(long, global = true, default_value = "100")]
    dump_buffer_size: usize,

    /// Enable full execution trace (logs every instruction to file)
    #[arg(long, global = true)]
    trace: bool,

    /// File path for full trace output (required if --trace is enabled)
    #[arg(long, global = true)]
    trace_file: Option<PathBuf>,

    /// Filter trace by module/function pattern (regex)
    #[arg(long, global = true)]
    trace_filter: Option<String>,

    /// Capture timing information in trace (adds overhead)
    #[arg(long, global = true)]
    trace_timing: bool,
}

#[derive(Clone, Copy, Debug, clap::ValueEnum)]
enum NodeApiMode {
    None,
    SafeCore,
    Full,
}

impl NodeApiMode {
    fn to_profile(self) -> NodeApiProfile {
        match self {
            Self::None => NodeApiProfile::None,
            Self::SafeCore => NodeApiProfile::SafeCore,
            Self::Full => NodeApiProfile::Full,
        }
    }
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

        /// Watch for changes and re-run tests
        #[arg(long, short = 'w')]
        watch: bool,
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

    // Handle --print flag (evaluate and print result)
    if let Some(ref code) = cli.print {
        return run_code(code, "<eval>", &cli, true).await;
    }

    // Handle --eval flag (evaluate silently, only console.log produces output)
    if let Some(ref code) = cli.eval {
        return run_code(code, "<eval>", &cli, false).await;
    }

    // Handle direct file argument (otter script.js)
    if let Some(ref file) = cli.file {
        return run_file(file, &cli).await;
    }

    match &cli.command {
        Some(Commands::Run { file }) => run_file(file, &cli).await,
        Some(Commands::Repl) => run_repl(&cli).await,
        Some(Commands::Test { paths, filter, watch }) => run_tests(paths, filter.as_deref(), *watch, &cli).await,
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

fn build_engine(cli: &Cli, caps: otter_engine::Capabilities) -> otter_engine::Otter {
    EngineBuilder::new()
        .capabilities(caps)
        .with_http()
        .with_nodejs_profile(cli.node_api.to_profile())
        .build()
}

struct ResolveBaseDirGuard {
    original_cwd: Option<PathBuf>,
}

impl ResolveBaseDirGuard {
    fn from_source_url(source_url: &str) -> Result<Self> {
        if source_url == "<eval>" {
            return Ok(Self { original_cwd: None });
        }

        let source_path = if let Some(raw) = source_url.strip_prefix("file://") {
            PathBuf::from(raw)
        } else {
            PathBuf::from(source_url)
        };

        let Some(parent) = source_path.parent() else {
            return Ok(Self { original_cwd: None });
        };

        if parent.as_os_str().is_empty() {
            return Ok(Self { original_cwd: None });
        }

        let original_cwd = std::env::current_dir().context("Failed to get current directory")?;
        std::env::set_current_dir(parent)
            .with_context(|| format!("Failed to switch cwd to {}", parent.display()))?;

        Ok(Self {
            original_cwd: Some(original_cwd),
        })
    }
}

impl Drop for ResolveBaseDirGuard {
    fn drop(&mut self) {
        if let Some(original) = self.original_cwd.take() {
            let _ = std::env::set_current_dir(original);
        }
    }
}

/// Run a JavaScript file
async fn run_file(path: &PathBuf, cli: &Cli) -> Result<()> {
    let source = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read file: {}", path.display()))?;

    // Use absolute path as source_url so module resolution works correctly
    // regardless of CWD changes by ResolveBaseDirGuard.
    let abs_path = std::fs::canonicalize(path)
        .unwrap_or_else(|_| std::env::current_dir().unwrap_or_default().join(path));
    let source_url = abs_path.to_string_lossy();
    run_code(&source, &source_url, cli, false).await
}

/// Run JavaScript code using EngineBuilder
async fn run_code(source: &str, source_url: &str, cli: &Cli, print_result: bool) -> Result<()> {
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
    let mut engine = {
        let _resolve_guard = ResolveBaseDirGuard::from_source_url(source_url)?;
        build_engine(cli, caps)
    };

    // Configure trace (either for full trace or timeout dumps)
    if cli.trace {
        // Full execution trace mode
        if cli.trace_file.is_none() {
            eprintln!("Error: --trace requires --trace-file to be specified");
            return Err(anyhow::anyhow!("--trace requires --trace-file"));
        }
        engine.set_trace_config(otter_vm_core::TraceConfig {
            enabled: true,
            mode: otter_vm_core::TraceMode::FullTrace,
            ring_buffer_size: cli.dump_buffer_size,
            output_path: cli.trace_file.clone(),
            filter: cli.trace_filter.clone(),
            capture_timing: cli.trace_timing,
        });
    } else if cli.dump_on_timeout {
        // Ring buffer mode for timeout dumps only
        engine.set_trace_config(otter_vm_core::TraceConfig {
            enabled: true,
            mode: otter_vm_core::TraceMode::RingBuffer,
            ring_buffer_size: cli.dump_buffer_size,
            output_path: cli.dump_file.clone(),
            filter: None,
            capture_timing: false,
        });
    }

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
    let result = engine.eval(source, Some(source_url)).await;

    // Cancel timeout task if still running
    if let Some(handle) = timeout_handle {
        handle.abort();
    }

    match result {
        Ok(value) => {
            // Print result only when explicitly requested (e.g., -p flag or REPL)
            if print_result {
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
                // Dump debug snapshot on timeout if enabled
                if cli.dump_on_timeout {
                    dump_timeout_info(&engine, &cli, Some(source_url));
                }
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
    let mut engine = build_engine(cli, caps);

    // Configure trace (though REPL usually doesn't timeout or use full trace)
    if cli.trace {
        // Full execution trace mode
        if cli.trace_file.is_none() {
            eprintln!("Error: --trace requires --trace-file to be specified");
            return Err(anyhow::anyhow!("--trace requires --trace-file"));
        }
        engine.set_trace_config(otter_vm_core::TraceConfig {
            enabled: true,
            mode: otter_vm_core::TraceMode::FullTrace,
            ring_buffer_size: cli.dump_buffer_size,
            output_path: cli.trace_file.clone(),
            filter: cli.trace_filter.clone(),
            capture_timing: cli.trace_timing,
        });
    } else if cli.dump_on_timeout {
        // Ring buffer mode for timeout dumps only
        engine.set_trace_config(otter_vm_core::TraceConfig {
            enabled: true,
            mode: otter_vm_core::TraceMode::RingBuffer,
            ring_buffer_size: cli.dump_buffer_size,
            output_path: cli.dump_file.clone(),
            filter: None,
            capture_timing: false,
        });
    }

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
    match engine.eval(line, None).await {
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
async fn run_tests(paths: &[PathBuf], filter: Option<&str>, watch: bool, cli: &Cli) -> Result<()> {
    if watch {
        run_tests_watch(paths, filter, cli).await
    } else {
        run_tests_once(paths, filter, cli).await
    }
}

async fn run_tests_once(paths: &[PathBuf], filter: Option<&str>, cli: &Cli) -> Result<()> {
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

async fn run_tests_watch(paths: &[PathBuf], filter: Option<&str>, cli: &Cli) -> Result<()> {
    use crate::watch::{FileWatcher, WatchConfig, WatchEvent};
    use std::io::Write;

    println!("Watch mode enabled. Watching for changes...\n");

    let watch_config = WatchConfig {
        debounce_ms: 200,
        extensions: vec![
            "ts".to_string(),
            "tsx".to_string(),
            "js".to_string(),
            "jsx".to_string(),
        ],
        ignore_dirs: vec![
            "node_modules".to_string(),
            ".git".to_string(),
            "dist".to_string(),
            "build".to_string(),
            ".otter".to_string(),
        ],
        clear_console: true,
    };

    let mut watcher = FileWatcher::new(watch_config);

    // Watch all provided paths
    for path in paths {
        if path.is_dir() {
            watcher.watch(path).map_err(|e| anyhow::anyhow!(e))?;
        } else if let Some(parent) = path.parent() {
            watcher.watch(parent).map_err(|e| anyhow::anyhow!(e))?;
        }
    }

    // Run tests initially
    let _ = run_tests_once(paths, filter, cli).await;

    println!("\n👀 Watching for changes... (Press Ctrl+C to exit)");

    // Watch loop - poll for events
    loop {
        // Check for events without blocking
        if let Some(event) = watcher.try_recv() {
            match event {
                WatchEvent::FilesChanged(changed_paths) => {
                    // Clear console
                    print!("\x1b[2J\x1b[1;1H");
                    std::io::stdout().flush().unwrap();

                    println!("🔄 Changes detected:");
                    for path in &changed_paths {
                        if let Some(name) = path.file_name() {
                            println!("  - {}", name.to_string_lossy());
                        }
                    }
                    println!();

                    // Re-run tests
                    let _ = run_tests_once(paths, filter, cli).await;

                    println!("\n👀 Watching for changes... (Press Ctrl+C to exit)");
                }
                WatchEvent::Error(err) => {
                    eprintln!("❌ Watch error: {}", err);
                }
            }
        }

        // Sleep to avoid busy-waiting
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    }
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

    let mut engine = {
        let source_url = path.to_string_lossy().to_string();
        let _resolve_guard = ResolveBaseDirGuard::from_source_url(&source_url)?;
        build_engine(cli, caps)
    };

    // Configure trace (either for full trace or timeout dumps)
    if cli.trace {
        // Full execution trace mode
        if cli.trace_file.is_none() {
            eprintln!("Error: --trace requires --trace-file to be specified");
            return Err(anyhow::anyhow!("--trace requires --trace-file"));
        }
        engine.set_trace_config(otter_vm_core::TraceConfig {
            enabled: true,
            mode: otter_vm_core::TraceMode::FullTrace,
            ring_buffer_size: cli.dump_buffer_size,
            output_path: cli.trace_file.clone(),
            filter: cli.trace_filter.clone(),
            capture_timing: cli.trace_timing,
        });
    } else if cli.dump_on_timeout {
        // Ring buffer mode for timeout dumps only
        engine.set_trace_config(otter_vm_core::TraceConfig {
            enabled: true,
            mode: otter_vm_core::TraceMode::RingBuffer,
            ring_buffer_size: cli.dump_buffer_size,
            output_path: cli.dump_file.clone(),
            filter: None,
            capture_timing: false,
        });
    }

    // Build filter pattern for test.run()
    let filter_opt = match filter {
        Some(f) => format!("{{ testNamePattern: \"{}\" }}", f.replace('\\', "\\\\").replace('"', "\\\"")),
        None => "{}".to_string(),
    };

    // Use absolute path for dynamic import
    let abs_test_path = std::fs::canonicalize(path)
        .unwrap_or_else(|_| std::env::current_dir().unwrap_or_default().join(path));
    let test_file_url = abs_test_path.to_string_lossy();

    // Test harness that uses node:test module
    let test_harness = format!(
        r#"import test from 'node:test';

// Make test API globally available for test files that don't import it
globalThis.test = test;
globalThis.describe = test.describe;
globalThis.it = test.it;
globalThis.before = test.before;
globalThis.after = test.after;
globalThis.beforeEach = test.beforeEach;
globalThis.afterEach = test.afterEach;

// Import and execute the test file (registers tests)
await import("{test_file_url}");

// Run collected tests and collect results via event stream
const __otter_results = {{ passed: 0, failed: 0, skipped: 0, todo: 0, failures: [] }};

const stream = test.run({filter_opt});

stream.on("test:pass", (event) => {{
    const name = event && event.data ? event.data.name : "<unknown>";
    __otter_results.passed++;
    console.log("    \u{{2713}} " + name);
}});

stream.on("test:fail", (event) => {{
    const name = event && event.data ? event.data.name : "<unknown>";
    const error = event && event.data ? event.data.error : "";
    __otter_results.failed++;
    __otter_results.failures.push({{ name, error }});
    console.log("    \u{{2717}} " + name);
    if (error) console.log("      " + error);
}});

stream.on("test:diagnostic", (event) => {{
    const name = event && event.data ? event.data.name : "";
    __otter_results.todo++;
}});

(() => __otter_results)();
"#,
        test_file_url = test_file_url.replace('\\', "/"),
        filter_opt = filter_opt,
    );

    // Use file:// URL as source_url for the wrapper so imports resolve correctly
    let wrapper_url = format!("file://{}", abs_test_path.parent()
        .unwrap_or(abs_test_path.as_path())
        .join("__test_harness__.js")
        .to_string_lossy()
        .replace('\\', "/"));

    // Run the test harness
    match engine.eval(&test_harness, Some(&wrapper_url)).await {
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
                let todo = obj
                    .get(&PropertyKey::string("todo"))
                    .and_then(|v| v.as_int32())
                    .unwrap_or(0) as usize;

                Ok((passed, failed, skipped + todo))
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

/// Dump debug information when execution times out
fn dump_timeout_info(engine: &otter_engine::Otter, cli: &Cli, source_path: Option<&str>) {
    use std::io::Write;

    let source_desc = source_path
        .map(|p| p.to_string())
        .unwrap_or_else(|| "<eval>".to_string());

    let header = format!(
        "\n═══════════════════════════════════════════════════════════════\n\
         TIMEOUT DETECTED: {}\n\
         Timeout: {} seconds\n\
         ═══════════════════════════════════════════════════════════════\n",
        source_desc, cli.timeout
    );

    if let Some(ref dump_file) = cli.dump_file {
        // Write to file
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dump_file)
        {
            let _ = write!(file, "{}", header);
            let _ = engine.dump_snapshot(&mut file);
            eprintln!("Debug snapshot written to: {}", dump_file.display());
        } else {
            eprintln!("Failed to open dump file: {:?}", dump_file);
            // Fallback to stderr
            eprint!("{}", header);
            let _ = engine.dump_snapshot(&mut std::io::stderr());
        }
    } else {
        // Write to stderr
        eprint!("{}", header);
        let _ = engine.dump_snapshot(&mut std::io::stderr());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn node_api_default_is_full() {
        let cli = Cli::try_parse_from(["otter", "repl"]).expect("cli parse");
        assert!(matches!(cli.node_api, NodeApiMode::Full));
    }

    #[test]
    fn node_api_safe_core_parses() {
        let cli =
            Cli::try_parse_from(["otter", "--node-api", "safe-core", "repl"]).expect("cli parse");
        assert!(matches!(cli.node_api, NodeApiMode::SafeCore));
    }
}
