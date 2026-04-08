//! Merge multiple `PersistedReport` batch files into one canonical report.
//!
//! Used by `scripts/test262-full-run.sh` after the per-subdirectory batches
//! complete: each batch writes its own `PersistedReport` via `--save` and
//! this binary concatenates the result arrays, recomputes the aggregate
//! summary, and writes a single `latest.json` that `gen-conformance` can
//! consume directly. Replaces the old Python merger that relied on a
//! now-gone `--save` contract.
//!
//! Usage:
//!   cargo run -p otter-test262 --bin merge-reports -- \
//!       --input 'test262_results/batch_*.json' \
//!       --output test262_results/latest.json

use std::path::PathBuf;

use clap::Parser;
use otter_test262::{PersistedReport, TestReport};

#[derive(Parser, Debug)]
#[command(name = "merge-reports")]
#[command(about = "Merge per-batch Test262 reports into one canonical PersistedReport")]
struct Cli {
    /// Glob pattern for input batch files. Example:
    /// `test262_results/batch_*.json`.
    #[arg(long)]
    input: String,

    /// Output path for the merged `PersistedReport` JSON.
    #[arg(long, default_value = "test262_results/latest.json")]
    output: PathBuf,

    /// Verbose progress output.
    #[arg(short, long)]
    verbose: bool,
}

fn main() {
    let cli = Cli::parse();

    let paths = match glob_expand(&cli.input) {
        Ok(paths) => paths,
        Err(e) => {
            eprintln!("Failed to expand glob '{}': {e}", cli.input);
            std::process::exit(1);
        }
    };

    if paths.is_empty() {
        eprintln!(
            "No files matched '{}'. Nothing to merge (exiting with code 0).",
            cli.input
        );
        return;
    }

    let mut merged_results = Vec::new();
    let mut earliest_timestamp: Option<String> = None;
    let mut otter_version = env!("CARGO_PKG_VERSION").to_string();
    let mut test262_commit: Option<String> = None;
    let mut duration_secs: f64 = 0.0;
    let mut loaded = 0usize;
    let mut skipped = 0usize;

    for path in &paths {
        match PersistedReport::load(path) {
            Ok(report) => {
                if cli.verbose {
                    eprintln!(
                        "+ {} ({} results, {:.1}s)",
                        path.display(),
                        report.results.len(),
                        report.duration_secs
                    );
                }
                if earliest_timestamp.is_none() {
                    earliest_timestamp = Some(report.timestamp.clone());
                }
                if report.test262_commit.is_some() {
                    test262_commit = report.test262_commit.clone();
                }
                otter_version = report.otter_version.clone();
                duration_secs += report.duration_secs;
                merged_results.extend(report.results);
                loaded += 1;
            }
            Err(e) => {
                eprintln!("! {}: {e}", path.display());
                skipped += 1;
            }
        }
    }

    if loaded == 0 {
        eprintln!("No batch files could be loaded. Aborting merge.");
        std::process::exit(1);
    }

    // Rebuild the aggregate summary from the merged result list so per-type
    // counts and the pass-rate reflect the whole run, not any single batch.
    let summary = TestReport::from_results(&merged_results);

    let merged = PersistedReport {
        timestamp: earliest_timestamp.unwrap_or_else(|| chrono::Utc::now().to_rfc3339()),
        otter_version,
        test262_commit,
        duration_secs,
        summary,
        results: merged_results,
    };

    if let Err(e) = merged.save(&cli.output) {
        eprintln!("Failed to save merged report to {}: {e}", cli.output.display());
        std::process::exit(1);
    }

    eprintln!(
        "Merged {loaded} batch(es) ({skipped} skipped) -> {} ({} results, pass {}/{} = {:.1}%)",
        cli.output.display(),
        merged.results.len(),
        merged.summary.passed,
        merged.summary.passed
            + merged.summary.failed
            + merged.summary.timeout
            + merged.summary.crashed
            + merged.summary.out_of_memory,
        merged.summary.pass_rate,
    );
}

/// Minimal glob expansion using only the standard library — avoids
/// pulling in the `glob` crate just for this one helper. Supports the
/// shell-style `prefix/batch_*.json` pattern used by `test262-full-run.sh`.
fn glob_expand(pattern: &str) -> std::io::Result<Vec<PathBuf>> {
    // Split into directory + file pattern at the last `/`.
    let (dir, file_pattern) = match pattern.rsplit_once('/') {
        Some((d, f)) => (PathBuf::from(d), f.to_string()),
        None => (PathBuf::from("."), pattern.to_string()),
    };

    let star = file_pattern.find('*');
    let (prefix, suffix) = match star {
        Some(idx) => (
            file_pattern[..idx].to_string(),
            file_pattern[idx + 1..].to_string(),
        ),
        None => {
            // No wildcard: just check the exact file.
            let candidate = dir.join(&file_pattern);
            return Ok(if candidate.is_file() {
                vec![candidate]
            } else {
                Vec::new()
            });
        }
    };

    let mut out = Vec::new();
    if !dir.is_dir() {
        return Ok(out);
    }
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with(&prefix) && name.ends_with(&suffix) && name.len() >= prefix.len() + suffix.len()
        {
            out.push(entry.path());
        }
    }
    out.sort();
    Ok(out)
}
