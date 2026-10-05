//! x86 integer guards and unboxed floating arithmetic.
//!
//! # Contents
//! - Late integer result commitment and implicit division registers.
//! - CL/immediate shifts and two-address SSE arithmetic.
//! - Shared typed scalar calls for arithmetic, conversion and primitive string order.
//! - Pure floating remainder calls preserve exact live registers.
//!
//! # Invariants
//! - All eager arithmetic guards leave allocated input registers unchanged.
//! - Division operands exclude RAX/RDX; #DE cases branch before IDIV.
//! - Variable counts stay in CL until the staged result has consumed them.
//!
//! # See also
//! - `super::super::super::registers` owns fixed arithmetic words.

use super::*;

impl Codegen<'_> {
    pub(super) fn emit_arithmetic(&mut self, node: NodeId) -> Result<bool, Unsupported> {
        let kind = self.graph.node(node).kind.clone();
        match kind {
            Kind::Int32Add | Kind::Int32Sub => {
                let (a, b, dst) = (
                    Self::gp(self.loc(node).inputs[0]),
                    self.scalar_int32_operand(self.loc(node).inputs[1]),
                    Self::gp(self.loc(node).result.unwrap()),
                );
                let overflow = self.eager_exit(node, DeoptReason::Overflow);
                emit_add_sub(&mut self.ops, kind == Kind::Int32Add, a, b, dst, Some(overflow));
            }
            Kind::Int32AddWrapping | Kind::Int32SubWrapping => {
                let (a, b, dst) = (
                    Self::gp(self.loc(node).inputs[0]),
                    self.scalar_int32_operand(self.loc(node).inputs[1]),
                    Self::gp(self.loc(node).result.unwrap()),
                );
                emit_add_sub(&mut self.ops, kind == Kind::Int32AddWrapping, a, b, dst, None);
            }
            Kind::Int32Mul => {
                let (a, b, dst) = self.scalar_gp_binary(node);
                let overflow = self.eager_exit(node, DeoptReason::Overflow);
                let minus_zero = self.eager_exit(node, DeoptReason::MinusZero);
                emit_mul(&mut self.ops, a, b, dst, overflow, minus_zero);
            }
            Kind::Int32Div | Kind::Int32Mod => {
                let (a, b, dst) = self.scalar_gp_binary(node);
                debug_assert!(![0, 2].contains(&a) && ![0, 2].contains(&b));
                debug_assert_eq!(dst, if kind == Kind::Int32Div { 0 } else { 2 });
                let lost = self.eager_exit(node, DeoptReason::LostPrecision);
                let minus_zero = self.eager_exit(node, DeoptReason::MinusZero);
                let overflow = if kind == Kind::Int32Div {
                    self.eager_exit(node, DeoptReason::Overflow)
                } else {
                    minus_zero
                };
                emit_div_mod(
                    &mut self.ops,
                    kind == Kind::Int32Mod,
                    a,
                    b,
                    lost,
                    minus_zero,
                    overflow,
                );
            }
            Kind::Int32Negate => {
                let a = Self::gp(self.loc(node).inputs[0]);
                let dst = Self::gp(self.loc(node).result.unwrap());
                let minus_zero = self.eager_exit(node, DeoptReason::MinusZero);
                let overflow = self.eager_exit(node, DeoptReason::Overflow);
                emit_negate(&mut self.ops, a, dst, minus_zero, overflow);
            }
            Kind::Int32BitAnd | Kind::Int32BitOr | Kind::Int32BitXor => {
                let a = Self::gp(self.loc(node).inputs[0]);
                let b = self.scalar_int32_operand(self.loc(node).inputs[1]);
                let dst = Self::gp(self.loc(node).result.unwrap());
                emit_bitwise(&mut self.ops, &kind, a, b, dst);
            }
            Kind::Int32BitNot => {
                let a = Self::gp(self.loc(node).inputs[0]);
                let dst = Self::gp(self.loc(node).result.unwrap());
                dynasm!(self.ops ; .arch x64 ; mov r10d, Rd(a) ; not r10d ; mov Rd(dst), r10d);
            }
            Kind::Int32ShiftLeft
            | Kind::Int32ShiftRight
            | Kind::Int32ShiftRightLogical
            | Kind::Uint32ShiftRightToFloat64 => {
                let a = Self::gp(self.loc(node).inputs[0]);
                let count = self.scalar_int32_operand(self.loc(node).inputs[1]);
                emit_shift(&mut self.ops, &kind, a, count);
                if kind == Kind::Uint32ShiftRightToFloat64 {
                    // R10d cleared its high word: the 64-bit signed conversion
                    // therefore converts the full uint32 domain exactly.
                    let dst = Self::fp(self.loc(node).result.unwrap());
                    dynasm!(self.ops ; .arch x64 ; cvtsi2sd Rx(dst), r10);
                } else {
                    if kind == Kind::Int32ShiftRightLogical {
                        let exit = self.eager_exit(node, DeoptReason::LostPrecision);
                        dynasm!(self.ops ; .arch x64 ; test r10d, r10d ; js =>exit);
                    }
                    let dst = Self::gp(self.loc(node).result.unwrap());
                    dynasm!(self.ops ; .arch x64 ; mov Rd(dst), r10d);
                }
            }
            Kind::Int32Compare(condition) => {
                let a = Self::gp(self.loc(node).inputs[0]);
                let b = self.loc(node).inputs[1];
                let dst = Self::gp(self.loc(node).result.unwrap());
                self.emit_int32_compare(a, b);
                self.emit_cset_bool(dst, condition, false);
            }
            Kind::Float64Add | Kind::Float64Sub | Kind::Float64Mul | Kind::Float64Div => {
                let (a, b, dst) = self.scalar_fp_binary(node);
                emit_float_binary(&mut self.ops, &kind, a, b, dst);
            }
            Kind::Float64Negate => {
                let a = Self::fp(self.loc(node).inputs[0]);
                let dst = Self::fp(self.loc(node).result.unwrap());
                dynasm!(self.ops ; .arch x64 ; movq r10, Rx(a));
                self.load_immediate(11, 1 << 63);
                dynasm!(self.ops ; .arch x64 ; xor r10, r11 ; movq Rx(dst), r10);
            }
            Kind::Float64Mod => {
                let (a, b, dst) = self.scalar_fp_binary(node);
                let live = self.loc(node).live_registers.clone();
                let saved = self.emit_save_registers(&live);
                dynasm!(self.ops ; .arch x64
                    ; movsd xmm15, Rx(b) ; movsd xmm0, Rx(a) ; movsd xmm1, xmm15
                );
                self.emit_scalar_vm_leaf(
                    abi::STUB_NUMBER_REM_F64_LEAF,
                    otter_vm::runtime_stubs::NUMBER_REM_F64_LEAF.entry_addr() as u64,
                );
                dynasm!(self.ops ; .arch x64 ; movsd xmm15, xmm0);
                self.emit_restore_registers(&live, saved);
                dynasm!(self.ops ; .arch x64 ; movsd Rx(dst), xmm15);
            }
            Kind::Float64Compare(condition) => {
                let (a, b) = (
                    Self::fp(self.loc(node).inputs[0]),
                    Self::fp(self.loc(node).inputs[1]),
                );
                let dst = Self::gp(self.loc(node).result.unwrap());
                dynasm!(self.ops ; .arch x64 ; ucomisd Rx(a), Rx(b));
                self.emit_cset_bool(dst, condition, true);
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    fn scalar_gp_binary(&self, node: NodeId) -> (u8, u8, u8) {
        let allocation = self.loc(node);
        (
            Self::gp(allocation.inputs[0]),
            Self::gp(allocation.inputs[1]),
            Self::gp(allocation.result.unwrap()),
        )
    }

    fn scalar_fp_binary(&self, node: NodeId) -> (u8, u8, u8) {
        let allocation = self.loc(node);
        (
            Self::fp(allocation.inputs[0]),
            Self::fp(allocation.inputs[1]),
            Self::fp(allocation.result.unwrap()),
        )
    }

    pub(in crate::graph::x86_64) fn emit_scalar_vm_leaf(
        &mut self,
        stub: abi::RuntimeStubDescriptor,
        entry: u64,
    ) {
        crate::x86_64::values::emit_load_runtime_stub(
            &mut self.ops,
            &mut self.relocations,
            entry,
            stub,
        );
        crate::x86_64::call_abi::emit_runtime_call(&mut self.ops, stub);
    }
}

pub(super) fn emit_add_sub(
    ops: &mut Assembler,
    add: bool,
    a: u8,
    b: Int32Operand,
    dst: u8,
    overflow: Option<DynamicLabel>,
) {
    dynasm!(ops ; .arch x64 ; mov r10d, Rd(a));
    match (add, b) {
        (true, Int32Operand::Register(b)) => dynasm!(ops ; .arch x64 ; add r10d, Rd(b)),
        (false, Int32Operand::Register(b)) => dynasm!(ops ; .arch x64 ; sub r10d, Rd(b)),
        (true, Int32Operand::Constant(b)) => dynasm!(ops ; .arch x64 ; add r10d, DWORD b),
        (false, Int32Operand::Constant(b)) => dynasm!(ops ; .arch x64 ; sub r10d, DWORD b),
    }
    if let Some(overflow) = overflow {
        dynasm!(ops ; .arch x64 ; jo =>overflow);
    }
    dynasm!(ops ; .arch x64 ; mov Rd(dst), r10d);
}

pub(super) fn emit_mul(
    ops: &mut Assembler,
    a: u8,
    b: u8,
    dst: u8,
    overflow: DynamicLabel,
    minus_zero: DynamicLabel,
) {
    let done = ops.new_dynamic_label();
    dynasm!(ops ; .arch x64
        ; mov r10d, Rd(a) ; imul r10d, Rd(b) ; jo =>overflow
        ; test r10d, r10d ; jnz =>done
        ; mov r11d, Rd(a) ; or r11d, Rd(b) ; js =>minus_zero
        ; =>done ; mov Rd(dst), r10d
    );
}

pub(super) fn emit_negate(
    ops: &mut Assembler,
    a: u8,
    dst: u8,
    minus_zero: DynamicLabel,
    overflow: DynamicLabel,
) {
    dynasm!(ops ; .arch x64
        ; test Rd(a), Rd(a) ; jz =>minus_zero
        ; mov r10d, Rd(a) ; neg r10d ; jo =>overflow ; mov Rd(dst), r10d
    );
}

pub(super) fn emit_div_mod(
    ops: &mut Assembler,
    remainder: bool,
    a: u8,
    b: u8,
    lost: DynamicLabel,
    minus_zero: DynamicLabel,
    overflow: DynamicLabel,
) {
    let ordinary = ops.new_dynamic_label();
    let nonzero = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    dynasm!(ops ; .arch x64
        ; test Rd(b), Rd(b) ; jz =>lost
        ; cmp Rd(a), DWORD i32::MIN ; jne =>ordinary
        ; cmp Rd(b), DWORD -1 ; je =>overflow
        ; =>ordinary
    );
    if !remainder {
        dynasm!(ops ; .arch x64
            ; test Rd(a), Rd(a) ; jnz =>nonzero
            ; test Rd(b), Rd(b) ; js =>minus_zero ; =>nonzero
        );
    }
    dynasm!(ops ; .arch x64 ; mov eax, Rd(a) ; cdq ; idiv Rd(b));
    if remainder {
        dynasm!(ops ; .arch x64
            ; test edx, edx ; jnz =>done
            ; test Rd(a), Rd(a) ; js =>minus_zero ; =>done
        );
    } else {
        dynasm!(ops ; .arch x64 ; test edx, edx ; jnz =>lost);
    }
}

pub(super) fn emit_bitwise(ops: &mut Assembler, kind: &Kind, a: u8, b: Int32Operand, dst: u8) {
    dynasm!(ops ; .arch x64 ; mov r10d, Rd(a));
    match (kind, b) {
        (Kind::Int32BitAnd, Int32Operand::Register(b)) => {
            dynasm!(ops ; .arch x64 ; and r10d, Rd(b))
        }
        (Kind::Int32BitOr, Int32Operand::Register(b)) => dynasm!(ops ; .arch x64 ; or r10d, Rd(b)),
        (Kind::Int32BitXor, Int32Operand::Register(b)) => {
            dynasm!(ops ; .arch x64 ; xor r10d, Rd(b))
        }
        (Kind::Int32BitAnd, Int32Operand::Constant(b)) => {
            dynasm!(ops ; .arch x64 ; and r10d, DWORD b)
        }
        (Kind::Int32BitOr, Int32Operand::Constant(b)) => {
            dynasm!(ops ; .arch x64 ; or r10d, DWORD b)
        }
        (Kind::Int32BitXor, Int32Operand::Constant(b)) => {
            dynasm!(ops ; .arch x64 ; xor r10d, DWORD b)
        }
        _ => unreachable!("bitwise integer kind"),
    }
    dynasm!(ops ; .arch x64 ; mov Rd(dst), r10d);
}

pub(super) fn emit_shift(ops: &mut Assembler, kind: &Kind, a: u8, count: Int32Operand) {
    // Stage before writing any allocator result, including an output in RCX.
    dynasm!(ops ; .arch x64 ; mov r10d, Rd(a));
    match (kind, count) {
        (Kind::Int32ShiftLeft, Int32Operand::Constant(count)) => {
            dynasm!(ops ; .arch x64 ; shl r10d, (count & 31) as i8)
        }
        (Kind::Int32ShiftRight, Int32Operand::Constant(count)) => {
            dynasm!(ops ; .arch x64 ; sar r10d, (count & 31) as i8)
        }
        (_, Int32Operand::Constant(count)) => {
            dynasm!(ops ; .arch x64 ; shr r10d, (count & 31) as i8)
        }
        (kind, Int32Operand::Register(count)) => {
            assert_eq!(count, 1, "variable shift is owned by CL");
            match kind {
                Kind::Int32ShiftLeft => dynasm!(ops ; .arch x64 ; shl r10d, cl),
                Kind::Int32ShiftRight => dynasm!(ops ; .arch x64 ; sar r10d, cl),
                _ => dynasm!(ops ; .arch x64 ; shr r10d, cl),
            }
        }
    }
}

pub(super) fn emit_float_binary(ops: &mut Assembler, kind: &Kind, a: u8, b: u8, dst: u8) {
    dynasm!(ops ; .arch x64 ; movsd xmm15, Rx(a));
    match kind {
        Kind::Float64Add => dynasm!(ops ; .arch x64 ; addsd xmm15, Rx(b)),
        Kind::Float64Sub => dynasm!(ops ; .arch x64 ; subsd xmm15, Rx(b)),
        Kind::Float64Mul => dynasm!(ops ; .arch x64 ; mulsd xmm15, Rx(b)),
        Kind::Float64Div => dynasm!(ops ; .arch x64 ; divsd xmm15, Rx(b)),
        _ => unreachable!("floating binary arithmetic"),
    }
    dynasm!(ops ; .arch x64 ; movsd Rx(dst), xmm15);
}
