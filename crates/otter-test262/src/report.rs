//! Complete conformance reports with validated derived rollups.
//!
//! # Contents
//! - [`Baseline`] owns every selected [`TestResult`] exactly once.
//! - [`Totals`] and section summaries derive from canonical rows.
//! - Strict loading, coverage checks, shard union and JSON/Markdown writers.
//!
//! # Invariants
//! Passes and skips retain their identities and diagnostics. Rows are sorted,
//! unique, normalized corpus paths; supplied rollups must equal recomputation.
//! A full-corpus publication checks the exact expected path set independently.
//! There is one current format and no aggregate-only compatibility reader.
//!
//! # See also
//! - [`crate::results`] for outcomes; [`crate::diff`] for exact transitions.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;

use crate::provenance::RunnerProvenance;
use crate::results::{Outcome, TestResult};

/// Counts derived from canonical rows.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Totals {
    /// All selected paths.
    pub total: u64,
    /// Conforming tests.
    pub passed: u64,
    /// Conformance failures.
    pub failed: u64,
    /// Explicit policy skips.
    pub skipped: u64,
    /// Panics or isolated process failures.
    pub crashed: u64,
    /// Per-test watchdog expirations.
    pub timed_out: u64,
    /// Heap-cap failures.
    pub oom: u64,
}

impl Totals {
    /// Add one explicit outcome to the derived counts.
    pub fn record(&mut self, outcome: &Outcome) {
        self.total += 1;
        match outcome {
            Outcome::Pass => self.passed += 1,
            Outcome::Fail { .. } => self.failed += 1,
            Outcome::Skipped { .. } => self.skipped += 1,
            Outcome::Crash { .. } => self.crashed += 1,
            Outcome::Timeout { .. } => self.timed_out += 1,
            Outcome::OutOfMemory { .. } => self.oom += 1,
        }
    }

    /// Percentage passing among tests that were attempted.
    #[must_use]
    pub fn pass_rate(&self) -> f64 {
        let denominator = self.total.saturating_sub(self.skipped);
        if denominator == 0 {
            0.0
        } else {
            self.passed as f64 * 100.0 / denominator as f64
        }
    }
}

/// Deterministically ordered counts by three-component corpus section.
pub type BySection = BTreeMap<String, Totals>;

/// One current canonical report; all per-test data lives in `tests`.
#[derive(Debug, Clone, Serialize)]
pub struct Baseline {
    /// Actual runner executable and effective per-test policy.
    pub runner: RunnerProvenance,
    /// Pinned Test262 commit.
    pub test262_commit: String,
    /// Engine source commit, accompanied by the actual runner digest.
    pub engine_commit: String,
    /// RFC-3339 capture timestamp.
    pub ran_at: String,
    /// Counts derived from all canonical test rows.
    pub totals: Totals,
    /// Section counts derived from canonical rows.
    pub by_section: BySection,
    /// Sole canonical per-test data, sorted uniquely by normalized path.
    pub tests: Vec<TestResult>,
}

impl<'de> Deserialize<'de> for Baseline {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // This decode boundary moves the same owned rows into the validated
        // report. It neither copies rows nor reads another report format.
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Fields {
            runner: RunnerProvenance,
            test262_commit: String,
            engine_commit: String,
            ran_at: String,
            totals: Totals,
            by_section: BySection,
            tests: Vec<TestResult>,
        }
        let fields = Fields::deserialize(deserializer)?;
        let report = Self {
            runner: fields.runner,
            test262_commit: fields.test262_commit,
            engine_commit: fields.engine_commit,
            ran_at: fields.ran_at,
            totals: fields.totals,
            by_section: fields.by_section,
            tests: fields.tests,
        };
        report.validate().map_err(serde::de::Error::custom)?;
        Ok(report)
    }
}

impl Baseline {
    /// Consume completed worker rows and derive every aggregate from them.
    pub fn from_results(
        mut tests: Vec<TestResult>,
        runner: RunnerProvenance,
        test262_commit: impl Into<String>,
        engine_commit: impl Into<String>,
        ran_at: impl Into<String>,
    ) -> Result<Self, ReportError> {
        tests.sort_by(|a, b| a.path.cmp(&b.path));
        let (totals, by_section) = rollups(&tests);
        let report = Self {
            runner,
            test262_commit: test262_commit.into(),
            engine_commit: engine_commit.into(),
            ran_at: ran_at.into(),
            totals,
            by_section,
            tests,
        };
        report.validate()?;
        Ok(report)
    }

