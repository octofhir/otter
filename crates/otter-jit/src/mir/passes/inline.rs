//! Simple function inlining — monomorphic call targets.
//!
//! Inlines small monomorphic call targets (< MAX_INLINE_SIZE bytecodes)
//! into the caller's MIR graph. This eliminates call overhead and enables
//! cross-function optimizations (constant propagation across boundaries).
//!
//! ## Constraints
//!
//! - Only inline monomorphic targets (one observed callee at a call site).
//! - Callee must be "small" (< 50 bytecodes by default).
//! - Budget: max 200 inlined nodes per function (prevent graph explosion).
//! - Max 1 level of inlining depth (no recursive inlining).
//!
//! V8 Maglev: limited inlining, small hot callees only.
//! JSC DFG: inlines based on code size and call frequency.
//!
//! Spec: Phase 4.5 of JIT_INCREMENTAL_PLAN.md

use otter_vm::feedback::{CallFeedback, FeedbackSlotId, FeedbackVector};

/// Maximum bytecode size of a function that can be inlined.
pub const MAX_INLINE_BYTECODE_SIZE: usize = 50;

/// Maximum total inlined nodes per caller function.
pub const MAX_INLINE_NODE_BUDGET: usize = 200;

/// Maximum inlining depth (1 = only inline direct callees, not their callees).
pub const MAX_INLINE_DEPTH: u8 = 1;

/// Inlining decision for a call site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InlineDecision {
    /// Inline the callee.
    Inline {
        /// Function index of the callee in the module.
        target_index: u32,
    },
    /// Don't inline (too large, polymorphic, budget exhausted, etc.).
    DontInline(DontInlineReason),
}

/// Why a call site was not inlined.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DontInlineReason {
    /// No monomorphic feedback — call target unknown.
    NotMonomorphic,
    /// Callee bytecode too large.
    TooLarge,
    /// Inlining budget exhausted.
    BudgetExhausted,
    /// Max inlining depth reached.
    MaxDepthReached,
    /// Callee is recursive.
    Recursive,
}

/// Inlining budget tracker.
#[derive(Debug, Clone)]
pub struct InlineBudget {
    /// Remaining node budget.
    remaining: usize,
    /// Current inlining depth.
    depth: u8,
    /// Caller function index (to detect recursion).
    caller_index: u32,
}

impl InlineBudget {
    /// Create a new inlining budget for a caller function.
    #[must_use]
    pub fn new(caller_index: u32) -> Self {
        Self {
            remaining: MAX_INLINE_NODE_BUDGET,
            depth: 0,
            caller_index,
        }
    }

    /// Decide whether to inline a call site.
    #[must_use]
    pub fn decide(
        &self,
        feedback: &FeedbackVector,
        slot: FeedbackSlotId,
        callee_bytecode_size: Option<usize>,
    ) -> InlineDecision {
        // Check depth.
        if self.depth >= MAX_INLINE_DEPTH {
            return InlineDecision::DontInline(DontInlineReason::MaxDepthReached);
        }

        // Check feedback: must be monomorphic.
        let target = match feedback.call(slot) {
            Some(CallFeedback::Monomorphic(t)) => *t,
            _ => return InlineDecision::DontInline(DontInlineReason::NotMonomorphic),
        };

        // Check recursion.
        if target == self.caller_index {
            return InlineDecision::DontInline(DontInlineReason::Recursive);
        }

        // Check callee size.
        if let Some(size) = callee_bytecode_size {
            if size > MAX_INLINE_BYTECODE_SIZE {
                return InlineDecision::DontInline(DontInlineReason::TooLarge);
            }
            // Check budget.
            if size > self.remaining {
                return InlineDecision::DontInline(DontInlineReason::BudgetExhausted);
            }
        }

        InlineDecision::Inline { target_index: target }
    }

    /// Consume budget for an inlined callee.
    pub fn consume(&mut self, node_count: usize) {
        self.remaining = self.remaining.saturating_sub(node_count);
    }

    /// Enter a deeper inlining level.
    pub fn enter_inline(&mut self) {
        self.depth += 1;
    }

