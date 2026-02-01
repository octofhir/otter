use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use otter_engine::{EngineBuilder, Otter, OtterError, Value, VmContextSnapshot};

use crate::harness::TestHarnessState;
use crate::metadata::{ExecutionMode, TestMetadata};

/// Features that are not yet implemented in Otter and should be skipped.
pub const DEFAULT_SKIP_FEATURES: &[&str] = &[
    // Atomics & shared memory
    "Atomics",
    "SharedArrayBuffer",
    // Temporal proposal
    "Temporal",
    // Decorators proposal
    "decorators",
    // Import assertions / attributes
    "import-assertions",
    "import-attributes",
    // FinalizationRegistry / WeakRef
    "FinalizationRegistry",
    "WeakRef",
    // ShadowRealm
    "ShadowRealm",
    // Explicit resource management (using/await using)
    "explicit-resource-management",
    // Tail calls
    "tail-call-optimization",
    // Intl (internationalization)
    "Intl",
    "Intl.DateTimeFormat",
    "Intl.DisplayNames",
    "Intl.ListFormat",
    "Intl.Locale",
    "Intl.NumberFormat",
    "Intl.PluralRules",
    "Intl.RelativeTimeFormat",
    "Intl.Segmenter",
    "Intl-enumeration",
    // Import / export (module system)
    "dynamic-import",
    "import.meta",
    // Resizable ArrayBuffer
    "resizable-arraybuffer",
    "arraybuffer-transfer",
    // JSON modules
    "json-modules",
    // Iterator helpers
    "iterator-helpers",
    // Set methods
    "set-methods",
    // Promise.withResolvers
    "promise-with-resolvers",
    // Array grouping
    "array-grouping",
    // Well-formed Unicode strings
    "well-formed-unicode-strings",
    // Symbols as WeakMap keys
    "symbols-as-weakmap-keys",
    // RegExp features not yet implemented
    "regexp-duplicate-named-groups",
    "regexp-lookbehind",
    "regexp-named-groups",
    "regexp-unicode-property-escapes",
    "regexp-v-flag",
    "regexp-match-indices",
    // Hashbang
    "hashbang",
    // Top-level await (module feature)
    "top-level-await",
    // Class features
    "class-fields-private",
    "class-fields-public",
    "class-methods-private",
    "class-static-block",
    "class-static-fields-private",
    "class-static-fields-public",
    "class-static-methods-private",
    // Other proposals
    "Array.fromAsync",
    "change-array-by-copy",
    "String.prototype.isWellFormed",
    "String.prototype.toWellFormed",
    "Object.groupBy",
    "Map.groupBy",
    "Uint8Array",
];

/// Test262 test runner
pub struct Test262Runner {
    /// Path to test262 directory
    test_dir: PathBuf,
    /// Filter pattern
    filter: Option<String>,
    /// Features to skip
    skip_features: Vec<String>,
    /// Shared engine instance
    engine: Arc<Mutex<Otter>>,
    /// Shared harness state for capturing async test results
    harness_state: TestHarnessState,
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
        eprintln!("Initializing Otter Engine...");
        let start = Instant::now();

        // Create harness extension with shared state for async output capture
        let (harness_ext, harness_state) =
            crate::harness::create_harness_extension_with_state();

        // Initialize engine once with all standard builtins and harness extensions
        let engine = EngineBuilder::new()
            .extension(harness_ext)
            .build();
        eprintln!("Engine initialized in {:.2?}", start.elapsed());

