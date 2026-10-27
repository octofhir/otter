//! Source-owned intrinsic calls expressed as ordinary SSA operations.
//!
//! # Contents
//! - `Builder::emit_intrinsic_call` checks a held callable identity and lowers
//!   admitted fixed calls and constructions to the existing value/allocation IR.
//! - `tests` verifies evaluated operands, original source exits and nested splices.
//!
//! # Invariants
//! - Admission uses the shared intrinsic form policy and the current source realm.
//! - The callee is the evaluated SSA value; its guard precedes every effect and
//!   retains the eager call state, including ignored evaluated arguments.
//! - Allocation nodes keep their source's canonical root/reconstruction contract.
//! - Admitted allocation forms prove their exact physical body kind.
//! - `Reflect.apply(target, this, list)` becomes [`Kind::CallApplyArray`],
//!   which calls `target` with the list's elements.
//! - A refused proof or form leaves the existing native-kind call path intact.
//!
//! # See also
//! - `crate::intrinsic_calls` owns form admission for both generated tiers.
//! - `super::Builder::visit_call` owns the ordinary call and construct paths.
//! - `crate::graph::ir::Kind` owns allocation and conversion effects.

use otter_vm::{JitNativeCall, native_abi::ExitReason};
use smallvec::SmallVec;

use crate::graph::ir::NodeId;

use super::{Builder, Instruction, Kind, Repr};
use crate::intrinsic_calls::{IntrinsicCallForm, proof_form};

impl Builder<'_> {
    /// Lower an admitted intrinsic without building a call activation or window.
    /// `Reflect.apply(target, this, list)` with the realm's original
    /// `Reflect.apply`: the call of `target` with the list's elements, which
    /// [`Kind::CallApplyArray`] passes from a plain dense Array's storage.
    fn emit_reflect_apply(
        &mut self,
        instruction: &Instruction,
        proof: otter_vm::JitIntrinsicCallProof,
        callee: u16,
        arguments: &[u16],
        construct: bool,
    ) -> bool {
        if construct {
            return false;
        }
        let callee = self.read(callee);
        let callee = self.tagged(callee);
        self.emit_reflect_apply_value(instruction, proof, callee, arguments)
    }

    /// [`Self::emit_reflect_apply`] of an already read callee: a plain call's
    /// register or the method a `Reflect.apply(...)` call loaded.
    pub(super) fn emit_reflect_apply_value(
        &mut self,
        instruction: &Instruction,
        proof: otter_vm::JitIntrinsicCallProof,
        callee: NodeId,
        arguments: &[u16],
    ) -> bool {
        let &[target, this, list] = arguments else {
            return false;
        };
        if proof.identity_cell == 0 || proof.realm_id != self.view.literal_allocations.realm_id {
            return false;
        }
        self.add(
            Kind::CheckCellValue(proof.identity_cell),
            &[callee],
            Repr::None,
        );
        let inputs: SmallVec<[NodeId; 3]> = [target, this, list]
            .into_iter()
            .map(|register| {
                let value = self.read(register);
                self.tagged(value)
            })
            .collect();
        // A spliced body's untouched rest list: the call of its values.
        if let Some(values) = self.pristine_list_values(inputs[2]) {
            self.emit_call_with_receiver(instruction, inputs[0], inputs[1], &values, None);
            return true;
        }
        let node = self.add(Kind::CallApplyArray { pc: self.pc }, &inputs, Repr::Tagged);
        let destination = instruction.writes[0];
        let lazy = self.lazy_state(Some((destination, node)));
        self.graph.node_mut(node).lazy = Some(lazy);
        self.write(destination, node);
        true
    }

    pub(super) fn emit_intrinsic_call(
        &mut self,
        instruction: &Instruction,
        callee: u16,
        arguments: &[u16],
        construct: bool,
    ) -> bool {
        let Some(&JitNativeCall::Intrinsic(proof)) =
            self.view.native_calls.get(&instruction.byte_pc)
        else {
            return false;
        };
        if self.exited_for(ExitReason::IdentityGuard) {
            return false;
        }
        if proof.kind == otter_vm::IntrinsicCallKind::ReflectApply {
            return self.emit_reflect_apply(instruction, proof, callee, arguments, construct);
        }
        let Some(form) = proof_form(
            proof,
            self.view.literal_allocations.realm_id,
            construct,
            arguments.len(),
        ) else {
            return false;
        };
        let callee = self.read(callee);
        let callee = self.tagged(callee);
        self.add(
            Kind::CheckCellValue(proof.identity_cell),
            &[callee],
            Repr::None,
        );
        let result = match form {
            IntrinsicCallForm::ArrayEmpty | IntrinsicCallForm::ObjectEmpty => {
                let kind = if form == IntrinsicCallForm::ArrayEmpty {
                    Kind::NewArrayEmpty
                } else {
                    Kind::NewObject
                };
                let result = self.add(kind, &[], Repr::Tagged);
                if form == IntrinsicCallForm::ArrayEmpty {
                    self.prove_array_body(result);
                } else {
                    self.prove_ordinary_object(result);
                }
                result
            }
            IntrinsicCallForm::ArrayLength => {
                let value = self.read(arguments[0]);
                let value = self.tagged(value);
                let result = self.add(Kind::NewArrayWithLength, &[value], Repr::Tagged);
                self.prove_array_body(result);
                result
            }
            IntrinsicCallForm::Boolean => {
                if let Some(&register) = arguments.first() {
                    let value = self.read(register);
                    let value = self.tagged(value);
                    if self.graph.node(value).kind.produces_boolean() {
                        value
                    } else {
                        self.add(Kind::ToBoolean, &[value], Repr::Tagged)
                    }
                } else {
                    self.constant_tagged(otter_vm::Value::boolean(false).to_bits())
                }
            }
            IntrinsicCallForm::NumberZero => self.constant_int32(0),
        };
        self.write(instruction.writes[0], result);
        true
    }
}

#[cfg(test)]
#[path = "builder_intrinsic_call_tests.rs"]
mod tests;
