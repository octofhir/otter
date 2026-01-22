//! Otter CLI - A fast TypeScript/JavaScript runtime.
//!
//! This binary serves dual purposes:
//! 1. Normal CLI mode - parse commands and execute scripts
//! 2. Standalone executable mode - when embedded code is detected, run it directly

use anyhow::Result;
use clap::{Parser, Subcommand};
use otter_node::ext;
use otter_runtime::{ConsoleLevel, JscConfig, JscRuntime, set_console_handler};
use std::path::PathBuf;
use std::time::Duration;
use tracing_subscriber::filter::EnvFilter;

mod embedded;

mod commands;
mod config;
mod watch;

#[derive(Parser)]
#[command(
    name = "otter",
    version,
    about = "A fast TypeScript/JavaScript runtime",
    long_about = "Otter is a secure, fast TypeScript/JavaScript runtime built on JavaScriptCore.\n\n\
                  Run a script:  otter run script.ts\n\
                  Or directly:   otter script.ts\n\
                  Or eval code:  otter -e 'console.log(1+1)'",
    args_conflicts_with_subcommands = true
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Evaluate argument as a script
    #[arg(short = 'e', long = "eval")]
    eval: Option<String>,

    /// Evaluate and print the result
    #[arg(short = 'p', long = "print")]
    print: Option<String>,

    /// Verbose output
    #[arg(short, long, global = true)]
    verbose: bool,

    /// Config file path
    #[arg(long, global = true)]
    config: Option<PathBuf>,
}

/// Alternative CLI for direct file execution: otter file.ts [args...]
#[derive(Parser)]
#[command(name = "otter")]
struct DirectRun {
    /// File to execute or script name
    file: String,

    /// Arguments to pass to script
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    args: Vec<String>,

    /// Config file path
    #[arg(long)]
    config: Option<PathBuf>,

    /// Allow file system read access
    #[arg(long = "allow-read", value_name = "PATH", num_args = 0..)]
    allow_read: Option<Vec<String>>,

    /// Allow file system write access
    #[arg(long = "allow-write", value_name = "PATH", num_args = 0..)]
    allow_write: Option<Vec<String>>,

    /// Allow network access
    #[arg(long = "allow-net", value_name = "HOST", num_args = 0..)]
    allow_net: Option<Vec<String>>,

    /// Allow environment variable access
    #[arg(long = "allow-env", value_name = "VAR", num_args = 0..)]
    allow_env: Option<Vec<String>>,

    /// Allow subprocess execution
    #[arg(long = "allow-run")]
    allow_run: bool,

    /// Allow all permissions
    #[arg(long = "allow-all", short = 'A')]
    allow_all: bool,

    /// Script execution timeout in milliseconds
    #[arg(long, default_value = "30000")]
    timeout: u64,
}

#[derive(Subcommand)]
enum Commands {
    /// Run a JavaScript/TypeScript file or package.json script
    Run(commands::run::RunCommand),

    /// Bundle and compile JavaScript/TypeScript
    Build(commands::build::BuildCommand),

    /// Execute a package binary (npx-like)
    #[command(name = "x")]
    Exec(commands::exec::ExecCommand),

    /// Type check TypeScript files
    Check(commands::check::CheckCommand),

    /// Run tests
    Test(commands::test::TestCommand),

    /// Start interactive REPL
    Repl(commands::repl::ReplCommand),

    /// Install dependencies from package.json
    Install(commands::install::InstallCommand),

    /// Add a dependency
    Add(commands::add::AddCommand),

    /// Remove a dependency
    Remove(commands::remove::RemoveCommand),

    /// Initialize a new project
    Init(commands::init::InitCommand),

    /// Show runtime information
    Info(commands::info::InfoCommand),
}

