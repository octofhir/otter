//! Bounded ordinary-call target distributions for future optimizing tiers.
//!
//! The dense instruction cell retains the baseline tier's compact
//! unseen/mono/poly state. This Interpreter-owned side table adds target hit
//! counts and a bounded polymorphic set without changing any current compile or
//! tier-up decision.
//!
//! # Contents
//! - [`CallSiteDistribution`] — mono/poly/megamorphic state for one `Op::Call`.
//! - [`Interpreter::record_ordinary_call_feedback`] — additive recording keyed
//!   by caller function id and canonical instruction index.
//!
//! # Invariants
//! - At most [`MAX_CALL_TARGETS`] function ids and their saturating hit counts
//!   are retained at one site; the next distinct id makes it permanently
//!   megamorphic.
//! - Target order is first-observation order and table keys use a `BTreeMap`, so
//!   telemetry is deterministic.
//! - Existing baseline decisions never read this table.
//!
//! # See also
//! - [`crate::jit_feedback`] — compact per-instruction feedback and epochs.
//! - [`crate::MethodCallFeedback`] — bounded method-call distribution design.

use std::collections::{BTreeMap, btree_map::Entry};

use smallvec::SmallVec;

use crate::{CodeBlock, Interpreter, jit_feedback::CallTargetTransition};

/// Maximum distinct bytecode callees retained at one ordinary-call site.
pub(crate) const MAX_CALL_TARGETS: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CallTargetCount {
    pub(crate) fid: u32,
    pub(crate) hits: u32,
}

/// Bounded target population observed at one ordinary bytecode call site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CallSiteDistribution {
    Mono(CallTargetCount),
    Poly(Box<SmallVec<[CallTargetCount; MAX_CALL_TARGETS]>>),
    Megamorphic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DistributionTransition {
    Unchanged,
    /// The dense eight-byte cell makes the same unseen-to-mono or mono-to-poly
    /// transition and therefore already advances the CodeBlock epoch.
    MirroredByDenseCell,
    /// The bounded target set gained information beyond dense mono/poly state.
    Extended,
}

impl CallSiteDistribution {
    fn record(&mut self, callee_fid: u32) -> DistributionTransition {
        match self {
            Self::Mono(target) if target.fid == callee_fid => {
                target.hits = target.hits.saturating_add(1);
                DistributionTransition::Unchanged
            }
            Self::Mono(target) => {
                let prior = *target;
                let mut targets: SmallVec<[CallTargetCount; MAX_CALL_TARGETS]> = SmallVec::new();
                targets.push(prior);
                targets.push(CallTargetCount {
                    fid: callee_fid,
                    hits: 1,
                });
                *self = Self::Poly(Box::new(targets));
                DistributionTransition::MirroredByDenseCell
            }
            Self::Poly(targets) => {
                if let Some(target) = targets.iter_mut().find(|target| target.fid == callee_fid) {
                    target.hits = target.hits.saturating_add(1);
                    DistributionTransition::Unchanged
                } else if targets.len() < MAX_CALL_TARGETS {
                    targets.push(CallTargetCount {
                        fid: callee_fid,
                        hits: 1,
                    });
                    DistributionTransition::Extended
                } else {
                    *self = Self::Megamorphic;
                    DistributionTransition::Extended
                }
            }
            Self::Megamorphic => DistributionTransition::Unchanged,
        }
    }
}

impl Interpreter {
    /// Record both compact and bounded ordinary-call feedback for one site.
    ///
    /// Dense and side-table transitions that describe the same first or second
    /// target share one epoch bump. Later distinct targets and saturation add
    /// their own single transition without feeding any current JIT decision.
    pub(crate) fn record_ordinary_call_feedback(
        &mut self,
        code_block: &CodeBlock,
        instruction_pc: u32,
        callee_fid: u32,
    ) -> CallTargetTransition {
        let Some(feedback) = code_block.feedback_recorder_at(instruction_pc as usize) else {
            return CallTargetTransition::Unchanged;
        };
        let transition = feedback.record_call_target(callee_fid);
        if self.note_call_target_distribution(code_block.id, instruction_pc, callee_fid) {
            code_block.bump_feedback_epoch();
        }
        transition
    }

