//! x86-64 Graph indexed storage over the VM's current access declarations.
//!
//! # Contents
//! - Width-correct boxed, integer and floating element loads and stores.
//! - Numeric hole bitmaps and the array-index protector's undefined result.
//! - SSE2 ToUint8Clamp with explicit ties-to-even rounding.
//!
//! # Invariants
//! - Bounds and storage guards precede access; index high words are discarded.
//! - No derived base survives a collecting or reentrant operation.
//! - Clamping does not depend on MXCSR rounding or an untracked constant pool.
//! - Only results, declared FP temporaries and r10/r11/xmm15 are clobbered.
//!
//! # See also
//! - `otter_vm::jit::JitElementAccess` owns storage geometry and hole declarations.
//! - [`super::guards`] owns receiver, bounds, present and detach checks.

use dynasmrt::{DynasmApi, DynasmLabelApi, dynasm};
use otter_vm::{
    jit::{JitElementRepr as E, JitHoleBitmap},
    value::tag,
};

use super::Codegen;
use crate::{
    Unsupported,
    graph::{
        ir::{DeoptReason, Kind, NodeId},
        regalloc::Location,
    },
};

impl Codegen<'_> {
    pub(super) fn emit_elements(&mut self, node: NodeId) -> Result<bool, Unsupported> {
        let assigned = self.allocation.node(node).clone();
        let input = |i| assigned.inputs[i];
        match self.graph.node(node).kind.clone() {
            Kind::LoadElementsLength { byte, width } => self.emit_body_load(
                Self::gp(input(0)),
                Self::gp(assigned.result.expect("element length")),
                byte,
                width,
            ),
            Kind::LoadElementsBase { byte, .. } => self.emit_body_load(
                Self::gp(input(0)),
                Self::gp(assigned.result.expect("element base")),
                byte,
                otter_vm::jit::JitGuardWidth::Word64,
            ),
            Kind::LoadElement(element) => self.emit_load_element(
                node,
                element,
                Self::gp(input(0)),
                Self::gp(input(1)),
                assigned.result.expect("element result"),
            ),
            Kind::LoadElementUint32ToFloat64 => {
                let (base, index, result) = (
                    Self::gp(input(0)),
                    Self::gp(input(1)),
                    Self::fp(assigned.result.expect("uint32 double")),
                );
                dynasm!(self.ops ; .arch x64
                    ; mov r10d, Rd(index) ; mov r11d, [Rq(base) + r10 * 4]
                    ; cvtsi2sd Rx(result), r11);
            }
            Kind::LoadHoleyFloat64Element(holes) => {
                let (base, index, result) = (
                    Self::gp(input(0)),
                    Self::gp(input(1)),
                    Self::gp(assigned.result.expect("holey result")),
                );
                let double = assigned.fp_temps[0];
                let hole = self.ops.new_dynamic_label();
                let done = self.ops.new_dynamic_label();
                self.emit_hole_bit(holes, base, index);
                dynasm!(self.ops ; .arch x64 ; jc =>hole ; mov r10d, Rd(index) ; movsd Rx(double), [Rq(base) + r10 * 8]);
                self.emit_box_float64(node, result, double);
                dynasm!(self.ops ; .arch x64 ; jmp =>done ; =>hole);
                self.emit_hole_undefined(node, result);
                dynasm!(self.ops ; .arch x64 ; =>done);
            }
            Kind::StoreElement(element) => {
                self.emit_store_element(element, Self::gp(input(0)), Self::gp(input(1)), input(2))?
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    /// Carry is the selected hole bit; BT avoids an implicit RCX shift clobber.
    pub(super) fn emit_hole_bit(&mut self, holes: JitHoleBitmap, base: u8, index: u8) {
        dynasm!(self.ops ; .arch x64
            ; mov r11d, [Rq(base) + holes.capacity_byte]
            ; lea r11, [Rq(base) + r11 * 8]
            ; mov r10d, Rd(index)
            ; bt QWORD [r11], r10);
    }

    fn emit_hole_undefined(&mut self, node: NodeId, destination: u8) {
        let exit = self.eager_exit(node, DeoptReason::OutOfBounds);
        dynasm!(self.ops ; .arch x64
            ; mov r10, [r15 + crate::entry::THREAD_OFFSET as i32]
            ; mov r10, [r10 + crate::entry::VM_THREAD_ARRAY_INDEX_PROTECTOR_CELL_OFFSET as i32]
            ; cmp BYTE [r10], 0 ; jne =>exit);
        self.load_immediate(destination, tag::VALUE_UNDEFINED);
    }

    fn emit_load_element(
        &mut self,
        node: NodeId,
        element: E,
        base: u8,
        index: u8,
        result: Location,
    ) {
        dynasm!(self.ops ; .arch x64 ; mov r10d, Rd(index));
        match element {
            E::Boxed => {
                let destination = Self::gp(result);
                let hole = self.ops.new_dynamic_label();
                let done = self.ops.new_dynamic_label();
                self.load_immediate(11, tag::VALUE_HOLE);
                dynasm!(self.ops ; .arch x64
                    ; cmp [Rq(base) + r10 * 8], r11 ; je =>hole
                    ; mov Rq(destination), [Rq(base) + r10 * 8] ; jmp =>done ; =>hole);
                self.emit_hole_undefined(node, destination);
                dynasm!(self.ops ; .arch x64 ; =>done);
            }
            E::Int8 => {
                dynasm!(self.ops ; .arch x64 ; movsx Rd(Self::gp(result)), BYTE [Rq(base) + r10])
            }
            E::Uint8 | E::Uint8Clamped => {
                dynasm!(self.ops ; .arch x64 ; movzx Rd(Self::gp(result)), BYTE [Rq(base) + r10])
            }
            E::Int16 => {
                dynasm!(self.ops ; .arch x64 ; movsx Rd(Self::gp(result)), WORD [Rq(base) + r10 * 2])
            }
            E::Uint16 => {
                dynasm!(self.ops ; .arch x64 ; movzx Rd(Self::gp(result)), WORD [Rq(base) + r10 * 2])
            }
            E::Int32 => {
                dynasm!(self.ops ; .arch x64 ; mov Rd(Self::gp(result)), [Rq(base) + r10 * 4])
            }
            E::Uint32 => {
                let exit = self.eager_exit(node, DeoptReason::LostPrecision);
                dynasm!(self.ops ; .arch x64
                    ; mov r11d, [Rq(base) + r10 * 4] ; test r11d, r11d ; js =>exit
                    ; mov Rd(Self::gp(result)), r11d);
            }
            E::Float32 => {
                dynasm!(self.ops ; .arch x64 ; cvtss2sd Rx(Self::fp(result)), DWORD [Rq(base) + r10 * 4])
            }
            E::Float64 => {
                dynasm!(self.ops ; .arch x64 ; movsd Rx(Self::fp(result)), [Rq(base) + r10 * 8])
            }
        }
    }

    fn emit_store_element(
        &mut self,
        element: E,
        base: u8,
        index: u8,
        value: Location,
    ) -> Result<(), Unsupported> {
        dynasm!(self.ops ; .arch x64 ; mov r10d, Rd(index));
        match (element, value) {
            (E::Boxed, Location::Gp(value)) => {
                dynasm!(self.ops ; .arch x64 ; mov [Rq(base) + r10 * 8], Rq(value))
            }
            (E::Int8 | E::Uint8, Location::Gp(value)) => {
                dynasm!(self.ops ; .arch x64 ; mov BYTE [Rq(base) + r10], Rb(value))
            }
            (E::Int16 | E::Uint16, Location::Gp(value)) => {
                dynasm!(self.ops ; .arch x64 ; mov WORD [Rq(base) + r10 * 2], Rw(value))
            }
            (E::Int32 | E::Uint32, Location::Gp(value)) => {
                dynasm!(self.ops ; .arch x64 ; mov [Rq(base) + r10 * 4], Rd(value))
            }
            (E::Float32, Location::Fp(value)) => {
                dynasm!(self.ops ; .arch x64 ; cvtsd2ss xmm15, Rx(value) ; movss [Rq(base) + r10 * 4], xmm15)
            }
            (E::Float64, Location::Fp(value)) => {
                dynasm!(self.ops ; .arch x64 ; movsd [Rq(base) + r10 * 8], Rx(value))
            }
            (E::Uint8Clamped, Location::Gp(value)) => {
                let zero = self.ops.new_dynamic_label();
                let ready = self.ops.new_dynamic_label();
                dynasm!(self.ops ; .arch x64
                    ; mov r11d, Rd(value) ; test r11d, r11d ; js =>zero
                    ; cmp r11d, 255 ; jle =>ready ; mov r11d, 255 ; jmp =>ready
                    ; =>zero ; xor r11d, r11d ; =>ready
                    ; mov BYTE [Rq(base) + r10], r11b);
            }
            (E::Uint8Clamped, Location::Fp(value)) => {
                self.emit_clamped_double_store(base, index, value)
            }
            _ => return Err(Unsupported::OperandShape("x86 graph element store operand")),
        }
        Ok(())
    }

    fn emit_clamped_double_store(&mut self, base: u8, index: u8, value: u8) {
        let zero = self.ops.new_dynamic_label();
        let maximum = self.ops.new_dynamic_label();
        let increment = self.ops.new_dynamic_label();
        let store = self.ops.new_dynamic_label();
        dynasm!(self.ops ; .arch x64
            ; pxor xmm15, xmm15 ; ucomisd Rx(value), xmm15 ; jp =>zero ; jbe =>zero);
        self.load_immediate(10, 255_f64.to_bits());
        dynasm!(self.ops ; .arch x64 ; movq xmm15, r10 ; ucomisd Rx(value), xmm15 ; jae =>maximum
            ; cvttsd2si r11d, Rx(value) ; cvtsi2sd xmm15, r11d);
        self.load_immediate(10, 0.5_f64.to_bits());
        dynasm!(self.ops ; .arch x64 ; sub rsp, 16 ; mov [rsp], r10);
        self.sp_delta += 16;
        dynasm!(self.ops ; .arch x64 ; addsd xmm15, QWORD [rsp] ; add rsp, 16);
        self.sp_delta -= 16;
        // This noncollecting packet contains only scalar bits. No exit or slot
        // access runs while it is reserved, so canonical frame geometry stays
        // balanced and no safepoint can observe it.
        dynasm!(self.ops ; .arch x64
            ; ucomisd Rx(value), xmm15 ; jb =>store ; ja =>increment
            ; test r11d, 1 ; jz =>store
            ; =>increment ; inc r11d ; jmp =>store
            ; =>zero ; xor r11d, r11d ; jmp =>store
            ; =>maximum ; mov r11d, 255
            ; =>store ; mov r10d, Rd(index) ; mov BYTE [Rq(base) + r10], r11b);
    }
}
