//! Node.js compatibility test execution engine.

use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use futures::FutureExt;
use otter_engine::{EngineBuilder, Otter};
use serde::{Deserialize, Serialize};

use crate::config::NodeCompatConfig;

/// Node.js compatibility test runner.
pub struct NodeCompatRunner {
    /// Root directory containing test files.
    test_dir: PathBuf,
    /// Harness directory (common.js, etc.).
    #[allow(dead_code)]
    harness_dir: PathBuf,
    /// Cached harness source (prepended to each test).
    harness_source: String,
    /// Reusable engine instance.
    engine: Option<Otter>,
    /// Config for skip lists, etc.
    config: NodeCompatConfig,
}

/// Result of running a single test file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestResult {
    /// Relative path of the test file.
    pub path: String,
    /// Which module this test belongs to.
    pub module: String,
    /// Test outcome.
    pub outcome: TestOutcome,
    /// Execution time in milliseconds.
    pub duration_ms: u64,
    /// Error message if failed.
    pub error: Option<String>,
}

/// Test outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TestOutcome {
    Pass,
    Fail,
    Skip,
    Timeout,
    Crash,
}

impl NodeCompatRunner {
    /// Create a new runner.
    ///
    /// `test_dir` should point to e.g. `tests/node-compat/node/test/parallel/`.
    /// `harness_dir` should point to `tests/node-compat/harness/`.
    pub fn new(test_dir: &Path, harness_dir: &Path, config: NodeCompatConfig) -> Self {
        let harness_source = load_harness(harness_dir);
        let engine = Some(build_engine());

        Self {
            test_dir: test_dir.to_path_buf(),
            harness_dir: harness_dir.to_path_buf(),
            harness_source,
            engine,
            config,
        }
    }

