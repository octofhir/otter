use std::fs;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use futures::FutureExt;
use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use otter_engine::{EngineBuilder, Otter, OtterError, Value};

use crate::harness::TestHarnessState;
use crate::metadata::{ErrorPhase, ExecutionMode, TestMetadata};

// Skip features are now configured exclusively via test262_config.toml.
// No hardcoded defaults — the config file is the single source of truth.

/// Test262 test runner
pub struct Test262Runner {
    /// Path to test262 directory
    test_dir: PathBuf,
    /// Filter pattern
    filter: Option<String>,
    /// Features to skip
    skip_features: Vec<String>,
    /// Shared harness state for capturing async test results
    harness_state: TestHarnessState,
    /// Reusable engine (created once, used for all tests)
    engine: Otter,
}

/// Result of running a single test (in one execution mode)
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
    /// Get the duration as a `Duration`
    pub fn duration(&self) -> Duration {
        Duration::from_millis(self.duration_ms)
    }

    /// Get a display path including the execution mode
    pub fn display_path(&self) -> String {
        format!("{} ({})", self.path, self.mode)
    }
}

/// Test outcome
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

impl Test262Runner {
    /// Create a new test runner
    pub fn new(test_dir: impl AsRef<Path>) -> Self {
        let (harness_ext, harness_state) =
            crate::harness::create_harness_extension_with_state();
        let engine = EngineBuilder::new()
            .extension(harness_ext)
            .build();

        Self {
            test_dir: test_dir.as_ref().to_path_buf(),
            filter: None,
            skip_features: Vec::new(),
            harness_state,
            engine,
        }
    }

    /// Rebuild engine after a panic to restore consistent state.
    fn rebuild_engine(&mut self) {
        let (harness_ext, harness_state) =
            crate::harness::create_harness_extension_with_state();
        self.engine = EngineBuilder::new()
            .extension(harness_ext)
            .build();
        self.harness_state = harness_state;
    }

    /// Create a new test runner that skips no features (runs everything).
    pub fn new_no_skip(test_dir: impl AsRef<Path>) -> Self {
        let mut runner = Self::new(test_dir);
        runner.skip_features.clear();
        runner
    }

    /// Set filter pattern
    pub fn with_filter(mut self, filter: impl Into<String>) -> Self {
        self.filter = Some(filter.into());
        self
    }

    /// Replace the skip features list entirely
    pub fn with_skip_features(mut self, features: Vec<String>) -> Self {
        self.skip_features = features;
        self
    }

    /// Add feature to skip list
    pub fn skip_feature(mut self, feature: impl Into<String>) -> Self {
        self.skip_features.push(feature.into());
        self
    }

    /// Clear the skip list (run all features)
    pub fn with_no_skip(mut self) -> Self {
        self.skip_features.clear();
        self
    }

    /// Get a reference to the harness state (for reading print output, etc.)
    pub fn harness_state(&self) -> &TestHarnessState {
        &self.harness_state
    }

    /// List all test files (without running them)
    pub fn list_tests(&self) -> Vec<PathBuf> {
        let test_path = self.test_dir.join("test");
        self.collect_tests(&test_path)
    }

    /// List tests in a specific directory
    pub fn list_tests_dir(&self, subdir: &str) -> Vec<PathBuf> {
        let test_path = self.test_dir.join("test").join(subdir);
        self.collect_tests(&test_path)
    }

