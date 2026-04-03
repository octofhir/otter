//! Shared result types for the new Test262 runner.

use serde::{Deserialize, Serialize};
use std::time::Duration;

use crate::metadata::ExecutionMode;

/// Result of running a single test in one execution mode.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestResult {
    /// Test file path (relative to test dir)
    pub path: String,
    /// Execution mode this result is from
    pub mode: ExecutionMode,
    /// Test outcome
    pub outcome: TestOutcome,
    /// Execution time in milliseconds
    pub duration_ms: u64,
    /// Error message if failed
    pub error: Option<String>,
    /// Features used by test
    pub features: Vec<String>,
}

impl TestResult {
    /// Get the duration as a `Duration`.
    pub fn duration(&self) -> Duration {
        Duration::from_millis(self.duration_ms)
    }

    /// Get a display path including the execution mode.
    pub fn display_path(&self) -> String {
        format!("{} ({})", self.path, self.mode)
    }
}

/// Test outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TestOutcome {
    /// Test passed
    Pass,
    /// Test failed
    Fail,
    /// Test was skipped
    Skip,
    /// Test timed out
    Timeout,
    /// Test crashed
    Crash,
}
