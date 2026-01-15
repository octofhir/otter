//! node:test implementation
//!
//! Provides a test runner with `describe`, `it`, and `test` functions
//! compatible with Node.js's `node:test` module.
//!
//! # Snapshot Testing
//!
//! The test runner includes snapshot testing capabilities similar to Jest:
//!
//! ```javascript
//! test('renders correctly', () => {
//!     const result = render(<Component />);
//!     expect(result).toMatchSnapshot();
//! });
//! ```
//!
//! Snapshots are stored in `__snapshots__/<test_file>.snap`.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
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

// ============================================================================
// Snapshot Testing
// ============================================================================

/// Result of a snapshot comparison.
#[derive(Debug, Clone)]
pub enum SnapshotResult {
    /// Snapshot matched.
    Match,
    /// Snapshot didn't match, with expected and actual values.
    Mismatch { expected: String, actual: String },
    /// New snapshot was created.
    New,
    /// Snapshot was updated.
    Updated,
}

/// Snapshot manager for storing and comparing test snapshots.
#[derive(Debug)]
pub struct SnapshotManager {
    /// Path to the snapshot file.
    snapshot_file: PathBuf,
    /// Snapshots loaded from file (key -> serialized value).
    snapshots: HashMap<String, String>,
    /// New/updated snapshots to write.
    pending: HashMap<String, String>,
    /// Whether we're in update mode.
    update_mode: bool,
    /// Counter for auto-generated snapshot names.
    counter: u32,
    /// Current test name for auto-naming.
    current_test: Option<String>,
    /// Statistics.
    stats: SnapshotStats,
}

/// Statistics about snapshot operations.
#[derive(Debug, Clone, Default)]
pub struct SnapshotStats {
    /// Number of snapshots that matched.
    pub matched: u32,
    /// Number of new snapshots created.
    pub added: u32,
    /// Number of snapshots updated.
    pub updated: u32,
    /// Number of snapshots that failed to match.
    pub failed: u32,
    /// Obsolete snapshots (in file but not used).
    pub obsolete: u32,
}

impl SnapshotManager {
    /// Create a new snapshot manager for a test file.
    pub fn new(test_file: &Path, update_mode: bool) -> Self {
        let snapshot_dir = test_file
            .parent()
            .unwrap_or(Path::new("."))
            .join("__snapshots__");

        let snapshot_file = snapshot_dir.join(format!(
            "{}.snap",
            test_file
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("test")
        ));

        let snapshots = Self::load_snapshots(&snapshot_file);

        Self {
            snapshot_file,
            snapshots,
            pending: HashMap::new(),
            update_mode,
            counter: 0,
            current_test: None,
            stats: SnapshotStats::default(),
        }
    }

    /// Load snapshots from a file.
    fn load_snapshots(path: &Path) -> HashMap<String, String> {
        let mut snapshots = HashMap::new();

        if let Ok(content) = std::fs::read_to_string(path) {
            // Parse the snapshot file format:
            // exports[`snapshot name`] = `
            // snapshot content
            // `;
            let mut current_key: Option<String> = None;
            let mut current_value = String::new();
            let mut in_value = false;

            for line in content.lines() {
                if let Some(key) = Self::parse_snapshot_header(line) {
                    if let Some(prev_key) = current_key.take() {
                        // Remove trailing newline and backtick
                        if current_value.ends_with("\n`") {
                            current_value.truncate(current_value.len() - 2);
                        } else if current_value.ends_with('`') {
                            current_value.truncate(current_value.len() - 1);
                        }
                        snapshots.insert(prev_key, current_value);
                        current_value = String::new();
                    }
                    current_key = Some(key);
                    in_value = false;
                } else if current_key.is_some() {
                    if !in_value && line.trim().is_empty() {
                        continue;
                    }
                    in_value = true;
                    if !current_value.is_empty() {
                        current_value.push('\n');
                    }
                    current_value.push_str(line);
                }
            }

            // Handle last snapshot
            if let Some(key) = current_key {
                if current_value.ends_with("\n`") {
                    current_value.truncate(current_value.len() - 2);
                } else if current_value.ends_with('`') {
                    current_value.truncate(current_value.len() - 1);
                }
                // Remove leading backtick if present
                if current_value.starts_with('`') {
                    current_value = current_value[1..].to_string();
                }
                snapshots.insert(key, current_value);
            }
        }

        snapshots
    }

