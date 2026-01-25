use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

use walkdir::WalkDir;

use otter_engine::{EngineBuilder, Otter, OtterError, PropertyKey, Value};

use crate::metadata::TestMetadata;

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
}

/// Result of running a single test
#[derive(Debug, Clone)]
pub struct TestResult {
    /// Test file path (relative to test dir)
    pub path: String,
    /// Test outcome
    pub outcome: TestOutcome,
    /// Execution time
    pub duration: Duration,
    /// Error message if failed
    pub error: Option<String>,
    /// Features used by test
    pub features: Vec<String>,
}

/// Test outcome
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
        println!("Initializing Otter Engine...");
        let start = Instant::now();
        // Initialize engine once with all standard builtins and harness extensions
        let engine = EngineBuilder::new()
            .with_http()
            .extension(crate::harness::create_harness_extension())
            .build();
        println!("Engine initialized in {:.2?}", start.elapsed());

        Self {
            test_dir: test_dir.as_ref().to_path_buf(),
            filter: None,
            skip_features: vec![
                // User requested to run ALL tests, so we empty this list
            ],
            engine: Arc::new(Mutex::new(engine)),
        }
    }

    /// Set filter pattern
    pub fn with_filter(mut self, filter: impl Into<String>) -> Self {
        self.filter = Some(filter.into());
        self
    }

    /// Add feature to skip list
    pub fn skip_feature(mut self, feature: impl Into<String>) -> Self {
        self.skip_features.push(feature.into());
        self
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
        let mut results = Vec::with_capacity(tests.len());
        for path in tests {
            results.push(self.run_test(&path).await);
        }
        results
    }

    /// Run all tests with a callback
    pub async fn run_all_with_callback<F>(&self, callback: F)
    where
        F: Fn(TestResult) + Sync + Send,
    {
        let tests = self.list_tests();
        for path in tests {
            let result = self.run_test(&path).await;
            callback(result);
        }
    }

    /// Run tests in a specific directory
    pub async fn run_dir(&self, subdir: &str) -> Vec<TestResult> {
        let tests = self.list_tests_dir(subdir);
        let mut results = Vec::with_capacity(tests.len());
        for path in tests {
            results.push(self.run_test(&path).await);
        }
        results
    }

    /// Run tests in a specific directory with a callback
    pub async fn run_dir_with_callback<F>(&self, subdir: &str, callback: F)
    where
        F: Fn(TestResult) + Sync + Send,
    {
        let tests = self.list_tests_dir(subdir);
        for path in tests {
            let result = self.run_test(&path).await;
            callback(result);
        }
    }

    /// Run a single test
    pub async fn run_test(&self, path: &Path) -> TestResult {
        let start = Instant::now();
        let relative_path = path
            .strip_prefix(&self.test_dir)
            .unwrap_or(path)
            .to_string_lossy()
            .to_string();

        // Read test file
        let content = match fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) => {
                return TestResult {
                    path: relative_path,
                    outcome: TestOutcome::Crash,
                    duration: start.elapsed(),
                    error: Some(format!("Failed to read file: {}", e)),
                    features: vec![],
                };
            }
        };

        // Parse metadata
        let metadata = TestMetadata::parse(&content).unwrap_or_default();

        // Check if we should skip this test
        for feature in &metadata.features {
            if self.skip_features.contains(feature) {
                return TestResult {
                    path: relative_path,
                    outcome: TestOutcome::Skip,
                    duration: start.elapsed(),
                    error: Some(format!("Skipped feature: {}", feature)),
                    features: metadata.features.clone(),
                };
            }
        }

        // Build test source with harness
        let mut test_source = String::new();

        // Add default harness files (sta.js and assert.js)
        let mut includes = vec!["sta.js".to_string(), "assert.js".to_string()];

        // Add explicitly requested harness files
        for include in &metadata.includes {
            if !includes.contains(include) {
                includes.push(include.clone());
            }
        }

        // Add harness files to source
        for include in includes {
            let harness_path = self.test_dir.join("harness").join(&include);
            match fs::read_to_string(&harness_path) {
                Ok(harness_content) => {
                    test_source.push_str(&harness_content);
                    test_source.push('\n');
                }
                Err(e) => {
                    // Only warn for explicit includes failing
                    if metadata.includes.contains(&include) {
                        eprintln!(
                            "Warning: Failed to read harness file {}: {}",
                            harness_path.display(),
                            e
                        );
                    }
                }
            }
        }

        // Add strict mode if needed
        if metadata.is_strict() {
            test_source.insert_str(0, "\"use strict\";\n");
        }

        // Add test content (strip metadata)
        let test_content = content
            .find("---*/")
            .map(|i| &content[i + 5..])
            .unwrap_or(&content);
        test_source.push_str(test_content);

        // Run the test
        let result = self
            .execute_test(&test_source, &metadata, &relative_path)
            .await;

        TestResult {
            path: relative_path,
            outcome: result.0,
            duration: start.elapsed(),
            error: result.1,
            features: metadata.features,
        }
    }

    /// Execute a test and return (outcome, error_message)
    async fn execute_test(
        &self,
        source: &str,
        metadata: &TestMetadata,
        test_name: &str,
    ) -> (TestOutcome, Option<String>) {
        let mut engine = self.engine.lock().await;

        match engine.eval(source).await {
            Ok(value) => {
                if !value.is_undefined() {
                    // println!("RESULT {}: {}", test_name, format_value(&value));
                }
                if metadata.expects_early_error() {
                    return (
                        TestOutcome::Fail,
                        Some("Expected parse/early error but compilation succeeded".to_string()),
                    );
                }
                if metadata.expects_runtime_error() {
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
                        (TestOutcome::Pass, None)
                    } else {
                        (TestOutcome::Fail, Some(format!("Compile error: {}", msg)))
                    }
                }
                OtterError::Runtime(msg) => {
                    if metadata.expects_runtime_error() {
                        (TestOutcome::Pass, None)
                    } else {
                        (TestOutcome::Fail, Some(msg))
                    }
                }
                OtterError::PermissionDenied(msg) => (TestOutcome::Fail, Some(msg)),
            },
        }
    }
}

fn format_value(value: &Value) -> String {
    if value.is_undefined() {
        return "undefined".to_string();
    }

    if value.is_null() {
        return "null".to_string();
    }

    if let Some(b) = value.as_boolean() {
        return b.to_string();
    }

    if let Some(n) = value.as_number() {
        if n.is_nan() {
            return "NaN".to_string();
        }
        if n.is_infinite() {
            return if n.is_sign_positive() {
                "Infinity"
            } else {
                "-Infinity"
            }
            .to_string();
        }
        if n.fract() == 0.0 && n.abs() < 1e15 {
            return format!("{}", n as i64);
        }
        return format!("{}", n);
    }

    if let Some(s) = value.as_string() {
        return format!("'{}'", s.as_str());
    }

    if let Some(obj) = value.as_object() {
        if obj.is_array() {
            let len = obj
                .get(&PropertyKey::string("length"))
                .and_then(|v| v.as_int32())
                .unwrap_or(0);
            return format!("[Array({})]", len);
        }
        return "[object Object]".to_string();
    }

    if value.is_function() {
        return "[Function]".to_string();
    }

    "[unknown]".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_runner_creation() {
        let runner = Test262Runner::new("tests/test262");
        assert!(runner.skip_features.is_empty());
    }
}