    /// Reject duplicate/noncanonical paths and inconsistent derived counts.
    pub fn validate(&self) -> Result<(), ReportError> {
        self.runner
            .validate()
            .map_err(|message| ReportError::Invalid { message })?;
        let mut previous: Option<&str> = None;
        for test in &self.tests {
            let path = test.path.as_str();
            test.validate()
                .map_err(|message| ReportError::Invalid { message })?;
            if let Some(before) = previous {
                if path == before {
                    return Err(ReportError::Invalid {
                        message: format!("duplicate test path {path:?}"),
                    });
                }
                if path < before {
                    return Err(ReportError::Invalid {
                        message: "test rows are not sorted by path".to_owned(),
                    });
                }
            }
            previous = Some(path);
        }
        let (totals, sections) = rollups(&self.tests);
        if self.totals != totals || self.by_section != sections {
            return Err(ReportError::Invalid {
                message: "rollups do not match canonical test rows".to_owned(),
            });
        }
        Ok(())
    }

    /// Require the exact unique expected selection, including pass and skip.
    pub fn validate_coverage<'a>(
        &self,
        expected: impl IntoIterator<Item = &'a str>,
    ) -> Result<(), ReportError> {
        self.validate()?;
        let mut expected_set = BTreeSet::new();
        for path in expected {
            if !expected_set.insert(path) {
                return Err(ReportError::Invalid {
                    message: format!("duplicate expected test path {path:?}"),
                });
            }
        }
        let actual: BTreeSet<_> = self.tests.iter().map(|row| row.path.as_str()).collect();
        let missing: Vec<_> = expected_set
            .difference(&actual)
            .map(|path| (*path).to_owned())
            .collect();
        let unexpected: Vec<_> = actual
            .difference(&expected_set)
            .map(|path| (*path).to_owned())
            .collect();
        if missing.is_empty() && unexpected.is_empty() {
            Ok(())
        } else {
            Err(ReportError::Coverage {
                missing,
                unexpected,
            })
        }
    }

    /// Union actual rows from disjoint shards. Every outcome participates in
    /// collision checks; differing corpus/engine identities cannot be merged.
    pub fn merge(
        shards: Vec<(String, Self)>,
        ran_at: impl Into<String>,
    ) -> Result<Self, ReportError> {
        let head = shards.first().ok_or_else(|| ReportError::Invalid {
            message: "merge requires a shard".to_owned(),
        })?;
        let corpus = head.1.test262_commit.clone();
        let engine = head.1.engine_commit.clone();
        let runner = head.1.runner.clone();
        let mut seen = BTreeMap::new();
        let mut tests = Vec::new();
        for (source, shard) in shards {
            shard.validate()?;
            if shard.test262_commit != corpus
                || shard.engine_commit != engine
                || shard.runner != runner
            {
                return Err(ReportError::Invalid {
                    message: format!(
                        "shard {source:?} has different corpus/engine/runner identity"
                    ),
                });
            }
            for row in shard.tests {
                if let Some(first) = seen.insert(row.path.clone(), source.clone()) {
                    return Err(ReportError::MergeCollision {
                        path: row.path,
                        first,
                        second: source,
                    });
                }
                tests.push(row);
            }
        }
        Self::from_results(tests, runner, corpus, engine, ran_at)
    }

    /// Serialize a validated current-format report.
    pub fn to_json_pretty(&self) -> Result<String, ReportError> {
        self.validate()?;
        let mut text = serde_json::to_string_pretty(self).map_err(ReportError::Json)?;
        text.push('\n');
        Ok(text)
    }

    #[must_use]
    /// Render the summary and diagnostic failure view.
    pub fn to_markdown(&self) -> String {
        let mut out = String::with_capacity(8 * 1024);
        out.push_str("# Test262 conformance baseline\n\n");
        out.push_str(&format!(
            "- **Engine commit:** `{}`\n- **Test262 commit:** `{}`\n- **Captured:** {}\n\n",
            self.engine_commit, self.test262_commit, self.ran_at
        ));
        out.push_str(&format!(
            "- **Runner SHA256:** `{}`\n- **Target:** `{}`\n- **Debug assertions:** {}\n\n",
            self.runner.executable_sha256, self.runner.target, self.runner.debug_assertions
        ));
        out.push_str("The canonical JSON retains every selected test path, its outcome, skip policy, diagnostics and elapsed time.\n\n## Totals\n\n| Bucket | Count |\n|---|---|\n");
        for (name, count) in [
            ("total", self.totals.total),
            ("passed", self.totals.passed),
            ("failed", self.totals.failed),
            ("skipped", self.totals.skipped),
            ("crashed", self.totals.crashed),
            ("timed_out", self.totals.timed_out),
            ("oom", self.totals.oom),
        ] {
            out.push_str(&format!("| {name} | {count} |\n"));
        }
        out.push_str(&format!(
            "\n**Pass rate (excl. skipped):** {:.2}%\n\n",
            self.totals.pass_rate()
        ));
        let mut sections: Vec<_> = self.by_section.iter().collect();
        sections.sort_by_key(|(_, total)| std::cmp::Reverse(total.failed));
        if !sections.is_empty() {
            out.push_str("## Top failing sections (top 50)\n\n| Section | total | passed | failed | pass-rate |\n|---|---:|---:|---:|---:|\n");
            for (name, total) in sections.into_iter().take(50) {
                out.push_str(&format!(
                    "| {} | {} | {} | {} | {:.1}% |\n",
                    name,
                    total.total,
                    total.passed,
                    total.failed,
                    total.pass_rate()
                ));
            }
            out.push('\n');
        }
        if self.tests.iter().any(|test| test.outcome.is_failure()) {
            out.push_str("## Top failing-test patterns (top 100)\n\n| Outcome | Reason (truncated) | Path |\n|---|---|---|\n");
            for row in self
                .tests
                .iter()
                .filter(|row| row.outcome.is_failure())
                .take(100)
            {
                out.push_str(&format!(
                    "| {} | {} | `{}` |\n",
                    row.outcome.label(),
                    truncate(&row.outcome.detail().unwrap_or_default(), 80),
                    row.path
                ));
            }
            out.push('\n');
        }
        out
    }

    /// Write validated canonical JSON and its derived Markdown summary.
    pub fn write_pair(&self, dir: &Path, stem: &str) -> Result<(PathBuf, PathBuf), ReportError> {
        let json = self.to_json_pretty()?;
        std::fs::create_dir_all(dir).map_err(|e| ReportError::Io {
            path: dir.to_owned(),
            message: e.to_string(),
        })?;
        let json_path = dir.join(format!("{stem}.json"));
        let md_path = dir.join(format!("{stem}.md"));
        std::fs::write(&json_path, json).map_err(|e| ReportError::Io {
            path: json_path.clone(),
            message: e.to_string(),
        })?;
        std::fs::write(&md_path, self.to_markdown()).map_err(|e| ReportError::Io {
            path: md_path.clone(),
            message: e.to_string(),
        })?;
        Ok((json_path, md_path))
    }

    /// Read the one current format, rejecting malformed rows and rollups.
    pub fn from_path(path: &Path) -> Result<Self, ReportError> {
        let bytes = std::fs::read(path).map_err(|e| ReportError::Io {
            path: path.to_owned(),
            message: e.to_string(),
        })?;
        serde_json::from_slice(&bytes).map_err(ReportError::Json)
    }
}

