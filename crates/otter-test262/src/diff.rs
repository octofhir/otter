//! Baseline diff (`+N newly passing` / `-N regressed`).
//!
//! # Contents
//! - Exact path-set and corpus checks, then per-test outcome transitions.
//!
//! # Invariants
//! Missing tests never count as passing. New skips and changed skip policies
//! fail the comparison even when aggregate skip counts remain unchanged.
//!
//! # See also
//! - [`crate::report`] validates complete rows and their derived rollups.
//!
//! Spec: <https://tc39.es/ecma262/>

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::provenance::RunnerProvenance;
use crate::report::{Baseline, ReportError};
use crate::results::Outcome;

/// One row in the diff report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffRow {
    /// Test path.
    pub path: String,
    /// Outcome label in the previous baseline (`pass` / `fail` /
    /// `skip` / `crash` / `timeout` / `oom`).
    pub before: String,
    /// Outcome label in the current baseline.
    pub after: String,
    /// Reason for the *after* outcome (only set when transitioning
    /// to a non-pass state).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Diff result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffReport {
    /// Actual earlier executable and effective conditions.
    pub previous_runner: RunnerProvenance,
    /// Actual current executable and effective conditions.
    pub current_runner: RunnerProvenance,
    /// Tests that were not Pass before but are Pass now.
    pub newly_passing: Vec<DiffRow>,
    /// Tests that were Pass before but aren't now (or transitioned
    /// to a worse-than-skip state).
    pub regressed: Vec<DiffRow>,
    /// Paths without a newly-passing or regressed transition.
    pub unchanged: u64,
}

impl DiffReport {
    /// `true` iff there are no regressions.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.regressed.is_empty()
    }

    /// CI exit code (`0` = clean, `1` = at least one regression).
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        if self.is_clean() { 0 } else { 1 }
    }

    /// Render exact transitions and both actual executable identities.
    #[must_use]
    pub fn to_text(&self, previous_path: &str) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "runner SHA256: {} -> {}\n",
            self.previous_runner.executable_sha256, self.current_runner.executable_sha256
        ));
        out.push_str(&format!(
            "test262 diff against {previous_path}:\n  +{} newly passing\n   -{} regressed",
            self.newly_passing.len(),
            self.regressed.len()
        ));
        if !self.regressed.is_empty() {
            out.push_str(":\n");
            for row in &self.regressed {
                let reason = row
                    .reason
                    .as_deref()
                    .map(|r| format!(": {r}"))
                    .unwrap_or_default();
                out.push_str(&format!(
                    "     - {}  (was {}, now {}{})\n",
                    row.path, row.before, row.after, reason
                ));
            }
        } else {
            out.push('\n');
        }
        out.push_str(&format!("   ±0 unchanged: {}\n", self.unchanged));
        out
    }
}

/// Compare the same complete selection; an absent row never implies Pass.
/// New or changed skips are regressions, including failures hidden by a skip.
pub fn compute(previous: &Baseline, current: &Baseline) -> Result<DiffReport, ReportError> {
    previous.validate()?;
    current.validate_coverage(previous.tests.iter().map(|row| row.path.as_str()))?;
    if previous.test262_commit != current.test262_commit {
        return Err(ReportError::Invalid {
            message: "cannot compare different Test262 corpus identities".to_owned(),
        });
    }
    if !previous.runner.comparable_policy(&current.runner) {
        return Err(ReportError::Invalid {
            message: "cannot compare different target/assertion/effective semantic policy"
                .to_owned(),
        });
    }
    let previous_runner = previous.runner.clone();
    let previous: BTreeMap<_, _> = previous
        .tests
        .iter()
        .map(|row| (row.path.as_str(), &row.outcome))
        .collect();
    let mut report = DiffReport {
        previous_runner,
        current_runner: current.runner.clone(),
        newly_passing: Vec::new(),
        regressed: Vec::new(),
        unchanged: 0,
    };
    for row in &current.tests {
        let before = previous[row.path.as_str()];
        let after = &row.outcome;
        let change = || DiffRow {
            path: row.path.clone(),
            before: before.label().to_owned(),
            after: after.label().to_owned(),
            reason: after.detail(),
        };
        if !matches!(before, Outcome::Pass) && matches!(after, Outcome::Pass) {
            report.newly_passing.push(change());
        } else if matches!(after, Outcome::Skipped { .. }) && before != after
            || outcome_severity(after.label()) > outcome_severity(before.label())
        {
            report.regressed.push(change());
        } else {
            report.unchanged += 1;
        }
    }
    Ok(report)
}