    /// Parse a snapshot header line.
    fn parse_snapshot_header(line: &str) -> Option<String> {
        // Format: exports[`snapshot name`] = `
        let trimmed = line.trim();
        if trimmed.starts_with("exports[`") && trimmed.ends_with("`] = `") {
            let key = &trimmed[9..trimmed.len() - 6];
            return Some(key.to_string());
        }
        None
    }

    /// Set the current test name for auto-naming snapshots.
    pub fn set_current_test(&mut self, name: &str) {
        self.current_test = Some(name.to_string());
        self.counter = 0;
    }

    /// Generate a snapshot key.
    fn generate_key(&mut self, name: Option<&str>) -> String {
        match name {
            Some(n) => n.to_string(),
            None => {
                self.counter += 1;
                let test_name = self.current_test.as_deref().unwrap_or("unknown");
                format!("{} {}", test_name, self.counter)
            }
        }
    }

    /// Match a value against a snapshot.
    pub fn match_snapshot(&mut self, value: &str, name: Option<&str>) -> SnapshotResult {
        let key = self.generate_key(name);

        // Normalize the value (trim trailing whitespace on each line)
        let normalized = Self::normalize_snapshot(value);

        if let Some(expected) = self.snapshots.get(&key) {
            let expected_normalized = Self::normalize_snapshot(expected);

            if normalized == expected_normalized {
                self.stats.matched += 1;
                SnapshotResult::Match
            } else if self.update_mode {
                self.pending.insert(key, normalized);
                self.stats.updated += 1;
                SnapshotResult::Updated
            } else {
                self.stats.failed += 1;
                SnapshotResult::Mismatch {
                    expected: expected_normalized,
                    actual: normalized,
                }
            }
        } else {
            // New snapshot
            self.pending.insert(key, normalized);
            self.stats.added += 1;
            SnapshotResult::New
        }
    }

    /// Normalize a snapshot value for comparison.
    fn normalize_snapshot(value: &str) -> String {
        value
            .lines()
            .map(|line| line.trim_end())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Save pending snapshots to file.
    pub fn save(&self) -> std::io::Result<()> {
        if self.pending.is_empty() && self.snapshots.is_empty() {
            return Ok(());
        }

        // Merge existing and pending snapshots
        let mut all_snapshots = self.snapshots.clone();
        for (key, value) in &self.pending {
            all_snapshots.insert(key.clone(), value.clone());
        }

        if all_snapshots.is_empty() {
            return Ok(());
        }

        // Create snapshot directory
        if let Some(parent) = self.snapshot_file.parent() {
            std::fs::create_dir_all(parent)?;
        }

        // Build snapshot file content
        let mut content = String::new();
        content.push_str("// Jest Snapshot v1, https://goo.gl/fbAQLP\n\n");

        // Sort keys for deterministic output
        let mut keys: Vec<_> = all_snapshots.keys().collect();
        keys.sort();

        for key in keys {
            if let Some(value) = all_snapshots.get(key) {
                content.push_str(&format!("exports[`{}`] = `\n{}\n`;\n\n", key, value));
            }
        }

        std::fs::write(&self.snapshot_file, content)?;
        Ok(())
    }

    /// Get snapshot statistics.
    pub fn stats(&self) -> &SnapshotStats {
        &self.stats
    }

    /// Check if there are pending changes.
    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Get the snapshot file path.
    pub fn snapshot_file(&self) -> &Path {
        &self.snapshot_file
    }
}

/// Generate a diff between two strings for display.
pub fn diff_snapshots(expected: &str, actual: &str) -> String {
    let mut result = String::new();

    let expected_lines: Vec<&str> = expected.lines().collect();
    let actual_lines: Vec<&str> = actual.lines().collect();

    result.push_str("\n  Snapshot diff:\n");
    result.push_str("  \x1b[31m- Expected\x1b[0m\n");
    result.push_str("  \x1b[32m+ Received\x1b[0m\n\n");

    // Simple line-by-line diff
    let max_lines = expected_lines.len().max(actual_lines.len());

    for i in 0..max_lines {
        let exp = expected_lines.get(i).copied();
        let act = actual_lines.get(i).copied();

        match (exp, act) {
            (Some(e), Some(a)) if e == a => {
                result.push_str(&format!("    {}\n", e));
            }
            (Some(e), Some(a)) => {
                result.push_str(&format!("  \x1b[31m- {}\x1b[0m\n", e));
                result.push_str(&format!("  \x1b[32m+ {}\x1b[0m\n", a));
            }
            (Some(e), None) => {
                result.push_str(&format!("  \x1b[31m- {}\x1b[0m\n", e));
            }
            (None, Some(a)) => {
                result.push_str(&format!("  \x1b[32m+ {}\x1b[0m\n", a));
            }
            (None, None) => {}
        }
    }

    result
}

/// Thread-safe snapshot manager handle.
pub type SnapshotManagerHandle = Arc<Mutex<SnapshotManager>>;

// ============================================================================
// Mocking & Spying
// ============================================================================

/// A call record for a mock function.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MockCall {
    /// Arguments passed to the call (serialized as JSON).
    pub args: Vec<serde_json::Value>,
    /// Return value (if captured).
    pub return_value: Option<serde_json::Value>,
    /// Error thrown (if any).
    pub error: Option<String>,
    /// Timestamp of the call.
    pub timestamp_ms: u64,
}

