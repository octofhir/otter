//! JSON + Markdown report writers.
//!
//! Output layout: `docs/new-engine/test262-baseline/<engine-commit>.{json,md}`
//! per task 100 §"Output formats". The JSON is the canonical
//! machine-readable wire format; the Markdown is a human-readable
//! summary that GitHub renders inline on PR review.
//!
//! Spec link: <https://tc39.es/ecma262/>

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::runner::{Outcome, TestResult};

/// Top-level totals for a single sweep.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Totals {
    /// Every [`TestResult`] recorded in this sweep, regardless of
    /// outcome.
    pub total: u64,
    /// `Outcome::Pass`.
    pub passed: u64,
    /// `Outcome::Fail`.
    pub failed: u64,
    /// `Outcome::Skipped`.
    pub skipped: u64,
    /// `Outcome::Crash`.
    pub crashed: u64,
    /// `Outcome::Timeout`.
    pub timed_out: u64,
    /// `Outcome::OutOfMemory`.
    pub oom: u64,
}

impl Totals {
    /// Add a single outcome to the running totals.
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

    /// Pass rate as a percentage (denominator = total - skipped, so
    /// skips don't deflate the headline number).
    #[must_use]
    pub fn pass_rate(&self) -> f64 {
        let denom = self.total.saturating_sub(self.skipped);
        if denom == 0 {
            return 0.0;
        }
        (self.passed as f64) * 100.0 / (denom as f64)
    }
}

/// Per-section totals (keyed by the directory prefix of the test
/// path).
pub type BySection = BTreeMap<String, Totals>;

/// One row in the `failing_tests` array of the JSON report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FailingTest {
    /// Test path relative to `vendor/test262/test/`.
    pub path: String,
    /// `esid:` from the frontmatter (if any).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub esid: Option<String>,
    /// One-word kind (`fail` / `crash` / `timeout` / `oom`).
    pub outcome: String,
    /// Human-readable failure reason.
    pub reason: String,
}

/// Canonical JSON shape written to
/// `docs/new-engine/test262-baseline/<engine-commit>.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Baseline {
    /// SHA of the pinned `vendor/test262` commit.
    pub test262_commit: String,
    /// SHA of the engine commit that produced the baseline.
    pub engine_commit: String,
    /// ISO-8601 wall-clock timestamp.
    pub ran_at: String,
    /// Roll-up.
    pub totals: Totals,
    /// Per-section totals, keyed by directory prefix.
    pub by_section: BySection,
    /// Every test whose outcome is not `Pass` / `Skipped`.
    pub failing_tests: Vec<FailingTest>,
}

impl Baseline {
    /// Build a baseline from a list of [`TestResult`] records.
    ///
    /// `test262_commit` and `engine_commit` are passed in by the
    /// caller (typically `git rev-parse HEAD` against
    /// `vendor/test262` and the workspace root).
    #[must_use]
    pub fn from_results(
        results: &[TestResult],
        test262_commit: impl Into<String>,
        engine_commit: impl Into<String>,
        ran_at: impl Into<String>,
    ) -> Self {
        let mut totals = Totals::default();
        let mut by_section: BySection = BTreeMap::new();
        let mut failing_tests = Vec::new();
        for result in results {
            totals.record(&result.outcome);
            let section = section_of(&result.path);
            by_section
                .entry(section.to_string())
                .or_default()
                .record(&result.outcome);
            if let Some(row) = failing_row_from(result) {
                failing_tests.push(row);
            }
        }
        Self {
            test262_commit: test262_commit.into(),
            engine_commit: engine_commit.into(),
            ran_at: ran_at.into(),
            totals,
            by_section,
            failing_tests,
        }
    }

    /// Serialise to canonical pretty-printed JSON ending with a
    /// trailing newline.
    ///
    /// # Errors
    /// Returns [`ReportError::Json`] on serialization failure (none
    /// of the variants can fail under normal conditions; the
    /// `Result` is preserved for caller-side propagation).
    pub fn to_json_pretty(&self) -> Result<String, ReportError> {
        let mut s = serde_json::to_string_pretty(self).map_err(ReportError::Json)?;
        s.push('\n');
        Ok(s)
    }