    /// Record one ordinary bytecode callee in the optimizing-tier side table.
    ///
    /// Returns `true` only when the distribution gained information beyond the
    /// dense cell's mirrored unseen/mono/poly transitions and therefore needs
    /// one additional CodeBlock epoch increment.
    fn note_call_target_distribution(
        &mut self,
        caller_fid: u32,
        instruction_pc: u32,
        callee_fid: u32,
    ) -> bool {
        let transition = match self
            .jit_call_site_feedback
            .entry((caller_fid, instruction_pc))
        {
            Entry::Vacant(entry) => {
                entry.insert(CallSiteDistribution::Mono(CallTargetCount {
                    fid: callee_fid,
                    hits: 1,
                }));
                DistributionTransition::MirroredByDenseCell
            }
            Entry::Occupied(mut entry) => entry.get_mut().record(callee_fid),
        };
        matches!(transition, DistributionTransition::Extended)
    }
}

pub(crate) type CallSiteFeedbackTable = BTreeMap<(u32, u32), CallSiteDistribution>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distribution_counts_targets_to_bound_then_saturates() {
        let mut distribution = CallSiteDistribution::Mono(CallTargetCount { fid: 10, hits: 1 });

        assert_eq!(distribution.record(10), DistributionTransition::Unchanged);
        assert_eq!(
            distribution.record(11),
            DistributionTransition::MirroredByDenseCell
        );
        let CallSiteDistribution::Poly(targets) = &distribution else {
            panic!("second distinct target must make the site polymorphic");
        };
        assert_eq!(targets.as_slice()[0], CallTargetCount { fid: 10, hits: 2 });
        assert_eq!(targets.as_slice()[1], CallTargetCount { fid: 11, hits: 1 });

        for fid in 12..(10 + MAX_CALL_TARGETS as u32) {
            assert_eq!(distribution.record(fid), DistributionTransition::Extended);
        }
        let CallSiteDistribution::Poly(targets) = &distribution else {
            panic!("the bounded target set must remain polymorphic at its cap");
        };
        assert_eq!(targets.len(), MAX_CALL_TARGETS);
        assert_eq!(distribution.record(17), DistributionTransition::Unchanged);
        let CallSiteDistribution::Poly(targets) = &distribution else {
            panic!("a repeated target at the cap must remain polymorphic");
        };
        assert_eq!(targets.last().map(|target| target.hits), Some(2));

        assert_eq!(
            distribution.record(10 + MAX_CALL_TARGETS as u32),
            DistributionTransition::Extended
        );
        assert_eq!(distribution, CallSiteDistribution::Megamorphic);
        assert_eq!(
            distribution.record(u32::MAX),
            DistributionTransition::Unchanged
        );
    }

    #[test]
    fn hit_counts_saturate_without_changing_distribution_state() {
        let mut distribution = CallSiteDistribution::Mono(CallTargetCount {
            fid: u32::MAX,
            hits: u32::MAX,
        });
        assert_eq!(
            distribution.record(u32::MAX),
            DistributionTransition::Unchanged
        );
        assert_eq!(
            distribution,
            CallSiteDistribution::Mono(CallTargetCount {
                fid: u32::MAX,
                hits: u32::MAX,
            })
        );
    }

    #[test]
    fn ordinary_call_epoch_tracks_each_distinct_target_and_saturation_once() {
        let code_block = CodeBlock::jit_test_stub(
            42,
            0,
            0,
            &[crate::jit::JitTestInstruction::new(
                otter_bytecode::Op::Nop,
                0,
                0,
                Vec::new(),
            )],
        );
        let mut interpreter = Interpreter::new();

        for fid in 0..MAX_CALL_TARGETS as u32 {
            let transition = interpreter.record_ordinary_call_feedback(&code_block, 0, fid);
            let expected = match fid {
                0 => CallTargetTransition::BecameMonomorphic,
                1 => CallTargetTransition::BecamePolymorphic,
                _ => CallTargetTransition::Unchanged,
            };
            assert_eq!(transition, expected);
            assert_eq!(code_block.feedback_epoch(), fid + 1);
        }

        assert_eq!(
            interpreter.record_ordinary_call_feedback(&code_block, 0, 0),
            CallTargetTransition::Unchanged
        );
        assert_eq!(code_block.feedback_epoch(), MAX_CALL_TARGETS as u32);

        assert_eq!(
            interpreter.record_ordinary_call_feedback(&code_block, 0, MAX_CALL_TARGETS as u32),
            CallTargetTransition::Unchanged
        );
        assert_eq!(code_block.feedback_epoch(), MAX_CALL_TARGETS as u32 + 1);
        assert!(matches!(
            interpreter.jit_call_site_feedback.get(&(42, 0)),
            Some(CallSiteDistribution::Megamorphic)
        ));

        interpreter.record_ordinary_call_feedback(&code_block, 0, u32::MAX);
        assert_eq!(code_block.feedback_epoch(), MAX_CALL_TARGETS as u32 + 1);
    }
}
