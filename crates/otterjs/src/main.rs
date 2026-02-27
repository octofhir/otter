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

    /// Arguments to pass to script (when using direct file shorthand)
    #[arg(
        value_name = "ARGS",
        trailing_var_arg = true,
        allow_hyphen_values = true
    )]
    file_args: Vec<String>,

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

    /// Enable CPU sampling profiler (writes .cpuprofile and .folded outputs)
    #[arg(long, global = true)]
    cpu_prof: bool,

    /// CPU profiler sampling interval in microseconds (default: 1000)
    #[arg(long, global = true, default_value = "1000")]
    cpu_prof_interval: u64,

    /// CPU profile output base name (default: otter-<pid>-<ts>.cpuprofile)
    #[arg(long, global = true)]
    cpu_prof_name: Option<String>,

    /// CPU profile output directory (default: current working directory)
    #[arg(long, global = true)]
    cpu_prof_dir: Option<PathBuf>,

    /// Enable async/op trace export (Chrome Trace JSON)
    #[arg(long, global = true)]
    async_trace: bool,

    /// Async trace output file (default: otter-<pid>-<ts>.trace.json)
    #[arg(long, global = true)]
    async_trace_file: Option<PathBuf>,
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

        /// Arguments to pass to script
        #[arg(
            value_name = "ARGS",
            trailing_var_arg = true,
            allow_hyphen_values = true
        )]
        args: Vec<String>,
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
        return run_code(code, "<eval>", &[], &cli, true).await;
    }

    // Handle --eval flag (evaluate silently, only console.log produces output)
    if let Some(ref code) = cli.eval {
        return run_code(code, "<eval>", &[], &cli, false).await;
    }

    // Handle direct file argument (otter script.js)
    if let Some(ref file) = cli.file {
        return run_file(file, &cli.file_args, &cli).await;
    }

    match &cli.command {
        Some(Commands::Run { file, args }) => run_file(file, args, &cli).await,
        Some(Commands::Repl) => run_repl(&cli).await,
        Some(Commands::Test {
            paths,
            filter,
            watch,
        }) => run_tests(paths, filter.as_deref(), *watch, &cli).await,
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
async fn run_file(path: &PathBuf, script_args: &[String], cli: &Cli) -> Result<()> {
    let source = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read file: {}", path.display()))?;

    // Use absolute path as source_url so module resolution works correctly
    // regardless of CWD changes by ResolveBaseDirGuard.
    let abs_path = std::fs::canonicalize(path)
        .unwrap_or_else(|_| std::env::current_dir().unwrap_or_default().join(path));
    let source_url = abs_path.to_string_lossy();
    run_code(&source, &source_url, script_args, cli, false).await
}

struct ProcessArgvOverrideGuard;

impl ProcessArgvOverrideGuard {
    fn install(source_url: &str, script_args: &[String]) -> Self {
        let argv_override = build_process_argv_override(source_url, script_args);
        otter_engine::set_process_argv_override(argv_override);
        Self
    }
}

impl Drop for ProcessArgvOverrideGuard {
    fn drop(&mut self) {
        otter_engine::set_process_argv_override(None);
    }
}

fn build_process_argv_override(source_url: &str, script_args: &[String]) -> Option<Vec<String>> {
    if source_url == "<eval>" {
        return None;
    }

    let exec_path = std::env::current_exe()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| "otter".to_string());
    let script_path = source_url
        .strip_prefix("file://")
        .unwrap_or(source_url)
        .to_string();

    let mut argv = Vec::with_capacity(script_args.len() + 2);
    argv.push(exec_path);
    argv.push(script_path);
    argv.extend(script_args.iter().cloned());
    Some(argv)
}

#[cfg(feature = "jit")]
fn parse_env_truthy(value: &str) -> bool {
    !matches!(value.trim(), "" | "0")
        && !value.trim().eq_ignore_ascii_case("false")
        && !value.trim().eq_ignore_ascii_case("off")
        && !value.trim().eq_ignore_ascii_case("no")
}

