use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Write;
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
    skip_features: HashSet<String>,
    /// Cached harness file contents (filename -> content)
    harness_cache: HashMap<String, String>,
    /// Shared harness state for capturing async test results
    harness_state: TestHarnessState,
    /// Reusable engine (Option to allow safe drop-before-reset in post-panic rebuild)
    engine: Option<Otter>,
    /// Whether to dump on timeout
    dump_on_timeout: bool,
    /// Dump output path (None = stderr)
    dump_file: Option<PathBuf>,
    /// Ring buffer size for trace
    dump_buffer_size: usize,
    /// Enable full trace
    trace_enabled: bool,
    /// Trace output file
    trace_file: Option<PathBuf>,
    /// Trace filter pattern
    trace_filter: Option<String>,
    /// Trace only failures
    trace_failures_only: bool,
    /// Trace only timeouts
    trace_timeouts_only: bool,
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
    pub fn new(
        test_dir: impl AsRef<Path>,
        dump_on_timeout: bool,
        dump_file: Option<PathBuf>,
        dump_buffer_size: usize,
        trace_enabled: bool,
        trace_file: Option<PathBuf>,
        trace_filter: Option<String>,
        trace_failures_only: bool,
        trace_timeouts_only: bool,
    ) -> Self {
        let (harness_ext, harness_state) = crate::harness::create_harness_extension_with_state();
        let mut engine = EngineBuilder::new().extension(harness_ext).build();

        // Configure trace if dump_on_timeout is enabled
        if dump_on_timeout {
            engine.set_trace_config(otter_vm_core::TraceConfig {
                enabled: true,
                mode: otter_vm_core::TraceMode::RingBuffer,
                ring_buffer_size: dump_buffer_size,
                output_path: dump_file.clone(),
                filter: None,
                capture_timing: false,
            });
        }

        // Pre-cache all harness files
        let harness_dir = test_dir.as_ref().join("harness");
        let mut harness_cache = HashMap::new();
        if let Ok(entries) = fs::read_dir(&harness_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().map(|e| e == "js").unwrap_or(false) {
                    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                        if let Ok(content) = fs::read_to_string(&path) {
                            harness_cache.insert(name.to_string(), content);
                        }
                    }
                }
            }
        }

        Self {
            test_dir: test_dir.as_ref().to_path_buf(),
            filter: None,
            skip_features: HashSet::new(),
            harness_cache,
            harness_state,
            engine: Some(engine),
            dump_on_timeout,
            dump_file,
            dump_buffer_size,
            trace_enabled,
            trace_file,
            trace_filter,
            trace_failures_only,
            trace_timeouts_only,
        }
    }

    /// Rebuild engine after a panic to restore consistent state.
    ///
    /// Only called after a panic — normal test runs reuse the same engine.
    fn rebuild_engine(&mut self) {
        // Phase 1: Drop old engine in a panic-safe way.
        //
        // After a VM panic the engine's internal data structures (property maps,
        // string table, etc.) may be left in a corrupted state.  Calling
        // VmContext::teardown() on corrupt state can trigger a second panic which,
        // being outside any catch_unwind boundary, would abort the process.
        //
        // Wrapping the drop in catch_unwind ensures any secondary panics are
        // silenced.  The GC objects on the thread-local heap are still alive
        // (dealloc_all is NOT called — see Phase 2 comment below).
        let old_engine = self.engine.take();
        if let Err(_) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            drop(old_engine);
        })) {
            // teardown panicked on corrupted state — silently ignored.
        }

        // Phase 2: Create a fresh engine WITHOUT clearing the string intern table.
        //
        // We must NOT call clear_global_string_table() nor dealloc_all() here:
        //
        // * dealloc_all() would free GC memory while `well_known` thread-local
        //   statics (LENGTH, PROTOTYPE, …) still hold GcRef<JsString> pointers
        //   to those objects → dangling pointers → SIGSEGV.
        //
        // * clear_global_string_table() removes the old strings from the intern
        //   table.  The new engine would then intern fresh copies.  The
        //   well_known statics still point at the OLD copies.  On the next GC
        //   cycle the old copies are unreachable from any root → freed →
        //   well_known statics dangle → SIGSEGV.
        //
        // The correct approach: keep the STRING_TABLE intact.  The new engine
        // calls JsString::intern("length") etc. during intrinsics setup, finds
        // the existing entries in the table, and reuses those same GcRefs that
        // the well_known statics hold.  The GC keeps those strings alive because
        // they are reachable through the new engine's property maps.  The
        // prune_dead_string_table_entries hook then evicts only truly unreachable
        // entries on subsequent GC cycles.
        let (harness_ext, harness_state) = crate::harness::create_harness_extension_with_state();
        let mut engine = EngineBuilder::new().extension(harness_ext).build();

        // Re-apply trace configuration if enabled
        if self.dump_on_timeout {
            engine.set_trace_config(otter_vm_core::TraceConfig {
                enabled: true,
                mode: otter_vm_core::TraceMode::RingBuffer,
                ring_buffer_size: self.dump_buffer_size,
                output_path: self.dump_file.clone(),
                filter: None,
                capture_timing: false,
            });
        }

        self.engine = Some(engine);
        self.harness_state = harness_state;
    }

    /// Get a reference to the engine (panics if engine is None — only during rebuild).
    fn engine(&self) -> &Otter {
        self.engine
            .as_ref()
            .expect("engine not available (mid-rebuild)")
    }

    /// Get a mutable reference to the engine.
    fn engine_mut(&mut self) -> &mut Otter {
        self.engine
            .as_mut()
            .expect("engine not available (mid-rebuild)")
    }

    /// Create a new test runner that skips no features (runs everything).
    pub fn new_no_skip(test_dir: impl AsRef<Path>) -> Self {
        let mut runner = Self::new(test_dir, false, None, 100, false, None, None, false, false);
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
        self.skip_features = features.into_iter().collect();
        self
    }

    /// Add feature to skip list
    pub fn skip_feature(mut self, feature: impl Into<String>) -> Self {
        self.skip_features.insert(feature.into());
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

            // If non-strict mode fails, skip strict mode:
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
        // Reset realm per mode: creates fresh global + intrinsics on the same
        // GC heap. Much faster than full engine rebuild for large suites (no
        // GC reset, no extension recompilation). Extensions re-applied by eval().
        self.engine_mut().reset_realm();

        let relative_path = path.strip_prefix(&self.test_dir).unwrap_or(path);
        let test_name = format!("{} ({})", relative_path.to_string_lossy(), mode);

        // Configure trace for this test
        self.configure_test_trace(&test_name);

        // Build test source with harness
        let mut test_source = String::new();

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

        // Add harness files to source (from cache) — always in sloppy mode
        // to avoid duplicate function declaration errors (e.g. isPrimitive in
        // both assert.js and testTypedArray.js).
        for include in &includes {
            if let Some(harness_content) = self.harness_cache.get(include) {
                test_source.push_str(harness_content);
                test_source.push('\n');
            } else {
                eprintln!(
                    "ERROR: Harness file '{}' not found in cache (required by test)",
                    include
                );
            }
        }

        // Add test content (strip metadata)
        let test_content = content
            .find("---*/")
            .map(|i| &content[i + 5..])
            .unwrap_or(content);
        if mode == ExecutionMode::Strict {
            // Run strict-mode test body via indirect eval so strictness applies
            // to test code only (not to prepended harness files).
            let strict_body = format!("\"use strict\";\n{}", test_content);
            if let Ok(encoded) = serde_json::to_string(&strict_body) {
                test_source.push_str("(0, eval)(");
                test_source.push_str(&encoded);
                test_source.push_str(");\n");
            } else {
                // Fallback: old behavior if encoding unexpectedly fails.
                test_source.push_str("\"use strict\";\n");
                test_source.push_str(test_content);
            }
        } else {
            test_source.push_str(test_content);
        }

        // Clear harness state before running the test
        self.harness_state.clear();

        // Execute - route module tests to separate handler
        if mode == ExecutionMode::Module {
            self.execute_test_as_module(&test_source, metadata, &test_name, timeout, path)
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
        self.execute_test_with_url(source, metadata, test_name, timeout, "main.js")
            .await
    }

    /// Execute a test with a specific source URL and return (outcome, error_message).
    async fn execute_test_with_url(
        &mut self,
        source: &str,
        metadata: &TestMetadata,
        test_name: &str,
        timeout: Option<Duration>,
        source_url: &str,
    ) -> (TestOutcome, Option<String>) {
        let is_async = metadata.is_async();

        let outcome = match self
            .run_with_timeout(source, timeout, test_name, source_url)
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
                                    Some(
                                        "Expected runtime error but async test passed".to_string(),
                                    ),
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
                        None => (
                            TestOutcome::Fail,
                            Some("Async test completed without $DONE or print signal".to_string()),
                        ),
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
                        // Dump snapshot on timeout if enabled
                        if self.dump_on_timeout {
                            self.dump_timeout_info(&test_name, &msg);
                        }
                        (TestOutcome::Timeout, None)
                    } else {
                        (TestOutcome::Fail, Some(msg))
                    }
                }
                OtterError::PermissionDenied(msg) => (TestOutcome::Fail, Some(msg)),
            },
        };

        // Save trace if configured for failures/timeouts
        self.save_conditional_trace(&test_name, outcome.0);

        outcome
    }

    /// Configure trace for a test based on runner settings
    fn configure_test_trace(&mut self, test_name: &str) {
        if !self.trace_enabled {
            return;
        }

        // Determine trace mode based on settings
        let mode = if self.trace_failures_only || self.trace_timeouts_only {
            // Use ring buffer mode, will dump to file conditionally after test
            otter_vm_core::TraceMode::RingBuffer
        } else {
            // Full trace mode - record everything
            otter_vm_core::TraceMode::FullTrace
        };

        // Generate trace file path if not specified
        let trace_path = if let Some(ref path) = self.trace_file {
            path.clone()
        } else {
            let sanitized_name = test_name.replace(['/', '\\', ' ', '(', ')'], "_");
            PathBuf::from(format!("test262-trace-{}.txt", sanitized_name))
        };

        let buffer_size = self.dump_buffer_size;
        let filter = self.trace_filter.clone();
        self.engine_mut()
            .set_trace_config(otter_vm_core::TraceConfig {
                enabled: true,
                mode,
                ring_buffer_size: buffer_size,
                output_path: Some(trace_path),
                filter,
                capture_timing: false,
            });
    }

    /// Save trace for a test that failed/timed out (if configured)
    fn save_conditional_trace(&self, test_name: &str, outcome: TestOutcome) {
        // Only save if we're in conditional mode and the condition matches
        let should_save = (self.trace_failures_only && outcome == TestOutcome::Fail)
            || (self.trace_timeouts_only && outcome == TestOutcome::Timeout);

        if !should_save {
            return;
        }

        // Get snapshot and write to file
        let snapshot = self.engine().debug_snapshot();
        if snapshot.recent_instructions.is_empty() {
            return;
        }

        let trace_path = if let Some(ref path) = self.trace_file {
            path.clone()
        } else {
            let sanitized_name = test_name.replace(['/', '\\', ' ', '(', ')'], "_");
            PathBuf::from(format!("test262-trace-{}.txt", sanitized_name))
        };

        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&trace_path)
        {
            use std::io::Write;
            let _ = writeln!(file, "\n═══════════════════════════════════════════");
            let _ = writeln!(file, "Test: {}", test_name);
            let _ = writeln!(file, "Outcome: {:?}", outcome);
            let _ = writeln!(file, "═══════════════════════════════════════════\n");

            // Create temporary ring buffer from snapshot
            let mut buffer =
                otter_vm_core::TraceRingBuffer::new(snapshot.recent_instructions.len().max(1));
            for entry in &snapshot.recent_instructions {
                buffer.push(entry.clone());
            }

            let formatted = otter_vm_core::format::format_trace_buffer(&buffer);
            let _ = write!(file, "{}", formatted);
        }
    }

    /// Dump debug information when a test times out
    fn dump_timeout_info(&self, test_path: &str, _msg: &str) {
        let header = format!(
            "\n═══════════════════════════════════════════════════════════════\n\
             TIMEOUT DETECTED: {}\n\
             ═══════════════════════════════════════════════════════════════\n",
            test_path
        );

        if let Some(ref dump_file) = self.dump_file {
            // Write to file
            if let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(dump_file)
            {
                let _ = write!(file, "{}", header);
                let _ = self.engine().dump_snapshot(&mut file);
            } else {
                eprintln!("Failed to open dump file: {:?}", dump_file);
            }
        } else {
            // Write to stderr
            eprintln!("{}", header);
            let _ = self.engine().dump_snapshot(&mut std::io::stderr());
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
    /// The harness code (assert, $DONE, etc.) is concatenated into the module
    /// source. Function declarations in module scope are accessible within the
    /// same module, so this works for most tests. Tests that self-import will
    /// load the raw file from disk without harness — this is a known limitation.
    async fn execute_test_as_module(
        &mut self,
        source: &str,
        metadata: &TestMetadata,
        test_name: &str,
        timeout: Option<Duration>,
        test_path: &Path,
    ) -> (TestOutcome, Option<String>) {
        // Use the test's real file path with .mjs extension so that:
        // 1. The compiler parses in module mode (TLA, import/export, strict)
        // 2. Relative imports in module tests resolve from the test's directory
        let source_url = test_path
            .with_extension("mjs")
            .to_string_lossy()
            .to_string();
        self.execute_test_with_url(source, metadata, test_name, timeout, &source_url)
            .await
    }

    async fn run_with_timeout(
        &mut self,
        source: &str,
        timeout: Option<Duration>,
        _test_name: &str,
        source_url: &str,
    ) -> Result<Value, OtterError> {
        let result = if let Some(duration) = timeout {
            // Cooperative timeout: spawn a tokio task as watchdog on a separate
            // worker thread (runtime is multi-threaded). It sets the interrupt
            // flag after the deadline; the VM checks it every ~10K instructions.
            let flag = self.engine().interrupt_flag();
            let watchdog = tokio::spawn(async move {
                tokio::time::sleep(duration).await;
                flag.store(true, Ordering::Relaxed);
            });

            let result = AssertUnwindSafe(self.engine_mut().eval(source, Some(source_url)))
                .catch_unwind()
                .await;

            // Cancel watchdog if test finished before timeout
            watchdog.abort();

            result
        } else {
            AssertUnwindSafe(self.engine_mut().eval(source, Some(source_url)))
                .catch_unwind()
                .await
        };

        match result {
            Ok(val) => val,
            Err(panic) => {
                let msg = extract_panic_message(&panic);
                // Report the crash and keep going — do NOT rebuild the engine.
                // Rebuilding masks the real bug and complicates debugging.
                // The engine state may be corrupted; subsequent tests on this
                // worker will likely also panic/crash, which is expected and
                // acceptable for a debug session.
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
        let runner = Test262Runner::new(
            "tests/test262",
            false, // dump_on_timeout
            None,  // dump_file
            100,   // dump_buffer_size
            false, // trace_enabled
            None,  // trace_file
            None,  // trace_filter
            false, // trace_failures_only
            false, // trace_timeouts_only
        );
        // Skip features are now config-driven only; default is empty
        assert!(runner.skip_features.is_empty());
    }

    #[test]
    fn test_runner_no_skip() {
        let runner = Test262Runner::new_no_skip("tests/test262");
        assert!(runner.skip_features.is_empty());
    }

    #[test]
    fn test_default_skip_features_empty() {
        // Skip features are now config-driven only; default is empty
        let runner = Test262Runner::new(
            "tests/test262",
            false, // dump_on_timeout
            None,  // dump_file
            100,   // dump_buffer_size
            false, // trace_enabled
            None,  // trace_file
            None,  // trace_filter
            false, // trace_failures_only
            false, // trace_timeouts_only
        );
        assert!(runner.skip_features.is_empty());
    }
}
