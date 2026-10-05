//! x86 scalar encoding on the shared Graph allocation and exit contracts.
//!
//! # Contents
//! - Checked integer and unboxed floating arithmetic.
//! - Number representation conversions and canonical boxing.
//! - Tagged predicates and condition flags consumed by control encoding.
//!
//! # Invariants
//! - R10/R11 and XMM15 are reserved scratch, never allocator homes.
//! - Eager guards consume original operands before committing a result.
//! - Unordered comparisons satisfy only NotEqual, as JavaScript requires.
//! - NoAlloc leaves preserve exact-live registers in untraced save areas.
//!
//! # See also
//! - `super::homes` owns stack deltas and temporary register preservation.
//! - `crate::x86_64::values` owns the shared tagged-number encoding.

use super::*;

mod arithmetic;
pub(in crate::graph::x86_64) mod conversions;
mod predicates;
#[cfg(all(test, target_arch = "x86_64"))]
mod tests;

#[derive(Clone, Copy)]
pub(super) enum Int32Operand {
    Register(u8),
    Constant(i32),
}

impl Codegen<'_> {
    pub(super) fn emit_scalar(&mut self, node: NodeId) -> Result<bool, Unsupported> {
        Ok(self.emit_arithmetic(node)?
            || self.emit_conversion(node)?
            || self.emit_predicate(node)?)
    }

    pub(super) fn scalar_int32_operand(&self, location: Location) -> Int32Operand {
        match location {
            Location::Gp(register) => Int32Operand::Register(register),
            Location::Constant(node) => match self.graph.node(node).kind {
                Kind::ConstInt32(value) => Int32Operand::Constant(value),
                _ => unreachable!("int32 operand constant"),
            },
            _ => unreachable!("int32 register or constant"),
        }
    }

    pub(super) fn emit_int32_compare(&mut self, a: u8, operand: Location) {
        let operand = self.scalar_int32_operand(operand);
        emit_int32_compare(&mut self.ops, a, operand);
    }

    pub(super) fn emit_branch_condition(
        &mut self,
        condition: Condition,
        float: bool,
        target: DynamicLabel,
    ) {
        emit_branch_condition(&mut self.ops, condition, float, target);
    }

    pub(super) fn emit_cset_bool(&mut self, destination: u8, condition: Condition, float: bool) {
        emit_cset_bool(&mut self.ops, destination, condition, float);
    }
}

fn emit_int32_compare(ops: &mut Assembler, a: u8, operand: Int32Operand) {
    match operand {
        Int32Operand::Register(b) => dynasm!(ops ; .arch x64 ; cmp Rd(a), Rd(b)),
        Int32Operand::Constant(value) => dynasm!(ops ; .arch x64 ; cmp Rd(a), DWORD value),
    }
}

/// Consume flags without changing either operand. For floating flags from
/// UCOMISD, PF distinguishes unordered from an ordered equal/less result.
fn emit_branch_condition(
    ops: &mut Assembler,
    condition: Condition,
    float: bool,
    target: DynamicLabel,
) {
    if float {
        if condition == Condition::NotEqual {
            dynasm!(ops ; .arch x64 ; jp =>target ; jne =>target);
            return;
        }
        let ordered = ops.new_dynamic_label();
        dynasm!(ops ; .arch x64 ; jp =>ordered);
        match condition {
            Condition::Equal => dynasm!(ops ; .arch x64 ; je =>target),
            Condition::Less => dynasm!(ops ; .arch x64 ; jb =>target),
            Condition::LessEqual => dynasm!(ops ; .arch x64 ; jbe =>target),
            Condition::Greater => dynasm!(ops ; .arch x64 ; ja =>target),
            Condition::GreaterEqual => dynasm!(ops ; .arch x64 ; jae =>target),
            Condition::NotEqual => unreachable!(),
        }
        dynasm!(ops ; .arch x64 ; =>ordered);
    } else {
        match condition {
            Condition::Equal => dynasm!(ops ; .arch x64 ; je =>target),
            Condition::NotEqual => dynasm!(ops ; .arch x64 ; jne =>target),
            Condition::Less => dynasm!(ops ; .arch x64 ; jl =>target),
            Condition::LessEqual => dynasm!(ops ; .arch x64 ; jle =>target),
            Condition::Greater => dynasm!(ops ; .arch x64 ; jg =>target),
            Condition::GreaterEqual => dynasm!(ops ; .arch x64 ; jge =>target),
        }
    }
}

fn emit_cset_bool(ops: &mut Assembler, destination: u8, condition: Condition, float: bool) {
    let yes = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    emit_branch_condition(ops, condition, float, yes);
    crate::x86_64::values::emit_load_u64(ops, destination, tag::VALUE_FALSE);
    dynasm!(ops ; .arch x64 ; jmp =>done ; =>yes);
    crate::x86_64::values::emit_load_u64(ops, destination, tag::VALUE_TRUE);
    dynasm!(ops ; .arch x64 ; =>done);
}
