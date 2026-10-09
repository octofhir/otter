//! Interpreted execution evidence required before replacing deoptimized code.
//!
//! # Contents
//! - Optimized-code retirement and entry routing for one retraining function;
//!   callers that spliced its body keep their code until their own guards
//!   fail.
//! - Fresh-activation and complete-loop evidence carried by cold frame records.
//!
//! # Invariants
//! - The existing tier policy owns all per-function retraining state.
//! - Entry cells select the interpreter while retraining; entry, OSR and inline
//!   compilation consult the same state before selecting or preparing code.
//! - A deopt suffix is not a fresh activation. Two interpreted visits to the
//!   same loop header prove a complete iteration even in a non-returning loop.
//! - Frame evidence names the retraining generation, so an older recursive
//!   activation cannot complete a newer observation.
//! - One completed path ends retraining; the re-optimization work window is
//!   the tier policy's, earned in baseline code as well as interpreted.
//!
//! # See also
//! - [`crate::tier_policy`] for the deterministic work model and state owner.
//! - [`crate::cold_frame`] for activation-local evidence.

use crate::tier_policy::{CostedTier, RetrainingActivation};
use crate::{ExecutionContext, Frame, Interpreter};

impl Interpreter {
    pub(crate) fn begin_jit_retraining(&mut self, context: &ExecutionContext, fid: u32) {
        if let Ok(owner) = context.for_function(fid)
            && let Some(function) = owner.exec_function(fid)
        {
            self.optimizing_tier_policy.bind_source(function);
        }
        // Only optimized code embeds the failed speculation: drop this
        // function's own. Like V8, a caller that spliced the body keeps its
        // code and leaves through its own copy of the guard if that guard
        // ever fails there, recompiling from the feedback this retraining
        // collects. The baseline generation stays installed while the
        // stable cell routes entries to the interpreter.
        let mut affected = self.jit_code_registry.invalidate_own_optimizing(fid);
        self.jit_runtime_stats.caller_invalidations = self
            .jit_runtime_stats
            .caller_invalidations
            .saturating_add(affected.iter().filter(|&&caller| caller != fid).count() as u64);
        if affected.binary_search(&fid).is_err() {
            affected.push(fid);
            affected.sort_unstable();
        }
        self.jit_code_registry.set_retraining(fid, true);
        self.discard_invalidated_jit_state(&affected);
        self.optimizing_tier_policy.begin_retraining(fid);
    }

    /// Restore `fid`'s stable cell once its retraining evidence completes,
    /// and re-arm the baseline generation's source-work wakeup: its earlier
    /// optimized outcome suppressed further requests, and the deopt's
    /// feedback change starts a fresh optimizing work window.
    fn finish_jit_retraining_if_complete(&mut self, fid: u32) {
        if self.jit_retraining_blocks(fid) {
            return;
        }
        self.jit_code_registry.set_retraining(fid, false);
        if let Ok(context) = self.function_context(None, fid)
            && let Some(function) = context.exec_function(fid)
        {
            let decision = self.optimizing_tier_policy.decide(
                function,
                CostedTier::Optimizing,
                self.jit_code_registry.available_code_bytes(),
            );
            self.jit_code_registry
                .defer_generated_tiering(fid, decision.work_target);
        }
    }

    pub(crate) fn jit_retraining_blocks(&self, fid: u32) -> bool {
        self.optimizing_tier_policy
            .retraining_generation(fid)
            .is_some()
    }

    pub(crate) fn begin_interpreted_retraining_activation(&mut self, frame: &mut Frame) {
        let Some(generation) = self
            .optimizing_tier_policy
            .retraining_generation(frame.function_id)
        else {
            return;
        };
        self.frame_ensure_cold(frame).tier_retraining =
            Some(RetrainingActivation::new(generation, true));
    }

    pub(crate) fn note_interpreted_retraining_backedge(&mut self, frame: &mut Frame) {
        let Some(generation) = self
            .optimizing_tier_policy
            .retraining_generation(frame.function_id)
        else {
            return;
        };
        let (fid, header) = (frame.function_id, frame.pc);
        // A deopt suffix enters without a fresh activation token; its first
        // backedge starts the loop evidence of this generation.
        let token = &mut self.frame_ensure_cold(frame).tier_retraining;
        if token
            .as_ref()
            .is_none_or(|token| token.generation != generation)
        {
            *token = Some(RetrainingActivation::new(generation, false));
        }
        if token
            .as_mut()
            .is_some_and(|token| token.observe_backedge(header))
        {
            self.optimizing_tier_policy
                .note_completed_interpreted_path(fid, generation);
            self.finish_jit_retraining_if_complete(fid);
        }
    }

    pub(crate) fn complete_interpreted_retraining_activation(&mut self, frame: &mut Frame) {
        let token = self
            .frame_cold_mut(frame)
            .and_then(|cold| cold.tier_retraining.take());
        if let Some(token) = token
            && token.is_fresh()
        {
            self.optimizing_tier_policy
                .note_completed_interpreted_path(frame.function_id, token.generation);
            self.finish_jit_retraining_if_complete(frame.function_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deopt_suffix_needs_two_interpreted_backedges_to_complete_a_loop() {
        let mut vm = Interpreter::new().expect("fixture interpreter bootstrap");
        let mut frame = vm
            .test_frame_for_function(&otter_bytecode::Function::default())
            .unwrap();
        let fid = frame.function_id;
        vm.optimizing_tier_policy.begin_retraining(fid);
        frame.pc = 10;
        vm.note_interpreted_retraining_backedge(&mut frame);
        assert!(
            vm.jit_retraining_blocks(fid),
            "the first backedge may close only a deopt suffix"
        );
        vm.complete_interpreted_retraining_activation(&mut frame);
        assert!(
            vm.jit_retraining_blocks(fid),
            "a suffix return is no fresh activation"
        );
        vm.note_interpreted_retraining_backedge(&mut frame);
        vm.note_interpreted_retraining_backedge(&mut frame);
        assert!(
            !vm.jit_retraining_blocks(fid),
            "a complete loop can promote before return"
        );
        vm.frame_release_cold(&mut frame);
    }

    #[test]
    fn recursive_frame_completion_cannot_certify_a_newer_retraining_generation() {
        let mut vm = Interpreter::new().expect("fixture interpreter bootstrap");
        let function = otter_bytecode::Function::default();
        let mut outer = vm.test_frame_for_function(&function).unwrap();
        let mut inner = vm.test_frame_for_function(&function).unwrap();
        let fid = outer.function_id;
        assert_eq!(inner.function_id, fid);
        vm.optimizing_tier_policy.begin_retraining(fid);
        vm.begin_interpreted_retraining_activation(&mut outer);
        vm.optimizing_tier_policy.begin_retraining(fid);
        inner.pc = 4;
        vm.note_interpreted_retraining_backedge(&mut inner);
        vm.complete_interpreted_retraining_activation(&mut outer);
        vm.complete_interpreted_retraining_activation(&mut inner);
        assert!(
            vm.jit_retraining_blocks(fid),
            "old fresh entry and new suffix are independent"
        );
        vm.begin_interpreted_retraining_activation(&mut inner);
        vm.complete_interpreted_retraining_activation(&mut inner);
        assert!(!vm.jit_retraining_blocks(fid));
        vm.frame_release_cold(&mut outer);
        vm.frame_release_cold(&mut inner);
    }
}
