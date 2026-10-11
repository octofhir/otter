//! x86-64 guards for defining-field bounds and their nullish alternatives.
//!
//! # Contents
//! - `emit_field_type_guards` compares rooted defining summary bytes.
//! - `emit_not_nullish` rejects the alternatives before a shaped dereference.
//!
//! # Invariants
//! - Bounds are guarded before the source load; target stability is guarded
//!   separately before a result's shape proof is used.
//! - Every widening changes the low byte's shape count or nullish bits, or
//!   saturates to Top. Equality proves the full captured word is unchanged.
//! - Guards allocate nothing and leave by eager reconstruction at their exact
//!   source instruction, including the active inline-frame chain.
//! - Only reserved scratch registers and flags are modified.
//!
//! # See also
//! - [`otter_vm::jit::JitFieldTypeGuard`] owns the scalar compile contract.
//! - [`super::super::builder`] owns effect-region proof reuse.

use super::*;
use otter_vm::jit::JitFieldTypeGuard;
use otter_vm::object::field_type;

impl Codegen<'_> {
    pub(super) fn emit_field_type_guards(&mut self, node: NodeId, guards: &[JitFieldTypeGuard]) {
        let exit = self.eager_exit(node, DeoptReason::FieldTypeChanged);
        let view = self.view;
        for guard in guards {
            emit_load_symbol_u64(
                &mut self.ops,
                &mut self.relocations,
                10,
                view.cage_base as u64
                    + u64::from(guard.owner_shape)
                    + u64::from(view.shape_field_type_byte),
                RelocationTarget::FieldTypeWord {
                    owner_shape: guard.owner_shape,
                },
            );
            dynasm!(self.ops ; .arch x64 ; cmp BYTE [r10], guard.expected as u8 as i8 ; jne =>exit);
        }
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