/// Configuration for mock behavior.
#[derive(Debug, Clone, Default)]
pub struct MockBehavior {
    /// Values to return on successive calls.
    pub return_values: Vec<serde_json::Value>,
    /// Value to always return.
    pub return_value: Option<serde_json::Value>,
    /// Implementation function (as JavaScript code).
    pub implementation: Option<String>,
    /// Whether to call the original implementation.
    pub call_through: bool,
    /// Whether the mock should throw an error.
    pub throws: Option<String>,
    /// Value to resolve with (for async mocks).
    pub resolves: Option<serde_json::Value>,
    /// Value to reject with (for async mocks).
    pub rejects: Option<String>,
}

/// A mock function that tracks calls and can be configured.
#[derive(Debug, Default)]
pub struct MockFn {
    /// All recorded calls.
    pub calls: Vec<MockCall>,
    /// Mock behavior configuration.
    pub behavior: MockBehavior,
    /// Original implementation (for spies).
    pub original: Option<String>,
    /// Name of the mock (for debugging).
    pub name: Option<String>,
}

impl MockFn {
    /// Create a new mock function.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a named mock function.
    pub fn with_name(name: &str) -> Self {
        Self {
            name: Some(name.to_string()),
            ..Default::default()
        }
    }

    /// Record a call to this mock.
    pub fn record_call(&mut self, args: Vec<serde_json::Value>) {
        self.calls.push(MockCall {
            args,
            return_value: None,
            error: None,
            timestamp_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
        });
    }

    /// Set return value for mock calls.
    pub fn returns(&mut self, value: serde_json::Value) {
        self.behavior.return_value = Some(value);
    }

    /// Set return values for successive calls.
    pub fn returns_once(&mut self, values: Vec<serde_json::Value>) {
        self.behavior.return_values = values;
    }

    /// Set implementation.
    pub fn implementation(&mut self, code: String) {
        self.behavior.implementation = Some(code);
    }

    /// Make the mock throw an error.
    pub fn throws(&mut self, error: String) {
        self.behavior.throws = Some(error);
    }

    /// Make the mock resolve with a value (async).
    pub fn resolves(&mut self, value: serde_json::Value) {
        self.behavior.resolves = Some(value);
    }

    /// Make the mock reject with an error (async).
    pub fn rejects(&mut self, error: String) {
        self.behavior.rejects = Some(error);
    }

    /// Get number of times called.
    pub fn call_count(&self) -> usize {
        self.calls.len()
    }

    /// Check if mock was called.
    pub fn was_called(&self) -> bool {
        !self.calls.is_empty()
    }

    /// Check if mock was called exactly n times.
    pub fn was_called_times(&self, n: usize) -> bool {
        self.calls.len() == n
    }

    /// Check if mock was called with specific arguments.
    pub fn was_called_with(&self, args: &[serde_json::Value]) -> bool {
        self.calls.iter().any(|call| call.args == args)
    }

    /// Get the last call's arguments.
    pub fn last_call_args(&self) -> Option<&[serde_json::Value]> {
        self.calls.last().map(|call| call.args.as_slice())
    }

    /// Get all calls' arguments.
    pub fn all_calls(&self) -> &[MockCall] {
        &self.calls
    }

    /// Reset the mock (clear calls but keep behavior).
    pub fn reset_calls(&mut self) {
        self.calls.clear();
    }

    /// Reset everything (calls and behavior).
    pub fn reset(&mut self) {
        self.calls.clear();
        self.behavior = MockBehavior::default();
    }

    /// Restore original implementation (for spies).
    pub fn restore(&mut self) {
        self.behavior = MockBehavior::default();
        self.behavior.call_through = true;
    }

