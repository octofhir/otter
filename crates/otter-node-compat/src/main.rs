use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use otter_node_compat::Outcome;

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

    let full_corpus = options.is_full_corpus();
    let report = otter_node_compat::run(options)?;
    println!(
        "node-compat: {}/{} passed ({:.1}%)",
        report.summary.passed, report.summary.total, report.summary.pass_rate
    );
    if !full_corpus {
        println!(
            "note: partial run — NODE_CONFORMANCE.md and the site dashboard keep \
             the last full-corpus baseline"
        );
    }
    let failures: Vec<_> = report
        .results
        .iter()
        .filter(|result| result.outcome == Outcome::Fail)
        .collect();
    if !failures.is_empty() {
        println!("failures:");
        for failure in failures.iter().take(20) {
            println!(
                "  {}: {}",
                failure.path,
                failure
                    .error
                    .as_deref()
                    .and_then(|error| error.lines().next())
                    .unwrap_or("")
            );
        }
    }
    for outcome in [Outcome::Timeout, Outcome::Crashed] {
        let results: Vec<_> = report
            .results
            .iter()
            .filter(|result| result.outcome == outcome)
            .collect();
        if results.is_empty() {
            continue;
        }
        let label = if outcome == Outcome::Timeout {
            "timeouts"
        } else {
            "crashes"
        };
        println!("{label}:");
        for result in results {
            println!("  {} ({} ms)", result.path, result.duration_ms);
        }
    }
    Ok(())
}
