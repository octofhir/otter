//! x86 Number conversions over canonical tagged values and unboxed scalars.
//!
//! # Contents
//! - Checked tag removal and exact integral/index conversions.
//! - Canonical Number boxing through the shared x86 value encoder.
//! - Total ECMAScript ToInt32 through its typed NoAlloc leaf.
//!
//! # Invariants
//! - CVTTSD2SI's sentinel is accepted only after an ordered exact round trip.
//! - Numeric conversion rejects negative zero; an element index accepts it.
//! - Tagged checks and precision guards commit no allocated result on failure.
//!
//! # See also
//! - `crate::x86_64::values` owns double purification and integer tags.

use super::*;

impl Codegen<'_> {
    pub(super) fn emit_conversion(&mut self, node: NodeId) -> Result<bool, Unsupported> {
        match self.graph.node(node).kind.clone() {
            Kind::CheckedTaggedToInt32 => {
                let a = Self::gp(self.loc(node).inputs[0]);
                let dst = Self::gp(self.loc(node).result.unwrap());
                let exit = self.eager_exit(node, DeoptReason::WrongType);
                dynasm!(self.ops ; .arch x64
                    ; mov r10, Rq(a) ; sar r10, 49 ; cmp r10, -1 ; jne =>exit
                    ; mov Rd(dst), Rd(a)
                );
            }
            Kind::CheckedTaggedToFloat64 => {
                let a = Self::gp(self.loc(node).inputs[0]);
                let dst = Self::fp(self.loc(node).result.unwrap());
                let exit = self.eager_exit(node, DeoptReason::WrongType);
                emit_tagged_number_to_float(&mut self.ops, a, dst, exit);
            }
            Kind::Int32ToTagged => {
                let a = Self::gp(self.loc(node).inputs[0]);
                let dst = Self::gp(self.loc(node).result.unwrap());
                crate::x86_64::values::emit_box_int32(&mut self.ops, a, dst, 11);
            }
            Kind::Float64ToTagged => {
                let a = Self::fp(self.loc(node).inputs[0]);
                let dst = Self::gp(self.loc(node).result.unwrap());
                self.emit_box_float64(node, dst, a);
            }
            Kind::Int32ToFloat64 => {
                let a = Self::gp(self.loc(node).inputs[0]);
                let dst = Self::fp(self.loc(node).result.unwrap());
                dynasm!(self.ops ; .arch x64 ; cvtsi2sd Rx(dst), Rd(a));
            }
            Kind::TruncateFloat64ToInt32 => {
                let a = Self::fp(self.loc(node).inputs[0]);
                let dst = Self::gp(self.loc(node).result.unwrap());
                let live = self.loc(node).live_registers.clone();
                let saved = self.emit_save_registers(&live);
                dynasm!(self.ops ; .arch x64 ; movsd xmm0, Rx(a));
                self.emit_scalar_vm_leaf(
                    abi::STUB_NUMBER_TO_INT32_F64_LEAF,
                    otter_vm::runtime_stubs::NUMBER_TO_INT32_F64_LEAF.entry_addr() as u64,
                );
                // Park before restoring a live original RAX.
                dynasm!(self.ops ; .arch x64 ; mov r10, rax);
                self.emit_restore_registers(&live, saved);
                dynasm!(self.ops ; .arch x64 ; mov Rd(dst), r10d);
            }
            Kind::CheckedFloat64ToInt32 => {
                let a = Self::fp(self.loc(node).inputs[0]);
                let dst = Self::gp(self.loc(node).result.unwrap());
                let exit = self.eager_exit(node, DeoptReason::LostPrecision);
                let minus_zero = self.eager_exit(node, DeoptReason::MinusZero);
                emit_checked_float_to_int(&mut self.ops, a, dst, exit, Some(minus_zero));
            }
            Kind::CheckedFloat64ToIndex => {
                let a = Self::fp(self.loc(node).inputs[0]);
                let dst = Self::gp(self.loc(node).result.unwrap());
                let exit = self.eager_exit(node, DeoptReason::InvalidIndex);
                emit_checked_float_to_int(&mut self.ops, a, dst, exit, None);
            }
            Kind::CheckedTaggedToIndex => {
                let a = Self::gp(self.loc(node).inputs[0]);
                let dst = Self::gp(self.loc(node).result.unwrap());
                let temp = self.loc(node).fp_temps[0];
                let exit = self.eager_exit(node, DeoptReason::InvalidIndex);
                let integer = self.ops.new_dynamic_label();
                let done = self.ops.new_dynamic_label();
                dynasm!(self.ops ; .arch x64
                    ; mov r10, Rq(a) ; sar r10, 49 ; cmp r10, -1 ; je =>integer
                );
                emit_tagged_number_to_float(&mut self.ops, a, temp, exit);
                emit_checked_float_to_int(&mut self.ops, temp, dst, exit, None);
                dynasm!(self.ops ; .arch x64 ; jmp =>done ; =>integer ; mov Rd(dst), Rd(a) ; =>done);
            }
            Kind::BooleanToInt32 => {
                let a = Self::gp(self.loc(node).inputs[0]);
                let dst = Self::gp(self.loc(node).result.unwrap());
                dynasm!(self.ops ; .arch x64 ; mov Rd(dst), Rd(a) ; and Rd(dst), 1);
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    pub(in super::super) fn emit_box_float64(&mut self, _node: NodeId, destination: u8, input: u8) {
        emit_box_number(&mut self.ops, input, destination);
    }
}

pub(in crate::graph::x86_64) fn emit_tagged_number_to_float(
    ops: &mut Assembler,
    a: u8,
    dst: u8,
    exit: DynamicLabel,
) {
    let integer = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    dynasm!(ops ; .arch x64
        ; mov r10, Rq(a) ; sar r10, 49 ; cmp r10, -1 ; je =>integer
    );
    crate::x86_64::values::emit_load_u64(ops, 11, tag::NUMBER_TAG);
    dynasm!(ops ; .arch x64 ; test Rq(a), r11 ; jz =>exit);
    crate::x86_64::values::emit_load_u64(ops, 11, tag::DOUBLE_ENCODE_OFFSET);
    dynasm!(ops ; .arch x64
        ; mov r10, Rq(a) ; sub r10, r11 ; movq Rx(dst), r10 ; jmp =>done
        ; =>integer ; cvtsi2sd Rx(dst), Rd(a) ; =>done
    );
}

pub(super) fn emit_checked_float_to_int(
    ops: &mut Assembler,
    a: u8,
    dst: u8,
    exit: DynamicLabel,
    minus_zero: Option<DynamicLabel>,
) {
    dynasm!(ops ; .arch x64
        ; cvttsd2si r10d, Rx(a) ; cvtsi2sd xmm15, r10d
        ; ucomisd Rx(a), xmm15 ; jp =>exit ; jne =>exit
    );
    if let Some(minus_zero) = minus_zero {
        let nonzero = ops.new_dynamic_label();
        dynasm!(ops ; .arch x64
            ; test r10d, r10d ; jnz =>nonzero
            ; movq r11, Rx(a) ; test r11, r11 ; js =>minus_zero ; =>nonzero
        );
    }
    dynasm!(ops ; .arch x64 ; mov Rd(dst), r10d);
}

/// Prefer the int32 representation only when the exact round trip succeeds
/// and the zero has a positive sign; all other doubles use the shared encoder.
pub(super) fn emit_box_number(ops: &mut Assembler, a: u8, dst: u8) {
    let double = ops.new_dynamic_label();
    let integer = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    dynasm!(ops ; .arch x64
        ; cvttsd2si r10d, Rx(a) ; cvtsi2sd xmm15, r10d
        ; ucomisd Rx(a), xmm15 ; jp =>double ; jne =>double
        ; test r10d, r10d ; jnz =>integer
        ; movq r11, Rx(a) ; test r11, r11 ; js =>double ; =>integer
    );
    crate::x86_64::values::emit_box_int32(ops, 10, dst, 11);
    dynasm!(ops ; .arch x64 ; jmp =>done ; =>double);
    crate::x86_64::values::emit_box_double(ops, a, dst, 11);
    dynasm!(ops ; .arch x64 ; =>done);
}
