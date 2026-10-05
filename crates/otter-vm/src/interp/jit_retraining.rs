//! Interpreted execution evidence required before replacing deoptimized code.
//!
//! # Contents
//! - Native retirement and deterministic source-work admission for one function.
//! - Fresh-activation and complete-loop evidence carried by cold frame records.
//! - One work debit for each opcode actually dispatched by the interpreter.
//!
//! # Invariants
//! - The existing tier policy owns all per-function retraining state.
//! - Entry cells select the interpreter while retraining; entry, OSR and inline
//!   compilation consult the same state before selecting or preparing code.
//! - A deopt suffix is not a fresh activation. Two interpreted visits to the
//!   same loop header prove a complete iteration even in a non-returning loop.
//! - Frame evidence names the retraining generation, so an older recursive
//!   activation cannot complete a newer observation.
//!
//! # See also
//! - [`crate::tier_policy`] for the deterministic work model and state owner.
//! - [`crate::cold_frame`] for activation-local evidence.

use crate::tier_policy::{CostedTier, RetrainingActivation, TierWorkInput, TierWorkModel};
use crate::{ExecutionContext, Frame, Interpreter};

impl Interpreter {
    pub(crate) fn begin_jit_retraining(&mut self, context: &ExecutionContext, fid: u32) {
        let work = context
            .for_function(fid)
            .ok()
            .and_then(|owner| {
                let function = owner.exec_function(fid)?;
                let instructions = u64::try_from(function.code.len())
                    .unwrap_or(u64::MAX)
                    .max(1);
                self.optimizing_tier_policy.bind_source(function);
                Some(
                    TierWorkModel::calibrated().minimum_required_work(TierWorkInput {
                        tier: CostedTier::Optimizing,
                        observed_work: 0,
                        bytecode_instructions: instructions,
                        register_count: u64::from(function.register_count),
                        parameter_count: u64::from(function.param_count),
                        available_code_bytes: self.jit_code_registry.available_code_bytes(),
                        previous_compile_attempts: self
                            .optimizing_tier_policy
                            .compile_attempts(fid, CostedTier::Optimizing),
                    }),
                )
            })
            .unwrap_or(1);
        self.invalidate_jit_function(fid);
        self.optimizing_tier_policy.begin_retraining(fid, work);
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

    pub(crate) fn note_interpreted_retraining_step(&mut self, frame: &mut Frame) {
        let Some(generation) = self
            .optimizing_tier_policy
            .retraining_generation(frame.function_id)
        else {
            return;
        };
        let fid = frame.function_id;
        let token = &mut self.frame_ensure_cold(frame).tier_retraining;
        if token
            .as_ref()
            .is_none_or(|token| token.generation != generation)
        {
            *token = Some(RetrainingActivation::new(generation, false));
        }
        self.optimizing_tier_policy
            .note_interpreted_work(fid, generation);
    }

    pub(crate) fn note_interpreted_retraining_backedge(&mut self, frame: &mut Frame) {
        let Some(generation) = self
            .optimizing_tier_policy
            .retraining_generation(frame.function_id)
        else {
            return;
        };
        let (fid, header) = (frame.function_id, frame.pc);
        let token = self
            .frame_cold_mut(frame)
            .and_then(|cold| cold.tier_retraining.as_mut());
        if token
            .is_some_and(|token| token.generation == generation && token.observe_backedge(header))
        {
            self.optimizing_tier_policy
                .note_completed_interpreted_path(fid, generation);
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
        vm.optimizing_tier_policy.begin_retraining(fid, 2);
        vm.note_interpreted_retraining_step(&mut frame);
        frame.pc = 10;
        vm.note_interpreted_retraining_backedge(&mut frame);
        assert!(vm.jit_retraining_blocks(fid));
        vm.note_interpreted_retraining_step(&mut frame);
        assert!(
            vm.jit_retraining_blocks(fid),
            "exhausted work cannot certify a deopt suffix"
        );
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
        vm.optimizing_tier_policy.begin_retraining(fid, 1);
        vm.begin_interpreted_retraining_activation(&mut outer);
        vm.optimizing_tier_policy.begin_retraining(fid, 1);
        vm.note_interpreted_retraining_step(&mut inner);
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
