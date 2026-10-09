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
//! - Every frame word uses the root Codegen's canonical home geometry.
//!
//! # See also
//! - `crate::x86_64::fields` owns physical field and prototype geometry.
//! - [`super::Codegen`] owns canonical homes and collecting boundaries.

use dynasmrt::{DynasmApi, DynasmLabelApi, dynasm};
use otter_vm::{jit::JitGuardWidth, native_abi as abi, value::tag};

use super::Codegen;
use crate::{
    Unsupported,
    graph::ir::{Kind, NodeId},
};

impl Codegen<'_> {
    pub(super) fn emit_memory(&mut self, node: NodeId) -> Result<bool, Unsupported> {
        if self.emit_elements(node)?
            || self.emit_properties(node)?
            || self.emit_keyed_store(node)?
            || self.emit_keyed_load(node)?
        {
            return Ok(true);
        }
        let allocation = self.allocation.node(node).clone();
        let input = |index| allocation.inputs[index];
        let output = || Self::gp(allocation.result.expect("memory result"));
        match self.graph.node(node).kind.clone() {
            Kind::LoadOwnField(field) => crate::x86_64::fields::emit_own_field(
                &mut self.ops,
                self.view.field_layout,
                Self::gp(input(0)),
                output(),
                field,
                false,
            ),
            Kind::StoreOwnField(field) => crate::x86_64::fields::emit_own_field(
                &mut self.ops,
                self.view.field_layout,
                Self::gp(input(0)),
                Self::gp(input(1)),
                field,
                true,
            ),
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
                self.emit_ordinary_receiver(receiver, 0, other);
                self.emit_body_load(
                    receiver,
                    destination,
                    self.view.object_shape_byte,
                    JitGuardWidth::Word32,
                );
                dynasm!(self.ops ; .arch x64 ; jmp =>done ; =>other ; xor Rd(destination), Rd(destination) ; =>done);
            }
            Kind::WriteBarrier => {
                self.emit_write_barrier(node, Self::gp(input(0)), Self::gp(input(1)))
            }
            Kind::ElementWriteBarrier => self.emit_element_write_barrier(
                node,
                Self::gp(input(0)),
                Self::gp(input(1)),
                Self::gp(input(2)),
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