#[cfg(feature = "jit")]
fn maybe_print_jit_stats() {
    let enabled = std::env::var("OTTER_JIT_STATS")
        .ok()
        .is_some_and(|v| parse_env_truthy(&v));
    if !enabled {
        return;
    }

    let stats = otter_vm_core::jit_runtime_stats();
    let hit_rate = if stats.execute_attempts > 0 {
        (stats.execute_hits as f64 * 100.0) / stats.execute_attempts as f64
    } else {
        0.0
    };

    eprintln!(
        "JIT stats: compile req={} ok={} err={} | exec attempts={} hits={} misses={} bailouts={} deopts={} hit_rate={:.2}% | compiled={}",
        stats.compile_requests,
        stats.compile_successes,
        stats.compile_errors,
        stats.execute_attempts,
        stats.execute_hits,
        stats.execute_not_compiled,
        stats.execute_bailouts,
        stats.deoptimizations,
        hit_rate,
        stats.compiled_functions
    );
}

#[cfg(not(feature = "jit"))]
fn maybe_print_jit_stats() {}

struct CpuSamplingSession {
    profiler: std::sync::Arc<otter_profiler::CpuProfiler>,
    stop_signal: std::sync::Arc<std::sync::atomic::AtomicBool>,
    sampler_handle: tokio::task::JoinHandle<()>,
    cpuprofile_path: PathBuf,
    folded_path: PathBuf,
}

struct CpuProfileArtifacts {
    cpuprofile_path: PathBuf,
    folded_path: PathBuf,
    sample_count: usize,
}

struct AsyncTraceArtifacts {
    trace_path: PathBuf,
    span_count: usize,
}

fn default_cpu_profile_name() -> String {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_else(|_| std::time::Duration::from_secs(0))
        .as_secs();
    format!("otter-{}-{}.cpuprofile", std::process::id(), ts)
}

fn default_async_trace_name() -> String {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_else(|_| std::time::Duration::from_secs(0))
        .as_secs();
    format!("otter-{}-{}.trace.json", std::process::id(), ts)
}

fn resolve_cpu_profile_paths(cli: &Cli) -> Result<(PathBuf, PathBuf)> {
    let dir = if let Some(dir) = &cli.cpu_prof_dir {
        dir.clone()
    } else {
        std::env::current_dir().context("Failed to get current directory for --cpu-prof-dir")?
    };
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("Failed to create profile output dir {}", dir.display()))?;

    let base_name = cli
        .cpu_prof_name
        .clone()
        .unwrap_or_else(default_cpu_profile_name);
    let cpuprofile_name = if base_name.ends_with(".cpuprofile") {
        base_name
    } else {
        format!("{}.cpuprofile", base_name)
    };

    let cpuprofile_path = dir.join(cpuprofile_name);
    let folded_path = cpuprofile_path.with_extension("folded");
    Ok((cpuprofile_path, folded_path))
}

fn resolve_async_trace_path(cli: &Cli) -> Result<PathBuf> {
    if let Some(path) = &cli.async_trace_file {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).with_context(|| {
                format!(
                    "Failed to create async trace output dir {}",
                    parent.display()
                )
            })?;
        }
        return Ok(path.clone());
    }

    let cwd =
        std::env::current_dir().context("Failed to get current directory for --async-trace")?;
    Ok(cwd.join(default_async_trace_name()))
}

fn finish_async_trace(
    engine: &otter_engine::Otter,
    cli: &Cli,
) -> Result<Option<AsyncTraceArtifacts>> {
    if !cli.async_trace {
        return Ok(None);
    }

    let trace_path = resolve_async_trace_path(cli)?;
    let trace_json = engine.async_trace_json().unwrap_or_else(|| {
        serde_json::json!({
            "otterAsyncTraceSchemaVersion": otter_profiler::ASYNC_TRACE_SCHEMA_VERSION,
            "displayTimeUnit": "ms",
            "traceEvents": []
        })
    });
    let span_count = trace_json["traceEvents"]
        .as_array()
        .map(|events| events.len())
        .unwrap_or(0);
    let bytes =
        serde_json::to_vec_pretty(&trace_json).context("Failed to serialize async trace JSON")?;
    std::fs::write(&trace_path, bytes)
        .with_context(|| format!("Failed to write async trace to {}", trace_path.display()))?;

    Ok(Some(AsyncTraceArtifacts {
        trace_path,
        span_count,
    }))
}

