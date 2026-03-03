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
use crate::loop_analysis;
use crate::runtime_helpers::{
    HelperKind, HelperRefs, JIT_CTX_BAILOUT_PC_OFFSET, JIT_CTX_BAILOUT_REASON_OFFSET,
    JIT_CTX_DEOPT_LOCALS_PTR_OFFSET, JIT_CTX_DEOPT_REGS_PTR_OFFSET, JIT_CTX_OSR_ENTRY_PC_OFFSET,
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

// ---------------------------------------------------------------------------
// Static callee resolution for function inlining
// ---------------------------------------------------------------------------

/// A statically resolved inline candidate.
struct InlineCandidate<'a> {
    /// The function to inline.
    callee: &'a Function,
    /// The function index in the module (for diagnostics).
    #[allow(dead_code)]
    function_index: u32,
}

/// Resolve which Call instructions have statically known callees eligible for inlining.
///
/// Tracks `Closure` → `SetLocal` → `GetLocal` → `Call` patterns to identify
/// which register holds which function index at each Call site.
fn resolve_inline_candidates<'a>(
    instructions: &[Instruction],
    module_functions: &'a [(u32, Function)],
) -> std::collections::HashMap<usize, InlineCandidate<'a>> {
    if module_functions.is_empty() {
        return std::collections::HashMap::new();
    }

    // Build index: function_index → &Function
    let func_by_index: std::collections::HashMap<u32, &Function> = module_functions
        .iter()
        .map(|(idx, func)| (*idx, func))
        .collect();

    // Track which registers and locals hold known function indices.
    // reg_func[reg] = Some(function_index) if the register holds a known closure.
    // local_func[local] = Some(function_index) if the local holds a known closure.
    let mut reg_func: std::collections::HashMap<u16, u32> = std::collections::HashMap::new();
    let mut local_func: std::collections::HashMap<u16, u32> = std::collections::HashMap::new();
    let mut result: std::collections::HashMap<usize, InlineCandidate<'a>> =
        std::collections::HashMap::new();

    for (pc, instruction) in instructions.iter().enumerate() {
        match instruction {
            // Closure loads a known function into a register
            Instruction::Closure { dst, func } => {
                reg_func.insert(dst.0, func.0);
            }
            // SetLocal: propagate function index from register to local
            Instruction::SetLocal { idx, src } => {
                if let Some(&func_idx) = reg_func.get(&src.0) {
                    local_func.insert(idx.index(), func_idx);
                } else {
                    // Local now holds unknown value
                    local_func.remove(&idx.index());
                }
            }
            // GetLocal: propagate function index from local to register
            Instruction::GetLocal { dst, idx } => {
                if let Some(&func_idx) = local_func.get(&idx.index()) {
                    reg_func.insert(dst.0, func_idx);
                } else {
                    reg_func.remove(&dst.0);
                }
            }
            // Move: propagate register tracking
            Instruction::Move { dst, src } => {
                if let Some(&func_idx) = reg_func.get(&src.0) {
                    reg_func.insert(dst.0, func_idx);
                } else {
                    reg_func.remove(&dst.0);
                }
            }
            // Call: check if callee register has a known function index
            Instruction::Call { func, .. } => {
                if let Some(&func_idx) = reg_func.get(&func.0) {
                    if let Some(&callee) = func_by_index.get(&func_idx) {
                        // Verify all callee instructions are JIT-translatable
                        let callee_instrs = callee.instructions.read();
                        let all_translatable = callee_instrs
                            .iter()
                            .all(|inst| is_supported_baseline_opcode(inst));
                        if all_translatable {
                            result.insert(
                                pc,
                                InlineCandidate {
                                    callee,
                                    function_index: func_idx,
                                },
                            );
                        }
                    }
                }
            }
            // Any instruction that writes to a register invalidates tracking
            _ => {
                // Clear register tracking for any dst register this instruction writes to
                if let Some(dst_reg) = instruction_dst_register(instruction) {
                    reg_func.remove(&dst_reg);
                }
            }
        }
    }

    result
}

