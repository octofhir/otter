//! Builtin methods at a guarded method call as SSA nodes:
//! `Array.prototype.push` and `pop`, and the methods of primitive strings,
//! Arrays and keyed collections the builtin registry declares.
//!
//! # Contents
//! - `Builder::emit_array_method` lowers a guarded `push` of one value or
//!   `pop` of an Array receiver to [`Kind::ArrayPush`] / [`Kind::ArrayPop`].
//! - `Builder::emit_registry_method` lowers a guarded registry method of a
//!   primitive string, an Array or a `Map` / `Set` to its reduction, or to
//!   the row's read-only [`Kind::NativeLeaf`].
//!
//! # Invariants
//! - Only a site whose guarded receiver is an Array body pinned to a
//!   fast-mode `%Array.prototype%` lowers; its proof is the node's eager
//!   deopt, and a site that left optimized code on that proof keeps its
//!   baseline operation.
//! - The node runs no JavaScript: a receiver state neither its inline paths
//!   nor the VM's push and pop entries complete leaves through the same
//!   eager deopt, so the call needs no frame, an inlined body containing it
//!   stays inlined, and it writes only element storage and lengths.
//! - A registry method is proved by [`Kind::CheckGuardedMethod`]: the
//!   receiver has the body type and guard word its site saw, and the pinned
//!   prototype still holds the builtin in the slot its layout fixes. Any
//!   other receiver, index or result, and a leaf's miss, leaves through an
//!   eager deopt, after which the site keeps its baseline operation.
//!
//! # See also
//! - `otter_vm::Interpreter::jit_array_method_call` for the site's proof.
//! - `super::Builder::visit_call_method` for the other method call paths.

use otter_vm::jit::{JitArrayStorage, JitGuardedReceiver, JitMethodHolder};
use otter_vm::native_abi::{ExitReason, STUB_ARRAY_POP_LEAF, STUB_ARRAY_PUSH_LEAF};
use smallvec::SmallVec;

use super::{Builder, Instruction, Kind, NodeId, Repr};

impl Builder<'_> {
    /// Lower the guarded `push` / `pop` call `instruction` on `receiver`,
    /// or refuse a site with another callee or receiver proof.
    pub(super) fn emit_array_method(
        &mut self,
        instruction: &Instruction,
        receiver: u16,
        arguments: &[u16],
    ) -> bool {
        let byte_pc = instruction.byte_pc;
        let view = self.view;
        if view.cage_base == 0
            || view.native_call_layout.identity_byte == 0
            || self.exited_for(ExitReason::ShapeGuard)
        {
            return false;
        }
        let Some(call) = view.guarded_method_calls.get(&byte_pc) else {
            return false;
        };
        let JitGuardedReceiver::Exotic(target) = call.receiver else {
            return false;
        };
        if target.type_tag != JitArrayStorage::current().type_tag
            || !target.is_generated_receiver()
            || target.active_realm.is_some()
            || !matches!(call.holder, JitMethodHolder::Shape(_))
        {
            return false;
        }
        let push = match (call.entry_stub_id, arguments) {
            (id, [_]) if id == STUB_ARRAY_PUSH_LEAF.id => true,
            (id, []) if id == STUB_ARRAY_POP_LEAF.id => false,
            _ => return false,
        };
        let receiver = self.read(receiver);
        let receiver = self.tagged(receiver);
        let result = if push {
            let value = self.read(arguments[0]);
            let value = self.tagged(value);
            self.add(Kind::ArrayPush(byte_pc), &[receiver, value], Repr::Tagged)
        } else {
            self.add(Kind::ArrayPop(byte_pc), &[receiver], Repr::Tagged)
        };
        self.write(instruction.writes[0], result);
        true
    }

    /// Lower the guarded registry method call `instruction` on a primitive
    /// string, Array or keyed collection `receiver`, or refuse any other
    /// site.
    pub(super) fn emit_registry_method(
        &mut self,
        instruction: &Instruction,
        receiver: u16,
        arguments: &[u16],
    ) -> bool {
        let byte_pc = instruction.byte_pc;
        let Some(call) = self.view.guarded_method_calls.get(&byte_pc) else {
            return false;
        };
        let JitGuardedReceiver::Exotic(target) = call.receiver else {
            return false;
        };
        let Some(declaration) = otter_vm::jit_static_native::jit_leaf_builtin(call.entry_stub_id)
        else {
            return false;
        };
        let receiver_kind = target.type_tag == otter_vm::string::JS_STRING_BODY_TYPE_TAG
            || target.type_tag == otter_vm::collections::MAP_BODY_TYPE_TAG
            || target.type_tag == otter_vm::collections::SET_BODY_TYPE_TAG
            || (target.type_tag == JitArrayStorage::current().type_tag
                && target.is_generated_receiver()
                && target.active_realm.is_none());
        if !receiver_kind
            || crate::graph::call::IntrinsicHolder::of(call.holder).is_none()
            || !declaration.this_operand
            || arguments.len() != usize::from(declaration.argument_count)
            || super::super::native_leaf::entry(call.entry_stub_id).is_err()
            || self.exited_for(ExitReason::IdentityGuard)
            || self.exited_for(ExitReason::TypeMismatch)
            || self.exited_for(ExitReason::BoundsGuard)
        {
            return false;
        }
        let receiver = self.read(receiver);
        let receiver = self.tagged(receiver);
        let arguments: SmallVec<[NodeId; 2]> = arguments
            .iter()
            .map(|&register| {
                let value = self.read(register);
                self.tagged(value)
            })
            .collect();
        self.add(Kind::CheckGuardedMethod(byte_pc), &[receiver], Repr::None);
        let result = match self.reduce_builtin(declaration, receiver, &arguments) {
            Some(result) => result,
            None => {
                let mut inputs: SmallVec<[NodeId; 2]> = SmallVec::new();
                inputs.push(receiver);
                inputs.extend(arguments.iter().copied());
                inputs.resize(2, self.undefined);
                self.add(Kind::NativeLeaf(call.entry_stub_id), &inputs, Repr::Tagged)
            }
        };
        self.write(instruction.writes[0], result);
        true
    }
}
