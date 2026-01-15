//! Test command - run test files.

use anyhow::Result;
use clap::Args;
use otter_runtime::{JscConfig, JscRuntime, needs_transpilation, transpile_typescript};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::config::Config;

#[derive(Args)]
pub struct TestCommand {
    /// Test files or directories to run (defaults to current directory)
    #[arg(default_value = ".")]
    pub paths: Vec<PathBuf>,

    /// Filter tests by name pattern
    #[arg(long, short = 'f')]
    pub filter: Option<String>,

    /// Allow all permissions for tests
    #[arg(long = "allow-all", short = 'A')]
    pub allow_all: bool,

    /// Skip type checking
    #[arg(long = "no-check")]
    pub no_check: bool,

    /// Timeout per test in milliseconds
    #[arg(long, default_value_t = 30000)]
    pub timeout: u64,

    /// Watch mode - re-run on file changes
    #[arg(long)]
    pub watch: bool,
}

impl TestCommand {
    pub async fn run(&self, _config: &Config) -> Result<()> {
        // Find test files
        let test_files = self.find_test_files()?;

        if test_files.is_empty() {
            println!("No test files found.");
            return Ok(());
        }

        println!("Running {} test file(s)...\n", test_files.len());

        let mut passed = 0;
        let mut failed = 0;
        let mut skipped = 0;

        for file in &test_files {
            match self.run_test_file(file).await {
                Ok(result) => {
                    passed += result.passed;
                    failed += result.failed;
                    skipped += result.skipped;
                }
                Err(e) => {
                    eprintln!("Error running {}: {}", file.display(), e);
                    failed += 1;
                }
            }
        }

        println!();
        if failed > 0 {
            println!(
                "Result: {} passed, {} failed, {} skipped",
                passed, failed, skipped
            );
            std::process::exit(1);
        } else {
            println!("Result: {} passed, {} skipped", passed, skipped);
        }

        Ok(())
    }

    fn find_test_files(&self) -> Result<Vec<PathBuf>> {
        let mut files = Vec::new();

        for path in &self.paths {
            if path.is_file() {
                if self.is_test_file(path) {
                    files.push(path.clone());
                }
            } else if path.is_dir() {
                self.find_test_files_in_dir(path, &mut files)?;
            }
        }

        files.sort();
        Ok(files)
    }

    fn find_test_files_in_dir(&self, dir: &PathBuf, files: &mut Vec<PathBuf>) -> Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();

