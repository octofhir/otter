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
use crate::bailout::{BAILOUT_SENTINEL, BailoutReason};
use crate::runtime_helpers::{
    HelperKind, HelperRefs, JIT_CTX_BAILOUT_PC_OFFSET, JIT_CTX_BAILOUT_REASON_OFFSET,
    JIT_CTX_DEOPT_LOCALS_PTR_OFFSET, JIT_CTX_DEOPT_REGS_PTR_OFFSET,
};
use crate::type_guards::{self, ArithOp, BitwiseOp, SpecializationHint};
use otter_vm_bytecode::function::InlineCacheState;

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
                | Instruction::GetPropQuickened { .. }
                | Instruction::SetPropQuickened { .. }
                | Instruction::Call { .. }
                | Instruction::GetLocalProp { .. }
                | Instruction::NewObject { .. }
                | Instruction::NewArray { .. }
                | Instruction::GetGlobal { .. }
                | Instruction::SetGlobal { .. }
                | Instruction::GetUpvalue { .. }
                | Instruction::SetUpvalue { .. }
                | Instruction::LoadThis { .. }
                | Instruction::CloseUpvalue { .. }
                | Instruction::TypeOf { .. }
                | Instruction::TypeOfName { .. }
                | Instruction::Pow { .. }
                | Instruction::GetProp { .. }
                | Instruction::SetProp { .. }
                | Instruction::GetElem { .. }
                | Instruction::SetElem { .. }
                | Instruction::DeleteProp { .. }
                | Instruction::DefineProperty { .. }
                | Instruction::Throw { .. }
                | Instruction::Construct { .. }
                | Instruction::CallMethod { .. }
                | Instruction::CallWithReceiver { .. }
                | Instruction::CallMethodComputed { .. }
                | Instruction::ToNumber { .. }
                | Instruction::ToString { .. }
                | Instruction::RequireCoercible { .. }
                | Instruction::InstanceOf { .. }
                | Instruction::In { .. }
                | Instruction::DeclareGlobalVar { .. }
                | Instruction::Pop
                | Instruction::Dup { .. }
                | Instruction::Debugger
                | Instruction::DefineGetter { .. }
                | Instruction::DefineSetter { .. }
                | Instruction::DefineMethod { .. }
                | Instruction::Spread { .. }
                | Instruction::Closure { .. }
                | Instruction::CreateArguments { .. }
                | Instruction::GetIterator { .. }
                | Instruction::IteratorNext { .. }
                | Instruction::IteratorClose { .. }
                | Instruction::CallSpread { .. }
                | Instruction::ConstructSpread { .. }
                | Instruction::CallMethodComputedSpread { .. }
                | Instruction::TailCall { .. }
                | Instruction::TryStart { .. }
                | Instruction::TryEnd
                | Instruction::Catch { .. }
                | Instruction::DefineClass { .. }
                | Instruction::GetSuper { .. }
                | Instruction::CallSuper { .. }
                | Instruction::GetSuperProp { .. }
                | Instruction::SetHomeObject { .. }
                | Instruction::CallSuperForward { .. }
                | Instruction::CallSuperSpread { .. }
                | Instruction::AsyncClosure { .. }
                | Instruction::GeneratorClosure { .. }
                | Instruction::AsyncGeneratorClosure { .. }
                | Instruction::CallEval { .. }
                | Instruction::Import { .. }
                | Instruction::Export { .. }
                | Instruction::GetAsyncIterator { .. }
                | Instruction::ForInNext { .. }
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
        || (!has_helpers && !function.upvalues.is_empty())
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
            | Instruction::JumpIfNotNullish { offset, .. }
            | Instruction::ForInNext { offset, .. }
            | Instruction::TryStart {
                catch_offset: offset,
            } => {
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

/// Record bailout telemetry AND dump live local/register state to deopt buffers.
///
/// When `local_slots` and `reg_slots` are non-empty, loads each value from its
/// Cranelift stack slot and writes it to the deopt buffer pointed to by the
/// JitContext fields `deopt_locals_ptr` / `deopt_regs_ptr`. This enables precise
/// interpreter resume from the bailout PC instead of restarting from PC 0.
fn emit_record_bailout_with_state(
    builder: &mut FunctionBuilder<'_>,
    ctx_ptr: Value,
    pc: usize,
    reason: BailoutReason,
    local_slots: &[cranelift_codegen::ir::StackSlot],
    reg_slots: &[cranelift_codegen::ir::StackSlot],
) {
    let is_null = {
        let zero = builder.ins().iconst(types::I64, 0);
        builder.ins().icmp(IntCC::Equal, ctx_ptr, zero)
    };
    let skip_block = builder.create_block();
    let write_block = builder.create_block();
    builder
        .ins()
        .brif(is_null, skip_block, &[], write_block, &[]);

    builder.switch_to_block(write_block);
    let reason_val = builder.ins().iconst(types::I64, reason.code());
    let pc_val = builder.ins().iconst(types::I64, pc as i64);
    builder.ins().store(
        MemFlags::trusted(),
        reason_val,
        ctx_ptr,
        JIT_CTX_BAILOUT_REASON_OFFSET,
    );
    builder.ins().store(
        MemFlags::trusted(),
        pc_val,
        ctx_ptr,
        JIT_CTX_BAILOUT_PC_OFFSET,
    );

    // Dump locals to deopt buffer
    if !local_slots.is_empty() {
        let locals_ptr = builder.ins().load(
            types::I64,
            MemFlags::trusted(),
            ctx_ptr,
            JIT_CTX_DEOPT_LOCALS_PTR_OFFSET,
        );
        let locals_null = {
            let zero = builder.ins().iconst(types::I64, 0);
            builder.ins().icmp(IntCC::Equal, locals_ptr, zero)
        };
        let dump_locals_block = builder.create_block();
        let after_locals_block = builder.create_block();
        builder
            .ins()
            .brif(locals_null, after_locals_block, &[], dump_locals_block, &[]);
        builder.switch_to_block(dump_locals_block);
        for (i, &slot) in local_slots.iter().enumerate() {
            let val = builder.ins().stack_load(types::I64, slot, 0);
            builder.ins().store(
                MemFlags::trusted(),
                val,
                locals_ptr,
                (i * 8) as i32,
            );
        }
        builder.ins().jump(after_locals_block, &[]);
        builder.switch_to_block(after_locals_block);
    }

    // Dump registers to deopt buffer
    if !reg_slots.is_empty() {
        let regs_ptr = builder.ins().load(
            types::I64,
            MemFlags::trusted(),
            ctx_ptr,
            JIT_CTX_DEOPT_REGS_PTR_OFFSET,
        );
        let regs_null = {
            let zero = builder.ins().iconst(types::I64, 0);
            builder.ins().icmp(IntCC::Equal, regs_ptr, zero)
        };
        let dump_regs_block = builder.create_block();
        let after_regs_block = builder.create_block();
        builder
            .ins()
            .brif(regs_null, after_regs_block, &[], dump_regs_block, &[]);
        builder.switch_to_block(dump_regs_block);
        for (i, &slot) in reg_slots.iter().enumerate() {
            let val = builder.ins().stack_load(types::I64, slot, 0);
            builder.ins().store(
                MemFlags::trusted(),
                val,
                regs_ptr,
                (i * 8) as i32,
            );
        }
        builder.ins().jump(after_regs_block, &[]);
        builder.switch_to_block(after_regs_block);
    }

    builder.ins().jump(skip_block, &[]);
    builder.switch_to_block(skip_block);
}

fn emit_bailout_return_with_state(
    builder: &mut FunctionBuilder<'_>,
    ctx_ptr: Value,
    pc: usize,
    reason: BailoutReason,
    local_slots: &[cranelift_codegen::ir::StackSlot],
    reg_slots: &[cranelift_codegen::ir::StackSlot],
) {
    emit_record_bailout_with_state(builder, ctx_ptr, pc, reason, local_slots, reg_slots);
    emit_bailout_return(builder);
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

/// Lower a guarded result with generic helper fallback.
///
/// On type guard failure, calls the provided generic helper instead of bailing
/// out the whole function. This keeps JIT code executing even when operand types
/// don't match the speculative fast path (e.g., Int32 overflow → GenericAdd).
///
/// If no generic helper is available, falls back to the standard bailout.
fn lower_guarded_with_generic_fallback(
    builder: &mut FunctionBuilder<'_>,
    guarded: type_guards::GuardedResult,
    generic_ref: Option<cranelift_codegen::ir::FuncRef>,
    generic_args: &[Value],
    ctx_ptr: Value,
    pc: usize,
    reason: BailoutReason,
    local_slots: &[cranelift_codegen::ir::StackSlot],
    reg_slots: &[cranelift_codegen::ir::StackSlot],
) -> Value {
    builder.switch_to_block(guarded.slow_block);
    if let Some(helper_ref) = generic_ref {
        let call = builder.ins().call(helper_ref, generic_args);
        let result = builder.inst_results(call)[0];
        let bail_block = builder.create_block();
        let ok_block = builder.create_block();
        let sentinel = builder.ins().iconst(types::I64, BAILOUT_SENTINEL);
        let is_bailout = builder.ins().icmp(IntCC::Equal, result, sentinel);
        builder
            .ins()
            .brif(is_bailout, bail_block, &[], ok_block, &[]);
        builder.switch_to_block(bail_block);
        emit_bailout_return_with_state(builder, ctx_ptr, pc, reason, local_slots, reg_slots);
        builder.switch_to_block(ok_block);
        builder.ins().jump(guarded.merge_block, &[BlockArg::Value(result)]);
    } else {
        emit_bailout_return_with_state(
            builder,
            ctx_ptr,
            pc,
            BailoutReason::TypeGuardFailure,
            local_slots,
            reg_slots,
        );
    }
    builder.switch_to_block(guarded.merge_block);
    guarded.result
}

/// Emit monomorphic property read with fallback to full GetPropConst.
///
/// 1. Call GetPropMono(obj, shape_id, offset) — lightweight, no JitContext
/// 2. If BAILOUT → call full GetPropConst(ctx, obj, name_idx, ic_idx)
/// 3. If still BAILOUT → bail out function
/// 4. Merge results from either path
fn emit_mono_prop_with_fallback(
    builder: &mut FunctionBuilder<'_>,
    mono_ref: cranelift_codegen::ir::FuncRef,
    full_ref: cranelift_codegen::ir::FuncRef,
    obj_val: Value,
    ctx_ptr: Value,
    shape_id: u64,
    offset: u32,
    name_index: u32,
    ic_index: u16,
) -> Value {
    let merge_block = builder.create_block();
    builder.append_block_param(merge_block, types::I64);

    // Fast path: monomorphic helper
    let shape_const = builder.ins().iconst(types::I64, shape_id as i64);
    let offset_const = builder.ins().iconst(types::I64, offset as i64);
    let mono_call = builder.ins().call(mono_ref, &[obj_val, shape_const, offset_const]);
    let mono_result = builder.inst_results(mono_call)[0];

    let sentinel = builder.ins().iconst(types::I64, BAILOUT_SENTINEL);
    let mono_bail = builder.ins().icmp(IntCC::Equal, mono_result, sentinel);
    let slow_block = builder.create_block();
    let mono_ok = builder.create_block();
    builder.ins().brif(mono_bail, slow_block, &[], mono_ok, &[]);

    // Mono hit → merge
    builder.switch_to_block(mono_ok);
    builder.ins().jump(merge_block, &[BlockArg::Value(mono_result)]);

    // Slow path: full GetPropConst
    builder.switch_to_block(slow_block);
    let name_idx_val = builder.ins().iconst(types::I64, name_index as i64);
    let ic_idx_val = builder.ins().iconst(types::I64, ic_index as i64);
    let full_call = builder.ins().call(full_ref, &[ctx_ptr, obj_val, name_idx_val, ic_idx_val]);
    let full_result = builder.inst_results(full_call)[0];

    let full_bail = builder.ins().icmp(IntCC::Equal, full_result, sentinel);
    let bail_block = builder.create_block();
    let full_ok = builder.create_block();
    builder.ins().brif(full_bail, bail_block, &[], full_ok, &[]);

    builder.switch_to_block(bail_block);
    emit_bailout_return(builder);

    builder.switch_to_block(full_ok);
    builder.ins().jump(merge_block, &[BlockArg::Value(full_result)]);

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
    let (feedback_snapshot, ic_snapshot): (Vec<_>, Vec<_>) = {
        let fv = function.feedback_vector.read();
        (
            fv.iter().map(|m| m.type_observations).collect(),
            fv.iter().map(|m| m.ic_state).collect(),
        )
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
        let emit_bailout_return = |builder: &mut FunctionBuilder<'_>| {
            emit_bailout_return_with_state(
                builder,
                ctx_ptr,
                pc,
                BailoutReason::HelperReturnedSentinel,
                &local_slots,
                &reg_slots,
            );
        };
        let emit_helper_call_with_bailout = |builder: &mut FunctionBuilder<'_>,
                                             helper_ref: cranelift_codegen::ir::FuncRef,
                                             args: &[Value]|
         -> Value {
            let call = builder.ins().call(helper_ref, args);
            let result = builder.inst_results(call)[0];
            let bail_block = builder.create_block();
            let continue_block = builder.create_block();
            let sentinel = builder.ins().iconst(types::I64, BAILOUT_SENTINEL);
            let is_bailout = builder.ins().icmp(IntCC::Equal, result, sentinel);
            builder
                .ins()
                .brif(is_bailout, bail_block, &[], continue_block, &[]);
            builder.switch_to_block(bail_block);
            emit_bailout_return_with_state(
                builder,
                ctx_ptr,
                pc,
                BailoutReason::HelperReturnedSentinel,
                &local_slots,
                &reg_slots,
            );
            builder.switch_to_block(continue_block);
            result
        };
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
            Instruction::Add {
                dst,
                lhs,
                rhs,
                feedback_index,
            } => {
                let left = read_reg(builder, &reg_slots, *lhs);
                let right = read_reg(builder, &reg_slots, *rhs);
                let hint = get_hint(*feedback_index);
                let guarded =
                    type_guards::emit_specialized_arith(builder, ArithOp::Add, left, right, hint);
                let generic_ref = helpers.and_then(|h| h.get(HelperKind::GenericAdd));
                let out = lower_guarded_with_generic_fallback(
                    builder, guarded, generic_ref, &[ctx_ptr, left, right],
                    ctx_ptr, pc, BailoutReason::HelperReturnedSentinel, &local_slots, &reg_slots,
                );
                write_reg(builder, &reg_slots, *dst, out);
            }
            Instruction::Sub {
                dst,
                lhs,
                rhs,
                feedback_index,
            } => {
                let left = read_reg(builder, &reg_slots, *lhs);
                let right = read_reg(builder, &reg_slots, *rhs);
                let hint = get_hint(*feedback_index);
                let guarded =
                    type_guards::emit_specialized_arith(builder, ArithOp::Sub, left, right, hint);
                let generic_ref = helpers.and_then(|h| h.get(HelperKind::GenericSub));
                let out = lower_guarded_with_generic_fallback(
                    builder, guarded, generic_ref, &[ctx_ptr, left, right],
                    ctx_ptr, pc, BailoutReason::HelperReturnedSentinel, &local_slots, &reg_slots,
                );
                write_reg(builder, &reg_slots, *dst, out);
            }
            Instruction::Mul {
                dst,
                lhs,
                rhs,
                feedback_index,
            } => {
                let left = read_reg(builder, &reg_slots, *lhs);
                let right = read_reg(builder, &reg_slots, *rhs);
                let hint = get_hint(*feedback_index);
                let guarded =
                    type_guards::emit_specialized_arith(builder, ArithOp::Mul, left, right, hint);
                let generic_ref = helpers.and_then(|h| h.get(HelperKind::GenericMul));
                let out = lower_guarded_with_generic_fallback(
                    builder, guarded, generic_ref, &[ctx_ptr, left, right],
                    ctx_ptr, pc, BailoutReason::HelperReturnedSentinel, &local_slots, &reg_slots,
                );
                write_reg(builder, &reg_slots, *dst, out);
            }
            Instruction::Div {
                dst,
                lhs,
                rhs,
                feedback_index,
            } => {
                let left = read_reg(builder, &reg_slots, *lhs);
                let right = read_reg(builder, &reg_slots, *rhs);
                let hint = get_hint(*feedback_index);
                // JS division always returns f64 (even 4/2 → 2.0), so Int32 hint
                // still needs the numeric path for div-by-zero → Infinity handling.
                let guarded = match hint {
                    SpecializationHint::Float64 => {
                        type_guards::emit_guarded_f64_div(builder, left, right)
                    }
                    _ => type_guards::emit_guarded_numeric_div(builder, left, right),
                };
                let generic_ref = helpers.and_then(|h| h.get(HelperKind::GenericDiv));
                let out = lower_guarded_with_generic_fallback(
                    builder, guarded, generic_ref, &[ctx_ptr, left, right],
                    ctx_ptr, pc, BailoutReason::HelperReturnedSentinel, &local_slots, &reg_slots,
                );
                write_reg(builder, &reg_slots, *dst, out);
            }
            // Quickened arithmetic: type already known from interpreter feedback.
            // Still use generic fallback for guard failures (e.g., Int32 overflow).
            Instruction::AddInt32 { dst, lhs, rhs, .. } => {
                let left = read_reg(builder, &reg_slots, *lhs);
                let right = read_reg(builder, &reg_slots, *rhs);
                let guarded =
                    type_guards::emit_guarded_i32_arith(builder, ArithOp::Add, left, right);
                let generic_ref = helpers.and_then(|h| h.get(HelperKind::GenericAdd));
                let out = lower_guarded_with_generic_fallback(
                    builder, guarded, generic_ref, &[ctx_ptr, left, right],
                    ctx_ptr, pc, BailoutReason::HelperReturnedSentinel, &local_slots, &reg_slots,
                );
                write_reg(builder, &reg_slots, *dst, out);
            }
            Instruction::SubInt32 { dst, lhs, rhs, .. } => {
                let left = read_reg(builder, &reg_slots, *lhs);
                let right = read_reg(builder, &reg_slots, *rhs);
                let guarded =
                    type_guards::emit_guarded_i32_arith(builder, ArithOp::Sub, left, right);
                let generic_ref = helpers.and_then(|h| h.get(HelperKind::GenericSub));
                let out = lower_guarded_with_generic_fallback(
                    builder, guarded, generic_ref, &[ctx_ptr, left, right],
                    ctx_ptr, pc, BailoutReason::HelperReturnedSentinel, &local_slots, &reg_slots,
                );
                write_reg(builder, &reg_slots, *dst, out);
            }
            Instruction::MulInt32 { dst, lhs, rhs, .. } => {
                let left = read_reg(builder, &reg_slots, *lhs);
                let right = read_reg(builder, &reg_slots, *rhs);
                let guarded =
                    type_guards::emit_guarded_i32_arith(builder, ArithOp::Mul, left, right);
                let generic_ref = helpers.and_then(|h| h.get(HelperKind::GenericMul));
                let out = lower_guarded_with_generic_fallback(
                    builder, guarded, generic_ref, &[ctx_ptr, left, right],
                    ctx_ptr, pc, BailoutReason::HelperReturnedSentinel, &local_slots, &reg_slots,
                );
                write_reg(builder, &reg_slots, *dst, out);
            }
            Instruction::DivInt32 { dst, lhs, rhs, .. } => {
                let left = read_reg(builder, &reg_slots, *lhs);
                let right = read_reg(builder, &reg_slots, *rhs);
                let guarded = type_guards::emit_guarded_i32_div(builder, left, right);
                let generic_ref = helpers.and_then(|h| h.get(HelperKind::GenericDiv));
                let out = lower_guarded_with_generic_fallback(
                    builder, guarded, generic_ref, &[ctx_ptr, left, right],
                    ctx_ptr, pc, BailoutReason::HelperReturnedSentinel, &local_slots, &reg_slots,
                );
                write_reg(builder, &reg_slots, *dst, out);
            }
            Instruction::AddNumber { dst, lhs, rhs, .. } => {
                let left = read_reg(builder, &reg_slots, *lhs);
                let right = read_reg(builder, &reg_slots, *rhs);
                let guarded =
                    type_guards::emit_guarded_f64_arith(builder, ArithOp::Add, left, right);
                let generic_ref = helpers.and_then(|h| h.get(HelperKind::GenericAdd));
                let out = lower_guarded_with_generic_fallback(
                    builder, guarded, generic_ref, &[ctx_ptr, left, right],
                    ctx_ptr, pc, BailoutReason::HelperReturnedSentinel, &local_slots, &reg_slots,
                );
                write_reg(builder, &reg_slots, *dst, out);
            }
            Instruction::SubNumber { dst, lhs, rhs, .. } => {
                let left = read_reg(builder, &reg_slots, *lhs);
                let right = read_reg(builder, &reg_slots, *rhs);
                let guarded =
                    type_guards::emit_guarded_f64_arith(builder, ArithOp::Sub, left, right);
                let generic_ref = helpers.and_then(|h| h.get(HelperKind::GenericSub));
                let out = lower_guarded_with_generic_fallback(
                    builder, guarded, generic_ref, &[ctx_ptr, left, right],
                    ctx_ptr, pc, BailoutReason::HelperReturnedSentinel, &local_slots, &reg_slots,
                );
                write_reg(builder, &reg_slots, *dst, out);
            }
            Instruction::Mod { dst, lhs, rhs } => {
                let left = read_reg(builder, &reg_slots, *lhs);
                let right = read_reg(builder, &reg_slots, *rhs);
                let guarded = type_guards::emit_guarded_i32_mod(builder, left, right);
                let generic_ref = helpers.and_then(|h| h.get(HelperKind::GenericMod));
                let out = lower_guarded_with_generic_fallback(
                    builder, guarded, generic_ref, &[ctx_ptr, left, right],
                    ctx_ptr, pc, BailoutReason::HelperReturnedSentinel, &local_slots, &reg_slots,
                );
                write_reg(builder, &reg_slots, *dst, out);
            }
            Instruction::Neg { dst, src } => {
                let val = read_reg(builder, &reg_slots, *src);
                let guarded = type_guards::emit_guarded_i32_neg(builder, val);
                let generic_ref = helpers.and_then(|h| h.get(HelperKind::GenericNeg));
                let out = lower_guarded_with_generic_fallback(
                    builder, guarded, generic_ref, &[ctx_ptr, val],
                    ctx_ptr, pc, BailoutReason::HelperReturnedSentinel, &local_slots, &reg_slots,
                );
                write_reg(builder, &reg_slots, *dst, out);
            }
            Instruction::Inc { dst, src } => {
                let val = read_reg(builder, &reg_slots, *src);
                let guarded = type_guards::emit_guarded_i32_inc(builder, val);
                let generic_ref = helpers.and_then(|h| h.get(HelperKind::GenericInc));
                let out = lower_guarded_with_generic_fallback(
                    builder, guarded, generic_ref, &[ctx_ptr, val],
                    ctx_ptr, pc, BailoutReason::HelperReturnedSentinel, &local_slots, &reg_slots,
                );
                write_reg(builder, &reg_slots, *dst, out);
            }
            Instruction::Dec { dst, src } => {
                let val = read_reg(builder, &reg_slots, *src);
                let guarded = type_guards::emit_guarded_i32_dec(builder, val);
                let generic_ref = helpers.and_then(|h| h.get(HelperKind::GenericDec));
                let out = lower_guarded_with_generic_fallback(
                    builder, guarded, generic_ref, &[ctx_ptr, val],
                    ctx_ptr, pc, BailoutReason::HelperReturnedSentinel, &local_slots, &reg_slots,
                );
                write_reg(builder, &reg_slots, *dst, out);
            }
            Instruction::BitAnd { dst, lhs, rhs } => {
                let left = read_reg(builder, &reg_slots, *lhs);
                let right = read_reg(builder, &reg_slots, *rhs);
                let op_id = builder.ins().iconst(types::I64, 0);
                let guarded =
                    type_guards::emit_guarded_i32_bitwise(builder, BitwiseOp::And, left, right);
                let generic_ref = helpers.and_then(|h| h.get(HelperKind::GenericBitOp));
                let out = lower_guarded_with_generic_fallback(
                    builder, guarded, generic_ref, &[ctx_ptr, left, right, op_id],
                    ctx_ptr, pc, BailoutReason::HelperReturnedSentinel, &local_slots, &reg_slots,
                );
                write_reg(builder, &reg_slots, *dst, out);
            }
            Instruction::BitOr { dst, lhs, rhs } => {
                let left = read_reg(builder, &reg_slots, *lhs);
                let right = read_reg(builder, &reg_slots, *rhs);
                let op_id = builder.ins().iconst(types::I64, 1);
                let guarded =
                    type_guards::emit_guarded_i32_bitwise(builder, BitwiseOp::Or, left, right);
                let generic_ref = helpers.and_then(|h| h.get(HelperKind::GenericBitOp));
                let out = lower_guarded_with_generic_fallback(
                    builder, guarded, generic_ref, &[ctx_ptr, left, right, op_id],
                    ctx_ptr, pc, BailoutReason::HelperReturnedSentinel, &local_slots, &reg_slots,
                );
                write_reg(builder, &reg_slots, *dst, out);
            }
            Instruction::BitXor { dst, lhs, rhs } => {
                let left = read_reg(builder, &reg_slots, *lhs);
                let right = read_reg(builder, &reg_slots, *rhs);
                let op_id = builder.ins().iconst(types::I64, 2);
                let guarded =
                    type_guards::emit_guarded_i32_bitwise(builder, BitwiseOp::Xor, left, right);
                let generic_ref = helpers.and_then(|h| h.get(HelperKind::GenericBitOp));
                let out = lower_guarded_with_generic_fallback(
                    builder, guarded, generic_ref, &[ctx_ptr, left, right, op_id],
                    ctx_ptr, pc, BailoutReason::HelperReturnedSentinel, &local_slots, &reg_slots,
                );
                write_reg(builder, &reg_slots, *dst, out);
            }
            Instruction::Shl { dst, lhs, rhs } => {
                let left = read_reg(builder, &reg_slots, *lhs);
                let right = read_reg(builder, &reg_slots, *rhs);
                let op_id = builder.ins().iconst(types::I64, 3);
                let guarded =
                    type_guards::emit_guarded_i32_bitwise(builder, BitwiseOp::Shl, left, right);
                let generic_ref = helpers.and_then(|h| h.get(HelperKind::GenericBitOp));
                let out = lower_guarded_with_generic_fallback(
                    builder, guarded, generic_ref, &[ctx_ptr, left, right, op_id],
                    ctx_ptr, pc, BailoutReason::HelperReturnedSentinel, &local_slots, &reg_slots,
                );
                write_reg(builder, &reg_slots, *dst, out);
            }
            Instruction::Shr { dst, lhs, rhs } => {
                let left = read_reg(builder, &reg_slots, *lhs);
                let right = read_reg(builder, &reg_slots, *rhs);
                let op_id = builder.ins().iconst(types::I64, 4);
                let guarded =
                    type_guards::emit_guarded_i32_bitwise(builder, BitwiseOp::Shr, left, right);
                let generic_ref = helpers.and_then(|h| h.get(HelperKind::GenericBitOp));
                let out = lower_guarded_with_generic_fallback(
                    builder, guarded, generic_ref, &[ctx_ptr, left, right, op_id],
                    ctx_ptr, pc, BailoutReason::HelperReturnedSentinel, &local_slots, &reg_slots,
                );
                write_reg(builder, &reg_slots, *dst, out);
            }
            Instruction::Ushr { dst, lhs, rhs } => {
                let left = read_reg(builder, &reg_slots, *lhs);
                let right = read_reg(builder, &reg_slots, *rhs);
                let op_id = builder.ins().iconst(types::I64, 5);
                let guarded =
                    type_guards::emit_guarded_i32_bitwise(builder, BitwiseOp::Ushr, left, right);
                let generic_ref = helpers.and_then(|h| h.get(HelperKind::GenericBitOp));
                let out = lower_guarded_with_generic_fallback(
                    builder, guarded, generic_ref, &[ctx_ptr, left, right, op_id],
                    ctx_ptr, pc, BailoutReason::HelperReturnedSentinel, &local_slots, &reg_slots,
                );
                write_reg(builder, &reg_slots, *dst, out);
            }
            Instruction::BitNot { dst, src } => {
                let val = read_reg(builder, &reg_slots, *src);
                let guarded = type_guards::emit_guarded_i32_bitnot(builder, val);
                let generic_ref = helpers.and_then(|h| h.get(HelperKind::GenericBitNot));
                let out = lower_guarded_with_generic_fallback(
                    builder, guarded, generic_ref, &[ctx_ptr, val],
                    ctx_ptr, pc, BailoutReason::HelperReturnedSentinel, &local_slots, &reg_slots,
                );
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
                let generic_ref = helpers.and_then(|h| h.get(HelperKind::GenericEq));
                let out = lower_guarded_with_generic_fallback(
                    builder, guarded, generic_ref, &[ctx_ptr, left, right],
                    ctx_ptr, pc, BailoutReason::HelperReturnedSentinel, &local_slots, &reg_slots,
                );
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
                let generic_ref = helpers.and_then(|h| h.get(HelperKind::GenericNeq));
                let out = lower_guarded_with_generic_fallback(
                    builder, guarded, generic_ref, &[ctx_ptr, left, right],
                    ctx_ptr, pc, BailoutReason::HelperReturnedSentinel, &local_slots, &reg_slots,
                );
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
                let generic_ref = helpers.and_then(|h| h.get(HelperKind::GenericLt));
                let out = lower_guarded_with_generic_fallback(
                    builder, guarded, generic_ref, &[ctx_ptr, left, right],
                    ctx_ptr, pc, BailoutReason::HelperReturnedSentinel, &local_slots, &reg_slots,
                );
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
                let generic_ref = helpers.and_then(|h| h.get(HelperKind::GenericLe));
                let out = lower_guarded_with_generic_fallback(
                    builder, guarded, generic_ref, &[ctx_ptr, left, right],
                    ctx_ptr, pc, BailoutReason::HelperReturnedSentinel, &local_slots, &reg_slots,
                );
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
                let generic_ref = helpers.and_then(|h| h.get(HelperKind::GenericGt));
                let out = lower_guarded_with_generic_fallback(
                    builder, guarded, generic_ref, &[ctx_ptr, left, right],
                    ctx_ptr, pc, BailoutReason::HelperReturnedSentinel, &local_slots, &reg_slots,
                );
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
                let generic_ref = helpers.and_then(|h| h.get(HelperKind::GenericGe));
                let out = lower_guarded_with_generic_fallback(
                    builder, guarded, generic_ref, &[ctx_ptr, left, right],
                    ctx_ptr, pc, BailoutReason::HelperReturnedSentinel, &local_slots, &reg_slots,
                );
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
                let full_ref = helpers
                    .and_then(|h| h.get(HelperKind::GetPropConst))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let obj_val = read_reg(builder, &reg_slots, *obj);

                // Try monomorphic fast path based on compile-time IC snapshot
                let mono_ic = ic_snapshot.get(*ic_index as usize).and_then(|ic| {
                    if let InlineCacheState::Monomorphic { shape_id, offset } = ic {
                        Some((*shape_id, *offset))
                    } else {
                        None
                    }
                });
                let mono_ref = helpers.and_then(|h| h.get(HelperKind::GetPropMono));

                let result = if let (Some((shape_id, offset)), Some(mono_helper)) = (mono_ic, mono_ref) {
                    emit_mono_prop_with_fallback(
                        builder, mono_helper, full_ref,
                        obj_val, ctx_ptr, shape_id, offset,
                        name.index(), *ic_index,
                    )
                } else {
                    let name_idx = builder.ins().iconst(types::I64, name.index() as i64);
                    let ic_idx = builder.ins().iconst(types::I64, *ic_index as i64);
                    emit_helper_call_with_bailout(
                        builder, full_ref, &[ctx_ptr, obj_val, name_idx, ic_idx],
                    )
                };
                write_reg(builder, &reg_slots, *dst, result);
            }
            // Superinstruction: fused GetLocal + GetPropConst
            Instruction::GetLocalProp {
                dst,
                local_idx,
                name,
                ic_index,
            } => {
                let full_ref = helpers
                    .and_then(|h| h.get(HelperKind::GetPropConst))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let obj_val = read_local(builder, &local_slots, *local_idx);

                let mono_ic = ic_snapshot.get(*ic_index as usize).and_then(|ic| {
                    if let InlineCacheState::Monomorphic { shape_id, offset } = ic {
                        Some((*shape_id, *offset))
                    } else {
                        None
                    }
                });
                let mono_ref = helpers.and_then(|h| h.get(HelperKind::GetPropMono));

                let result = if let (Some((shape_id, offset)), Some(mono_helper)) = (mono_ic, mono_ref) {
                    emit_mono_prop_with_fallback(
                        builder, mono_helper, full_ref,
                        obj_val, ctx_ptr, shape_id, offset,
                        name.index(), *ic_index,
                    )
                } else {
                    let name_idx = builder.ins().iconst(types::I64, name.index() as i64);
                    let ic_idx = builder.ins().iconst(types::I64, *ic_index as i64);
                    emit_helper_call_with_bailout(
                        builder, full_ref, &[ctx_ptr, obj_val, name_idx, ic_idx],
                    )
                };
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
                let call = builder
                    .ins()
                    .call(helper_ref, &[ctx_ptr, obj_val, name_idx, value, ic_idx]);
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
                        let arg_val = read_reg(builder, &reg_slots, Register(func.0 + 1 + i));
                        builder.ins().stack_store(arg_val, slot, (i as i32) * 8);
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
            Instruction::GetGlobal {
                dst,
                name,
                ic_index,
            } => {
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
            Instruction::SetGlobal {
                name,
                src,
                ic_index,
                is_declaration,
            } => {
                let helper_ref = helpers
                    .and_then(|h| h.get(HelperKind::SetGlobal))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let val = read_reg(builder, &reg_slots, *src);
                let name_idx = builder.ins().iconst(types::I64, name.0 as i64);
                let ic_idx = builder.ins().iconst(types::I64, *ic_index as i64);
                let is_decl = builder.ins().iconst(types::I64, *is_declaration as i64);
                let call = builder
                    .ins()
                    .call(helper_ref, &[ctx_ptr, name_idx, val, ic_idx, is_decl]);
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
            Instruction::GetUpvalue { dst, idx } => {
                let helper_ref = helpers
                    .and_then(|h| h.get(HelperKind::GetUpvalue))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let idx_val = builder.ins().iconst(types::I64, idx.index() as i64);
                let call = builder.ins().call(helper_ref, &[ctx_ptr, idx_val]);
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
            Instruction::SetUpvalue { idx, src } => {
                let helper_ref = helpers
                    .and_then(|h| h.get(HelperKind::SetUpvalue))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let idx_val = builder.ins().iconst(types::I64, idx.index() as i64);
                let val = read_reg(builder, &reg_slots, *src);
                emit_helper_call_with_bailout(builder, helper_ref, &[ctx_ptr, idx_val, val]);
            }
            // --- Trivial opcodes (no helper needed) ---
            Instruction::Pop => {
                // No-op in register VM — Pop is a stack concept
            }
            Instruction::Dup { dst, src } => {
                // Same as Move
                let v = read_reg(builder, &reg_slots, *src);
                write_reg(builder, &reg_slots, *dst, v);
            }
            Instruction::Debugger => {
                // No-op for JIT
            }
            // --- Quickened property access (same helper as const variants) ---
            Instruction::GetPropQuickened {
                dst,
                obj,
                name,
                ic_index,
            } => {
                let full_ref = helpers
                    .and_then(|h| h.get(HelperKind::GetPropConst))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let obj_val = read_reg(builder, &reg_slots, *obj);

                let mono_ic = ic_snapshot.get(*ic_index as usize).and_then(|ic| {
                    if let InlineCacheState::Monomorphic { shape_id, offset } = ic {
                        Some((*shape_id, *offset))
                    } else {
                        None
                    }
                });
                let mono_ref = helpers.and_then(|h| h.get(HelperKind::GetPropMono));

                let result = if let (Some((shape_id, offset)), Some(mono_helper)) = (mono_ic, mono_ref) {
                    emit_mono_prop_with_fallback(
                        builder, mono_helper, full_ref,
                        obj_val, ctx_ptr, shape_id, offset,
                        name.index(), *ic_index,
                    )
                } else {
                    let name_idx = builder.ins().iconst(types::I64, name.index() as i64);
                    let ic_idx = builder.ins().iconst(types::I64, *ic_index as i64);
                    emit_helper_call_with_bailout(
                        builder, full_ref, &[ctx_ptr, obj_val, name_idx, ic_idx],
                    )
                };
                write_reg(builder, &reg_slots, *dst, result);
            }
            Instruction::SetPropQuickened {
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
                emit_helper_call_with_bailout(
                    builder,
                    helper_ref,
                    &[ctx_ptr, obj_val, name_idx, value, ic_idx],
                );
            }
            // --- LoadThis ---
            Instruction::LoadThis { dst } => {
                let helper_ref = helpers
                    .and_then(|h| h.get(HelperKind::LoadThis))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let result = emit_helper_call_with_bailout(builder, helper_ref, &[ctx_ptr]);
                write_reg(builder, &reg_slots, *dst, result);
            }
            // --- CloseUpvalue ---
            Instruction::CloseUpvalue { local_idx } => {
                let helper_ref = helpers
                    .and_then(|h| h.get(HelperKind::CloseUpvalue))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let idx_val = builder.ins().iconst(types::I64, local_idx.index() as i64);
                emit_helper_call_with_bailout(builder, helper_ref, &[ctx_ptr, idx_val]);
            }
            // --- TypeOf / TypeOfName ---
            Instruction::TypeOf { dst, src } => {
                let helper_ref = helpers
                    .and_then(|h| h.get(HelperKind::TypeOf))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let val = read_reg(builder, &reg_slots, *src);
                let result = emit_helper_call_with_bailout(builder, helper_ref, &[ctx_ptr, val]);
                write_reg(builder, &reg_slots, *dst, result);
            }
            Instruction::TypeOfName { dst, name } => {
                let helper_ref = helpers
                    .and_then(|h| h.get(HelperKind::TypeOfName))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let name_idx = builder.ins().iconst(types::I64, name.index() as i64);
                let result =
                    emit_helper_call_with_bailout(builder, helper_ref, &[ctx_ptr, name_idx]);
                write_reg(builder, &reg_slots, *dst, result);
            }
            // --- Pow ---
            Instruction::Pow { dst, lhs, rhs } => {
                let helper_ref = helpers
                    .and_then(|h| h.get(HelperKind::Pow))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let left = read_reg(builder, &reg_slots, *lhs);
                let right = read_reg(builder, &reg_slots, *rhs);
                let result =
                    emit_helper_call_with_bailout(builder, helper_ref, &[ctx_ptr, left, right]);
                write_reg(builder, &reg_slots, *dst, result);
            }
            // --- GetProp / SetProp (dynamic key) ---
            Instruction::GetProp {
                dst,
                obj,
                key,
                ic_index,
            } => {
                let helper_ref = helpers
                    .and_then(|h| h.get(HelperKind::GetProp))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let obj_val = read_reg(builder, &reg_slots, *obj);
                let key_val = read_reg(builder, &reg_slots, *key);
                let ic_idx = builder.ins().iconst(types::I64, *ic_index as i64);
                let result = emit_helper_call_with_bailout(
                    builder,
                    helper_ref,
                    &[ctx_ptr, obj_val, key_val, ic_idx],
                );
                write_reg(builder, &reg_slots, *dst, result);
            }
            Instruction::SetProp {
                obj,
                key,
                val,
                ic_index,
            } => {
                let helper_ref = helpers
                    .and_then(|h| h.get(HelperKind::SetProp))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let obj_val = read_reg(builder, &reg_slots, *obj);
                let key_val = read_reg(builder, &reg_slots, *key);
                let value = read_reg(builder, &reg_slots, *val);
                let ic_idx = builder.ins().iconst(types::I64, *ic_index as i64);
                emit_helper_call_with_bailout(
                    builder,
                    helper_ref,
                    &[ctx_ptr, obj_val, key_val, value, ic_idx],
                );
            }
            // --- GetElem / SetElem ---
            Instruction::GetElem {
                dst,
                arr,
                idx,
                ic_index,
            } => {
                let helper_ref = helpers
                    .and_then(|h| h.get(HelperKind::GetElem))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let obj_val = read_reg(builder, &reg_slots, *arr);
                let idx_val = read_reg(builder, &reg_slots, *idx);
                let ic_idx = builder.ins().iconst(types::I64, *ic_index as i64);
                let result = emit_helper_call_with_bailout(
                    builder,
                    helper_ref,
                    &[ctx_ptr, obj_val, idx_val, ic_idx],
                );
                write_reg(builder, &reg_slots, *dst, result);
            }
            Instruction::SetElem {
                arr,
                idx,
                val,
                ic_index,
            } => {
                let helper_ref = helpers
                    .and_then(|h| h.get(HelperKind::SetElem))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let obj_val = read_reg(builder, &reg_slots, *arr);
                let idx_val = read_reg(builder, &reg_slots, *idx);
                let value = read_reg(builder, &reg_slots, *val);
                let ic_idx = builder.ins().iconst(types::I64, *ic_index as i64);
                emit_helper_call_with_bailout(
                    builder,
                    helper_ref,
                    &[ctx_ptr, obj_val, idx_val, value, ic_idx],
                );
            }
            // --- DeleteProp ---
            Instruction::DeleteProp { dst, obj, key } => {
                let helper_ref = helpers
                    .and_then(|h| h.get(HelperKind::DeleteProp))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let obj_val = read_reg(builder, &reg_slots, *obj);
                let key_val = read_reg(builder, &reg_slots, *key);
                let result = emit_helper_call_with_bailout(
                    builder,
                    helper_ref,
                    &[ctx_ptr, obj_val, key_val],
                );
                write_reg(builder, &reg_slots, *dst, result);
            }
            // --- DefineProperty ---
            Instruction::DefineProperty { obj, key, val } => {
                let helper_ref = helpers
                    .and_then(|h| h.get(HelperKind::DefineProperty))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let obj_val = read_reg(builder, &reg_slots, *obj);
                let key_val = read_reg(builder, &reg_slots, *key);
                let value = read_reg(builder, &reg_slots, *val);
                emit_helper_call_with_bailout(
                    builder,
                    helper_ref,
                    &[ctx_ptr, obj_val, key_val, value],
                );
            }
            // --- Throw ---
            Instruction::Throw { src } => {
                let helper_ref = helpers
                    .and_then(|h| h.get(HelperKind::ThrowValue))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let val = read_reg(builder, &reg_slots, *src);
                // ThrowValue always returns BAILOUT_SENTINEL, which triggers bailout
                emit_helper_call_with_bailout(builder, helper_ref, &[ctx_ptr, val]);
                // Throw is terminal — don't fall through
                continue;
            }
            // --- Construct ---
            Instruction::Construct { dst, func, argc } => {
                let helper_ref = helpers
                    .and_then(|h| h.get(HelperKind::Construct))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let callee_val = read_reg(builder, &reg_slots, *func);
                let argc_val = builder.ins().iconst(types::I64, *argc as i64);
                let argv_ptr = if *argc > 0 {
                    let slot = builder.create_sized_stack_slot(StackSlotData::new(
                        StackSlotKind::ExplicitSlot,
                        (*argc as u32) * 8,
                        8,
                    ));
                    for i in 0..(*argc as u16) {
                        let arg_val = read_reg(builder, &reg_slots, Register(func.0 + 1 + i));
                        builder.ins().stack_store(arg_val, slot, (i as i32) * 8);
                    }
                    builder.ins().stack_addr(types::I64, slot, 0)
                } else {
                    builder.ins().iconst(types::I64, 0)
                };
                let result = emit_helper_call_with_bailout(
                    builder,
                    helper_ref,
                    &[ctx_ptr, callee_val, argc_val, argv_ptr],
                );
                write_reg(builder, &reg_slots, *dst, result);
            }
            // --- CallMethod ---
            Instruction::CallMethod {
                dst,
                obj,
                method,
                argc,
                ic_index,
            } => {
                let helper_ref = helpers
                    .and_then(|h| h.get(HelperKind::CallMethod))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let obj_val = read_reg(builder, &reg_slots, *obj);
                let method_name_idx = builder.ins().iconst(types::I64, method.index() as i64);
                let argc_val = builder.ins().iconst(types::I64, *argc as i64);
                let argv_ptr = if *argc > 0 {
                    let slot = builder.create_sized_stack_slot(StackSlotData::new(
                        StackSlotKind::ExplicitSlot,
                        (*argc as u32) * 8,
                        8,
                    ));
                    for i in 0..(*argc as u16) {
                        let arg_val = read_reg(builder, &reg_slots, Register(obj.0 + 1 + i));
                        builder.ins().stack_store(arg_val, slot, (i as i32) * 8);
                    }
                    builder.ins().stack_addr(types::I64, slot, 0)
                } else {
                    builder.ins().iconst(types::I64, 0)
                };
                let ic_idx = builder.ins().iconst(types::I64, *ic_index as i64);
                let result = emit_helper_call_with_bailout(
                    builder,
                    helper_ref,
                    &[
                        ctx_ptr,
                        obj_val,
                        method_name_idx,
                        argc_val,
                        argv_ptr,
                        ic_idx,
                    ],
                );
                write_reg(builder, &reg_slots, *dst, result);
            }
            // --- CallWithReceiver ---
            Instruction::CallWithReceiver {
                dst,
                func,
                this,
                argc,
            } => {
                let helper_ref = helpers
                    .and_then(|h| h.get(HelperKind::CallWithReceiver))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let callee_val = read_reg(builder, &reg_slots, *func);
                let this_val = read_reg(builder, &reg_slots, *this);
                let argc_val = builder.ins().iconst(types::I64, *argc as i64);
                let argv_ptr = if *argc > 0 {
                    let slot = builder.create_sized_stack_slot(StackSlotData::new(
                        StackSlotKind::ExplicitSlot,
                        (*argc as u32) * 8,
                        8,
                    ));
                    for i in 0..(*argc as u16) {
                        let arg_val = read_reg(builder, &reg_slots, Register(func.0 + 1 + i));
                        builder.ins().stack_store(arg_val, slot, (i as i32) * 8);
                    }
                    builder.ins().stack_addr(types::I64, slot, 0)
                } else {
                    builder.ins().iconst(types::I64, 0)
                };
                let result = emit_helper_call_with_bailout(
                    builder,
                    helper_ref,
                    &[ctx_ptr, callee_val, this_val, argc_val, argv_ptr],
                );
                write_reg(builder, &reg_slots, *dst, result);
            }
            // --- CallMethodComputed ---
            Instruction::CallMethodComputed {
                dst,
                obj,
                key,
                argc,
                ic_index,
            } => {
                let helper_ref = helpers
                    .and_then(|h| h.get(HelperKind::CallMethodComputed))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let obj_val = read_reg(builder, &reg_slots, *obj);
                let key_val = read_reg(builder, &reg_slots, *key);
                let argc_val = builder.ins().iconst(types::I64, *argc as i64);
                let argv_ptr = if *argc > 0 {
                    let slot = builder.create_sized_stack_slot(StackSlotData::new(
                        StackSlotKind::ExplicitSlot,
                        (*argc as u32) * 8,
                        8,
                    ));
                    // args start after key register
                    for i in 0..(*argc as u16) {
                        let arg_val = read_reg(builder, &reg_slots, Register(key.0 + 1 + i));
                        builder.ins().stack_store(arg_val, slot, (i as i32) * 8);
                    }
                    builder.ins().stack_addr(types::I64, slot, 0)
                } else {
                    builder.ins().iconst(types::I64, 0)
                };
                let ic_idx = builder.ins().iconst(types::I64, *ic_index as i64);
                let result = emit_helper_call_with_bailout(
                    builder,
                    helper_ref,
                    &[ctx_ptr, obj_val, key_val, argc_val, argv_ptr, ic_idx],
                );
                write_reg(builder, &reg_slots, *dst, result);
            }
            // --- ToNumber / ToString / RequireCoercible ---
            Instruction::ToNumber { dst, src } => {
                let helper_ref = helpers
                    .and_then(|h| h.get(HelperKind::ToNumber))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let val = read_reg(builder, &reg_slots, *src);
                let result = emit_helper_call_with_bailout(builder, helper_ref, &[ctx_ptr, val]);
                write_reg(builder, &reg_slots, *dst, result);
            }
            Instruction::ToString { dst, src } => {
                let helper_ref = helpers
                    .and_then(|h| h.get(HelperKind::JsToString))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let val = read_reg(builder, &reg_slots, *src);
                let result = emit_helper_call_with_bailout(builder, helper_ref, &[ctx_ptr, val]);
                write_reg(builder, &reg_slots, *dst, result);
            }
            Instruction::RequireCoercible { src } => {
                let helper_ref = helpers
                    .and_then(|h| h.get(HelperKind::RequireCoercible))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let val = read_reg(builder, &reg_slots, *src);
                emit_helper_call_with_bailout(builder, helper_ref, &[ctx_ptr, val]);
            }
            // --- InstanceOf / In ---
            Instruction::InstanceOf {
                dst,
                lhs,
                rhs,
                ic_index,
            } => {
                let helper_ref = helpers
                    .and_then(|h| h.get(HelperKind::InstanceOf))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let left = read_reg(builder, &reg_slots, *lhs);
                let right = read_reg(builder, &reg_slots, *rhs);
                let ic_idx = builder.ins().iconst(types::I64, *ic_index as i64);
                let result = emit_helper_call_with_bailout(
                    builder,
                    helper_ref,
                    &[ctx_ptr, left, right, ic_idx],
                );
                write_reg(builder, &reg_slots, *dst, result);
            }
            Instruction::In {
                dst,
                lhs,
                rhs,
                ic_index,
            } => {
                let helper_ref = helpers
                    .and_then(|h| h.get(HelperKind::InOp))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let left = read_reg(builder, &reg_slots, *lhs);
                let right = read_reg(builder, &reg_slots, *rhs);
                let ic_idx = builder.ins().iconst(types::I64, *ic_index as i64);
                let result = emit_helper_call_with_bailout(
                    builder,
                    helper_ref,
                    &[ctx_ptr, left, right, ic_idx],
                );
                write_reg(builder, &reg_slots, *dst, result);
            }
            // --- DeclareGlobalVar ---
            Instruction::DeclareGlobalVar { name, configurable } => {
                let helper_ref = helpers
                    .and_then(|h| h.get(HelperKind::DeclareGlobalVar))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let name_idx = builder.ins().iconst(types::I64, name.index() as i64);
                let config = builder.ins().iconst(types::I64, *configurable as i64);
                emit_helper_call_with_bailout(builder, helper_ref, &[ctx_ptr, name_idx, config]);
            }
            // --- DefineGetter / DefineSetter / DefineMethod ---
            Instruction::DefineGetter { obj, key, func } => {
                let helper_ref = helpers
                    .and_then(|h| h.get(HelperKind::DefineGetter))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let obj_val = read_reg(builder, &reg_slots, *obj);
                let key_val = read_reg(builder, &reg_slots, *key);
                let func_val = read_reg(builder, &reg_slots, *func);
                emit_helper_call_with_bailout(
                    builder,
                    helper_ref,
                    &[ctx_ptr, obj_val, key_val, func_val],
                );
            }
            Instruction::DefineSetter { obj, key, func } => {
                let helper_ref = helpers
                    .and_then(|h| h.get(HelperKind::DefineSetter))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let obj_val = read_reg(builder, &reg_slots, *obj);
                let key_val = read_reg(builder, &reg_slots, *key);
                let func_val = read_reg(builder, &reg_slots, *func);
                emit_helper_call_with_bailout(
                    builder,
                    helper_ref,
                    &[ctx_ptr, obj_val, key_val, func_val],
                );
            }
            Instruction::DefineMethod { obj, key, val } => {
                let helper_ref = helpers
                    .and_then(|h| h.get(HelperKind::DefineMethod))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let obj_val = read_reg(builder, &reg_slots, *obj);
                let key_val = read_reg(builder, &reg_slots, *key);
                let val_val = read_reg(builder, &reg_slots, *val);
                emit_helper_call_with_bailout(
                    builder,
                    helper_ref,
                    &[ctx_ptr, obj_val, key_val, val_val],
                );
            }
            // --- Spread ---
            Instruction::Spread { dst, src } => {
                let helper_ref = helpers
                    .and_then(|h| h.get(HelperKind::SpreadArray))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let dst_val = read_reg(builder, &reg_slots, *dst);
                let src_val = read_reg(builder, &reg_slots, *src);
                emit_helper_call_with_bailout(builder, helper_ref, &[ctx_ptr, dst_val, src_val]);
            }
            // --- Closure ---
            Instruction::Closure { dst, func: _ } => {
                // Closure creation needs capture_upvalues from interpreter frame.
                // Always bails out so the interpreter handles it.
                let helper_ref = helpers
                    .and_then(|h| h.get(HelperKind::ClosureCreate))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let func_idx = builder.ins().iconst(types::I64, 0);
                let result =
                    emit_helper_call_with_bailout(builder, helper_ref, &[ctx_ptr, func_idx]);
                write_reg(builder, &reg_slots, *dst, result);
            }
            // --- CreateArguments ---
            Instruction::CreateArguments { dst } => {
                // Arguments object needs frame info. Always bails out.
                let helper_ref = helpers
                    .and_then(|h| h.get(HelperKind::CreateArguments))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let result = emit_helper_call_with_bailout(builder, helper_ref, &[ctx_ptr]);
                write_reg(builder, &reg_slots, *dst, result);
            }
            // --- GetIterator ---
            Instruction::GetIterator { dst, src } => {
                let helper_ref = helpers
                    .and_then(|h| h.get(HelperKind::GetIterator))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let src_val = read_reg(builder, &reg_slots, *src);
                let result =
                    emit_helper_call_with_bailout(builder, helper_ref, &[ctx_ptr, src_val]);
                write_reg(builder, &reg_slots, *dst, result);
            }
            // --- IteratorNext ---
            Instruction::IteratorNext { dst, done, iter } => {
                // Call helper: returns value, writes done to ctx.secondary_result
                let helper_ref = helpers
                    .and_then(|h| h.get(HelperKind::IteratorNext))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let iter_val = read_reg(builder, &reg_slots, *iter);
                let result =
                    emit_helper_call_with_bailout(builder, helper_ref, &[ctx_ptr, iter_val]);
                write_reg(builder, &reg_slots, *dst, result);
                // Read done flag from ctx.secondary_result
                let done_val = builder.ins().load(
                    types::I64,
                    MemFlags::trusted(),
                    ctx_ptr,
                    crate::runtime_helpers::JIT_CTX_SECONDARY_RESULT_OFFSET,
                );
                write_reg(builder, &reg_slots, *done, done_val);
            }
            // --- IteratorClose ---
            Instruction::IteratorClose { iter } => {
                let helper_ref = helpers
                    .and_then(|h| h.get(HelperKind::IteratorClose))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let iter_val = read_reg(builder, &reg_slots, *iter);
                emit_helper_call_with_bailout(builder, helper_ref, &[ctx_ptr, iter_val]);
            }
            // --- CallSpread ---
            Instruction::CallSpread {
                dst,
                func,
                argc,
                spread,
            } => {
                let helper_ref = helpers
                    .and_then(|h| h.get(HelperKind::CallSpread))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let callee_val = read_reg(builder, &reg_slots, *func);
                let spread_val = read_reg(builder, &reg_slots, *spread);
                let argc_val = builder.ins().iconst(types::I64, *argc as i64);

                // Build argv on stack for regular args
                if *argc > 0 {
                    let slot = builder.create_sized_stack_slot(StackSlotData::new(
                        StackSlotKind::ExplicitSlot,
                        (*argc as u32) * 8,
                        8,
                    ));
                    for i in 0..(*argc as u16) {
                        let arg = read_reg(builder, &reg_slots, Register(func.0 + 1 + i));
                        builder.ins().stack_store(arg, slot, (i as i32) * 8);
                    }
                    let argv = builder.ins().stack_addr(types::I64, slot, 0);
                    let result = emit_helper_call_with_bailout(
                        builder,
                        helper_ref,
                        &[ctx_ptr, callee_val, argc_val, argv, spread_val],
                    );
                    write_reg(builder, &reg_slots, *dst, result);
                } else {
                    let zero = builder.ins().iconst(types::I64, 0);
                    let result = emit_helper_call_with_bailout(
                        builder,
                        helper_ref,
                        &[ctx_ptr, callee_val, argc_val, zero, spread_val],
                    );
                    write_reg(builder, &reg_slots, *dst, result);
                }
            }
            // --- ConstructSpread ---
            Instruction::ConstructSpread {
                dst,
                func,
                argc,
                spread,
            } => {
                let helper_ref = helpers
                    .and_then(|h| h.get(HelperKind::ConstructSpread))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let callee_val = read_reg(builder, &reg_slots, *func);
                let spread_val = read_reg(builder, &reg_slots, *spread);
                let argc_val = builder.ins().iconst(types::I64, *argc as i64);

                if *argc > 0 {
                    let slot = builder.create_sized_stack_slot(StackSlotData::new(
                        StackSlotKind::ExplicitSlot,
                        (*argc as u32) * 8,
                        8,
                    ));
                    for i in 0..(*argc as u16) {
                        let arg = read_reg(builder, &reg_slots, Register(func.0 + 1 + i));
                        builder.ins().stack_store(arg, slot, (i as i32) * 8);
                    }
                    let argv = builder.ins().stack_addr(types::I64, slot, 0);
                    let result = emit_helper_call_with_bailout(
                        builder,
                        helper_ref,
                        &[ctx_ptr, callee_val, argc_val, argv, spread_val],
                    );
                    write_reg(builder, &reg_slots, *dst, result);
                } else {
                    let zero = builder.ins().iconst(types::I64, 0);
                    let result = emit_helper_call_with_bailout(
                        builder,
                        helper_ref,
                        &[ctx_ptr, callee_val, argc_val, zero, spread_val],
                    );
                    write_reg(builder, &reg_slots, *dst, result);
                }
            }
            // --- CallMethodComputedSpread ---
            Instruction::CallMethodComputedSpread {
                dst,
                obj,
                key,
                spread,
                ic_index,
            } => {
                let helper_ref = helpers
                    .and_then(|h| h.get(HelperKind::CallMethodComputedSpread))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let obj_val = read_reg(builder, &reg_slots, *obj);
                let key_val = read_reg(builder, &reg_slots, *key);
                let spread_val = read_reg(builder, &reg_slots, *spread);
                let ic_idx = builder.ins().iconst(types::I64, *ic_index as i64);
                let result = emit_helper_call_with_bailout(
                    builder,
                    helper_ref,
                    &[ctx_ptr, obj_val, key_val, spread_val, ic_idx],
                );
                write_reg(builder, &reg_slots, *dst, result);
            }
            // --- TailCall ---
            Instruction::TailCall { func, argc } => {
                let helper_ref = helpers
                    .and_then(|h| h.get(HelperKind::TailCallHelper))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let callee_val = read_reg(builder, &reg_slots, *func);
                let argc_val = builder.ins().iconst(types::I64, *argc as i64);

                if *argc > 0 {
                    let slot = builder.create_sized_stack_slot(StackSlotData::new(
                        StackSlotKind::ExplicitSlot,
                        (*argc as u32) * 8,
                        8,
                    ));
                    for i in 0..(*argc as u16) {
                        let arg = read_reg(builder, &reg_slots, Register(func.0 + 1 + i));
                        builder.ins().stack_store(arg, slot, (i as i32) * 8);
                    }
                    let argv = builder.ins().stack_addr(types::I64, slot, 0);
                    let result = emit_helper_call_with_bailout(
                        builder,
                        helper_ref,
                        &[ctx_ptr, callee_val, argc_val, argv],
                    );
                    // TailCall: return the result directly
                    builder.ins().return_(&[result]);
                    continue;
                } else {
                    let zero = builder.ins().iconst(types::I64, 0);
                    let result = emit_helper_call_with_bailout(
                        builder,
                        helper_ref,
                        &[ctx_ptr, callee_val, argc_val, zero],
                    );
                    builder.ins().return_(&[result]);
                    continue;
                }
            }
            // === Helper-backed extended subset ===
            // Most opcodes below have real runtime helper implementations.
            // Yield/Await remain non-eligible and are guarded at translator
            // eligibility level because suspension is not yet supported in JIT.
            Instruction::TryStart { catch_offset } => {
                let helper_ref = helpers
                    .and_then(|h| h.get(HelperKind::TryStart))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let catch_pc = (pc as i32 + catch_offset.0) as i64;
                let catch_pc_val = builder.ins().iconst(types::I64, catch_pc);
                emit_helper_call_with_bailout(builder, helper_ref, &[ctx_ptr, catch_pc_val]);
            }
            Instruction::TryEnd => {
                let helper_ref = helpers
                    .and_then(|h| h.get(HelperKind::TryEnd))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                emit_helper_call_with_bailout(builder, helper_ref, &[ctx_ptr]);
            }
            Instruction::Catch { dst } => {
                let helper_ref = helpers
                    .and_then(|h| h.get(HelperKind::CatchOp))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let result = emit_helper_call_with_bailout(builder, helper_ref, &[ctx_ptr]);
                write_reg(builder, &reg_slots, *dst, result);
            }
            Instruction::DefineClass {
                dst,
                name,
                ctor,
                super_class,
            } => {
                let helper_ref = helpers
                    .and_then(|h| h.get(HelperKind::DefineClass))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let ctor_val = read_reg(builder, &reg_slots, *ctor);
                let super_val = match super_class {
                    Some(reg) => read_reg(builder, &reg_slots, *reg),
                    None => builder.ins().iconst(types::I64, type_guards::TAG_UNDEFINED),
                };
                let name_idx = builder.ins().iconst(types::I64, name.0 as i64);
                let result = emit_helper_call_with_bailout(
                    builder,
                    helper_ref,
                    &[ctx_ptr, ctor_val, super_val, name_idx],
                );
                write_reg(builder, &reg_slots, *dst, result);
            }
            Instruction::GetSuper { dst } => {
                let helper_ref = helpers
                    .and_then(|h| h.get(HelperKind::GetSuper))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let result = emit_helper_call_with_bailout(builder, helper_ref, &[ctx_ptr]);
                write_reg(builder, &reg_slots, *dst, result);
            }
            Instruction::CallSuper { dst, args, argc } => {
                let helper_ref = helpers
                    .and_then(|h| h.get(HelperKind::CallSuper))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let argc_val = builder.ins().iconst(types::I64, *argc as i64);
                if *argc > 0 {
                    let slot = builder.create_sized_stack_slot(StackSlotData::new(
                        StackSlotKind::ExplicitSlot,
                        (*argc as u32) * 8,
                        8,
                    ));
                    for i in 0..(*argc as u16) {
                        let arg = read_reg(builder, &reg_slots, Register(args.0 + i));
                        builder.ins().stack_store(arg, slot, (i as i32) * 8);
                    }
                    let argv = builder.ins().stack_addr(types::I64, slot, 0);
                    let result = emit_helper_call_with_bailout(
                        builder,
                        helper_ref,
                        &[ctx_ptr, argc_val, argv],
                    );
                    write_reg(builder, &reg_slots, *dst, result);
                } else {
                    let zero = builder.ins().iconst(types::I64, 0);
                    let result = emit_helper_call_with_bailout(
                        builder,
                        helper_ref,
                        &[ctx_ptr, argc_val, zero],
                    );
                    write_reg(builder, &reg_slots, *dst, result);
                }
            }
            Instruction::GetSuperProp { dst, name } => {
                let helper_ref = helpers
                    .and_then(|h| h.get(HelperKind::GetSuperProp))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let name_idx = builder.ins().iconst(types::I64, name.0 as i64);
                let result =
                    emit_helper_call_with_bailout(builder, helper_ref, &[ctx_ptr, name_idx]);
                write_reg(builder, &reg_slots, *dst, result);
            }
            Instruction::SetHomeObject { func, obj } => {
                let helper_ref = helpers
                    .and_then(|h| h.get(HelperKind::SetHomeObject))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let func_val = read_reg(builder, &reg_slots, *func);
                let obj_val = read_reg(builder, &reg_slots, *obj);
                let result = emit_helper_call_with_bailout(
                    builder,
                    helper_ref,
                    &[ctx_ptr, func_val, obj_val],
                );
                // SetHomeObject returns the new function value — write back to func register
                write_reg(builder, &reg_slots, *func, result);
            }
            Instruction::CallSuperForward { dst } => {
                let helper_ref = helpers
                    .and_then(|h| h.get(HelperKind::CallSuperForward))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let result = emit_helper_call_with_bailout(builder, helper_ref, &[ctx_ptr]);
                write_reg(builder, &reg_slots, *dst, result);
            }
            Instruction::CallSuperSpread { dst, args } => {
                let helper_ref = helpers
                    .and_then(|h| h.get(HelperKind::CallSuperSpread))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let args_val = read_reg(builder, &reg_slots, *args);
                let result =
                    emit_helper_call_with_bailout(builder, helper_ref, &[ctx_ptr, args_val]);
                write_reg(builder, &reg_slots, *dst, result);
            }
            Instruction::Yield { dst, .. } => {
                let helper_ref = helpers
                    .and_then(|h| h.get(HelperKind::YieldOp))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let result = emit_helper_call_with_bailout(builder, helper_ref, &[ctx_ptr]);
                write_reg(builder, &reg_slots, *dst, result);
            }
            Instruction::Await { dst, .. } => {
                let helper_ref = helpers
                    .and_then(|h| h.get(HelperKind::AwaitOp))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let result = emit_helper_call_with_bailout(builder, helper_ref, &[ctx_ptr]);
                write_reg(builder, &reg_slots, *dst, result);
            }
            Instruction::AsyncClosure { dst, func } => {
                let helper_ref = helpers
                    .and_then(|h| h.get(HelperKind::AsyncClosure))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let func_idx = builder.ins().iconst(types::I64, func.0 as i64);
                let result =
                    emit_helper_call_with_bailout(builder, helper_ref, &[ctx_ptr, func_idx]);
                write_reg(builder, &reg_slots, *dst, result);
            }
            Instruction::GeneratorClosure { dst, func } => {
                let helper_ref = helpers
                    .and_then(|h| h.get(HelperKind::GeneratorClosure))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let func_idx = builder.ins().iconst(types::I64, func.0 as i64);
                let result =
                    emit_helper_call_with_bailout(builder, helper_ref, &[ctx_ptr, func_idx]);
                write_reg(builder, &reg_slots, *dst, result);
            }
            Instruction::AsyncGeneratorClosure { dst, func } => {
                let helper_ref = helpers
                    .and_then(|h| h.get(HelperKind::AsyncGeneratorClosure))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let func_idx = builder.ins().iconst(types::I64, func.0 as i64);
                let result =
                    emit_helper_call_with_bailout(builder, helper_ref, &[ctx_ptr, func_idx]);
                write_reg(builder, &reg_slots, *dst, result);
            }
            Instruction::CallEval { dst, code } => {
                let helper_ref = helpers
                    .and_then(|h| h.get(HelperKind::CallEval))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let code_val = read_reg(builder, &reg_slots, *code);
                let result =
                    emit_helper_call_with_bailout(builder, helper_ref, &[ctx_ptr, code_val]);
                write_reg(builder, &reg_slots, *dst, result);
            }
            Instruction::Import { dst, module } => {
                let helper_ref = helpers
                    .and_then(|h| h.get(HelperKind::ImportOp))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let module_idx = builder.ins().iconst(types::I64, module.index() as i64);
                let result =
                    emit_helper_call_with_bailout(builder, helper_ref, &[ctx_ptr, module_idx]);
                write_reg(builder, &reg_slots, *dst, result);
            }
            Instruction::Export { name, src } => {
                let helper_ref = helpers
                    .and_then(|h| h.get(HelperKind::ExportOp))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let name_idx = builder.ins().iconst(types::I64, name.index() as i64);
                let src_val = read_reg(builder, &reg_slots, *src);
                emit_helper_call_with_bailout(builder, helper_ref, &[ctx_ptr, name_idx, src_val]);
            }
            Instruction::GetAsyncIterator { dst, src } => {
                let helper_ref = helpers
                    .and_then(|h| h.get(HelperKind::GetAsyncIterator))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let src_val = read_reg(builder, &reg_slots, *src);
                let result =
                    emit_helper_call_with_bailout(builder, helper_ref, &[ctx_ptr, src_val]);
                write_reg(builder, &reg_slots, *dst, result);
            }
            Instruction::ForInNext { dst, obj, offset } => {
                let helper_ref = helpers
                    .and_then(|h| h.get(HelperKind::ForInNext))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let obj_val = read_reg(builder, &reg_slots, *obj);
                let result =
                    emit_helper_call_with_bailout(builder, helper_ref, &[ctx_ptr, obj_val]);
                write_reg(builder, &reg_slots, *dst, result);

                // Interpreter semantics: if helper returns `undefined`, take the jump offset.
                let undef = builder.ins().iconst(types::I64, type_guards::TAG_UNDEFINED);
                let is_done = builder.ins().icmp(IntCC::Equal, result, undef);
                let jump_to = jump_target(pc, offset.offset(), instruction_count)?;
                let fallthrough = pc + 1;
                if fallthrough < instruction_count {
                    builder
                        .ins()
                        .brif(is_done, blocks[jump_to], &[], blocks[fallthrough], &[]);
                } else {
                    builder.ins().brif(is_done, blocks[jump_to], &[], exit, &[]);
                }
                continue;
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

#[cfg(test)]
mod tests {
    use super::*;
    use otter_vm_bytecode::operand::{JumpOffset, Register};

    #[test]
    fn helper_eligibility_rejects_yield_and_await() {
        let yield_fn = Function::builder()
            .name("yield_non_eligible")
            .register_count(2)
            .instruction(Instruction::LoadInt32 {
                dst: Register(1),
                value: 1,
            })
            .instruction(Instruction::Yield {
                dst: Register(0),
                src: Register(1),
            })
            .instruction(Instruction::Return { src: Register(0) })
            .build();
        assert!(!can_translate_function_with_helpers(&yield_fn, &[]));

        let await_fn = Function::builder()
            .name("await_non_eligible")
            .register_count(2)
            .instruction(Instruction::LoadInt32 {
                dst: Register(1),
                value: 1,
            })
            .instruction(Instruction::Await {
                dst: Register(0),
                src: Register(1),
            })
            .instruction(Instruction::Return { src: Register(0) })
            .build();
        assert!(!can_translate_function_with_helpers(&await_fn, &[]));
    }

    #[test]
    fn helper_eligibility_keeps_real_helper_paths() {
        let try_fn = Function::builder()
            .name("try_supported")
            .register_count(1)
            .instruction(Instruction::TryStart {
                catch_offset: JumpOffset(2),
            })
            .instruction(Instruction::TryEnd)
            .instruction(Instruction::ReturnUndefined)
            .build();
        assert!(can_translate_function_with_helpers(&try_fn, &[]));

        let import_fn = Function::builder()
            .name("import_supported")
            .register_count(1)
            .instruction(Instruction::Import {
                dst: Register(0),
                module: ConstantIndex(0),
            })
            .instruction(Instruction::Return { src: Register(0) })
            .build();
        assert!(can_translate_function_with_helpers(&import_fn, &[]));
    }

    #[test]
    fn helper_eligibility_rejects_async_and_generator_flags() {
        let async_fn = Function::builder()
            .name("async_flag_non_eligible")
            .register_count(1)
            .is_async(true)
            .instruction(Instruction::ReturnUndefined)
            .build();
        assert!(!can_translate_function_with_helpers(&async_fn, &[]));

        let generator_fn = Function::builder()
            .name("generator_flag_non_eligible")
            .register_count(1)
            .is_generator(true)
            .instruction(Instruction::ReturnUndefined)
            .build();
        assert!(!can_translate_function_with_helpers(&generator_fn, &[]));
    }
}