    /// Rebuild engine after a panic.
    fn rebuild_engine(&mut self) {
        // Drop in a panic-safe wrapper to avoid aborting on corrupted teardown.
        let old = self.engine.take();
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(old)));
        // Do NOT clear STRING_TABLE or call dealloc_all().  The new engine
        // reuses the existing interned strings; clearing them makes well_known
        // thread-local GcRef statics dangle → SIGSEGV after the next GC cycle.
        self.engine = Some(build_engine());
    }

    fn engine_mut(&mut self) -> &mut Otter {
        self.engine
            .as_mut()
            .expect("engine not available (mid-rebuild)")
    }

    fn engine(&self) -> &Otter {
        self.engine
            .as_ref()
            .expect("engine not available (mid-rebuild)")
    }

    /// List all test files for a module.
    ///
    /// Scans `test_dir` as a flat directory and matches filenames against the
    /// module's configured glob patterns (e.g. `"test-assert-*.js"`).
    pub fn list_tests_for_module(&self, module: &str) -> Vec<PathBuf> {
        let module_config = match self.config.modules.get(module) {
            Some(cfg) => cfg,
            None => return Vec::new(),
        };

        if !self.test_dir.is_dir() {
            return Vec::new();
        }

        let mut tests = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&self.test_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_file() {
                    continue;
                }
                let filename = match path.file_name().and_then(|f| f.to_str()) {
                    Some(f) => f,
                    None => continue,
                };
                if module_config.matches(filename) && !self.config.is_skipped(module, filename) {
                    tests.push(path);
                }
            }
        }

        tests.sort();
        tests
    }

    /// List all available modules (from config keys).
    pub fn list_modules(&self) -> Vec<String> {
        let mut modules: Vec<String> = self.config.modules.keys().cloned().collect();
        modules.sort();
        modules
    }

    /// List all test files across all modules.
    pub fn list_all_tests(&self) -> Vec<(String, PathBuf)> {
        let mut all = Vec::new();
        for module in self.list_modules() {
            for path in self.list_tests_for_module(&module) {
                all.push((module.clone(), path));
            }
        }
        all
    }

    /// Run a single test file.
    pub async fn run_test(
        &mut self,
        module: &str,
        test_path: &Path,
        timeout: Option<Duration>,
    ) -> TestResult {
        let rel_path = test_path
            .strip_prefix(&self.test_dir)
            .unwrap_or(test_path)
            .to_string_lossy()
            .to_string();

        let start = Instant::now();

        // Read test source
        let test_source = match std::fs::read_to_string(test_path) {
            Ok(s) => s,
            Err(e) => {
                return TestResult {
                    path: rel_path,
                    module: module.to_string(),
                    outcome: TestOutcome::Skip,
                    duration_ms: 0,
                    error: Some(format!("Failed to read: {}", e)),
                };
            }
        };

        // Compose: harness + test
        // `require` and `module`/`exports` are set up by the engine's module extension
        // (globalThis.require is created automatically from __createRequire).
        // __filename and __dirname
        let test_path_abs = test_path.canonicalize().unwrap_or(test_path.to_path_buf());
        let test_filename = test_path_abs.to_string_lossy().replace("\\", "\\\\");
        let test_dirname = test_path_abs
            .parent()
            .unwrap_or(test_path)
            .to_string_lossy()
            .replace("\\", "\\\\");

        let full_source = format!(
            "var __filename = '{}';\nvar __dirname = '{}';\n{}\n\n// --- Test: {} ---\n{}",
            test_filename, test_dirname, self.harness_source, rel_path, test_source
        );

        // Reset realm for clean test
        self.engine_mut().reset_realm();

        // Run with timeout and panic recovery — resolve require() from test file location
        let test_path_str = test_path.to_string_lossy().to_string();
        let (outcome, error) = self
            .execute(&full_source, timeout, &rel_path, Some(&test_path_str))
            .await;

        TestResult {
            path: rel_path,
            module: module.to_string(),
            outcome,
            duration_ms: start.elapsed().as_millis() as u64,
            error,
        }
    }

    /// Execute source with timeout and panic catching.
    async fn execute(
        &mut self,
        source: &str,
        timeout: Option<Duration>,
        test_name: &str,
        source_url: Option<&str>,
    ) -> (TestOutcome, Option<String>) {
        let result = if let Some(duration) = timeout {
            let flag = self.engine().interrupt_flag();
            let watchdog = tokio::spawn(async move {
                tokio::time::sleep(duration).await;
                flag.store(true, Ordering::Relaxed);
            });

            let result = AssertUnwindSafe(self.engine_mut().eval(source, source_url))
                .catch_unwind()
                .await;

            watchdog.abort();
            result
        } else {
            AssertUnwindSafe(self.engine_mut().eval(source, source_url))
                .catch_unwind()
                .await
        };

        match result {
            Ok(Ok(_)) => (TestOutcome::Pass, None),
            Ok(Err(err)) => {
                let msg = format!("{}", err);
                if msg.contains("interrupted") || msg.contains("Interrupt") {
                    (
                        TestOutcome::Timeout,
                        Some(format!("Timeout: {}", test_name)),
                    )
                } else {
                    (TestOutcome::Fail, Some(msg))
                }
            }
            Err(panic) => {
                let msg = extract_panic_message(&panic);
                self.rebuild_engine();
                (TestOutcome::Crash, Some(format!("VM panic: {}", msg)))
            }
        }
    }

    /// Run all tests for a specific module.
    pub async fn run_module(&mut self, module: &str, timeout: Option<Duration>) -> Vec<TestResult> {
        let tests = self.list_tests_for_module(module);
        let mut results = Vec::with_capacity(tests.len());

        for test_path in tests {
            let result = self.run_test(module, &test_path, timeout).await;
            results.push(result);
        }

        results
    }

    /// Run all tests for all modules.
    pub async fn run_all(&mut self, timeout: Option<Duration>) -> Vec<TestResult> {
        let modules = self.list_modules();
        let mut results = Vec::new();

        for module in modules {
            let module_results = self.run_module(&module, timeout).await;
            results.extend(module_results);
        }

        results
    }
}

/// Build an Otter engine with Node.js compatibility enabled.
fn build_engine() -> Otter {
    EngineBuilder::new()
        .with_nodejs()
        .capabilities(
            otter_engine::CapabilitiesBuilder::new()
                .allow_read_all()
                .allow_write_all()
                .allow_env_all()
                .allow_subprocess()
                .build(),
        )
        .build()
}

/// Load harness files (common.js, etc.) and concatenate them.
fn load_harness(harness_dir: &Path) -> String {
    let mut source = String::new();

    // Load common.js shim
    let common_path = harness_dir.join("common.js");
    if common_path.exists() {
        if let Ok(content) = std::fs::read_to_string(&common_path) {
            source.push_str("// --- harness: common.js ---\n");
            source.push_str(&content);
            source.push('\n');
        }
    }

    // Load any additional harness files
    let extras_path = harness_dir.join("extras.js");
    if extras_path.exists() {
        if let Ok(content) = std::fs::read_to_string(&extras_path) {
            source.push_str("// --- harness: extras.js ---\n");
            source.push_str(&content);
            source.push('\n');
        }
    }

    source
}

/// Extract a readable message from a caught panic.
fn extract_panic_message(panic: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = panic.downcast_ref::<&str>() {
        s.to_string()
    } else if let Some(s) = panic.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".to_string()
    }
}