    /// Exit an inlining level.
    pub fn exit_inline(&mut self) {
        self.depth = self.depth.saturating_sub(1);
    }

    /// Remaining budget.
    #[must_use]
    pub fn remaining(&self) -> usize {
        self.remaining
    }
}

/// Run the inlining pass (placeholder — actual graph splicing is future work).
///
/// For now, this pass identifies inlinable call sites and records decisions.
/// Actual MIR graph splicing requires cloning callee MIR, remapping values,
/// and inserting guard-for-call-target + inlined body + merge block.
pub fn run(graph: &mut crate::mir::graph::MirGraph) {
    // Inlining pass requires FeedbackVector and Module access,
    // which the `fn run(&mut MirGraph)` signature doesn't provide.
    // This is a no-op placeholder. The actual inlining is done in
    // the speculative builder (Phase 4.1) which has access to feedback.
    let _ = graph;
}

#[cfg(test)]
mod tests {
    use super::*;
    use otter_vm::feedback::*;

    fn make_feedback_mono_call(target: u32) -> FeedbackVector {
        let layout = FeedbackTableLayout::new(vec![
            FeedbackSlotLayout::new(FeedbackSlotId(0), FeedbackKind::Call),
        ]);
        let mut fv = FeedbackVector::from_layout(&layout);
        fv.record_call(FeedbackSlotId(0), target);
        fv
    }

    #[test]
    fn test_inline_monomorphic_small() {
        let fv = make_feedback_mono_call(5);
        let budget = InlineBudget::new(0); // caller is fn 0

        let decision = budget.decide(&fv, FeedbackSlotId(0), Some(30));
        assert_eq!(decision, InlineDecision::Inline { target_index: 5 });
    }

    #[test]
    fn test_dont_inline_too_large() {
        let fv = make_feedback_mono_call(5);
        let budget = InlineBudget::new(0);

        let decision = budget.decide(&fv, FeedbackSlotId(0), Some(100)); // > 50
        assert_eq!(
            decision,
            InlineDecision::DontInline(DontInlineReason::TooLarge)
        );
    }

    #[test]
    fn test_dont_inline_polymorphic() {
        let layout = FeedbackTableLayout::new(vec![
            FeedbackSlotLayout::new(FeedbackSlotId(0), FeedbackKind::Call),
        ]);
        let mut fv = FeedbackVector::from_layout(&layout);
        fv.record_call(FeedbackSlotId(0), 5);
        fv.record_call(FeedbackSlotId(0), 6); // Now polymorphic.

        let budget = InlineBudget::new(0);
        let decision = budget.decide(&fv, FeedbackSlotId(0), Some(10));
        assert_eq!(
            decision,
            InlineDecision::DontInline(DontInlineReason::NotMonomorphic)
        );
    }

    #[test]
    fn test_dont_inline_recursive() {
        let fv = make_feedback_mono_call(0); // target == caller
        let budget = InlineBudget::new(0);

        let decision = budget.decide(&fv, FeedbackSlotId(0), Some(10));
        assert_eq!(
            decision,
            InlineDecision::DontInline(DontInlineReason::Recursive)
        );
    }

    #[test]
    fn test_budget_exhaustion() {
        let fv = make_feedback_mono_call(5);
        let mut budget = InlineBudget::new(0);
        budget.consume(190); // 10 remaining

        let decision = budget.decide(&fv, FeedbackSlotId(0), Some(30)); // > 10
        assert_eq!(
            decision,
            InlineDecision::DontInline(DontInlineReason::BudgetExhausted)
        );
    }

    #[test]
    fn test_max_depth() {
        let fv = make_feedback_mono_call(5);
        let mut budget = InlineBudget::new(0);
        budget.enter_inline(); // depth = 1 = MAX_INLINE_DEPTH

        let decision = budget.decide(&fv, FeedbackSlotId(0), Some(10));
        assert_eq!(
            decision,
            InlineDecision::DontInline(DontInlineReason::MaxDepthReached)
        );
    }
}
