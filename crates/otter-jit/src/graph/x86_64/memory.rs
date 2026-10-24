//! x86-64 Graph field, context and indexed-memory dispatch.
//!
//! # Contents
//! - Fused own fields and fixed tagged/context words.
//! - Dispatch into indexed storage, property programs and edge barriers.
//! - Unsigned VM body-word reads shared with guards.
//!
//! # Invariants
//! - Only results, declared temporaries and reserved r10/r11 are overwritten.
//! - Own-field banks resolve inside their access; no suffix address escapes.
//! - Own stores select their receiver's defining field owner after the write;
//!   membership misses use a noncollecting leaf preserving live registers.
//! - Every frame word uses the root Codegen's canonical home geometry.
//! - Array body proofs skip receiver guards, never the full-width length guard.
//!
//! # See also
//! - `crate::x86_64::fields` owns physical field and prototype geometry.
//! - [`super::Codegen`] owns canonical homes and collecting boundaries.

use dynasmrt::{DynasmApi, DynasmLabelApi, dynasm};
use otter_vm::{jit::JitGuardWidth, native_abi as abi, value::tag};

use super::Codegen;
use crate::{
    Unsupported,
    graph::call::CommittedArgument,
    graph::ir::{Kind, NodeId},
};

impl Codegen<'_> {
    pub(super) fn emit_memory(&mut self, node: NodeId) -> Result<bool, Unsupported> {
        if self.emit_elements(node)?
            || self.emit_properties(node)?
            || self.emit_keyed_store(node)?
            || self.emit_array_method(node)?
            || self.emit_keyed_load(node)?
        {
            return Ok(true);
        }
        let allocation = self.allocation.node(node).clone();
        let input = |index| allocation.inputs[index];
        let output = || Self::gp(allocation.result.expect("memory result"));
        match self.graph.node(node).kind.clone() {
            Kind::BuiltinMethod(form) => {
                let mut arguments: smallvec::SmallVec<[CommittedArgument; 4]> = (0..=form.arity())
                    .map(|index| CommittedArgument::Value(Self::gp(input(index))))
                    .collect();
                let stub = form.stub();
                if stub.id == abi::STUB_JIT_STRING_METHOD.id {
                    // The entry reads two arguments and the kind.
                    while arguments.len() < 3 {
                        arguments.push(CommittedArgument::Scalar(
                            otter_vm::value::tag::VALUE_UNDEFINED,
                        ));
                    }
                    arguments.push(CommittedArgument::Scalar(u64::from(
                        form.kind().feedback_payload(),
                    )));
                }
                self.emit_committed_call(node, stub, &arguments, Some(output()))?;
            }
            Kind::LoadOwnField { field, .. } => {
                crate::x86_64::fields::emit_own_field(
                    &mut self.ops,
                    self.view.field_layout,
                    Self::gp(input(0)),
                    output(),
                    field,
                    if self.graph.node(node).repr == crate::graph::ir::Repr::Int32 {
                        crate::field_access::FieldAccess::LoadInt32
                    } else {
                        crate::field_access::FieldAccess::LoadTagged
                    },
                );
            }
            Kind::LoadNumberOwnField {
                field, double_only, ..
            } => {
                crate::x86_64::fields::emit_own_field(
                    &mut self.ops,
                    self.view.field_layout,
                    Self::gp(input(0)),
                    10,
                    field,
                    crate::field_access::FieldAccess::LoadTagged,
                );
                crate::x86_64::field_word::emit_decode_number(
                    &mut self.ops,
                    10,
                    Self::fp(allocation.result.unwrap()),
                    11,
                    double_only,
                );
            }
            Kind::StoreOwnField { field, owners, .. }
            | Kind::StoreDoubleOwnField { field, owners, .. } => {
                let object = Self::gp(input(0));
                let value = match self.graph.node(node).kind {
                    Kind::StoreDoubleOwnField { .. } => {
                        let physical = allocation.gp_temps[usize::from(!owners.is_empty())];
                        crate::x86_64::field_word::emit_encode_double(
                            &mut self.ops,
                            Self::fp(input(1)),
                            physical,
                            11,
                        );
                        physical
                    }
                    Kind::StoreOwnField { .. } => Self::gp(input(1)),
                    _ => unreachable!(),
                };
                crate::x86_64::fields::emit_own_field(
                    &mut self.ops,
                    self.view.field_layout,
                    object,
                    value,
                    field,
                    crate::field_access::FieldAccess::Store,
                );
                if !owners.is_empty() {
                    let owner = allocation.gp_temps[0];
                    let first = &owners[0].1;
                    if first.observes() && owners.iter().all(|(_, arm)| arm == first) {
                        self.load_immediate(owner, u64::from(first.owner_shape));
                        crate::x86_64::field_type::emit_observe_known(
                            &mut self.ops,
                            &mut self.relocations,
                            self.view,
                            owner,
                            value,
                            first,
                        );
                    } else {
                        let done = self.ops.new_dynamic_label();
                        for (receiver_shape, authority) in
                            owners.iter().filter(|(_, arm)| arm.observes())
                        {
                            let next = self.ops.new_dynamic_label();
                            dynasm!(self.ops ; .arch x64
                                ; cmp DWORD [Rq(object) + self.view.object_shape_byte as i32], *receiver_shape as i32
                                ; jne =>next);
                            self.load_immediate(owner, u64::from(authority.owner_shape));
                            crate::x86_64::field_type::emit_observe_known(
                                &mut self.ops,
                                &mut self.relocations,
                                self.view,
                                owner,
                                value,
                                authority,
                            );
                            dynasm!(self.ops ; .arch x64 ; jmp =>done ; =>next);
                        }
                        dynasm!(self.ops ; .arch x64 ; =>done);
                    }
                }
            }
            Kind::LoadArgumentCount => {
                let destination = output();
                dynasm!(self.ops ; .arch x64
                    ; mov Rd(destination), [r14 + abi::NATIVE_FRAME_ARGUMENT_COUNT_OFFSET as i32]);
            }
            Kind::LoadActualArgument => {
                let (index, destination) = (Self::gp(input(0)), output());
                let exit = self.eager_exit(node, crate::graph::ir::DeoptReason::OutOfBounds);
                // The actual window answers while no arguments object exists;
                // the index compares unsigned, so a negative one is out of
                // range too.
                dynasm!(self.ops ; .arch x64
                    ; cmp DWORD [r14 + abi::NATIVE_FRAME_ARGUMENTS_OBJECT_OFFSET as i32], 0
                    ; jne =>exit
                    ; cmp Rd(index), [r14 + abi::NATIVE_FRAME_ARGUMENT_COUNT_OFFSET as i32]
                    ; jae =>exit
                    ; mov r10, [r14 + abi::NATIVE_FRAME_ACTUALS_OFFSET as i32]
                    ; mov r11d, Rd(index)
                    ; mov Rq(destination), [r10 + r11 * 8]);
            }
            Kind::SelectArgument { count } => {
                let (index, destination) = (Self::gp(input(0)), output());
                let exit = self.eager_exit(node, crate::graph::ir::DeoptReason::OutOfBounds);
                dynasm!(self.ops ; .arch x64
                    ; cmp Rd(index), i32::from(count) ; jae =>exit
                    ; mov r10, Rq(Self::gp(input(1))));
                for argument in 1..count {
                    let value = Self::gp(input(1 + usize::from(argument)));
                    dynasm!(self.ops ; .arch x64
                        ; cmp Rd(index), i32::from(argument) ; cmove r10, Rq(value));
                }
                dynasm!(self.ops ; .arch x64 ; mov Rq(destination), r10);
            }
            Kind::LoadTaggedField(offset) => {
                let (base, destination) = (Self::gp(input(0)), output());
                dynasm!(self.ops ; .arch x64 ; mov Rq(destination), [Rq(base) + offset]);
            }
            Kind::StoreTaggedField(offset) => {
                let (base, value) = (Self::gp(input(0)), Self::gp(input(1)));
                dynasm!(self.ops ; .arch x64 ; mov [Rq(base) + offset], Rq(value));
            }
            Kind::LoadContextParent => self.emit_body_load(
                Self::gp(input(0)),
                output(),
                self.view.context_layout.parent_byte,
                JitGuardWidth::Word64,
            ),
            Kind::LoadClosureContext => {
                let (closure, destination) = (Self::gp(input(0)), output());
                let bare = self.ops.new_dynamic_label();
                let done = self.ops.new_dynamic_label();
                dynasm!(self.ops ; .arch x64
                    ; mov r10d, Rd(closure) ; and r10d, 0xffff
                    ; cmp r10d, tag::FUNCTION_ID_TAG as i32 ; je =>bare);
                self.emit_body_load(
                    closure,
                    destination,
                    self.view.closure_call_layout.context_byte,
                    JitGuardWidth::Word64,
                );
                dynasm!(self.ops ; .arch x64 ; jmp =>done ; =>bare);
                self.load_immediate(destination, tag::VALUE_UNDEFINED);
                dynasm!(self.ops ; .arch x64 ; =>done);
            }
            Kind::LoadReceiverShape => {
                let (receiver, destination) = (Self::gp(input(0)), output());
                let other = self.ops.new_dynamic_label();
                let done = self.ops.new_dynamic_label();
                // Every compared shape is baked, so neither dictionary nor
                // opaque, and a shape's state never changes: the map word
                // alone selects the arm, and no other cell's word matches.
                self.emit_cell_guard(receiver, other);
                self.emit_body_load(
                    receiver,
                    destination,
                    otter_vm::jit::JIT_MAP_WORD_BYTE,
                    JitGuardWidth::Word32,
                );
                dynasm!(self.ops ; .arch x64 ; jmp =>done ; =>other ; xor Rd(destination), Rd(destination) ; =>done);
            }
            Kind::WriteBarrier => {
                self.emit_write_barrier(node, Self::gp(input(0)), Self::gp(input(1)))
            }
            Kind::NoteContextWrite(slot) => {
                self.emit_note_context_write(node, Self::gp(input(0)), slot);
            }
            Kind::LoadArrayLength { array_proved } => {
                let (receiver, destination) = (Self::gp(input(0)), output());
                let exit = self.eager_exit(node, crate::graph::ir::DeoptReason::WrongShape);
                let storage = otter_vm::jit::JitArrayStorage::current();
                if !array_proved {
                    self.emit_cell_guard(receiver, exit);
                    dynasm!(self.ops ; .arch x64
                        ; cmp BYTE [Rq(receiver)], storage.type_tag as i8 ; jne =>exit);
                }
                dynasm!(self.ops ; .arch x64
                    ; mov r10d, [Rq(receiver) + storage.logical_length_byte as i32]
                    ; test r10d, r10d ; js =>exit
                    ; mov Rd(destination), r10d);
            }
            Kind::LoadBackedgeFuel => {
                let destination = output();
                crate::x86_64::values::emit_load_backedge_fuel_cell(
                    &mut self.ops,
                    &mut self.relocations,
                    10,
                    self.view.backedge_fuel_cell,
                );
                dynasm!(self.ops ; .arch x64 ; mov Rq(destination), [r10]);
            }
            Kind::DecrementBackedgeFuel => {
                let (fuel, destination) = (Self::gp(input(0)), output());
                dynasm!(self.ops ; .arch x64 ; lea Rq(destination), [Rq(fuel) - 1]);
            }
            Kind::StoreBackedgeFuel => {
                let fuel = Self::gp(input(0));
                crate::x86_64::values::emit_load_backedge_fuel_cell(
                    &mut self.ops,
                    &mut self.relocations,
                    10,
                    self.view.backedge_fuel_cell,
                );
                dynasm!(self.ops ; .arch x64 ; mov [r10], Rq(fuel));
            }
            Kind::LoadTypedArrayLength => {
                let (receiver, destination) = (Self::gp(input(0)), output());
                let exit = self.eager_exit(node, crate::graph::ir::DeoptReason::WrongShape);
                let typed = self.view_of(node).typed_array_length;
                self.emit_cell_guard(receiver, exit);
                dynasm!(self.ops ; .arch x64
                    ; cmp BYTE [Rq(receiver)], typed.type_tag as i8 ; jne =>exit
                    ; cmp DWORD [Rq(receiver) + typed.expando_byte as i32], 0 ; jne =>exit
                    ; cmp QWORD [Rq(receiver) + typed.data_byte as i32], 0 ; je =>exit
                    ; mov r10, [r15 + crate::entry::THREAD_OFFSET as i32]
                    ; mov r10, [r10 + crate::entry::VM_THREAD_ARRAY_BUFFER_DETACH_PROTECTOR_CELL_OFFSET as i32]
                    ; test r10, r10 ; jz =>exit
                    ; cmp BYTE [r10], 0 ; jne =>exit
                    ; mov r10, QWORD [Rq(receiver) + typed.length_byte as i32]
                    ; cmp r10, i32::MAX ; ja =>exit
                    ; mov Rd(destination), r10d);
            }
            Kind::ElementWriteBarrier => self.emit_element_write_barrier(
                node,
                Self::gp(input(0)),
                Self::gp(input(1)),
                Self::gp(input(2)),
                &[],
            ),
            _ => return Ok(false),
        }
        Ok(true)
    }

    /// Unsigned body word; all u32 layout offsets retain their full width.
    pub(super) fn emit_body_load(
        &mut self,
        base: u8,
        destination: u8,
        byte: u32,
        width: JitGuardWidth,
    ) {
        if let Ok(byte) = i32::try_from(byte) {
            match width {
                JitGuardWidth::Byte => {
                    dynasm!(self.ops ; .arch x64 ; movzx Rd(destination), BYTE [Rq(base) + byte])
                }
                JitGuardWidth::Word32 => {
                    dynasm!(self.ops ; .arch x64 ; mov Rd(destination), [Rq(base) + byte])
                }
                JitGuardWidth::Word64 => {
                    dynasm!(self.ops ; .arch x64 ; mov Rq(destination), [Rq(base) + byte])
                }
            }
        } else {
            self.load_immediate(11, u64::from(byte));
            match width {
                JitGuardWidth::Byte => {
                    dynasm!(self.ops ; .arch x64 ; movzx Rd(destination), BYTE [Rq(base) + r11])
                }
                JitGuardWidth::Word32 => {
                    dynasm!(self.ops ; .arch x64 ; mov Rd(destination), [Rq(base) + r11])
                }
                JitGuardWidth::Word64 => {
                    dynasm!(self.ops ; .arch x64 ; mov Rq(destination), [Rq(base) + r11])
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "memory_tests.rs"]
mod tests;
