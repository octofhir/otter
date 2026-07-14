//! Optimizing-tier promotion policy over hotness and feedback stability.
//!
//! # Contents
//! - [`OptimizingDecision`] classifies one function's current promotion state.
//! - [`Interpreter::optimizing_tier_decision`] derives the function's hotness
//!   from the existing baseline call counter, samples its feedback epoch, and
//!   returns the current classification.
//! - [`TierPolicy`] owns per-function feedback-stability history only.
//!
//! # Invariants
//! - The policy is deterministic and isolate-local; it reads no environment or
//!   process-global state.
//! - Function entry advances the shared tiering call counter. Entry selection
//!   samples feedback stability only while a hot function has no installed
//!   optimizing body; back-edge OSR policy remains independent.
//! - Promotion requires both sustained hotness and an unchanged feedback epoch
//!   across [`STABLE_SAMPLES`] consecutive checks.
//! - A decision itself never compiles or installs code. The function-entry path
//!   owns compilation after a `Promote` result; loop OSR remains unchanged.
//!
//! # See also
//! - [`crate::executable::CodeBlock::feedback_epoch`]
//! - [`crate::Interpreter`]

use rustc_hash::FxHashMap;

use crate::Interpreter;

/// Optimizing-tier hotness threshold, compared against the shared function-entry
/// call counter so a candidate stays hot well after baseline tier-up.
pub const OPTIMIZING_HOTNESS_THRESHOLD: u32 = 4_000;

/// Number of consecutive unchanged feedback-epoch checks required before promotion.
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
    last_feedback_epoch: Option<u32>,
    stable_streak: u8,
    observed_feedback_change: bool,
}

impl FunctionTierState {
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
        if self.stable_streak >= STABLE_SAMPLES {
            OptimizingDecision::Promote
        } else if self.observed_feedback_change {
            OptimizingDecision::FeedbackUnstable
        } else {
            OptimizingDecision::Warming
        }
    }
}

/// Isolate-local promotion telemetry. The table is touched only when a hot
/// entry without installed optimized code queries a decision.
#[derive(Debug, Default)]
pub(crate) struct TierPolicy {
    functions: FxHashMap<u32, FunctionTierState>,
}

impl TierPolicy {
    /// Classify one function from its shared entry hotness and its
    /// current feedback epoch. A cold function creates no state; a hot one
    /// accumulates only feedback-stability history. This is the sole mutator of
    /// the policy table and runs only from a decision query, never per call.
    fn sample_and_decide(
        &mut self,
        function_id: u32,
        hotness: u32,
        feedback_epoch: Option<u32>,
    ) -> OptimizingDecision {
        if hotness < OPTIMIZING_HOTNESS_THRESHOLD {
            return OptimizingDecision::Cold;
        }
        let Some(epoch) = feedback_epoch else {
            return OptimizingDecision::Cold;
        };
        let state = self.functions.entry(function_id).or_default();
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
    /// installs, or executes JIT code.
    #[must_use]
    pub fn optimizing_tier_decision(&mut self, function_id: u32) -> OptimizingDecision {
        // Hotness is read lazily from the shared entry counter, so cold calls do
        // not touch policy-local stability state.
        let hotness = self.jit_call_counts.get(&function_id).copied().unwrap_or(0);
        let feedback_epoch = self.code_space.feedback_epoch(function_id);
        self.optimizing_tier_policy
            .sample_and_decide(function_id, hotness, feedback_epoch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOT: u32 = OPTIMIZING_HOTNESS_THRESHOLD;

    #[test]
    fn cold_below_optimizing_threshold() {
        let mut policy = TierPolicy::default();

        for _ in 0..=STABLE_SAMPLES {
            assert_eq!(
                policy.sample_and_decide(7, OPTIMIZING_HOTNESS_THRESHOLD - 1, Some(0)),
                OptimizingDecision::Cold
            );
        }
    }

    #[test]
    fn hot_function_warms_while_building_initial_stability_history() {
        let mut policy = TierPolicy::default();

        assert_eq!(
            policy.sample_and_decide(11, HOT, Some(3)),
            OptimizingDecision::Warming
        );
        assert_eq!(
            policy.sample_and_decide(11, HOT, Some(3)),
            OptimizingDecision::Warming
        );
    }

    #[test]
    fn feedback_change_stays_unstable_for_the_full_sample_window() {
        let mut policy = TierPolicy::default();
        assert_eq!(
            policy.sample_and_decide(13, HOT, Some(1)),
            OptimizingDecision::Warming
        );
        assert_eq!(
            policy.sample_and_decide(13, HOT, Some(2)),
            OptimizingDecision::FeedbackUnstable
        );

        for _ in 1..STABLE_SAMPLES {
            assert_eq!(
                policy.sample_and_decide(13, HOT, Some(2)),
                OptimizingDecision::FeedbackUnstable
            );
        }
        assert_eq!(
            policy.sample_and_decide(13, HOT, Some(2)),
            OptimizingDecision::Promote
        );
    }

    #[test]
    fn hot_stable_feedback_promotes_at_exact_sample_boundary() {
        let mut policy = TierPolicy::default();
        assert_eq!(
            policy.sample_and_decide(17, HOT, Some(9)),
            OptimizingDecision::Warming
        );
        for _ in 1..STABLE_SAMPLES {
            assert_eq!(
                policy.sample_and_decide(17, HOT, Some(9)),
                OptimizingDecision::Warming
            );
        }
        assert_eq!(
            policy.sample_and_decide(17, HOT, Some(9)),
            OptimizingDecision::Promote
        );
    }

    #[test]
    fn identical_histories_are_deterministic() {
        let mut first = TierPolicy::default();
        let mut second = TierPolicy::default();
        let epochs = [4, 5, 5, 5, 5];

        let first_decisions: Vec<_> = epochs
            .into_iter()
            .map(|epoch| first.sample_and_decide(19, HOT, Some(epoch)))
            .collect();
        let second_decisions: Vec<_> = epochs
            .into_iter()
            .map(|epoch| second.sample_and_decide(19, HOT, Some(epoch)))
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
