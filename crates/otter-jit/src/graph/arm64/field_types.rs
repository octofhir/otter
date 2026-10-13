//! AArch64 guards for defining-field bounds and their nullish alternatives.
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
//! - Only reserved scratch registers are modified.
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
                16,
                view.cage_base as u64
                    + u64::from(guard.owner_shape)
                    + u64::from(view.shape_field_type_byte),
                RelocationTarget::FieldTypeWord {
                    owner_shape: guard.owner_shape,
                },
            );
            // Field summaries widen only on the mutator, which also runs this
            // code: a plain load sees every widening before it.
            dynasm!(self.ops ; .arch aarch64
                ; ldrb w16, [x16]
                ; cmp w16, u32::from(guard.expected as u8)
                ; b.ne =>exit
            );
        }
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
