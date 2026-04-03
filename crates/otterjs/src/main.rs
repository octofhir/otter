//! Otter CLI - A fast TypeScript/JavaScript runtime.
//!
//! New-VM-only CLI surface during the legacy stack freeze.

#![allow(dead_code)]

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::time::Duration;
use tracing_subscriber::filter::EnvFilter;

mod commands;
#[allow(dead_code)]
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

    /// Allow FFI (foreign function interface) access
    #[arg(long = "allow-ffi", global = true)]
    allow_ffi: bool,

    /// Node.js API profile (`none`, `safe-core`, `full`)
    #[arg(long = "node-api", value_enum, global = true, default_value_t = NodeApiMode::Full)]
    node_api: NodeApiMode,

    /// Execution timeout in seconds (0 = no timeout)
    #[arg(long, global = true, default_value = "30")]
    timeout: u64,

    /// Show profiling information (memory usage)
    #[arg(long, global = true)]
    profile: bool,

    /// Dump debug snapshot on timeout (default: false)
    #[arg(long, global = true)]
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

    /// Dump IC hit/miss statistics on exit (top 20 by miss count)
    #[arg(long, global = true)]
    trace_ic: bool,

    /// Dump GC pause histogram on exit
    #[arg(long, global = true)]
    gc_stats: bool,

    /// Dump allocation category counts on exit
    #[arg(long, global = true)]
    alloc_stats: bool,
}

#[derive(Clone, Copy, Debug, clap::ValueEnum)]
enum NodeApiMode {
    None,
    SafeCore,
    Full,
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
    /// Temporarily unavailable during the new VM migration
    Repl,
    /// Temporarily unavailable during the new VM migration
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

    if let Some(ref code) = cli.print {
        return run_eval_new_vm(code, true);
    }

    if let Some(ref code) = cli.eval {
        return run_eval_new_vm(code, false);
    }

    if let Some(ref file) = cli.file {
        return run_file(file, &cli.file_args, &cli).await;
    }

    match &cli.command {
        Some(Commands::Run { file, args }) => run_file(file, args, &cli).await,
        Some(Commands::Repl) => command_temporarily_disabled("repl"),
        Some(Commands::Test { .. }) => command_temporarily_disabled("test"),
        Some(Commands::Check { files }) => {
            println!("Type checking: {:?}", files);
            println!("Note: Type checking integration (tsgo) is being ported to the new VM.");
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

fn command_temporarily_disabled(command: &str) -> Result<()> {
    Err(anyhow::anyhow!(
        "`otter {command}` is temporarily disabled during the new VM migration"
    ))
}

async fn run_file(path: &PathBuf, _script_args: &[String], cli: &Cli) -> Result<()> {
    let path_str = path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("Invalid file path: {}", path.display()))?;
    let mut rt = build_runtime_for_cli(cli)?;
    rt.run_entry_specifier(path_str, None)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(())
}

fn run_eval_new_vm(source: &str, print_result: bool) -> Result<()> {
    let mut rt = otter_runtime::OtterRuntime::builder()
        .extension(otter_modules::modules_extension())
        .build();
    match rt.eval(source) {
        Ok(result) => {
            if print_result {
                let formatted =
                    otter_runtime::console::format_value(result.return_value(), rt.state());
                println!("{formatted}");
            }
            Ok(())
        }
        Err(e) => Err(anyhow::anyhow!("{e}")),
    }
}

fn build_runtime_for_cli(cli: &Cli) -> Result<otter_runtime::OtterRuntime> {
    let mut loader = otter_runtime::ModuleLoaderConfig::default();
    loader.base_dir = std::env::current_dir()?;

    let profile = match cli.node_api {
        NodeApiMode::None => otter_runtime::RuntimeProfile::Core,
        NodeApiMode::SafeCore => otter_runtime::RuntimeProfile::SafeCore,
        NodeApiMode::Full => otter_runtime::RuntimeProfile::Full,
    };

    let mut builder = otter_runtime::OtterRuntime::builder()
        .profile(profile)
        .module_loader(loader)
        .extension(otter_modules::modules_extension());

    if cli.timeout > 0 {
        builder = builder.timeout(Duration::from_secs(cli.timeout));
    }

    Ok(builder.build())
}
