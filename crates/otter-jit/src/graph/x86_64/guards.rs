//! x86-64 Graph guards over current object, indexed and callable layouts.
//!
//! # Contents
//! - Shape, body, bounds, hole and detach-protector checks.
//! - Native/function identity and Function.prototype.call proofs.
//! - Shared ordinary-receiver and exact body-word checks.
//!
//! # Invariants
//! - Every failed speculative proof leaves through the node's eager recipe.
//! - Cell and null checks precede body reads; no guard has heap effects.
//! - Inputs survive checks; only r10/r11 and declared temporaries are clobbered.
//! - Exact eligible shape identity fixes lookup state. Dynamic shared probes
//!   read the sole current ShapeState byte and permit provisional feedback.
//!
//! # See also
//! - `otter_vm::jit` owns layouts and typed guard declarations.
//! - `otter_vm::object::ShapeState` owns immutable lookup/allocation facts.
//! - [`super::properties`] resolves fields after these shape proofs.

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, dynasm};
use otter_vm::{
    jit::{JitBodyGuard, JitGuardWidth},
    native_abi as abi,
    object::ShapeState,
    value::tag,
};

use super::Codegen;
use crate::x86_64::values::emit_load_symbol_u64;
use crate::{
    Unsupported,
    artifact::relocation::RelocationTarget,
    graph::ir::{DeoptReason, Kind, NodeId},
};