#[tokio::main]
async fn main() -> Result<()> {
    // Set up console handler
    set_console_handler(|level, message| match level {
        ConsoleLevel::Warn | ConsoleLevel::Error => eprintln!("{}", message),
        _ => println!("{}", message),
    });

    // Check if we have embedded code FIRST (standalone executable mode)
    // This must happen before any CLI parsing to support compiled executables
    if let Some(code) = embedded::load_embedded_code()? {
        return embedded::run_embedded(code).await;
    }

    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("warn".parse()?))
        .init();

    // First try parsing as main CLI with subcommands
    let args: Vec<String> = std::env::args().collect();

    // Check if first non-flag arg looks like a file path (not a subcommand)
    let maybe_file = args.get(1).map(|s| s.as_str());
    let is_direct_run = match maybe_file {
        Some(arg) if !arg.starts_with('-') => {
            // Not a flag - check if it's a known subcommand
            !matches!(
                arg,
                "run"
                    | "build"
                    | "x"
                    | "check"
                    | "test"
                    | "repl"
                    | "install"
                    | "add"
                    | "remove"
                    | "init"
                    | "info"
                    | "help"
            )
        }
        _ => false,
    };

    if is_direct_run {
        // Direct file execution: otter file.ts [args...]
        let direct = DirectRun::parse();
        let config = config::load_config(direct.config.as_deref())?;

        let run_cmd = commands::run::RunCommand {
            entry: Some(direct.file),
            args: direct.args,
            allow_read: direct.allow_read,
            allow_write: direct.allow_write,
            allow_net: direct.allow_net,
            allow_env: direct.allow_env,
            env_files: vec![],
            env_vars: vec![],
            allow_run: direct.allow_run,
            allow_all: direct.allow_all,
            check: false,
            timeout: direct.timeout,
            watch: false,
            file: false,
        };
        return run_cmd.run(&config).await;
    }

    // Standard subcommand parsing
    let cli = Cli::parse();
    let config = config::load_config(cli.config.as_deref())?;

    // Handle --eval / -e flag
    if let Some(code) = cli.eval {
        return run_eval(&code, false).await;
    }

    // Handle --print / -p flag
    if let Some(code) = cli.print {
        return run_eval(&code, true).await;
    }

    match cli.command {
        Some(Commands::Run(cmd)) => cmd.run(&config).await,
        Some(Commands::Build(cmd)) => cmd.run(&config).await,
        Some(Commands::Exec(cmd)) => cmd.run().await,
        Some(Commands::Check(cmd)) => cmd.run(&config).await,
        Some(Commands::Test(cmd)) => cmd.run(&config).await,
        Some(Commands::Repl(cmd)) => cmd.run(&config).await,
        Some(Commands::Install(cmd)) => cmd.run().await,
        Some(Commands::Add(cmd)) => cmd.run().await,
        Some(Commands::Remove(cmd)) => cmd.run().await,
        Some(Commands::Init(cmd)) => cmd.run().await,
        Some(Commands::Info(cmd)) => cmd.run(),
        None => {
            // No command - show help and available scripts
            show_help_with_scripts()
        }
    }
}

/// Show help with available scripts from package.json
fn show_help_with_scripts() -> Result<()> {
    use clap::CommandFactory;
    use otter_pm::ScriptRunner;

    Cli::command().print_help()?;
    println!();

    // Show available scripts if in a project
    let cwd = std::env::current_dir()?;
    if let Some(runner) = ScriptRunner::try_new(&cwd) {
        let scripts = runner.list();
        if !scripts.is_empty() {
            println!("\nAvailable scripts (from package.json):");
            for (name, cmd) in scripts.iter().take(5) {
                let truncated = if cmd.len() > 40 {
                    format!("{}...", &cmd[..37])
                } else {
                    cmd.to_string()
                };
                println!("  {} → {}", name, truncated);
            }
            if scripts.len() > 5 {
                println!("  ... and {} more", scripts.len() - 5);
            }
            println!("\nRun 'otter run <script>' or 'otter <file.ts>'");
        }
    }

    Ok(())
}

/// Run code from --eval / -e or --print / -p flag
async fn run_eval(code: &str, print_result: bool) -> Result<()> {
    let runtime = JscRuntime::new(JscConfig::default())?;

    // Register essential extensions for eval mode
    runtime.register_extension(ext::url())?;
    runtime.register_extension(ext::buffer())?;
    runtime.register_extension(ext::events())?;
    runtime.register_extension(ext::util())?;
    runtime.register_extension(ext::process())?;
    runtime.register_extension(ext::string_decoder())?;
    runtime.register_extension(ext::readline())?;

    // Set up minimal process object for eval mode
    let process_setup = r#"
(function() {
    globalThis.process = globalThis.process || {};
    process.env = process.env || {};
    process.argv = ['otter', '-e'];
    process.cwd = () => '.';
    process.platform = 'darwin';
    process.arch = 'arm64';
    process.version = 'v1.0.0';
    process.exit = (code) => { throw new Error('process.exit() called with code ' + code); };
    process.stdout = { write: (s) => { console.log(s); return true; }, isTTY: false };
    process.stderr = { write: (s) => { console.error(s); return true; }, isTTY: false };
    process.stdin = { isTTY: false };
})();
"#;

    let wrapped = if print_result {
        // --print: evaluate and print the result
        format!(
            "{process_setup}\n\
             globalThis.__eval_result = (function() {{ return ({code}); }})();\n\
             console.log(__eval_result);"
        )
    } else {
        // --eval: just evaluate
        format!("{process_setup}\n{code}")
    };

    runtime.eval(&wrapped)?;
    runtime.run_event_loop_until_idle(Duration::from_millis(30000))?;

    Ok(())
}
