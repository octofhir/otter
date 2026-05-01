//! Baseline diff (`+N newly passing` / `-N regressed`).
//!
//! Loaded from two [`Baseline`] JSONs and reports per-test
//! transitions. The CI gate (slice 105) fails on any regression.
//!
//! Spec: <https://tc39.es/ecma262/>

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::report::{Baseline, FailingTest};

/// One row in the diff report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffRow {
    /// Test path.
    pub path: String,
    /// Outcome label in the previous baseline (`pass` / `fail` /
    /// `skip` / `crash` / `timeout` / `oom` / `missing`).
    pub before: String,
    /// Outcome label in the current baseline.
    pub after: String,
    /// Reason for the *after* outcome (only set when transitioning
    /// to a non-pass state).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Diff result.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DiffReport {
    /// Tests that were not Pass before but are Pass now.
    pub newly_passing: Vec<DiffRow>,
    /// Tests that were Pass before but aren't now (or transitioned
    /// to a worse-than-skip state).
    pub regressed: Vec<DiffRow>,
    /// Tests whose outcome did not change.
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

    /// Render the diff in the format from task 100 §"`--diff <previous>` mode".
    #[must_use]
    pub fn to_text(&self, previous_path: &str) -> String {
        let mut out = String::new();
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

/// Compute the diff between two baselines.
///
/// `previous` and `current` are typically loaded via
/// [`Baseline::from_path`]. The diff treats the union of the two
/// `failing_tests` lists as the "non-Pass set"; any test missing
/// from both is implicitly Pass on both sides.
///
/// **Limitation:** the canonical baseline JSON only carries the
/// failing tests, not the full per-test result list. So the diff
/// cannot tell `Pass-now / Skip-before` apart from `Pass-now /
/// Pass-before` — both look identical when neither side records the
/// test. This matches how V8/SpiderMonkey/Hermes publish their
/// matrices.
#[must_use]
pub fn compute(previous: &Baseline, current: &Baseline) -> DiffReport {
    let prev_failing: BTreeMap<&str, &FailingTest> = previous
        .failing_tests
        .iter()
        .map(|f| (f.path.as_str(), f))
        .collect();
    let cur_failing: BTreeMap<&str, &FailingTest> = current
        .failing_tests
        .iter()
        .map(|f| (f.path.as_str(), f))
        .collect();

    let mut newly_passing = Vec::new();
    let mut regressed = Vec::new();

    // Anything failing before that is no longer failing → newly passing.
    for (path, prev_row) in &prev_failing {
        if !cur_failing.contains_key(path) {
            newly_passing.push(DiffRow {
                path: (*path).to_string(),
                before: prev_row.outcome.clone(),
                after: "pass".to_string(),
                reason: None,
            });
        }
    }

    // Anything failing now that wasn't failing before → regressed.
    // Anything failing in both whose outcome label changed for the
    // worse → also a regression (covers `fail → crash`, etc.).
    for (path, cur_row) in &cur_failing {
        match prev_failing.get(path) {
            None => regressed.push(DiffRow {
                path: (*path).to_string(),
                before: "pass".to_string(),
                after: cur_row.outcome.clone(),
                reason: Some(cur_row.reason.clone()),
            }),
            Some(prev_row)
                if outcome_severity(&cur_row.outcome) > outcome_severity(&prev_row.outcome) =>
            {
                regressed.push(DiffRow {
                    path: (*path).to_string(),
                    before: prev_row.outcome.clone(),
                    after: cur_row.outcome.clone(),
                    reason: Some(cur_row.reason.clone()),
                });
            }
            _ => {}
        }
    }

    // Tests that flipped within the same severity (still failing
    // for the same reason) count as unchanged for the gate; the
    // unchanged tally below absorbs both "still pass" and "still
    // fail with the same severity" states.
    let touched = (newly_passing.len() + regressed.len()) as u64;
    let union: u64 = prev_failing
        .keys()
        .chain(cur_failing.keys())
        .collect::<std::collections::HashSet<_>>()
        .len() as u64;
    let total = current.totals.total.max(previous.totals.total);
    let unchanged = total.saturating_sub(touched.max(union));

    DiffReport {
        newly_passing,
        regressed,
        unchanged,
    }
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
    use crate::runner::{Outcome, TestResult};

    fn baseline_from(results: &[TestResult]) -> Baseline {
        Baseline::from_results(results, "t", "e", "now")
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
        let d = compute(&b, &b);
        assert!(d.is_clean());
        assert_eq!(d.exit_code(), 0);
        assert_eq!(d.regressed.len(), 0);
        assert_eq!(d.newly_passing.len(), 0);
    }

    #[test]
    fn regression_pass_to_fail_flagged() {
        let prev = baseline_from(&[pass("a.js"), pass("b.js")]);
        let cur = baseline_from(&[pass("a.js"), fail("b.js", "broke")]);
        let d = compute(&prev, &cur);
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
        let d = compute(&prev, &cur);
        assert_eq!(d.newly_passing.len(), 1);
        assert_eq!(d.newly_passing[0].path, "a.js");
    }

    #[test]
    fn fail_to_crash_is_regression() {
        let prev = baseline_from(&[fail("a.js", "x")]);
        let cur = baseline_from(&[crash("a.js")]);
        let d = compute(&prev, &cur);
        assert!(!d.is_clean());
        assert_eq!(d.regressed[0].after, "crash");
    }

    #[test]
    fn text_format_matches_template() {
        let prev = baseline_from(&[pass("a.js"), pass("b.js")]);
        let cur = baseline_from(&[pass("a.js"), fail("b.js", "broke")]);
        let d = compute(&prev, &cur);
        let text = d.to_text("docs/.../prev.json");
        assert!(text.contains("newly passing"));
        assert!(text.contains("regressed"));
        assert!(text.contains("b.js"));
    }
}
