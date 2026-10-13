//! x86-64 dependency-flag tests and nullish rejections of field bounds.
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
//! - Only reserved scratch registers and flags are modified.
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
            10,
            self.view.dependency_flag.address() as u64,
            RelocationTarget::DependencyFlag,
        );
        dynasm!(self.ops ; .arch x64 ; cmp BYTE [r10], 0 ; jne =>exit);
    }

    pub(super) fn emit_not_nullish(&mut self, node: NodeId, mask: u8) {
        let input = Self::gp(self.loc(node).inputs[0]);
        let exit = self.eager_exit(node, DeoptReason::WrongType);
        debug_assert!(
            mask != 0 && u64::from(mask) & !(field_type::UNDEFINED | field_type::NULL) == 0
        );
        if u64::from(mask) == field_type::UNDEFINED | field_type::NULL {
            dynasm!(self.ops ; .arch x64
                ; mov r10, Rq(input)
                ; and r10, !(tag::UNDEFINED_TAG as i32)
                ; cmp r10, tag::VALUE_NULL as i32
                ; je =>exit
            );
        } else {
            let value = if u64::from(mask) == field_type::UNDEFINED {
                tag::VALUE_UNDEFINED
            } else {
                tag::VALUE_NULL
            };
            dynasm!(self.ops ; .arch x64 ; cmp Rq(input), value as i32 ; je =>exit);
        }
    }
}
