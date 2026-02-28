//! Bytecode to Cranelift IR translation.
//!
//! Values are represented as NaN-boxed `i64` (matching `otter-vm-core::Value`).
//! This baseline translator implements a guarded int32 fast path for a useful
//! subset of bytecode instructions. Any unsupported type combination bails out
//! by returning `BAILOUT_SENTINEL`, allowing the caller to re-execute in the
//! interpreter.

use cranelift_codegen::ir::condcodes::{FloatCC, IntCC};
use cranelift_codegen::ir::instructions::BlockArg;
use cranelift_codegen::ir::{InstBuilder, MemFlags, StackSlotData, StackSlotKind};
use cranelift_codegen::ir::{Value, types};
use cranelift_frontend::FunctionBuilder;
use otter_vm_bytecode::instruction::Instruction;
use otter_vm_bytecode::operand::{ConstantIndex, LocalIndex, Register};
use otter_vm_bytecode::{Constant, Function};

use crate::JitError;
use crate::bailout::BAILOUT_SENTINEL;
use crate::runtime_helpers::{HelperKind, HelperRefs};
use crate::type_guards::{self, ArithOp, BitwiseOp, SpecializationHint};

fn jump_target(pc: usize, offset: i32, instruction_count: usize) -> Result<usize, JitError> {
    let target = pc as i64 + offset as i64;
    if !(0..instruction_count as i64).contains(&target) {
        return Err(JitError::InvalidJumpTarget {
            pc,
            offset,
            instruction_count,
        });
    }
    Ok(target as usize)
}

fn unsupported(pc: usize, instruction: &Instruction) -> JitError {
    let debug = format!("{:?}", instruction);
    let opcode = debug.split([' ', '{', '(']).next().unwrap_or("unknown");
    JitError::UnsupportedInstruction {
        pc,
        opcode: opcode.to_string(),
    }
}

fn number_to_nanbox_bits(number: f64) -> i64 {
    // Mirrors otter-vm-core::Value::number semantics.
    if number.is_nan() {
        return type_guards::TAG_NAN;
    }

    if number.fract() == 0.0
        && number >= i32::MIN as f64
        && number <= i32::MAX as f64
        && (number != 0.0 || (1.0_f64 / number).is_sign_positive())
    {
        return type_guards::TAG_INT32 | ((number as i32 as u32) as i64);
    }

    number.to_bits() as i64
}

fn constant_to_nanbox_bits(constant: &Constant) -> Option<i64> {
    match constant {
        Constant::Number(number) => Some(number_to_nanbox_bits(*number)),
        _ => None,
    }
}

fn resolve_const_bits(constants: &[Constant], idx: ConstantIndex) -> Option<i64> {
    constants
        .get(idx.index() as usize)
        .and_then(constant_to_nanbox_bits)
}

#[inline]
fn is_supported_baseline_opcode(instruction: &Instruction) -> bool {
    matches!(
        instruction,
        Instruction::LoadUndefined { .. }
            | Instruction::LoadNull { .. }
            | Instruction::LoadTrue { .. }
            | Instruction::LoadFalse { .. }
            | Instruction::LoadInt8 { .. }
            | Instruction::LoadInt32 { .. }
            | Instruction::LoadConst { .. }
            | Instruction::GetLocal { .. }
            | Instruction::SetLocal { .. }
            | Instruction::Move { .. }
            | Instruction::Add { .. }
            | Instruction::Sub { .. }
            | Instruction::Mul { .. }
            | Instruction::Div { .. }
            | Instruction::Mod { .. }
            | Instruction::Neg { .. }
            | Instruction::Inc { .. }
            | Instruction::Dec { .. }
            | Instruction::BitAnd { .. }
            | Instruction::BitOr { .. }
            | Instruction::BitXor { .. }
            | Instruction::BitNot { .. }
            | Instruction::Shl { .. }
            | Instruction::Shr { .. }
            | Instruction::Ushr { .. }
            | Instruction::Eq { .. }
            | Instruction::StrictEq { .. }
            | Instruction::Ne { .. }
            | Instruction::StrictNe { .. }
            | Instruction::Lt { .. }
            | Instruction::Le { .. }
            | Instruction::Gt { .. }
            | Instruction::Ge { .. }
            | Instruction::Not { .. }
            | Instruction::Jump { .. }
            | Instruction::JumpIfTrue { .. }
            | Instruction::JumpIfFalse { .. }
            | Instruction::JumpIfNullish { .. }
            | Instruction::JumpIfNotNullish { .. }
            | Instruction::Return { .. }
            | Instruction::ReturnUndefined
            | Instruction::Nop
            | Instruction::AddInt32 { .. }
            | Instruction::SubInt32 { .. }
            | Instruction::MulInt32 { .. }
            | Instruction::DivInt32 { .. }
            | Instruction::AddNumber { .. }
            | Instruction::SubNumber { .. }
    )
}

/// Check if an instruction is supported when runtime helpers are available.
///
/// This extends the baseline set with property access and other operations
/// that delegate to extern "C" helper functions.
#[inline]
fn is_supported_with_helpers(instruction: &Instruction) -> bool {
    is_supported_baseline_opcode(instruction)
        || matches!(
            instruction,
            Instruction::GetPropConst { .. }
                | Instruction::SetPropConst { .. }
                | Instruction::Call { .. }
                | Instruction::GetLocalProp { .. }
                | Instruction::NewObject { .. }
                | Instruction::NewArray { .. }
                | Instruction::GetGlobal { .. }
                | Instruction::SetGlobal { .. }
        )
}

