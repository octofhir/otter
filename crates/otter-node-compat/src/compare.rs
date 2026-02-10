//! Result comparison between test runs.

use colored::*;
use std::collections::HashMap;
use std::path::Path;

use crate::report::PersistedReport;
use crate::runner::TestOutcome;

/// Comparison between two test runs.
#[derive(Debug)]
pub struct RunComparison {
    /// Tests that were fixed (fail → pass).
    pub fixed: Vec<String>,
    /// Tests that regressed (pass → fail).
    pub broken: Vec<String>,
    /// Tests that newly crash.
    pub new_panics: Vec<String>,
    /// Tests that no longer crash.
    pub fixed_panics: Vec<String>,
    pub pass_delta: i64,
    pub fail_delta: i64,
    pub base_pass_rate: f64,
    pub new_pass_rate: f64,
}

impl RunComparison {
    /// Compare two persisted reports.
    pub fn compare(base: &PersistedReport, new: &PersistedReport) -> Self {
        let mut base_results: HashMap<String, TestOutcome> = HashMap::new();
        for result in &base.results {
            base_results.insert(result.path.clone(), result.outcome);
        }

        let mut new_results: HashMap<String, TestOutcome> = HashMap::new();
        for result in &new.results {
            new_results.insert(result.path.clone(), result.outcome);
        }

        let mut fixed = Vec::new();
        let mut broken = Vec::new();
        let mut new_panics = Vec::new();
        let mut fixed_panics = Vec::new();

        for (key, new_outcome) in &new_results {
            if let Some(base_outcome) = base_results.get(key) {
                match (base_outcome, new_outcome) {
                    (TestOutcome::Fail, TestOutcome::Pass) => fixed.push(key.clone()),
                    (TestOutcome::Pass, TestOutcome::Fail) => broken.push(key.clone()),
                    (TestOutcome::Crash, TestOutcome::Pass | TestOutcome::Fail) => {
                        fixed_panics.push(key.clone());
                    }
                    (_, TestOutcome::Crash) if *base_outcome != TestOutcome::Crash => {
                        new_panics.push(key.clone());
                    }
                    _ => {}
                }
            }
        }

        fixed.sort();
        broken.sort();
        new_panics.sort();
        fixed_panics.sort();

        RunComparison {
            fixed,
            broken,
            new_panics,
            fixed_panics,
            pass_delta: new.summary.passed as i64 - base.summary.passed as i64,
            fail_delta: new.summary.failed as i64 - base.summary.failed as i64,
            base_pass_rate: base.summary.pass_rate,
            new_pass_rate: new.summary.pass_rate,
        }
    }

    /// Print colored comparison report.
    pub fn print(&self) {
        println!("{}", "=== Node Compat Run Comparison ===".bold().cyan());
        println!();

        let rate_delta = self.new_pass_rate - self.base_pass_rate;
        let rate_str = if rate_delta >= 0.0 {
            format!("+{:.2}%", rate_delta).green()
        } else {
            format!("{:.2}%", rate_delta).red()
        };
        println!(
            "Pass rate: {:.2}% → {:.2}% ({})",
            self.base_pass_rate, self.new_pass_rate, rate_str
        );

        let pass_str = if self.pass_delta >= 0 {
            format!("+{}", self.pass_delta).green()
        } else {
            format!("{}", self.pass_delta).red()
        };
        println!("Pass delta: {}", pass_str);

        if !self.fixed.is_empty() {
            println!();
            println!("{} ({}):", "Fixed tests".green().bold(), self.fixed.len());
            for test in self.fixed.iter().take(20) {
                println!("  {} {}", "+".green(), test);
            }
            if self.fixed.len() > 20 {
                println!("  ... and {} more", self.fixed.len() - 20);
            }
        }

        if !self.broken.is_empty() {
            println!();
            println!("{} ({}):", "Regressions".red().bold(), self.broken.len());
            for test in self.broken.iter().take(20) {
                println!("  {} {}", "-".red(), test);
            }
            if self.broken.len() > 20 {
                println!("  ... and {} more", self.broken.len() - 20);
            }
        }

        if !self.new_panics.is_empty() {
            println!();
            println!(
                "{} ({}):",
                "New crashes".red().bold(),
                self.new_panics.len()
            );
            for test in self.new_panics.iter().take(10) {
                println!("  {} {}", "!".red(), test);
            }
        }

        if !self.fixed_panics.is_empty() {
            println!();
            println!(
                "{} ({}):",
                "Fixed crashes".green().bold(),
                self.fixed_panics.len()
            );
            for test in self.fixed_panics.iter().take(10) {
                println!("  {} {}", "+".green(), test);
            }
        }

        if self.fixed.is_empty()
            && self.broken.is_empty()
            && self.new_panics.is_empty()
            && self.fixed_panics.is_empty()
        {
            println!();
            println!("{}", "No changes detected.".dimmed());
        }
    }

    /// Returns true if there are regressions.
    pub fn has_regressions(&self) -> bool {
        !self.broken.is_empty() || !self.new_panics.is_empty()
    }
}

/// Compare two result files.
pub fn compare_files(base_path: &Path, new_path: &Path) -> Result<RunComparison, String> {
    let base = PersistedReport::load(base_path).map_err(|e| {
        format!(
            "Failed to load base report '{}': {}",
            base_path.display(),
            e
        )
    })?;
    let new = PersistedReport::load(new_path)
        .map_err(|e| format!("Failed to load new report '{}': {}", new_path.display(), e))?;

    Ok(RunComparison::compare(&base, &new))
}
