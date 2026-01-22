//! Test result reporting

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::runner::{TestOutcome, TestResult};

/// Test run report
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestReport {
    /// Total number of tests
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
    /// Pass rate as percentage
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
    /// Test path
    pub path: String,
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
        println!("\n=== Test262 Results ===");
        println!("Total:   {}", self.total);
        println!("Passed:  {} ({:.1}%)", self.passed, self.pass_rate);
        println!("Failed:  {}", self.failed);
        println!("Skipped: {}", self.skipped);
        println!("Timeout: {}", self.timeout);
        println!("Crashed: {}", self.crashed);

        if !self.failures.is_empty() {
            println!("\n=== Failures (first 10) ===");
            for failure in self.failures.iter().take(10) {
                println!("  {} - {}", failure.path, failure.error);
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn test_report_generation() {
        let results = vec![
            TestResult {
                path: "test1.js".to_string(),
                outcome: TestOutcome::Pass,
                duration: Duration::from_millis(10),
                error: None,
                features: vec!["arrow-function".to_string()],
            },
            TestResult {
                path: "test2.js".to_string(),
                outcome: TestOutcome::Fail,
                duration: Duration::from_millis(10),
                error: Some("Error".to_string()),
                features: vec!["arrow-function".to_string()],
            },
            TestResult {
                path: "test3.js".to_string(),
                outcome: TestOutcome::Skip,
                duration: Duration::from_millis(1),
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