    /// Get the next return value based on configuration.
    pub fn get_return_value(&mut self) -> Option<serde_json::Value> {
        // First check for successive return values
        if !self.behavior.return_values.is_empty() {
            return Some(self.behavior.return_values.remove(0));
        }

        // Then check for constant return value
        self.behavior.return_value.clone()
    }
}

/// Manager for mock functions.
#[derive(Debug, Default)]
pub struct MockManager {
    /// All created mocks by ID.
    mocks: HashMap<u32, MockFn>,
    /// Next mock ID.
    next_id: u32,
}

impl MockManager {
    /// Create a new mock manager.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a new mock function.
    pub fn create_mock(&mut self, name: Option<&str>) -> u32 {
        let id = self.next_id;
        self.next_id += 1;

        let mock = match name {
            Some(n) => MockFn::with_name(n),
            None => MockFn::new(),
        };

        self.mocks.insert(id, mock);
        id
    }

    /// Create a spy on an object method.
    pub fn spy_on(&mut self, original_impl: &str, name: &str) -> u32 {
        let id = self.next_id;
        self.next_id += 1;

        let mut mock = MockFn::with_name(name);
        mock.original = Some(original_impl.to_string());
        mock.behavior.call_through = true;

        self.mocks.insert(id, mock);
        id
    }

    /// Get a mock by ID.
    pub fn get(&self, id: u32) -> Option<&MockFn> {
        self.mocks.get(&id)
    }

    /// Get a mutable mock by ID.
    pub fn get_mut(&mut self, id: u32) -> Option<&mut MockFn> {
        self.mocks.get_mut(&id)
    }

    /// Record a call to a mock.
    pub fn record_call(&mut self, id: u32, args: Vec<serde_json::Value>) {
        if let Some(mock) = self.mocks.get_mut(&id) {
            mock.record_call(args);
        }
    }

    /// Clear all mocks.
    pub fn clear_all(&mut self) {
        self.mocks.clear();
        self.next_id = 0;
    }

    /// Reset all mocks (clear calls but keep configuration).
    pub fn reset_all(&mut self) {
        for mock in self.mocks.values_mut() {
            mock.reset_calls();
        }
    }

    /// Restore all spies.
    pub fn restore_all(&mut self) {
        for mock in self.mocks.values_mut() {
            if mock.original.is_some() {
                mock.restore();
            }
        }
    }

    /// Remove a mock.
    pub fn remove(&mut self, id: u32) {
        self.mocks.remove(&id);
    }
}

/// Thread-safe mock manager handle.
pub type MockManagerHandle = Arc<Mutex<MockManager>>;

/// Assertion helpers for mocks.
pub mod mock_assertions {
    use super::*;

    /// Assert mock was called.
    pub fn assert_called(mock: &MockFn) -> Result<(), String> {
        if mock.was_called() {
            Ok(())
        } else {
            Err(format!(
                "Expected mock {} to have been called",
                mock.name.as_deref().unwrap_or("(anonymous)")
            ))
        }
    }

    /// Assert mock was called exactly n times.
    pub fn assert_called_times(mock: &MockFn, n: usize) -> Result<(), String> {
        if mock.was_called_times(n) {
            Ok(())
        } else {
            Err(format!(
                "Expected mock {} to have been called {} times, but was called {} times",
                mock.name.as_deref().unwrap_or("(anonymous)"),
                n,
                mock.call_count()
            ))
        }
    }

    /// Assert mock was called with specific arguments.
    pub fn assert_called_with(mock: &MockFn, args: &[serde_json::Value]) -> Result<(), String> {
        if mock.was_called_with(args) {
            Ok(())
        } else {
            Err(format!(
                "Expected mock {} to have been called with {:?}",
                mock.name.as_deref().unwrap_or("(anonymous)"),
                args
            ))
        }
    }

    /// Assert mock was not called.
    pub fn assert_not_called(mock: &MockFn) -> Result<(), String> {
        if !mock.was_called() {
            Ok(())
        } else {
            Err(format!(
                "Expected mock {} to not have been called, but was called {} times",
                mock.name.as_deref().unwrap_or("(anonymous)"),
                mock.call_count()
            ))
        }
    }

    /// Assert mock was last called with specific arguments.
    pub fn assert_last_called_with(
        mock: &MockFn,
        args: &[serde_json::Value],
    ) -> Result<(), String> {
        if let Some(last_args) = mock.last_call_args() {
            if last_args == args {
                return Ok(());
            }
            Err(format!(
                "Expected last call to {} with {:?}, got {:?}",
                mock.name.as_deref().unwrap_or("(anonymous)"),
                args,
                last_args
            ))
        } else {
            Err(format!(
                "Expected mock {} to have been called",
                mock.name.as_deref().unwrap_or("(anonymous)")
            ))
        }
    }
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

