//! Test result reporting

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::editions;
use crate::metadata::ExecutionMode;
use crate::runner::{TestOutcome, TestResult};

/// Test run report
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestReport {
    /// Total number of test executions (counting each mode separately)
    pub total: usize,
    /// Number of passed tests
    pub passed: usize,
    /// Number of failed tests
    pub failed: usize,
    /// Number of skipped tests
    pub skipped: usize,
    /// Number of timed out tests
    pub timeout: usize,
    /// Number of crashed tests
    pub crashed: usize,
    /// Pass rate as percentage (excluding skipped)
    pub pass_rate: f64,
    /// Results by feature
    pub by_feature: HashMap<String, FeatureReport>,
    /// Failed test details
    pub failures: Vec<FailureInfo>,
}

/// Per-feature report
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FeatureReport {
    /// Total tests for this feature
    pub total: usize,
    /// Passed tests
    pub passed: usize,
    /// Failed tests
    pub failed: usize,
    /// Skipped tests
    pub skipped: usize,
}

/// Information about a failed test
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FailureInfo {
    /// Test path (including mode suffix)
    pub path: String,
    /// Execution mode
    pub mode: ExecutionMode,
    /// Error message
    pub error: String,
}

impl TestReport {
    /// Generate a report from test results
    pub fn from_results(results: &[TestResult]) -> Self {
        let mut report = Self {
            total: results.len(),
            passed: 0,
            failed: 0,
            skipped: 0,
            timeout: 0,
            crashed: 0,
            pass_rate: 0.0,
            by_feature: HashMap::new(),
            failures: Vec::new(),
        };

        for result in results {
            match result.outcome {
                TestOutcome::Pass => report.passed += 1,
                TestOutcome::Fail => {
                    report.failed += 1;
                    report.failures.push(FailureInfo {
                        path: result.path.clone(),
                        mode: result.mode,
                        error: result.error.clone().unwrap_or_default(),
                    });
                }
                TestOutcome::Skip => report.skipped += 1,
                TestOutcome::Timeout => report.timeout += 1,
                TestOutcome::Crash => report.crashed += 1,
            }

            // Track by feature
            for feature in &result.features {
                let feature_report = report.by_feature.entry(feature.clone()).or_default();

                feature_report.total += 1;
                match result.outcome {
                    TestOutcome::Pass => feature_report.passed += 1,
                    TestOutcome::Fail => feature_report.failed += 1,
                    TestOutcome::Skip => feature_report.skipped += 1,
                    _ => {}
                }
            }
        }

        // Calculate pass rate (excluding skipped)
        let run_count = report.passed + report.failed + report.timeout + report.crashed;
        if run_count > 0 {
            report.pass_rate = (report.passed as f64 / run_count as f64) * 100.0;
        }

        report
    }

    /// Print a summary to stdout
    pub fn print_summary(&self) {
        use colored::*;

        println!();
        println!("{}", "=== Test262 Results ===".bold().cyan());
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

        if !self.failures.is_empty() {
            println!();
            println!("{}", "=== Failures (first 10) ===".bold().red());
            for failure in self.failures.iter().take(200) {
                println!(
                    "  {} ({}) - {}",
                    failure.path.yellow(),
                    failure.mode,
                    failure.error
                );
            }
            if self.failures.len() > 10 {
                println!("  ... and {} more", self.failures.len() - 10);
            }
        }
    }

    /// Export to JSON
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }
}

/// Persisted report for saving results to disk and comparing between runs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedReport {
    /// Timestamp of the run (ISO 8601)
    pub timestamp: String,
    /// Otter version
    pub otter_version: String,
    /// Test262 commit SHA (if known)
    pub test262_commit: Option<String>,
    /// Total duration of the run in seconds
    pub duration_secs: f64,
    /// Summary statistics
    pub summary: TestReport,
    /// Individual test results
    pub results: Vec<TestResult>,
}

impl PersistedReport {
    /// Save to a JSON file
    pub fn save(&self, path: &std::path::Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(path, json)
    }

    /// Load from a JSON file
    pub fn load(path: &std::path::Path) -> std::io::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        serde_json::from_str(&content)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }
}

// ---------------------------------------------------------------------------
// RunSummary — streaming accumulator used by both sequential and parallel paths
// ---------------------------------------------------------------------------