/// Extract the destination register index from an instruction, if any.
fn instruction_dst_register(instruction: &Instruction) -> Option<u16> {
    match instruction {
        Instruction::LoadUndefined { dst }
        | Instruction::LoadNull { dst }
        | Instruction::LoadTrue { dst }
        | Instruction::LoadFalse { dst }
        | Instruction::LoadInt8 { dst, .. }
        | Instruction::LoadInt32 { dst, .. }
        | Instruction::LoadConst { dst, .. }
        | Instruction::GetLocal { dst, .. }
        | Instruction::GetUpvalue { dst, .. }
        | Instruction::GetGlobal { dst, .. }
        | Instruction::LoadThis { dst }
        | Instruction::Add { dst, .. }
        | Instruction::Sub { dst, .. }
        | Instruction::Mul { dst, .. }
        | Instruction::Div { dst, .. }
        | Instruction::Mod { dst, .. }
        | Instruction::Neg { dst, .. }
        | Instruction::Inc { dst, .. }
        | Instruction::Dec { dst, .. }
        | Instruction::BitAnd { dst, .. }
        | Instruction::BitOr { dst, .. }
        | Instruction::BitXor { dst, .. }
        | Instruction::BitNot { dst, .. }
        | Instruction::Shl { dst, .. }
        | Instruction::Shr { dst, .. }
        | Instruction::Ushr { dst, .. }
        | Instruction::Not { dst, .. }
        | Instruction::Eq { dst, .. }
        | Instruction::Ne { dst, .. }
        | Instruction::StrictEq { dst, .. }
        | Instruction::StrictNe { dst, .. }
        | Instruction::Lt { dst, .. }
        | Instruction::Le { dst, .. }
        | Instruction::Gt { dst, .. }
        | Instruction::Ge { dst, .. }
        | Instruction::GetPropConst { dst, .. }
        | Instruction::GetProp { dst, .. }
        | Instruction::GetElem { dst, .. }
        | Instruction::GetLocalProp { dst, .. }
        | Instruction::Call { dst, .. }
        | Instruction::CallMethod { dst, .. }
        | Instruction::Construct { dst, .. }
        | Instruction::NewObject { dst }
        | Instruction::NewArray { dst, .. }
        | Instruction::TypeOf { dst, .. }
        | Instruction::TypeOfName { dst, .. }
        | Instruction::Pow { dst, .. }
        | Instruction::DeleteProp { dst, .. }
        | Instruction::InstanceOf { dst, .. }
        | Instruction::In { dst, .. }
        | Instruction::Dup { dst, .. }
        | Instruction::AddInt32 { dst, .. }
        | Instruction::SubInt32 { dst, .. }
        | Instruction::MulInt32 { dst, .. }
        | Instruction::DivInt32 { dst, .. }
        | Instruction::AddNumber { dst, .. }
        | Instruction::SubNumber { dst, .. }
        | Instruction::Closure { dst, .. }
        | Instruction::ToNumber { dst, .. }
        | Instruction::ToString { dst, .. }
        | Instruction::Catch { dst }
        | Instruction::CallWithReceiver { dst, .. }
        | Instruction::CallMethodComputed { dst, .. }
        | Instruction::GetPropQuickened { dst, .. } => Some(dst.0),
        _ => None,
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
                | Instruction::Yield { .. }
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
                if has_helpers {
                    if constants.get(idx.index() as usize).is_none() {
                        return false;
                    }
                } else if resolve_const_bits(constants, *idx).is_none() {
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
            builder
                .ins()
                .store(MemFlags::trusted(), val, locals_ptr, (i * 8) as i32);
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
            builder
                .ins()
                .store(MemFlags::trusted(), val, regs_ptr, (i * 8) as i32);
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
        builder
            .ins()
            .jump(guarded.merge_block, &[BlockArg::Value(result)]);
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
    let mono_call = builder
        .ins()
        .call(mono_ref, &[obj_val, shape_const, offset_const]);
    let mono_result = builder.inst_results(mono_call)[0];

    let sentinel = builder.ins().iconst(types::I64, BAILOUT_SENTINEL);
    let mono_bail = builder.ins().icmp(IntCC::Equal, mono_result, sentinel);
    let slow_block = builder.create_block();
    let mono_ok = builder.create_block();
    builder.ins().brif(mono_bail, slow_block, &[], mono_ok, &[]);

    // Mono hit → merge
    builder.switch_to_block(mono_ok);
    builder
        .ins()
        .jump(merge_block, &[BlockArg::Value(mono_result)]);

    // Slow path: full GetPropConst
    builder.switch_to_block(slow_block);
    let name_idx_val = builder.ins().iconst(types::I64, name_index as i64);
    let ic_idx_val = builder.ins().iconst(types::I64, ic_index as i64);
    let full_call = builder
        .ins()
        .call(full_ref, &[ctx_ptr, obj_val, name_idx_val, ic_idx_val]);
    let full_result = builder.inst_results(full_call)[0];

    let full_bail = builder.ins().icmp(IntCC::Equal, full_result, sentinel);
    let bail_block = builder.create_block();
    let full_ok = builder.create_block();
    builder.ins().brif(full_bail, bail_block, &[], full_ok, &[]);

    builder.switch_to_block(bail_block);
    emit_bailout_return(builder);

    builder.switch_to_block(full_ok);
    builder
        .ins()
        .jump(merge_block, &[BlockArg::Value(full_result)]);

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
    translate_function_with_constants(builder, function, &[], None, &[])
}

/// Translate a bytecode function into Cranelift IR with constant pool access.
pub fn translate_function_with_constants(
    builder: &mut FunctionBuilder<'_>,
    function: &Function,
    constants: &[Constant],
    helpers: Option<&HelperRefs>,
    module_functions: &[(u32, Function)],
) -> Result<(), JitError> {
    // Hold a reference to the instructions for the entire function.
    let instructions_ref = function.instructions.read();
    let instruction_count = instructions_ref.len();
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
    // --- Loop versioning: detect qualified loops and create optimized blocks ---
    let versioned_loops = loop_analysis::detect_loops(&instructions_ref, &feedback_snapshot);

    // For each qualified loop, create:
    // - A pre-header block (type checks → branch to opt or guard path)
    // - Optimized blocks for each PC in the loop body (one per instruction)
    struct VersionedLoop {
        header_pc: usize,
        back_edge_pc: usize,
        pre_header: cranelift_codegen::ir::Block,
        /// Optimized blocks indexed by (body_pc - header_pc)
        opt_blocks: Vec<cranelift_codegen::ir::Block>,
        check_registers: Vec<u16>,
    }

    let mut versioned: Vec<VersionedLoop> = Vec::new();
    // Map header_pc → index in `versioned` for redirecting loop entries
    let mut header_to_preheader: std::collections::HashMap<usize, cranelift_codegen::ir::Block> =
        std::collections::HashMap::new();

    for info in &versioned_loops {
        if !info.qualifies {
            continue;
        }
        let pre_header = builder.create_block();
        let body_len = info.back_edge_pc - info.header_pc + 1;
        let mut opt_blocks = Vec::with_capacity(body_len);
        for _ in 0..body_len {
            opt_blocks.push(builder.create_block());
        }
        header_to_preheader.insert(info.header_pc, pre_header);
        versioned.push(VersionedLoop {
            header_pc: info.header_pc,
            back_edge_pc: info.back_edge_pc,
            pre_header,
            opt_blocks,
            check_registers: info.check_registers.clone(),
        });
    }

    // Helper: resolve a jump target, redirecting loop headers to pre-headers
    // when the jump originates from outside the loop.
    let resolve_target = |source_pc: usize, target_pc: usize| -> cranelift_codegen::ir::Block {
        if let Some(&pre_header) = header_to_preheader.get(&target_pc) {
            // Only redirect if the source is outside this loop
            let is_inside = versioned.iter().any(|vl| {
                vl.header_pc == target_pc
                    && source_pc >= vl.header_pc
                    && source_pc <= vl.back_edge_pc
            });
            if !is_inside {
                return pre_header;
            }
        }
        blocks[target_pc]
    };

    // --- Function inlining: resolve static call targets ---
    let inline_sites = resolve_inline_candidates(&instructions_ref, module_functions);

    // --- OSR entry dispatch ---
    // Collect ALL loop headers as valid OSR targets (not just qualifying ones).
    // Qualifying loops get routed through pre-headers for type guard checks;
    // non-qualifying loops jump directly to blocks[header_pc].
    let osr_loop_headers: Vec<usize> = versioned_loops.iter().map(|info| info.header_pc).collect();

    if osr_loop_headers.is_empty() {
        // No qualifying loops → always normal entry. Skip OSR dispatch entirely
        // (ctx_ptr may be null in unit tests).
        builder.ins().jump(blocks[0], &[]);
    } else {
        // Read osr_entry_pc from JitContext. If < 0 → normal entry.
        // If >= 0 → OSR: load locals/regs from deopt buffers and jump to loop header.
        let osr_pc_val = builder.ins().load(
            types::I64,
            MemFlags::trusted(),
            ctx_ptr,
            JIT_CTX_OSR_ENTRY_PC_OFFSET,
        );
        let zero_i64 = builder.ins().iconst(types::I64, 0);
        let is_normal_entry = builder
            .ins()
            .icmp(IntCC::SignedLessThan, osr_pc_val, zero_i64);

        let normal_entry_block = builder.create_block();
        let osr_entry_block = builder.create_block();

        builder.ins().brif(
            is_normal_entry,
            normal_entry_block,
            &[],
            osr_entry_block,
            &[],
        );

        // Normal entry: jump to blocks[0] as before.
        builder.switch_to_block(normal_entry_block);
        builder.ins().jump(blocks[0], &[]);

        // OSR entry: load full frame state from deopt buffers, dispatch to loop header.
        builder.switch_to_block(osr_entry_block);

        // Load locals from deopt_locals buffer.
        let locals_ptr = builder.ins().load(
            types::I64,
            MemFlags::trusted(),
            ctx_ptr,
            JIT_CTX_DEOPT_LOCALS_PTR_OFFSET,
        );
        for i in 0..local_count {
            let val =
                builder
                    .ins()
                    .load(types::I64, MemFlags::trusted(), locals_ptr, (i * 8) as i32);
            builder.ins().stack_store(val, local_slots[i], 0);
        }

        // Load registers from deopt_regs buffer.
        let regs_ptr = builder.ins().load(
            types::I64,
            MemFlags::trusted(),
            ctx_ptr,
            JIT_CTX_DEOPT_REGS_PTR_OFFSET,
        );
        for i in 0..reg_count {
            let val = builder
                .ins()
                .load(types::I64, MemFlags::trusted(), regs_ptr, (i * 8) as i32);
            builder.ins().stack_store(val, reg_slots[i], 0);
        }

        // Dispatch to the correct loop header via comparisons.
        // OSR uses resolve_target to go through pre-headers for versioned loops.
        // Use usize::MAX as a pseudo "outside" source_pc so resolve_target routes
        // through the pre-header (type guard checks) like any external-to-loop jump.
        let osr_source_pc = usize::MAX;
        let mut remaining_headers = &osr_loop_headers[..];
        while let Some((&header_pc, rest)) = remaining_headers.split_first() {
            let target_block = resolve_target(osr_source_pc, header_pc);
            let header_const = builder.ins().iconst(types::I64, header_pc as i64);
            let is_match = builder.ins().icmp(IntCC::Equal, osr_pc_val, header_const);
            if rest.is_empty() {
                // Last header: if match jump there, otherwise bailout.
                let match_block = builder.create_block();
                let fallback_block = builder.create_block();
                builder
                    .ins()
                    .brif(is_match, match_block, &[], fallback_block, &[]);
                builder.switch_to_block(match_block);
                builder.ins().jump(target_block, &[]);
                builder.switch_to_block(fallback_block);
                // Invalid OSR target → bailout.
                let sentinel = builder.ins().iconst(types::I64, BAILOUT_SENTINEL);
                builder.ins().return_(&[sentinel]);
            } else {
                // More headers to check: if match jump, else continue.
                let match_block = builder.create_block();
                let next_check = builder.create_block();
                builder
                    .ins()
                    .brif(is_match, match_block, &[], next_check, &[]);
                builder.switch_to_block(match_block);
                builder.ins().jump(target_block, &[]);
                builder.switch_to_block(next_check);
            }
            remaining_headers = rest;
        }
    }

    for pc in 0..instruction_count {
        let instruction = &instructions_ref[pc];
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
                if let Some(bits) = resolve_const_bits(constants, *idx) {
                    let v = builder.ins().iconst(types::I64, bits);
                    write_reg(builder, &reg_slots, *dst, v);
                } else {
                    let helper_ref = helpers
                        .and_then(|h| h.get(HelperKind::LoadConst))
                        .ok_or_else(|| unsupported(pc, instruction))?;
                    let idx_val = builder.ins().iconst(types::I64, i64::from(idx.index()));
                    let result =
                        emit_helper_call_with_bailout(builder, helper_ref, &[ctx_ptr, idx_val]);
                    write_reg(builder, &reg_slots, *dst, result);
                }
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
                    builder,
                    guarded,
                    generic_ref,
                    &[ctx_ptr, left, right],
                    ctx_ptr,
                    pc,
                    BailoutReason::HelperReturnedSentinel,
                    &local_slots,
                    &reg_slots,
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
                    builder,
                    guarded,
                    generic_ref,
                    &[ctx_ptr, left, right],
                    ctx_ptr,
                    pc,
                    BailoutReason::HelperReturnedSentinel,
                    &local_slots,
                    &reg_slots,
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
                    builder,
                    guarded,
                    generic_ref,
                    &[ctx_ptr, left, right],
                    ctx_ptr,
                    pc,
                    BailoutReason::HelperReturnedSentinel,
                    &local_slots,
                    &reg_slots,
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
                    builder,
                    guarded,
                    generic_ref,
                    &[ctx_ptr, left, right],
                    ctx_ptr,
                    pc,
                    BailoutReason::HelperReturnedSentinel,
                    &local_slots,
                    &reg_slots,
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
                    builder,
                    guarded,
                    generic_ref,
                    &[ctx_ptr, left, right],
                    ctx_ptr,
                    pc,
                    BailoutReason::HelperReturnedSentinel,
                    &local_slots,
                    &reg_slots,
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
                    builder,
                    guarded,
                    generic_ref,
                    &[ctx_ptr, left, right],
                    ctx_ptr,
                    pc,
                    BailoutReason::HelperReturnedSentinel,
                    &local_slots,
                    &reg_slots,
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
                    builder,
                    guarded,
                    generic_ref,
                    &[ctx_ptr, left, right],
                    ctx_ptr,
                    pc,
                    BailoutReason::HelperReturnedSentinel,
                    &local_slots,
                    &reg_slots,
                );
                write_reg(builder, &reg_slots, *dst, out);
            }
            Instruction::DivInt32 { dst, lhs, rhs, .. } => {
                let left = read_reg(builder, &reg_slots, *lhs);
                let right = read_reg(builder, &reg_slots, *rhs);
                let guarded = type_guards::emit_guarded_i32_div(builder, left, right);
                let generic_ref = helpers.and_then(|h| h.get(HelperKind::GenericDiv));
                let out = lower_guarded_with_generic_fallback(
                    builder,
                    guarded,
                    generic_ref,
                    &[ctx_ptr, left, right],
                    ctx_ptr,
                    pc,
                    BailoutReason::HelperReturnedSentinel,
                    &local_slots,
                    &reg_slots,
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
                    builder,
                    guarded,
                    generic_ref,
                    &[ctx_ptr, left, right],
                    ctx_ptr,
                    pc,
                    BailoutReason::HelperReturnedSentinel,
                    &local_slots,
                    &reg_slots,
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
                    builder,
                    guarded,
                    generic_ref,
                    &[ctx_ptr, left, right],
                    ctx_ptr,
                    pc,
                    BailoutReason::HelperReturnedSentinel,
                    &local_slots,
                    &reg_slots,
                );
                write_reg(builder, &reg_slots, *dst, out);
            }
            Instruction::Mod { dst, lhs, rhs } => {
                let left = read_reg(builder, &reg_slots, *lhs);
                let right = read_reg(builder, &reg_slots, *rhs);
                let guarded = type_guards::emit_guarded_i32_mod(builder, left, right);
                let generic_ref = helpers.and_then(|h| h.get(HelperKind::GenericMod));
                let out = lower_guarded_with_generic_fallback(
                    builder,
                    guarded,
                    generic_ref,
                    &[ctx_ptr, left, right],
                    ctx_ptr,
                    pc,
                    BailoutReason::HelperReturnedSentinel,
                    &local_slots,
                    &reg_slots,
                );
                write_reg(builder, &reg_slots, *dst, out);
            }
            Instruction::Neg { dst, src } => {
                let val = read_reg(builder, &reg_slots, *src);
                let guarded = type_guards::emit_guarded_i32_neg(builder, val);
                let generic_ref = helpers.and_then(|h| h.get(HelperKind::GenericNeg));
                let out = lower_guarded_with_generic_fallback(
                    builder,
                    guarded,
                    generic_ref,
                    &[ctx_ptr, val],
                    ctx_ptr,
                    pc,
                    BailoutReason::HelperReturnedSentinel,
                    &local_slots,
                    &reg_slots,
                );
                write_reg(builder, &reg_slots, *dst, out);
            }
            Instruction::Inc { dst, src } => {
                let val = read_reg(builder, &reg_slots, *src);
                let guarded = type_guards::emit_guarded_i32_inc(builder, val);
                let generic_ref = helpers.and_then(|h| h.get(HelperKind::GenericInc));
                let out = lower_guarded_with_generic_fallback(
                    builder,
                    guarded,
                    generic_ref,
                    &[ctx_ptr, val],
                    ctx_ptr,
                    pc,
                    BailoutReason::HelperReturnedSentinel,
                    &local_slots,
                    &reg_slots,
                );
                write_reg(builder, &reg_slots, *dst, out);
            }
            Instruction::Dec { dst, src } => {
                let val = read_reg(builder, &reg_slots, *src);
                let guarded = type_guards::emit_guarded_i32_dec(builder, val);
                let generic_ref = helpers.and_then(|h| h.get(HelperKind::GenericDec));
                let out = lower_guarded_with_generic_fallback(
                    builder,
                    guarded,
                    generic_ref,
                    &[ctx_ptr, val],
                    ctx_ptr,
                    pc,
                    BailoutReason::HelperReturnedSentinel,
                    &local_slots,
                    &reg_slots,
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
                    builder,
                    guarded,
                    generic_ref,
                    &[ctx_ptr, left, right, op_id],
                    ctx_ptr,
                    pc,
                    BailoutReason::HelperReturnedSentinel,
                    &local_slots,
                    &reg_slots,
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
                    builder,
                    guarded,
                    generic_ref,
                    &[ctx_ptr, left, right, op_id],
                    ctx_ptr,
                    pc,
                    BailoutReason::HelperReturnedSentinel,
                    &local_slots,
                    &reg_slots,
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
                    builder,
                    guarded,
                    generic_ref,
                    &[ctx_ptr, left, right, op_id],
                    ctx_ptr,
                    pc,
                    BailoutReason::HelperReturnedSentinel,
                    &local_slots,
                    &reg_slots,
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
                    builder,
                    guarded,
                    generic_ref,
                    &[ctx_ptr, left, right, op_id],
                    ctx_ptr,
                    pc,
                    BailoutReason::HelperReturnedSentinel,
                    &local_slots,
                    &reg_slots,
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
                    builder,
                    guarded,
                    generic_ref,
                    &[ctx_ptr, left, right, op_id],
                    ctx_ptr,
                    pc,
                    BailoutReason::HelperReturnedSentinel,
                    &local_slots,
                    &reg_slots,
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
                    builder,
                    guarded,
                    generic_ref,
                    &[ctx_ptr, left, right, op_id],
                    ctx_ptr,
                    pc,
                    BailoutReason::HelperReturnedSentinel,
                    &local_slots,
                    &reg_slots,
                );
                write_reg(builder, &reg_slots, *dst, out);
            }
            Instruction::BitNot { dst, src } => {
                let val = read_reg(builder, &reg_slots, *src);
                let guarded = type_guards::emit_guarded_i32_bitnot(builder, val);
                let generic_ref = helpers.and_then(|h| h.get(HelperKind::GenericBitNot));
                let out = lower_guarded_with_generic_fallback(
                    builder,
                    guarded,
                    generic_ref,
                    &[ctx_ptr, val],
                    ctx_ptr,
                    pc,
                    BailoutReason::HelperReturnedSentinel,
                    &local_slots,
                    &reg_slots,
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
                    builder,
                    guarded,
                    generic_ref,
                    &[ctx_ptr, left, right],
                    ctx_ptr,
                    pc,
                    BailoutReason::HelperReturnedSentinel,
                    &local_slots,
                    &reg_slots,
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
                    builder,
                    guarded,
                    generic_ref,
                    &[ctx_ptr, left, right],
                    ctx_ptr,
                    pc,
                    BailoutReason::HelperReturnedSentinel,
                    &local_slots,
                    &reg_slots,
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
                    builder,
                    guarded,
                    generic_ref,
                    &[ctx_ptr, left, right],
                    ctx_ptr,
                    pc,
                    BailoutReason::HelperReturnedSentinel,
                    &local_slots,
                    &reg_slots,
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
                    builder,
                    guarded,
                    generic_ref,
                    &[ctx_ptr, left, right],
                    ctx_ptr,
                    pc,
                    BailoutReason::HelperReturnedSentinel,
                    &local_slots,
                    &reg_slots,
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
                    builder,
                    guarded,
                    generic_ref,
                    &[ctx_ptr, left, right],
                    ctx_ptr,
                    pc,
                    BailoutReason::HelperReturnedSentinel,
                    &local_slots,
                    &reg_slots,
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
                    builder,
                    guarded,
                    generic_ref,
                    &[ctx_ptr, left, right],
                    ctx_ptr,
                    pc,
                    BailoutReason::HelperReturnedSentinel,
                    &local_slots,
                    &reg_slots,
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
                let target_block = resolve_target(pc, target);
                builder.ins().jump(target_block, &[]);
                continue;
            }
            Instruction::JumpIfTrue { cond, offset } => {
                let cond_val = read_reg(builder, &reg_slots, *cond);
                let truthy = type_guards::emit_is_truthy(builder, cond_val);
                let is_truthy = builder.ins().icmp_imm(IntCC::NotEqual, truthy, 0);
                let jump_to = jump_target(pc, offset.offset(), instruction_count)?;
                let jump_block = resolve_target(pc, jump_to);
                let fallthrough = pc + 1;
                if fallthrough < instruction_count {
                    let ft_block = resolve_target(pc, fallthrough);
                    builder
                        .ins()
                        .brif(is_truthy, jump_block, &[], ft_block, &[]);
                } else {
                    builder.ins().brif(is_truthy, jump_block, &[], exit, &[]);
                }
                continue;
            }
            Instruction::JumpIfFalse { cond, offset } => {
                let cond_val = read_reg(builder, &reg_slots, *cond);
                let truthy = type_guards::emit_is_truthy(builder, cond_val);
                let is_truthy = builder.ins().icmp_imm(IntCC::NotEqual, truthy, 0);
                let jump_to = jump_target(pc, offset.offset(), instruction_count)?;
                let jump_block = resolve_target(pc, jump_to);
                let fallthrough = pc + 1;
                if fallthrough < instruction_count {
                    let ft_block = resolve_target(pc, fallthrough);
                    builder
                        .ins()
                        .brif(is_truthy, ft_block, &[], jump_block, &[]);
                } else {
                    builder.ins().brif(is_truthy, exit, &[], jump_block, &[]);
                }
                continue;
            }
            Instruction::JumpIfNullish { src, offset } => {
                let src_val = read_reg(builder, &reg_slots, *src);
                let is_nullish = type_guards::emit_is_nullish(builder, src_val);
                let jump_to = jump_target(pc, offset.offset(), instruction_count)?;
                let jump_block = resolve_target(pc, jump_to);
                let fallthrough = pc + 1;
                if fallthrough < instruction_count {
                    let ft_block = resolve_target(pc, fallthrough);
                    builder
                        .ins()
                        .brif(is_nullish, jump_block, &[], ft_block, &[]);
                } else {
                    builder.ins().brif(is_nullish, jump_block, &[], exit, &[]);
                }
                continue;
            }
            Instruction::JumpIfNotNullish { src, offset } => {
                let src_val = read_reg(builder, &reg_slots, *src);
                let is_nullish = type_guards::emit_is_nullish(builder, src_val);
                let jump_to = jump_target(pc, offset.offset(), instruction_count)?;
                let jump_block = resolve_target(pc, jump_to);
                let fallthrough = pc + 1;
                if fallthrough < instruction_count {
                    let ft_block = resolve_target(pc, fallthrough);
                    builder
                        .ins()
                        .brif(is_nullish, ft_block, &[], jump_block, &[]);
                } else {
                    builder.ins().brif(is_nullish, exit, &[], jump_block, &[]);
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

                let result =
                    if let (Some((shape_id, offset)), Some(mono_helper)) = (mono_ic, mono_ref) {
                        emit_mono_prop_with_fallback(
                            builder,
                            mono_helper,
                            full_ref,
                            obj_val,
                            ctx_ptr,
                            shape_id,
                            offset,
                            name.index(),
                            *ic_index,
                        )
                    } else {
                        let name_idx = builder.ins().iconst(types::I64, name.index() as i64);
                        let ic_idx = builder.ins().iconst(types::I64, *ic_index as i64);
                        emit_helper_call_with_bailout(
                            builder,
                            full_ref,
                            &[ctx_ptr, obj_val, name_idx, ic_idx],
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

                let result =
                    if let (Some((shape_id, offset)), Some(mono_helper)) = (mono_ic, mono_ref) {
                        emit_mono_prop_with_fallback(
                            builder,
                            mono_helper,
                            full_ref,
                            obj_val,
                            ctx_ptr,
                            shape_id,
                            offset,
                            name.index(),
                            *ic_index,
                        )
                    } else {
                        let name_idx = builder.ins().iconst(types::I64, name.index() as i64);
                        let ic_idx = builder.ins().iconst(types::I64, *ic_index as i64);
                        emit_helper_call_with_bailout(
                            builder,
                            full_ref,
                            &[ctx_ptr, obj_val, name_idx, ic_idx],
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
                // Check if this call site has a statically resolved inline candidate
                if let Some(candidate) = inline_sites.get(&pc) {
                    let callee = candidate.callee;
                    let callee_instrs = callee.instructions.read();
                    let callee_instr_count = callee_instrs.len();

                    if callee_instr_count == 0 {
                        // Empty function → return undefined
                        let undef_val =
                            builder.ins().iconst(types::I64, type_guards::TAG_UNDEFINED);
                        write_reg(builder, &reg_slots, *dst, undef_val);
                    } else {
                        // Create callee register and local slots
                        let callee_reg_count = callee.register_count as usize;
                        let mut callee_reg_slots = Vec::with_capacity(callee_reg_count);
                        for _ in 0..callee_reg_count {
                            callee_reg_slots.push(builder.create_sized_stack_slot(
                                StackSlotData::new(StackSlotKind::ExplicitSlot, 8, 8),
                            ));
                        }
                        let callee_local_count = callee.local_count as usize;
                        let mut callee_local_slots = Vec::with_capacity(callee_local_count);
                        for _ in 0..callee_local_count {
                            callee_local_slots.push(builder.create_sized_stack_slot(
                                StackSlotData::new(StackSlotKind::ExplicitSlot, 8, 8),
                            ));
                        }

                        // Initialize callee registers to undefined
                        let undef_val =
                            builder.ins().iconst(types::I64, type_guards::TAG_UNDEFINED);
                        for idx in 0..callee_reg_count {
                            builder
                                .ins()
                                .stack_store(undef_val, callee_reg_slots[idx], 0);
                        }

                        // Map caller args → callee param locals
                        let callee_param_count = callee.param_count as usize;
                        for idx in 0..callee_local_count {
                            let init = if idx < callee_param_count && idx < (*argc as usize) {
                                // Read argument from caller's register layout
                                // Args are in registers func.0+1, func.0+2, ...
                                read_reg(builder, &reg_slots, Register(func.0 + 1 + idx as u16))
                            } else {
                                undef_val
                            };
                            builder.ins().stack_store(init, callee_local_slots[idx], 0);
                        }

                        // Create blocks for callee instructions + continuation
                        let mut callee_blocks = Vec::with_capacity(callee_instr_count);
                        for _ in 0..callee_instr_count {
                            callee_blocks.push(builder.create_block());
                        }
                        let continuation = builder.create_block();
                        builder.append_block_param(continuation, types::I64);

                        // Jump to first callee block
                        builder.ins().jump(callee_blocks[0], &[]);

                        // Translate callee bytecode using callee's slots
                        for (ci, callee_inst) in callee_instrs.iter().enumerate() {
                            builder.switch_to_block(callee_blocks[ci]);

                            match callee_inst {
                                // Returns → jump to continuation with value
                                Instruction::Return { src } => {
                                    let out = read_reg(builder, &callee_reg_slots, *src);
                                    builder.ins().jump(continuation, &[BlockArg::Value(out)]);
                                    continue;
                                }
                                Instruction::ReturnUndefined => {
                                    let undef_ret = builder
                                        .ins()
                                        .iconst(types::I64, type_guards::TAG_UNDEFINED);
                                    builder
                                        .ins()
                                        .jump(continuation, &[BlockArg::Value(undef_ret)]);
                                    continue;
                                }
                                // Constants
                                Instruction::LoadUndefined { dst: d } => {
                                    let v = builder
                                        .ins()
                                        .iconst(types::I64, type_guards::TAG_UNDEFINED);
                                    write_reg(builder, &callee_reg_slots, *d, v);
                                }
                                Instruction::LoadNull { dst: d } => {
                                    let v = builder.ins().iconst(types::I64, type_guards::TAG_NULL);
                                    write_reg(builder, &callee_reg_slots, *d, v);
                                }
                                Instruction::LoadTrue { dst: d } => {
                                    let v = builder.ins().iconst(types::I64, type_guards::TAG_TRUE);
                                    write_reg(builder, &callee_reg_slots, *d, v);
                                }
                                Instruction::LoadFalse { dst: d } => {
                                    let v =
                                        builder.ins().iconst(types::I64, type_guards::TAG_FALSE);
                                    write_reg(builder, &callee_reg_slots, *d, v);
                                }
                                Instruction::LoadInt8 { dst: d, value } => {
                                    let v = type_guards::emit_box_int32_const(
                                        builder,
                                        i32::from(*value),
                                    );
                                    write_reg(builder, &callee_reg_slots, *d, v);
                                }
                                Instruction::LoadInt32 { dst: d, value } => {
                                    let v = type_guards::emit_box_int32_const(builder, *value);
                                    write_reg(builder, &callee_reg_slots, *d, v);
                                }
                                Instruction::LoadConst { dst: d, idx } => {
                                    if let Some(bits) = resolve_const_bits(constants, *idx) {
                                        let v = builder.ins().iconst(types::I64, bits);
                                        write_reg(builder, &callee_reg_slots, *d, v);
                                    } else {
                                        // Can't resolve constant — bail out to runtime call
                                        // Fall through to continuation with undefined
                                        let undef_ret = builder
                                            .ins()
                                            .iconst(types::I64, type_guards::TAG_UNDEFINED);
                                        builder
                                            .ins()
                                            .jump(continuation, &[BlockArg::Value(undef_ret)]);
                                        continue;
                                    }
                                }
                                // Variables (use callee's slots)
                                Instruction::GetLocal { dst: d, idx } => {
                                    let v = read_local(builder, &callee_local_slots, *idx);
                                    write_reg(builder, &callee_reg_slots, *d, v);
                                }
                                Instruction::SetLocal { idx, src } => {
                                    let v = read_reg(builder, &callee_reg_slots, *src);
                                    write_local(builder, &callee_local_slots, *idx, v);
                                }
                                Instruction::Move { dst: d, src } => {
                                    let v = read_reg(builder, &callee_reg_slots, *src);
                                    write_reg(builder, &callee_reg_slots, *d, v);
                                }
                                // Arithmetic (guarded, using callee's slots)
                                Instruction::Add {
                                    dst: d,
                                    lhs,
                                    rhs,
                                    feedback_index,
                                }
                                | Instruction::AddInt32 {
                                    dst: d,
                                    lhs,
                                    rhs,
                                    feedback_index,
                                } => {
                                    let left = read_reg(builder, &callee_reg_slots, *lhs);
                                    let right = read_reg(builder, &callee_reg_slots, *rhs);
                                    let callee_feedback = callee.feedback_vector.read();
                                    let hint = SpecializationHint::from_type_flags(
                                        callee_feedback
                                            .get(*feedback_index as usize)
                                            .map(|m| &m.type_observations),
                                    );
                                    let guarded = type_guards::emit_specialized_arith(
                                        builder,
                                        ArithOp::Add,
                                        left,
                                        right,
                                        hint,
                                    );
                                    let generic_ref =
                                        helpers.and_then(|h| h.get(HelperKind::GenericAdd));
                                    let out = lower_guarded_with_generic_fallback(
                                        builder,
                                        guarded,
                                        generic_ref,
                                        &[ctx_ptr, left, right],
                                        ctx_ptr,
                                        pc,
                                        BailoutReason::HelperReturnedSentinel,
                                        &local_slots,
                                        &reg_slots,
                                    );
                                    write_reg(builder, &callee_reg_slots, *d, out);
                                }
                                Instruction::Sub {
                                    dst: d,
                                    lhs,
                                    rhs,
                                    feedback_index,
                                }
                                | Instruction::SubInt32 {
                                    dst: d,
                                    lhs,
                                    rhs,
                                    feedback_index,
                                } => {
                                    let left = read_reg(builder, &callee_reg_slots, *lhs);
                                    let right = read_reg(builder, &callee_reg_slots, *rhs);
                                    let callee_feedback = callee.feedback_vector.read();
                                    let hint = SpecializationHint::from_type_flags(
                                        callee_feedback
                                            .get(*feedback_index as usize)
                                            .map(|m| &m.type_observations),
                                    );
                                    let guarded = type_guards::emit_specialized_arith(
                                        builder,
                                        ArithOp::Sub,
                                        left,
                                        right,
                                        hint,
                                    );
                                    let generic_ref =
                                        helpers.and_then(|h| h.get(HelperKind::GenericSub));
                                    let out = lower_guarded_with_generic_fallback(
                                        builder,
                                        guarded,
                                        generic_ref,
                                        &[ctx_ptr, left, right],
                                        ctx_ptr,
                                        pc,
                                        BailoutReason::HelperReturnedSentinel,
                                        &local_slots,
                                        &reg_slots,
                                    );
                                    write_reg(builder, &callee_reg_slots, *d, out);
                                }
                                Instruction::Mul {
                                    dst: d,
                                    lhs,
                                    rhs,
                                    feedback_index,
                                }
                                | Instruction::MulInt32 {
                                    dst: d,
                                    lhs,
                                    rhs,
                                    feedback_index,
                                } => {
                                    let left = read_reg(builder, &callee_reg_slots, *lhs);
                                    let right = read_reg(builder, &callee_reg_slots, *rhs);
                                    let callee_feedback = callee.feedback_vector.read();
                                    let hint = SpecializationHint::from_type_flags(
                                        callee_feedback
                                            .get(*feedback_index as usize)
                                            .map(|m| &m.type_observations),
                                    );
                                    let guarded = type_guards::emit_specialized_arith(
                                        builder,
                                        ArithOp::Mul,
                                        left,
                                        right,
                                        hint,
                                    );
                                    let generic_ref =
                                        helpers.and_then(|h| h.get(HelperKind::GenericMul));
                                    let out = lower_guarded_with_generic_fallback(
                                        builder,
                                        guarded,
                                        generic_ref,
                                        &[ctx_ptr, left, right],
                                        ctx_ptr,
                                        pc,
                                        BailoutReason::HelperReturnedSentinel,
                                        &local_slots,
                                        &reg_slots,
                                    );
                                    write_reg(builder, &callee_reg_slots, *d, out);
                                }
                                // Inc/Dec
                                Instruction::Inc { dst: d, src } => {
                                    let val = read_reg(builder, &callee_reg_slots, *src);
                                    let guarded = type_guards::emit_guarded_i32_inc(builder, val);
                                    let generic_ref =
                                        helpers.and_then(|h| h.get(HelperKind::GenericInc));
                                    let out = lower_guarded_with_generic_fallback(
                                        builder,
                                        guarded,
                                        generic_ref,
                                        &[ctx_ptr, val],
                                        ctx_ptr,
                                        pc,
                                        BailoutReason::HelperReturnedSentinel,
                                        &local_slots,
                                        &reg_slots,
                                    );
                                    write_reg(builder, &callee_reg_slots, *d, out);
                                }
                                Instruction::Dec { dst: d, src } => {
                                    let val = read_reg(builder, &callee_reg_slots, *src);
                                    let guarded = type_guards::emit_guarded_i32_dec(builder, val);
                                    let generic_ref =
                                        helpers.and_then(|h| h.get(HelperKind::GenericDec));
                                    let out = lower_guarded_with_generic_fallback(
                                        builder,
                                        guarded,
                                        generic_ref,
                                        &[ctx_ptr, val],
                                        ctx_ptr,
                                        pc,
                                        BailoutReason::HelperReturnedSentinel,
                                        &local_slots,
                                        &reg_slots,
                                    );
                                    write_reg(builder, &callee_reg_slots, *d, out);
                                }
                                // Comparisons
                                Instruction::Lt { dst: d, lhs, rhs } => {
                                    let left = read_reg(builder, &callee_reg_slots, *lhs);
                                    let right = read_reg(builder, &callee_reg_slots, *rhs);
                                    let guarded = type_guards::emit_guarded_numeric_cmp(
                                        builder,
                                        IntCC::SignedLessThan,
                                        FloatCC::LessThan,
                                        left,
                                        right,
                                    );
                                    let generic_ref =
                                        helpers.and_then(|h| h.get(HelperKind::GenericLt));
                                    let out = lower_guarded_with_generic_fallback(
                                        builder,
                                        guarded,
                                        generic_ref,
                                        &[ctx_ptr, left, right],
                                        ctx_ptr,
                                        pc,
                                        BailoutReason::HelperReturnedSentinel,
                                        &local_slots,
                                        &reg_slots,
                                    );
                                    write_reg(builder, &callee_reg_slots, *d, out);
                                }
                                Instruction::Le { dst: d, lhs, rhs } => {
                                    let left = read_reg(builder, &callee_reg_slots, *lhs);
                                    let right = read_reg(builder, &callee_reg_slots, *rhs);
                                    let guarded = type_guards::emit_guarded_numeric_cmp(
                                        builder,
                                        IntCC::SignedLessThanOrEqual,
                                        FloatCC::LessThanOrEqual,
                                        left,
                                        right,
                                    );
                                    let generic_ref =
                                        helpers.and_then(|h| h.get(HelperKind::GenericLe));
                                    let out = lower_guarded_with_generic_fallback(
                                        builder,
                                        guarded,
                                        generic_ref,
                                        &[ctx_ptr, left, right],
                                        ctx_ptr,
                                        pc,
                                        BailoutReason::HelperReturnedSentinel,
                                        &local_slots,
                                        &reg_slots,
                                    );
                                    write_reg(builder, &callee_reg_slots, *d, out);
                                }
                                Instruction::Gt { dst: d, lhs, rhs } => {
                                    let left = read_reg(builder, &callee_reg_slots, *lhs);
                                    let right = read_reg(builder, &callee_reg_slots, *rhs);
                                    let guarded = type_guards::emit_guarded_numeric_cmp(
                                        builder,
                                        IntCC::SignedGreaterThan,
                                        FloatCC::GreaterThan,
                                        left,
                                        right,
                                    );
                                    let generic_ref =
                                        helpers.and_then(|h| h.get(HelperKind::GenericGt));
                                    let out = lower_guarded_with_generic_fallback(
                                        builder,
                                        guarded,
                                        generic_ref,
                                        &[ctx_ptr, left, right],
                                        ctx_ptr,
                                        pc,
                                        BailoutReason::HelperReturnedSentinel,
                                        &local_slots,
                                        &reg_slots,
                                    );
                                    write_reg(builder, &callee_reg_slots, *d, out);
                                }
                                Instruction::Ge { dst: d, lhs, rhs } => {
                                    let left = read_reg(builder, &callee_reg_slots, *lhs);
                                    let right = read_reg(builder, &callee_reg_slots, *rhs);
                                    let guarded = type_guards::emit_guarded_numeric_cmp(
                                        builder,
                                        IntCC::SignedGreaterThanOrEqual,
                                        FloatCC::GreaterThanOrEqual,
                                        left,
                                        right,
                                    );
                                    let generic_ref =
                                        helpers.and_then(|h| h.get(HelperKind::GenericGe));
                                    let out = lower_guarded_with_generic_fallback(
                                        builder,
                                        guarded,
                                        generic_ref,
                                        &[ctx_ptr, left, right],
                                        ctx_ptr,
                                        pc,
                                        BailoutReason::HelperReturnedSentinel,
                                        &local_slots,
                                        &reg_slots,
                                    );
                                    write_reg(builder, &callee_reg_slots, *d, out);
                                }
                                Instruction::StrictEq { dst: d, lhs, rhs } => {
                                    let left = read_reg(builder, &callee_reg_slots, *lhs);
                                    let right = read_reg(builder, &callee_reg_slots, *rhs);
                                    let out =
                                        type_guards::emit_strict_eq(builder, left, right, false);
                                    write_reg(builder, &callee_reg_slots, *d, out);
                                }
                                Instruction::StrictNe { dst: d, lhs, rhs } => {
                                    let left = read_reg(builder, &callee_reg_slots, *lhs);
                                    let right = read_reg(builder, &callee_reg_slots, *rhs);
                                    let out =
                                        type_guards::emit_strict_eq(builder, left, right, true);
                                    write_reg(builder, &callee_reg_slots, *d, out);
                                }
                                Instruction::Not { dst: d, src } => {
                                    let val = read_reg(builder, &callee_reg_slots, *src);
                                    let truthy = type_guards::emit_is_truthy(builder, val);
                                    let is_falsy = builder.ins().icmp_imm(IntCC::Equal, truthy, 0);
                                    let out = type_guards::emit_bool_to_nanbox(builder, is_falsy);
                                    write_reg(builder, &callee_reg_slots, *d, out);
                                }
                                // Jumps within the callee (relative to callee blocks)
                                Instruction::Jump { offset } => {
                                    if let Ok(target) =
                                        jump_target(ci, offset.offset(), callee_instr_count)
                                    {
                                        builder.ins().jump(callee_blocks[target], &[]);
                                    } else {
                                        let undef_ret = builder
                                            .ins()
                                            .iconst(types::I64, type_guards::TAG_UNDEFINED);
                                        builder
                                            .ins()
                                            .jump(continuation, &[BlockArg::Value(undef_ret)]);
                                    }
                                    continue;
                                }
                                Instruction::JumpIfTrue { cond, offset } => {
                                    let cond_val = read_reg(builder, &callee_reg_slots, *cond);
                                    let truthy = type_guards::emit_is_truthy(builder, cond_val);
                                    let is_truthy =
                                        builder.ins().icmp_imm(IntCC::NotEqual, truthy, 0);
                                    if let Ok(target) =
                                        jump_target(ci, offset.offset(), callee_instr_count)
                                    {
                                        let fallthrough = ci + 1;
                                        let ft_block = if fallthrough < callee_instr_count {
                                            callee_blocks[fallthrough]
                                        } else {
                                            // Past end → return undefined via continuation
                                            let exit_block = builder.create_block();
                                            builder.switch_to_block(exit_block);
                                            let undef_ret = builder
                                                .ins()
                                                .iconst(types::I64, type_guards::TAG_UNDEFINED);
                                            builder
                                                .ins()
                                                .jump(continuation, &[BlockArg::Value(undef_ret)]);
                                            // Switch back to emit the branch
                                            builder.switch_to_block(callee_blocks[ci]);
                                            exit_block
                                        };
                                        builder.ins().brif(
                                            is_truthy,
                                            callee_blocks[target],
                                            &[],
                                            ft_block,
                                            &[],
                                        );
                                    } else {
                                        let undef_ret = builder
                                            .ins()
                                            .iconst(types::I64, type_guards::TAG_UNDEFINED);
                                        builder
                                            .ins()
                                            .jump(continuation, &[BlockArg::Value(undef_ret)]);
                                    }
                                    continue;
                                }
                                Instruction::JumpIfFalse { cond, offset } => {
                                    let cond_val = read_reg(builder, &callee_reg_slots, *cond);
                                    let truthy = type_guards::emit_is_truthy(builder, cond_val);
                                    let is_truthy =
                                        builder.ins().icmp_imm(IntCC::NotEqual, truthy, 0);
                                    if let Ok(target) =
                                        jump_target(ci, offset.offset(), callee_instr_count)
                                    {
                                        let fallthrough = ci + 1;
                                        let ft_block = if fallthrough < callee_instr_count {
                                            callee_blocks[fallthrough]
                                        } else {
                                            let exit_block = builder.create_block();
                                            builder.switch_to_block(exit_block);
                                            let undef_ret = builder
                                                .ins()
                                                .iconst(types::I64, type_guards::TAG_UNDEFINED);
                                            builder
                                                .ins()
                                                .jump(continuation, &[BlockArg::Value(undef_ret)]);
                                            builder.switch_to_block(callee_blocks[ci]);
                                            exit_block
                                        };
                                        builder.ins().brif(
                                            is_truthy,
                                            ft_block,
                                            &[],
                                            callee_blocks[target],
                                            &[],
                                        );
                                    } else {
                                        let undef_ret = builder
                                            .ins()
                                            .iconst(types::I64, type_guards::TAG_UNDEFINED);
                                        builder
                                            .ins()
                                            .jump(continuation, &[BlockArg::Value(undef_ret)]);
                                    }
                                    continue;
                                }
                                Instruction::Nop => {}
                                // Unsupported in inlined code → bail out (return undefined)
                                _ => {
                                    let undef_ret = builder
                                        .ins()
                                        .iconst(types::I64, type_guards::TAG_UNDEFINED);
                                    builder
                                        .ins()
                                        .jump(continuation, &[BlockArg::Value(undef_ret)]);
                                    continue;
                                }
                            }

                            // Fallthrough within inlined callee
                            let next_ci = ci + 1;
                            if next_ci < callee_instr_count {
                                builder.ins().jump(callee_blocks[next_ci], &[]);
                            } else {
                                // Past end of callee → implicit return undefined
                                let undef_ret =
                                    builder.ins().iconst(types::I64, type_guards::TAG_UNDEFINED);
                                builder
                                    .ins()
                                    .jump(continuation, &[BlockArg::Value(undef_ret)]);
                            }
                        }

                        // Continuation: read inlined result
                        builder.switch_to_block(continuation);
                        let inline_result = builder.block_params(continuation)[0];
                        write_reg(builder, &reg_slots, *dst, inline_result);
                    }
                } else {
                    // No inline candidate → use runtime helper as before
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
                shape_id,
                offset,
            } => {
                let full_ref = helpers
                    .and_then(|h| h.get(HelperKind::GetPropConst))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let obj_val = read_reg(builder, &reg_slots, *obj);

                let mono_ref = helpers.and_then(|h| h.get(HelperKind::GetPropMono));

                let result = if let Some(mono_helper) = mono_ref {
                    // The macro expects `key` and `ic_index`. Since we skipped the IC lookup
                    // we pass dummy values (0) because `emit_mono_prop_with_fallback`
                    // is an abstraction that requires an `ic_index` to fallback to `GetPropConst`.
                    // Wait! we need `name` and `ic_idx` to fallback. `GetPropQuickened` doesn't have them anymore.
                    // For the JIT, if `GetPropQuickened` misses, we emit a bailout!
                    let bail_block = builder.create_block();
                    let continue_block = builder.create_block();
                    let is_object = type_guards::emit_is_object(builder, obj_val);
                    // Check if object
                    let shape_check_block = builder.create_block();
                    builder
                        .ins()
                        .brif(is_object, shape_check_block, &[], bail_block, &[]);

                    builder.switch_to_block(shape_check_block);
                    // Extract ptr and check shape
                    let obj_ptr = builder.ins().band_imm(obj_val, !type_guards::PTR_MASK);
                    let shape_ptr_addr = builder.ins().iadd_imm(obj_ptr, 16); // shape is offset 16
                    let current_shape =
                        builder
                            .ins()
                            .load(types::I64, MemFlags::new(), shape_ptr_addr, 0);
                    let expected_shape = builder.ins().iconst(types::I64, *shape_id as i64);
                    let shape_match =
                        builder
                            .ins()
                            .icmp(IntCC::Equal, current_shape, expected_shape);

                    let load_block = builder.create_block();
                    builder
                        .ins()
                        .brif(shape_match, load_block, &[], bail_block, &[]);

                    builder.switch_to_block(load_block);
                    let props_ptr_addr = builder.ins().iadd_imm(obj_ptr, 24); // properties is offset 24
                    let props_ptr =
                        builder
                            .ins()
                            .load(types::I64, MemFlags::new(), props_ptr_addr, 0);
                    let val_addr = builder.ins().iadd_imm(props_ptr, (*offset as i64) * 8);
                    let val = builder.ins().load(types::I64, MemFlags::new(), val_addr, 0);
                    builder.ins().jump(continue_block, &[BlockArg::Value(val)]);

                    builder.switch_to_block(bail_block);
                    emit_bailout_return(builder);

                    builder.switch_to_block(continue_block);
                    builder.block_params(continue_block)[0]
                } else {
                    // Fallback bailout
                    emit_bailout_return(builder);
                    builder.ins().iconst(types::I64, type_guards::TAG_UNDEFINED)
                };
                write_reg(builder, &reg_slots, *dst, result);
            }
            Instruction::SetPropQuickened {
                obj,
                val,
                shape_id,
                offset,
            } => {
                let obj_val = read_reg(builder, &reg_slots, *obj);
                let value = read_reg(builder, &reg_slots, *val);

                let bail_block = builder.create_block();
                let continue_block = builder.create_block();
                let is_object = type_guards::emit_is_object(builder, obj_val);

                let shape_check_block = builder.create_block();
                builder
                    .ins()
                    .brif(is_object, shape_check_block, &[], bail_block, &[]);

                builder.switch_to_block(shape_check_block);
                let obj_ptr = builder.ins().band_imm(obj_val, !type_guards::PTR_MASK);
                let shape_ptr_addr = builder.ins().iadd_imm(obj_ptr, 16);
                let current_shape =
                    builder
                        .ins()
                        .load(types::I64, MemFlags::new(), shape_ptr_addr, 0);
                let expected_shape = builder.ins().iconst(types::I64, *shape_id as i64);
                let shape_match = builder
                    .ins()
                    .icmp(IntCC::Equal, current_shape, expected_shape);

                let store_block = builder.create_block();
                builder
                    .ins()
                    .brif(shape_match, store_block, &[], bail_block, &[]);

                builder.switch_to_block(store_block);
                let props_ptr_addr = builder.ins().iadd_imm(obj_ptr, 24);
                let props_ptr = builder
                    .ins()
                    .load(types::I64, MemFlags::new(), props_ptr_addr, 0);
                let val_addr = builder.ins().iadd_imm(props_ptr, (*offset as i64) * 8);
                builder.ins().store(MemFlags::new(), value, val_addr, 0);

                // Note: Object pointers in memory need to trigger GC write barriers!
                // Since this engine is embedding first, it handles write barriers in the allocator. We rely on standard conservative GC.
                builder.ins().jump(continue_block, &[]);

                builder.switch_to_block(bail_block);
                emit_bailout_return(builder);

                builder.switch_to_block(continue_block);
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
                let jump_block = resolve_target(pc, jump_to);
                let fallthrough = pc + 1;
                if fallthrough < instruction_count {
                    let ft_block = resolve_target(pc, fallthrough);
                    builder.ins().brif(is_done, jump_block, &[], ft_block, &[]);
                } else {
                    builder.ins().brif(is_done, jump_block, &[], exit, &[]);
                }
                continue;
            }
        }

        let next_pc = pc + 1;
        if next_pc < instruction_count {
            let ft_block = resolve_target(pc, next_pc);
            builder.ins().jump(ft_block, &[]);
        } else {
            let undef = builder.ins().iconst(types::I64, type_guards::TAG_UNDEFINED);
            builder.ins().return_(&[undef]);
        }
    }

    // --- Emit loop versioning pre-headers and optimized bodies ---
    for vl in &versioned {
        // Pre-header: check that all relevant registers are int32
        builder.switch_to_block(vl.pre_header);

        // Check each register
        let mut all_int32 = None;
        for &reg_idx in &vl.check_registers {
            if (reg_idx as usize) < reg_count {
                let val = builder
                    .ins()
                    .stack_load(types::I64, reg_slots[reg_idx as usize], 0);
                let is_i32 = type_guards::emit_is_int32(builder, val);
                all_int32 = Some(match all_int32 {
                    None => is_i32,
                    Some(prev) => builder.ins().band(prev, is_i32),
                });
            }
        }

        if let Some(check) = all_int32 {
            // Branch: all int32 → optimized, otherwise → guarded
            builder
                .ins()
                .brif(check, vl.opt_blocks[0], &[], blocks[vl.header_pc], &[]);
        } else {
            // No registers to check (shouldn't happen for qualified loops)
            builder.ins().jump(blocks[vl.header_pc], &[]);
        }

        // Emit optimized loop body
        let body_len = vl.back_edge_pc - vl.header_pc + 1;
        for body_idx in 0..body_len {
            let body_pc = vl.header_pc + body_idx;
            let instruction = &instructions_ref[body_pc];
            builder.switch_to_block(vl.opt_blocks[body_idx]);

            match instruction {
                // --- Constant loads (same as guarded) ---
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
                    if let Some(bits) = resolve_const_bits(constants, *idx) {
                        let v = builder.ins().iconst(types::I64, bits);
                        write_reg(builder, &reg_slots, *dst, v);
                    } else {
                        // Can't resolve → fall back to guarded
                        builder.ins().jump(blocks[body_pc], &[]);
                        continue;
                    }
                }
                // --- Variable access (same as guarded) ---
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
                // --- Bare arithmetic (no type guard, overflow only) ---
                Instruction::Add { dst, lhs, rhs, .. }
                | Instruction::AddInt32 { dst, lhs, rhs, .. } => {
                    let left = read_reg(builder, &reg_slots, *lhs);
                    let right = read_reg(builder, &reg_slots, *rhs);
                    let guarded =
                        type_guards::emit_bare_i32_arith(builder, ArithOp::Add, left, right);
                    // On overflow: call generic helper → transfer to guarded path
                    let generic_ref = helpers.and_then(|h| h.get(HelperKind::GenericAdd));
                    builder.switch_to_block(guarded.slow_block);
                    if let Some(helper_ref) = generic_ref {
                        let call = builder.ins().call(helper_ref, &[ctx_ptr, left, right]);
                        let result = builder.inst_results(call)[0];
                        // Write result and transfer to guarded version of next instruction
                        write_reg(builder, &reg_slots, *dst, result);
                        let next_guarded_pc = body_pc + 1;
                        if next_guarded_pc < instruction_count {
                            builder.ins().jump(blocks[next_guarded_pc], &[]);
                        } else {
                            builder.ins().return_(&[result]);
                        }
                    } else {
                        emit_bailout_return_with_state(
                            builder,
                            ctx_ptr,
                            body_pc,
                            BailoutReason::TypeGuardFailure,
                            &local_slots,
                            &reg_slots,
                        );
                    }
                    builder.switch_to_block(guarded.merge_block);
                    write_reg(builder, &reg_slots, *dst, guarded.result);
                }
                Instruction::Sub { dst, lhs, rhs, .. }
                | Instruction::SubInt32 { dst, lhs, rhs, .. } => {
                    let left = read_reg(builder, &reg_slots, *lhs);
                    let right = read_reg(builder, &reg_slots, *rhs);
                    let guarded =
                        type_guards::emit_bare_i32_arith(builder, ArithOp::Sub, left, right);
                    let generic_ref = helpers.and_then(|h| h.get(HelperKind::GenericSub));
                    builder.switch_to_block(guarded.slow_block);
                    if let Some(helper_ref) = generic_ref {
                        let call = builder.ins().call(helper_ref, &[ctx_ptr, left, right]);
                        let result = builder.inst_results(call)[0];
                        write_reg(builder, &reg_slots, *dst, result);
                        let next_guarded_pc = body_pc + 1;
                        if next_guarded_pc < instruction_count {
                            builder.ins().jump(blocks[next_guarded_pc], &[]);
                        } else {
                            builder.ins().return_(&[result]);
                        }
                    } else {
                        emit_bailout_return_with_state(
                            builder,
                            ctx_ptr,
                            body_pc,
                            BailoutReason::TypeGuardFailure,
                            &local_slots,
                            &reg_slots,
                        );
                    }
                    builder.switch_to_block(guarded.merge_block);
                    write_reg(builder, &reg_slots, *dst, guarded.result);
                }
                Instruction::Mul { dst, lhs, rhs, .. }
                | Instruction::MulInt32 { dst, lhs, rhs, .. } => {
                    let left = read_reg(builder, &reg_slots, *lhs);
                    let right = read_reg(builder, &reg_slots, *rhs);
                    let guarded =
                        type_guards::emit_bare_i32_arith(builder, ArithOp::Mul, left, right);
                    let generic_ref = helpers.and_then(|h| h.get(HelperKind::GenericMul));
                    builder.switch_to_block(guarded.slow_block);
                    if let Some(helper_ref) = generic_ref {
                        let call = builder.ins().call(helper_ref, &[ctx_ptr, left, right]);
                        let result = builder.inst_results(call)[0];
                        write_reg(builder, &reg_slots, *dst, result);
                        let next_guarded_pc = body_pc + 1;
                        if next_guarded_pc < instruction_count {
                            builder.ins().jump(blocks[next_guarded_pc], &[]);
                        } else {
                            builder.ins().return_(&[result]);
                        }
                    } else {
                        emit_bailout_return_with_state(
                            builder,
                            ctx_ptr,
                            body_pc,
                            BailoutReason::TypeGuardFailure,
                            &local_slots,
                            &reg_slots,
                        );
                    }
                    builder.switch_to_block(guarded.merge_block);
                    write_reg(builder, &reg_slots, *dst, guarded.result);
                }
                // --- Inc/Dec (bare, overflow only) ---
                Instruction::Inc { dst, src } => {
                    let val = read_reg(builder, &reg_slots, *src);
                    let guarded = type_guards::emit_bare_i32_inc(builder, val);
                    let generic_ref = helpers.and_then(|h| h.get(HelperKind::GenericInc));
                    builder.switch_to_block(guarded.slow_block);
                    if let Some(helper_ref) = generic_ref {
                        let call = builder.ins().call(helper_ref, &[ctx_ptr, val]);
                        let result = builder.inst_results(call)[0];
                        write_reg(builder, &reg_slots, *dst, result);
                        let next_guarded_pc = body_pc + 1;
                        if next_guarded_pc < instruction_count {
                            builder.ins().jump(blocks[next_guarded_pc], &[]);
                        } else {
                            builder.ins().return_(&[result]);
                        }
                    } else {
                        emit_bailout_return_with_state(
                            builder,
                            ctx_ptr,
                            body_pc,
                            BailoutReason::TypeGuardFailure,
                            &local_slots,
                            &reg_slots,
                        );
                    }
                    builder.switch_to_block(guarded.merge_block);
                    write_reg(builder, &reg_slots, *dst, guarded.result);
                }
                Instruction::Dec { dst, src } => {
                    let val = read_reg(builder, &reg_slots, *src);
                    let guarded = type_guards::emit_bare_i32_dec(builder, val);
                    let generic_ref = helpers.and_then(|h| h.get(HelperKind::GenericDec));
                    builder.switch_to_block(guarded.slow_block);
                    if let Some(helper_ref) = generic_ref {
                        let call = builder.ins().call(helper_ref, &[ctx_ptr, val]);
                        let result = builder.inst_results(call)[0];
                        write_reg(builder, &reg_slots, *dst, result);
                        let next_guarded_pc = body_pc + 1;
                        if next_guarded_pc < instruction_count {
                            builder.ins().jump(blocks[next_guarded_pc], &[]);
                        } else {
                            builder.ins().return_(&[result]);
                        }
                    } else {
                        emit_bailout_return_with_state(
                            builder,
                            ctx_ptr,
                            body_pc,
                            BailoutReason::TypeGuardFailure,
                            &local_slots,
                            &reg_slots,
                        );
                    }
                    builder.switch_to_block(guarded.merge_block);
                    write_reg(builder, &reg_slots, *dst, guarded.result);
                }
                // --- Bare comparisons (no type guard needed) ---
                Instruction::Lt { dst, lhs, rhs } => {
                    let left = read_reg(builder, &reg_slots, *lhs);
                    let right = read_reg(builder, &reg_slots, *rhs);
                    let guarded =
                        type_guards::emit_bare_i32_cmp(builder, IntCC::SignedLessThan, left, right);
                    // slow_block is never reached for bare cmp, but seal it
                    builder.switch_to_block(guarded.slow_block);
                    emit_bailout_return_with_state(
                        builder,
                        ctx_ptr,
                        body_pc,
                        BailoutReason::TypeGuardFailure,
                        &local_slots,
                        &reg_slots,
                    );
                    builder.switch_to_block(guarded.merge_block);
                    write_reg(builder, &reg_slots, *dst, guarded.result);
                }
                Instruction::Le { dst, lhs, rhs } => {
                    let left = read_reg(builder, &reg_slots, *lhs);
                    let right = read_reg(builder, &reg_slots, *rhs);
                    let guarded = type_guards::emit_bare_i32_cmp(
                        builder,
                        IntCC::SignedLessThanOrEqual,
                        left,
                        right,
                    );
                    builder.switch_to_block(guarded.slow_block);
                    emit_bailout_return_with_state(
                        builder,
                        ctx_ptr,
                        body_pc,
                        BailoutReason::TypeGuardFailure,
                        &local_slots,
                        &reg_slots,
                    );
                    builder.switch_to_block(guarded.merge_block);
                    write_reg(builder, &reg_slots, *dst, guarded.result);
                }
                Instruction::Gt { dst, lhs, rhs } => {
                    let left = read_reg(builder, &reg_slots, *lhs);
                    let right = read_reg(builder, &reg_slots, *rhs);
                    let guarded = type_guards::emit_bare_i32_cmp(
                        builder,
                        IntCC::SignedGreaterThan,
                        left,
                        right,
                    );
                    builder.switch_to_block(guarded.slow_block);
                    emit_bailout_return_with_state(
                        builder,
                        ctx_ptr,
                        body_pc,
                        BailoutReason::TypeGuardFailure,
                        &local_slots,
                        &reg_slots,
                    );
                    builder.switch_to_block(guarded.merge_block);
                    write_reg(builder, &reg_slots, *dst, guarded.result);
                }
                Instruction::Ge { dst, lhs, rhs } => {
                    let left = read_reg(builder, &reg_slots, *lhs);
                    let right = read_reg(builder, &reg_slots, *rhs);
                    let guarded = type_guards::emit_bare_i32_cmp(
                        builder,
                        IntCC::SignedGreaterThanOrEqual,
                        left,
                        right,
                    );
                    builder.switch_to_block(guarded.slow_block);
                    emit_bailout_return_with_state(
                        builder,
                        ctx_ptr,
                        body_pc,
                        BailoutReason::TypeGuardFailure,
                        &local_slots,
                        &reg_slots,
                    );
                    builder.switch_to_block(guarded.merge_block);
                    write_reg(builder, &reg_slots, *dst, guarded.result);
                }
                // --- Strict eq/ne (same as guarded — no type guard needed) ---
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
                // --- Not (same as guarded) ---
                Instruction::Not { dst, src } => {
                    let val = read_reg(builder, &reg_slots, *src);
                    let truthy = type_guards::emit_is_truthy(builder, val);
                    let is_falsy = builder.ins().icmp_imm(IntCC::Equal, truthy, 0);
                    let out = type_guards::emit_bool_to_nanbox(builder, is_falsy);
                    write_reg(builder, &reg_slots, *dst, out);
                }
                // --- Control flow in optimized body ---
                Instruction::Jump { offset } => {
                    let target = jump_target(body_pc, offset.offset(), instruction_count)?;
                    // Back-edge → stay in optimized; exit → shared blocks
                    if target == vl.header_pc {
                        builder.ins().jump(vl.opt_blocks[0], &[]);
                    } else if target >= vl.header_pc && target <= vl.back_edge_pc {
                        builder
                            .ins()
                            .jump(vl.opt_blocks[target - vl.header_pc], &[]);
                    } else {
                        builder.ins().jump(blocks[target], &[]);
                    }
                    continue;
                }
                Instruction::JumpIfTrue { cond, offset } => {
                    let cond_val = read_reg(builder, &reg_slots, *cond);
                    let truthy = type_guards::emit_is_truthy(builder, cond_val);
                    let is_truthy = builder.ins().icmp_imm(IntCC::NotEqual, truthy, 0);
                    let jump_to = jump_target(body_pc, offset.offset(), instruction_count)?;
                    let jump_block = if jump_to >= vl.header_pc && jump_to <= vl.back_edge_pc {
                        vl.opt_blocks[jump_to - vl.header_pc]
                    } else {
                        blocks[jump_to]
                    };
                    let fallthrough = body_pc + 1;
                    let ft_block = if fallthrough >= vl.header_pc && fallthrough <= vl.back_edge_pc
                    {
                        vl.opt_blocks[fallthrough - vl.header_pc]
                    } else if fallthrough < instruction_count {
                        blocks[fallthrough]
                    } else {
                        exit
                    };
                    builder
                        .ins()
                        .brif(is_truthy, jump_block, &[], ft_block, &[]);
                    continue;
                }
                Instruction::JumpIfFalse { cond, offset } => {
                    let cond_val = read_reg(builder, &reg_slots, *cond);
                    let truthy = type_guards::emit_is_truthy(builder, cond_val);
                    let is_truthy = builder.ins().icmp_imm(IntCC::NotEqual, truthy, 0);
                    let jump_to = jump_target(body_pc, offset.offset(), instruction_count)?;
                    let jump_block = if jump_to >= vl.header_pc && jump_to <= vl.back_edge_pc {
                        vl.opt_blocks[jump_to - vl.header_pc]
                    } else {
                        blocks[jump_to]
                    };
                    let fallthrough = body_pc + 1;
                    let ft_block = if fallthrough >= vl.header_pc && fallthrough <= vl.back_edge_pc
                    {
                        vl.opt_blocks[fallthrough - vl.header_pc]
                    } else if fallthrough < instruction_count {
                        blocks[fallthrough]
                    } else {
                        exit
                    };
                    builder
                        .ins()
                        .brif(is_truthy, ft_block, &[], jump_block, &[]);
                    continue;
                }
                Instruction::JumpIfNullish { src, offset } => {
                    let src_val = read_reg(builder, &reg_slots, *src);
                    let is_nullish = type_guards::emit_is_nullish(builder, src_val);
                    let jump_to = jump_target(body_pc, offset.offset(), instruction_count)?;
                    let jump_block = if jump_to >= vl.header_pc && jump_to <= vl.back_edge_pc {
                        vl.opt_blocks[jump_to - vl.header_pc]
                    } else {
                        blocks[jump_to]
                    };
                    let fallthrough = body_pc + 1;
                    let ft_block = if fallthrough >= vl.header_pc && fallthrough <= vl.back_edge_pc
                    {
                        vl.opt_blocks[fallthrough - vl.header_pc]
                    } else if fallthrough < instruction_count {
                        blocks[fallthrough]
                    } else {
                        exit
                    };
                    builder
                        .ins()
                        .brif(is_nullish, jump_block, &[], ft_block, &[]);
                    continue;
                }
                Instruction::JumpIfNotNullish { src, offset } => {
                    let src_val = read_reg(builder, &reg_slots, *src);
                    let is_nullish = type_guards::emit_is_nullish(builder, src_val);
                    let jump_to = jump_target(body_pc, offset.offset(), instruction_count)?;
                    let jump_block = if jump_to >= vl.header_pc && jump_to <= vl.back_edge_pc {
                        vl.opt_blocks[jump_to - vl.header_pc]
                    } else {
                        blocks[jump_to]
                    };
                    let fallthrough = body_pc + 1;
                    let ft_block = if fallthrough >= vl.header_pc && fallthrough <= vl.back_edge_pc
                    {
                        vl.opt_blocks[fallthrough - vl.header_pc]
                    } else if fallthrough < instruction_count {
                        blocks[fallthrough]
                    } else {
                        exit
                    };
                    builder
                        .ins()
                        .brif(is_nullish, ft_block, &[], jump_block, &[]);
                    continue;
                }
                // --- Return (same as guarded) ---
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
                // --- Anything else: transfer to guarded version ---
                _ => {
                    builder.ins().jump(blocks[body_pc], &[]);
                    continue;
                }
            }

            // Fallthrough within optimized body
            let next_body_idx = body_idx + 1;
            if next_body_idx < body_len {
                builder.ins().jump(vl.opt_blocks[next_body_idx], &[]);
            } else {
                // Past the back-edge — shouldn't normally happen since back-edge
                // is a Jump instruction, but handle gracefully
                let post_pc = vl.back_edge_pc + 1;
                if post_pc < instruction_count {
                    builder.ins().jump(blocks[post_pc], &[]);
                } else {
                    let undef = builder.ins().iconst(types::I64, type_guards::TAG_UNDEFINED);
                    builder.ins().return_(&[undef]);
                }
            }
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
    fn helper_eligibility_accepts_yield_rejects_await() {
        let yield_fn = Function::builder()
            .name("yield_eligible")
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
        assert!(can_translate_function_with_helpers(&yield_fn, &[]));

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
    fn helper_eligibility_rejects_async_accepts_generator_flags() {
        let async_fn = Function::builder()
            .name("async_flag_non_eligible")
            .register_count(1)
            .is_async(true)
            .instruction(Instruction::ReturnUndefined)
            .build();
        assert!(!can_translate_function_with_helpers(&async_fn, &[]));

        let generator_fn = Function::builder()
            .name("generator_flag_eligible")
            .register_count(1)
            .is_generator(true)
            .instruction(Instruction::ReturnUndefined)
            .build();
        assert!(can_translate_function_with_helpers(&generator_fn, &[]));
    }
}