/// Order outcomes by severity so transitions like `fail → crash`
/// register as a regression. `pass < skip < fail < timeout < oom <
/// crash`.
fn outcome_severity(label: &str) -> u8 {
    match label {
        "pass" => 0,
        "skip" => 1,
        "fail" => 2,
        "timeout" => 3,
        "oom" => 4,
        "crash" => 5,
        _ => 6,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::results::{Outcome, SkipReason, TestResult};

    fn baseline_from(results: &[TestResult]) -> Baseline {
        Baseline::from_results(
            results.to_vec(),
            crate::provenance::synthetic(),
            "t",
            "e",
            "now",
        )
        .unwrap()
    }

    fn pass(path: &str) -> TestResult {
        TestResult {
            path: path.to_string(),
            esid: None,
            features: vec![],
            outcome: Outcome::Pass,
            wall_ms: 0,
        }
    }

    fn fail(path: &str, reason: &str) -> TestResult {
        TestResult {
            path: path.to_string(),
            esid: None,
            features: vec![],
            outcome: Outcome::Fail {
                reason: reason.to_string(),
                stack: None,
            },
            wall_ms: 0,
        }
    }

    fn crash(path: &str) -> TestResult {
        TestResult {
            path: path.to_string(),
            esid: None,
            features: vec![],
            outcome: Outcome::Crash {
                panic: "oops".to_string(),
            },
            wall_ms: 0,
        }
    }

    #[test]
    fn self_diff_is_clean() {
        let b = baseline_from(&[pass("a.js"), fail("b.js", "x")]);
        let d = compute(&b, &b).unwrap();
        assert!(d.is_clean());
        assert_eq!(d.exit_code(), 0);
        assert_eq!(d.regressed.len(), 0);
        assert_eq!(d.newly_passing.len(), 0);
    }

    #[test]
    fn regression_pass_to_fail_flagged() {
        let prev = baseline_from(&[pass("a.js"), pass("b.js")]);
        let cur = baseline_from(&[pass("a.js"), fail("b.js", "broke")]);
        let d = compute(&prev, &cur).unwrap();
        assert!(!d.is_clean());
        assert_eq!(d.exit_code(), 1);
        assert_eq!(d.regressed.len(), 1);
        assert_eq!(d.regressed[0].path, "b.js");
        assert_eq!(d.regressed[0].before, "pass");
        assert_eq!(d.regressed[0].after, "fail");
    }

    #[test]
    fn newly_passing_tests_listed() {
        let prev = baseline_from(&[fail("a.js", "x")]);
        let cur = baseline_from(&[pass("a.js")]);
        let d = compute(&prev, &cur).unwrap();
        assert_eq!(d.newly_passing.len(), 1);
        assert_eq!(d.newly_passing[0].path, "a.js");
    }

    #[test]
    fn fail_to_crash_is_regression() {
        let prev = baseline_from(&[fail("a.js", "x")]);
        let cur = baseline_from(&[crash("a.js")]);
        let d = compute(&prev, &cur).unwrap();
        assert!(!d.is_clean());
        assert_eq!(d.regressed[0].after, "crash");
    }

    #[test]
    fn text_format_matches_template() {
        let prev = baseline_from(&[pass("a.js"), pass("b.js")]);
        let cur = baseline_from(&[pass("a.js"), fail("b.js", "broke")]);
        let d = compute(&prev, &cur).unwrap();
        let text = d.to_text("docs/.../prev.json");
        assert!(text.contains("newly passing"));
        assert!(text.contains("regressed"));
        assert!(text.contains("b.js"));
    }
    #[test]
    fn same_count_skip_swap_is_a_regression() {
        let mut skipped = pass("a.js");
        skipped.outcome = Outcome::Skipped {
            reason: SkipReason::Feature {
                feature: "Atomics".to_owned(),
            },
        };
        let previous = baseline_from(&[skipped.clone(), pass("b.js")]);
        skipped.path = "b.js".to_owned();
        let current = baseline_from(&[pass("a.js"), skipped]);
        let diff = compute(&previous, &current).unwrap();
        assert_eq!(diff.newly_passing[0].path, "a.js");
        assert_eq!(diff.regressed[0].path, "b.js");
        assert!(!diff.is_clean());
    }

    #[test]
    fn missing_failure_never_becomes_a_pass_and_skip_never_hides_a_failure() {
        let previous = baseline_from(&[fail("a.js", "failure"), pass("b.js")]);
        let missing = baseline_from(&[pass("b.js")]);
        assert!(compute(&previous, &missing).is_err());
        let mut skipped = pass("a.js");
        skipped.outcome = Outcome::Skipped {
            reason: SkipReason::MissingFrontmatter,
        };
        let hidden = baseline_from(&[skipped, pass("b.js")]);
        let diff = compute(&previous, &hidden).unwrap();
        assert_eq!(diff.regressed.len(), 1);
        assert!(diff.newly_passing.is_empty());
    }

    #[test]
    fn intentional_binary_change_is_recorded_but_policy_change_is_rejected() {
        let previous = baseline_from(&[pass("a.js")]);
        let mut current = previous.clone();
        current.runner.executable_sha256 = "1".repeat(64);
        let diff = compute(&previous, &current).unwrap();
        assert_ne!(
            diff.previous_runner.executable_sha256,
            diff.current_runner.executable_sha256
        );
        current
            .runner
            .semantic_config
            .skip_flags
            .push("module".to_owned());
        assert!(compute(&previous, &current).is_err());
    }
}