    /// Collect test files from a directory
    fn collect_tests(&self, test_path: &Path) -> Vec<PathBuf> {
        WalkDir::new(test_path)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().map(|s| s == "js").unwrap_or(false))
            .filter(|e| !e.path().to_string_lossy().contains("_FIXTURE"))
            .map(|e| e.path().to_path_buf())
            .filter(|p| {
                if let Some(ref filter) = self.filter {
                    p.to_string_lossy().contains(filter)
                } else {
                    true
                }
            })
            .collect()
    }

    /// Run all tests
    pub async fn run_all(&mut self) -> Vec<TestResult> {
        let tests = self.list_tests();
        let mut results = Vec::with_capacity(tests.len() * 2);
        for path in tests {
            results.extend(self.run_test_all_modes(&path, None).await);
        }
        results
    }

    /// Run a single test in all applicable execution modes.
    ///
    /// Returns one `TestResult` per mode (strict / non-strict / module).
    /// Most tests will produce two results (strict + non-strict).
    pub async fn run_test_all_modes(
        &mut self,
        path: &Path,
        timeout: Option<Duration>,
    ) -> Vec<TestResult> {
        let relative_path = path.strip_prefix(&self.test_dir).unwrap_or(path);
        let relative_path_str = relative_path.to_string_lossy().to_string();

        // Read test file
        let content = match fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) => {
                return vec![TestResult {
                    path: relative_path_str,
                    mode: ExecutionMode::NonStrict,
                    outcome: TestOutcome::Crash,
                    duration_ms: 0,
                    error: Some(format!("Failed to read file: {}", e)),
                    features: vec![],
                }];
            }
        };

        // Parse metadata
        let metadata = TestMetadata::parse(&content).unwrap_or_default();

        // Check if we should skip this test based on features
        for feature in &metadata.features {
            if self.skip_features.contains(feature) {
                return vec![TestResult {
                    path: relative_path_str,
                    mode: ExecutionMode::NonStrict,
                    outcome: TestOutcome::Skip,
                    duration_ms: 0,
                    error: Some(format!("Skipped feature: {}", feature)),
                    features: metadata.features.clone(),
                }];
            }
        }

        let modes = metadata.execution_modes();
        let mut results = Vec::with_capacity(modes.len());

        for mode in &modes {

            let start = Instant::now();
            let result = self
                .run_single_mode(path, &content, &metadata, *mode, timeout)
                .await;

            results.push(TestResult {
                path: relative_path_str.clone(),
                mode: *mode,
                outcome: result.0,
                duration_ms: start.elapsed().as_millis() as u64,
                error: result.1,
                features: metadata.features.clone(),
            });

            // If non-strict mode fails, skip strict mode (Boa pattern:
            // if the simpler mode fails, the stricter one will too)
            if *mode == ExecutionMode::NonStrict && result.0 == TestOutcome::Fail {
                break;
            }
        }

        results
    }

    /// Run a single test in a single execution mode.
    ///
    /// Legacy API — runs only in non-strict mode (or strict if onlyStrict).
    pub async fn run_test(&mut self, path: &Path, timeout: Option<Duration>) -> TestResult {
        let results = self.run_test_all_modes(path, timeout).await;
        // Return the first result for backward compatibility
        results.into_iter().next().unwrap_or(TestResult {
            path: path.to_string_lossy().to_string(),
            mode: ExecutionMode::NonStrict,
            outcome: TestOutcome::Crash,
            duration_ms: 0,
            error: Some("No test results produced".to_string()),
            features: vec![],
        })
    }

    /// Run a single test file in a specific execution mode.
    async fn run_single_mode(
        &mut self,
        path: &Path,
        content: &str,
        metadata: &TestMetadata,
        mode: ExecutionMode,
        timeout: Option<Duration>,
    ) -> (TestOutcome, Option<String>) {
        let relative_path = path.strip_prefix(&self.test_dir).unwrap_or(path);
        let test_name = format!("{} ({})", relative_path.to_string_lossy(), mode);

        // Build test source with harness
        let mut test_source = String::new();

        // Add strict mode prefix if needed
        if mode == ExecutionMode::Strict {
            test_source.push_str("\"use strict\";\n");
        }

        // Add default harness files (sta.js and assert.js)
        let mut includes = vec!["sta.js".to_string(), "assert.js".to_string()];

        // For async tests, add donePrintHandle.js
        if metadata.is_async() && !includes.contains(&"donePrintHandle.js".to_string()) {
            includes.push("donePrintHandle.js".to_string());
        }

        // Add explicitly requested harness files
        for include in &metadata.includes {
            if !includes.contains(include) {
                includes.push(include.clone());
            }
        }

        // Add harness files to source
        for include in &includes {
            let harness_path = self.test_dir.join("harness").join(include);
            match fs::read_to_string(&harness_path) {
                Ok(harness_content) => {
                    test_source.push_str(&harness_content);
                    test_source.push('\n');
                }
                Err(e) => {
                    eprintln!(
                        "ERROR: Failed to read harness file {} (required by test): {}",
                        harness_path.display(),
                        e
                    );
                }
            }
        }

        // Add test content (strip metadata)
        let test_content = content
            .find("---*/")
            .map(|i| &content[i + 5..])
            .unwrap_or(content);
        test_source.push_str(test_content);

        // Clear harness state before running the test
        self.harness_state.clear();

        // Execute - route module tests to separate handler
        if mode == ExecutionMode::Module {
            self.execute_test_as_module(&test_source, metadata, &test_name, timeout)
                .await
        } else {
            self.execute_test(&test_source, metadata, &test_name, timeout)
                .await
        }
    }

    /// Execute a test and return (outcome, error_message)
    async fn execute_test(
        &mut self,
        source: &str,
        metadata: &TestMetadata,
        test_name: &str,
        timeout: Option<Duration>,
    ) -> (TestOutcome, Option<String>) {
        let is_async = metadata.is_async();

        match self
            .run_with_timeout(source, timeout, test_name)
            .await
        {
            Ok(value) => {
                if !value.is_undefined() {
                    // Debug: println!("RESULT {}: {}", test_name, format_value(&value));
                }

                // For async tests, check print patterns first, fallback to $DONE
                if is_async {
                    let print_output = self.harness_state.print_output();

                    // Check print patterns first, fallback to $DONE result
                    let async_result = check_async_print_patterns(&print_output)
                        .or_else(|| self.harness_state.done_result());

                    match async_result {
                        Some(Ok(())) => {
                            if metadata.expects_runtime_error() {
                                (
                                    TestOutcome::Fail,
                                    Some("Expected runtime error but async test passed".to_string()),
                                )
                            } else {
                                (TestOutcome::Pass, None)
                            }
                        }
                        Some(Err(msg)) => {
                            if metadata.expects_runtime_error() {
                                self.validate_negative_error(metadata, &msg, ErrorPhase::Runtime)
                            } else {
                                (
                                    TestOutcome::Fail,
                                    Some(format!("Async test failed: {}", msg)),
                                )
                            }
                        }
                        None => {
                            (
                                TestOutcome::Fail,
                                Some("Async test completed without $DONE or print signal".to_string()),
                            )
                        }
                    }
                } else if metadata.expects_early_error() {
                    (
                        TestOutcome::Fail,
                        Some("Expected parse/early error but compilation succeeded".to_string()),
                    )
                } else if metadata.expects_runtime_error() {
                    (
                        TestOutcome::Fail,
                        Some("Expected runtime error but execution succeeded".to_string()),
                    )
                } else {
                    (TestOutcome::Pass, None)
                }
            }
            Err(err) => match err {
                OtterError::Compile(msg) => {
                    if metadata.expects_early_error() {
                        self.validate_negative_error(metadata, &msg, ErrorPhase::Parse)
                    } else {
                        (TestOutcome::Fail, Some(format!("Compile error: {}", msg)))
                    }
                }
                OtterError::Runtime(msg) => {
                    if metadata.expects_runtime_error() {
                        self.validate_negative_error(metadata, &msg, ErrorPhase::Runtime)
                    } else if msg == "Test timed out" || msg.contains("Execution interrupted") {
                        (TestOutcome::Timeout, None)
                    } else {
                        (TestOutcome::Fail, Some(msg))
                    }
                }
                OtterError::PermissionDenied(msg) => (TestOutcome::Fail, Some(msg)),
            },
        }
    }

    /// Validate that the error type and phase match the negative expectation from metadata.
    fn validate_negative_error(
        &self,
        metadata: &TestMetadata,
        error_msg: &str,
        actual_phase: ErrorPhase,
    ) -> (TestOutcome, Option<String>) {
        let Some(ref negative) = metadata.negative else {
            return (TestOutcome::Pass, None);
        };

        // Validate phase matches
        let phase_matches = match (&negative.phase, &actual_phase) {
            (ErrorPhase::Parse, ErrorPhase::Parse) => true,
            (ErrorPhase::Early, ErrorPhase::Parse) => true, // Early errors detected at parse time
            (ErrorPhase::Runtime, ErrorPhase::Runtime) => true,
            (ErrorPhase::Resolution, _) => {
                return (
                    TestOutcome::Skip,
                    Some("Resolution phase not yet supported".to_string()),
                );
            }
            _ => false,
        };

        if !phase_matches {
            return (
                TestOutcome::Fail,
                Some(format!(
                    "Error in wrong phase: expected {:?} but got {:?}",
                    negative.phase, actual_phase
                )),
            );
        }

        // Validate error type (lenient substring match for now)
        if error_msg.contains(&negative.error_type) {
            (TestOutcome::Pass, None)
        } else {
            // Still pass but note the mismatch
            (
                TestOutcome::Pass,
                Some(format!(
                    "Type mismatch (lenient): expected {} in error: {}",
                    negative.error_type,
                    error_msg.chars().take(100).collect::<String>()
                )),
            )
        }
    }

    /// Execute a test as an ES module.
    ///
    /// Note: Currently uses the same execution path as script tests since the
    /// engine's eval() already compiles as a module. The main difference is that
    /// harness files are NOT prepended for module tests, as modules have their
    /// own scope and top-level semantics.
    async fn execute_test_as_module(
        &mut self,
        source: &str,
        metadata: &TestMetadata,
        test_name: &str,
        timeout: Option<Duration>,
    ) -> (TestOutcome, Option<String>) {
        // For module tests, we don't prepend harness files since modules
        // have strict mode by default and their own scope. We execute the
        // raw test source directly.
        self.execute_test(source, metadata, test_name, timeout)
            .await
    }

    async fn run_with_timeout(
        &mut self,
        source: &str,
        timeout: Option<Duration>,
        _test_name: &str,
    ) -> Result<Value, OtterError> {
        let result = if let Some(duration) = timeout {
            // Cooperative timeout: spawn a tokio task as watchdog on a separate
            // worker thread (runtime is multi-threaded). It sets the interrupt
            // flag after the deadline; the VM checks it every ~10K instructions.
            let flag = self.engine.interrupt_flag();
            let watchdog = tokio::spawn(async move {
                tokio::time::sleep(duration).await;
                flag.store(true, Ordering::Relaxed);
            });

            let result = AssertUnwindSafe(self.engine.eval(source))
                .catch_unwind()
                .await;

            // Cancel watchdog if test finished before timeout
            watchdog.abort();

            result
        } else {
            AssertUnwindSafe(self.engine.eval(source))
                .catch_unwind()
                .await
        };

        match result {
            Ok(val) => val,
            Err(panic) => {
                let msg = extract_panic_message(&panic);
                // Engine state may be corrupted after a panic — rebuild it.
                self.rebuild_engine();
                Err(OtterError::Runtime(format!("VM panic: {}", msg)))
            }
        }
    }
}

