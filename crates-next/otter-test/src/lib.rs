//! Engine test harness (`otter test`).
//!
//! Implements the contract pinned by
//! [`docs/new-engine/specs/otter-test-harness.md`](
//!     ../../../docs/new-engine/specs/otter-test-harness.md
//!   ). The harness slice (task 07) supports the minimum needed to
//! drive `tests/engine/smoke/*.ts` end-to-end: TOML metadata,
//! `engine` and `smoke` suites, exit-code assertions, fresh runtime
//! per fixture, NDJSON `--json` output.
//!
//! # Contents
//! - [`Suite`] — suite enum (`Engine`, `Smoke`, `Test262`).
//! - [`RunOptions`] — runner configuration.
//! - [`run_suite`] — discover and execute fixtures, return a report.
//! - [`Report`], [`TestRecord`], [`Outcome`] — structured output
//!   types (`serde::Serialize` for `--json`).
//!
//! # See also
//! - [`docs/new-engine/specs/otter-test-harness.md`](
//!     ../../../docs/new-engine/specs/otter-test-harness.md
//!   )

use std::path::{Path, PathBuf};

use otter_runtime::{Otter, OtterError};
use serde::{Deserialize, Serialize};

/// Stable JSON wire-format version for the harness output.
pub const HARNESS_SCHEMA_VERSION: u32 = 1;

/// Test suite selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Suite {
    /// First-party engine fixtures: `tests/engine/`.
    Engine,
    /// Short release smoke tests: `tests/smoke/`.
    Smoke,
    /// Curated Test262 subset: `tests/test262-curated/`.
    Test262,
}

impl Suite {
    /// Default root directory for the suite.
    #[must_use]
    pub fn default_root(self) -> &'static Path {
        match self {
            Suite::Engine => Path::new("tests/engine"),
            Suite::Smoke => Path::new("tests/smoke"),
            Suite::Test262 => Path::new("tests/test262-curated"),
        }
    }

    /// Suite display name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Suite::Engine => "engine",
            Suite::Smoke => "smoke",
            Suite::Test262 => "test262",
        }
    }
}

/// Runner configuration.
#[derive(Debug, Clone)]
pub struct RunOptions {
    /// Suite to execute.
    pub suite: Suite,
    /// Optional substring filter against fixture path / declared
    /// `name`.
    pub filter: Option<String>,
    /// Optional override for the suite root.
    pub root_override: Option<PathBuf>,
}

impl RunOptions {
    /// Resolve the root directory for the configured suite.
    #[must_use]
    pub fn root(&self) -> PathBuf {
        self.root_override
            .clone()
            .unwrap_or_else(|| self.suite.default_root().to_path_buf())
    }
}

/// One test outcome (NDJSON-friendly tagged enum).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Outcome {
    /// All assertions held.
    Passed,
    /// An assertion failed; reason is human-readable.
    Failed {
        /// Short reason.
        reason: String,
    },
    /// Watchdog fired (foundation runner does not yet implement
    /// per-test watchdogs; reserved variant).
    Timeout,
    /// Heap cap reached unexpectedly.
    OutOfMemory,
    /// Capability denied unexpectedly.
    CapabilityDenied {
        /// Name of the denied capability.
        capability: String,
    },
    /// Skipped because a `requires` feature was missing.
    Skipped {
        /// Reason string (e.g., `"requires=ts"`).
        reason: String,
    },
    /// Runtime crashed.
    Crash {
        /// Short reason.
        reason: String,
    },
}

/// Per-test record (NDJSON line shape).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestRecord {
    /// Always `"test"`.
    pub kind: &'static str,
    /// Display name.
    pub name: String,
    /// Path relative to the suite root.
    pub path: String,
    /// Outcome.
    pub outcome: Outcome,
    /// Wall-clock duration.
    pub duration_ms: u64,
}

/// Final summary record (NDJSON line shape).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SummaryRecord {
    /// Always `"summary"`.
    pub kind: &'static str,
    /// Pinned at [`HARNESS_SCHEMA_VERSION`].
    pub schema_version: u32,
    /// Suite name.
    pub suite: String,
    /// Counters.
    pub passed: u32,
    /// Counters.
    pub failed: u32,
    /// Counters.
    pub timeout: u32,
    /// Counters.
    pub oom: u32,
    /// Counters.
    pub capability_denied: u32,
    /// Counters.
    pub skipped: u32,
    /// Counters.
    pub crash: u32,
    /// Total wall-clock duration.
    pub duration_ms: u64,
}

/// Combined report (records + summary).
#[derive(Debug, Clone)]
pub struct Report {
    /// One record per fixture.
    pub records: Vec<TestRecord>,
    /// Summary.
    pub summary: SummaryRecord,
}

impl Report {
    /// Did all fixtures pass (or skip)?
    #[must_use]
    pub fn all_passed(&self) -> bool {
        self.summary.failed == 0
            && self.summary.timeout == 0
            && self.summary.oom == 0
            && self.summary.capability_denied == 0
            && self.summary.crash == 0
    }
}