/// Streaming accumulator for test run results.
///
/// Collects results incrementally as tests complete — suitable for both the
/// sequential runner loop and the parallel worker result collector.
pub struct RunSummary {
    pub total: usize,
    pub passed: usize,
    pub failed: usize,
    pub skipped: usize,
    pub timeout: usize,
    pub crashed: usize,
    pub by_feature: HashMap<String, FeatureReport>,
    pub by_edition: HashMap<editions::EsEdition, editions::EditionReport>,
    pub failures: Vec<FailureInfo>,
    pub all_results: Vec<TestResult>,
    pub max_failures: usize,
}

impl RunSummary {
    pub fn new(max_failures: usize) -> Self {
        Self {
            total: 0,
            passed: 0,
            failed: 0,
            skipped: 0,
            timeout: 0,
            crashed: 0,
            by_feature: HashMap::new(),
            by_edition: HashMap::new(),
            failures: Vec::new(),
            all_results: Vec::new(),
            max_failures,
        }
    }

    pub fn record(&mut self, result: &TestResult, save_all: bool) {
        self.total += 1;
        match result.outcome {
            TestOutcome::Pass => self.passed += 1,
            TestOutcome::Fail => {
                self.failed += 1;
                if self.failures.len() < self.max_failures {
                    self.failures.push(FailureInfo {
                        path: result.path.clone(),
                        mode: result.mode,
                        error: result.error.clone().unwrap_or_default(),
                    });
                }
            }
            TestOutcome::Skip => self.skipped += 1,
            TestOutcome::Timeout => self.timeout += 1,
            TestOutcome::Crash => self.crashed += 1,
        }

        // Track by feature
        for feature in &result.features {
            let feature_report = self.by_feature.entry(feature.clone()).or_default();
            feature_report.total += 1;
            match result.outcome {
                TestOutcome::Pass => feature_report.passed += 1,
                TestOutcome::Fail => feature_report.failed += 1,
                TestOutcome::Skip => feature_report.skipped += 1,
                _ => {}
            }

            // Track by edition
            let edition = editions::feature_edition(feature);
            let edition_report = self.by_edition.entry(edition).or_default();
            edition_report.total += 1;
            match result.outcome {
                TestOutcome::Pass => edition_report.passed += 1,
                TestOutcome::Fail => edition_report.failed += 1,
                TestOutcome::Skip => edition_report.skipped += 1,
                _ => {}
            }
        }

        // Tests with no features are classified as ES5
        if result.features.is_empty() {
            let ed = self.by_edition.entry(editions::EsEdition::ES5).or_default();
            ed.total += 1;
            match result.outcome {
                TestOutcome::Pass => ed.passed += 1,
                TestOutcome::Fail => ed.failed += 1,
                TestOutcome::Skip => ed.skipped += 1,
                _ => {}
            }
        }

        if save_all {
            self.all_results.push(result.clone());
        }
    }

    pub fn into_report(self) -> TestReport {
        let run_count = self.passed + self.failed + self.timeout + self.crashed;
        let pass_rate = if run_count > 0 {
            (self.passed as f64 / run_count as f64) * 100.0
        } else {
            0.0
        };
        TestReport {
            total: self.total,
            passed: self.passed,
            failed: self.failed,
            skipped: self.skipped,
            timeout: self.timeout,
            crashed: self.crashed,
            pass_rate,
            by_feature: self.by_feature,
            failures: self.failures,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_report_generation() {
        let results = vec![
            TestResult {
                path: "test1.js".to_string(),
                mode: ExecutionMode::Strict,
                outcome: TestOutcome::Pass,
                duration_ms: 10,
                error: None,
                features: vec!["arrow-function".to_string()],
            },
            TestResult {
                path: "test2.js".to_string(),
                mode: ExecutionMode::NonStrict,
                outcome: TestOutcome::Fail,
                duration_ms: 10,
                error: Some("Error".to_string()),
                features: vec!["arrow-function".to_string()],
            },
            TestResult {
                path: "test3.js".to_string(),
                mode: ExecutionMode::NonStrict,
                outcome: TestOutcome::Skip,
                duration_ms: 1,
                error: None,
                features: vec!["BigInt".to_string()],
            },
        ];

        let report = TestReport::from_results(&results);

        assert_eq!(report.total, 3);
        assert_eq!(report.passed, 1);
        assert_eq!(report.failed, 1);
        assert_eq!(report.skipped, 1);
        assert_eq!(report.pass_rate, 50.0); // 1/2 (excluding skipped)
    }
}