fn rollups(tests: &[TestResult]) -> (Totals, BySection) {
    let mut totals = Totals::default();
    let mut sections = BTreeMap::<String, Totals>::new();
    for result in tests {
        totals.record(&result.outcome);
        sections
            .entry(section_of(&result.path).to_owned())
            .or_default()
            .record(&result.outcome);
    }
    (totals, sections)
}

/// Report I/O, structural, selection or shard identity failure.
#[derive(Debug, Error)]
pub enum ReportError {
    #[error("io error at {path:?}: {message}")]
    /// File access failed.
    Io {
        /// File involved.
        path: PathBuf,
        /// Original error detail.
        message: String,
    },
    #[error("json error: {0}")]
    /// Current-format JSON decoding or encoding failed.
    Json(#[source] serde_json::Error),
    #[error("invalid conformance report: {message}")]
    /// A structural invariant failed.
    Invalid {
        /// Precise rejected condition.
        message: String,
    },
    #[error("test coverage mismatch: missing {missing:?}, unexpected {unexpected:?}")]
    /// Actual path set differs from the expected selection.
    Coverage {
        /// Expected paths absent from the report.
        missing: Vec<String>,
        /// Report paths absent from the selection.
        unexpected: Vec<String>,
    },
    #[error("merge collision: test {path:?} appears in shards {first} and {second}")]
    /// Two shards claim the same selected test.
    MergeCollision {
        /// Repeated path.
        path: String,
        /// First shard.
        first: String,
        /// Second shard.
        second: String,
    },
}

/// First three normalized path components, or the whole shorter path.
#[must_use]
pub fn section_of(rel_path: &str) -> &str {
    let mut separators = 0;
    for (index, character) in rel_path.char_indices() {
        if character == '/' {
            separators += 1;
            if separators == 3 {
                return &rel_path[..index];
            }
        }
    }
    rel_path
}

fn truncate(text: &str, maximum: usize) -> String {
    let mut out: String = text.chars().take(maximum).collect();
    if text.chars().count() > maximum {
        out.push('…');
    }
    out.replace('|', "\\|").replace('\n', " ")
}

#[cfg(test)]
#[path = "report_tests.rs"]
mod tests;