fn snapshot_to_profiler_frames(
    snapshot: &otter_engine::VmContextSnapshot,
) -> Vec<otter_profiler::StackFrame> {
    if !snapshot.profiler_stack.is_empty() {
        return snapshot.profiler_stack.clone();
    }

    fn normalized_function_name(name: Option<&String>) -> String {
        if let Some(name) = name {
            let trimmed = name.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
        "(anonymous)".to_string()
    }

    let mut frames = Vec::new();

    if !snapshot.call_stack.is_empty() {
        // Snapshot call_stack is top->bottom; convert to bottom->top.
        for frame in snapshot.call_stack.iter().rev() {
            frames.push(otter_profiler::StackFrame {
                function: normalized_function_name(frame.function_name.as_ref()),
                file: Some(frame.module_url.clone()),
                line: None,
                column: None,
            });
        }
        return frames;
    }

    if let Some(frame) = &snapshot.current_frame {
        frames.push(otter_profiler::StackFrame {
            function: normalized_function_name(frame.function_name.as_ref()),
            file: Some(frame.module_url.clone()),
            line: None,
            column: None,
        });
    }

    frames
}

fn start_cpu_sampling(
    engine: &otter_engine::Otter,
    cli: &Cli,
) -> Result<Option<CpuSamplingSession>> {
    if !cli.cpu_prof {
        return Ok(None);
    }

    let (cpuprofile_path, folded_path) = resolve_cpu_profile_paths(cli)?;
    let interval = std::time::Duration::from_micros(cli.cpu_prof_interval.max(100));

    let profiler = std::sync::Arc::new(otter_profiler::CpuProfiler::with_interval(interval));
    profiler.start();

    let stop_signal = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let snapshot_handle = engine.debug_snapshot_handle();
    let sampler_profiler = std::sync::Arc::clone(&profiler);
    let sampler_stop = std::sync::Arc::clone(&stop_signal);

    let sampler_handle = tokio::spawn(async move {
        while !sampler_stop.load(std::sync::atomic::Ordering::Relaxed) {
            let snapshot = snapshot_handle.lock().clone();
            let frames = snapshot_to_profiler_frames(&snapshot);
            if !frames.is_empty() {
                sampler_profiler.record_sample(frames);
            }
            tokio::time::sleep(interval).await;
        }
    });

    Ok(Some(CpuSamplingSession {
        profiler,
        stop_signal,
        sampler_handle,
        cpuprofile_path,
        folded_path,
    }))
}

fn finish_cpu_sampling(session: CpuSamplingSession) -> Result<CpuProfileArtifacts> {
    session
        .stop_signal
        .store(true, std::sync::atomic::Ordering::Relaxed);
    session.sampler_handle.abort();

    let profile = session.profiler.stop();
    let cpuprofile = profile.to_cpuprofile();
    let folded = profile.to_folded();
    let sample_count = profile.sample_count;

    let cpuprofile_bytes =
        serde_json::to_vec_pretty(&cpuprofile).context("Failed to serialize cpuprofile JSON")?;
    std::fs::write(&session.cpuprofile_path, cpuprofile_bytes).with_context(|| {
        format!(
            "Failed to write cpuprofile to {}",
            session.cpuprofile_path.display()
        )
    })?;
    std::fs::write(&session.folded_path, folded).with_context(|| {
        format!(
            "Failed to write folded stacks to {}",
            session.folded_path.display()
        )
    })?;

    Ok(CpuProfileArtifacts {
        cpuprofile_path: session.cpuprofile_path,
        folded_path: session.folded_path,
        sample_count,
    })
}

/// Run JavaScript code using EngineBuilder
async fn run_code(
    source: &str,
    source_url: &str,
    script_args: &[String],
    cli: &Cli,
    print_result: bool,
) -> Result<()> {
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

    let _argv_override_guard = ProcessArgvOverrideGuard::install(source_url, script_args);
    let caps = build_capabilities(cli);

    // Create engine with builtins (EngineBuilder handles all setup)
    let mut engine = {
        let _resolve_guard = ResolveBaseDirGuard::from_source_url(source_url)?;
        build_engine(cli, caps)
    };
    engine.set_async_trace_enabled(cli.async_trace);

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

    let cpu_sampling = start_cpu_sampling(&engine, cli)?;

    // Execute code
    let result = engine.eval(source, Some(source_url)).await;

    // Cancel timeout task if still running
    if let Some(handle) = timeout_handle {
        handle.abort();
    }

    let cpu_profile_artifacts = if let Some(session) = cpu_sampling {
        Some(finish_cpu_sampling(session)?)
    } else {
        None
    };
    let async_trace_artifacts = finish_async_trace(&engine, cli)?;
    maybe_print_jit_stats();

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

            if let Some(artifacts) = cpu_profile_artifacts {
                eprintln!(
                    "CPU profile written: {} (samples={})",
                    artifacts.cpuprofile_path.display(),
                    artifacts.sample_count
                );
                eprintln!("Folded stacks written: {}", artifacts.folded_path.display());
            }
            if let Some(artifacts) = async_trace_artifacts {
                eprintln!(
                    "Async trace written: {} (spans={})",
                    artifacts.trace_path.display(),
                    artifacts.span_count
                );
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
                if let Some(artifacts) = cpu_profile_artifacts {
                    eprintln!(
                        "CPU profile written: {} (samples={})",
                        artifacts.cpuprofile_path.display(),
                        artifacts.sample_count
                    );
                    eprintln!("Folded stacks written: {}", artifacts.folded_path.display());
                }
                if let Some(artifacts) = async_trace_artifacts {
                    eprintln!(
                        "Async trace written: {} (spans={})",
                        artifacts.trace_path.display(),
                        artifacts.span_count
                    );
                }
                Err(anyhow::anyhow!(
                    "Execution timed out after {} seconds",
                    cli.timeout
                ))
            } else {
                if let Some(artifacts) = cpu_profile_artifacts {
                    eprintln!(
                        "CPU profile written: {} (samples={})",
                        artifacts.cpuprofile_path.display(),
                        artifacts.sample_count
                    );
                    eprintln!("Folded stacks written: {}", artifacts.folded_path.display());
                }
                if let Some(artifacts) = async_trace_artifacts {
                    eprintln!(
                        "Async trace written: {} (spans={})",
                        artifacts.trace_path.display(),
                        artifacts.span_count
                    );
                }
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

    let _source = std::fs::read_to_string(path)?;
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
        Some(f) => format!(
            "{{ testNamePattern: \"{}\" }}",
            f.replace('\\', "\\\\").replace('"', "\\\"")
        ),
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
    let wrapper_url = format!(
        "file://{}",
        abs_test_path
            .parent()
            .unwrap_or(abs_test_path.as_path())
            .join("__test_harness__.js")
            .to_string_lossy()
            .replace('\\', "/")
    );

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

    #[test]
    fn cpu_prof_flags_parse() {
        let cli = Cli::try_parse_from([
            "otter",
            "--cpu-prof",
            "--cpu-prof-interval",
            "500",
            "--cpu-prof-name",
            "bench.cpuprofile",
            "--cpu-prof-dir",
            "/tmp/otter-prof",
            "repl",
        ])
        .expect("cli parse");

        assert!(cli.cpu_prof);
        assert_eq!(cli.cpu_prof_interval, 500);
        assert_eq!(cli.cpu_prof_name.as_deref(), Some("bench.cpuprofile"));
        assert_eq!(
            cli.cpu_prof_dir
                .as_ref()
                .map(|p| p.to_string_lossy().to_string()),
            Some("/tmp/otter-prof".to_string())
        );
    }

    #[test]
    fn cpu_prof_is_off_by_default() {
        let cli = Cli::try_parse_from(["otter", "repl"]).expect("cli parse");
        assert!(!cli.cpu_prof, "cpu profiler must be opt-in");
    }

    #[test]
    fn async_trace_flags_parse() {
        let cli = Cli::try_parse_from([
            "otter",
            "--async-trace",
            "--async-trace-file",
            "/tmp/otter-async-trace.json",
            "repl",
        ])
        .expect("cli parse");

        assert!(cli.async_trace);
        assert_eq!(
            cli.async_trace_file
                .as_ref()
                .map(|p| p.to_string_lossy().to_string()),
            Some("/tmp/otter-async-trace.json".to_string())
        );
    }

    #[test]
    fn resolve_cpu_profile_paths_adds_extensions() {
        let temp_dir = std::env::temp_dir().join("otter-profiler-path-test");
        let cli = Cli::try_parse_from([
            "otter",
            "--cpu-prof",
            "--cpu-prof-name",
            "run",
            "--cpu-prof-dir",
            temp_dir.to_string_lossy().as_ref(),
            "repl",
        ])
        .expect("cli parse");

        let (cpuprofile, folded) = resolve_cpu_profile_paths(&cli).expect("paths");
        assert!(cpuprofile.ends_with("run.cpuprofile"));
        assert!(folded.ends_with("run.folded"));
    }

    #[test]
    fn run_subcommand_parses_script_args() {
        let cli = Cli::try_parse_from([
            "otter",
            "run",
            "benchmarks/cpu/flamegraph.ts",
            "math",
            "2",
            "--flag-like",
        ])
        .expect("cli parse");

        match cli.command {
            Some(Commands::Run { file, args }) => {
                assert_eq!(file, PathBuf::from("benchmarks/cpu/flamegraph.ts"));
                assert_eq!(args, vec!["math", "2", "--flag-like"]);
            }
            _ => panic!("expected run command"),
        }
    }

    #[test]
    fn shorthand_file_parses_script_args() {
        let cli = Cli::try_parse_from([
            "otter",
            "benchmarks/cpu/flamegraph.ts",
            "json",
            "3",
            "--flag-like",
        ])
        .expect("cli parse");

        assert_eq!(
            cli.file,
            Some(PathBuf::from("benchmarks/cpu/flamegraph.ts"))
        );
        assert_eq!(cli.file_args, vec!["json", "3", "--flag-like"]);
    }

    #[test]
    fn build_process_argv_override_includes_exec_and_script() {
        let args = vec!["phase".to_string(), "2".to_string()];
        let argv = build_process_argv_override("/tmp/workload.ts", &args).expect("argv");
        assert!(argv.len() >= 4);
        assert_eq!(argv[1], "/tmp/workload.ts");
        assert_eq!(argv[2], "phase");
        assert_eq!(argv[3], "2");
    }

    #[test]
    fn snapshot_to_profiler_frames_prefers_vm_profiler_stack() {
        let mut snapshot = otter_engine::VmContextSnapshot::default();
        snapshot.profiler_stack.push(otter_profiler::StackFrame {
            function: "vm_fn".to_string(),
            file: Some("vm.ts".to_string()),
            line: Some(7),
            column: Some(3),
        });

        let frames = snapshot_to_profiler_frames(&snapshot);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].function, "vm_fn");
        assert_eq!(frames[0].file.as_deref(), Some("vm.ts"));
        assert_eq!(frames[0].line, Some(7));
        assert_eq!(frames[0].column, Some(3));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn cpu_prof_e2e_generates_devtools_compatible_artifacts() {
        let unique = format!(
            "otter-cpu-prof-e2e-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        );
        let temp_dir = std::env::temp_dir().join(unique);
        std::fs::create_dir_all(&temp_dir).expect("create temp dir");

        let script_path = temp_dir.join("cpu-prof-e2e.ts");
        std::fs::write(
            &script_path,
            r#"
            const start = Date.now();
            let spin = 0;
            while (Date.now() - start < 30) {
                spin++;
            }
            console.log(spin);
            "#,
        )
        .expect("write script");

        let temp_dir_arg = temp_dir.to_string_lossy().to_string();
        let script_arg = script_path.to_string_lossy().to_string();
        let cli = Cli::try_parse_from([
            "otter",
            "--cpu-prof",
            "--cpu-prof-interval",
            "200",
            "--cpu-prof-name",
            "e2e.cpuprofile",
            "--cpu-prof-dir",
            temp_dir_arg.as_str(),
            "run",
            script_arg.as_str(),
        ])
        .expect("cli parse");

        let script_args: Vec<String> = Vec::new();
        run_file(&script_path, &script_args, &cli)
            .await
            .expect("run file with cpu profiler");

        let cpuprofile_path = temp_dir.join("e2e.cpuprofile");
        let folded_path = temp_dir.join("e2e.folded");
        assert!(cpuprofile_path.exists(), "missing cpuprofile output");
        assert!(folded_path.exists(), "missing folded output");

        let cpuprofile_raw = std::fs::read(&cpuprofile_path).expect("read cpuprofile");
        let cpuprofile: serde_json::Value =
            serde_json::from_slice(&cpuprofile_raw).expect("parse cpuprofile JSON");

        assert!(cpuprofile["nodes"].is_array());
        assert!(cpuprofile["samples"].is_array());
        assert!(cpuprofile["timeDeltas"].is_array());
        assert!(cpuprofile["startTime"].is_number());
        assert!(cpuprofile["endTime"].is_number());

        let samples = cpuprofile["samples"].as_array().expect("samples array");
        let deltas = cpuprofile["timeDeltas"]
            .as_array()
            .expect("timeDeltas array");
        assert_eq!(samples.len(), deltas.len());
        assert!(
            !samples.is_empty(),
            "expected profiler to capture at least one sample"
        );

        let first_node = cpuprofile["nodes"]
            .as_array()
            .and_then(|nodes| nodes.first())
            .expect("cpuprofile node");
        let call_frame = first_node["callFrame"].as_object().expect("callFrame");
        for key in [
            "functionName",
            "scriptId",
            "url",
            "lineNumber",
            "columnNumber",
        ] {
            assert!(call_frame.contains_key(key), "missing callFrame key: {key}");
        }

        let folded = std::fs::read_to_string(&folded_path).expect("read folded");
        assert!(
            !folded.trim().is_empty(),
            "folded output should not be empty"
        );
        for line in folded.lines().filter(|line| !line.trim().is_empty()) {
            let mut parts = line.rsplitn(2, ' ');
            let count = parts.next().expect("count");
            let stack = parts.next().expect("stack");
            assert!(
                count.parse::<u64>().is_ok(),
                "invalid folded count in line: {line}"
            );
            assert!(
                !stack.trim().is_empty(),
                "empty folded stack in line: {line}"
            );
        }

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn cpu_prof_off_by_default_emits_no_profile_artifacts() {
        let unique = format!(
            "otter-cpu-prof-default-off-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        );
        let temp_dir = std::env::temp_dir().join(unique);
        std::fs::create_dir_all(&temp_dir).expect("create temp dir");

        let script_path = temp_dir.join("cpu-prof-default-off.ts");
        std::fs::write(
            &script_path,
            r#"
            let x = 0;
            for (let i = 0; i < 1000; i++) x += i;
            console.log(x);
            "#,
        )
        .expect("write script");

        let script_arg = script_path.to_string_lossy().to_string();
        let cli = Cli::try_parse_from(["otter", "run", script_arg.as_str()]).expect("cli parse");
        assert!(!cli.cpu_prof, "cpu profiler should be disabled by default");

        let script_args: Vec<String> = Vec::new();
        run_file(&script_path, &script_args, &cli)
            .await
            .expect("run file without cpu profiler");

        let mut generated_profiles = Vec::new();
        for entry in std::fs::read_dir(&temp_dir).expect("read temp dir entries") {
            let entry = entry.expect("dir entry");
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) == Some("cpuprofile")
                || path.extension().and_then(|ext| ext.to_str()) == Some("folded")
            {
                generated_profiles.push(path);
            }
        }

        assert!(
            generated_profiles.is_empty(),
            "expected no profile artifacts when --cpu-prof is not set, found: {:?}",
            generated_profiles
        );

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn async_trace_e2e_generates_chrome_trace_json() {
        let unique = format!(
            "otter-async-trace-e2e-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        );
        let temp_dir = std::env::temp_dir().join(unique);
        std::fs::create_dir_all(&temp_dir).expect("create temp dir");

        let script_path = temp_dir.join("async-trace-e2e.ts");
        std::fs::write(
            &script_path,
            r#"
            queueMicrotask(() => {});
            await new Promise((resolve) => setTimeout(resolve, 1));
            "#,
        )
        .expect("write script");

        let trace_path = temp_dir.join("async.trace.json");
        let script_arg = script_path.to_string_lossy().to_string();
        let trace_arg = trace_path.to_string_lossy().to_string();

        let cli = Cli::try_parse_from([
            "otter",
            "--async-trace",
            "--async-trace-file",
            trace_arg.as_str(),
            "run",
            script_arg.as_str(),
        ])
        .expect("cli parse");

        let script_args: Vec<String> = Vec::new();
        run_file(&script_path, &script_args, &cli)
            .await
            .expect("run file with async trace");

        assert!(trace_path.exists(), "missing async trace output");
        let trace_raw = std::fs::read(&trace_path).expect("read trace");
        let trace: serde_json::Value =
            serde_json::from_slice(&trace_raw).expect("parse async trace JSON");
        assert_eq!(
            trace["otterAsyncTraceSchemaVersion"],
            serde_json::json!(otter_profiler::ASYNC_TRACE_SCHEMA_VERSION)
        );
        let events = trace["traceEvents"].as_array().expect("traceEvents array");
        assert!(
            !events.is_empty(),
            "expected async trace to contain at least one span"
        );
        assert!(
            events
                .iter()
                .any(|event| event["name"] == "setTimeout" || event["name"] == "queueMicrotask"),
            "expected timers/jobs spans in async trace"
        );
        assert!(
            events
                .iter()
                .any(|event| event["cat"] == "timers" || event["cat"] == "jobs"),
            "expected timers/jobs categories in async trace"
        );

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn trace_e2e_generates_chrome_perfetto_compatible_json() {
        let unique = format!(
            "otter-trace-e2e-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        );
        let temp_dir = std::env::temp_dir().join(unique);
        std::fs::create_dir_all(&temp_dir).expect("create temp dir");

        let script_path = temp_dir.join("trace-e2e.ts");
        std::fs::write(
            &script_path,
            r#"
            let acc = 0;
            for (let i = 0; i < 64; i++) {
                acc += i;
            }
            console.log(acc);
            "#,
        )
        .expect("write script");

        let trace_path = temp_dir.join("vm.trace.json");
        let script_arg = script_path.to_string_lossy().to_string();
        let trace_arg = trace_path.to_string_lossy().to_string();
        let cli = Cli::try_parse_from([
            "otter",
            "--trace",
            "--trace-file",
            trace_arg.as_str(),
            "run",
            script_arg.as_str(),
        ])
        .expect("cli parse");

        let script_args: Vec<String> = Vec::new();
        run_file(&script_path, &script_args, &cli)
            .await
            .expect("run file with trace");

        assert!(trace_path.exists(), "missing trace output");
        let trace_raw = std::fs::read(&trace_path).expect("read trace");
        let trace: serde_json::Value =
            serde_json::from_slice(&trace_raw).expect("parse trace JSON");
        assert_eq!(
            trace["otterTraceSchemaVersion"],
            serde_json::json!(otter_vm_core::trace::TRACE_EVENT_SCHEMA_VERSION)
        );

        let events = trace["traceEvents"].as_array().expect("traceEvents array");
        assert!(!events.is_empty(), "expected trace events");
        for event in events {
            assert!(event["name"].is_string(), "event name must be string");
            assert_eq!(event["cat"], "vm.instruction");
            assert_eq!(event["ph"], "X");
            assert!(event["ts"].is_number(), "event ts must be numeric");
            assert!(event["dur"].is_number(), "event dur must be numeric");
            assert!(event["pid"].is_number(), "event pid must be numeric");
            assert!(event["tid"].is_number(), "event tid must be numeric");

            let args = event["args"].as_object().expect("event args object");
            for key in [
                "module",
                "function",
                "pc",
                "function_index",
                "operands",
                "modified_registers",
            ] {
                assert!(args.contains_key(key), "missing trace args key: {key}");
            }
        }

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn timeout_dump_is_reproducible_for_immediate_interrupt() {
        fn extract_instruction_opcodes(dump: &str) -> Vec<String> {
            dump.lines()
                .filter_map(|line| {
                    let trimmed = line.trim_start();
                    let (prefix, suffix) = trimmed.split_once(':')?;
                    if !prefix
                        .chars()
                        .all(|ch| ch.is_ascii_digit() || ch.is_ascii_whitespace())
                    {
                        return None;
                    }
                    let opcode = suffix.split_whitespace().next()?;
                    Some(opcode.to_string())
                })
                .collect()
        }

        async fn write_timeout_dump(
            cli: &Cli,
            source: &str,
            source_url: &str,
        ) -> (String, std::path::PathBuf) {
            let caps = build_capabilities(cli);
            let mut engine = build_engine(cli, caps);
            engine.set_trace_config(otter_vm_core::TraceConfig {
                enabled: true,
                mode: otter_vm_core::TraceMode::RingBuffer,
                ring_buffer_size: cli.dump_buffer_size,
                output_path: cli.dump_file.clone(),
                filter: None,
                capture_timing: false,
            });

            let interrupt_flag = engine.interrupt_flag();
            let interrupt_task = tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                interrupt_flag.store(true, std::sync::atomic::Ordering::Relaxed);
            });
            let err = engine
                .eval(source, Some(source_url))
                .await
                .expect_err("execution should be interrupted");
            interrupt_task.abort();
            assert!(
                err.to_string().to_lowercase().contains("interrupt"),
                "expected interrupted error, got: {err}"
            );

            dump_timeout_info(&engine, cli, Some(source_url));
            let dump_path = cli.dump_file.clone().expect("dump file path");
            let dump = std::fs::read_to_string(&dump_path).expect("read timeout dump");
            (dump, dump_path)
        }

        let unique = format!(
            "otter-timeout-repro-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        );
        let temp_dir = std::env::temp_dir().join(unique);
        std::fs::create_dir_all(&temp_dir).expect("create temp dir");

        let source = "let x = 0; while (true) { x++; }";
        let source_url = "timeout-repro.js";
        let dump_one_path = temp_dir.join("timeout-one.txt");
        let dump_two_path = temp_dir.join("timeout-two.txt");
        let dump_one_arg = dump_one_path.to_string_lossy().to_string();
        let dump_two_arg = dump_two_path.to_string_lossy().to_string();

        let cli_one = Cli::try_parse_from([
            "otter",
            "--timeout",
            "1",
            "--dump-on-timeout",
            "--dump-file",
            dump_one_arg.as_str(),
            "repl",
        ])
        .expect("cli one parse");
        let cli_two = Cli::try_parse_from([
            "otter",
            "--timeout",
            "1",
            "--dump-on-timeout",
            "--dump-file",
            dump_two_arg.as_str(),
            "repl",
        ])
        .expect("cli two parse");

        let (dump_one, dump_one_path) = write_timeout_dump(&cli_one, source, source_url).await;
        let (dump_two, dump_two_path) = write_timeout_dump(&cli_two, source, source_url).await;

        assert!(
            dump_one.contains("Recent Instructions"),
            "timeout dump should include trace section"
        );
        let opcodes_one = extract_instruction_opcodes(&dump_one);
        let opcodes_two = extract_instruction_opcodes(&dump_two);
        assert!(
            !opcodes_one.is_empty(),
            "expected timeout dump to include instruction opcode lines"
        );
        assert_eq!(
            opcodes_one, opcodes_two,
            "timeout dump opcode sequence should be reproducible across identical runs"
        );

        let _ = std::fs::remove_file(dump_one_path);
        let _ = std::fs::remove_file(dump_two_path);
        let _ = std::fs::remove_dir_all(&temp_dir);
    }
}