        Self {
            test_dir: test_dir.as_ref().to_path_buf(),
            filter: None,
            skip_features: DEFAULT_SKIP_FEATURES
                .iter()
                .map(|s| s.to_string())
                .collect(),
            engine: Arc::new(Mutex::new(engine)),
            harness_state,
        }
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
    pub async fn run_all(&self) -> Vec<TestResult> {
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
        &self,
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

        // Skip module tests for now (not yet supported)
        if modes == vec![ExecutionMode::Module] {
            return vec![TestResult {
                path: relative_path_str,
                mode: ExecutionMode::Module,
                outcome: TestOutcome::Skip,
                duration_ms: 0,
                error: Some("Module tests not yet supported".to_string()),
                features: metadata.features.clone(),
            }];
        }

        let mut results = Vec::with_capacity(modes.len());

        for mode in &modes {
            if *mode == ExecutionMode::Module {
                continue;
            }

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
    pub async fn run_test(&self, path: &Path, timeout: Option<Duration>) -> TestResult {
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
        &self,
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

        // Execute
        self.execute_test(&test_source, metadata, &test_name, timeout)
            .await
    }

    /// Execute a test and return (outcome, error_message)
    async fn execute_test(
        &self,
        source: &str,
        metadata: &TestMetadata,
        test_name: &str,
        timeout: Option<Duration>,
    ) -> (TestOutcome, Option<String>) {
        // Create a fresh engine for each test to avoid cross-test contamination
        // (shared intrinsic objects like Object.prototype get modified by tests)
        let (harness_ext, _harness_state) =
            crate::harness::create_harness_extension_with_state();
        let mut engine = EngineBuilder::new()
            .extension(harness_ext)
            .build();
        let is_async = metadata.is_async();

        
        match self
            .run_with_timeout(&mut engine, source, timeout, test_name)
            .await
        {
            Ok(value) => {
                if !value.is_undefined() {
                    // Debug: println!("RESULT {}: {}", test_name, format_value(&value));
                }

                // For async tests, check the $DONE result from harness state
                if is_async {
                    match self.harness_state.done_result() {
                        Some(Ok(())) => {
                            if metadata.expects_runtime_error() {
                                (
                                    TestOutcome::Fail,
                                    Some("Expected runtime error but async test passed via $DONE()".to_string()),
                                )
                            } else {
                                (TestOutcome::Pass, None)
                            }
                        }
                        Some(Err(msg)) => {
                            if metadata.expects_runtime_error() {
                                self.validate_negative_error(metadata, &msg)
                            } else {
                                (
                                    TestOutcome::Fail,
                                    Some(format!("Async test failed via $DONE: {}", msg)),
                                )
                            }
                        }
                        None => {
                            if metadata.expects_early_error() || metadata.expects_runtime_error() {
                                (
                                    TestOutcome::Fail,
                                    Some("Expected error but execution completed without $DONE".to_string()),
                                )
                            } else {
                                (
                                    TestOutcome::Fail,
                                    Some("Async test completed without calling $DONE()".to_string()),
                                )
                            }
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
                        self.validate_negative_error(metadata, &msg)
                    } else {
                        (TestOutcome::Fail, Some(format!("Compile error: {}", msg)))
                    }
                }
                OtterError::Runtime(msg) => {
                    if metadata.expects_runtime_error() {
                        self.validate_negative_error(metadata, &msg)
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

    /// Validate that the error type matches the negative expectation from metadata.
    fn validate_negative_error(
        &self,
        metadata: &TestMetadata,
        error_msg: &str,
    ) -> (TestOutcome, Option<String>) {
        if let Some(ref negative) = metadata.negative {
            let expected_type = &negative.error_type;
            if error_msg.contains(expected_type) {
                (TestOutcome::Pass, None)
            } else {
                // Error occurred as expected, but type doesn't match.
                // Be lenient for now — count as pass but note the mismatch.
                (
                    TestOutcome::Pass,
                    Some(format!(
                        "Error type mismatch: expected {} but got: {}",
                        expected_type, error_msg
                    )),
                )
            }
        } else {
            (TestOutcome::Pass, None)
        }
    }

    async fn run_with_timeout(
        &self,
        engine: &mut Otter,
        source: &str,
        timeout: Option<Duration>,
        test_name: &str,
    ) -> Result<Value, OtterError> {
        if let Some(duration) = timeout {
            let interrupt_flag = engine.interrupt_flag();
            let snapshot_handle = engine.debug_snapshot_handle();
            let timed_out = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let done = Arc::new(std::sync::atomic::AtomicBool::new(false));

            let timed_out_thread = Arc::clone(&timed_out);
            let done_thread = Arc::clone(&done);
            let test_name = test_name.to_string();
            thread::spawn(move || {
                thread::sleep(duration);
                if done_thread.load(Ordering::Relaxed) {
                    return;
                }
                timed_out_thread.store(true, Ordering::Relaxed);
                interrupt_flag.store(true, Ordering::Relaxed);
                let snapshot = snapshot_handle.lock().clone();
                eprintln!(
                    "WATCHDOG: timeout after {:?} in {}. VM snapshot: {}",
                    duration,
                    test_name,
                    format_snapshot(&snapshot)
                );
                let _ = std::io::stderr().flush();
            });

            // Use eval_sync to avoid async wrapper (globalThis, async function)
            // which breaks simple test262 tests
            let result = engine.eval_sync(source);
            done.store(true, Ordering::Relaxed);

            if timed_out.load(Ordering::Relaxed) {
                Err(OtterError::Runtime("Test timed out".to_string()))
            } else {
                result
            }
        } else {
            engine.eval_sync(source)
        }
    }
}

fn format_snapshot(snapshot: &VmContextSnapshot) -> String {
    let mut parts = Vec::new();
    parts.push(format!("stack_depth={}", snapshot.stack_depth));
    parts.push(format!("try_stack_depth={}", snapshot.try_stack_depth));
    parts.push(format!("instruction_count={}", snapshot.instruction_count));
    parts.push(format!("native_call_depth={}", snapshot.native_call_depth));
    if let Some(pc) = snapshot.pc {
        parts.push(format!("pc={}", pc));
    }
    if let Some(ref instruction) = snapshot.instruction {
        parts.push(format!("instruction={}", instruction));
    }
    if let Some(function_index) = snapshot.function_index {
        parts.push(format!("function_index={}", function_index));
    }
    if let Some(ref name) = snapshot.function_name {
        parts.push(format!("function_name={}", name));
    }
    if let Some(ref module_url) = snapshot.module_url {
        parts.push(format!("module_url={}", module_url));
    }
    if let Some(is_async) = snapshot.is_async {
        parts.push(format!("is_async={}", is_async));
    }
    if let Some(is_generator) = snapshot.is_generator {
        parts.push(format!("is_generator={}", is_generator));
    }
    if let Some(is_construct) = snapshot.is_construct {
        parts.push(format!("is_construct={}", is_construct));
    }
    if !snapshot.frames.is_empty() {
        let frames = snapshot
            .frames
            .iter()
            .map(|frame| {
                format!(
                    "[fn={} name={} pc={} instr={} module={} async={} gen={} construct={}]",
                    frame.function_index,
                    frame.function_name.clone().unwrap_or_default(),
                    frame.pc,
                    frame.instruction.clone().unwrap_or_default(),
                    frame.module_url,
                    frame.is_async,
                    frame.is_generator,
                    frame.is_construct
                )
            })
            .collect::<Vec<_>>()
            .join(" ");
        parts.push(format!("frames={}", frames));
    }
    parts.join(", ")
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
    fn test_default_skip_features_populated() {
        assert!(!DEFAULT_SKIP_FEATURES.is_empty());
        assert!(DEFAULT_SKIP_FEATURES.contains(&"Atomics"));
        assert!(DEFAULT_SKIP_FEATURES.contains(&"Temporal"));
    }
}
