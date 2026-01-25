//! Test262 test runner

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rayon::prelude::*;
use walkdir::WalkDir;

use otter_vm_compiler::Compiler;
use otter_vm_core::VmRuntime;

use crate::metadata::TestMetadata;

/// Test262 test runner
pub struct Test262Runner {
    /// Path to test262 directory
    test_dir: PathBuf,
    /// VM runtime
    runtime: Arc<VmRuntime>,
    /// Filter pattern
    filter: Option<String>,
    /// Features to skip
    skip_features: Vec<String>,
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
        Self {
            test_dir: test_dir.as_ref().to_path_buf(),
            runtime: Arc::new(VmRuntime::new()),
            filter: None,
            skip_features: vec![
                // User requested to run ALL tests, so we empty this list
            ],
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
    pub fn run_all(&self) -> Vec<TestResult> {
        let tests = self.list_tests();

        // Run tests in parallel
        tests.par_iter().map(|path| self.run_test(path)).collect()
    }

    /// Run all tests with a callback
    pub fn run_all_with_callback<F>(&self, callback: F)
    where
        F: Fn(TestResult) + Sync + Send,
    {
        let tests = self.list_tests();
        tests.par_iter().for_each(|path| {
            let result = self.run_test(path);
            callback(result);
        });
    }

    /// Run tests in a specific directory
    pub fn run_dir(&self, subdir: &str) -> Vec<TestResult> {
        let tests = self.list_tests_dir(subdir);
        tests.par_iter().map(|path| self.run_test(path)).collect()
    }

    /// Run tests in a specific directory with a callback
    pub fn run_dir_with_callback<F>(&self, subdir: &str, callback: F)
    where
        F: Fn(TestResult) + Sync + Send,
    {
        let tests = self.list_tests_dir(subdir);
        tests.par_iter().for_each(|path| {
            let result = self.run_test(path);
            callback(result);
        });
    }

    /// Run a single test
    pub fn run_test(&self, path: &Path) -> TestResult {
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

        // Skip module tests for now
        // Skip module tests for now
        // User requested to run ALL tests
        // if metadata.is_module() {
        //    return TestResult {
        //        path: relative_path,
        //        outcome: TestOutcome::Skip,
        //        duration: start.elapsed(),
        //        error: Some("Module tests not yet supported".to_string()),
        //        features: metadata.features.clone(),
        //    };
        // }

        // Skip async tests for now
        // Skip async tests for now
        // User requested to run ALL tests
        // if metadata.is_async() {
        //    return TestResult {
        //        path: relative_path,
        //        outcome: TestOutcome::Skip,
        //        duration: start.elapsed(),
        //        error: Some("Async tests not yet supported".to_string()),
        //        features: metadata.features.clone(),
        //    };
        // }

        // Build test source with harness
        let mut test_source = String::new();

        // Add default harness files (sta.js and assert.js)
        // These are required by almost all tests but not always explicitly included
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
        let result = self.execute_test(&test_source, &metadata);

        TestResult {
            path: relative_path,
            outcome: result.0,
            duration: start.elapsed(),
            error: result.1,
            features: metadata.features,
        }
    }

    /// Execute a test and return (outcome, error_message)
    fn execute_test(&self, source: &str, metadata: &TestMetadata) -> (TestOutcome, Option<String>) {
        // Compile
        let compiler = Compiler::new();
        let compile_result = compiler.compile(source, "test.js");

        match compile_result {
            Ok(module) => {
                // Expected parse error but compilation succeeded
                if metadata.expects_early_error() {
                    return (
                        TestOutcome::Fail,
                        Some("Expected parse/early error but compilation succeeded".to_string()),
                    );
                }

                // Execute
                match self.runtime.execute_module(&module) {
                    Ok(_) => {
                        // Expected runtime error but execution succeeded
                        if metadata.expects_runtime_error() {
                            (
                                TestOutcome::Fail,
                                Some("Expected runtime error but execution succeeded".to_string()),
                            )
                        } else {
                            (TestOutcome::Pass, None)
                        }
                    }
                    Err(e) => {
                        // Got runtime error
                        if metadata.expects_runtime_error() {
                            // TODO: Check error type matches expected
                            (TestOutcome::Pass, None)
                        } else {
                            (TestOutcome::Fail, Some(e.to_string()))
                        }
                    }
                }
            }
            Err(e) => {
                // Compilation failed
                if metadata.expects_early_error() {
                    // TODO: Check error type matches expected
                    (TestOutcome::Pass, None)
                } else {
                    (TestOutcome::Fail, Some(format!("Compile error: {}", e)))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_runner_creation() {
        let runner = Test262Runner::new("tests/test262");
        assert!(runner.skip_features.contains(&"BigInt".to_string()));
    }
}