    // Snapshot testing tests

    #[test]
    fn test_snapshot_manager_new() {
        let test_file = Path::new("/tmp/test_file.ts");
        let manager = SnapshotManager::new(test_file, false);

        assert_eq!(
            manager.snapshot_file(),
            Path::new("/tmp/__snapshots__/test_file.snap")
        );
        assert!(!manager.update_mode);
    }

    #[test]
    fn test_snapshot_manager_match_new() {
        let test_file = Path::new("/nonexistent/test.ts");
        let mut manager = SnapshotManager::new(test_file, false);

        manager.set_current_test("my test");
        let result = manager.match_snapshot("hello world", None);

        assert!(matches!(result, SnapshotResult::New));
        assert_eq!(manager.stats().added, 1);
        assert!(manager.has_pending());
    }

    #[test]
    fn test_snapshot_manager_auto_naming() {
        let test_file = Path::new("/tmp/test.ts");
        let mut manager = SnapshotManager::new(test_file, false);

        manager.set_current_test("renders component");

        // First snapshot
        manager.match_snapshot("value1", None);
        // Second snapshot
        manager.match_snapshot("value2", None);

        assert_eq!(manager.stats().added, 2);
    }

    #[test]
    fn test_snapshot_manager_explicit_naming() {
        let test_file = Path::new("/tmp/test.ts");
        let mut manager = SnapshotManager::new(test_file, false);

        manager.match_snapshot("value", Some("custom name"));
        assert_eq!(manager.stats().added, 1);
    }

    #[test]
    fn test_snapshot_normalize() {
        let input = "hello  \nworld\t\nfoo";
        let normalized = SnapshotManager::normalize_snapshot(input);
        assert_eq!(normalized, "hello\nworld\nfoo");
    }

    #[test]
    fn test_snapshot_parse_header() {
        assert_eq!(
            SnapshotManager::parse_snapshot_header("exports[`my test 1`] = `"),
            Some("my test 1".to_string())
        );
        assert_eq!(
            SnapshotManager::parse_snapshot_header("some other line"),
            None
        );
    }

    #[test]
    fn test_diff_snapshots() {
        let expected = "line1\nline2\nline3";
        let actual = "line1\nmodified\nline3";

        let diff = diff_snapshots(expected, actual);

        assert!(diff.contains("Expected"));
        assert!(diff.contains("Received"));
        assert!(diff.contains("line2"));
        assert!(diff.contains("modified"));
    }

    #[test]
    fn test_snapshot_stats_default() {
        let stats = SnapshotStats::default();
        assert_eq!(stats.matched, 0);
        assert_eq!(stats.added, 0);
        assert_eq!(stats.updated, 0);
        assert_eq!(stats.failed, 0);
        assert_eq!(stats.obsolete, 0);
    }

    // Mock/spy tests

    #[test]
    fn test_mock_fn_new() {
        let mock = MockFn::new();
        assert!(!mock.was_called());
        assert_eq!(mock.call_count(), 0);
        assert!(mock.name.is_none());
    }

    #[test]
    fn test_mock_fn_with_name() {
        let mock = MockFn::with_name("myMock");
        assert_eq!(mock.name, Some("myMock".to_string()));
    }

    #[test]
    fn test_mock_fn_record_call() {
        let mut mock = MockFn::new();

        mock.record_call(vec![serde_json::json!(1), serde_json::json!("hello")]);

        assert!(mock.was_called());
        assert_eq!(mock.call_count(), 1);
        assert_eq!(mock.calls[0].args.len(), 2);
    }

    #[test]
    fn test_mock_fn_was_called_times() {
        let mut mock = MockFn::new();

        assert!(mock.was_called_times(0));

        mock.record_call(vec![]);
        assert!(mock.was_called_times(1));

        mock.record_call(vec![]);
        assert!(mock.was_called_times(2));
        assert!(!mock.was_called_times(1));
    }

    #[test]
    fn test_mock_fn_was_called_with() {
        let mut mock = MockFn::new();

        mock.record_call(vec![serde_json::json!(1), serde_json::json!(2)]);
        mock.record_call(vec![serde_json::json!("a"), serde_json::json!("b")]);

        assert!(mock.was_called_with(&[serde_json::json!(1), serde_json::json!(2)]));
        assert!(mock.was_called_with(&[serde_json::json!("a"), serde_json::json!("b")]));
        assert!(!mock.was_called_with(&[serde_json::json!(3)]));
    }