    /// Render the human-readable Markdown summary.
    #[must_use]
    pub fn to_markdown(&self) -> String {
        let mut out = String::with_capacity(8 * 1024);
        out.push_str("# Test262 conformance baseline\n\n");
        out.push_str(&format!(
            "- **Engine commit:** `{}`\n- **Test262 commit:** `{}`\n- **Captured:** {}\n\n",
            self.engine_commit, self.test262_commit, self.ran_at
        ));
        out.push_str("## Totals\n\n");
        out.push_str("| Bucket | Count |\n|---|---|\n");
        out.push_str(&format!("| total      | {} |\n", self.totals.total));
        out.push_str(&format!("| passed     | {} |\n", self.totals.passed));
        out.push_str(&format!("| failed     | {} |\n", self.totals.failed));
        out.push_str(&format!("| skipped    | {} |\n", self.totals.skipped));
        out.push_str(&format!("| crashed    | {} |\n", self.totals.crashed));
        out.push_str(&format!("| timed_out  | {} |\n", self.totals.timed_out));
        out.push_str(&format!("| oom        | {} |\n", self.totals.oom));
        out.push_str(&format!(
            "\n**Pass rate (excl. skipped):** {:.2}%\n\n",
            self.totals.pass_rate()
        ));

        // Top 50 failing sections by absolute fail count.
        let mut sections: Vec<(&String, &Totals)> = self.by_section.iter().collect();
        sections.sort_by(|a, b| b.1.failed.cmp(&a.1.failed));
        let truncated_sections = sections.iter().take(50).collect::<Vec<_>>();
        if !truncated_sections.is_empty() {
            out.push_str("## Top failing sections (top 50)\n\n");
            out.push_str(
                "| Section | total | passed | failed | pass-rate |\n|---|---:|---:|---:|---:|\n",
            );
            for (name, t) in &truncated_sections {
                out.push_str(&format!(
                    "| {} | {} | {} | {} | {:.1}% |\n",
                    name,
                    t.total,
                    t.passed,
                    t.failed,
                    t.pass_rate()
                ));
            }
            out.push('\n');
        }

        // Top 100 failing tests by recurrence pattern (deduplicated
        // by reason-prefix).
        if !self.failing_tests.is_empty() {
            out.push_str("## Top failing-test patterns (top 100)\n\n");
            out.push_str("| Outcome | Reason (truncated) | Path |\n|---|---|---|\n");
            for row in self.failing_tests.iter().take(100) {
                let reason = truncate(&row.reason, 80);
                out.push_str(&format!(
                    "| {} | {} | `{}` |\n",
                    row.outcome, reason, row.path
                ));
            }
            out.push('\n');
        }
        out
    }

    /// Write the baseline to disk: both `.json` and `.md` end up in
    /// `dir` named after `stem` (typically the engine commit SHA).
    ///
    /// # Errors
    /// [`ReportError::Io`] / [`ReportError::Json`] on the obvious
    /// failure paths.
    pub fn write_pair(&self, dir: &Path, stem: &str) -> Result<(PathBuf, PathBuf), ReportError> {
        std::fs::create_dir_all(dir).map_err(|e| ReportError::Io {
            path: dir.to_path_buf(),
            message: e.to_string(),
        })?;
        let json_path = dir.join(format!("{stem}.json"));
        let md_path = dir.join(format!("{stem}.md"));
        std::fs::write(&json_path, self.to_json_pretty()?).map_err(|e| ReportError::Io {
            path: json_path.clone(),
            message: e.to_string(),
        })?;
        std::fs::write(&md_path, self.to_markdown()).map_err(|e| ReportError::Io {
            path: md_path.clone(),
            message: e.to_string(),
        })?;
        Ok((json_path, md_path))
    }

    /// Read a baseline from `path`.
    ///
    /// # Errors
    /// [`ReportError::Io`] / [`ReportError::Json`].
    pub fn from_path(path: &Path) -> Result<Self, ReportError> {
        let text = std::fs::read_to_string(path).map_err(|e| ReportError::Io {
            path: path.to_path_buf(),
            message: e.to_string(),
        })?;
        serde_json::from_str(&text).map_err(ReportError::Json)
    }
}

