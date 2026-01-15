//! node:test implementation
//!
//! Provides a test runner with `describe`, `it`, and `test` functions
//! compatible with Node.js's `node:test` module.

use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};

/// Test result for a single test case.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestResult {
    /// Full name of the test (including suite path).
    pub name: String,
    /// Whether the test passed.
    pub passed: bool,
    /// Duration in milliseconds.
    pub duration_ms: u64,
    /// Error message if test failed.
    pub error: Option<String>,
    /// Whether the test was skipped.
    pub skipped: bool,
}

/// Test runner state that tracks test execution.
#[derive(Debug, Default)]
pub struct TestRunner {
    /// All recorded test results.
    pub results: Vec<TestResult>,
    /// Current suite path for nested describes.
    pub current_suite: Vec<String>,
    /// Count of passed tests.
    pub passed: u32,
    /// Count of failed tests.
    pub failed: u32,
    /// Count of skipped tests.
    pub skipped: u32,
}

impl TestRunner {
    /// Create a new test runner.
    pub fn new() -> Self {
        Self::default()
    }

    /// Start a new test suite.
    pub fn start_suite(&mut self, name: &str) {
        self.current_suite.push(name.to_string());
    }

    /// End the current test suite.
    pub fn end_suite(&mut self) {
        self.current_suite.pop();
    }

    /// Get the full test name including suite path.
    pub fn full_test_name(&self, name: &str) -> String {
        if self.current_suite.is_empty() {
            name.to_string()
        } else {
            format!("{} > {}", self.current_suite.join(" > "), name)
        }
    }

    /// Record a test result.
    pub fn record_test(
        &mut self,
        name: &str,
        passed: bool,
        duration_ms: u64,
        error: Option<String>,
    ) {
        if passed {
            self.passed += 1;
        } else {
            self.failed += 1;
        }

        let full_name = self.full_test_name(name);

        self.results.push(TestResult {
            name: full_name,
            passed,
            duration_ms,
            error,
            skipped: false,
        });
    }

    /// Record a skipped test.
    pub fn skip_test(&mut self, name: &str) {
        self.skipped += 1;

        let full_name = self.full_test_name(name);

        self.results.push(TestResult {
            name: full_name,
            passed: true,
            duration_ms: 0,
            error: None,
            skipped: true,
        });
    }

    /// Get the total number of tests.
    pub fn total(&self) -> u32 {
        self.passed + self.failed + self.skipped
    }

    /// Check if all tests passed.
    pub fn all_passed(&self) -> bool {
        self.failed == 0
    }

    /// Get a summary string.
    pub fn summary(&self) -> String {
        let mut lines = vec![String::new(), "Test Results:".to_string()];

        if self.passed > 0 {
            lines.push(format!("  ✓ {} passed", self.passed));
        }
        if self.failed > 0 {
            lines.push(format!("  ✗ {} failed", self.failed));
        }
        if self.skipped > 0 {
            lines.push(format!("  ⊘ {} skipped", self.skipped));
        }

        lines.push(format!("  {} total", self.total()));
        lines.push(String::new());

        lines.join("\n")
    }

    /// Reset the test runner for a new run.
    pub fn reset(&mut self) {
        self.results.clear();
        self.current_suite.clear();
        self.passed = 0;
        self.failed = 0;
        self.skipped = 0;
    }
}

/// Summary of test execution results.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestSummary {
    /// Number of passed tests.
    pub passed: u32,
    /// Number of failed tests.
    pub failed: u32,
    /// Number of skipped tests.
    pub skipped: u32,
    /// Total number of tests.
    pub total: u32,
    /// All test results.
    pub results: Vec<TestResult>,
}

impl From<&TestRunner> for TestSummary {
    fn from(runner: &TestRunner) -> Self {
        TestSummary {
            passed: runner.passed,
            failed: runner.failed,
            skipped: runner.skipped,
            total: runner.total(),
            results: runner.results.clone(),
        }
    }
}

/// Thread-safe test runner handle.
pub type TestRunnerHandle = Arc<Mutex<TestRunner>>;

