//! Otter CLI - A fast TypeScript/JavaScript runtime.
//!
//! New-VM-only CLI surface during the legacy stack freeze.

#![allow(dead_code)]

use anyhow::Result;
use clap::{Parser, Subcommand};
use miette::{GraphicalReportHandler, GraphicalTheme, Report};
use otter_runtime::RunError;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
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

    /// Execution timeout in seconds (0 = no timeout)
    #[arg(long, global = true, default_value = "30")]
    timeout: u64,

    /// Maximum old-space size in MB. Drop-in compatible with Node.js's
    /// `--max-old-space-size` — Otter accepts both the dashed and
    /// underscored forms (Node V8-flag convention). When exceeded, Otter
    /// throws a catchable `RangeError` instead of letting the OS kill
    /// the process. Default `2048` (2 GB) matches modern Node.js; pass
    /// `0` to disable.
    #[arg(
        long = "max-old-space-size",
        visible_alias = "max_old_space_size",
        global = true,
        default_value = "2048",
        value_name = "MB"
    )]
    max_old_space_size: u64,

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

    // ---- JIT introspection flags ----
    /// Dump compiled bytecodes before JIT compilation
    #[arg(long, global = true)]
    dump_bytecode: bool,

    /// Dump MIR (middle IR) before codegen
    #[arg(long, global = true)]
    dump_mir: bool,

    /// Dump Cranelift IR (CLIF) before native compilation
    #[arg(long, global = true)]
    dump_clif: bool,

    /// Dump native code hex after JIT compilation
    #[arg(long, global = true)]
    dump_asm: bool,

    /// Dump JIT telemetry on exit (compile times, bailout counts)
    #[arg(long, global = true)]
    dump_jit_stats: bool,
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
async fn main() -> ExitCode {
    let init_result = (|| -> Result<()> {
        tracing_subscriber::fmt()
            .with_env_filter(EnvFilter::from_default_env().add_directive("warn".parse()?))
            .init();
        Ok(())
    })();
    if let Err(err) = init_result {
        eprintln!("error: {err}");
        return ExitCode::from(1);
    }

    let cli = Cli::parse();

    if let Some(ref code) = cli.print {
        return run_eval(code, true, &cli);
    }

    if let Some(ref code) = cli.eval {
        return run_eval(code, false, &cli);
    }

    if let Some(ref file) = cli.file {
        return run_file(file, &cli.file_args, &cli).await;
    }

    let result: Result<()> = match &cli.command {
        Some(Commands::Run { file, args }) => return run_file(file, args, &cli).await,
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
        Some(Commands::Init) => commands::init::run().map(|_| ()),
        Some(Commands::Info { json }) => {
            commands::info::run(*json);
            Ok(())
        }
        None => {
            use clap::CommandFactory;
            match Cli::command().print_help() {
                Ok(_) => {
                    println!();
                    Ok(())
                }
                Err(error) => Err(anyhow::anyhow!("{error}")),
            }
        }
    };
    match result {
        Ok(()) => ExitCode::from(0),
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::from(1)
        }
    }
}

fn command_temporarily_disabled(command: &str) -> Result<()> {
    Err(anyhow::anyhow!(
        "`otter {command}` is temporarily disabled during the new VM migration"
    ))
}

/// Picks the right miette `GraphicalTheme` based on `NO_COLOR` and whether
/// stderr is a TTY. Mirrors what miette's own auto-detection does, but
/// applied explicitly so we don't have to rely on the `Debug` impl picking
/// up an installed hook.
///
/// Honors the [NO_COLOR](https://no-color.org/) convention so CI logs and
/// piped output stay plain.
fn pick_graphical_theme() -> GraphicalTheme {
    let no_color = std::env::var_os("NO_COLOR").is_some();
    let stderr_is_tty = atty::is(atty::Stream::Stderr);
    if no_color || !stderr_is_tty {
        GraphicalTheme::unicode_nocolor()
    } else {
        GraphicalTheme::unicode()
    }
}

/// Renders a `RunError` to stderr and returns the appropriate exit code.
///
/// `JsThrow` is the rich path: we build a `miette::Report`, render it with
/// an explicit `GraphicalReportHandler` so colors and box-drawing actually
/// show up (Display fallback is plain text). `Compile` and `Runtime` keep
/// the legacy plain `error: ...` shape so existing scripts that grep stderr
/// still work.
fn report_run_error(err: RunError) -> ExitCode {
    match err {
        RunError::JsThrow(diagnostic) => render_miette(Report::new(*diagnostic)),
        RunError::Compile(diagnostic) => render_miette(Report::new(*diagnostic)),
        RunError::Runtime(message) => {
            eprintln!("error: {message}");
            ExitCode::from(1)
        }
    }
}

