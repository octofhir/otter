//! Optimizing-tier promotion policy over hotness and feedback stability.
//!
//! # Contents
//! - [`OptimizingDecision`] classifies one function's current promotion state.
//! - [`Interpreter::optimizing_tier_decision`] samples the function's feedback
//!   epoch and returns the current classification.
//! - [`TierPolicy`] owns saturating per-function hotness and feedback history.
//!
//! # Invariants
//! - The policy is deterministic and isolate-local; it reads no environment or
//!   process-global state.
//! - Promotion requires both sustained post-baseline hotness and an unchanged
//!   feedback epoch across [`STABLE_SAMPLES`] consecutive checks.
//! - A decision never compiles or installs code and is not consulted by the
//!   baseline function-entry or loop-OSR paths.
//! - All thresholds are provisional Phase 11 values to be tuned against real
//!   workloads when the optimizing compiler lands in Phase 12.
//!
//! # See also
//! - [`crate::executable::CodeBlock::feedback_epoch`]
//! - [`crate::Interpreter`]

use rustc_hash::FxHashMap;

use crate::Interpreter;

/// Provisional optimizing-tier hotness threshold.
///
/// This is four times the default baseline loop-OSR threshold (and eighty
/// times the baseline call threshold), so a candidate must remain hot well
/// after baseline tier-up. Phase 12 will tune it against production workloads.
pub const OPTIMIZING_HOTNESS_THRESHOLD: u32 = 4_000;

/// Provisional number of consecutive unchanged feedback-epoch checks required
/// before promotion. Phase 12 will tune this against deoptimization behavior.
pub const STABLE_SAMPLES: u8 = 3;

/// Optimizing-tier candidacy for one bytecode function.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OptimizingDecision {
    /// The function has not accumulated optimizing-tier hotness.
    Cold,
    /// The function is hot, but has not yet established a full stability
    /// history.
    Warming,
    /// Material feedback changed and has not yet remained unchanged for a full
    /// stability window.
    FeedbackUnstable,
    /// Both the hotness and feedback-stability gates are satisfied.
    Promote,
}

#[derive(Debug, Default)]
struct FunctionTierState {
    hotness: u32,
    last_feedback_epoch: Option<u32>,
    stable_streak: u8,
    observed_feedback_change: bool,
}

impl FunctionTierState {
    fn record_hotness(&mut self, amount: u32) {
        self.hotness = self.hotness.saturating_add(amount);
    }

    fn observe_feedback(&mut self, epoch: u32) {
        match self.last_feedback_epoch {
            None => {
                self.last_feedback_epoch = Some(epoch);
                self.stable_streak = 0;
            }
            Some(previous) if previous == epoch => {
                self.stable_streak = self.stable_streak.saturating_add(1);
            }
            Some(_) => {
                self.last_feedback_epoch = Some(epoch);
                self.stable_streak = 0;
                self.observed_feedback_change = true;
            }
        }
    }

    fn decision(&self) -> OptimizingDecision {
        if self.hotness < OPTIMIZING_HOTNESS_THRESHOLD {
            OptimizingDecision::Cold
        } else if self.stable_streak >= STABLE_SAMPLES {
            OptimizingDecision::Promote
        } else if self.observed_feedback_change {
            OptimizingDecision::FeedbackUnstable
        } else {
            OptimizingDecision::Warming
        }
    }
}

/// Isolate-local promotion telemetry. No current execution path consumes its
/// decisions; Phase 12 may consult it before requesting optimizing compilation.
#[derive(Debug, Default)]
pub(crate) struct TierPolicy {
    functions: FxHashMap<u32, FunctionTierState>,
}

impl TierPolicy {
    /// Add call or back-edge hotness without changing baseline counters.
    pub(crate) fn record_hotness(&mut self, function_id: u32, amount: u32) {
        self.functions
            .entry(function_id)
            .or_default()
            .record_hotness(amount);
    }

