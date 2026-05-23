//! Bytecode to Cranelift IR translation with NaN-boxing-aware type guards.
//!
//! All values are `i64` in Cranelift (NaN-boxed, matching the interpreter).
//! Arithmetic and comparisons use type guards to specialize for int32 fast
//! paths, falling back to runtime helpers for the generic case.
//!
//! # Instruction coverage
//!
//! **Inline with type guards:** LoadUndefined/Null/True/False/Int8/Int32, Move, Dup,
//! Add/Sub/Mul (+ quickened I32/F64 variants) — guarded i32 fast path + generic fallback,
//! Div/Mod — always generic, Neg/Inc/Dec — guarded i32 + generic,
//! BitAnd/Or/Xor/Not/Shl/Shr/Ushr — guarded i32 + generic,
//! Eq/StrictEq/Ne/StrictNe — bit comparison + generic fallback,
//! Lt/Le/Gt/Ge — guarded i32 comparison + generic fallback,
//! Not — truthiness-based, GetLocal/SetLocal,
//! Jump/JumpIfTrue/JumpIfFalse/JumpIfNullish/JumpIfNotNullish,
//! Return/ReturnUndefined, Nop/Debugger/Pop.
//!
//! **Via helper call:** LoadConst, GetGlobal/SetGlobal, GetPropConst/SetPropConst,
//! GetProp/SetProp, Call, Closure, NewObject, NewArray, GetElem/SetElem,
//! DefineProperty, DeleteProp, GetUpvalue/SetUpvalue, LoadThis, Throw.

use cranelift_codegen::ir::condcodes::{FloatCC, IntCC};
use cranelift_codegen::ir::instructions::BlockArg;
use cranelift_codegen::ir::{types, InstBuilder, StackSlotData, StackSlotKind};
use cranelift_frontend::FunctionBuilder;
use otter_vm_bytecode::instruction::Instruction;
use otter_vm_bytecode::operand::Register;
use otter_vm_bytecode::Function;

use crate::runtime_helpers::{HelperKind, HelperRefs};
use crate::bailout::BAILOUT_SENTINEL;
use crate::type_guards::{
    self, ArithOp, BitwiseOp, GuardedResult, SpecializationHint, TAG_FALSE, TAG_NULL, TAG_TRUE,
    TAG_UNDEFINED,
};
use crate::JitError;

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

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

fn read_reg(
    builder: &mut FunctionBuilder<'_>,
    slots: &[cranelift_codegen::ir::StackSlot],
    reg: Register,
) -> cranelift_codegen::ir::Value {
    builder
        .ins()
        .stack_load(types::I64, slots[reg.index() as usize], 0)
}

fn write_reg(
    builder: &mut FunctionBuilder<'_>,
    slots: &[cranelift_codegen::ir::StackSlot],
    reg: Register,
    value: cranelift_codegen::ir::Value,
) {
    builder
        .ins()
        .stack_store(value, slots[reg.index() as usize], 0);
}

fn fallthrough_or_exit(
    pc: usize,
    len: usize,
    blocks: &[cranelift_codegen::ir::Block],
    exit: cranelift_codegen::ir::Block,
) -> cranelift_codegen::ir::Block {
    if pc + 1 < len {
        blocks[pc + 1]
    } else {
        exit
    }
}

/// Look up the specialization hint for a feedback index.
fn specialization_for(function: &Function, feedback_index: u16) -> SpecializationHint {
    let fv = function.feedback_vector.read();
    let flags = fv.get(feedback_index as usize).map(|m| &m.type_observations);
    SpecializationHint::from_type_flags(flags)
}

/// Map integer comparison condition code to float comparison condition code.
///
/// Uses "ordered" variants: NaN comparisons return false for `<`, `<=`, `>`, `>=`, `==`
/// and `NotEqual` returns true when either operand is NaN (UN | LT | GT).
#[allow(dead_code)]
fn int_cc_to_float_cc(cc: IntCC) -> FloatCC {
    match cc {
        IntCC::SignedLessThan => FloatCC::LessThan,
        IntCC::SignedLessThanOrEqual => FloatCC::LessThanOrEqual,
        IntCC::SignedGreaterThan => FloatCC::GreaterThan,
        IntCC::SignedGreaterThanOrEqual => FloatCC::GreaterThanOrEqual,
        IntCC::Equal => FloatCC::Equal,
        IntCC::NotEqual => FloatCC::NotEqual,
        _ => FloatCC::Equal, // fallback
    }
}

/// Call a runtime helper and return the first result value.
fn call_helper(
    builder: &mut FunctionBuilder<'_>,
    helpers: &HelperRefs,
    kind: HelperKind,
    pc: usize,
    opcode_name: &str,
    args: &[cranelift_codegen::ir::Value],
) -> Result<cranelift_codegen::ir::Value, JitError> {
    let func_ref = helpers.require(kind, pc, opcode_name)?;
    let inst = builder.ins().call(func_ref, args);
    Ok(builder.inst_results(inst)[0])
}

/// Call a runtime helper, ignoring its return value.
fn call_helper_void(
    builder: &mut FunctionBuilder<'_>,
    helpers: &HelperRefs,
    kind: HelperKind,
    pc: usize,
    opcode_name: &str,
    args: &[cranelift_codegen::ir::Value],
) -> Result<(), JitError> {
    let func_ref = helpers.require(kind, pc, opcode_name)?;
    builder.ins().call(func_ref, args);
    Ok(())
}