impl Codegen<'_> {
    pub(super) fn emit_guard(&mut self, node: NodeId) -> Result<bool, Unsupported> {
        let assigned = self.allocation.node(node).clone();
        let input = |index| Self::gp(assigned.inputs[index]);
        match self.graph.node(node).kind.clone() {
            Kind::CheckNumber => {
                let exit = self.eager_exit(node, DeoptReason::WrongType);
                self.load_immediate(10, tag::NUMBER_TAG);
                dynasm!(self.ops ; .arch x64 ; test Rq(input(0)), r10 ; jz =>exit);
            }
            // A writable site's shapes come from store handlers, which no
            // prototype shape receives; a shape's state never changes, and
            // this code keeps its shapes alive, so an exact match proves the
            // receiver's role too.
            Kind::CheckShapes {
                shapes,
                object_proved,
                ..
            } => {
                let exit = self.eager_exit(node, DeoptReason::WrongShape);
                if !object_proved {
                    self.emit_object_receiver(input(0), exit);
                }
                let matched = self.ops.new_dynamic_label();
                self.emit_body_load(
                    input(0),
                    10,
                    self.view.object_shape_byte,
                    JitGuardWidth::Word32,
                );
                for shape in shapes {
                    dynasm!(self.ops ; .arch x64 ; cmp r10d, shape as i32 ; je =>matched);
                }
                dynasm!(self.ops ; .arch x64 ; jmp =>exit ; =>matched);
            }
            Kind::CheckBounds => {
                let exit = self.eager_exit(node, DeoptReason::OutOfBounds);
                dynasm!(self.ops ; .arch x64 ; movsxd r10, Rd(input(0)) ; cmp r10, Rq(input(1)) ; jae =>exit);
            }
            Kind::CheckElements {
                type_tag,
                guards,
                holes,
                cached_base,
            } => {
                let receiver = input(0);
                let exit = self.eager_exit(node, DeoptReason::WrongShape);
                self.emit_cell_guard(receiver, exit);
                dynasm!(self.ops ; .arch x64 ; cmp BYTE [Rq(receiver)], type_tag as i8 ; jne =>exit);
                for guard in guards.into_iter().flatten() {
                    self.emit_body_guard(receiver, guard, exit);
                }
                if let Some(byte) = cached_base {
                    self.emit_body_load(receiver, 10, byte, JitGuardWidth::Word64);
                    dynasm!(self.ops ; .arch x64
                        ; test r10, r10 ; jz =>exit
                        ; mov r10, [r15 + crate::entry::THREAD_OFFSET as i32]
                        ; mov r10, [r10 + crate::entry::VM_THREAD_ARRAY_BUFFER_DETACH_PROTECTOR_CELL_OFFSET as i32]
                        ; test r10, r10 ; jz =>exit
                        ; cmp BYTE [r10], 0 ; jne =>exit);
                }
                if let Some(holes) = holes {
                    let matched = self.ops.new_dynamic_label();
                    self.emit_body_load(receiver, 10, holes.kind_byte, JitGuardWidth::Byte);
                    dynasm!(self.ops ; .arch x64
                        ; cmp r10d, holes.packed_kind as i32 ; je =>matched
                        ; cmp r10d, holes.holey_kind as i32 ; jne =>exit ; =>matched);
                }
            }
            Kind::CheckElementPresent => {
                let exit = self.eager_exit(node, DeoptReason::OutOfBounds);
                self.load_immediate(11, tag::VALUE_HOLE);
                dynasm!(self.ops ; .arch x64 ; mov r10d, Rd(input(1)) ; cmp [Rq(input(0)) + r10 * 8], r11 ; je =>exit);
            }
            Kind::CheckHoleyElementPresent(holes) => {
                let exit = self.eager_exit(node, DeoptReason::OutOfBounds);
                self.emit_hole_bit(holes, input(0), input(1));
                dynasm!(self.ops ; .arch x64 ; jc =>exit);
            }
            Kind::CheckNative(identity) => {
                let exit = self.eager_exit(node, DeoptReason::WrongValue);
                self.emit_native_identity(input(0), identity, exit);
            }
            Kind::CheckArgumentsElided => {
                let exit = self.eager_exit(node, DeoptReason::WrongValue);
                dynasm!(self.ops ; .arch x64 ; cmp DWORD [r14 + abi::NATIVE_FRAME_ARGUMENTS_OBJECT_OFFSET as i32], 0 ; jne =>exit);
            }
            Kind::CheckFunction { function_id, cell } => {
                let exit = self.eager_exit(node, DeoptReason::WrongValue);
                self.emit_function_identity(
                    function_id,
                    cell,
                    self.graph.node(node).pc,
                    input(0),
                    exit,
                );
            }
            Kind::CheckCellValue(cell) => {
                let exit = self.eager_exit(node, DeoptReason::WrongValue);
                let target = RelocationTarget::CalleeIdentityCell {
                    function_id: self.view_of(node).code_block.id,
                    call_pc: self.graph.node(node).pc,
                };
                emit_load_symbol_u64(&mut self.ops, &mut self.relocations, 11, cell, target);
                dynasm!(self.ops ; .arch x64 ; cmp Rq(input(0)), [r11] ; jne =>exit);
            }
            Kind::CheckNotHole => {
                let exit = self.eager_exit(node, DeoptReason::WrongValue);
                self.load_immediate(10, tag::VALUE_HOLE);
                dynasm!(self.ops ; .arch x64 ; cmp Rq(input(0)), r10 ; je =>exit);
            }
            Kind::CheckFunctionPrototypeCall(byte_pc) => {
                self.emit_check_function_prototype_call(
                    node,
                    byte_pc,
                    input(0),
                    assigned.gp_temps[0],
                )?;
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    pub(super) fn emit_cell_guard(&mut self, value: u8, miss: DynamicLabel) {
        self.load_immediate(10, tag::NOT_CELL_MASK);
        dynasm!(self.ops ; .arch x64 ; test Rq(value), r10 ; jnz =>miss ; test Rq(value), Rq(value) ; jz =>miss);
    }

    /// Published ordinary cell. Static users must prove an exact eligible
    /// baked shape before reading a field; this prefix claims no state facts.
    pub(super) fn emit_object_receiver(&mut self, receiver: u8, miss: DynamicLabel) {
        self.emit_cell_guard(receiver, miss);
        dynasm!(self.ops ; .arch x64
            ; cmp BYTE [Rq(receiver)], crate::entry::OBJECT_BODY_TYPE_TAG as i8 ; jne =>miss);
    }

    /// Current immutable ShapeState in r10d; only r10/r11 and flags change.
    /// A published object has a non-null shape and remains unchanged.
    pub(super) fn emit_shape_state(&mut self, receiver: u8) {
        let view = self.view;
        self.load_immediate(11, 0xffff_ffff_0000_0000);
        dynasm!(self.ops ; .arch x64
            ; and r11, Rq(receiver)
            ; mov r10d, [Rq(receiver) + view.object_shape_byte as i32]
            ; add r10, r11
            ; movzx r10d, BYTE [r10 + view.shape_state_byte as i32]);
    }

    /// Dynamic/shared lookup may train on provisional ordinary shapes. The
    /// live table identity, rather than a baked lineage, selects its entry.
    pub(super) fn emit_ordinary_receiver(
        &mut self,
        receiver: u8,
        extra_state: u8,
        miss: DynamicLabel,
    ) {
        self.emit_object_receiver(receiver, miss);
        self.emit_shape_state(receiver);
        let mask = ShapeState::DICTIONARY_MASK | ShapeState::OPAQUE_LOOKUP_MASK | extra_state;
        dynasm!(self.ops ; .arch x64 ; test r10b, mask as i8 ; jnz =>miss);
    }

    pub(super) fn emit_body_guard(
        &mut self,
        receiver: u8,
        guard: JitBodyGuard,
        miss: DynamicLabel,
    ) {
        self.emit_body_load(receiver, 10, guard.byte, guard.width);
        self.load_immediate(11, u64::from(guard.expect));
        dynasm!(self.ops ; .arch x64 ; cmp r10, r11 ; jne =>miss);
    }

    pub(super) fn emit_native_identity(&mut self, value: u8, identity: u32, miss: DynamicLabel) {
        self.emit_cell_guard(value, miss);
        dynasm!(self.ops ; .arch x64
            ; cmp BYTE [Rq(value)], self.view.collection_layout.native_function_type_tag as i8 ; jne =>miss
            ; cmp DWORD [Rq(value) + self.view.native_call_layout.identity_byte as i32], identity as i32 ; jne =>miss);
    }

    pub(super) fn emit_function_identity(
        &mut self,
        function_id: u32,
        cell: u64,
        pc: u32,
        value: u8,
        miss: DynamicLabel,
    ) {
        let proved = self.ops.new_dynamic_label();
        let verified = self.ops.new_dynamic_label();
        let target = RelocationTarget::CalleeIdentityCell {
            function_id,
            call_pc: pc,
        };
        if cell != 0 {
            emit_load_symbol_u64(
                &mut self.ops,
                &mut self.relocations,
                11,
                cell,
                target.clone(),
            );
            dynasm!(self.ops ; .arch x64 ; cmp Rq(value), [r11] ; je =>proved);
        }
        self.load_immediate(10, tag::box_function_id(function_id));
        dynasm!(self.ops ; .arch x64 ; cmp Rq(value), r10 ; je =>verified);
        self.emit_cell_guard(value, miss);
        let layout = self.view.closure_call_layout;
        dynasm!(self.ops ; .arch x64
            ; cmp BYTE [Rq(value)], otter_vm::closure::JS_CLOSURE_BODY_TYPE_TAG as i8 ; jne =>miss
            ; test DWORD [Rq(value) + layout.flags_byte as i32], layout.runtime_setup_flags as i32 ; jnz =>miss
            ; cmp DWORD [Rq(value) + layout.function_id_byte as i32], function_id as i32 ; jne =>miss);
        dynasm!(self.ops ; .arch x64 ; =>verified);
        if cell != 0 {
            emit_load_symbol_u64(&mut self.ops, &mut self.relocations, 11, cell, target);
            dynasm!(self.ops ; .arch x64 ; mov [r11], Rq(value));
        }
        dynasm!(self.ops ; .arch x64 ; =>proved);
    }

    fn emit_check_function_prototype_call(
        &mut self,
        node: NodeId,
        byte_pc: u32,
        value: u8,
        holder: u8,
    ) -> Result<(), Unsupported> {
        let proof = self
            .view_of(node)
            .function_prototype_calls
            .get(&byte_pc)
            .ok_or(Unsupported::OperandShape("x86 graph f.call site"))?
            .proof;
        let miss = self.eager_exit(node, DeoptReason::WrongValue);
        if let Some(lookup) = proof.lookup {
            self.emit_intrinsic_prototype(
                lookup.receiver,
                byte_pc,
                value,
                holder,
                miss,
                abi::STUB_JIT_RESOLVE_METHOD.id,
            );
            let view = self.view;
            dynasm!(self.ops ; .arch x64
                ; cmp DWORD [Rq(holder) + view.object_shape_byte as i32], lookup.holder_shape as i32 ; jne =>miss);
            crate::x86_64::fields::emit_own_field(
                &mut self.ops,
                view.field_layout,
                holder,
                holder,
                lookup.call_field,
                false,
            );
            self.emit_native_identity(holder, proof.call_native_ref, miss);
        } else {
            self.emit_native_identity(value, proof.call_native_ref, miss);
        }
        Ok(())
    }
}