            // Skip node_modules and hidden directories
            if path.is_dir() {
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if name == "node_modules" || name.starts_with('.') {
                    continue;
                }
                self.find_test_files_in_dir(&path, files)?;
            } else if self.is_test_file(&path) {
                files.push(path);
            }
        }

        Ok(())
    }

    fn is_test_file(&self, path: &Path) -> bool {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");

        // Match patterns like *.test.ts, *.spec.ts, *_test.ts
        name.ends_with(".test.ts")
            || name.ends_with(".test.js")
            || name.ends_with(".spec.ts")
            || name.ends_with(".spec.js")
            || name.ends_with("_test.ts")
            || name.ends_with("_test.js")
    }

    async fn run_test_file(&self, path: &PathBuf) -> Result<TestResult> {
        println!("  {}", path.display());

        let source = std::fs::read_to_string(path)?;

        // Transpile if needed
        let code = if needs_transpilation(&path.to_string_lossy()) {
            let result = transpile_typescript(&source)
                .map_err(|e| anyhow::anyhow!("Transpilation error: {}", e))?;
            result.code
        } else {
            source
        };

        let runtime = JscRuntime::new(JscConfig::default())?;

        // Inject test framework
        let filter_json = match &self.filter {
            Some(f) => format!("\"{}\"", f),
            None => "null".to_string(),
        };

        let test_harness = format!(
            r#"
globalThis.__otter_tests = [];
globalThis.__otter_results = {{ passed: 0, failed: 0, skipped: 0 }};
globalThis.__otter_filter = {filter_json};

globalThis.describe = function(name, fn) {{
    fn();
}};

globalThis.it = globalThis.test = function(name, fn) {{
    const filter = globalThis.__otter_filter;
    if (filter && !name.includes(filter)) {{
        globalThis.__otter_results.skipped++;
        console.log("    - " + name + " (skipped)");
        return;
    }}
    globalThis.__otter_tests.push({{ name, fn }});
}};

globalThis.expect = function(actual) {{
    return {{
        toBe: function(expected) {{
            if (actual !== expected) {{
                throw new Error("Expected " + JSON.stringify(expected) + " but got " + JSON.stringify(actual));
            }}
        }},
        toEqual: function(expected) {{
            if (JSON.stringify(actual) !== JSON.stringify(expected)) {{
                throw new Error("Expected " + JSON.stringify(expected) + " but got " + JSON.stringify(actual));
            }}
        }},
        toBeTruthy: function() {{
            if (!actual) {{
                throw new Error("Expected truthy but got " + JSON.stringify(actual));
            }}
        }},
        toBeFalsy: function() {{
            if (actual) {{
                throw new Error("Expected falsy but got " + JSON.stringify(actual));
            }}
        }},
        toThrow: function(message) {{
            let threw = false;
            try {{
                if (typeof actual === 'function') actual();
            }} catch (e) {{
                threw = true;
                if (message && !e.message.includes(message)) {{
                    throw new Error("Expected error containing '" + message + "' but got '" + e.message + "'");
                }}
            }}
            if (!threw) {{
                throw new Error("Expected function to throw");
            }}
        }},
        toContain: function(expected) {{
            if (Array.isArray(actual)) {{
                if (!actual.includes(expected)) {{
                    throw new Error("Expected array to contain " + JSON.stringify(expected));
                }}
            }} else if (typeof actual === 'string') {{
                if (!actual.includes(expected)) {{
                    throw new Error("Expected string to contain " + JSON.stringify(expected));
                }}
            }}
        }},
        toBeGreaterThan: function(expected) {{
            if (!(actual > expected)) {{
                throw new Error("Expected " + actual + " to be greater than " + expected);
            }}
        }},
        toBeLessThan: function(expected) {{
            if (!(actual < expected)) {{
                throw new Error("Expected " + actual + " to be less than " + expected);
            }}
        }},
        toBeNull: function() {{
            if (actual !== null) {{
                throw new Error("Expected null but got " + JSON.stringify(actual));
            }}
        }},
        toBeUndefined: function() {{
            if (actual !== undefined) {{
                throw new Error("Expected undefined but got " + JSON.stringify(actual));
            }}
        }},
        toBeDefined: function() {{
            if (actual === undefined) {{
                throw new Error("Expected value to be defined");
            }}
        }},
        not: {{
            toBe: function(expected) {{
                if (actual === expected) {{
                    throw new Error("Expected not to be " + JSON.stringify(expected));
                }}
            }},
            toEqual: function(expected) {{
                if (JSON.stringify(actual) === JSON.stringify(expected)) {{
                    throw new Error("Expected not to equal " + JSON.stringify(expected));
                }}
            }},
            toBeNull: function() {{
                if (actual === null) {{
                    throw new Error("Expected not to be null");
                }}
            }},
            toBeUndefined: function() {{
                if (actual === undefined) {{
                    throw new Error("Expected not to be undefined");
                }}
            }},
        }}
    }};
}};

// Load test file
{code}

// Run tests
(async () => {{
    for (const test of globalThis.__otter_tests) {{
        try {{
            const result = test.fn();
            if (result instanceof Promise) {{
                await result;
            }}
            globalThis.__otter_results.passed++;
            console.log("    ✓ " + test.name);
        }} catch (e) {{
            globalThis.__otter_results.failed++;
            console.log("    ✗ " + test.name);
            console.log("      " + e.message);
        }}
    }}
}})();
"#
        );

        runtime.eval(&test_harness)?;

        let timeout = Duration::from_millis(self.timeout);
        runtime.run_event_loop_until_idle(timeout)?;

        // Get results via JSON deserialization
        let results = runtime.context().get_global("__otter_results")?;
        let results: TestResultJson = results.deserialize().unwrap_or_default();

        Ok(TestResult {
            passed: results.passed,
            failed: results.failed,
            skipped: results.skipped,
        })
    }
}

#[derive(Default, serde::Deserialize)]
struct TestResultJson {
    passed: usize,
    failed: usize,
    skipped: usize,
}

struct TestResult {
    passed: usize,
    failed: usize,
    skipped: usize,
}