    #[test]
    fn test_mock_fn_last_call_args() {
        let mut mock = MockFn::new();

        assert!(mock.last_call_args().is_none());

        mock.record_call(vec![serde_json::json!(1)]);
        mock.record_call(vec![serde_json::json!(2)]);

        let last = mock.last_call_args().unwrap();
        assert_eq!(last, &[serde_json::json!(2)]);
    }

    #[test]
    fn test_mock_fn_returns() {
        let mut mock = MockFn::new();
        mock.returns(serde_json::json!(42));

        assert_eq!(mock.get_return_value(), Some(serde_json::json!(42)));
        assert_eq!(mock.get_return_value(), Some(serde_json::json!(42)));
    }

    #[test]
    fn test_mock_fn_returns_once() {
        let mut mock = MockFn::new();
        mock.returns_once(vec![
            serde_json::json!(1),
            serde_json::json!(2),
            serde_json::json!(3),
        ]);

        assert_eq!(mock.get_return_value(), Some(serde_json::json!(1)));
        assert_eq!(mock.get_return_value(), Some(serde_json::json!(2)));
        assert_eq!(mock.get_return_value(), Some(serde_json::json!(3)));
        assert_eq!(mock.get_return_value(), None);
    }

    #[test]
    fn test_mock_fn_reset_calls() {
        let mut mock = MockFn::new();
        mock.returns(serde_json::json!(42));
        mock.record_call(vec![]);
        mock.record_call(vec![]);

        mock.reset_calls();

        assert!(!mock.was_called());
        // Behavior is preserved
        assert_eq!(mock.get_return_value(), Some(serde_json::json!(42)));
    }

    #[test]
    fn test_mock_fn_reset() {
        let mut mock = MockFn::new();
        mock.returns(serde_json::json!(42));
        mock.record_call(vec![]);

        mock.reset();

        assert!(!mock.was_called());
        assert_eq!(mock.get_return_value(), None);
    }

    #[test]
    fn test_mock_manager_create_mock() {
        let mut manager = MockManager::new();

        let id1 = manager.create_mock(Some("mock1"));
        let id2 = manager.create_mock(None);

        assert_ne!(id1, id2);
        assert!(manager.get(id1).is_some());
        assert!(manager.get(id2).is_some());
        assert_eq!(manager.get(id1).unwrap().name, Some("mock1".to_string()));
    }

    #[test]
    fn test_mock_manager_spy_on() {
        let mut manager = MockManager::new();

        let id = manager.spy_on("function original() {}", "myMethod");

        let mock = manager.get(id).unwrap();
        assert!(mock.original.is_some());
        assert!(mock.behavior.call_through);
    }

    #[test]
    fn test_mock_manager_record_call() {
        let mut manager = MockManager::new();
        let id = manager.create_mock(None);

        manager.record_call(id, vec![serde_json::json!("arg1")]);

        assert!(manager.get(id).unwrap().was_called());
    }

    #[test]
    fn test_mock_manager_clear_all() {
        let mut manager = MockManager::new();
        manager.create_mock(None);
        manager.create_mock(None);

        manager.clear_all();

        assert!(manager.get(0).is_none());
        assert!(manager.get(1).is_none());
    }

    #[test]
    fn test_mock_manager_reset_all() {
        let mut manager = MockManager::new();
        let id1 = manager.create_mock(None);
        let id2 = manager.create_mock(None);

        manager.record_call(id1, vec![]);
        manager.record_call(id2, vec![]);

        manager.reset_all();

        assert!(!manager.get(id1).unwrap().was_called());
        assert!(!manager.get(id2).unwrap().was_called());
    }

    #[test]
    fn test_mock_assertions() {
        use mock_assertions::*;

        let mut mock = MockFn::with_name("testMock");

        // Not called yet
        assert!(assert_not_called(&mock).is_ok());
        assert!(assert_called(&mock).is_err());

        // After a call
        mock.record_call(vec![serde_json::json!(1)]);

        assert!(assert_called(&mock).is_ok());
        assert!(assert_not_called(&mock).is_err());
        assert!(assert_called_times(&mock, 1).is_ok());
        assert!(assert_called_with(&mock, &[serde_json::json!(1)]).is_ok());
        assert!(assert_last_called_with(&mock, &[serde_json::json!(1)]).is_ok());
    }
}