/// D6: render any miette `Report` through the graphical handler
/// (source frame + coloured caret) with a plain-text fallback so
/// pipes and `NO_COLOR=1` still emit something useful.
fn render_miette(report: Report) -> ExitCode {
    let theme = pick_graphical_theme();
    let width = std::env::var("COLUMNS")
        .ok()
        .and_then(|w| w.parse::<usize>().ok())
        .unwrap_or(120);
    let handler = GraphicalReportHandler::new_themed(theme).with_width(width);
    let mut rendered = String::new();
    if handler
        .render_report(&mut rendered, report.as_ref())
        .is_err()
    {
        rendered = format!("error: {report}");
    }
    eprintln!("{rendered}");
    ExitCode::from(1)
}

async fn run_file(path: &Path, script_args: &[String], cli: &Cli) -> ExitCode {
    let Some(path_str) = path.to_str() else {
        eprintln!("error: invalid file path: {}", path.display());
        return ExitCode::from(1);
    };
    let argv = std::iter::once(std::env::current_exe().unwrap_or_else(|_| PathBuf::from("otter")))
        .map(|path| path.to_string_lossy().to_string())
        .chain(std::iter::once(
            path.canonicalize()
                .unwrap_or_else(|_| path.to_path_buf())
                .to_string_lossy()
                .to_string(),
        ))
        .chain(script_args.iter().cloned())
        .collect::<Vec<_>>();
    let mut rt = match build_runtime_for_cli(cli, argv) {
        Ok(rt) => rt,
        Err(error) => {
            eprintln!("error: {error}");
            return ExitCode::from(1);
        }
    };
    match rt.run_entry_specifier(path_str, None) {
        Ok(_) => ExitCode::from(0),
        Err(error) => report_run_error(error),
    }
}

fn run_eval(source: &str, print_result: bool, cli: &Cli) -> ExitCode {
    let argv = vec![
        std::env::current_exe()
            .unwrap_or_else(|_| PathBuf::from("otter"))
            .to_string_lossy()
            .to_string(),
        "[eval]".to_string(),
    ];
    let mut rt = match build_runtime_for_cli(cli, argv) {
        Ok(rt) => rt,
        Err(error) => {
            eprintln!("error: {error}");
            return ExitCode::from(1);
        }
    };
    match rt.eval(source) {
        Ok(result) => {
            if print_result {
                let formatted =
                    otter_runtime::console::format_value(result.return_value(), rt.state());
                println!("{formatted}");
            }
            ExitCode::from(0)
        }
        Err(error) => report_run_error(error),
    }
}

fn build_runtime_for_cli(cli: &Cli, argv: Vec<String>) -> Result<otter_runtime::OtterRuntime> {
    let capabilities = if cli.allow_all {
        otter_runtime::Capabilities::all()
    } else {
        let mut builder = otter_runtime::CapabilitiesBuilder::new();
        if cli.allow_read {
            builder = builder.allow_read_all();
        }
        if cli.allow_write {
            builder = builder.allow_write_all();
        }
        if cli.allow_net {
            builder = builder.allow_net_all();
        }
        if cli.allow_env {
            builder = builder.allow_env_all();
        }
        if cli.allow_run {
            builder = builder.allow_subprocess();
        }
        if cli.allow_ffi {
            builder = builder.allow_ffi();
        }
        builder.build()
    };

    let env_store = if cli.allow_all || cli.allow_env {
        Some(std::env::vars().fold(
            otter_runtime::EnvStoreBuilder::new(),
            |builder, (key, _)| builder.passthrough_var(key),
        ))
    } else {
        None
    };

    let loader = otter_runtime::ModuleLoaderConfig {
        base_dir: std::env::current_dir()?,
        ..Default::default()
    };

    let mut builder = otter_runtime::OtterRuntime::builder()
        .profile(otter_runtime::RuntimeProfile::Full)
        .capabilities(capabilities)
        .process_argv(argv)
        .module_loader(loader)
        .extension(otter_nodejs::nodejs_extension())
        .extension(otter_modules::modules_extension())
        .extension(otter_web::web_extension());

    if let Some(env_store) = env_store {
        builder = builder.env(|_| env_store);
    }

    if cli.timeout > 0 {
        builder = builder.timeout(Duration::from_secs(cli.timeout));
    }

    if cli.max_old_space_size > 0 {
        // Convert the Node-style MB unit into bytes before handing off to
        // the runtime builder.
        let bytes = (cli.max_old_space_size as usize).saturating_mul(1024 * 1024);
        builder = builder.max_heap_bytes(bytes);
    }

    // JIT introspection flags
    if cli.dump_bytecode {
        builder = builder.dump_bytecode(true);
    }
    if cli.dump_mir {
        builder = builder.dump_mir(true);
    }
    if cli.dump_clif {
        builder = builder.dump_clif(true);
    }
    if cli.dump_asm {
        builder = builder.dump_asm(true);
    }
    if cli.dump_jit_stats {
        builder = builder.dump_jit_stats(true);
    }

    Ok(builder.build())
}