/// Fast eligibility check for the current baseline translator subset.
///
/// Returns `true` only when all instructions are supported and control-flow
/// jump targets are in range.
pub fn can_translate_function(function: &Function) -> bool {
    can_translate_function_with_constants(function, &[])
}

/// Fast eligibility check including module constants.
///
/// Supports `LoadConst` only for number constants that can be represented
/// as non-pointer NaN-boxed values.
pub fn can_translate_function_with_constants(function: &Function, constants: &[Constant]) -> bool {
    can_translate_impl(function, constants, false)
}

/// Fast eligibility check when runtime helpers are available.
///
/// Extends the baseline set with property access instructions that delegate
/// to extern "C" runtime helper functions.
pub fn can_translate_function_with_helpers(function: &Function, constants: &[Constant]) -> bool {
    can_translate_impl(function, constants, true)
}

fn can_translate_impl(function: &Function, constants: &[Constant], has_helpers: bool) -> bool {
    if function.flags.has_rest
        || function.flags.uses_arguments
        || function.flags.uses_eval
        || function.flags.is_async
        || function.flags.is_generator
        || !function.upvalues.is_empty()
        || u16::from(function.param_count) > function.local_count
    {
        return false;
    }

    let instruction_count = function.instructions.read().len();
    if instruction_count == 0 {
        return true;
    }

    let opcode_check = if has_helpers {
        is_supported_with_helpers
    } else {
        is_supported_baseline_opcode
    };

    for (pc, instruction) in function.instructions.read().iter().enumerate() {
        if !opcode_check(instruction) {
            return false;
        }

        match instruction {
            Instruction::LoadConst { idx, .. } => {
                if resolve_const_bits(constants, *idx).is_none() {
                    return false;
                }
            }
            Instruction::GetLocal { idx, .. }
            | Instruction::SetLocal { idx, .. }
            | Instruction::GetLocalProp { local_idx: idx, .. } => {
                if idx.index() >= function.local_count {
                    return false;
                }
            }
            _ => {}
        }

        match instruction {
            Instruction::Jump { offset }
            | Instruction::JumpIfTrue { offset, .. }
            | Instruction::JumpIfFalse { offset, .. }
            | Instruction::JumpIfNullish { offset, .. }
            | Instruction::JumpIfNotNullish { offset, .. } => {
                if jump_target(pc, offset.offset(), instruction_count).is_err() {
                    return false;
                }
            }
            _ => {}
        }
    }

    true
}

fn read_reg(
    builder: &mut FunctionBuilder<'_>,
    slots: &[cranelift_codegen::ir::StackSlot],
    reg: Register,
) -> Value {
    builder
        .ins()
        .stack_load(types::I64, slots[reg.index() as usize], 0)
}

fn read_local(
    builder: &mut FunctionBuilder<'_>,
    slots: &[cranelift_codegen::ir::StackSlot],
    idx: LocalIndex,
) -> Value {
    builder
        .ins()
        .stack_load(types::I64, slots[idx.index() as usize], 0)
}

fn write_reg(
    builder: &mut FunctionBuilder<'_>,
    slots: &[cranelift_codegen::ir::StackSlot],
    reg: Register,
    value: Value,
) {
    builder
        .ins()
        .stack_store(value, slots[reg.index() as usize], 0);
}

fn write_local(
    builder: &mut FunctionBuilder<'_>,
    slots: &[cranelift_codegen::ir::StackSlot],
    idx: LocalIndex,
    value: Value,
) {
    builder
        .ins()
        .stack_store(value, slots[idx.index() as usize], 0);
}

/// Emit a `return BAILOUT_SENTINEL` — signals the caller to re-execute
/// in the interpreter.
fn emit_bailout_return(builder: &mut FunctionBuilder<'_>) {
    let sentinel = builder.ins().iconst(types::I64, BAILOUT_SENTINEL);
    builder.ins().return_(&[sentinel]);
}

/// Wire a guarded fast-path result and make the slow path bail out.
#[inline]
fn lower_guarded_or_bail(
    builder: &mut FunctionBuilder<'_>,
    guarded: type_guards::GuardedResult,
) -> Value {
    builder.switch_to_block(guarded.slow_block);
    emit_bailout_return(builder);

    builder.switch_to_block(guarded.merge_block);
    guarded.result
}

/// Initialize a parameter local from argv or `undefined` when missing.
fn init_param_local(
    builder: &mut FunctionBuilder<'_>,
    args_ptr: Value,
    argc: Value,
    param_idx: usize,
    undef: Value,
) -> Value {
    let load_block = builder.create_block();
    let undef_block = builder.create_block();
    let merge_block = builder.create_block();
    builder.append_block_param(merge_block, types::I64);

    let idx = builder.ins().iconst(types::I32, param_idx as i64);
    let has_arg = builder.ins().icmp(IntCC::UnsignedGreaterThan, argc, idx);
    builder
        .ins()
        .brif(has_arg, load_block, &[], undef_block, &[]);

    builder.switch_to_block(load_block);
    let load_offset = (param_idx * 8) as i32;
    let loaded = builder
        .ins()
        .load(types::I64, MemFlags::trusted(), args_ptr, load_offset);
    builder.ins().jump(merge_block, &[BlockArg::Value(loaded)]);

    builder.switch_to_block(undef_block);
    builder.ins().jump(merge_block, &[BlockArg::Value(undef)]);

    builder.switch_to_block(merge_block);
    builder.block_params(merge_block)[0]
}