/// Errors raised by the report writers.
#[derive(Debug, Error)]
pub enum ReportError {
    /// Filesystem error.
    #[error("io error at {path:?}: {message}")]
    Io {
        /// Path that triggered the error.
        path: PathBuf,
        /// Underlying message.
        message: String,
    },
    /// JSON serialization / deserialization error.
    #[error("json error: {0}")]
    Json(#[source] serde_json::Error),
    /// Two shard reports claim the same test path.
    #[error("merge collision: test {path:?} appears in shards {first} and {second}")]
    MergeCollision {
        /// Conflicting path.
        path: String,
        /// First shard / source.
        first: String,
        /// Second shard / source.
        second: String,
    },
}

/// Compute the section for a test path. The runner uses the first
/// **three** path segments (`language/expressions/addition`,
/// `built-ins/Array/prototype/flat`, …); paths shorter than three
/// segments fall back to whatever's there.
#[must_use]
pub fn section_of(rel_path: &str) -> &str {
    let mut count = 0;
    let mut end = rel_path.len();
    for (idx, ch) in rel_path.char_indices() {
        if ch == '/' {
            count += 1;
            if count == 3 {
                end = idx;
                break;
            }
        }
    }
    &rel_path[..end]
}

/// Convert a [`TestResult`] into a [`FailingTest`] row, returning
/// `None` for `Pass` / `Skipped` outcomes (which do not appear in
/// `failing_tests`).
fn failing_row_from(r: &TestResult) -> Option<FailingTest> {
    let (outcome, reason) = match &r.outcome {
        Outcome::Pass | Outcome::Skipped { .. } => return None,
        Outcome::Fail { reason, .. } => ("fail", reason.clone()),
        Outcome::Crash { panic } => ("crash", panic.clone()),
        Outcome::Timeout { ms } => ("timeout", format!("timeout after {ms} ms")),
        Outcome::OutOfMemory { bytes } => ("oom", format!("oom: {bytes} bytes requested")),
    };
    Some(FailingTest {
        path: r.path.clone(),
        esid: r.esid.clone(),
        outcome: outcome.to_string(),
        reason,
    })
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.replace('|', "\\|").replace('\n', " ");
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out.replace('|', "\\|").replace('\n', " ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synth_results() -> Vec<TestResult> {
        vec![
            TestResult {
                path: "built-ins/Math/abs/length.js".to_string(),
                esid: Some("sec-math.abs".to_string()),
                features: vec![],
                outcome: Outcome::Pass,
                wall_ms: 5,
            },
            TestResult {
                path: "built-ins/Math/abs/nan.js".to_string(),
                esid: None,
                features: vec![],
                outcome: Outcome::Fail {
                    reason: "expected NaN".to_string(),
                    stack: None,
                },
                wall_ms: 7,
            },
            TestResult {
                path: "language/expressions/addition/x.js".to_string(),
                esid: None,
                features: vec![],
                outcome: Outcome::Skipped {
                    feature: "Atomics".to_string(),
                },
                wall_ms: 0,
            },
        ]
    }

    #[test]
    fn baseline_roll_up_counts_match() {
        let b = Baseline::from_results(&synth_results(), "abc", "deadbeef", "2026-05-01");
        assert_eq!(b.totals.total, 3);
        assert_eq!(b.totals.passed, 1);
        assert_eq!(b.totals.failed, 1);
        assert_eq!(b.totals.skipped, 1);
        assert_eq!(b.failing_tests.len(), 1);
        assert!(b.by_section.contains_key("built-ins/Math/abs"));
    }

    #[test]
    fn json_roundtrip_preserves_totals() {
        let b = Baseline::from_results(&synth_results(), "abc", "deadbeef", "2026-05-01");
        let json = b.to_json_pretty().unwrap();
        let parsed: Baseline = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.totals.total, 3);
        assert_eq!(parsed.test262_commit, "abc");
        assert_eq!(parsed.engine_commit, "deadbeef");
    }

    #[test]
    fn markdown_renders_top_failing_section() {
        let b = Baseline::from_results(&synth_results(), "abc", "deadbeef", "2026-05-01");
        let md = b.to_markdown();
        assert!(md.contains("# Test262 conformance baseline"));
        assert!(md.contains("built-ins/Math/abs"));
        assert!(md.contains("Top failing-test patterns"));
    }

    #[test]
    fn pass_rate_excludes_skipped() {
        let b = Baseline::from_results(&synth_results(), "x", "y", "z");
        // 1 pass, 1 fail, 1 skip → denominator 2 (skip excluded).
        assert!((b.totals.pass_rate() - 50.0).abs() < 0.01);
    }

    #[test]
    fn write_pair_creates_json_and_markdown() {
        let dir = tempfile::tempdir().unwrap();
        let b = Baseline::from_results(&synth_results(), "x", "y", "z");
        let (json_path, md_path) = b.write_pair(dir.path(), "abc1234").unwrap();
        assert!(json_path.exists());
        assert!(md_path.exists());
    }

    #[test]
    fn section_uses_three_segments() {
        assert_eq!(
            section_of("built-ins/Math/abs/length.js"),
            "built-ins/Math/abs"
        );
        assert_eq!(
            section_of("language/expressions/addition/x.js"),
            "language/expressions/addition"
        );
        assert_eq!(section_of("short.js"), "short.js");
    }
}