/// Fill in the slow-path block of a guarded operation with a binary runtime helper call.
///
/// If the helper is not registered, emits a trap (guard failure = abort).
/// This allows pure-int32 code to compile without generic helpers.
#[allow(clippy::too_many_arguments)]
fn fill_slow_binary(
    builder: &mut FunctionBuilder<'_>,
    helpers: &HelperRefs,
    guard: &GuardedResult,
    kind: HelperKind,
    _pc: usize,
    _opcode_name: &str,
    ctx_slot: cranelift_codegen::ir::StackSlot,
    lhs: cranelift_codegen::ir::Value,
    rhs: cranelift_codegen::ir::Value,
) -> Result<(), JitError> {
    builder.switch_to_block(guard.slow_block);
    if let Some(func_ref) = helpers.get(kind) {
        let ctx = builder.ins().stack_load(types::I64, ctx_slot, 0);
        let inst = builder.ins().call(func_ref, &[ctx, lhs, rhs]);
        let result = builder.inst_results(inst)[0];
        builder.ins().jump(guard.merge_block, &[BlockArg::Value(result)]);
    } else {
        // No helper available — bail out to interpreter by returning sentinel
        let sentinel = builder.ins().iconst(types::I64, BAILOUT_SENTINEL);
        builder.ins().return_(&[sentinel]);
    }
    Ok(())
}

/// Fill in the slow-path block of a guarded operation with a unary runtime helper call.
///
/// If the helper is not registered, emits a trap (guard failure = abort).
#[allow(clippy::too_many_arguments)]
fn fill_slow_unary(
    builder: &mut FunctionBuilder<'_>,
    helpers: &HelperRefs,
    guard: &GuardedResult,
    kind: HelperKind,
    _pc: usize,
    _opcode_name: &str,
    ctx_slot: cranelift_codegen::ir::StackSlot,
    val: cranelift_codegen::ir::Value,
) -> Result<(), JitError> {
    builder.switch_to_block(guard.slow_block);
    if let Some(func_ref) = helpers.get(kind) {
        let ctx = builder.ins().stack_load(types::I64, ctx_slot, 0);
        let inst = builder.ins().call(func_ref, &[ctx, val]);
        let result = builder.inst_results(inst)[0];
        builder.ins().jump(guard.merge_block, &[BlockArg::Value(result)]);
    } else {
        // No helper available — bail out to interpreter by returning sentinel
        let sentinel = builder.ins().iconst(types::I64, BAILOUT_SENTINEL);
        builder.ins().return_(&[sentinel]);
    }
    Ok(())
}

/// Emit a guarded arithmetic operation with feedback-driven specialization.
///
/// Chooses between i32 guard, f64 guard, or direct generic call based on the
/// feedback vector's TypeFlags for this instruction.
#[allow(clippy::too_many_arguments)]
fn emit_specialized_arith(
    builder: &mut FunctionBuilder<'_>,
    helpers: &HelperRefs,
    slots: &[cranelift_codegen::ir::StackSlot],
    ctx_slot: cranelift_codegen::ir::StackSlot,
    dst: Register,
    lhs: Register,
    rhs: Register,
    op: ArithOp,
    helper_kind: HelperKind,
    pc: usize,
    opcode_name: &str,
    hint: SpecializationHint,
) -> Result<(), JitError> {
    let l = read_reg(builder, slots, lhs);
    let r = read_reg(builder, slots, rhs);

    match hint {
        SpecializationHint::Int32 => {
            let guard = type_guards::emit_guarded_i32_arith(builder, op, l, r);
            fill_slow_binary(builder, helpers, &guard, helper_kind, pc, opcode_name, ctx_slot, l, r)?;
            builder.switch_to_block(guard.merge_block);
            write_reg(builder, slots, dst, guard.result);
        }
        SpecializationHint::Float64 => {
            let guard = type_guards::emit_guarded_f64_arith(builder, op, l, r);
            fill_slow_binary(builder, helpers, &guard, helper_kind, pc, opcode_name, ctx_slot, l, r)?;
            builder.switch_to_block(guard.merge_block);
            write_reg(builder, slots, dst, guard.result);
        }
        SpecializationHint::Numeric => {
            // Cascading: try i32 first, then f64, then generic
            let guard = type_guards::emit_guarded_i32_arith(builder, op, l, r);
            // Fill slow block with f64 check before generic fallback
            builder.switch_to_block(guard.slow_block);
            let f64_guard = type_guards::emit_guarded_f64_arith(builder, op, l, r);
            fill_slow_binary(builder, helpers, &f64_guard, helper_kind, pc, opcode_name, ctx_slot, l, r)?;
            builder.switch_to_block(f64_guard.merge_block);
            builder.ins().jump(guard.merge_block, &[BlockArg::Value(f64_guard.result)]);
            builder.switch_to_block(guard.merge_block);
            write_reg(builder, slots, dst, guard.result);
        }
        SpecializationHint::Generic => {
            // No guard — call generic helper directly
            let ctx = builder.ins().stack_load(types::I64, ctx_slot, 0);
            let result = call_helper(
                builder, helpers, helper_kind, pc, opcode_name, &[ctx, l, r],
            )?;
            write_reg(builder, slots, dst, result);
        }
    }
    Ok(())
}