/// Translate a bytecode function into Cranelift IR.
///
/// This baseline path supports a guarded int32 subset and bails out for
/// generic/non-fast-path cases.
pub fn translate_function(
    builder: &mut FunctionBuilder<'_>,
    function: &Function,
) -> Result<(), JitError> {
    translate_function_with_constants(builder, function, &[], None)
}

/// Translate a bytecode function into Cranelift IR with constant pool access.
pub fn translate_function_with_constants(
    builder: &mut FunctionBuilder<'_>,
    function: &Function,
    constants: &[Constant],
    helpers: Option<&HelperRefs>,
) -> Result<(), JitError> {
    let instruction_count = function.instructions.read().len();
    if instruction_count == 0 {
        let undef = builder.ins().iconst(types::I64, type_guards::TAG_UNDEFINED);
        builder.ins().return_(&[undef]);
        return Ok(());
    }

    // Snapshot type feedback for speculative optimization.
    // Read the feedback vector once at compile time (not during IR emission).
    let feedback_snapshot: Vec<_> = {
        let fv = function.feedback_vector.read();
        fv.iter().map(|m| m.type_observations).collect()
    };
    let get_hint = |feedback_index: u16| -> SpecializationHint {
        SpecializationHint::from_type_flags(feedback_snapshot.get(feedback_index as usize))
    };

    let reg_count = function.register_count as usize;
    let mut reg_slots = Vec::with_capacity(reg_count);
    for _ in 0..reg_count {
        reg_slots.push(builder.create_sized_stack_slot(StackSlotData::new(
            StackSlotKind::ExplicitSlot,
            8,
            8,
        )));
    }
    let local_count = function.local_count as usize;
    let mut local_slots = Vec::with_capacity(local_count);
    for _ in 0..local_count {
        local_slots.push(builder.create_sized_stack_slot(StackSlotData::new(
            StackSlotKind::ExplicitSlot,
            8,
            8,
        )));
    }

    let mut blocks = Vec::with_capacity(instruction_count);
    for _ in 0..instruction_count {
        blocks.push(builder.create_block());
    }

    let entry = builder.create_block();
    builder.append_block_params_for_function_params(entry);
    let exit = builder.create_block();

    builder.switch_to_block(entry);
    let entry_params = builder.block_params(entry);
    // Signature: (ctx: I64, args_ptr: I64, argc: I32) -> I64
    let ctx_ptr = entry_params[0];
    let args_ptr = entry_params[1];
    let argc = entry_params[2];
    let undef = builder.ins().iconst(types::I64, type_guards::TAG_UNDEFINED);
    for idx in 0..reg_count {
        builder.ins().stack_store(undef, reg_slots[idx], 0);
    }
    let param_count = function.param_count as usize;
    for idx in 0..local_count {
        let init = if idx < param_count {
            init_param_local(builder, args_ptr, argc, idx, undef)
        } else {
            undef
        };
        builder.ins().stack_store(init, local_slots[idx], 0);
    }
    builder.ins().jump(blocks[0], &[]);

    for (pc, instruction) in function.instructions.read().iter().enumerate() {
        builder.switch_to_block(blocks[pc]);

        match instruction {
            Instruction::LoadUndefined { dst } => {
                let v = builder.ins().iconst(types::I64, type_guards::TAG_UNDEFINED);
                write_reg(builder, &reg_slots, *dst, v);
            }
            Instruction::LoadNull { dst } => {
                let v = builder.ins().iconst(types::I64, type_guards::TAG_NULL);
                write_reg(builder, &reg_slots, *dst, v);
            }
            Instruction::LoadTrue { dst } => {
                let v = builder.ins().iconst(types::I64, type_guards::TAG_TRUE);
                write_reg(builder, &reg_slots, *dst, v);
            }
            Instruction::LoadFalse { dst } => {
                let v = builder.ins().iconst(types::I64, type_guards::TAG_FALSE);
                write_reg(builder, &reg_slots, *dst, v);
            }
            Instruction::LoadInt8 { dst, value } => {
                let v = type_guards::emit_box_int32_const(builder, i32::from(*value));
                write_reg(builder, &reg_slots, *dst, v);
            }
            Instruction::LoadInt32 { dst, value } => {
                let v = type_guards::emit_box_int32_const(builder, *value);
                write_reg(builder, &reg_slots, *dst, v);
            }
            Instruction::LoadConst { dst, idx } => {
                let Some(bits) = resolve_const_bits(constants, *idx) else {
                    return Err(unsupported(pc, instruction));
                };
                let v = builder.ins().iconst(types::I64, bits);
                write_reg(builder, &reg_slots, *dst, v);
            }
            Instruction::GetLocal { dst, idx } => {
                let v = read_local(builder, &local_slots, *idx);
                write_reg(builder, &reg_slots, *dst, v);
            }
            Instruction::SetLocal { idx, src } => {
                let v = read_reg(builder, &reg_slots, *src);
                write_local(builder, &local_slots, *idx, v);
            }
            Instruction::Move { dst, src } => {
                let v = read_reg(builder, &reg_slots, *src);
                write_reg(builder, &reg_slots, *dst, v);
            }
            Instruction::Add { dst, lhs, rhs, feedback_index } => {
                let left = read_reg(builder, &reg_slots, *lhs);
                let right = read_reg(builder, &reg_slots, *rhs);
                let hint = get_hint(*feedback_index);
                let guarded =
                    type_guards::emit_specialized_arith(builder, ArithOp::Add, left, right, hint);
                let out = lower_guarded_or_bail(builder, guarded);
                write_reg(builder, &reg_slots, *dst, out);
            }
            Instruction::Sub { dst, lhs, rhs, feedback_index } => {
                let left = read_reg(builder, &reg_slots, *lhs);
                let right = read_reg(builder, &reg_slots, *rhs);
                let hint = get_hint(*feedback_index);
                let guarded =
                    type_guards::emit_specialized_arith(builder, ArithOp::Sub, left, right, hint);
                let out = lower_guarded_or_bail(builder, guarded);
                write_reg(builder, &reg_slots, *dst, out);
            }
            Instruction::Mul { dst, lhs, rhs, feedback_index } => {
                let left = read_reg(builder, &reg_slots, *lhs);
                let right = read_reg(builder, &reg_slots, *rhs);
                let hint = get_hint(*feedback_index);
                let guarded =
                    type_guards::emit_specialized_arith(builder, ArithOp::Mul, left, right, hint);
                let out = lower_guarded_or_bail(builder, guarded);
                write_reg(builder, &reg_slots, *dst, out);
            }
            Instruction::Div { dst, lhs, rhs, feedback_index } => {
                let left = read_reg(builder, &reg_slots, *lhs);
                let right = read_reg(builder, &reg_slots, *rhs);
                let hint = get_hint(*feedback_index);
                // JS division always returns f64 (even 4/2 → 2.0), so Int32 hint
                // still needs the numeric path for div-by-zero → Infinity handling.
                let guarded = match hint {
                    SpecializationHint::Float64 => type_guards::emit_guarded_f64_div(builder, left, right),
                    _ => type_guards::emit_guarded_numeric_div(builder, left, right),
                };
                let out = lower_guarded_or_bail(builder, guarded);
                write_reg(builder, &reg_slots, *dst, out);
            }
            // Quickened arithmetic: type already known from interpreter feedback
            Instruction::AddInt32 { dst, lhs, rhs, .. } => {
                let left = read_reg(builder, &reg_slots, *lhs);
                let right = read_reg(builder, &reg_slots, *rhs);
                let guarded = type_guards::emit_guarded_i32_arith(builder, ArithOp::Add, left, right);
                let out = lower_guarded_or_bail(builder, guarded);
                write_reg(builder, &reg_slots, *dst, out);
            }
            Instruction::SubInt32 { dst, lhs, rhs, .. } => {
                let left = read_reg(builder, &reg_slots, *lhs);
                let right = read_reg(builder, &reg_slots, *rhs);
                let guarded = type_guards::emit_guarded_i32_arith(builder, ArithOp::Sub, left, right);
                let out = lower_guarded_or_bail(builder, guarded);
                write_reg(builder, &reg_slots, *dst, out);
            }
            Instruction::MulInt32 { dst, lhs, rhs, .. } => {
                let left = read_reg(builder, &reg_slots, *lhs);
                let right = read_reg(builder, &reg_slots, *rhs);
                let guarded = type_guards::emit_guarded_i32_arith(builder, ArithOp::Mul, left, right);
                let out = lower_guarded_or_bail(builder, guarded);
                write_reg(builder, &reg_slots, *dst, out);
            }
            Instruction::DivInt32 { dst, lhs, rhs, .. } => {
                let left = read_reg(builder, &reg_slots, *lhs);
                let right = read_reg(builder, &reg_slots, *rhs);
                let guarded = type_guards::emit_guarded_i32_div(builder, left, right);
                let out = lower_guarded_or_bail(builder, guarded);
                write_reg(builder, &reg_slots, *dst, out);
            }
            Instruction::AddNumber { dst, lhs, rhs, .. } => {
                let left = read_reg(builder, &reg_slots, *lhs);
                let right = read_reg(builder, &reg_slots, *rhs);
                let guarded = type_guards::emit_guarded_f64_arith(builder, ArithOp::Add, left, right);
                let out = lower_guarded_or_bail(builder, guarded);
                write_reg(builder, &reg_slots, *dst, out);
            }
            Instruction::SubNumber { dst, lhs, rhs, .. } => {
                let left = read_reg(builder, &reg_slots, *lhs);
                let right = read_reg(builder, &reg_slots, *rhs);
                let guarded = type_guards::emit_guarded_f64_arith(builder, ArithOp::Sub, left, right);
                let out = lower_guarded_or_bail(builder, guarded);
                write_reg(builder, &reg_slots, *dst, out);
            }
            Instruction::Mod { dst, lhs, rhs } => {
                let left = read_reg(builder, &reg_slots, *lhs);
                let right = read_reg(builder, &reg_slots, *rhs);
                let guarded = type_guards::emit_guarded_i32_mod(builder, left, right);
                let out = lower_guarded_or_bail(builder, guarded);
                write_reg(builder, &reg_slots, *dst, out);
            }
            Instruction::Neg { dst, src } => {
                let val = read_reg(builder, &reg_slots, *src);
                let guarded = type_guards::emit_guarded_i32_neg(builder, val);
                let out = lower_guarded_or_bail(builder, guarded);
                write_reg(builder, &reg_slots, *dst, out);
            }
            Instruction::Inc { dst, src } => {
                let val = read_reg(builder, &reg_slots, *src);
                let guarded = type_guards::emit_guarded_i32_inc(builder, val);
                let out = lower_guarded_or_bail(builder, guarded);
                write_reg(builder, &reg_slots, *dst, out);
            }
            Instruction::Dec { dst, src } => {
                let val = read_reg(builder, &reg_slots, *src);
                let guarded = type_guards::emit_guarded_i32_dec(builder, val);
                let out = lower_guarded_or_bail(builder, guarded);
                write_reg(builder, &reg_slots, *dst, out);
            }
            Instruction::BitAnd { dst, lhs, rhs } => {
                let left = read_reg(builder, &reg_slots, *lhs);
                let right = read_reg(builder, &reg_slots, *rhs);
                let guarded =
                    type_guards::emit_guarded_i32_bitwise(builder, BitwiseOp::And, left, right);
                let out = lower_guarded_or_bail(builder, guarded);
                write_reg(builder, &reg_slots, *dst, out);
            }
            Instruction::BitOr { dst, lhs, rhs } => {
                let left = read_reg(builder, &reg_slots, *lhs);
                let right = read_reg(builder, &reg_slots, *rhs);
                let guarded =
                    type_guards::emit_guarded_i32_bitwise(builder, BitwiseOp::Or, left, right);
                let out = lower_guarded_or_bail(builder, guarded);
                write_reg(builder, &reg_slots, *dst, out);
            }
            Instruction::BitXor { dst, lhs, rhs } => {
                let left = read_reg(builder, &reg_slots, *lhs);
                let right = read_reg(builder, &reg_slots, *rhs);
                let guarded =
                    type_guards::emit_guarded_i32_bitwise(builder, BitwiseOp::Xor, left, right);
                let out = lower_guarded_or_bail(builder, guarded);
                write_reg(builder, &reg_slots, *dst, out);
            }
            Instruction::Shl { dst, lhs, rhs } => {
                let left = read_reg(builder, &reg_slots, *lhs);
                let right = read_reg(builder, &reg_slots, *rhs);
                let guarded =
                    type_guards::emit_guarded_i32_bitwise(builder, BitwiseOp::Shl, left, right);
                let out = lower_guarded_or_bail(builder, guarded);
                write_reg(builder, &reg_slots, *dst, out);
            }
            Instruction::Shr { dst, lhs, rhs } => {
                let left = read_reg(builder, &reg_slots, *lhs);
                let right = read_reg(builder, &reg_slots, *rhs);
                let guarded =
                    type_guards::emit_guarded_i32_bitwise(builder, BitwiseOp::Shr, left, right);
                let out = lower_guarded_or_bail(builder, guarded);
                write_reg(builder, &reg_slots, *dst, out);
            }
            Instruction::Ushr { dst, lhs, rhs } => {
                let left = read_reg(builder, &reg_slots, *lhs);
                let right = read_reg(builder, &reg_slots, *rhs);
                let guarded =
                    type_guards::emit_guarded_i32_bitwise(builder, BitwiseOp::Ushr, left, right);
                let out = lower_guarded_or_bail(builder, guarded);
                write_reg(builder, &reg_slots, *dst, out);
            }
            Instruction::BitNot { dst, src } => {
                let val = read_reg(builder, &reg_slots, *src);
                let guarded = type_guards::emit_guarded_i32_bitnot(builder, val);
                let out = lower_guarded_or_bail(builder, guarded);
                write_reg(builder, &reg_slots, *dst, out);
            }
            Instruction::Eq { dst, lhs, rhs } => {
                let left = read_reg(builder, &reg_slots, *lhs);
                let right = read_reg(builder, &reg_slots, *rhs);
                let guarded = type_guards::emit_guarded_numeric_cmp(
                    builder,
                    IntCC::Equal,
                    FloatCC::Equal,
                    left,
                    right,
                );
                let out = lower_guarded_or_bail(builder, guarded);
                write_reg(builder, &reg_slots, *dst, out);
            }
            Instruction::Ne { dst, lhs, rhs } => {
                let left = read_reg(builder, &reg_slots, *lhs);
                let right = read_reg(builder, &reg_slots, *rhs);
                let guarded = type_guards::emit_guarded_numeric_cmp(
                    builder,
                    IntCC::NotEqual,
                    FloatCC::NotEqual,
                    left,
                    right,
                );
                let out = lower_guarded_or_bail(builder, guarded);
                write_reg(builder, &reg_slots, *dst, out);
            }
            Instruction::StrictEq { dst, lhs, rhs } => {
                let left = read_reg(builder, &reg_slots, *lhs);
                let right = read_reg(builder, &reg_slots, *rhs);
                let out = type_guards::emit_strict_eq(builder, left, right, false);
                write_reg(builder, &reg_slots, *dst, out);
            }
            Instruction::StrictNe { dst, lhs, rhs } => {
                let left = read_reg(builder, &reg_slots, *lhs);
                let right = read_reg(builder, &reg_slots, *rhs);
                let out = type_guards::emit_strict_eq(builder, left, right, true);
                write_reg(builder, &reg_slots, *dst, out);
            }
            Instruction::Lt { dst, lhs, rhs } => {
                let left = read_reg(builder, &reg_slots, *lhs);
                let right = read_reg(builder, &reg_slots, *rhs);
                let guarded = type_guards::emit_guarded_numeric_cmp(
                    builder,
                    IntCC::SignedLessThan,
                    FloatCC::LessThan,
                    left,
                    right,
                );
                let out = lower_guarded_or_bail(builder, guarded);
                write_reg(builder, &reg_slots, *dst, out);
            }
            Instruction::Le { dst, lhs, rhs } => {
                let left = read_reg(builder, &reg_slots, *lhs);
                let right = read_reg(builder, &reg_slots, *rhs);
                let guarded = type_guards::emit_guarded_numeric_cmp(
                    builder,
                    IntCC::SignedLessThanOrEqual,
                    FloatCC::LessThanOrEqual,
                    left,
                    right,
                );
                let out = lower_guarded_or_bail(builder, guarded);
                write_reg(builder, &reg_slots, *dst, out);
            }
            Instruction::Gt { dst, lhs, rhs } => {
                let left = read_reg(builder, &reg_slots, *lhs);
                let right = read_reg(builder, &reg_slots, *rhs);
                let guarded = type_guards::emit_guarded_numeric_cmp(
                    builder,
                    IntCC::SignedGreaterThan,
                    FloatCC::GreaterThan,
                    left,
                    right,
                );
                let out = lower_guarded_or_bail(builder, guarded);
                write_reg(builder, &reg_slots, *dst, out);
            }
            Instruction::Ge { dst, lhs, rhs } => {
                let left = read_reg(builder, &reg_slots, *lhs);
                let right = read_reg(builder, &reg_slots, *rhs);
                let guarded = type_guards::emit_guarded_numeric_cmp(
                    builder,
                    IntCC::SignedGreaterThanOrEqual,
                    FloatCC::GreaterThanOrEqual,
                    left,
                    right,
                );
                let out = lower_guarded_or_bail(builder, guarded);
                write_reg(builder, &reg_slots, *dst, out);
            }
            Instruction::Not { dst, src } => {
                let val = read_reg(builder, &reg_slots, *src);
                let truthy = type_guards::emit_is_truthy(builder, val);
                let is_falsy = builder.ins().icmp_imm(IntCC::Equal, truthy, 0);
                let out = type_guards::emit_bool_to_nanbox(builder, is_falsy);
                write_reg(builder, &reg_slots, *dst, out);
            }
            Instruction::Jump { offset } => {
                let target = jump_target(pc, offset.offset(), instruction_count)?;
                builder.ins().jump(blocks[target], &[]);
                continue;
            }
            Instruction::JumpIfTrue { cond, offset } => {
                let cond_val = read_reg(builder, &reg_slots, *cond);
                let truthy = type_guards::emit_is_truthy(builder, cond_val);
                let is_truthy = builder.ins().icmp_imm(IntCC::NotEqual, truthy, 0);
                let jump_to = jump_target(pc, offset.offset(), instruction_count)?;
                let fallthrough = pc + 1;
                if fallthrough < instruction_count {
                    builder
                        .ins()
                        .brif(is_truthy, blocks[jump_to], &[], blocks[fallthrough], &[]);
                } else {
                    builder
                        .ins()
                        .brif(is_truthy, blocks[jump_to], &[], exit, &[]);
                }
                continue;
            }
            Instruction::JumpIfFalse { cond, offset } => {
                let cond_val = read_reg(builder, &reg_slots, *cond);
                let truthy = type_guards::emit_is_truthy(builder, cond_val);
                let is_truthy = builder.ins().icmp_imm(IntCC::NotEqual, truthy, 0);
                let jump_to = jump_target(pc, offset.offset(), instruction_count)?;
                let fallthrough = pc + 1;
                if fallthrough < instruction_count {
                    builder
                        .ins()
                        .brif(is_truthy, blocks[fallthrough], &[], blocks[jump_to], &[]);
                } else {
                    builder
                        .ins()
                        .brif(is_truthy, exit, &[], blocks[jump_to], &[]);
                }
                continue;
            }
            Instruction::JumpIfNullish { src, offset } => {
                let src_val = read_reg(builder, &reg_slots, *src);
                let is_nullish = type_guards::emit_is_nullish(builder, src_val);
                let jump_to = jump_target(pc, offset.offset(), instruction_count)?;
                let fallthrough = pc + 1;
                if fallthrough < instruction_count {
                    builder
                        .ins()
                        .brif(is_nullish, blocks[jump_to], &[], blocks[fallthrough], &[]);
                } else {
                    builder
                        .ins()
                        .brif(is_nullish, blocks[jump_to], &[], exit, &[]);
                }
                continue;
            }
            Instruction::JumpIfNotNullish { src, offset } => {
                let src_val = read_reg(builder, &reg_slots, *src);
                let is_nullish = type_guards::emit_is_nullish(builder, src_val);
                let jump_to = jump_target(pc, offset.offset(), instruction_count)?;
                let fallthrough = pc + 1;
                if fallthrough < instruction_count {
                    builder
                        .ins()
                        .brif(is_nullish, blocks[fallthrough], &[], blocks[jump_to], &[]);
                } else {
                    builder
                        .ins()
                        .brif(is_nullish, exit, &[], blocks[jump_to], &[]);
                }
                continue;
            }
            Instruction::Return { src } => {
                let out = read_reg(builder, &reg_slots, *src);
                builder.ins().return_(&[out]);
                continue;
            }
            Instruction::ReturnUndefined => {
                let undef = builder.ins().iconst(types::I64, type_guards::TAG_UNDEFINED);
                builder.ins().return_(&[undef]);
                continue;
            }
            Instruction::Nop => {}
            Instruction::GetPropConst {
                dst,
                obj,
                name,
                ic_index,
            } => {
                let helper_ref = helpers
                    .and_then(|h| h.get(HelperKind::GetPropConst))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let obj_val = read_reg(builder, &reg_slots, *obj);
                let name_idx = builder.ins().iconst(types::I64, name.index() as i64);
                let ic_idx = builder.ins().iconst(types::I64, *ic_index as i64);
                let call = builder.ins().call(helper_ref, &[ctx_ptr, obj_val, name_idx, ic_idx]);
                let result = builder.inst_results(call)[0];

                // If helper returns BAILOUT_SENTINEL, bail out the whole function
                let bail_block = builder.create_block();
                let continue_block = builder.create_block();
                let sentinel = builder.ins().iconst(types::I64, BAILOUT_SENTINEL);
                let is_bailout = builder.ins().icmp(IntCC::Equal, result, sentinel);
                builder
                    .ins()
                    .brif(is_bailout, bail_block, &[], continue_block, &[]);

                builder.switch_to_block(bail_block);
                emit_bailout_return(builder);

                builder.switch_to_block(continue_block);
                write_reg(builder, &reg_slots, *dst, result);
            }
            // Superinstruction: fused GetLocal + GetPropConst
            Instruction::GetLocalProp {
                dst,
                local_idx,
                name,
                ic_index,
            } => {
                let helper_ref = helpers
                    .and_then(|h| h.get(HelperKind::GetPropConst))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                // Read the local variable directly (GetLocal part)
                let obj_val = read_local(builder, &local_slots, *local_idx);
                // Call GetPropConst helper (GetPropConst part)
                let name_idx = builder.ins().iconst(types::I64, name.index() as i64);
                let ic_idx = builder.ins().iconst(types::I64, *ic_index as i64);
                let call = builder.ins().call(helper_ref, &[ctx_ptr, obj_val, name_idx, ic_idx]);
                let result = builder.inst_results(call)[0];

                let bail_block = builder.create_block();
                let continue_block = builder.create_block();
                let sentinel = builder.ins().iconst(types::I64, BAILOUT_SENTINEL);
                let is_bailout = builder.ins().icmp(IntCC::Equal, result, sentinel);
                builder
                    .ins()
                    .brif(is_bailout, bail_block, &[], continue_block, &[]);

                builder.switch_to_block(bail_block);
                emit_bailout_return(builder);

                builder.switch_to_block(continue_block);
                write_reg(builder, &reg_slots, *dst, result);
            }
            Instruction::SetPropConst {
                obj,
                name,
                val,
                ic_index,
            } => {
                let helper_ref = helpers
                    .and_then(|h| h.get(HelperKind::SetPropConst))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let obj_val = read_reg(builder, &reg_slots, *obj);
                let name_idx = builder.ins().iconst(types::I64, name.index() as i64);
                let value = read_reg(builder, &reg_slots, *val);
                let ic_idx = builder.ins().iconst(types::I64, *ic_index as i64);
                let call = builder.ins().call(
                    helper_ref,
                    &[ctx_ptr, obj_val, name_idx, value, ic_idx],
                );
                let result = builder.inst_results(call)[0];

                // If helper returns BAILOUT_SENTINEL, bail out the whole function
                let bail_block = builder.create_block();
                let continue_block = builder.create_block();
                let sentinel = builder.ins().iconst(types::I64, BAILOUT_SENTINEL);
                let is_bailout = builder.ins().icmp(IntCC::Equal, result, sentinel);
                builder
                    .ins()
                    .brif(is_bailout, bail_block, &[], continue_block, &[]);

                builder.switch_to_block(bail_block);
                emit_bailout_return(builder);

                builder.switch_to_block(continue_block);
                // SetPropConst doesn't write to a dst register
            }
            Instruction::Call { dst, func, argc } => {
                let helper_ref = helpers
                    .and_then(|h| h.get(HelperKind::CallFunction))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let callee_val = read_reg(builder, &reg_slots, *func);
                let argc_val = builder.ins().iconst(types::I64, *argc as i64);

                // Build argument array on the stack
                let argv_ptr = if *argc > 0 {
                    let slot = builder.create_sized_stack_slot(StackSlotData::new(
                        StackSlotKind::ExplicitSlot,
                        (*argc as u32) * 8,
                        8,
                    ));
                    for i in 0..(*argc as u16) {
                        let arg_val =
                            read_reg(builder, &reg_slots, Register(func.0 + 1 + i));
                        builder
                            .ins()
                            .stack_store(arg_val, slot, (i as i32) * 8);
                    }
                    builder.ins().stack_addr(types::I64, slot, 0)
                } else {
                    builder.ins().iconst(types::I64, 0) // null pointer
                };

                let call = builder
                    .ins()
                    .call(helper_ref, &[ctx_ptr, callee_val, argc_val, argv_ptr]);
                let result = builder.inst_results(call)[0];

                // If helper returns BAILOUT_SENTINEL, bail out the whole function
                let bail_block = builder.create_block();
                let continue_block = builder.create_block();
                let sentinel = builder.ins().iconst(types::I64, BAILOUT_SENTINEL);
                let is_bailout = builder.ins().icmp(IntCC::Equal, result, sentinel);
                builder
                    .ins()
                    .brif(is_bailout, bail_block, &[], continue_block, &[]);

                builder.switch_to_block(bail_block);
                emit_bailout_return(builder);

                builder.switch_to_block(continue_block);
                write_reg(builder, &reg_slots, *dst, result);
            }
            Instruction::NewObject { dst } => {
                let helper_ref = helpers
                    .and_then(|h| h.get(HelperKind::NewObject))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let call = builder.ins().call(helper_ref, &[ctx_ptr]);
                let result = builder.inst_results(call)[0];

                let bail_block = builder.create_block();
                let continue_block = builder.create_block();
                let sentinel = builder.ins().iconst(types::I64, BAILOUT_SENTINEL);
                let is_bailout = builder.ins().icmp(IntCC::Equal, result, sentinel);
                builder
                    .ins()
                    .brif(is_bailout, bail_block, &[], continue_block, &[]);

                builder.switch_to_block(bail_block);
                emit_bailout_return(builder);

                builder.switch_to_block(continue_block);
                write_reg(builder, &reg_slots, *dst, result);
            }
            Instruction::NewArray { dst, len } => {
                let helper_ref = helpers
                    .and_then(|h| h.get(HelperKind::NewArray))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let len_val = builder.ins().iconst(types::I64, *len as i64);
                let call = builder.ins().call(helper_ref, &[ctx_ptr, len_val]);
                let result = builder.inst_results(call)[0];

                let bail_block = builder.create_block();
                let continue_block = builder.create_block();
                let sentinel = builder.ins().iconst(types::I64, BAILOUT_SENTINEL);
                let is_bailout = builder.ins().icmp(IntCC::Equal, result, sentinel);
                builder
                    .ins()
                    .brif(is_bailout, bail_block, &[], continue_block, &[]);

                builder.switch_to_block(bail_block);
                emit_bailout_return(builder);

                builder.switch_to_block(continue_block);
                write_reg(builder, &reg_slots, *dst, result);
            }
            Instruction::GetGlobal { dst, name, ic_index } => {
                let helper_ref = helpers
                    .and_then(|h| h.get(HelperKind::GetGlobal))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let name_idx = builder.ins().iconst(types::I64, name.0 as i64);
                let ic_idx = builder.ins().iconst(types::I64, *ic_index as i64);
                let call = builder.ins().call(helper_ref, &[ctx_ptr, name_idx, ic_idx]);
                let result = builder.inst_results(call)[0];

                let bail_block = builder.create_block();
                let continue_block = builder.create_block();
                let sentinel = builder.ins().iconst(types::I64, BAILOUT_SENTINEL);
                let is_bailout = builder.ins().icmp(IntCC::Equal, result, sentinel);
                builder
                    .ins()
                    .brif(is_bailout, bail_block, &[], continue_block, &[]);

                builder.switch_to_block(bail_block);
                emit_bailout_return(builder);

                builder.switch_to_block(continue_block);
                write_reg(builder, &reg_slots, *dst, result);
            }
            Instruction::SetGlobal { name, src, ic_index, is_declaration } => {
                let helper_ref = helpers
                    .and_then(|h| h.get(HelperKind::SetGlobal))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let val = read_reg(builder, &reg_slots, *src);
                let name_idx = builder.ins().iconst(types::I64, name.0 as i64);
                let ic_idx = builder.ins().iconst(types::I64, *ic_index as i64);
                let is_decl = builder.ins().iconst(types::I64, *is_declaration as i64);
                let call = builder.ins().call(helper_ref, &[ctx_ptr, name_idx, val, ic_idx, is_decl]);
                let result = builder.inst_results(call)[0];

                let bail_block = builder.create_block();
                let continue_block = builder.create_block();
                let sentinel = builder.ins().iconst(types::I64, BAILOUT_SENTINEL);
                let is_bailout = builder.ins().icmp(IntCC::Equal, result, sentinel);
                builder
                    .ins()
                    .brif(is_bailout, bail_block, &[], continue_block, &[]);

                builder.switch_to_block(bail_block);
                emit_bailout_return(builder);

                builder.switch_to_block(continue_block);
            }
            _ => return Err(unsupported(pc, instruction)),
        }

        let next_pc = pc + 1;
        if next_pc < instruction_count {
            builder.ins().jump(blocks[next_pc], &[]);
        } else {
            let undef = builder.ins().iconst(types::I64, type_guards::TAG_UNDEFINED);
            builder.ins().return_(&[undef]);
        }
    }

    builder.switch_to_block(exit);
    let undef = builder.ins().iconst(types::I64, type_guards::TAG_UNDEFINED);
    builder.ins().return_(&[undef]);

    builder.seal_all_blocks();
    Ok(())
}
