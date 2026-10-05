//! Indexed-element receiver feedback.
//!
//! # Contents
//! - Recording one receiver family at a `LoadElement` / `StoreElement` site.
//!
//! # Invariants
//! - Classification precedes the indexed operation, so getters, proxies, and
//!   throwing stores cannot leave a taken site looking unexecuted.
//! - One feedback-cell transition advances the owning function epoch exactly
//!   once; installed code keeps its own family guards and the next
//!   compilation reads the new family. Repeated observations are no-ops.
//! - Template runtime transitions use the logical PC published in their native
//!   frame, so generated execution enriches the same cell as interpretation.
//!
//! # See also
//! - [`crate::call_feedback`] — the corresponding call-site publication rule.
//! - [`crate::property_dispatch::elements`] — receiver classification.

use crate::{CodeBlock, Interpreter, Value, jit::JitElementFamily};
use otter_bytecode::Op;

impl Interpreter {
    /// Classify `receiver` and publish it at one computed-element site.
    ///
    /// A changed family describes a different immutable generated access
    /// contract. Retire the caller immediately instead of letting its exact
    /// representation guard deopt again on every later invocation.
    pub(crate) fn record_element_family_feedback(
        &mut self,
        code_block: &CodeBlock,
        instruction_pc: u32,
        receiver: Value,
    ) -> bool {
        let observed = self.element_family_of(receiver);
        self.commit_element_family_feedback_transition(code_block, instruction_pc, observed)
    }

    /// Publish one already-classified element family; returns whether the
    /// site's family changed. Installed code keeps its own family guards.
    fn commit_element_family_feedback_transition(
        &mut self,
        code_block: &CodeBlock,
        instruction_pc: u32,
        observed: JitElementFamily,
    ) -> bool {
        let Some(instruction) = code_block.instr_at_index(instruction_pc as usize) else {
            return false;
        };
        if !matches!(
            code_block.op(instruction),
            Op::LoadElement | Op::StoreElement | Op::StoreElementStrict
        ) {
            return false;
        }
        let changed = code_block
            .feedback_recorder_at(instruction_pc as usize)
            .is_some_and(|feedback| feedback.record_element_family(observed));
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use otter_bytecode::{Op, Operand};

    fn element_code_block() -> std::sync::Arc<CodeBlock> {
        CodeBlock::jit_test_stub(
            42,
            3,
            0,
            &[crate::jit::JitTestInstruction::new(
                Op::LoadElement,
                0,
                0,
                vec![
                    Operand::Register(0),
                    Operand::Register(1),
                    Operand::Register(2),
                ],
            )],
            &[],
        )
    }

    #[test]
    fn material_family_transition_advances_epoch_once() {
        let mut interpreter = Interpreter::new().expect("fixture interpreter bootstrap");
        let code_block = element_code_block();

        assert!(interpreter.commit_element_family_feedback_transition(
            &code_block,
            0,
            JitElementFamily::DenseFloat64
        ));
        assert_eq!(code_block.feedback_epoch(), 1);
        assert!(!interpreter.commit_element_family_feedback_transition(
            &code_block,
            0,
            JitElementFamily::DenseFloat64
        ));
        assert_eq!(code_block.feedback_epoch(), 1);

        // `Unseen` represents Empty/HoleyDouble. It is transient before a
        // site specializes, but generalizes an already-packed access.
        assert!(interpreter.commit_element_family_feedback_transition(
            &code_block,
            0,
            JitElementFamily::Unseen
        ));
        assert_eq!(code_block.feedback_epoch(), 2);
        assert_eq!(
            code_block.feedback_at(0).unwrap().element_family(),
            JitElementFamily::Generic,
        );
        assert!(!interpreter.commit_element_family_feedback_transition(
            &code_block,
            0,
            JitElementFamily::DenseTagged
        ));
        assert_eq!(code_block.feedback_epoch(), 2);
    }
}