/// Fixture metadata header parsed from the leading
/// `/* otter-test: ... */` block.
#[derive(Debug, Clone, Default, Deserialize)]
struct FixtureMetadata {
    name: Option<String>,
    #[serde(default)]
    expect: ExpectBlock,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct ExpectBlock {
    exit_code: Option<i32>,
}

/// Discover and execute the fixtures in the configured suite.
///
/// # Errors
/// Returns [`OtterError::Io`] if the suite root cannot be read.
pub fn run_suite(opts: &RunOptions) -> Result<Report, OtterError> {
    let root = opts.root();
    if !root.exists() {
        return Err(OtterError::Io {
            path: root.clone(),
            kind: otter_runtime::IoErrorKind::NotFound,
            message: format!("suite root does not exist: {}", root.display()),
        });
    }
    let mut fixtures = Vec::new();
    discover(&root, &mut fixtures)?;
    fixtures.sort();

    let total_start = std::time::Instant::now();
    let mut records = Vec::with_capacity(fixtures.len());
    let mut counters = Counters::default();

    for path in fixtures {
        let rel = path
            .strip_prefix(&root)
            .unwrap_or(&path)
            .to_string_lossy()
            .into_owned();
        let display_name = rel.clone();

        if let Some(filter) = opts.filter.as_deref() {
            if !display_name.contains(filter) {
                continue;
            }
        }

        let start = std::time::Instant::now();
        let record = run_fixture(&path, &display_name);
        let duration_ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);

        match &record.outcome {
            Outcome::Passed => counters.passed += 1,
            Outcome::Failed { .. } => counters.failed += 1,
            Outcome::Timeout => counters.timeout += 1,
            Outcome::OutOfMemory => counters.oom += 1,
            Outcome::CapabilityDenied { .. } => counters.capability_denied += 1,
            Outcome::Skipped { .. } => counters.skipped += 1,
            Outcome::Crash { .. } => counters.crash += 1,
        }
        records.push(TestRecord {
            duration_ms,
            ..record
        });
    }

    let total_ms = u64::try_from(total_start.elapsed().as_millis()).unwrap_or(u64::MAX);
    let summary = SummaryRecord {
        kind: "summary",
        schema_version: HARNESS_SCHEMA_VERSION,
        suite: opts.suite.name().to_string(),
        passed: counters.passed,
        failed: counters.failed,
        timeout: counters.timeout,
        oom: counters.oom,
        capability_denied: counters.capability_denied,
        skipped: counters.skipped,
        crash: counters.crash,
        duration_ms: total_ms,
    };

    Ok(Report { records, summary })
}

#[derive(Debug, Default)]
struct Counters {
    passed: u32,
    failed: u32,
    timeout: u32,
    oom: u32,
    capability_denied: u32,
    skipped: u32,
    crash: u32,
}

fn discover(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), OtterError> {
    let entries = std::fs::read_dir(dir).map_err(|e| OtterError::Io {
        path: dir.to_path_buf(),
        kind: otter_runtime::IoErrorKind::from_std(e.kind()),
        message: e.to_string(),
    })?;
    for entry in entries {
        let entry = entry.map_err(|e| OtterError::Io {
            path: dir.to_path_buf(),
            kind: otter_runtime::IoErrorKind::from_std(e.kind()),
            message: e.to_string(),
        })?;
        let path = entry.path();
        if path.is_dir() {
            // Convention: any directory whose name starts with
            // `_` is a helper bundle — its contents are imported
            // by sibling entry fixtures but never enumerated as
            // standalone tests. This is how multi-file module
            // fixtures co-locate their helpers
            // (`tests/engine/modules/foo/_modules/util.ts`).
            //
            // `node_modules/` is skipped by the same logic: its
            // contents are package source loaded through the
            // module-graph driver, never tests themselves.
            //
            // See `docs/new-engine/specs/otter-test-harness.md` §2
            // for the full fixture-format spec amendment.
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default();
            if name.starts_with('_') || name == "node_modules" {
                continue;
            }
            discover(&path, out)?;
        } else if matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("ts" | "tsx" | "mts" | "cts" | "js" | "mjs" | "cjs")
        ) {
            out.push(path);
        }
    }
    Ok(())
}

fn run_fixture(path: &Path, display: &str) -> TestRecord {
    let source = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            return TestRecord {
                kind: "test",
                name: display.to_string(),
                path: display.to_string(),
                outcome: Outcome::Crash {
                    reason: format!("read failed: {e}"),
                },
                duration_ms: 0,
            };
        }
    };

    let metadata = parse_metadata(&source).unwrap_or_default();
    let expected_exit = metadata.expect.exit_code.unwrap_or(0);
    let display_name = metadata.name.clone().unwrap_or_else(|| display.to_string());

    let mut otter = Otter::new();
    let outcome = match otter.run_file(path) {
        Ok(_) => {
            if expected_exit == 0 {
                Outcome::Passed
            } else {
                Outcome::Failed {
                    reason: format!("expected exit {expected_exit}, got 0"),
                }
            }
        }
        Err(err) => {
            let actual = err.exit_code();
            if actual == expected_exit {
                Outcome::Passed
            } else {
                Outcome::Failed {
                    reason: format!("expected exit {expected_exit}, got {actual} ({err})"),
                }
            }
        }
    };

    TestRecord {
        kind: "test",
        name: display_name,
        path: display.to_string(),
        outcome,
        duration_ms: 0,
    }
}

fn parse_metadata(source: &str) -> Option<FixtureMetadata> {
    let source = source.trim_start();
    let rest = source.strip_prefix("/* otter-test:")?;
    let end = rest.find("*/")?;
    let toml_text = &rest[..end];
    toml::from_str(toml_text).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_metadata() {
        let m =
            parse_metadata("/* otter-test:\nname = \"x\"\n[expect]\nexit_code = 0\n*/").unwrap();
        assert_eq!(m.name.as_deref(), Some("x"));
        assert_eq!(m.expect.exit_code, Some(0));
    }

    #[test]
    fn no_metadata_yields_default() {
        assert!(parse_metadata("undefined;").is_none());
    }
}
