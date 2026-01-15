//! Otter CLI - A fast TypeScript/JavaScript runtime.

use anyhow::Result;
use clap::{Parser, Subcommand};
use otter_runtime::{ConsoleLevel, set_console_handler};
use std::path::PathBuf;
use tracing_subscriber::filter::EnvFilter;

mod commands;
mod config;

#[derive(Parser)]
#[command(
    name = "otter",
    version,
    about = "A fast TypeScript/JavaScript runtime",
    long_about = "Otter is a secure, fast TypeScript/JavaScript runtime built on JavaScriptCore.\n\n\
                  Run a script:  otter run script.ts\n\
                  Or directly:   otter script.ts",
    args_conflicts_with_subcommands = true
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

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
    /// File to execute
    file: PathBuf,

    /// Arguments to pass to script
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    args: Vec<String>,

    /// Config file path
    #[arg(long)]
    config: Option<PathBuf>,
}

#[derive(Subcommand)]
enum Commands {
    /// Run a JavaScript or TypeScript file
    Run(commands::run::RunCommand),

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
            entry: direct.file,
            args: direct.args,
            allow_read: None,
            allow_write: None,
            allow_net: None,
            allow_env: None,
            allow_run: false,
            allow_all: false,
            no_check: false,
            timeout: 30000,
            watch: false,
        };
        return run_cmd.run(&config).await;
    }

    // Standard subcommand parsing
    let cli = Cli::parse();
    let config = config::load_config(cli.config.as_deref())?;

    match cli.command {
        Some(Commands::Run(cmd)) => cmd.run(&config).await,
        Some(Commands::Check(cmd)) => cmd.run(&config).await,
        Some(Commands::Test(cmd)) => cmd.run(&config).await,
        Some(Commands::Repl(cmd)) => cmd.run(&config).await,
        Some(Commands::Install(cmd)) => cmd.run().await,
        Some(Commands::Add(cmd)) => cmd.run().await,
        Some(Commands::Remove(cmd)) => cmd.run().await,
        Some(Commands::Init(cmd)) => cmd.run().await,
        Some(Commands::Info(cmd)) => cmd.run(),
        None => {
            // No command - show help
            use clap::CommandFactory;
            Cli::command().print_help()?;
            println!();
            Ok(())
        }
    }
}
