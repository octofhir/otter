use anyhow::Result;
use clap::{Parser, Subcommand};
use otter_runtime::{ConsoleLevel, JscConfig, JscRuntime, set_console_handler};
use std::path::PathBuf;
use std::time::Duration;
use tracing_subscriber::filter::EnvFilter;

#[derive(Parser)]
#[command(name = "otter", version, about = "Otter JavaScript runtime")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Run {
        entry: PathBuf,
        #[arg(long, default_value_t = 5000)]
        timeout_ms: u64,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    set_console_handler(|level, message| match level {
        ConsoleLevel::Warn | ConsoleLevel::Error => eprintln!("{}", message),
        _ => println!("{}", message),
    });

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("info".parse()?))
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Run { entry, timeout_ms } => run_script(entry, timeout_ms)?,
    }

    Ok(())
}

fn run_script(entry: PathBuf, timeout_ms: u64) -> Result<()> {
    let source = std::fs::read_to_string(&entry)?;

    let runtime = JscRuntime::new(JscConfig::default())?;
    run_top_level_async(&runtime, &source, timeout_ms)?;

    Ok(())
}

fn run_top_level_async(runtime: &JscRuntime, source: &str, timeout_ms: u64) -> Result<()> {
    let wrapped = format!(
        "globalThis.__otter_script_error = null;\n\
         (async () => {{\n\
           try {{\n\
         {source}\n\
           }} catch (err) {{\n\
             globalThis.__otter_script_error = err ? String(err) : 'Error';\n\
           }}\n\
         }})();\n",
    );

    runtime.eval(&wrapped)?;

    let timeout = if timeout_ms == 0 {
        Duration::ZERO
    } else {
        Duration::from_millis(timeout_ms)
    };

    runtime.run_event_loop_until_idle(timeout)?;

    let error = runtime.context().get_global("__otter_script_error")?;
    if !error.is_null() && !error.is_undefined() {
        return Err(anyhow::anyhow!(error.to_string()?));
    }

    Ok(())
}