/// Emit a guarded division with feedback-driven specialization.
#[allow(clippy::too_many_arguments)]
fn emit_specialized_div(
    builder: &mut FunctionBuilder<'_>,
    helpers: &HelperRefs,
    slots: &[cranelift_codegen::ir::StackSlot],
    ctx_slot: cranelift_codegen::ir::StackSlot,
    dst: Register,
    lhs: Register,
    rhs: Register,
    pc: usize,
    hint: SpecializationHint,
) -> Result<(), JitError> {
    let l = read_reg(builder, slots, lhs);
    let r = read_reg(builder, slots, rhs);

    match hint {
        SpecializationHint::Int32 => {
            let guard = type_guards::emit_guarded_i32_div(builder, l, r);
            fill_slow_binary(builder, helpers, &guard, HelperKind::GenericDiv, pc, "Div", ctx_slot, l, r)?;
            builder.switch_to_block(guard.merge_block);
            write_reg(builder, slots, dst, guard.result);
        }
        SpecializationHint::Float64 => {
            let guard = type_guards::emit_guarded_f64_div(builder, l, r);
            fill_slow_binary(builder, helpers, &guard, HelperKind::GenericDiv, pc, "Div", ctx_slot, l, r)?;
            builder.switch_to_block(guard.merge_block);
            write_reg(builder, slots, dst, guard.result);
        }
        SpecializationHint::Numeric => {
            // Cascading: try i32 exact-division, then f64, then generic
            let guard = type_guards::emit_guarded_i32_div(builder, l, r);
            builder.switch_to_block(guard.slow_block);
            let f64_guard = type_guards::emit_guarded_f64_div(builder, l, r);
            fill_slow_binary(builder, helpers, &f64_guard, HelperKind::GenericDiv, pc, "Div", ctx_slot, l, r)?;
            builder.switch_to_block(f64_guard.merge_block);
            builder.ins().jump(guard.merge_block, &[BlockArg::Value(f64_guard.result)]);
            builder.switch_to_block(guard.merge_block);
            write_reg(builder, slots, dst, guard.result);
        }
        SpecializationHint::Generic => {
            let ctx = builder.ins().stack_load(types::I64, ctx_slot, 0);
            let result = call_helper(
                builder, helpers, HelperKind::GenericDiv, pc, "Div", &[ctx, l, r],
            )?;
            write_reg(builder, slots, dst, result);
        }
    }
    Ok(())
}