    /// Sample `feedback_epoch`, then classify from the tracked state. The
    /// classifier itself is pure; sampling mutates only this additive policy
    /// table and cannot affect baseline tier-up.
    fn sample_and_decide(
        &mut self,
        function_id: u32,
        feedback_epoch: Option<u32>,
    ) -> OptimizingDecision {
        let Some(state) = self.functions.get_mut(&function_id) else {
            return OptimizingDecision::Cold;
        };
        if state.hotness < OPTIMIZING_HOTNESS_THRESHOLD {
            return OptimizingDecision::Cold;
        }
        let Some(epoch) = feedback_epoch else {
            return OptimizingDecision::Cold;
        };
        state.observe_feedback(epoch);
        state.decision()
    }
}

impl Interpreter {
    /// Return the optimizing-tier candidacy of `function_id`.
    ///
    /// Each hot check samples the function's existing `feedback_epoch`; the
    /// epoch must then remain unchanged across [`STABLE_SAMPLES`] consecutive
    /// checks before this returns [`OptimizingDecision::Promote`]. Calling this
    /// method records only policy-local stability history. It never compiles,
    /// installs, or executes JIT code, and no baseline path calls it.
    #[must_use]
    pub fn optimizing_tier_decision(&mut self, function_id: u32) -> OptimizingDecision {
        let feedback_epoch = self.code_space.feedback_epoch(function_id);
        self.optimizing_tier_policy
            .sample_and_decide(function_id, feedback_epoch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hot_policy(function_id: u32) -> TierPolicy {
        let mut policy = TierPolicy::default();
        policy.record_hotness(function_id, OPTIMIZING_HOTNESS_THRESHOLD);
        policy
    }

    #[test]
    fn cold_below_optimizing_threshold() {
        let mut policy = TierPolicy::default();
        policy.record_hotness(7, OPTIMIZING_HOTNESS_THRESHOLD - 1);

        for _ in 0..=STABLE_SAMPLES {
            assert_eq!(
                policy.sample_and_decide(7, Some(0)),
                OptimizingDecision::Cold
            );
        }
    }

    #[test]
    fn hot_function_warms_while_building_initial_stability_history() {
        let mut policy = hot_policy(11);

        assert_eq!(
            policy.sample_and_decide(11, Some(3)),
            OptimizingDecision::Warming
        );
        assert_eq!(
            policy.sample_and_decide(11, Some(3)),
            OptimizingDecision::Warming
        );
    }

    #[test]
    fn feedback_change_stays_unstable_for_the_full_sample_window() {
        let mut policy = hot_policy(13);
        assert_eq!(
            policy.sample_and_decide(13, Some(1)),
            OptimizingDecision::Warming
        );
        assert_eq!(
            policy.sample_and_decide(13, Some(2)),
            OptimizingDecision::FeedbackUnstable
        );

        for _ in 1..STABLE_SAMPLES {
            assert_eq!(
                policy.sample_and_decide(13, Some(2)),
                OptimizingDecision::FeedbackUnstable
            );
        }
        assert_eq!(
            policy.sample_and_decide(13, Some(2)),
            OptimizingDecision::Promote
        );
    }

    #[test]
    fn hot_stable_feedback_promotes_at_exact_sample_boundary() {
        let mut policy = hot_policy(17);
        assert_eq!(
            policy.sample_and_decide(17, Some(9)),
            OptimizingDecision::Warming
        );
        for _ in 1..STABLE_SAMPLES {
            assert_eq!(
                policy.sample_and_decide(17, Some(9)),
                OptimizingDecision::Warming
            );
        }
        assert_eq!(
            policy.sample_and_decide(17, Some(9)),
            OptimizingDecision::Promote
        );
    }

    #[test]
    fn identical_histories_are_deterministic() {
        let mut first = hot_policy(19);
        let mut second = hot_policy(19);
        let epochs = [4, 5, 5, 5, 5];

        let first_decisions: Vec<_> = epochs
            .into_iter()
            .map(|epoch| first.sample_and_decide(19, Some(epoch)))
            .collect();
        let second_decisions: Vec<_> = epochs
            .into_iter()
            .map(|epoch| second.sample_and_decide(19, Some(epoch)))
            .collect();

        assert_eq!(first_decisions, second_decisions);
        assert_eq!(
            first_decisions,
            vec![
                OptimizingDecision::Warming,
                OptimizingDecision::FeedbackUnstable,
                OptimizingDecision::FeedbackUnstable,
                OptimizingDecision::FeedbackUnstable,
                OptimizingDecision::Promote,
            ]
        );
    }
}