/// Create a new thread-safe test runner handle.
pub fn create_test_runner() -> TestRunnerHandle {
    Arc::new(Mutex::new(TestRunner::new()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_runner_new() {
        let runner = TestRunner::new();
        assert_eq!(runner.passed, 0);
        assert_eq!(runner.failed, 0);
        assert_eq!(runner.skipped, 0);
        assert!(runner.results.is_empty());
        assert!(runner.current_suite.is_empty());
    }

    #[test]
    fn test_runner_record_passed_test() {
        let mut runner = TestRunner::new();
        runner.record_test("my test", true, 10, None);

        assert_eq!(runner.passed, 1);
        assert_eq!(runner.failed, 0);
        assert_eq!(runner.results.len(), 1);
        assert_eq!(runner.results[0].name, "my test");
        assert!(runner.results[0].passed);
        assert_eq!(runner.results[0].duration_ms, 10);
        assert!(runner.results[0].error.is_none());
    }

    #[test]
    fn test_runner_record_failed_test() {
        let mut runner = TestRunner::new();
        runner.record_test("failing test", false, 5, Some("expected 1, got 2".into()));

        assert_eq!(runner.passed, 0);
        assert_eq!(runner.failed, 1);
        assert_eq!(runner.results.len(), 1);
        assert!(!runner.results[0].passed);
        assert_eq!(runner.results[0].error, Some("expected 1, got 2".into()));
    }

    #[test]
    fn test_runner_skip_test() {
        let mut runner = TestRunner::new();
        runner.skip_test("skipped test");

        assert_eq!(runner.passed, 0);
        assert_eq!(runner.failed, 0);
        assert_eq!(runner.skipped, 1);
        assert!(runner.results[0].skipped);
    }

    #[test]
    fn test_runner_with_suites() {
        let mut runner = TestRunner::new();

        runner.start_suite("Math");
        runner.record_test("adds numbers", true, 5, None);

        runner.start_suite("subtract");
        runner.record_test("subtracts numbers", true, 3, None);
        runner.end_suite();

        runner.record_test("multiplies", true, 2, None);
        runner.end_suite();

        assert_eq!(runner.passed, 3);
        assert_eq!(runner.results[0].name, "Math > adds numbers");
        assert_eq!(
            runner.results[1].name,
            "Math > subtract > subtracts numbers"
        );
        assert_eq!(runner.results[2].name, "Math > multiplies");
    }

    #[test]
    fn test_runner_summary() {
        let mut runner = TestRunner::new();
        runner.record_test("test1", true, 5, None);
        runner.record_test("test2", false, 3, Some("error".into()));
        runner.skip_test("test3");

        let summary = runner.summary();
        assert!(summary.contains("1 passed"));
        assert!(summary.contains("1 failed"));
        assert!(summary.contains("1 skipped"));
        assert!(summary.contains("3 total"));
    }

    #[test]
    fn test_runner_all_passed() {
        let mut runner = TestRunner::new();
        runner.record_test("test1", true, 5, None);
        assert!(runner.all_passed());

        runner.record_test("test2", false, 3, None);
        assert!(!runner.all_passed());
    }

    #[test]
    fn test_runner_reset() {
        let mut runner = TestRunner::new();
        runner.start_suite("Suite");
        runner.record_test("test", true, 5, None);

        runner.reset();

        assert_eq!(runner.passed, 0);
        assert_eq!(runner.failed, 0);
        assert_eq!(runner.skipped, 0);
        assert!(runner.results.is_empty());
        assert!(runner.current_suite.is_empty());
    }

    #[test]
    fn test_summary_from_runner() {
        let mut runner = TestRunner::new();
        runner.record_test("test1", true, 5, None);
        runner.record_test("test2", false, 3, Some("error".into()));

        let summary = TestSummary::from(&runner);
        assert_eq!(summary.passed, 1);
        assert_eq!(summary.failed, 1);
        assert_eq!(summary.total, 2);
        assert_eq!(summary.results.len(), 2);
    }
}