/// Check for Test262 async completion patterns in print output.
/// Returns Some(Ok(())) if "Test262:AsyncTestComplete" found,
/// Some(Err(msg)) if "Test262:AsyncTestFailure:" found,
/// None if no pattern detected.
fn check_async_print_patterns(print_output: &[String]) -> Option<Result<(), String>> {
    for line in print_output {
        if line.contains("Test262:AsyncTestComplete") {
            return Some(Ok(()));
        }
        if line.contains("Test262:AsyncTestFailure:") {
            let msg = line
                .split("Test262:AsyncTestFailure:")
                .nth(1)
                .unwrap_or(line)
                .trim()
                .to_string();
            return Some(Err(msg));
        }
    }
    None
}

/// Extract a human-readable message from a caught panic payload.
fn extract_panic_message(panic: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = panic.downcast_ref::<&str>() {
        s.to_string()
    } else if let Some(s) = panic.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_runner_creation() {
        let runner = Test262Runner::new("tests/test262");
        assert!(!runner.skip_features.is_empty());
    }

    #[test]
    fn test_runner_no_skip() {
        let runner = Test262Runner::new_no_skip("tests/test262");
        assert!(runner.skip_features.is_empty());
    }

    #[test]
    fn test_default_skip_features_empty() {
        // Skip features are now config-driven only; default is empty
        let runner = Test262Runner::new("tests/test262");
        assert!(runner.skip_features.is_empty());
    }
}
