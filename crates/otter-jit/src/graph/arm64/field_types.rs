//! AArch64 dependency-flag tests and nullish rejections of field bounds.
//!
//! # Contents
//! - `emit_dependency_check` tests the generation's dependency flag.
//! - `emit_not_nullish` rejects the alternatives before a shaped dereference.
//!
//! # Invariants
//! - The flag is set only by mutator writers, and generated code runs on the
//!   mutator: a plain byte load sees every set before it.
//! - Tests allocate nothing and leave by eager reconstruction at their exact
//!   source instruction, including the active inline-frame chain.
//! - Only reserved scratch registers are modified.
//!
//! # See also
//! - [`otter_vm::code_dependencies`] owns subscriptions and the flag.
//! - [`super::super::dependency_checks`] places the tests.

use super::*;
use otter_vm::object::field_type;

impl Codegen<'_> {
    pub(super) fn emit_dependency_check(&mut self, node: NodeId) {
        let exit = self.eager_exit(node, DeoptReason::DependencyChanged);
        emit_load_symbol_u64(
            &mut self.ops,
            &mut self.relocations,
            16,
            self.view.dependency_flag.address() as u64,
            RelocationTarget::DependencyFlag,
        );
        dynasm!(self.ops ; .arch aarch64 ; ldrb w16, [x16] ; cbnz w16, =>exit);
    }

    pub(super) fn emit_not_nullish(&mut self, node: NodeId, mask: u8) {
        let input = Self::gp(self.allocation.node(node).inputs[0]);
        let exit = self.eager_exit(node, DeoptReason::WrongType);
        debug_assert!(
            mask != 0 && u64::from(mask) & !(field_type::UNDEFINED | field_type::NULL) == 0
        );
        if u64::from(mask) == field_type::UNDEFINED | field_type::NULL {
            dynasm!(self.ops ; .arch aarch64
                ; and x16, X(input), !tag::UNDEFINED_TAG
                ; cmp x16, VALUE_NULL as u32
                ; b.eq =>exit
            );
        } else {
            let value = if u64::from(mask) == field_type::UNDEFINED {
                VALUE_UNDEFINED
            } else {
                VALUE_NULL
            };
            dynasm!(self.ops ; .arch aarch64 ; cmp XSP(input), value as u32 ; b.eq =>exit);
        }
    }
}
