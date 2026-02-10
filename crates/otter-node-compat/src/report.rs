//! Test result reporting.

use colored::*;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::runner::{TestOutcome, TestResult};

/// Aggregated test run report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestReport {
    pub total: usize,
    pub passed: usize,
    pub failed: usize,
    pub skipped: usize,
    pub timeout: usize,
    pub crashed: usize,
    pub pass_rate: f64,
    /// Results grouped by module.
    pub by_module: BTreeMap<String, ModuleReport>,
    /// First N failure details.
    pub failures: Vec<FailureInfo>,
}

/// Per-module report.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModuleReport {
    pub total: usize,
    pub passed: usize,
    pub failed: usize,
    pub skipped: usize,
}

impl ModuleReport {
    pub fn pass_rate(&self) -> f64 {
        let run = self.passed + self.failed;
        if run > 0 {
            (self.passed as f64 / run as f64) * 100.0
        } else {
            0.0
        }
    }
}

/// Information about a failed test.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FailureInfo {
    pub path: String,
    pub module: String,
    pub error: String,
}

impl TestReport {
    /// Build a report from test results.
    pub fn from_results(results: &[TestResult]) -> Self {
        let mut report = Self {
            total: results.len(),
            passed: 0,
            failed: 0,
            skipped: 0,
            timeout: 0,
            crashed: 0,
            pass_rate: 0.0,
            by_module: BTreeMap::new(),
            failures: Vec::new(),
        };

        for result in results {
            match result.outcome {
                TestOutcome::Pass => report.passed += 1,
                TestOutcome::Fail => {
                    report.failed += 1;
                    if report.failures.len() < 500 {
                        report.failures.push(FailureInfo {
                            path: result.path.clone(),
                            module: result.module.clone(),
                            error: result.error.clone().unwrap_or_default(),
                        });
                    }
                }
                TestOutcome::Skip => report.skipped += 1,
                TestOutcome::Timeout => report.timeout += 1,
                TestOutcome::Crash => report.crashed += 1,
            }

            let mod_report = report.by_module.entry(result.module.clone()).or_default();
            mod_report.total += 1;
            match result.outcome {
                TestOutcome::Pass => mod_report.passed += 1,
                TestOutcome::Fail => mod_report.failed += 1,
                TestOutcome::Skip => mod_report.skipped += 1,
                _ => {}
            }
        }

        let run_count = report.passed + report.failed + report.timeout + report.crashed;
        if run_count > 0 {
            report.pass_rate = (report.passed as f64 / run_count as f64) * 100.0;
        }

        report
    }

    /// Print colored summary to stdout.
    pub fn print_summary(&self) {
        println!();
        println!("{}", "=== Node.js Compatibility Results ===".bold().cyan());
        println!("Total:   {}", self.total);
        println!(
            "Passed:  {} ({:.1}%)",
            self.passed.to_string().green(),
            self.pass_rate
        );
        println!("Failed:  {}", self.failed.to_string().red());
        println!("Skipped: {}", self.skipped.to_string().yellow());
        println!("Timeout: {}", self.timeout);
        println!("Crashed: {}", self.crashed);

        if !self.by_module.is_empty() {
            println!();
            println!("{}", "=== By Module ===".bold().cyan());
            for (module, mr) in &self.by_module {
                let rate = mr.pass_rate();
                let rate_color = if rate >= 80.0 {
                    format!("{:.1}%", rate).green()
                } else if rate >= 50.0 {
                    format!("{:.1}%", rate).yellow()
                } else {
                    format!("{:.1}%", rate).red()
                };
                println!(
                    "  {:<16} {}/{} ({})",
                    module,
                    mr.passed.to_string().green(),
                    mr.total,
                    rate_color,
                );
            }
        }

        if !self.failures.is_empty() {
            println!();
            println!("{}", "=== Failures (first 20) ===".bold().red());
            for failure in self.failures.iter().take(20) {
                println!(
                    "  {} [{}] - {}",
                    failure.path.yellow(),
                    failure.module,
                    failure.error,
                );
            }
            if self.failures.len() > 20 {
                println!("  ... and {} more", self.failures.len() - 20);
            }
        }
    }

    /// Export to JSON.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }
}

/// Persisted report for saving to disk and comparing between runs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedReport {
    pub timestamp: String,
    pub otter_version: String,
    pub duration_secs: f64,
    pub summary: TestReport,
    pub results: Vec<TestResult>,
}

impl PersistedReport {
    pub fn save(&self, path: &std::path::Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(path, json)
    }

    pub fn load(path: &std::path::Path) -> std::io::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        serde_json::from_str(&content)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }
}
