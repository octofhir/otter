//! Generate ES_CONFORMANCE.md from saved test262 results.
//!
//! Usage:
//!   cargo run -p otter-test262 --bin gen-conformance
//!   # or: just test262-conformance

use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;

use otter_test262::config::Test262Config;
use otter_test262::editions::{self, EsEdition};
use otter_test262::{PersistedReport, TestOutcome};

/// Stats accumulator for a category (edition, built-in, language feature).
#[derive(Default)]
struct Stats {
    pass: usize,
    fail: usize,
    timeout: usize,
    skip: usize,
    crash: usize,
}

impl Stats {
    fn total_run(&self) -> usize {
        self.pass + self.fail + self.timeout + self.crash
    }

    fn pass_rate(&self) -> f64 {
        let run = self.total_run();
        if run > 0 {
            (self.pass as f64 / run as f64) * 100.0
        } else {
            0.0
        }
    }

    fn record(&mut self, outcome: &TestOutcome) {
        match outcome {
            TestOutcome::Pass => self.pass += 1,
            TestOutcome::Fail => self.fail += 1,
            TestOutcome::Skip => self.skip += 1,
            TestOutcome::Timeout => self.timeout += 1,
            TestOutcome::Crash => self.crash += 1,
        }
    }
}

fn main() {
    let results_path = PathBuf::from("test262_results/latest.json");

    if !results_path.exists() {
        eprintln!("Error: {} not found.", results_path.display());
        eprintln!("Run `just test262-save` first to generate test results.");
        std::process::exit(1);
    }

    let report = match PersistedReport::load(&results_path) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Error loading {}: {}", results_path.display(), e);
            std::process::exit(1);
        }
    };

    // Load config to get skip_features list
    let config = Test262Config::load_or_default(None);

    // Accumulate stats
    let mut by_edition: BTreeMap<EsEdition, Stats> = BTreeMap::new();
    let mut by_builtin: BTreeMap<String, Stats> = BTreeMap::new();
    let mut by_language: BTreeMap<String, Stats> = BTreeMap::new();
    let mut overall = Stats::default();

    for result in &report.results {
        overall.record(&result.outcome);

        // Per-edition (same logic as RunSummary::record in main.rs)
        if result.features.is_empty() {
            by_edition
                .entry(EsEdition::ES5)
                .or_default()
                .record(&result.outcome);
        } else {
            for feature in &result.features {
                let edition = editions::feature_edition(feature);
                by_edition
                    .entry(edition)
                    .or_default()
                    .record(&result.outcome);
            }
        }

        // Per built-in / language feature (from path)
        // Paths may start with "test/" prefix, strip it first
        let path = result.path.strip_prefix("test/").unwrap_or(&result.path);

        // Try matching built-ins (including annexB/built-ins)
        if let Some(rest) = path
            .strip_prefix("built-ins/")
            .or_else(|| path.strip_prefix("annexB/built-ins/"))
        {
            let category = rest.split('/').next().unwrap_or("other");
            by_builtin
                .entry(category.to_string())
                .or_default()
                .record(&result.outcome);
        } else if let Some(rest) = path
            .strip_prefix("language/")
            .or_else(|| path.strip_prefix("annexB/language/"))
        {
            let category = rest.split('/').next().unwrap_or("other");
            by_language
                .entry(category.to_string())
                .or_default()
                .record(&result.outcome);
        }
    }

    // Get git commit
    let git_commit = std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    let today = chrono::Utc::now().format("%Y-%m-%d");

    // Generate markdown
    let output_path = PathBuf::from("ES_CONFORMANCE.md");
    let mut f = std::fs::File::create(&output_path).expect("Failed to create ES_CONFORMANCE.md");

    writeln!(f, "# ECMAScript Conformance Status").unwrap();
    writeln!(f).unwrap();
    writeln!(f, "Last updated: {} (commit: {})", today, git_commit).unwrap();
    writeln!(
        f,
        "Overall: {}/{} tests passing ({:.1}%)",
        overall.pass,
        overall.total_run(),
        overall.pass_rate()
    )
    .unwrap();
    writeln!(f, "Default per-test timeout: 10s").unwrap();
    writeln!(f).unwrap();

    // How to update
    writeln!(f, "## How to update").unwrap();
    writeln!(f).unwrap();
    writeln!(f, "```bash").unwrap();
    writeln!(
        f,
        "just test262-save              # Run full suite, save results"
    )
    .unwrap();
    writeln!(
        f,
        "just test262-conformance        # Regenerate this document"
    )
    .unwrap();
    writeln!(f, "```").unwrap();
    writeln!(f).unwrap();

    // Per-Edition Summary
    writeln!(f, "## Per-Edition Summary").unwrap();
    writeln!(f).unwrap();
    writeln!(
        f,
        "| Edition | Total | Pass | Fail | Timeout | Skip | Pass % |"
    )
    .unwrap();
    writeln!(
        f,
        "|---------|------:|-----:|-----:|--------:|-----:|-------:|"
    )
    .unwrap();

    for (edition, stats) in &by_edition {
        writeln!(
            f,
            "| {:<7} | {:>5} | {:>4} | {:>4} | {:>7} | {:>4} | {:>5.1}% |",
            edition,
            stats.total_run() + stats.skip,
            stats.pass,
            stats.fail + stats.crash,
            stats.timeout,
            stats.skip,
            stats.pass_rate()
        )
        .unwrap();
    }
    writeln!(f).unwrap();

    // Built-in Objects
    writeln!(f, "## Built-in Objects").unwrap();
    writeln!(f).unwrap();
    writeln!(
        f,
        "| Built-in | Total | Pass | Fail | Timeout | Skip | Pass % |"
    )
    .unwrap();
    writeln!(
        f,
        "|----------|------:|-----:|-----:|--------:|-----:|-------:|"
    )
    .unwrap();

    for (name, stats) in &by_builtin {
        writeln!(
            f,
            "| {:<30} | {:>5} | {:>4} | {:>4} | {:>7} | {:>4} | {:>5.1}% |",
            name,
            stats.total_run() + stats.skip,
            stats.pass,
            stats.fail + stats.crash,
            stats.timeout,
            stats.skip,
            stats.pass_rate()
        )
        .unwrap();
    }
    writeln!(f).unwrap();

    // Language Features
    writeln!(f, "## Language Features").unwrap();
    writeln!(f).unwrap();
    writeln!(
        f,
        "| Feature | Total | Pass | Fail | Timeout | Skip | Pass % |"
    )
    .unwrap();
    writeln!(
        f,
        "|---------|------:|-----:|-----:|--------:|-----:|-------:|"
    )
    .unwrap();

    for (name, stats) in &by_language {
        writeln!(
            f,
            "| {:<30} | {:>5} | {:>4} | {:>4} | {:>7} | {:>4} | {:>5.1}% |",
            name,
            stats.total_run() + stats.skip,
            stats.pass,
            stats.fail + stats.crash,
            stats.timeout,
            stats.skip,
            stats.pass_rate()
        )
        .unwrap();
    }
    writeln!(f).unwrap();

    // Skipped Features
    writeln!(f, "## Skipped Features (not yet implemented)").unwrap();
    writeln!(f).unwrap();
    writeln!(
        f,
        "These test262 features are skipped via `test262_config.toml`:"
    )
    .unwrap();
    writeln!(f).unwrap();
    writeln!(f, "| Feature | Edition |").unwrap();
    writeln!(f, "|---------|---------|").unwrap();

    let mut skip_features: Vec<_> = config.skip_features.iter().collect();
    skip_features.sort();
    for feature in &skip_features {
        let edition = editions::feature_edition(feature);
        writeln!(f, "| {} | {} |", feature, edition).unwrap();
    }
    writeln!(f).unwrap();

    eprintln!(
        "Generated {} — Overall: {}/{} ({:.1}%)",
        output_path.display(),
        overall.pass,
        overall.total_run(),
        overall.pass_rate()
    );
}