/// Emit a guarded comparison with feedback-driven specialization.
#[allow(dead_code, clippy::too_many_arguments)]
fn emit_specialized_cmp(
    builder: &mut FunctionBuilder<'_>,
    helpers: &HelperRefs,
    slots: &[cranelift_codegen::ir::StackSlot],
    ctx_slot: cranelift_codegen::ir::StackSlot,
    dst: Register,
    lhs: Register,
    rhs: Register,
    int_cc: IntCC,
    helper_kind: HelperKind,
    pc: usize,
    opcode_name: &str,
    hint: SpecializationHint,
) -> Result<(), JitError> {
    let l = read_reg(builder, slots, lhs);
    let r = read_reg(builder, slots, rhs);

    match hint {
        SpecializationHint::Int32 | SpecializationHint::Numeric => {
            // i32 guard (same as before — comparisons are cheap so no cascading needed)
            let guard = type_guards::emit_guarded_i32_cmp(builder, int_cc, l, r);
            fill_slow_binary(builder, helpers, &guard, helper_kind, pc, opcode_name, ctx_slot, l, r)?;
            builder.switch_to_block(guard.merge_block);
            write_reg(builder, slots, dst, guard.result);
        }
        SpecializationHint::Float64 => {
            let float_cc = int_cc_to_float_cc(int_cc);
            let guard = type_guards::emit_guarded_f64_cmp(builder, float_cc, l, r);
            fill_slow_binary(builder, helpers, &guard, helper_kind, pc, opcode_name, ctx_slot, l, r)?;
            builder.switch_to_block(guard.merge_block);
            write_reg(builder, slots, dst, guard.result);
        }
        SpecializationHint::Generic => {
            let ctx = builder.ins().stack_load(types::I64, ctx_slot, 0);
            let result = call_helper(
                builder, helpers, helper_kind, pc, opcode_name, &[ctx, l, r],
            )?;
            write_reg(builder, slots, dst, result);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Main translator
// ---------------------------------------------------------------------------

/// Translate a bytecode function into Cranelift IR.
///
/// The function signature is `(ctx: i64) -> i64`. The `ctx` value is passed
/// through to runtime helper calls. All values are NaN-boxed i64.
pub(crate) fn translate_function(
    builder: &mut FunctionBuilder<'_>,
    function: &Function,
    helpers: &HelperRefs,
) -> Result<(), JitError> {
    let instruction_count = function.instructions.len();
    if instruction_count == 0 {
        let entry = builder.create_block();
        builder.append_block_params_for_function_params(entry);
        builder.switch_to_block(entry);
        let v = builder.ins().iconst(types::I64, TAG_UNDEFINED);
        builder.ins().return_(&[v]);
        builder.seal_all_blocks();
        return Ok(());
    }

    // --- Register and local variable stack slots ---
    let reg_count = function.register_count as usize;
    let mut slots = Vec::with_capacity(reg_count);
    for _ in 0..reg_count {
        slots.push(builder.create_sized_stack_slot(StackSlotData::new(
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

    // --- Pre-allocate argument buffer slots for Call instructions ---
    let mut call_arg_slots: Vec<Option<cranelift_codegen::ir::StackSlot>> =
        vec![None; instruction_count];
    for (pc, instr) in function.instructions.iter().enumerate() {
        if let Instruction::Call { argc, .. } = instr
            && *argc > 0
        {
            call_arg_slots[pc] = Some(builder.create_sized_stack_slot(StackSlotData::new(
                StackSlotKind::ExplicitSlot,
                (*argc as u32) * 8,
                8,
            )));
        }
    }

    // --- Blocks ---
    let mut blocks = Vec::with_capacity(instruction_count);
    for _ in 0..instruction_count {
        blocks.push(builder.create_block());
    }
    let entry = builder.create_block();
    let exit = builder.create_block();

    // --- Entry: extract ctx parameter, init registers and locals to undefined ---
    builder.append_block_params_for_function_params(entry);
    builder.switch_to_block(entry);
    let ctx_val = builder.block_params(entry)[0];

    let ctx_slot = builder.create_sized_stack_slot(StackSlotData::new(
        StackSlotKind::ExplicitSlot,
        8,
        8,
    ));
    builder.ins().stack_store(ctx_val, ctx_slot, 0);

    let undef_val = builder.ins().iconst(types::I64, TAG_UNDEFINED);
    for &slot in &slots {
        builder.ins().stack_store(undef_val, slot, 0);
    }
    for &slot in &local_slots {
        builder.ins().stack_store(undef_val, slot, 0);
    }
    builder.ins().jump(blocks[0], &[]);

    // --- Translate each instruction ---
    for (pc, instruction) in function.instructions.iter().enumerate() {
        builder.switch_to_block(blocks[pc]);

        match instruction {
            // ===================== Constants =====================
            Instruction::LoadUndefined { dst } => {
                let v = builder.ins().iconst(types::I64, TAG_UNDEFINED);
                write_reg(builder, &slots, *dst, v);
            }
            Instruction::LoadNull { dst } => {
                let v = builder.ins().iconst(types::I64, TAG_NULL);
                write_reg(builder, &slots, *dst, v);
            }
            Instruction::LoadTrue { dst } => {
                let v = builder.ins().iconst(types::I64, TAG_TRUE);
                write_reg(builder, &slots, *dst, v);
            }
            Instruction::LoadFalse { dst } => {
                let v = builder.ins().iconst(types::I64, TAG_FALSE);
                write_reg(builder, &slots, *dst, v);
            }
            Instruction::LoadInt8 { dst, value } => {
                let v = type_guards::emit_box_int32_const(builder, i32::from(*value));
                write_reg(builder, &slots, *dst, v);
            }
            Instruction::LoadInt32 { dst, value } => {
                let v = type_guards::emit_box_int32_const(builder, *value);
                write_reg(builder, &slots, *dst, v);
            }
            Instruction::LoadConst { dst, idx } => {
                let c = builder.ins().stack_load(types::I64, ctx_slot, 0);
                let idx_v = builder.ins().iconst(types::I64, i64::from(idx.0));
                let result = call_helper(
                    builder,
                    helpers,
                    HelperKind::LoadConst,
                    pc,
                    "LoadConst",
                    &[c, idx_v],
                )?;
                write_reg(builder, &slots, *dst, result);
            }

            // ===================== Registers =====================
            Instruction::Move { dst, src } | Instruction::Dup { dst, src } => {
                let v = read_reg(builder, &slots, *src);
                write_reg(builder, &slots, *dst, v);
            }

            // ===================== Arithmetic (feedback-driven specialization) =====================
            Instruction::Add { dst, lhs, rhs, feedback_index }
            | Instruction::AddI32 { dst, lhs, rhs, feedback_index }
            | Instruction::AddF64 { dst, lhs, rhs, feedback_index } => {
                let hint = specialization_for(function, *feedback_index);
                emit_specialized_arith(
                    builder, helpers, &slots, ctx_slot, *dst, *lhs, *rhs,
                    ArithOp::Add, HelperKind::GenericAdd, pc, "Add", hint,
                )?;
            }
            Instruction::Sub { dst, lhs, rhs, feedback_index }
            | Instruction::SubI32 { dst, lhs, rhs, feedback_index }
            | Instruction::SubF64 { dst, lhs, rhs, feedback_index } => {
                let hint = specialization_for(function, *feedback_index);
                emit_specialized_arith(
                    builder, helpers, &slots, ctx_slot, *dst, *lhs, *rhs,
                    ArithOp::Sub, HelperKind::GenericSub, pc, "Sub", hint,
                )?;
            }
            Instruction::Mul { dst, lhs, rhs, feedback_index }
            | Instruction::MulI32 { dst, lhs, rhs, feedback_index }
            | Instruction::MulF64 { dst, lhs, rhs, feedback_index } => {
                let hint = specialization_for(function, *feedback_index);
                emit_specialized_arith(
                    builder, helpers, &slots, ctx_slot, *dst, *lhs, *rhs,
                    ArithOp::Mul, HelperKind::GenericMul, pc, "Mul", hint,
                )?;
            }
            Instruction::Div { dst, lhs, rhs, feedback_index }
            | Instruction::DivI32 { dst, lhs, rhs, feedback_index }
            | Instruction::DivF64 { dst, lhs, rhs, feedback_index } => {
                let hint = specialization_for(function, *feedback_index);
                emit_specialized_div(
                    builder, helpers, &slots, ctx_slot, *dst, *lhs, *rhs, pc, hint,
                )?;
            }
            Instruction::Mod { dst, lhs, rhs } => {
                let l = read_reg(builder, &slots, *lhs);
                let r = read_reg(builder, &slots, *rhs);
                let guard = type_guards::emit_guarded_i32_mod(builder, l, r);
                fill_slow_binary(
                    builder, helpers, &guard, HelperKind::GenericMod, pc, "Mod", ctx_slot, l, r,
                )?;
                builder.switch_to_block(guard.merge_block);
                write_reg(builder, &slots, *dst, guard.result);
            }
            Instruction::Neg { dst, src } => {
                let v = read_reg(builder, &slots, *src);
                let guard = type_guards::emit_guarded_i32_neg(builder, v);
                fill_slow_unary(
                    builder, helpers, &guard, HelperKind::GenericNeg, pc, "Neg", ctx_slot, v,
                )?;
                builder.switch_to_block(guard.merge_block);
                write_reg(builder, &slots, *dst, guard.result);
            }
            Instruction::Inc { dst, src } => {
                let v = read_reg(builder, &slots, *src);
                let guard = type_guards::emit_guarded_i32_inc(builder, v);
                fill_slow_unary(
                    builder, helpers, &guard, HelperKind::GenericInc, pc, "Inc", ctx_slot, v,
                )?;
                builder.switch_to_block(guard.merge_block);
                write_reg(builder, &slots, *dst, guard.result);
            }
            Instruction::Dec { dst, src } => {
                let v = read_reg(builder, &slots, *src);
                let guard = type_guards::emit_guarded_i32_dec(builder, v);
                fill_slow_unary(
                    builder, helpers, &guard, HelperKind::GenericDec, pc, "Dec", ctx_slot, v,
                )?;
                builder.switch_to_block(guard.merge_block);
                write_reg(builder, &slots, *dst, guard.result);
            }

            // ===================== Bitwise (guarded i32) =====================
            Instruction::BitAnd { dst, lhs, rhs } => {
                let (l, r) = (read_reg(builder, &slots, *lhs), read_reg(builder, &slots, *rhs));
                let guard = type_guards::emit_guarded_i32_bitwise(builder, BitwiseOp::And, l, r);
                fill_slow_binary(
                    builder, helpers, &guard, HelperKind::GenericBitOp, pc, "BitAnd", ctx_slot, l, r,
                )?;
                builder.switch_to_block(guard.merge_block);
                write_reg(builder, &slots, *dst, guard.result);
            }
            Instruction::BitOr { dst, lhs, rhs } => {
                let (l, r) = (read_reg(builder, &slots, *lhs), read_reg(builder, &slots, *rhs));
                let guard = type_guards::emit_guarded_i32_bitwise(builder, BitwiseOp::Or, l, r);
                fill_slow_binary(
                    builder, helpers, &guard, HelperKind::GenericBitOp, pc, "BitOr", ctx_slot, l, r,
                )?;
                builder.switch_to_block(guard.merge_block);
                write_reg(builder, &slots, *dst, guard.result);
            }
            Instruction::BitXor { dst, lhs, rhs } => {
                let (l, r) = (read_reg(builder, &slots, *lhs), read_reg(builder, &slots, *rhs));
                let guard = type_guards::emit_guarded_i32_bitwise(builder, BitwiseOp::Xor, l, r);
                fill_slow_binary(
                    builder, helpers, &guard, HelperKind::GenericBitOp, pc, "BitXor", ctx_slot, l, r,
                )?;
                builder.switch_to_block(guard.merge_block);
                write_reg(builder, &slots, *dst, guard.result);
            }
            Instruction::BitNot { dst, src } => {
                let v = read_reg(builder, &slots, *src);
                let guard = type_guards::emit_guarded_i32_bitnot(builder, v);
                fill_slow_unary(
                    builder, helpers, &guard, HelperKind::GenericBitNot, pc, "BitNot", ctx_slot, v,
                )?;
                builder.switch_to_block(guard.merge_block);
                write_reg(builder, &slots, *dst, guard.result);
            }
            Instruction::Shl { dst, lhs, rhs } => {
                let (l, r) = (read_reg(builder, &slots, *lhs), read_reg(builder, &slots, *rhs));
                let guard = type_guards::emit_guarded_i32_bitwise(builder, BitwiseOp::Shl, l, r);
                fill_slow_binary(
                    builder, helpers, &guard, HelperKind::GenericBitOp, pc, "Shl", ctx_slot, l, r,
                )?;
                builder.switch_to_block(guard.merge_block);
                write_reg(builder, &slots, *dst, guard.result);
            }
            Instruction::Shr { dst, lhs, rhs } => {
                let (l, r) = (read_reg(builder, &slots, *lhs), read_reg(builder, &slots, *rhs));
                let guard = type_guards::emit_guarded_i32_bitwise(builder, BitwiseOp::Shr, l, r);
                fill_slow_binary(
                    builder, helpers, &guard, HelperKind::GenericBitOp, pc, "Shr", ctx_slot, l, r,
                )?;
                builder.switch_to_block(guard.merge_block);
                write_reg(builder, &slots, *dst, guard.result);
            }
            Instruction::Ushr { dst, lhs, rhs } => {
                let (l, r) = (read_reg(builder, &slots, *lhs), read_reg(builder, &slots, *rhs));
                let guard = type_guards::emit_guarded_i32_bitwise(builder, BitwiseOp::Ushr, l, r);
                fill_slow_binary(
                    builder, helpers, &guard, HelperKind::GenericBitOp, pc, "Ushr", ctx_slot, l, r,
                )?;
                builder.switch_to_block(guard.merge_block);
                write_reg(builder, &slots, *dst, guard.result);
            }

            // ===================== Comparison (guarded i32) =====================
            Instruction::Lt { dst, lhs, rhs } => {
                let (l, r) = (read_reg(builder, &slots, *lhs), read_reg(builder, &slots, *rhs));
                let guard =
                    type_guards::emit_guarded_i32_cmp(builder, IntCC::SignedLessThan, l, r);
                fill_slow_binary(
                    builder, helpers, &guard, HelperKind::GenericLt, pc, "Lt", ctx_slot, l, r,
                )?;
                builder.switch_to_block(guard.merge_block);
                write_reg(builder, &slots, *dst, guard.result);
            }
            Instruction::Le { dst, lhs, rhs } => {
                let (l, r) = (read_reg(builder, &slots, *lhs), read_reg(builder, &slots, *rhs));
                let guard =
                    type_guards::emit_guarded_i32_cmp(builder, IntCC::SignedLessThanOrEqual, l, r);
                fill_slow_binary(
                    builder, helpers, &guard, HelperKind::GenericLe, pc, "Le", ctx_slot, l, r,
                )?;
                builder.switch_to_block(guard.merge_block);
                write_reg(builder, &slots, *dst, guard.result);
            }
            Instruction::Gt { dst, lhs, rhs } => {
                let (l, r) = (read_reg(builder, &slots, *lhs), read_reg(builder, &slots, *rhs));
                let guard =
                    type_guards::emit_guarded_i32_cmp(builder, IntCC::SignedGreaterThan, l, r);
                fill_slow_binary(
                    builder, helpers, &guard, HelperKind::GenericGt, pc, "Gt", ctx_slot, l, r,
                )?;
                builder.switch_to_block(guard.merge_block);
                write_reg(builder, &slots, *dst, guard.result);
            }
            Instruction::Ge { dst, lhs, rhs } => {
                let (l, r) = (read_reg(builder, &slots, *lhs), read_reg(builder, &slots, *rhs));
                let guard = type_guards::emit_guarded_i32_cmp(
                    builder,
                    IntCC::SignedGreaterThanOrEqual,
                    l,
                    r,
                );
                fill_slow_binary(
                    builder, helpers, &guard, HelperKind::GenericGe, pc, "Ge", ctx_slot, l, r,
                )?;
                builder.switch_to_block(guard.merge_block);
                write_reg(builder, &slots, *dst, guard.result);
            }

            // Strict equality: raw bit comparison (correct for all NaN-boxed types)
            Instruction::StrictEq { dst, lhs, rhs } => {
                let (l, r) = (read_reg(builder, &slots, *lhs), read_reg(builder, &slots, *rhs));
                let result = type_guards::emit_strict_eq(builder, l, r, false);
                write_reg(builder, &slots, *dst, result);
            }
            Instruction::StrictNe { dst, lhs, rhs } => {
                let (l, r) = (read_reg(builder, &slots, *lhs), read_reg(builder, &slots, *rhs));
                let result = type_guards::emit_strict_eq(builder, l, r, true);
                write_reg(builder, &slots, *dst, result);
            }

            // Abstract equality: guarded i32 + generic fallback
            Instruction::Eq { dst, lhs, rhs } => {
                let (l, r) = (read_reg(builder, &slots, *lhs), read_reg(builder, &slots, *rhs));
                let guard = type_guards::emit_guarded_i32_cmp(builder, IntCC::Equal, l, r);
                fill_slow_binary(
                    builder, helpers, &guard, HelperKind::GenericEq, pc, "Eq", ctx_slot, l, r,
                )?;
                builder.switch_to_block(guard.merge_block);
                write_reg(builder, &slots, *dst, guard.result);
            }
            Instruction::Ne { dst, lhs, rhs } => {
                let (l, r) = (read_reg(builder, &slots, *lhs), read_reg(builder, &slots, *rhs));
                let guard = type_guards::emit_guarded_i32_cmp(builder, IntCC::NotEqual, l, r);
                fill_slow_binary(
                    builder, helpers, &guard, HelperKind::GenericNeq, pc, "Ne", ctx_slot, l, r,
                )?;
                builder.switch_to_block(guard.merge_block);
                write_reg(builder, &slots, *dst, guard.result);
            }

            // ===================== Logical =====================
            Instruction::Not { dst, src } => {
                // JS logical NOT: !truthy → TAG_TRUE, !falsy → TAG_FALSE
                let v = read_reg(builder, &slots, *src);
                let is_truthy = type_guards::emit_is_truthy(builder, v);
                // NOT: if truthy → false, if falsy → true
                // emit_bool_to_nanbox(cond) → cond=1→TRUE, cond=0→FALSE
                // We want: truthy→FALSE, falsy→TRUE → invert
                let zero_i8 = builder.ins().iconst(types::I8, 0);
                let is_falsy = builder.ins().icmp(IntCC::Equal, is_truthy, zero_i8);
                let result = type_guards::emit_bool_to_nanbox(builder, is_falsy);
                write_reg(builder, &slots, *dst, result);
            }

            // ===================== Variables =====================
            Instruction::GetLocal { dst, idx } => {
                let i = idx.0 as usize;
                let v = if i < local_slots.len() {
                    builder.ins().stack_load(types::I64, local_slots[i], 0)
                } else {
                    builder.ins().iconst(types::I64, TAG_UNDEFINED)
                };
                write_reg(builder, &slots, *dst, v);
            }
            Instruction::SetLocal { idx, src } => {
                if (idx.0 as usize) < local_slots.len() {
                    let v = read_reg(builder, &slots, *src);
                    builder
                        .ins()
                        .stack_store(v, local_slots[idx.0 as usize], 0);
                }
            }
            Instruction::GetUpvalue { dst, idx } => {
                let c = builder.ins().stack_load(types::I64, ctx_slot, 0);
                let idx_v = builder.ins().iconst(types::I64, i64::from(idx.0));
                let result = call_helper(
                    builder,
                    helpers,
                    HelperKind::GetUpvalue,
                    pc,
                    "GetUpvalue",
                    &[c, idx_v],
                )?;
                write_reg(builder, &slots, *dst, result);
            }
            Instruction::SetUpvalue { idx, src } => {
                let c = builder.ins().stack_load(types::I64, ctx_slot, 0);
                let idx_v = builder.ins().iconst(types::I64, i64::from(idx.0));
                let val = read_reg(builder, &slots, *src);
                call_helper_void(
                    builder,
                    helpers,
                    HelperKind::SetUpvalue,
                    pc,
                    "SetUpvalue",
                    &[c, idx_v, val],
                )?;
            }
            Instruction::GetGlobal { dst, name, ic_index } => {
                let c = builder.ins().stack_load(types::I64, ctx_slot, 0);
                let name_v = builder.ins().iconst(types::I64, i64::from(name.0));
                let ic_v = builder.ins().iconst(types::I64, i64::from(*ic_index));
                let result = call_helper(
                    builder,
                    helpers,
                    HelperKind::GetGlobal,
                    pc,
                    "GetGlobal",
                    &[c, name_v, ic_v],
                )?;
                write_reg(builder, &slots, *dst, result);
            }
            Instruction::SetGlobal {
                name,
                src,
                ic_index,
                is_declaration,
            } => {
                let c = builder.ins().stack_load(types::I64, ctx_slot, 0);
                let name_v = builder.ins().iconst(types::I64, i64::from(name.0));
                let val = read_reg(builder, &slots, *src);
                let ic_v = builder.ins().iconst(types::I64, i64::from(*ic_index));
                let decl_v = builder
                    .ins()
                    .iconst(types::I64, if *is_declaration { 1 } else { 0 });
                call_helper_void(
                    builder,
                    helpers,
                    HelperKind::SetGlobal,
                    pc,
                    "SetGlobal",
                    &[c, name_v, val, ic_v, decl_v],
                )?;
            }
            Instruction::LoadThis { dst } => {
                let c = builder.ins().stack_load(types::I64, ctx_slot, 0);
                let result =
                    call_helper(builder, helpers, HelperKind::LoadThis, pc, "LoadThis", &[c])?;
                write_reg(builder, &slots, *dst, result);
            }

            // ===================== Objects =====================
            Instruction::GetPropConst {
                dst,
                obj,
                name,
                ic_index,
            } => {
                let c = builder.ins().stack_load(types::I64, ctx_slot, 0);
                let obj_v = read_reg(builder, &slots, *obj);
                let name_v = builder.ins().iconst(types::I64, i64::from(name.0));
                let ic_v = builder.ins().iconst(types::I64, i64::from(*ic_index));
                let result = call_helper(
                    builder,
                    helpers,
                    HelperKind::GetPropConst,
                    pc,
                    "GetPropConst",
                    &[c, obj_v, name_v, ic_v],
                )?;
                write_reg(builder, &slots, *dst, result);
            }
            Instruction::SetPropConst {
                obj,
                name,
                val,
                ic_index,
            } => {
                let c = builder.ins().stack_load(types::I64, ctx_slot, 0);
                let obj_v = read_reg(builder, &slots, *obj);
                let name_v = builder.ins().iconst(types::I64, i64::from(name.0));
                let val_v = read_reg(builder, &slots, *val);
                let ic_v = builder.ins().iconst(types::I64, i64::from(*ic_index));
                call_helper_void(
                    builder,
                    helpers,
                    HelperKind::SetPropConst,
                    pc,
                    "SetPropConst",
                    &[c, obj_v, name_v, val_v, ic_v],
                )?;
            }
            Instruction::GetProp {
                dst,
                obj,
                key,
                ic_index,
            } => {
                let c = builder.ins().stack_load(types::I64, ctx_slot, 0);
                let obj_v = read_reg(builder, &slots, *obj);
                let key_v = read_reg(builder, &slots, *key);
                let ic_v = builder.ins().iconst(types::I64, i64::from(*ic_index));
                let result = call_helper(
                    builder,
                    helpers,
                    HelperKind::GetProp,
                    pc,
                    "GetProp",
                    &[c, obj_v, key_v, ic_v],
                )?;
                write_reg(builder, &slots, *dst, result);
            }
            Instruction::SetProp {
                obj,
                key,
                val,
                ic_index,
            } => {
                let c = builder.ins().stack_load(types::I64, ctx_slot, 0);
                let obj_v = read_reg(builder, &slots, *obj);
                let key_v = read_reg(builder, &slots, *key);
                let val_v = read_reg(builder, &slots, *val);
                let ic_v = builder.ins().iconst(types::I64, i64::from(*ic_index));
                call_helper_void(
                    builder,
                    helpers,
                    HelperKind::SetProp,
                    pc,
                    "SetProp",
                    &[c, obj_v, key_v, val_v, ic_v],
                )?;
            }
            Instruction::NewObject { dst } => {
                let c = builder.ins().stack_load(types::I64, ctx_slot, 0);
                let result = call_helper(
                    builder,
                    helpers,
                    HelperKind::NewObject,
                    pc,
                    "NewObject",
                    &[c],
                )?;
                write_reg(builder, &slots, *dst, result);
            }
            Instruction::DefineProperty { obj, key, val } => {
                let c = builder.ins().stack_load(types::I64, ctx_slot, 0);
                let obj_v = read_reg(builder, &slots, *obj);
                let key_v = read_reg(builder, &slots, *key);
                let val_v = read_reg(builder, &slots, *val);
                call_helper_void(
                    builder,
                    helpers,
                    HelperKind::DefineProperty,
                    pc,
                    "DefineProperty",
                    &[c, obj_v, key_v, val_v],
                )?;
            }
            Instruction::DeleteProp { dst, obj, key } => {
                let c = builder.ins().stack_load(types::I64, ctx_slot, 0);
                let obj_v = read_reg(builder, &slots, *obj);
                let key_v = read_reg(builder, &slots, *key);
                let result = call_helper(
                    builder,
                    helpers,
                    HelperKind::DeleteProp,
                    pc,
                    "DeleteProp",
                    &[c, obj_v, key_v],
                )?;
                write_reg(builder, &slots, *dst, result);
            }

            // ===================== Arrays =====================
            Instruction::NewArray { dst, len } => {
                let c = builder.ins().stack_load(types::I64, ctx_slot, 0);
                let len_v = builder.ins().iconst(types::I64, i64::from(*len));
                let result = call_helper(
                    builder,
                    helpers,
                    HelperKind::NewArray,
                    pc,
                    "NewArray",
                    &[c, len_v],
                )?;
                write_reg(builder, &slots, *dst, result);
            }
            Instruction::GetElem {
                dst,
                arr,
                idx,
                ic_index,
            } => {
                let c = builder.ins().stack_load(types::I64, ctx_slot, 0);
                let arr_v = read_reg(builder, &slots, *arr);
                let idx_v = read_reg(builder, &slots, *idx);
                let ic_v = builder.ins().iconst(types::I64, i64::from(*ic_index));
                let result = call_helper(
                    builder,
                    helpers,
                    HelperKind::GetElem,
                    pc,
                    "GetElem",
                    &[c, arr_v, idx_v, ic_v],
                )?;
                write_reg(builder, &slots, *dst, result);
            }
            Instruction::SetElem {
                arr,
                idx,
                val,
                ic_index,
            } => {
                let c = builder.ins().stack_load(types::I64, ctx_slot, 0);
                let arr_v = read_reg(builder, &slots, *arr);
                let idx_v = read_reg(builder, &slots, *idx);
                let val_v = read_reg(builder, &slots, *val);
                let ic_v = builder.ins().iconst(types::I64, i64::from(*ic_index));
                call_helper_void(
                    builder,
                    helpers,
                    HelperKind::SetElem,
                    pc,
                    "SetElem",
                    &[c, arr_v, idx_v, val_v, ic_v],
                )?;
            }

            // ===================== Functions =====================
            Instruction::Closure { dst, func } => {
                let c = builder.ins().stack_load(types::I64, ctx_slot, 0);
                let idx_v = builder.ins().iconst(types::I64, i64::from(func.0));
                let result = call_helper(
                    builder,
                    helpers,
                    HelperKind::CreateClosure,
                    pc,
                    "Closure",
                    &[c, idx_v],
                )?;
                write_reg(builder, &slots, *dst, result);
            }
            Instruction::Call { dst, func, argc } => {
                let c = builder.ins().stack_load(types::I64, ctx_slot, 0);
                let callee = read_reg(builder, &slots, *func);
                let argc_v = builder.ins().iconst(types::I64, i64::from(*argc));

                let argv_ptr = if *argc > 0 {
                    let arg_slot = call_arg_slots[pc]
                        .expect("call_arg_slots should be pre-allocated for Call");
                    for i in 0..(*argc as u16) {
                        let arg_reg = Register(func.index() + 1 + i);
                        let arg_v = read_reg(builder, &slots, arg_reg);
                        builder
                            .ins()
                            .stack_store(arg_v, arg_slot, (i as i32) * 8);
                    }
                    builder.ins().stack_addr(types::I64, arg_slot, 0)
                } else {
                    builder.ins().iconst(types::I64, 0)
                };

                let result = call_helper(
                    builder,
                    helpers,
                    HelperKind::CallFunction,
                    pc,
                    "Call",
                    &[c, callee, argc_v, argv_ptr],
                )?;
                write_reg(builder, &slots, *dst, result);
            }

            // ===================== Exception =====================
            Instruction::Throw { src } => {
                let c = builder.ins().stack_load(types::I64, ctx_slot, 0);
                let val = read_reg(builder, &slots, *src);
                call_helper_void(
                    builder,
                    helpers,
                    HelperKind::ThrowValue,
                    pc,
                    "Throw",
                    &[c, val],
                )?;
                builder
                    .ins()
                    .trap(cranelift_codegen::ir::TrapCode::user(0).unwrap());
                continue; // no fallthrough
            }

            // ===================== Control Flow =====================
            Instruction::Jump { offset } => {
                let target = jump_target(pc, offset.offset(), instruction_count)?;
                builder.ins().jump(blocks[target], &[]);
                continue;
            }
            Instruction::JumpIfTrue { cond, offset } => {
                let v = read_reg(builder, &slots, *cond);
                let is_truthy = type_guards::emit_is_truthy(builder, v);
                let target = jump_target(pc, offset.offset(), instruction_count)?;
                let fall = fallthrough_or_exit(pc, instruction_count, &blocks, exit);
                builder
                    .ins()
                    .brif(is_truthy, blocks[target], &[], fall, &[]);
                continue;
            }
            Instruction::JumpIfFalse { cond, offset } => {
                let v = read_reg(builder, &slots, *cond);
                let is_truthy = type_guards::emit_is_truthy(builder, v);
                let target = jump_target(pc, offset.offset(), instruction_count)?;
                let fall = fallthrough_or_exit(pc, instruction_count, &blocks, exit);
                // Jump if NOT truthy
                let zero_i8 = builder.ins().iconst(types::I8, 0);
                let is_falsy = builder.ins().icmp(IntCC::Equal, is_truthy, zero_i8);
                builder
                    .ins()
                    .brif(is_falsy, blocks[target], &[], fall, &[]);
                continue;
            }
            Instruction::JumpIfNullish { src, offset } => {
                let v = read_reg(builder, &slots, *src);
                let is_undef = builder.ins().icmp_imm(IntCC::Equal, v, TAG_UNDEFINED);
                let is_null = builder.ins().icmp_imm(IntCC::Equal, v, TAG_NULL);
                let flag = builder.ins().bor(is_undef, is_null);
                let target = jump_target(pc, offset.offset(), instruction_count)?;
                let fall = fallthrough_or_exit(pc, instruction_count, &blocks, exit);
                builder.ins().brif(flag, blocks[target], &[], fall, &[]);
                continue;
            }
            Instruction::JumpIfNotNullish { src, offset } => {
                let v = read_reg(builder, &slots, *src);
                let is_undef = builder.ins().icmp_imm(IntCC::Equal, v, TAG_UNDEFINED);
                let is_null = builder.ins().icmp_imm(IntCC::Equal, v, TAG_NULL);
                let is_nullish = builder.ins().bor(is_undef, is_null);
                let zero_i8 = builder.ins().iconst(types::I8, 0);
                let flag = builder.ins().icmp(IntCC::Equal, is_nullish, zero_i8);
                let target = jump_target(pc, offset.offset(), instruction_count)?;
                let fall = fallthrough_or_exit(pc, instruction_count, &blocks, exit);
                builder.ins().brif(flag, blocks[target], &[], fall, &[]);
                continue;
            }

            // ===================== Returns =====================
            Instruction::Return { src } => {
                let out = read_reg(builder, &slots, *src);
                builder.ins().return_(&[out]);
                continue;
            }
            Instruction::ReturnUndefined => {
                let v = builder.ins().iconst(types::I64, TAG_UNDEFINED);
                builder.ins().return_(&[v]);
                continue;
            }

            // ===================== Misc =====================
            Instruction::Nop | Instruction::Debugger | Instruction::Pop => {}
            Instruction::CloseUpvalue { .. } => {
                // No-op in JIT: upvalue closing is handled by the runtime
            }

            // ===================== Unsupported =====================
            other => return Err(unsupported(pc, other)),
        }

        // Fall through to next instruction
        let next_pc = pc + 1;
        if next_pc < instruction_count {
            builder.ins().jump(blocks[next_pc], &[]);
        } else {
            let v = builder.ins().iconst(types::I64, TAG_UNDEFINED);
            builder.ins().return_(&[v]);
        }
    }

    // Exit block
    builder.switch_to_block(exit);
    let undef = builder.ins().iconst(types::I64, TAG_UNDEFINED);
    builder.ins().return_(&[undef]);

    builder.seal_all_blocks();
    Ok(())
}
