use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;

#[derive(Parser, Debug)]
#[command(
    name = "node-compat",
    about = "Run official Node.js compatibility tests against Otter"
)]
struct Cli {
    /// Module names from node_compat_config.toml (defaults to all configured modules)
    #[arg(value_name = "MODULE")]
    modules: Vec<String>,

    /// Limit the number of selected tests
    #[arg(long)]
    limit: Option<usize>,

    /// Keep only tests whose file name contains this substring
    #[arg(long)]
    filter: Option<String>,

    /// Override the config file path
    #[arg(long, default_value = "node_compat_config.toml")]
    config: PathBuf,

    /// Override the otter binary path
    #[arg(long)]
    otter_bin: Option<PathBuf>,

    /// Override timeout per test in seconds
    #[arg(long)]
    timeout_secs: Option<u64>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let workspace_root = std::env::current_dir()?;
    let mut options = otter_node_compat::RunOptions::new(workspace_root);
    options.config_path = options.workspace_root.join(cli.config);
    options.selected_modules = cli.modules;
    options.limit = cli.limit;
    options.substring_filter = cli.filter;
    options.timeout_secs = cli.timeout_secs;
    options.otter_bin = cli.otter_bin;

    let report = otter_node_compat::run(options)?;
    println!(
        "node-compat: {}/{} passed ({:.1}%)",
        report.summary.passed, report.summary.total, report.summary.pass_rate
    );
    if !report.summary.failures.is_empty() {
        println!("failures:");
        for failure in report.summary.failures.iter().take(20) {
            println!(
                "  {}: {}",
                failure.path,
                failure.error.lines().next().unwrap_or("")
            );
        }
    }
    Ok(())
}
