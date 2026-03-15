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
use cranelift_frontend::{FunctionBuilder, Variable};
use otter_vm_bytecode::function::UpvalueCapture;
use otter_vm_bytecode::instruction::Instruction;
use otter_vm_bytecode::operand::{ConstantIndex, LocalIndex, Register};
use otter_vm_bytecode::{Constant, Function};

use crate::JitError;
use crate::bailout::{BAILOUT_SENTINEL, BailoutReason};
use crate::compiler::{DeoptResumeSite, build_deopt_metadata};
use crate::loop_analysis;
use crate::runtime_helpers::{
    HelperKind, HelperRefs, JIT_CTX_BAILOUT_PC_OFFSET, JIT_CTX_BAILOUT_REASON_OFFSET,
    JIT_CTX_DEOPT_LOCALS_PTR_OFFSET, JIT_CTX_DEOPT_REGS_PTR_OFFSET, JIT_CTX_IC_PROBES_PTR_OFFSET,
    JIT_CTX_OSR_ENTRY_PC_OFFSET, JIT_CTX_TIER_UP_BUDGET_OFFSET, JIT_CTX_UPVALUE_COUNT_OFFSET,
    JIT_CTX_UPVALUES_PTR_OFFSET, JIT_UPVALUE_CELL_GCBOX_PTR_OFFSET, JIT_UPVALUE_CELL_SIZE,
    JIT_UPVALUE_DATA_VALUE_OFFSET, JIT_UPVALUE_GCBOX_VALUE_OFFSET,
};
use crate::type_guards::{self, ArithOp, BitwiseOp, SpecializationHint};

/// NaN-boxed bits for Value::hole() — array hole sentinel.
mod value_constants {
    pub const HOLE_BITS: u64 = 0x7FF8_0000_0000_0004;
}
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
    /// Parent local slots that are known to hold closure functions at this call site.
    local_func_snapshot: std::collections::HashMap<u16, u32>,
}

#[inline]
fn inline_compatible_upvalues(function: &Function) -> bool {
    function
        .upvalues
        .iter()
        .all(|capture| matches!(capture, UpvalueCapture::Local(_)))
}

/// Resolve which local slots are actually captured by nested closures in this function.
///
/// The bytecode compiler may emit `CloseUpvalue` conservatively for bindings that
/// turned out not to be captured at runtime. When we can prove from nested
/// closure metadata that a local is never captured, the JIT can elide the
/// `CloseUpvalue` entirely instead of paying a helper boundary for a no-op.
fn resolve_locally_captured_locals(
    instructions: &[Instruction],
    module_functions: &[(u32, Function)],
) -> std::collections::HashSet<u16> {
    if module_functions.is_empty() {
        return std::collections::HashSet::new();
    }

    let func_by_index: std::collections::HashMap<u32, &Function> = module_functions
        .iter()
        .map(|(idx, func)| (*idx, func))
        .collect();
    let mut captured = std::collections::HashSet::new();

    for instruction in instructions {
        let func_index = match instruction {
            Instruction::Closure { func, .. }
            | Instruction::AsyncClosure { func, .. }
            | Instruction::GeneratorClosure { func, .. }
            | Instruction::AsyncGeneratorClosure { func, .. } => func.0,
            _ => continue,
        };

        let Some(func) = func_by_index.get(&func_index) else {
            continue;
        };

        for capture in &func.upvalues {
            if let otter_vm_bytecode::function::UpvalueCapture::Local(local_idx) = capture {
                captured.insert(local_idx.0);
            }
        }
    }

    captured
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
            Instruction::GetLocal2 {
                dst1,
                idx1,
                dst2,
                idx2,
            } => {
                if let Some(&func_idx) = local_func.get(&idx1.index()) {
                    reg_func.insert(dst1.0, func_idx);
                } else {
                    reg_func.remove(&dst1.0);
                }
                if let Some(&func_idx) = local_func.get(&idx2.index()) {
                    reg_func.insert(dst2.0, func_idx);
                } else {
                    reg_func.remove(&dst2.0);
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
                if let Some(&func_idx) = reg_func.get(&func.0)
                    && let Some(&callee) = func_by_index.get(&func_idx)
                {
                    // Verify the callee stays within the subset handled by the
                    // inline emitter, including local-upvalue loads and nested
                    // calls lowered through the regular JIT call path.
                    let callee_instrs = callee.instructions.read();
                    let all_translatable = inline_compatible_upvalues(callee)
                        && callee_instrs.iter().all(is_supported_inline_opcode);
                    if all_translatable {
                        result.insert(
                            pc,
                            InlineCandidate {
                                callee,
                                function_index: func_idx,
                                local_func_snapshot: local_func
                                    .iter()
                                    .map(|(local, func_idx)| (*local, func_idx.saturating_add(1)))
                                    .collect(),
                            },
                        );
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
        | Instruction::GetElemInt { dst, .. }
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
        | Instruction::GetPropQuickened { dst, .. }
        | Instruction::GetPropString { dst, .. }
        | Instruction::GetArrayLength { dst, .. } => Some(dst.0),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum JumpConditionKind {
    Constant(bool),
    BoxedBoolean,
    Generic,
}

#[inline]
fn classify_jump_condition(
    instructions: &[Instruction],
    pc: usize,
    cond: Register,
) -> JumpConditionKind {
    let Some(prev_pc) = pc.checked_sub(1) else {
        return JumpConditionKind::Generic;
    };
    let Some(prev) = instructions.get(prev_pc) else {
        return JumpConditionKind::Generic;
    };

    match prev {
        Instruction::LoadTrue { dst } if *dst == cond => JumpConditionKind::Constant(true),
        Instruction::LoadFalse { dst } if *dst == cond => JumpConditionKind::Constant(false),
        Instruction::Eq { dst, .. }
        | Instruction::Ne { dst, .. }
        | Instruction::StrictEq { dst, .. }
        | Instruction::StrictNe { dst, .. }
        | Instruction::Lt { dst, .. }
        | Instruction::Le { dst, .. }
        | Instruction::Gt { dst, .. }
        | Instruction::Ge { dst, .. }
        | Instruction::Not { dst, .. }
            if *dst == cond =>
        {
            JumpConditionKind::BoxedBoolean
        }
        _ => JumpConditionKind::Generic,
    }
}

#[inline]
fn emit_jump_truthy_value(
    builder: &mut FunctionBuilder,
    condition_kind: JumpConditionKind,
    cond_value: Value,
) -> Value {
    match condition_kind {
        JumpConditionKind::BoxedBoolean => {
            builder
                .ins()
                .icmp_imm(IntCC::NotEqual, cond_value, type_guards::TAG_FALSE)
        }
        JumpConditionKind::Generic => {
            let truthy = type_guards::emit_is_truthy(builder, cond_value);
            builder.ins().icmp_imm(IntCC::NotEqual, truthy, 0)
        }
        JumpConditionKind::Constant(_) => unreachable!("constant conditions do not need a value"),
    }
}

#[inline]
fn instruction_reads_register(instruction: &Instruction, reg: Register) -> bool {
    match instruction {
        Instruction::SetLocal { src, .. }
        | Instruction::SetUpvalue { src, .. }
        | Instruction::Move { src, .. }
        | Instruction::Neg { src, .. }
        | Instruction::Inc { src, .. }
        | Instruction::Dec { src, .. }
        | Instruction::BitNot { src, .. }
        | Instruction::Not { src, .. }
        | Instruction::JumpIfNullish { src, .. }
        | Instruction::JumpIfNotNullish { src, .. }
        | Instruction::Return { src }
        | Instruction::Yield { src, .. }
        | Instruction::Await { src, .. }
        | Instruction::Dup { src, .. }
        | Instruction::ToNumber { src, .. }
        | Instruction::ToString { src, .. }
        | Instruction::TypeOf { src, .. }
        | Instruction::Export { src, .. }
        | Instruction::Spread { src, .. }
        | Instruction::RequireCoercible { src } => *src == reg,
        Instruction::Add { lhs, rhs, .. }
        | Instruction::Sub { lhs, rhs, .. }
        | Instruction::Mul { lhs, rhs, .. }
        | Instruction::Div { lhs, rhs, .. }
        | Instruction::Mod { lhs, rhs, .. }
        | Instruction::Pow { lhs, rhs, .. }
        | Instruction::BitAnd { lhs, rhs, .. }
        | Instruction::BitOr { lhs, rhs, .. }
        | Instruction::BitXor { lhs, rhs, .. }
        | Instruction::Shl { lhs, rhs, .. }
        | Instruction::Shr { lhs, rhs, .. }
        | Instruction::Ushr { lhs, rhs, .. }
        | Instruction::Eq { lhs, rhs, .. }
        | Instruction::StrictEq { lhs, rhs, .. }
        | Instruction::Ne { lhs, rhs, .. }
        | Instruction::StrictNe { lhs, rhs, .. }
        | Instruction::Lt { lhs, rhs, .. }
        | Instruction::Le { lhs, rhs, .. }
        | Instruction::Gt { lhs, rhs, .. }
        | Instruction::Ge { lhs, rhs, .. }
        | Instruction::InstanceOf { lhs, rhs, .. }
        | Instruction::In { lhs, rhs, .. } => *lhs == reg || *rhs == reg,
        Instruction::JumpIfTrue { cond, .. } | Instruction::JumpIfFalse { cond, .. } => {
            *cond == reg
        }
        Instruction::GetProp { obj, key, .. } | Instruction::DeleteProp { obj, key, .. } => {
            *obj == reg || *key == reg
        }
        Instruction::GetElem { arr, idx, .. } => *arr == reg || *idx == reg,
        Instruction::SetProp { obj, key, val, .. }
        | Instruction::DefineProperty { obj, key, val } => {
            *obj == reg || *key == reg || *val == reg
        }
        Instruction::SetElem { arr, idx, val, .. } => *arr == reg || *idx == reg || *val == reg,
        Instruction::GetPropConst { obj, .. } => *obj == reg,
        Instruction::SetPropConst { obj, val, .. } => *obj == reg || *val == reg,
        Instruction::GetElemInt { obj, index, .. } => *obj == reg || *index == reg,
        Instruction::DefineGetter { obj, key, func }
        | Instruction::DefineSetter { obj, key, func } => {
            *obj == reg || *key == reg || *func == reg
        }
        Instruction::DefineMethod { obj, key, val } => *obj == reg || *key == reg || *val == reg,
        Instruction::SetPrototype { obj, proto } => *obj == reg || *proto == reg,
        Instruction::Call { func, argc, .. } | Instruction::Construct { func, argc, .. } => {
            let start = func.0;
            let end = start.saturating_add(u16::from(*argc));
            reg.0 >= start && reg.0 <= end
        }
        Instruction::CallMethod { .. } | Instruction::CallMethodComputed { .. } => true,
        Instruction::CallWithReceiver {
            func, this, argc, ..
        } => {
            if *func == reg || *this == reg {
                return true;
            }
            let start = this.0.saturating_add(1);
            let end = start.saturating_add(u16::from(argc.saturating_sub(1)));
            reg.0 >= start && reg.0 <= end
        }
        Instruction::TailCall { func, argc } => {
            let start = func.0;
            let end = start.saturating_add(u16::from(*argc));
            reg.0 >= start && reg.0 <= end
        }
        Instruction::CallEval { code, .. } => *code == reg,
        Instruction::CallSpread { func, spread, .. }
        | Instruction::ConstructSpread { func, spread, .. } => *func == reg || *spread == reg,
        Instruction::CallMethodComputedSpread {
            obj, key, spread, ..
        } => *obj == reg || *key == reg || *spread == reg,
        Instruction::LoadUndefined { .. }
        | Instruction::LoadNull { .. }
        | Instruction::LoadTrue { .. }
        | Instruction::LoadFalse { .. }
        | Instruction::LoadInt8 { .. }
        | Instruction::LoadInt32 { .. }
        | Instruction::LoadConst { .. }
        | Instruction::GetLocal { .. }
        | Instruction::GetLocal2 { .. }
        | Instruction::GetUpvalue { .. }
        | Instruction::GetGlobal { .. }
        | Instruction::LoadThis { .. }
        | Instruction::Jump { .. }
        | Instruction::ReturnUndefined
        | Instruction::Nop
        | Instruction::Pop
        | Instruction::NewObject { .. }
        | Instruction::NewArray { .. }
        | Instruction::Closure { .. }
        | Instruction::AsyncClosure { .. }
        | Instruction::GeneratorClosure { .. }
        | Instruction::AsyncGeneratorClosure { .. }
        | Instruction::CreateArguments { .. }
        | Instruction::Import { .. }
        | Instruction::CloseUpvalue { .. } => false,
        _ => true,
    }
}

#[inline]
fn register_read_later_anywhere(
    instructions: &[Instruction],
    after_pc_exclusive: usize,
    reg: Register,
) -> bool {
    // Scan forward from after_pc_exclusive+1, but stop at the first
    // instruction that REDEFINES the register (since any later reads
    // refer to the new definition, not the old one).
    for instruction in instructions
        .iter()
        .skip(after_pc_exclusive.saturating_add(1))
    {
        if instruction_reads_register(instruction, reg) {
            return true;
        }
        // If this instruction defines (writes) the register, later
        // reads are of the new value — the old value is dead.
        let redefines = versioned_dst_registers(instruction)
            .iter()
            .any(|d| *d == Some(reg.0));
        if redefines {
            return false;
        }
    }
    false
}

#[inline]
fn next_jump_consumes_register(instructions: &[Instruction], pc: usize, reg: Register) -> bool {
    matches!(
        instructions.get(pc + 1),
        Some(Instruction::JumpIfTrue { cond, .. } | Instruction::JumpIfFalse { cond, .. }) if *cond == reg
    )
}

#[inline]
fn fused_versioned_compare_cc(
    instruction: &Instruction,
    dst: Register,
) -> Option<(Register, Register, IntCC)> {
    match instruction {
        Instruction::StrictEq { dst: d, lhs, rhs } if *d == dst => Some((*lhs, *rhs, IntCC::Equal)),
        Instruction::StrictNe { dst: d, lhs, rhs } if *d == dst => {
            Some((*lhs, *rhs, IntCC::NotEqual))
        }
        Instruction::Lt { dst: d, lhs, rhs } if *d == dst => {
            Some((*lhs, *rhs, IntCC::SignedLessThan))
        }
        Instruction::Le { dst: d, lhs, rhs } if *d == dst => {
            Some((*lhs, *rhs, IntCC::SignedLessThanOrEqual))
        }
        Instruction::Gt { dst: d, lhs, rhs } if *d == dst => {
            Some((*lhs, *rhs, IntCC::SignedGreaterThan))
        }
        Instruction::Ge { dst: d, lhs, rhs } if *d == dst => {
            Some((*lhs, *rhs, IntCC::SignedGreaterThanOrEqual))
        }
        _ => None,
    }
}

#[inline]
fn can_fuse_versioned_compare_branch(
    instructions: &[Instruction],
    pc: usize,
    dst: Register,
) -> bool {
    next_jump_consumes_register(instructions, pc, dst)
        && !register_read_later_anywhere(instructions, pc + 1, dst)
}

/// Get the destination register(s) for an instruction (versioned loop subset).
fn versioned_dst_registers(instr: &Instruction) -> [Option<u16>; 2] {
    match instr {
        Instruction::Add { dst, .. }
        | Instruction::Sub { dst, .. }
        | Instruction::Mul { dst, .. }
        | Instruction::Div { dst, .. }
        | Instruction::Mod { dst, .. }
        | Instruction::Pow { dst, .. }
        | Instruction::AddInt32 { dst, .. }
        | Instruction::SubInt32 { dst, .. }
        | Instruction::MulInt32 { dst, .. }
        | Instruction::BitOr { dst, .. }
        | Instruction::BitAnd { dst, .. }
        | Instruction::BitXor { dst, .. }
        | Instruction::Shl { dst, .. }
        | Instruction::Shr { dst, .. }
        | Instruction::Ushr { dst, .. }
        | Instruction::BitNot { dst, .. }
        | Instruction::Inc { dst, .. }
        | Instruction::Dec { dst, .. }
        | Instruction::Lt { dst, .. }
        | Instruction::Le { dst, .. }
        | Instruction::Gt { dst, .. }
        | Instruction::Ge { dst, .. }
        | Instruction::Eq { dst, .. }
        | Instruction::Ne { dst, .. }
        | Instruction::StrictEq { dst, .. }
        | Instruction::StrictNe { dst, .. }
        | Instruction::Not { dst, .. }
        | Instruction::Move { dst, .. }
        | Instruction::Neg { dst, .. }
        | Instruction::Dup { dst, .. }
        | Instruction::ToNumber { dst, .. }
        | Instruction::ToString { dst, .. }
        | Instruction::TypeOf { dst, .. }
        | Instruction::LoadInt8 { dst, .. }
        | Instruction::LoadInt32 { dst, .. }
        | Instruction::LoadTrue { dst }
        | Instruction::LoadFalse { dst }
        | Instruction::LoadNull { dst }
        | Instruction::LoadUndefined { dst }
        | Instruction::LoadConst { dst, .. }
        | Instruction::GetLocal { dst, .. }
        | Instruction::GetElemInt { dst, .. } => [Some(dst.0), None],
        Instruction::GetLocal2 { dst1, dst2, .. } => [Some(dst1.0), Some(dst2.0)],
        _ => [None, None],
    }
}

/// Get source register indices for an instruction (versioned loop subset).
fn versioned_src_registers(instr: &Instruction) -> [Option<u16>; 3] {
    match instr {
        Instruction::Add { lhs, rhs, .. }
        | Instruction::Sub { lhs, rhs, .. }
        | Instruction::Mul { lhs, rhs, .. }
        | Instruction::Div { lhs, rhs, .. }
        | Instruction::Mod { lhs, rhs, .. }
        | Instruction::Pow { lhs, rhs, .. }
        | Instruction::AddInt32 { lhs, rhs, .. }
        | Instruction::SubInt32 { lhs, rhs, .. }
        | Instruction::MulInt32 { lhs, rhs, .. }
        | Instruction::BitOr { lhs, rhs, .. }
        | Instruction::BitAnd { lhs, rhs, .. }
        | Instruction::BitXor { lhs, rhs, .. }
        | Instruction::Shl { lhs, rhs, .. }
        | Instruction::Shr { lhs, rhs, .. }
        | Instruction::Ushr { lhs, rhs, .. }
        | Instruction::Lt { lhs, rhs, .. }
        | Instruction::Le { lhs, rhs, .. }
        | Instruction::Gt { lhs, rhs, .. }
        | Instruction::Ge { lhs, rhs, .. }
        | Instruction::Eq { lhs, rhs, .. }
        | Instruction::Ne { lhs, rhs, .. }
        | Instruction::StrictEq { lhs, rhs, .. }
        | Instruction::StrictNe { lhs, rhs, .. } => [Some(lhs.0), Some(rhs.0), None],
        Instruction::Inc { src, .. }
        | Instruction::Dec { src, .. }
        | Instruction::BitNot { src, .. }
        | Instruction::Not { src, .. }
        | Instruction::Neg { src, .. }
        | Instruction::Move { src, .. }
        | Instruction::Dup { src, .. }
        | Instruction::ToNumber { src, .. }
        | Instruction::ToString { src, .. }
        | Instruction::TypeOf { src, .. }
        | Instruction::Return { src } => [Some(src.0), None, None],
        Instruction::SetLocal { src, .. } => [Some(src.0), None, None],
        Instruction::JumpIfTrue { cond, .. } | Instruction::JumpIfFalse { cond, .. } => {
            [Some(cond.0), None, None]
        }
        _ => [None, None, None],
    }
}

/// Build the set of PCs where arithmetic ops can use wrapping i32 (no overflow check)
/// because ALL consumers of the result are bitwise/truncating operations.
///
/// This implements V8 TurboFan / JSC DFG-style backwards truncation propagation:
/// - Any bitwise op (BitOr, BitAnd, BitXor, Shl, Shr) is a truncation sink
/// - An arithmetic op (Add/Sub/Mul) is wrapping if ALL its consumers are either
///   bitwise ops or themselves wrapping arithmetic ops
/// - Propagation continues to fixpoint to handle chains like `(a * b + c) | 0`
fn build_wrapping_set(
    instructions: &[Instruction],
    header_pc: usize,
    back_edge_pc: usize,
) -> std::collections::HashSet<usize> {
    use std::collections::{HashMap, HashSet};

    // Build def-use chains: for each register, track its most recent definition PC
    // and the PCs that consume it before it's redefined.
    let mut last_def: HashMap<u16, usize> = HashMap::new();
    let mut consumers: HashMap<usize, Vec<usize>> = HashMap::new();

    for pc in header_pc..=back_edge_pc {
        let instr = &instructions[pc];

        // Record uses FIRST (before updating defs)
        for src_idx in versioned_src_registers(instr).into_iter().flatten() {
            if let Some(&def_pc) = last_def.get(&src_idx) {
                consumers.entry(def_pc).or_default().push(pc);
            }
        }

        // Record defs
        for dst_idx in versioned_dst_registers(instr).into_iter().flatten() {
            last_def.insert(dst_idx, pc);
        }
    }

    let is_bitwise = |pc: usize| -> bool {
        matches!(
            instructions[pc],
            Instruction::BitOr { .. }
                | Instruction::BitAnd { .. }
                | Instruction::BitXor { .. }
                | Instruction::BitNot { .. }
                | Instruction::Shl { .. }
                | Instruction::Shr { .. }
                | Instruction::Ushr { .. }
        )
    };

    let is_arith = |pc: usize| -> bool {
        matches!(
            instructions[pc],
            Instruction::Add { .. }
                | Instruction::Sub { .. }
                | Instruction::Mul { .. }
                | Instruction::AddInt32 { .. }
                | Instruction::SubInt32 { .. }
                | Instruction::MulInt32 { .. }
        )
    };

    // Fixpoint iteration: mark arithmetic ops as wrapping if ALL consumers
    // are bitwise ops or already-marked wrapping ops.
    let mut wrapping: HashSet<usize> = HashSet::new();
    let mut changed = true;
    while changed {
        changed = false;
        for pc in header_pc..=back_edge_pc {
            if wrapping.contains(&pc) {
                continue;
            }
            if !is_arith(pc) {
                continue;
            }

            let uses = consumers.get(&pc).map(|v| v.as_slice()).unwrap_or(&[]);
            if uses.is_empty() {
                continue; // dead result — conservative, don't mark
            }

            if uses.iter().all(|&u| is_bitwise(u) || wrapping.contains(&u)) {
                wrapping.insert(pc);
                changed = true;
            }
        }
    }

    wrapping
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
fn is_string_constant(constants: &[Constant], idx: ConstantIndex) -> bool {
    matches!(
        constants.get(idx.index() as usize),
        Some(Constant::String(_))
    )
}

#[inline]
fn is_const_utf16(constants: &[Constant], idx: ConstantIndex, needle: &[u16]) -> bool {
    match constants.get(idx.index() as usize) {
        Some(Constant::String(units)) => units.as_slice() == needle,
        _ => false,
    }
}

/// Heuristic: detect nearby string-producing ops feeding `Add`.
///
/// This keeps numeric hot paths native while routing likely string-concat
/// sites to generic `+` handling without repeated guard bailouts.
fn add_likely_string_concat(
    instructions: &[Instruction],
    constants: &[Constant],
    pc: usize,
    lhs: Register,
    rhs: Register,
) -> bool {
    const LOOKBACK: usize = 6;
    const TO_STRING_UTF16: [u16; 8] = [116, 111, 83, 116, 114, 105, 110, 103];
    const SLICE_UTF16: [u16; 5] = [115, 108, 105, 99, 101];

    let start = pc.saturating_sub(LOOKBACK);
    for inst in instructions[start..pc].iter().rev() {
        match inst {
            Instruction::ToString { dst, .. } if *dst == lhs || *dst == rhs => {
                return true;
            }
            Instruction::LoadConst { dst, idx }
                if (*dst == lhs || *dst == rhs) && is_string_constant(constants, *idx) =>
            {
                return true;
            }
            Instruction::CallMethod { dst, method, .. }
                if (*dst == lhs || *dst == rhs)
                    && (is_const_utf16(constants, *method, &TO_STRING_UTF16)
                        || is_const_utf16(constants, *method, &SLICE_UTF16)) =>
            {
                return true;
            }
            _ => {}
        }
    }
    false
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
            | Instruction::GetLocal2 { .. }
    )
}

#[inline]
fn is_supported_inline_opcode(instruction: &Instruction) -> bool {
    is_supported_baseline_opcode(instruction)
        || matches!(
            instruction,
            Instruction::GetUpvalue { .. } | Instruction::Call { .. }
        )
}

struct BinaryLeafArithSpec {
    op: ArithOp,
    lhs_param: u16,
    rhs_param: u16,
    feedback_index: u16,
    uses_generic_fallback: bool,
}

#[inline]
fn match_binary_leaf_arith(function: &Function) -> Option<BinaryLeafArithSpec> {
    if function.flags.is_async
        || function.flags.is_generator
        || function.flags.has_rest
        || function.flags.uses_eval
        || function.flags.uses_arguments
        || !function.upvalues.is_empty()
        || function.param_count < 2
    {
        return None;
    }

    let instructions = function.instructions.read();
    let (lhs_dst, lhs_idx, rhs_dst, rhs_idx, op_inst, ret_inst) = match instructions.as_slice() {
        [
            Instruction::GetLocal {
                dst: lhs_dst,
                idx: lhs_idx,
            },
            Instruction::GetLocal {
                dst: rhs_dst,
                idx: rhs_idx,
            },
            op_inst,
            ret_inst,
        ] => (
            *lhs_dst,
            lhs_idx.index(),
            *rhs_dst,
            rhs_idx.index(),
            op_inst,
            ret_inst,
        ),
        [
            Instruction::GetLocal {
                dst: lhs_dst,
                idx: lhs_idx,
            },
            Instruction::GetLocal {
                dst: rhs_dst,
                idx: rhs_idx,
            },
            op_inst,
            ret_inst,
            Instruction::ReturnUndefined,
        ] => (
            *lhs_dst,
            lhs_idx.index(),
            *rhs_dst,
            rhs_idx.index(),
            op_inst,
            ret_inst,
        ),
        [
            Instruction::GetLocal2 {
                dst1: lhs_dst,
                idx1: lhs_idx,
                dst2: rhs_dst,
                idx2: rhs_idx,
            },
            op_inst,
            ret_inst,
        ] => (
            *lhs_dst,
            lhs_idx.index(),
            *rhs_dst,
            rhs_idx.index(),
            op_inst,
            ret_inst,
        ),
        [
            Instruction::GetLocal2 {
                dst1: lhs_dst,
                idx1: lhs_idx,
                dst2: rhs_dst,
                idx2: rhs_idx,
            },
            op_inst,
            ret_inst,
            Instruction::ReturnUndefined,
        ] => (
            *lhs_dst,
            lhs_idx.index(),
            *rhs_dst,
            rhs_idx.index(),
            op_inst,
            ret_inst,
        ),
        _ => return None,
    };

    let (op, dst, lhs, rhs, feedback_index, uses_generic_fallback) = match op_inst {
        Instruction::Add {
            dst,
            lhs,
            rhs,
            feedback_index,
        } => (ArithOp::Add, *dst, *lhs, *rhs, *feedback_index, true),
        Instruction::AddInt32 {
            dst,
            lhs,
            rhs,
            feedback_index,
        } => (ArithOp::Add, *dst, *lhs, *rhs, *feedback_index, false),
        Instruction::Sub {
            dst,
            lhs,
            rhs,
            feedback_index,
        } => (ArithOp::Sub, *dst, *lhs, *rhs, *feedback_index, true),
        Instruction::SubInt32 {
            dst,
            lhs,
            rhs,
            feedback_index,
        } => (ArithOp::Sub, *dst, *lhs, *rhs, *feedback_index, false),
        Instruction::Mul {
            dst,
            lhs,
            rhs,
            feedback_index,
        } => (ArithOp::Mul, *dst, *lhs, *rhs, *feedback_index, true),
        Instruction::MulInt32 {
            dst,
            lhs,
            rhs,
            feedback_index,
        } => (ArithOp::Mul, *dst, *lhs, *rhs, *feedback_index, false),
        _ => return None,
    };

    if lhs != lhs_dst || rhs != rhs_dst {
        return None;
    }
    if !matches!(ret_inst, Instruction::Return { src } if *src == dst) {
        return None;
    }

    Some(BinaryLeafArithSpec {
        op,
        lhs_param: lhs_idx,
        rhs_param: rhs_idx,
        feedback_index,
        uses_generic_fallback,
    })
}

#[allow(dead_code)]
#[inline]
fn can_inline_leaf_function(function: &Function) -> bool {
    match_binary_leaf_arith(function).is_some()
}

fn binary_leaf_generic_helper(op: ArithOp) -> HelperKind {
    match op {
        ArithOp::Add => HelperKind::GenericAdd,
        ArithOp::Sub => HelperKind::GenericSub,
        ArithOp::Mul => HelperKind::GenericMul,
    }
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
                | Instruction::GetPropString { .. }
                | Instruction::GetArrayLength { .. }
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
                | Instruction::GetElemInt { .. }
                | Instruction::SetElem { .. }
                | Instruction::DeleteProp { .. }
                | Instruction::DefineProperty { .. }
                | Instruction::SetPrototype { .. }
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
                | Instruction::IncLocal { .. }
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

/// Maximum function size for JIT compilation (instructions).
/// Functions beyond this are too expensive to compile (O(n²) regalloc)
/// and rarely benefit from JIT (usually cold sprawling code).
const MAX_JIT_FUNCTION_SIZE: usize = 2000;

/// Minimum ratio of inlineable instructions for JIT to be profitable.
/// If < 20% of instructions can be executed inline (rest are helper calls),
/// the function gains minimal benefit from JIT.
const MIN_INLINE_RATIO: f64 = 0.15;

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

    let instructions = function.instructions.read();
    let instruction_count = instructions.len();
    if instruction_count == 0 {
        return true;
    }

    // Profitability: reject very large functions (compilation cost too high)
    if instruction_count > MAX_JIT_FUNCTION_SIZE {
        return false;
    }

    // Profitability: check instruction mix — skip functions that are mostly helper calls.
    // Count instructions that execute purely in Cranelift IR (arithmetic, comparisons,
    // control flow, local access) vs instructions that require helper FFI calls.
    if has_helpers && instruction_count > 20 {
        let mut inline_count = 0usize;
        let mut has_backward_jump = false;
        for (pc, inst) in instructions.iter().enumerate() {
            if is_supported_baseline_opcode(inst) {
                inline_count += 1;
            }
            // Check for backward jumps (loops) — main source of JIT benefit
            match inst {
                Instruction::Jump { offset }
                | Instruction::JumpIfTrue { offset, .. }
                | Instruction::JumpIfFalse { offset, .. } => {
                    if offset.offset() < 0 || (offset.offset() as usize) < pc {
                        has_backward_jump = true;
                    }
                }
                _ => {}
            }
        }
        let ratio = inline_count as f64 / instruction_count as f64;
        // Functions without loops AND low inline ratio are poor JIT candidates.
        // Functions WITH loops always get compiled (loops are the primary JIT benefit).
        if !has_backward_jump && ratio < MIN_INLINE_RATIO {
            return false;
        }
    }
    drop(instructions);

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

#[inline]
fn collect_osr_loop_headers(versioned_loops: &[loop_analysis::LoopInfo]) -> Vec<usize> {
    versioned_loops.iter().map(|info| info.header_pc).collect()
}

fn read_reg(builder: &mut FunctionBuilder<'_>, vars: &[Variable], reg: Register) -> Value {
    builder.use_var(vars[reg.index() as usize])
}

fn current_block_is_filled(builder: &FunctionBuilder<'_>) -> bool {
    let Some(block) = builder.current_block() else {
        return false;
    };
    let Some(inst) = builder.func.layout.last_inst(block) else {
        return false;
    };
    builder.func.dfg.insts[inst].opcode().is_terminator()
}

fn read_local(builder: &mut FunctionBuilder<'_>, vars: &[Variable], idx: LocalIndex) -> Value {
    builder.use_var(vars[idx.index() as usize])
}

fn write_reg(builder: &mut FunctionBuilder<'_>, vars: &[Variable], reg: Register, value: Value) {
    builder.def_var(vars[reg.index() as usize], value);
}

fn write_local(
    builder: &mut FunctionBuilder<'_>,
    vars: &[Variable],
    idx: LocalIndex,
    value: Value,
) {
    builder.def_var(vars[idx.index() as usize], value);
}

/// Versioned loop metadata for optimized int32 loop bodies.
struct VersionedLoop {
    header_pc: usize,
    back_edge_pc: usize,
    pre_header: cranelift_codegen::ir::Block,
    /// Optimized blocks indexed by (body_pc - header_pc)
    opt_blocks: Vec<cranelift_codegen::ir::Block>,
    check_registers: Vec<u16>,
    /// Raw i32 SSA Variables for checked registers (unboxed in pre-header)
    i32_vars: Vec<Variable>,
    /// Map: register_index → index in i32_vars
    reg_to_i32: std::collections::HashMap<u16, usize>,
    /// Raw i32 SSA Variables for loop-local variables (eliminates box/unbox round-trip)
    i32_local_vars: Vec<Variable>,
    /// Map: local_index → index in i32_local_vars
    local_to_i32: std::collections::HashMap<u16, usize>,
    /// PCs where arithmetic ops can use wrapping i32 (no overflow check).
    /// Built by backwards truncation analysis (V8/JSC-style).
    wrapping_pcs: std::collections::HashSet<usize>,
    /// Map: obj_register → cached raw pointer Variable (shape pre-verified in pre-header).
    /// For property reads on loop-invariant objects, the tag check + shape check + pointer
    /// extraction is hoisted to the pre-header. Loop body uses the cached pointer directly
    /// (just a load from inline_slots — one instruction instead of ~11).
    shape_hoisted_ptrs: std::collections::HashMap<u16, Variable>,
}

/// Read a register as raw i32 in a versioned loop body.
/// If the register has an i32 variable (was checked in pre-header), uses it directly.
/// Otherwise, unboxes from the i64 variable.
fn read_reg_i32(
    builder: &mut FunctionBuilder<'_>,
    reg_vars: &[Variable],
    vl: &VersionedLoop,
    reg: Register,
) -> Value {
    if let Some(&j) = vl.reg_to_i32.get(&reg.0) {
        builder.use_var(vl.i32_vars[j])
    } else {
        let boxed = builder.use_var(reg_vars[reg.index() as usize]);
        type_guards::emit_unbox_int32(builder, boxed)
    }
}

/// Write a raw i32 result in a versioned loop body.
/// Only updates the i32 variable for tracked registers (no boxing in the hot path).
/// For non-tracked registers, boxes and updates the i64 variable.
fn write_reg_i32(
    builder: &mut FunctionBuilder<'_>,
    reg_vars: &[Variable],
    vl: &VersionedLoop,
    reg: Register,
    raw_i32: Value,
) {
    if let Some(&j) = vl.reg_to_i32.get(&reg.0) {
        // Hot path: only update i32 var, defer boxing to loop exit
        builder.def_var(vl.i32_vars[j], raw_i32);
    } else {
        // Non-tracked register: box and store in i64 var
        let boxed = type_guards::emit_box_int32(builder, raw_i32);
        builder.def_var(reg_vars[reg.index() as usize], boxed);
    }
}

/// Materialize all i32 variables (registers AND locals) back to their i64 (NaN-boxed) form.
/// Call this on every edge leaving the versioned loop body (overflow, loop exit, fallback).
fn materialize_i32_vars(
    builder: &mut FunctionBuilder<'_>,
    reg_vars: &[Variable],
    vl: &VersionedLoop,
) {
    for (&reg_idx, &j) in &vl.reg_to_i32 {
        let raw = builder.use_var(vl.i32_vars[j]);
        let boxed = type_guards::emit_box_int32(builder, raw);
        builder.def_var(reg_vars[reg_idx as usize], boxed);
    }
}

/// Materialize all i32 state (registers + locals) back to NaN-boxed form.
/// Use on every exit from the versioned loop body.
fn materialize_all_i32(
    builder: &mut FunctionBuilder<'_>,
    reg_vars: &[Variable],
    local_vars: &[Variable],
    vl: &VersionedLoop,
) {
    materialize_i32_vars(builder, reg_vars, vl);
    for (&local_idx, &j) in &vl.local_to_i32 {
        let raw = builder.use_var(vl.i32_local_vars[j]);
        let boxed = type_guards::emit_box_int32(builder, raw);
        builder.def_var(local_vars[local_idx as usize], boxed);
    }
}

/// Read a register as NaN-boxed i64 in a versioned loop body.
/// For tracked registers, reads the authoritative i32 var and boxes on-the-fly.
/// For non-tracked registers, reads the i64 var directly.
fn read_reg_versioned(
    builder: &mut FunctionBuilder<'_>,
    reg_vars: &[Variable],
    vl: &VersionedLoop,
    reg: Register,
) -> Value {
    if let Some(&j) = vl.reg_to_i32.get(&reg.0) {
        let raw = builder.use_var(vl.i32_vars[j]);
        type_guards::emit_box_int32(builder, raw)
    } else {
        builder.use_var(reg_vars[reg.index() as usize])
    }
}

#[inline]
fn try_emit_versioned_fused_compare_condition(
    builder: &mut FunctionBuilder<'_>,
    reg_vars: &[Variable],
    vl: &VersionedLoop,
    instructions: &[Instruction],
    jump_pc: usize,
    cond: Register,
) -> Option<Value> {
    let prev_pc = jump_pc.checked_sub(1)?;
    let (lhs, rhs, cc) = fused_versioned_compare_cc(instructions.get(prev_pc)?, cond)?;
    if !can_fuse_versioned_compare_branch(instructions, prev_pc, cond) {
        return None;
    }
    if !vl.reg_to_i32.contains_key(&lhs.0) || !vl.reg_to_i32.contains_key(&rhs.0) {
        return None;
    }

    let left = read_reg_i32(builder, reg_vars, vl, lhs);
    let right = read_reg_i32(builder, reg_vars, vl, rhs);
    Some(builder.ins().icmp(cc, left, right))
}

/// Emit a `return BAILOUT_SENTINEL` — signals the caller to re-execute
/// in the interpreter.
fn emit_bailout_return(builder: &mut FunctionBuilder<'_>) {
    let sentinel = builder.ins().iconst(types::I64, BAILOUT_SENTINEL);
    builder.ins().return_(&[sentinel]);
}

/// Record bailout telemetry AND dump live local/register state to deopt buffers.
///
/// When `local_vars` and `reg_vars` are non-empty, reads each value from its
/// Cranelift SSA variable and writes it to the deopt buffer pointed to by the
/// JitContext fields `deopt_locals_ptr` / `deopt_regs_ptr`. This enables precise
/// interpreter resume from the bailout PC instead of restarting from PC 0.
fn emit_record_bailout_with_state(
    builder: &mut FunctionBuilder<'_>,
    ctx_ptr: Value,
    pc: usize,
    reason: BailoutReason,
    local_vars: &[Variable],
    reg_vars: &[Variable],
    deopt_site: Option<&DeoptResumeSite>,
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
    let live_locals = deopt_site.map(|site| site.live_locals.as_slice());
    if !local_vars.is_empty() {
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
        if let Some(indices) = live_locals {
            for &index in indices {
                if let Some(&var) = local_vars.get(index as usize) {
                    let val = builder.use_var(var);
                    builder
                        .ins()
                        .store(MemFlags::trusted(), val, locals_ptr, i32::from(index) * 8);
                }
            }
        } else {
            for (i, &var) in local_vars.iter().enumerate() {
                let val = builder.use_var(var);
                builder
                    .ins()
                    .store(MemFlags::trusted(), val, locals_ptr, (i * 8) as i32);
            }
        }
        builder.ins().jump(after_locals_block, &[]);
        builder.switch_to_block(after_locals_block);
    }

    // Dump registers to deopt buffer
    let live_registers = deopt_site.map(|site| site.live_registers.as_slice());
    if !reg_vars.is_empty() {
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
        if let Some(indices) = live_registers {
            for &index in indices {
                if let Some(&var) = reg_vars.get(index as usize) {
                    let val = builder.use_var(var);
                    builder
                        .ins()
                        .store(MemFlags::trusted(), val, regs_ptr, i32::from(index) * 8);
                }
            }
        } else {
            for (i, &var) in reg_vars.iter().enumerate() {
                let val = builder.use_var(var);
                builder
                    .ins()
                    .store(MemFlags::trusted(), val, regs_ptr, (i * 8) as i32);
            }
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
    local_vars: &[Variable],
    reg_vars: &[Variable],
    deopt_site: Option<&DeoptResumeSite>,
) {
    emit_record_bailout_with_state(
        builder, ctx_ptr, pc, reason, local_vars, reg_vars, deopt_site,
    );
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
/// don't match the speculative fast path.
///
/// If no generic helper is available, falls back to the standard bailout.
#[allow(clippy::too_many_arguments)]
fn lower_guarded_with_generic_fallback(
    builder: &mut FunctionBuilder<'_>,
    guarded: type_guards::GuardedResult,
    generic_ref: Option<cranelift_codegen::ir::FuncRef>,
    generic_args: &[Value],
    ctx_ptr: Value,
    pc: usize,
    reason: BailoutReason,
    local_vars: &[Variable],
    reg_vars: &[Variable],
    deopt_site: Option<&DeoptResumeSite>,
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
        emit_bailout_return_with_state(
            builder, ctx_ptr, pc, reason, local_vars, reg_vars, deopt_site,
        );
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
            local_vars,
            reg_vars,
            deopt_site,
        );
    }
    builder.switch_to_block(guarded.merge_block);
    guarded.result
}

/// Lower a guarded result by bailing out directly on slow-path.
///
/// This keeps hot fast paths fully native in JIT code and lets the interpreter
/// handle uncommon megamorphic/spec-rare cases after deopt.
#[allow(clippy::too_many_arguments)]
fn lower_guarded_with_bailout(
    builder: &mut FunctionBuilder<'_>,
    guarded: type_guards::GuardedResult,
    ctx_ptr: Value,
    pc: usize,
    reason: BailoutReason,
    local_vars: &[Variable],
    reg_vars: &[Variable],
    deopt_site: Option<&DeoptResumeSite>,
) -> Value {
    builder.switch_to_block(guarded.slow_block);
    emit_bailout_return_with_state(
        builder, ctx_ptr, pc, reason, local_vars, reg_vars, deopt_site,
    );
    builder.switch_to_block(guarded.merge_block);
    guarded.result
}

/// Emit tier-up budget decrement and check at a backward jump.
///
/// Every backward jump (loop back-edge) decrements a budget counter in JitContext.
/// When the budget reaches zero, calls CheckTierUp helper to see if IC state
/// changed since compilation. If recompilation needed, bails out; otherwise
/// resets budget and continues.
///
/// This enables single-call multi-loop functions to recompile mid-execution
/// when inner loops warm up new ICs (V8 Maglev-style tier-up).
fn emit_tier_up_budget_check(
    builder: &mut FunctionBuilder<'_>,
    ctx_ptr: Value,
    tier_up_ref: cranelift_codegen::ir::FuncRef,
    target_block: cranelift_codegen::ir::Block,
    bailout_pc: usize,
) {
    // Decrement budget: ctx.tier_up_budget -= 1
    let budget_addr = builder
        .ins()
        .iadd_imm(ctx_ptr, JIT_CTX_TIER_UP_BUDGET_OFFSET as i64);
    let old_budget = builder
        .ins()
        .load(types::I64, MemFlags::trusted(), budget_addr, 0);
    let new_budget = builder.ins().iadd_imm(old_budget, -1);
    builder
        .ins()
        .store(MemFlags::trusted(), new_budget, budget_addr, 0);

    // Check if budget expired (new_budget <= 0)
    let expired = builder
        .ins()
        .icmp_imm(IntCC::SignedLessThanOrEqual, new_budget, 0);
    let tier_up_block = builder.create_block();
    builder
        .ins()
        .brif(expired, tier_up_block, &[], target_block, &[]);

    // Tier-up check: call helper
    builder.switch_to_block(tier_up_block);
    let result = builder.ins().call(tier_up_ref, &[ctx_ptr]);
    let result_val = builder.inst_results(result)[0];

    let sentinel = builder.ins().iconst(types::I64, BAILOUT_SENTINEL);
    let needs_recompile = builder.ins().icmp(IntCC::Equal, result_val, sentinel);
    let bail_block = builder.create_block();
    builder
        .ins()
        .brif(needs_recompile, bail_block, &[], target_block, &[]);

    // Bailout: store reason and PC, return BAILOUT_SENTINEL
    builder.switch_to_block(bail_block);
    let reason_addr = builder
        .ins()
        .iadd_imm(ctx_ptr, JIT_CTX_BAILOUT_REASON_OFFSET as i64);
    let reason_code = builder
        .ins()
        .iconst(types::I64, BailoutReason::HelperReturnedSentinel.code());
    builder
        .ins()
        .store(MemFlags::trusted(), reason_code, reason_addr, 0);
    let pc_addr = builder
        .ins()
        .iadd_imm(ctx_ptr, JIT_CTX_BAILOUT_PC_OFFSET as i64);
    let pc_val = builder.ins().iconst(types::I64, bailout_pc as i64);
    builder.ins().store(MemFlags::trusted(), pc_val, pc_addr, 0);
    builder.ins().return_(&[sentinel]);
}

/// Emit inline monomorphic property read (V8/JSC-style shape check + direct load).
///
/// For inline properties (offset < INLINE_PROPERTY_COUNT=8), emits:
/// 1. Extract pointer from NaN-boxed value (band with PAYLOAD_MASK)
/// 2. Verify TAG_PTR_OBJECT tag
/// 3. Load shape_tag from obj_ptr + 0, compare with expected
/// 4. Load SlotMeta, verify data property
/// 5. Load Value from inline_slots at known offset
///
/// On any mismatch, falls back to full GetPropConst helper.
/// No function call on the monomorphic fast path (~11 instructions vs ~35).
#[allow(clippy::too_many_arguments)]
fn emit_inline_prop_read(
    builder: &mut FunctionBuilder<'_>,
    full_ref: cranelift_codegen::ir::FuncRef,
    obj_val: Value,
    ctx_ptr: Value,
    shape_id: u64,
    offset: u32,
    name_index: u32,
    ic_index: u16,
    layout: &crate::runtime_helpers::JsObjectLayoutOffsets,
) -> Value {
    use crate::type_guards::{PAYLOAD_MASK, PTR_MASK};

    let merge_block = builder.create_block();
    builder.append_block_param(merge_block, types::I64);
    let slow_block = builder.create_block();

    // 1. Tag check: is this a TAG_PTR_OBJECT?
    let tag_mask = builder.ins().iconst(types::I64, PTR_MASK);
    let tag = builder.ins().band(obj_val, tag_mask);
    let expected_tag = builder
        .ins()
        .iconst(types::I64, 0x7FFC_0000_0000_0000_u64 as i64);
    let is_obj = builder.ins().icmp(IntCC::Equal, tag, expected_tag);
    let tag_ok = builder.create_block();
    builder.ins().brif(is_obj, tag_ok, &[], slow_block, &[]);

    builder.switch_to_block(tag_ok);

    // 2. Extract raw pointer
    let payload_mask = builder.ins().iconst(types::I64, PAYLOAD_MASK);
    let obj_ptr = builder.ins().band(obj_val, payload_mask);

    // 3. Shape check: load shape_tag (offset 0) and compare
    let shape_tag = builder
        .ins()
        .load(types::I64, MemFlags::trusted(), obj_ptr, 0);
    let expected_shape = builder.ins().iconst(types::I64, shape_id as i64);
    let shape_match = builder.ins().icmp(IntCC::Equal, shape_tag, expected_shape);
    let shape_ok = builder.create_block();
    builder
        .ins()
        .brif(shape_match, shape_ok, &[], slow_block, &[]);

    builder.switch_to_block(shape_ok);

    // 4. Meta check: load inline_meta[offset], verify it's a data property
    let meta_byte_offset = layout.inline_meta_data + offset as i32;
    let meta = builder
        .ins()
        .load(types::I8, MemFlags::trusted(), obj_ptr, meta_byte_offset);
    let meta_i32 = builder.ins().uextend(types::I32, meta);
    let kind_mask = builder
        .ins()
        .iconst(types::I32, crate::runtime_helpers::SLOTMETA_KIND_MASK);
    let kind = builder.ins().band(meta_i32, kind_mask);
    let data_kind = builder
        .ins()
        .iconst(types::I32, crate::runtime_helpers::SLOTMETA_KIND_DATA);
    let is_data = builder.ins().icmp(IntCC::Equal, kind, data_kind);
    let data_ok = builder.create_block();
    builder.ins().brif(is_data, data_ok, &[], slow_block, &[]);

    builder.switch_to_block(data_ok);

    // 5. Load value from inline_slots[offset]
    let value_byte_offset = layout.inline_slots_data + (offset as i32) * 8;
    let value = builder
        .ins()
        .load(types::I64, MemFlags::trusted(), obj_ptr, value_byte_offset);
    builder.ins().jump(merge_block, &[BlockArg::Value(value)]);

    // Slow path: call full GetPropConst
    builder.switch_to_block(slow_block);
    let name_idx_val = builder.ins().iconst(types::I64, name_index as i64);
    let ic_idx_val = builder.ins().iconst(types::I64, ic_index as i64);
    let full_call = builder
        .ins()
        .call(full_ref, &[ctx_ptr, obj_val, name_idx_val, ic_idx_val]);
    let full_result = builder.inst_results(full_call)[0];

    let sentinel = builder.ins().iconst(types::I64, BAILOUT_SENTINEL);
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

/// Emit polymorphic inline property read (V8/JSC-style linear scan).
///
/// For 2-4 shape entries with own properties (depth == 0) and offset < 8:
/// 1. Tag check: is object pointer?
/// 2. Extract raw pointer, load shape_tag
/// 3. Linear scan: compare shape_tag with each entry's shape_id
///    - Match → load meta, verify data property, load value from inline_slots
/// 4. No match → fall to GetPropConst helper
///
/// Pure Cranelift IR on the fast path — zero function calls for known shapes.
#[allow(clippy::too_many_arguments)]
fn emit_polymorphic_inline_read(
    builder: &mut FunctionBuilder<'_>,
    full_ref: cranelift_codegen::ir::FuncRef,
    obj_val: Value,
    ctx_ptr: Value,
    entries: &[(u64, u32)], // (shape_id, offset) — pre-filtered: depth==0, offset<8
    name_index: u32,
    ic_index: u16,
    layout: &crate::runtime_helpers::JsObjectLayoutOffsets,
) -> Value {
    use crate::type_guards::{PAYLOAD_MASK, PTR_MASK};

    let merge_block = builder.create_block();
    builder.append_block_param(merge_block, types::I64);
    let slow_block = builder.create_block();

    // 1. Tag check: is TAG_PTR_OBJECT?
    let tag_mask = builder.ins().iconst(types::I64, PTR_MASK);
    let tag = builder.ins().band(obj_val, tag_mask);
    let expected_tag = builder
        .ins()
        .iconst(types::I64, 0x7FFC_0000_0000_0000_u64 as i64);
    let is_obj = builder.ins().icmp(IntCC::Equal, tag, expected_tag);
    let tag_ok = builder.create_block();
    builder.ins().brif(is_obj, tag_ok, &[], slow_block, &[]);

    builder.switch_to_block(tag_ok);

    // 2. Extract raw pointer + load shape_tag
    let payload_mask = builder.ins().iconst(types::I64, PAYLOAD_MASK);
    let obj_ptr = builder.ins().band(obj_val, payload_mask);
    let shape_tag = builder
        .ins()
        .load(types::I64, MemFlags::trusted(), obj_ptr, 0);

    // 3. Linear scan over shapes
    let mut next_check = builder.create_block();
    for (i, &(shape_id, offset)) in entries.iter().enumerate() {
        let is_last = i == entries.len() - 1;
        let expected_shape = builder.ins().iconst(types::I64, shape_id as i64);
        let shape_match = builder.ins().icmp(IntCC::Equal, shape_tag, expected_shape);
        let hit_block = builder.create_block();
        let miss_target = if is_last { slow_block } else { next_check };
        builder
            .ins()
            .brif(shape_match, hit_block, &[], miss_target, &[]);

        builder.switch_to_block(hit_block);

        // Meta check: is_data?
        let meta_byte_offset = layout.inline_meta_data + offset as i32;
        let meta = builder
            .ins()
            .load(types::I8, MemFlags::trusted(), obj_ptr, meta_byte_offset);
        let meta_i32 = builder.ins().uextend(types::I32, meta);
        let kind_mask = builder
            .ins()
            .iconst(types::I32, crate::runtime_helpers::SLOTMETA_KIND_MASK);
        let kind = builder.ins().band(meta_i32, kind_mask);
        let data_kind = builder
            .ins()
            .iconst(types::I32, crate::runtime_helpers::SLOTMETA_KIND_DATA);
        let is_data = builder.ins().icmp(IntCC::Equal, kind, data_kind);
        let data_ok = builder.create_block();
        builder.ins().brif(is_data, data_ok, &[], slow_block, &[]);

        builder.switch_to_block(data_ok);

        // Load value from inline_slots
        let value_byte_offset = layout.inline_slots_data + (offset as i32) * 8;
        let value = builder
            .ins()
            .load(types::I64, MemFlags::trusted(), obj_ptr, value_byte_offset);
        builder.ins().jump(merge_block, &[BlockArg::Value(value)]);

        if !is_last {
            builder.switch_to_block(next_check);
            next_check = builder.create_block();
        }
    }

    // Slow path: full GetPropConst
    builder.switch_to_block(slow_block);
    let name_idx_val = builder.ins().iconst(types::I64, name_index as i64);
    let ic_idx_val = builder.ins().iconst(types::I64, ic_index as i64);
    let full_call = builder
        .ins()
        .call(full_ref, &[ctx_ptr, obj_val, name_idx_val, ic_idx_val]);
    let full_result = builder.inst_results(full_call)[0];

    let sentinel = builder.ins().iconst(types::I64, BAILOUT_SENTINEL);
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

/// Emit runtime IC probe read for cold ICs (V8/JSC-style adaptive inline cache).
///
/// For IC sites that were Uninitialized at compile time, reads the JIT IC probe
/// table at runtime. If the IC has warmed up since compilation (probe.state ==
/// STATE_MONO_INLINE), does an inline shape check + direct memory load without
/// any function call. Falls back to the full helper on miss.
///
/// This gives us V8-like behavior: the first iteration goes through the helper
/// (which warms up the IC + probe), and all subsequent iterations take the inline
/// fast path — without requiring function-level recompilation.
#[allow(clippy::too_many_arguments)]
fn emit_runtime_ic_probe_read(
    builder: &mut FunctionBuilder<'_>,
    full_ref: cranelift_codegen::ir::FuncRef,
    obj_val: Value,
    ctx_ptr: Value,
    name_index: u32,
    ic_index: u16,
    layout: &crate::runtime_helpers::JsObjectLayoutOffsets,
    pc: usize,
    local_vars: &[Variable],
    reg_vars: &[Variable],
    deopt_site: Option<&DeoptResumeSite>,
) -> Value {
    use crate::type_guards::{PAYLOAD_MASK, PTR_MASK};
    use otter_vm_bytecode::function::JitIcProbe;

    let merge_block = builder.create_block();
    builder.append_block_param(merge_block, types::I64);
    let slow_block = builder.create_block();

    // 1. Load ic_probes_ptr from JitContext
    let probes_ptr = builder.ins().load(
        types::I64,
        MemFlags::trusted(),
        ctx_ptr,
        JIT_CTX_IC_PROBES_PTR_OFFSET,
    );

    // 2. Check probes_ptr is not null
    let null = builder.ins().iconst(types::I64, 0);
    let probes_not_null = builder.ins().icmp(IntCC::NotEqual, probes_ptr, null);
    let probe_check = builder.create_block();
    builder
        .ins()
        .brif(probes_not_null, probe_check, &[], slow_block, &[]);

    builder.switch_to_block(probe_check);

    // 3. Compute probe entry address: probes_ptr + ic_index * 16
    let probe_byte_offset = (ic_index as i64) * (JitIcProbe::SIZE as i64);
    let probe_ptr = builder.ins().iadd_imm(probes_ptr, probe_byte_offset);

    // 4. Load probe.state (offset 12) and check == STATE_MONO_INLINE (1)
    let probe_state = builder.ins().load(
        types::I32,
        MemFlags::trusted(),
        probe_ptr,
        JitIcProbe::STATE_OFFSET,
    );
    let mono_state = builder
        .ins()
        .iconst(types::I32, JitIcProbe::STATE_MONO_INLINE as i64);
    let is_mono = builder.ins().icmp(IntCC::Equal, probe_state, mono_state);
    let inline_block = builder.create_block();
    builder
        .ins()
        .brif(is_mono, inline_block, &[], slow_block, &[]);

    builder.switch_to_block(inline_block);

    // 5. Load probe.shape_id (offset 0) and probe.offset (offset 8)
    let probe_shape = builder.ins().load(
        types::I64,
        MemFlags::trusted(),
        probe_ptr,
        JitIcProbe::SHAPE_ID_OFFSET,
    );
    let probe_offset = builder.ins().load(
        types::I32,
        MemFlags::trusted(),
        probe_ptr,
        JitIcProbe::OFFSET_OFFSET,
    );

    // 6. Check offset < 8 (inline property limit)
    let inline_limit = builder.ins().iconst(types::I32, 8);
    let offset_ok = builder
        .ins()
        .icmp(IntCC::UnsignedLessThan, probe_offset, inline_limit);
    let offset_ok_block = builder.create_block();
    builder
        .ins()
        .brif(offset_ok, offset_ok_block, &[], slow_block, &[]);

    builder.switch_to_block(offset_ok_block);

    // 7. Tag check: is this a TAG_PTR_OBJECT?
    let tag_mask = builder.ins().iconst(types::I64, PTR_MASK);
    let tag = builder.ins().band(obj_val, tag_mask);
    let expected_tag = builder
        .ins()
        .iconst(types::I64, 0x7FFC_0000_0000_0000_u64 as i64);
    let is_obj = builder.ins().icmp(IntCC::Equal, tag, expected_tag);
    let tag_ok = builder.create_block();
    builder.ins().brif(is_obj, tag_ok, &[], slow_block, &[]);

    builder.switch_to_block(tag_ok);

    // 8. Extract raw pointer
    let payload_mask = builder.ins().iconst(types::I64, PAYLOAD_MASK);
    let obj_ptr = builder.ins().band(obj_val, payload_mask);

    // 9. Shape check: load shape_tag from obj_ptr + 0, compare with probe_shape
    let shape_tag = builder
        .ins()
        .load(types::I64, MemFlags::trusted(), obj_ptr, 0);
    let shape_match = builder.ins().icmp(IntCC::Equal, shape_tag, probe_shape);
    let shape_ok = builder.create_block();
    builder
        .ins()
        .brif(shape_match, shape_ok, &[], slow_block, &[]);

    builder.switch_to_block(shape_ok);

    // 10. Meta check: load inline_meta[offset], verify it's a data property
    // meta_addr = obj_ptr + layout.inline_meta_data + offset (as i64)
    let meta_base = builder
        .ins()
        .iadd_imm(obj_ptr, layout.inline_meta_data as i64);
    let probe_offset_i64 = builder.ins().uextend(types::I64, probe_offset);
    let meta_addr = builder.ins().iadd(meta_base, probe_offset_i64);
    let meta = builder
        .ins()
        .load(types::I8, MemFlags::trusted(), meta_addr, 0);
    let meta_i32 = builder.ins().uextend(types::I32, meta);
    let kind_mask = builder
        .ins()
        .iconst(types::I32, crate::runtime_helpers::SLOTMETA_KIND_MASK);
    let kind = builder.ins().band(meta_i32, kind_mask);
    let data_kind = builder
        .ins()
        .iconst(types::I32, crate::runtime_helpers::SLOTMETA_KIND_DATA);
    let is_data = builder.ins().icmp(IntCC::Equal, kind, data_kind);
    let data_ok = builder.create_block();
    builder.ins().brif(is_data, data_ok, &[], slow_block, &[]);

    builder.switch_to_block(data_ok);

    // 11. Load value from inline_slots[offset]
    // value_addr = obj_ptr + layout.inline_slots_data + offset * 8
    let slots_base = builder
        .ins()
        .iadd_imm(obj_ptr, layout.inline_slots_data as i64);
    let offset_bytes = builder.ins().ishl_imm(probe_offset_i64, 3); // * 8
    let value_addr = builder.ins().iadd(slots_base, offset_bytes);
    let value = builder
        .ins()
        .load(types::I64, MemFlags::trusted(), value_addr, 0);
    builder.ins().jump(merge_block, &[BlockArg::Value(value)]);

    // Slow path: call full GetPropConst helper
    builder.switch_to_block(slow_block);
    let name_idx_val = builder.ins().iconst(types::I64, name_index as i64);
    let ic_idx_val = builder.ins().iconst(types::I64, ic_index as i64);
    let full_call = builder
        .ins()
        .call(full_ref, &[ctx_ptr, obj_val, name_idx_val, ic_idx_val]);
    let full_result = builder.inst_results(full_call)[0];

    let sentinel = builder.ins().iconst(types::I64, BAILOUT_SENTINEL);
    let full_bail = builder.ins().icmp(IntCC::Equal, full_result, sentinel);
    let bail_block = builder.create_block();
    let full_ok = builder.create_block();
    builder.ins().brif(full_bail, bail_block, &[], full_ok, &[]);

    builder.switch_to_block(bail_block);
    emit_bailout_return_with_state(
        builder,
        ctx_ptr,
        pc,
        BailoutReason::HelperReturnedSentinel,
        local_vars,
        reg_vars,
        deopt_site,
    );

    builder.switch_to_block(full_ok);
    builder
        .ins()
        .jump(merge_block, &[BlockArg::Value(full_result)]);

    builder.switch_to_block(merge_block);
    builder.block_params(merge_block)[0]
}

/// Emit runtime IC probe write for cold SetPropConst ICs.
///
/// Same architecture as runtime IC probe read but for property writes:
/// 1. Read probe table at runtime → check state == MONO_INLINE
/// 2. Shape check + meta check (data + writable)
/// 3. Value check: only inline-store non-heap values (numbers, int32, undefined, etc.)
///    Heap values (objects, strings) fall to helper for GC write barrier safety.
/// 4. Direct store to inline_slots[offset]
///
/// Falls back to the full SetPropConst helper on any miss.
#[allow(clippy::too_many_arguments)]
fn emit_runtime_ic_probe_write(
    builder: &mut FunctionBuilder<'_>,
    full_ref: cranelift_codegen::ir::FuncRef,
    barrier_ref: Option<cranelift_codegen::ir::FuncRef>,
    obj_val: Value,
    write_val: Value,
    ctx_ptr: Value,
    name_index: u32,
    ic_index: u16,
    layout: &crate::runtime_helpers::JsObjectLayoutOffsets,
    pc: usize,
    local_vars: &[Variable],
    reg_vars: &[Variable],
    deopt_site: Option<&DeoptResumeSite>,
) {
    use crate::type_guards::{PAYLOAD_MASK, PTR_MASK};
    use otter_vm_bytecode::function::JitIcProbe;

    let done_block = builder.create_block();
    let slow_block = builder.create_block();

    // 1. Load ic_probes_ptr from JitContext
    let probes_ptr = builder.ins().load(
        types::I64,
        MemFlags::trusted(),
        ctx_ptr,
        JIT_CTX_IC_PROBES_PTR_OFFSET,
    );

    // 2. Check probes_ptr is not null
    let null = builder.ins().iconst(types::I64, 0);
    let probes_not_null = builder.ins().icmp(IntCC::NotEqual, probes_ptr, null);
    let probe_check = builder.create_block();
    builder
        .ins()
        .brif(probes_not_null, probe_check, &[], slow_block, &[]);

    builder.switch_to_block(probe_check);

    // 3. Compute probe entry address
    let probe_byte_offset = (ic_index as i64) * (JitIcProbe::SIZE as i64);
    let probe_ptr = builder.ins().iadd_imm(probes_ptr, probe_byte_offset);

    // 4. Load probe.state, check == STATE_MONO_INLINE
    let probe_state = builder.ins().load(
        types::I32,
        MemFlags::trusted(),
        probe_ptr,
        JitIcProbe::STATE_OFFSET,
    );
    let mono_state = builder
        .ins()
        .iconst(types::I32, JitIcProbe::STATE_MONO_INLINE as i64);
    let is_mono = builder.ins().icmp(IntCC::Equal, probe_state, mono_state);
    let inline_block = builder.create_block();
    builder
        .ins()
        .brif(is_mono, inline_block, &[], slow_block, &[]);

    builder.switch_to_block(inline_block);

    // 5. Load shape_id and offset from probe
    let probe_shape = builder.ins().load(
        types::I64,
        MemFlags::trusted(),
        probe_ptr,
        JitIcProbe::SHAPE_ID_OFFSET,
    );
    let probe_offset = builder.ins().load(
        types::I32,
        MemFlags::trusted(),
        probe_ptr,
        JitIcProbe::OFFSET_OFFSET,
    );

    // 6. Check offset < 8
    let inline_limit = builder.ins().iconst(types::I32, 8);
    let offset_ok = builder
        .ins()
        .icmp(IntCC::UnsignedLessThan, probe_offset, inline_limit);
    let offset_ok_block = builder.create_block();
    builder
        .ins()
        .brif(offset_ok, offset_ok_block, &[], slow_block, &[]);

    builder.switch_to_block(offset_ok_block);

    // 7. Tag check on obj: is TAG_PTR_OBJECT?
    let tag_mask = builder.ins().iconst(types::I64, PTR_MASK);
    let tag = builder.ins().band(obj_val, tag_mask);
    let expected_tag = builder
        .ins()
        .iconst(types::I64, 0x7FFC_0000_0000_0000_u64 as i64);
    let is_obj = builder.ins().icmp(IntCC::Equal, tag, expected_tag);
    let tag_ok = builder.create_block();
    builder.ins().brif(is_obj, tag_ok, &[], slow_block, &[]);

    builder.switch_to_block(tag_ok);

    // 8. Extract raw pointer
    let payload_mask = builder.ins().iconst(types::I64, PAYLOAD_MASK);
    let obj_ptr = builder.ins().band(obj_val, payload_mask);

    // 9. Shape check
    let shape_tag = builder
        .ins()
        .load(types::I64, MemFlags::trusted(), obj_ptr, 0);
    let shape_match = builder.ins().icmp(IntCC::Equal, shape_tag, probe_shape);
    let shape_ok = builder.create_block();
    builder
        .ins()
        .brif(shape_match, shape_ok, &[], slow_block, &[]);

    builder.switch_to_block(shape_ok);

    // 10. Meta check: is_data + is_writable
    let meta_base = builder
        .ins()
        .iadd_imm(obj_ptr, layout.inline_meta_data as i64);
    let probe_offset_i64 = builder.ins().uextend(types::I64, probe_offset);
    let meta_addr = builder.ins().iadd(meta_base, probe_offset_i64);
    let meta = builder
        .ins()
        .load(types::I8, MemFlags::trusted(), meta_addr, 0);
    let meta_i32 = builder.ins().uextend(types::I32, meta);
    let dw_mask = builder.ins().iconst(
        types::I32,
        crate::runtime_helpers::SLOTMETA_DATA_WRITABLE_MASK,
    );
    let dw_bits = builder.ins().band(meta_i32, dw_mask);
    let dw_expected = builder
        .ins()
        .iconst(types::I32, crate::runtime_helpers::SLOTMETA_DATA_WRITABLE);
    let is_data_writable = builder.ins().icmp(IntCC::Equal, dw_bits, dw_expected);
    let dw_ok = builder.create_block();
    builder
        .ins()
        .brif(is_data_writable, dw_ok, &[], slow_block, &[]);

    builder.switch_to_block(dw_ok);

    // 11. Direct store to inline_slots[offset] + GC write barrier
    let slots_base = builder
        .ins()
        .iadd_imm(obj_ptr, layout.inline_slots_data as i64);
    let offset_bytes = builder.ins().ishl_imm(probe_offset_i64, 3);
    let value_addr = builder.ins().iadd(slots_base, offset_bytes);

    if let Some(barrier) = barrier_ref {
        // Store unconditionally (both heap and non-heap)
        builder
            .ins()
            .store(MemFlags::trusted(), write_val, value_addr, 0);

        // Call barrier only for heap values
        let val_tag = builder.ins().band(write_val, tag_mask);
        let heap_threshold = builder
            .ins()
            .iconst(types::I64, 0x7FFC_0000_0000_0000_u64 as i64);
        let is_heap =
            builder
                .ins()
                .icmp(IntCC::UnsignedGreaterThanOrEqual, val_tag, heap_threshold);
        let barrier_block = builder.create_block();
        builder
            .ins()
            .brif(is_heap, barrier_block, &[], done_block, &[]);

        builder.switch_to_block(barrier_block);
        builder.ins().call(barrier, &[write_val]);
        builder.ins().jump(done_block, &[]);
    } else {
        // No barrier — only inline non-heap values
        let val_tag = builder.ins().band(write_val, tag_mask);
        let heap_threshold = builder
            .ins()
            .iconst(types::I64, 0x7FFC_0000_0000_0000_u64 as i64);
        let is_non_heap = builder
            .ins()
            .icmp(IntCC::UnsignedLessThan, val_tag, heap_threshold);
        let store_ok = builder.create_block();
        builder
            .ins()
            .brif(is_non_heap, store_ok, &[], slow_block, &[]);

        builder.switch_to_block(store_ok);
        builder
            .ins()
            .store(MemFlags::trusted(), write_val, value_addr, 0);
        builder.ins().jump(done_block, &[]);
    }

    // Slow path: call full SetPropConst helper
    builder.switch_to_block(slow_block);
    let name_idx_val = builder.ins().iconst(types::I64, name_index as i64);
    let ic_idx_val = builder.ins().iconst(types::I64, ic_index as i64);
    let full_call = builder.ins().call(
        full_ref,
        &[ctx_ptr, obj_val, name_idx_val, write_val, ic_idx_val],
    );
    let full_result = builder.inst_results(full_call)[0];

    let sentinel = builder.ins().iconst(types::I64, BAILOUT_SENTINEL);
    let full_bail = builder.ins().icmp(IntCC::Equal, full_result, sentinel);
    let bail_block = builder.create_block();
    builder
        .ins()
        .brif(full_bail, bail_block, &[], done_block, &[]);

    builder.switch_to_block(bail_block);
    emit_bailout_return_with_state(
        builder,
        ctx_ptr,
        pc,
        BailoutReason::HelperReturnedSentinel,
        local_vars,
        reg_vars,
        deopt_site,
    );

    builder.switch_to_block(done_block);
}

/// Emit monomorphic property read with fallback to full GetPropConst.
///
/// 1. Call GetPropMono(obj, shape_id, offset) — lightweight, no JitContext
/// 2. If BAILOUT → call full GetPropConst(ctx, obj, name_idx, ic_idx)
/// 3. If still BAILOUT → bail out function
/// 4. Merge results from either path
#[allow(clippy::too_many_arguments)]
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

/// Emit inline monomorphic property write (compile-time shape constant).
///
/// For inline properties (offset < 8) with compile-time monomorphic IC:
/// 1. Tag check (is object pointer?)
/// 2. Shape check (load shape_tag, compare with expected)
/// 3. Meta check (is data + writable?)
/// 4. Direct store to inline_slots[offset]
/// 5. If value is heap-tagged → call GC write barrier helper
///
/// Falls to SetPropConst helper only on tag/shape/meta mismatch.
/// Heap values are stored inline + barrier (no full helper round-trip).
#[allow(clippy::too_many_arguments)]
fn emit_inline_prop_write(
    builder: &mut FunctionBuilder<'_>,
    full_ref: cranelift_codegen::ir::FuncRef,
    barrier_ref: Option<cranelift_codegen::ir::FuncRef>,
    obj_val: Value,
    write_val: Value,
    ctx_ptr: Value,
    shape_id: u64,
    offset: u32,
    name_index: u32,
    ic_index: u16,
    layout: &crate::runtime_helpers::JsObjectLayoutOffsets,
    pc: usize,
    local_vars: &[Variable],
    reg_vars: &[Variable],
    deopt_site: Option<&DeoptResumeSite>,
) {
    use crate::type_guards::{PAYLOAD_MASK, PTR_MASK};

    let done_block = builder.create_block();
    let slow_block = builder.create_block();

    // 1. Tag check: is this a TAG_PTR_OBJECT?
    let tag_mask = builder.ins().iconst(types::I64, PTR_MASK);
    let tag = builder.ins().band(obj_val, tag_mask);
    let expected_tag = builder
        .ins()
        .iconst(types::I64, 0x7FFC_0000_0000_0000_u64 as i64);
    let is_obj = builder.ins().icmp(IntCC::Equal, tag, expected_tag);
    let tag_ok = builder.create_block();
    builder.ins().brif(is_obj, tag_ok, &[], slow_block, &[]);

    builder.switch_to_block(tag_ok);

    // 2. Extract raw pointer
    let payload_mask = builder.ins().iconst(types::I64, PAYLOAD_MASK);
    let obj_ptr = builder.ins().band(obj_val, payload_mask);

    // 3. Shape check
    let shape_tag = builder
        .ins()
        .load(types::I64, MemFlags::trusted(), obj_ptr, 0);
    let expected_shape = builder.ins().iconst(types::I64, shape_id as i64);
    let shape_match = builder.ins().icmp(IntCC::Equal, shape_tag, expected_shape);
    let shape_ok = builder.create_block();
    builder
        .ins()
        .brif(shape_match, shape_ok, &[], slow_block, &[]);

    builder.switch_to_block(shape_ok);

    // 4. Meta check: is_data + is_writable (combined check)
    let meta_byte_offset = layout.inline_meta_data + offset as i32;
    let meta = builder
        .ins()
        .load(types::I8, MemFlags::trusted(), obj_ptr, meta_byte_offset);
    let meta_i32 = builder.ins().uextend(types::I32, meta);
    let dw_mask = builder.ins().iconst(
        types::I32,
        crate::runtime_helpers::SLOTMETA_DATA_WRITABLE_MASK,
    );
    let dw_bits = builder.ins().band(meta_i32, dw_mask);
    let dw_expected = builder
        .ins()
        .iconst(types::I32, crate::runtime_helpers::SLOTMETA_DATA_WRITABLE);
    let is_dw = builder.ins().icmp(IntCC::Equal, dw_bits, dw_expected);
    let dw_ok = builder.create_block();
    builder.ins().brif(is_dw, dw_ok, &[], slow_block, &[]);

    builder.switch_to_block(dw_ok);

    // 5. Direct store + GC write barrier
    let value_byte_offset = layout.inline_slots_data + (offset as i32) * 8;

    if let Some(barrier) = barrier_ref {
        // Store unconditionally (both heap and non-heap values)
        builder
            .ins()
            .store(MemFlags::trusted(), write_val, obj_ptr, value_byte_offset);

        // Call barrier only for heap values (tag >= 0x7FFC)
        let val_tag = builder.ins().band(write_val, tag_mask);
        let heap_threshold = builder
            .ins()
            .iconst(types::I64, 0x7FFC_0000_0000_0000_u64 as i64);
        let is_heap =
            builder
                .ins()
                .icmp(IntCC::UnsignedGreaterThanOrEqual, val_tag, heap_threshold);
        let barrier_block = builder.create_block();
        builder
            .ins()
            .brif(is_heap, barrier_block, &[], done_block, &[]);

        builder.switch_to_block(barrier_block);
        builder.ins().call(barrier, &[write_val]);
        builder.ins().jump(done_block, &[]);
    } else {
        // No barrier available — only inline non-heap values (no GC barrier needed)
        let val_tag = builder.ins().band(write_val, tag_mask);
        let heap_threshold = builder
            .ins()
            .iconst(types::I64, 0x7FFC_0000_0000_0000_u64 as i64);
        let is_non_heap = builder
            .ins()
            .icmp(IntCC::UnsignedLessThan, val_tag, heap_threshold);
        let store_block = builder.create_block();
        builder
            .ins()
            .brif(is_non_heap, store_block, &[], slow_block, &[]);

        builder.switch_to_block(store_block);
        builder
            .ins()
            .store(MemFlags::trusted(), write_val, obj_ptr, value_byte_offset);
        builder.ins().jump(done_block, &[]);
    }

    // Slow path: full SetPropConst helper (tag/shape/meta mismatch)
    builder.switch_to_block(slow_block);
    let name_idx_val = builder.ins().iconst(types::I64, name_index as i64);
    let ic_idx_val = builder.ins().iconst(types::I64, ic_index as i64);
    let full_call = builder.ins().call(
        full_ref,
        &[ctx_ptr, obj_val, name_idx_val, write_val, ic_idx_val],
    );
    let full_result = builder.inst_results(full_call)[0];

    let sentinel = builder.ins().iconst(types::I64, BAILOUT_SENTINEL);
    let full_bail = builder.ins().icmp(IntCC::Equal, full_result, sentinel);
    let bail_block = builder.create_block();
    builder
        .ins()
        .brif(full_bail, bail_block, &[], done_block, &[]);

    builder.switch_to_block(bail_block);
    emit_bailout_return_with_state(
        builder,
        ctx_ptr,
        pc,
        BailoutReason::HelperReturnedSentinel,
        local_vars,
        reg_vars,
        deopt_site,
    );

    builder.switch_to_block(done_block);
}

/// Emit monomorphic property write with fallback to full SetPropConst.
///
/// 1. Call SetPropMono(obj, shape_id, offset, value) — lightweight, no JitContext
/// 2. If BAILOUT → call full SetPropConst(ctx, obj, name_idx, value, ic_idx)
/// 3. If still BAILOUT → bail out function
#[allow(clippy::too_many_arguments)]
fn emit_mono_set_with_fallback(
    builder: &mut FunctionBuilder<'_>,
    mono_ref: cranelift_codegen::ir::FuncRef,
    full_ref: cranelift_codegen::ir::FuncRef,
    obj_val: Value,
    write_val: Value,
    ctx_ptr: Value,
    shape_id: u64,
    offset: u32,
    name_index: u32,
    ic_index: u16,
) {
    // Fast path: monomorphic helper
    let shape_const = builder.ins().iconst(types::I64, shape_id as i64);
    let offset_const = builder.ins().iconst(types::I64, offset as i64);
    let mono_call = builder
        .ins()
        .call(mono_ref, &[obj_val, shape_const, offset_const, write_val]);
    let mono_result = builder.inst_results(mono_call)[0];

    let sentinel = builder.ins().iconst(types::I64, BAILOUT_SENTINEL);
    let mono_bail = builder.ins().icmp(IntCC::Equal, mono_result, sentinel);
    let slow_block = builder.create_block();
    let continue_block = builder.create_block();
    let mono_ok = builder.create_block();
    builder.ins().brif(mono_bail, slow_block, &[], mono_ok, &[]);

    // Mono hit → continue
    builder.switch_to_block(mono_ok);
    builder.ins().jump(continue_block, &[]);

    // Slow path: full SetPropConst
    builder.switch_to_block(slow_block);
    let name_idx_val = builder.ins().iconst(types::I64, name_index as i64);
    let ic_idx_val = builder.ins().iconst(types::I64, ic_index as i64);
    let full_call = builder.ins().call(
        full_ref,
        &[ctx_ptr, obj_val, name_idx_val, write_val, ic_idx_val],
    );
    let full_result = builder.inst_results(full_call)[0];

    let full_bail = builder.ins().icmp(IntCC::Equal, full_result, sentinel);
    let bail_block = builder.create_block();
    let full_ok = builder.create_block();
    builder.ins().brif(full_bail, bail_block, &[], full_ok, &[]);

    builder.switch_to_block(bail_block);
    emit_bailout_return(builder);

    builder.switch_to_block(full_ok);
    builder.ins().jump(continue_block, &[]);

    builder.switch_to_block(continue_block);
}

/// Emit dense array element read with fallback to full GetElem.
///
/// 1. Call GetElemDense(obj, index, 0) — lightweight, no JitContext
/// 2. If BAILOUT → call full GetElem(ctx, obj, index, ic_idx)
/// 3. If still BAILOUT → bail out function
/// 4. Merge results from either path
/// Emit inline dense array element read using JsObject cached element fields.
///
/// Three-tier fast path:
/// 1. Inline: tag check → extract obj_ptr → load cached elements_kind/len/data →
///    kind == Object → index < len → load value → not hole → return value
/// 2. Dense helper: GetElemDense (syncs cache on call)
/// 3. Full helper: GetElem with IC
///
/// After the first dense helper call syncs the cache, subsequent iterations
/// take the inline path — no function call at all.
#[allow(clippy::too_many_arguments)]
fn emit_inline_dense_elem_with_fallback(
    builder: &mut FunctionBuilder<'_>,
    dense_ref: cranelift_codegen::ir::FuncRef,
    full_ref: cranelift_codegen::ir::FuncRef,
    obj_val: Value,
    idx_val: Value,
    ctx_ptr: Value,
    ic_index: u16,
    pc: usize,
    local_vars: &[Variable],
    reg_vars: &[Variable],
    deopt_site: Option<&DeoptResumeSite>,
) -> Value {
    use crate::runtime_helpers::{
        ELEMENTS_KIND_OBJECT, JSOBJECT_ELEMENTS_DATA_OFFSET, JSOBJECT_ELEMENTS_KIND_OFFSET,
        JSOBJECT_ELEMENTS_LEN_OFFSET,
    };
    use crate::type_guards::{PAYLOAD_MASK, PTR_MASK};

    let merge_block = builder.create_block();
    builder.append_block_param(merge_block, types::I64);
    let helper_block = builder.create_block();

    // 1. Tag check: is obj a TAG_PTR_OBJECT?
    let tag_mask = builder.ins().iconst(types::I64, PTR_MASK);
    let tag = builder.ins().band(obj_val, tag_mask);
    let expected_tag = builder
        .ins()
        .iconst(types::I64, 0x7FFC_0000_0000_0000_u64 as i64);
    let is_obj = builder.ins().icmp(IntCC::Equal, tag, expected_tag);
    let tag_ok = builder.create_block();
    builder.ins().brif(is_obj, tag_ok, &[], helper_block, &[]);

    builder.switch_to_block(tag_ok);

    // 2. Extract raw pointer
    let payload_mask = builder.ins().iconst(types::I64, PAYLOAD_MASK);
    let obj_ptr = builder.ins().band(obj_val, payload_mask);

    // 3. Load cached elements_kind, check == OBJECT (2)
    let kind = builder.ins().load(
        types::I8,
        MemFlags::trusted(),
        obj_ptr,
        JSOBJECT_ELEMENTS_KIND_OFFSET,
    );
    let kind_i32 = builder.ins().uextend(types::I32, kind);
    let expected_kind = builder.ins().iconst(types::I32, ELEMENTS_KIND_OBJECT);
    let kind_ok = builder.ins().icmp(IntCC::Equal, kind_i32, expected_kind);
    let kind_check = builder.create_block();
    builder
        .ins()
        .brif(kind_ok, kind_check, &[], helper_block, &[]);

    builder.switch_to_block(kind_check);

    // 4. Extract int32 index from NaN-boxed value
    let idx_tag_mask = builder
        .ins()
        .iconst(types::I64, 0xFFFF_FFFF_0000_0000_u64 as i64);
    let idx_tag = builder.ins().band(idx_val, idx_tag_mask);
    let expected_int_tag = builder
        .ins()
        .iconst(types::I64, 0x7FF8_0001_0000_0000_u64 as i64);
    let is_int = builder.ins().icmp(IntCC::Equal, idx_tag, expected_int_tag);
    let int_ok = builder.create_block();
    builder.ins().brif(is_int, int_ok, &[], helper_block, &[]);

    builder.switch_to_block(int_ok);

    // 5. Extract index value (lower 32 bits), check non-negative
    let idx_i32 = builder.ins().ireduce(types::I32, idx_val);
    let zero_i32 = builder.ins().iconst(types::I32, 0);
    let idx_non_neg = builder
        .ins()
        .icmp(IntCC::SignedGreaterThanOrEqual, idx_i32, zero_i32);
    let idx_ok = builder.create_block();
    builder
        .ins()
        .brif(idx_non_neg, idx_ok, &[], helper_block, &[]);

    builder.switch_to_block(idx_ok);

    // 6. Load cached elements_len, check index < len
    let elem_len = builder.ins().load(
        types::I32,
        MemFlags::trusted(),
        obj_ptr,
        JSOBJECT_ELEMENTS_LEN_OFFSET,
    );
    let in_bounds = builder
        .ins()
        .icmp(IntCC::UnsignedLessThan, idx_i32, elem_len);
    let bounds_ok = builder.create_block();
    builder
        .ins()
        .brif(in_bounds, bounds_ok, &[], helper_block, &[]);

    builder.switch_to_block(bounds_ok);

    // 7. Load cached elements_data pointer
    let elem_data = builder.ins().load(
        types::I64,
        MemFlags::trusted(),
        obj_ptr,
        JSOBJECT_ELEMENTS_DATA_OFFSET,
    );

    // 8. Check elements_data is not null
    let null = builder.ins().iconst(types::I64, 0);
    let data_not_null = builder.ins().icmp(IntCC::NotEqual, elem_data, null);
    let data_ok = builder.create_block();
    builder
        .ins()
        .brif(data_not_null, data_ok, &[], helper_block, &[]);

    builder.switch_to_block(data_ok);

    // 9. Load value from data[index] (Value = 8 bytes)
    let idx_i64 = builder.ins().uextend(types::I64, idx_i32);
    let byte_offset = builder.ins().ishl_imm(idx_i64, 3); // index * 8
    let value_addr = builder.ins().iadd(elem_data, byte_offset);
    let value = builder
        .ins()
        .load(types::I64, MemFlags::trusted(), value_addr, 0);

    // 10. Hole check: Value::HOLE is a special bit pattern
    // Hole = undefined NaN-boxed with a specific marker. Check it's not hole.
    let hole_bits = builder
        .ins()
        .iconst(types::I64, value_constants::HOLE_BITS as i64);
    let is_hole = builder.ins().icmp(IntCC::Equal, value, hole_bits);
    let not_hole = builder.create_block();
    builder
        .ins()
        .brif(is_hole, helper_block, &[], not_hole, &[]);

    builder.switch_to_block(not_hole);
    builder.ins().jump(merge_block, &[BlockArg::Value(value)]);

    // Helper fallback: dense helper → full helper
    builder.switch_to_block(helper_block);
    let helper_result = emit_dense_elem_with_fallback(
        builder, dense_ref, full_ref, obj_val, idx_val, ctx_ptr, ic_index, pc, local_vars,
        reg_vars, deopt_site,
    );
    builder
        .ins()
        .jump(merge_block, &[BlockArg::Value(helper_result)]);

    builder.switch_to_block(merge_block);
    builder.block_params(merge_block)[0]
}

#[allow(clippy::too_many_arguments)]
fn emit_dense_elem_with_fallback(
    builder: &mut FunctionBuilder<'_>,
    dense_ref: cranelift_codegen::ir::FuncRef,
    full_ref: cranelift_codegen::ir::FuncRef,
    obj_val: Value,
    idx_val: Value,
    ctx_ptr: Value,
    ic_index: u16,
    pc: usize,
    local_vars: &[Variable],
    reg_vars: &[Variable],
    deopt_site: Option<&DeoptResumeSite>,
) -> Value {
    let merge_block = builder.create_block();
    builder.append_block_param(merge_block, types::I64);

    // Fast path: dense array helper
    let zero = builder.ins().iconst(types::I64, 0);
    let dense_call = builder.ins().call(dense_ref, &[obj_val, idx_val, zero]);
    let dense_result = builder.inst_results(dense_call)[0];

    let sentinel = builder.ins().iconst(types::I64, BAILOUT_SENTINEL);
    let dense_bail = builder.ins().icmp(IntCC::Equal, dense_result, sentinel);
    let slow_block = builder.create_block();
    let dense_ok = builder.create_block();
    builder
        .ins()
        .brif(dense_bail, slow_block, &[], dense_ok, &[]);

    // Dense hit → merge
    builder.switch_to_block(dense_ok);
    builder
        .ins()
        .jump(merge_block, &[BlockArg::Value(dense_result)]);

    // Slow path: full GetElem
    builder.switch_to_block(slow_block);
    let ic_idx_val = builder.ins().iconst(types::I64, ic_index as i64);
    let full_call = builder
        .ins()
        .call(full_ref, &[ctx_ptr, obj_val, idx_val, ic_idx_val]);
    let full_result = builder.inst_results(full_call)[0];

    let full_bail = builder.ins().icmp(IntCC::Equal, full_result, sentinel);
    let bail_block = builder.create_block();
    let full_ok = builder.create_block();
    builder.ins().brif(full_bail, bail_block, &[], full_ok, &[]);

    builder.switch_to_block(bail_block);
    emit_bailout_return_with_state(
        builder,
        ctx_ptr,
        pc,
        BailoutReason::HelperReturnedSentinel,
        local_vars,
        reg_vars,
        deopt_site,
    );

    builder.switch_to_block(full_ok);
    builder
        .ins()
        .jump(merge_block, &[BlockArg::Value(full_result)]);

    builder.switch_to_block(merge_block);
    builder.block_params(merge_block)[0]
}

fn emit_inline_get_upvalue(builder: &mut FunctionBuilder<'_>, ctx_ptr: Value, idx: u16) -> Value {
    let upvalues_ptr = builder.ins().load(
        types::I64,
        MemFlags::trusted(),
        ctx_ptr,
        JIT_CTX_UPVALUES_PTR_OFFSET,
    );
    let upvalue_count = builder.ins().load(
        types::I32,
        MemFlags::trusted(),
        ctx_ptr,
        JIT_CTX_UPVALUE_COUNT_OFFSET,
    );
    let upvalue_count = builder.ins().uextend(types::I64, upvalue_count);
    let idx_val = builder.ins().iconst(types::I64, idx as i64);
    let zero = builder.ins().iconst(types::I64, 0);

    let bail_block = builder.create_block();
    let count_check_block = builder.create_block();
    let load_block = builder.create_block();
    let continue_block = builder.create_block();
    builder.append_block_param(continue_block, types::I64);

    let ptr_is_null = builder.ins().icmp(IntCC::Equal, upvalues_ptr, zero);
    builder
        .ins()
        .brif(ptr_is_null, bail_block, &[], count_check_block, &[]);

    builder.switch_to_block(count_check_block);
    let idx_is_oob = builder
        .ins()
        .icmp(IntCC::UnsignedGreaterThanOrEqual, idx_val, upvalue_count);
    builder
        .ins()
        .brif(idx_is_oob, bail_block, &[], load_block, &[]);

    builder.switch_to_block(load_block);
    let cell_stride = builder
        .ins()
        .iconst(types::I64, i64::from(JIT_UPVALUE_CELL_SIZE));
    let cell_offset = builder.ins().imul(idx_val, cell_stride);
    let cell_addr = builder.ins().iadd(upvalues_ptr, cell_offset);
    let cell_gcbox_ptr = builder.ins().load(
        types::I64,
        MemFlags::trusted(),
        cell_addr,
        JIT_UPVALUE_CELL_GCBOX_PTR_OFFSET,
    );
    let value_addr = builder.ins().iadd_imm(
        cell_gcbox_ptr,
        i64::from(JIT_UPVALUE_GCBOX_VALUE_OFFSET + JIT_UPVALUE_DATA_VALUE_OFFSET),
    );
    let value = builder
        .ins()
        .load(types::I64, MemFlags::trusted(), value_addr, 0);
    builder
        .ins()
        .jump(continue_block, &[BlockArg::Value(value)]);

    builder.switch_to_block(bail_block);
    emit_bailout_return(builder);

    builder.switch_to_block(continue_block);
    let result = builder.block_params(continue_block)[0];
    builder.seal_block(count_check_block);
    builder.seal_block(load_block);
    builder.seal_block(continue_block);
    result
}

#[allow(clippy::too_many_arguments)]
fn try_emit_binary_leaf_arith_call(
    builder: &mut FunctionBuilder<'_>,
    callee: &Function,
    caller_reg_vars: &[Variable],
    func_reg: Register,
    argc: u16,
    helpers: Option<&HelperRefs>,
    ctx_ptr: Value,
    pc: usize,
    local_vars: &[Variable],
    reg_vars: &[Variable],
    deopt_site: Option<&DeoptResumeSite>,
) -> Option<Value> {
    let spec = match_binary_leaf_arith(callee)?;
    let undef = builder.ins().iconst(types::I64, type_guards::TAG_UNDEFINED);
    let read_arg = |builder: &mut FunctionBuilder<'_>, param_idx: u16| {
        if param_idx < argc {
            read_reg(
                builder,
                caller_reg_vars,
                Register(func_reg.0 + 1 + param_idx),
            )
        } else {
            undef
        }
    };

    let left = read_arg(builder, spec.lhs_param);
    let right = read_arg(builder, spec.rhs_param);
    let callee_feedback = callee.feedback_vector.read();
    let hint = SpecializationHint::from_type_flags(
        callee_feedback
            .get(spec.feedback_index as usize)
            .map(|m| &m.type_observations),
    );
    let arith_hint = if matches!(hint, SpecializationHint::Int32) {
        SpecializationHint::Numeric
    } else {
        hint
    };
    let guarded = type_guards::emit_specialized_arith(builder, spec.op, left, right, arith_hint);

    Some(
        if spec.uses_generic_fallback && matches!(hint, SpecializationHint::Generic) {
            let generic_ref = helpers.and_then(|h| h.get(binary_leaf_generic_helper(spec.op)));
            lower_guarded_with_generic_fallback(
                builder,
                guarded,
                generic_ref,
                &[ctx_ptr, left, right],
                ctx_ptr,
                pc,
                BailoutReason::HelperReturnedSentinel,
                local_vars,
                reg_vars,
                deopt_site,
            )
        } else {
            lower_guarded_with_bailout(
                builder,
                guarded,
                ctx_ptr,
                pc,
                BailoutReason::TypeGuardFailure,
                local_vars,
                reg_vars,
                deopt_site,
            )
        },
    )
}

/// Returns true if the instruction is a control-flow terminator that emits its own
/// branch/return and should NOT have an implicit fallthrough jump appended.
#[allow(dead_code)]
fn is_terminator(inst: &Instruction) -> bool {
    matches!(
        inst,
        Instruction::Jump { .. }
            | Instruction::JumpIfTrue { .. }
            | Instruction::JumpIfFalse { .. }
            | Instruction::JumpIfNullish { .. }
            | Instruction::JumpIfNotNullish { .. }
            | Instruction::Return { .. }
            | Instruction::ReturnUndefined
            | Instruction::Throw { .. }
            | Instruction::TailCall { .. }
            | Instruction::ForInNext { .. }
    )
}

/// Pre-scan bytecode to determine which PCs start a new basic block ("leaders").
/// A PC is a leader if it is:
/// - PC 0 (always)
/// - Target of any branch instruction
/// - Fallthrough of any branch/return instruction
/// - A loop header (for versioned loop routing)
fn compute_block_leaders(instructions: &[Instruction], loop_headers: &[usize]) -> Vec<bool> {
    let len = instructions.len();
    let mut leaders = vec![false; len];
    if len > 0 {
        leaders[0] = true;
    }
    for &h in loop_headers {
        if h < len {
            leaders[h] = true;
        }
    }
    for (pc, inst) in instructions.iter().enumerate() {
        match inst {
            Instruction::Jump { offset } => {
                let t = (pc as i64 + offset.offset() as i64) as usize;
                if t < len {
                    leaders[t] = true;
                }
                if pc + 1 < len {
                    leaders[pc + 1] = true;
                }
            }
            Instruction::JumpIfTrue { offset, .. }
            | Instruction::JumpIfFalse { offset, .. }
            | Instruction::JumpIfNullish { offset, .. }
            | Instruction::JumpIfNotNullish { offset, .. }
            | Instruction::ForInNext { offset, .. } => {
                let t = (pc as i64 + offset.offset() as i64) as usize;
                if t < len {
                    leaders[t] = true;
                }
                if pc + 1 < len {
                    leaders[pc + 1] = true;
                }
            }
            Instruction::TryStart {
                catch_offset: offset,
            } => {
                let t = (pc as i64 + offset.offset() as i64) as usize;
                if t < len {
                    leaders[t] = true;
                }
            }
            Instruction::Return { .. }
            | Instruction::ReturnUndefined
            | Instruction::Throw { .. }
            | Instruction::TailCall { .. } => {
                if pc + 1 < len {
                    leaders[pc + 1] = true;
                }
            }
            _ => {}
        }
    }
    leaders
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
    let (feedback_snapshot, ic_snapshot, call_target_snapshot, ffi_call_info_snapshot): (
        Vec<_>,
        Vec<_>,
        Vec<_>,
        Vec<_>,
    ) = {
        let fv = function.feedback_vector.read();
        (
            fv.iter().map(|m| m.type_observations).collect(),
            fv.iter().map(|m| m.ic_state).collect(),
            fv.iter()
                .map(|m| (m.call_target_func_index, m.call_target_module_id))
                .collect(),
            fv.iter().map(|m| m.ffi_call_info_ptr).collect(),
        )
    };
    let deopt_metadata = build_deopt_metadata(function);
    let get_hint = |feedback_index: u16| -> SpecializationHint {
        SpecializationHint::from_type_flags(feedback_snapshot.get(feedback_index as usize))
    };

    let reg_count = function.register_count as usize;
    let mut reg_vars = Vec::with_capacity(reg_count);
    for _ in 0..reg_count {
        reg_vars.push(builder.declare_var(types::I64));
    }
    let local_count = function.local_count as usize;
    let mut local_vars = Vec::with_capacity(local_count);
    for _ in 0..local_count {
        local_vars.push(builder.declare_var(types::I64));
    }

    // --- Loop analysis (needed before block creation for leader computation) ---
    let versioned_loops = loop_analysis::detect_loops(instructions_ref, &feedback_snapshot);
    let osr_loop_headers_vec = collect_osr_loop_headers(&versioned_loops);

    // --- Block merging: only create blocks at basic-block leaders ---
    let leaders = compute_block_leaders(instructions_ref, &osr_loop_headers_vec);
    let mut blocks = Vec::with_capacity(instruction_count);
    let mut current_block = builder.create_block(); // PC 0 is always a leader
    blocks.push(current_block);
    for &is_leader in &leaders[1..] {
        if is_leader {
            current_block = builder.create_block();
        }
        blocks.push(current_block);
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
    for reg_var in &reg_vars {
        builder.def_var(*reg_var, undef);
    }
    let param_count = function.param_count as usize;
    for (idx, &local_var) in local_vars.iter().enumerate() {
        let init = if idx < param_count {
            init_param_local(builder, args_ptr, argc, idx, undef)
        } else {
            undef
        };
        builder.def_var(local_var, init);
    }
    // --- Loop versioning: create optimized blocks for qualified loops ---
    // (versioned_loops already computed above for leader analysis)

    // For each qualified loop, create:
    // - A pre-header block (type checks → branch to opt or guard path)
    // - Optimized blocks for each PC in the loop body (one per instruction)

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
        // Declare i32 Variables for each checked register
        let mut i32_vars = Vec::with_capacity(info.check_registers.len());
        let mut reg_to_i32 = std::collections::HashMap::new();
        for (j, &reg_idx) in info.check_registers.iter().enumerate() {
            let var = builder.declare_var(types::I32);
            i32_vars.push(var);
            reg_to_i32.insert(reg_idx, j);
        }

        // Collect locals accessed within the loop body for i32 tracking.
        // Track ALL read locals (not just read+written) so that loop-invariant
        // locals like loop bounds are unboxed once in the pre-header.
        let mut local_reads: std::collections::HashSet<u16> = std::collections::HashSet::new();
        for inst in &instructions_ref[info.header_pc..=info.back_edge_pc] {
            match inst {
                Instruction::GetLocal { idx, .. } => {
                    local_reads.insert(idx.index());
                }
                Instruction::GetLocal2 { idx1, idx2, .. } => {
                    local_reads.insert(idx1.index());
                    local_reads.insert(idx2.index());
                }
                _ => {}
            }
        }
        let mut i32_local_vars = Vec::new();
        let mut local_to_i32 = std::collections::HashMap::new();
        for &local_idx in &local_reads {
            if (local_idx as usize) < local_count {
                let j = i32_local_vars.len();
                let var = builder.declare_var(types::I32);
                i32_local_vars.push(var);
                local_to_i32.insert(local_idx, j);
            }
        }

        // Build backwards truncation set (V8/JSC-style).
        let wrapping_pcs = build_wrapping_set(instructions_ref, info.header_pc, info.back_edge_pc);

        // Shape hoisting: detect property reads on loop-invariant objects.
        // Build set of registers written inside the loop body.
        let mut shape_hoisted_ptrs = std::collections::HashMap::new();
        {
            let mut written_regs = std::collections::HashSet::new();
            for inst in &instructions_ref[info.header_pc..=info.back_edge_pc] {
                if let Some(dst) = instruction_dst_register(inst) {
                    written_regs.insert(dst);
                }
            }
            // Find GetPropConst on invariant receivers with warm monomorphic IC
            let mut seen_obj_regs = std::collections::HashSet::new();
            for inst in &instructions_ref[info.header_pc..=info.back_edge_pc] {
                let (obj_reg, ic_idx) = match inst {
                    Instruction::GetPropConst { obj, ic_index, .. } => (obj.0, *ic_index),
                    _ => continue,
                };
                if written_regs.contains(&obj_reg) || seen_obj_regs.contains(&obj_reg) {
                    continue;
                }
                if let Some(InlineCacheState::Monomorphic { depth: 0, offset, .. }) =
                    ic_snapshot.get(ic_idx as usize)
                {
                    if (*offset as usize) < 8 {
                        seen_obj_regs.insert(obj_reg);
                        let ptr_var = builder.declare_var(types::I64);
                        shape_hoisted_ptrs.insert(obj_reg, ptr_var);
                    }
                }
            }
        }

        header_to_preheader.insert(info.header_pc, pre_header);
        versioned.push(VersionedLoop {
            header_pc: info.header_pc,
            back_edge_pc: info.back_edge_pc,
            pre_header,
            opt_blocks,
            check_registers: info.check_registers.clone(),
            i32_vars,
            reg_to_i32,
            i32_local_vars,
            local_to_i32,
            wrapping_pcs,
            shape_hoisted_ptrs,
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
    let inline_sites = resolve_inline_candidates(instructions_ref, module_functions);
    let captured_locals = resolve_locally_captured_locals(instructions_ref, module_functions);
    let module_func_by_index: std::collections::HashMap<u32, &Function> = module_functions
        .iter()
        .map(|(idx, func)| (*idx, func))
        .collect();

    // --- OSR entry dispatch ---
    // osr_loop_headers_vec was computed above for leader analysis.
    // All detected loops are OSR-entry candidates.
    // Qualifying loops get routed through pre-headers for type guard checks;
    // non-qualifying loops jump directly into the baseline block at blocks[header_pc].
    let osr_loop_headers = &osr_loop_headers_vec;

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
        for (i, &local_var) in local_vars.iter().enumerate() {
            let val =
                builder
                    .ins()
                    .load(types::I64, MemFlags::trusted(), locals_ptr, (i * 8) as i32);
            builder.def_var(local_var, val);
        }

        // Load registers from deopt_regs buffer.
        let regs_ptr = builder.ins().load(
            types::I64,
            MemFlags::trusted(),
            ctx_ptr,
            JIT_CTX_DEOPT_REGS_PTR_OFFSET,
        );
        for (i, &reg_var) in reg_vars.iter().enumerate() {
            let val = builder
                .ins()
                .load(types::I64, MemFlags::trusted(), regs_ptr, (i * 8) as i32);
            builder.def_var(reg_var, val);
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
        let deopt_site = deopt_metadata.site(pc as u32);
        if leaders[pc] {
            builder.switch_to_block(blocks[pc]);
        } else if current_block_is_filled(builder) {
            continue;
        }
        let emit_bailout_return = |builder: &mut FunctionBuilder<'_>| {
            emit_bailout_return_with_state(
                builder,
                ctx_ptr,
                pc,
                BailoutReason::HelperReturnedSentinel,
                &local_vars,
                &reg_vars,
                deopt_site,
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
                &local_vars,
                &reg_vars,
                deopt_site,
            );
            builder.switch_to_block(continue_block);
            result
        };
        match instruction {
            Instruction::LoadUndefined { dst } => {
                let v = builder.ins().iconst(types::I64, type_guards::TAG_UNDEFINED);
                write_reg(builder, &reg_vars, *dst, v);
            }
            Instruction::LoadNull { dst } => {
                let v = builder.ins().iconst(types::I64, type_guards::TAG_NULL);
                write_reg(builder, &reg_vars, *dst, v);
            }
            Instruction::LoadTrue { dst } => {
                let v = builder.ins().iconst(types::I64, type_guards::TAG_TRUE);
                write_reg(builder, &reg_vars, *dst, v);
            }
            Instruction::LoadFalse { dst } => {
                let v = builder.ins().iconst(types::I64, type_guards::TAG_FALSE);
                write_reg(builder, &reg_vars, *dst, v);
            }
            Instruction::LoadInt8 { dst, value } => {
                let v = type_guards::emit_box_int32_const(builder, i32::from(*value));
                write_reg(builder, &reg_vars, *dst, v);
            }
            Instruction::LoadInt32 { dst, value } => {
                let v = type_guards::emit_box_int32_const(builder, *value);
                write_reg(builder, &reg_vars, *dst, v);
            }
            Instruction::LoadConst { dst, idx } => {
                if let Some(bits) = resolve_const_bits(constants, *idx) {
                    let v = builder.ins().iconst(types::I64, bits);
                    write_reg(builder, &reg_vars, *dst, v);
                } else {
                    let helper_ref = helpers
                        .and_then(|h| h.get(HelperKind::LoadConst))
                        .ok_or_else(|| unsupported(pc, instruction))?;
                    let idx_val = builder.ins().iconst(types::I64, i64::from(idx.index()));
                    let result =
                        emit_helper_call_with_bailout(builder, helper_ref, &[ctx_ptr, idx_val]);
                    write_reg(builder, &reg_vars, *dst, result);
                }
            }
            Instruction::GetLocal { dst, idx } => {
                let v = read_local(builder, &local_vars, *idx);
                write_reg(builder, &reg_vars, *dst, v);
            }
            Instruction::SetLocal { idx, src } => {
                let v = read_reg(builder, &reg_vars, *src);
                write_local(builder, &local_vars, *idx, v);
            }
            Instruction::Move { dst, src } => {
                let v = read_reg(builder, &reg_vars, *src);
                write_reg(builder, &reg_vars, *dst, v);
            }
            Instruction::Add {
                dst,
                lhs,
                rhs,
                feedback_index,
            } => {
                let left = read_reg(builder, &reg_vars, *lhs);
                let right = read_reg(builder, &reg_vars, *rhs);
                let hint = get_hint(*feedback_index);
                let likely_string =
                    add_likely_string_concat(instructions_ref, constants, pc, *lhs, *rhs);
                let effective_hint = if likely_string {
                    SpecializationHint::Generic
                } else {
                    hint
                };
                let arith_hint = if matches!(effective_hint, SpecializationHint::Int32) {
                    SpecializationHint::Numeric
                } else {
                    effective_hint
                };
                let guarded = type_guards::emit_specialized_arith(
                    builder,
                    ArithOp::Add,
                    left,
                    right,
                    arith_hint,
                );
                let out = if matches!(effective_hint, SpecializationHint::Generic) {
                    let generic_ref = helpers.and_then(|h| h.get(HelperKind::GenericAdd));
                    lower_guarded_with_generic_fallback(
                        builder,
                        guarded,
                        generic_ref,
                        &[ctx_ptr, left, right],
                        ctx_ptr,
                        pc,
                        BailoutReason::HelperReturnedSentinel,
                        &local_vars,
                        &reg_vars,
                        deopt_site,
                    )
                } else {
                    lower_guarded_with_bailout(
                        builder,
                        guarded,
                        ctx_ptr,
                        pc,
                        BailoutReason::TypeGuardFailure,
                        &local_vars,
                        &reg_vars,
                        deopt_site,
                    )
                };
                write_reg(builder, &reg_vars, *dst, out);
            }
            Instruction::Sub {
                dst,
                lhs,
                rhs,
                feedback_index,
            } => {
                let left = read_reg(builder, &reg_vars, *lhs);
                let right = read_reg(builder, &reg_vars, *rhs);
                let hint = get_hint(*feedback_index);
                let arith_hint = if matches!(hint, SpecializationHint::Int32) {
                    SpecializationHint::Numeric
                } else {
                    hint
                };
                let guarded = type_guards::emit_specialized_arith(
                    builder,
                    ArithOp::Sub,
                    left,
                    right,
                    arith_hint,
                );
                let out = if matches!(hint, SpecializationHint::Generic) {
                    let generic_ref = helpers.and_then(|h| h.get(HelperKind::GenericSub));
                    lower_guarded_with_generic_fallback(
                        builder,
                        guarded,
                        generic_ref,
                        &[ctx_ptr, left, right],
                        ctx_ptr,
                        pc,
                        BailoutReason::HelperReturnedSentinel,
                        &local_vars,
                        &reg_vars,
                        deopt_site,
                    )
                } else {
                    lower_guarded_with_bailout(
                        builder,
                        guarded,
                        ctx_ptr,
                        pc,
                        BailoutReason::TypeGuardFailure,
                        &local_vars,
                        &reg_vars,
                        deopt_site,
                    )
                };
                write_reg(builder, &reg_vars, *dst, out);
            }
            Instruction::Mul {
                dst,
                lhs,
                rhs,
                feedback_index,
            } => {
                let left = read_reg(builder, &reg_vars, *lhs);
                let right = read_reg(builder, &reg_vars, *rhs);
                let hint = get_hint(*feedback_index);
                let arith_hint = if matches!(hint, SpecializationHint::Int32) {
                    SpecializationHint::Numeric
                } else {
                    hint
                };
                let guarded = type_guards::emit_specialized_arith(
                    builder,
                    ArithOp::Mul,
                    left,
                    right,
                    arith_hint,
                );
                let out = if matches!(hint, SpecializationHint::Generic) {
                    let generic_ref = helpers.and_then(|h| h.get(HelperKind::GenericMul));
                    lower_guarded_with_generic_fallback(
                        builder,
                        guarded,
                        generic_ref,
                        &[ctx_ptr, left, right],
                        ctx_ptr,
                        pc,
                        BailoutReason::HelperReturnedSentinel,
                        &local_vars,
                        &reg_vars,
                        deopt_site,
                    )
                } else {
                    lower_guarded_with_bailout(
                        builder,
                        guarded,
                        ctx_ptr,
                        pc,
                        BailoutReason::TypeGuardFailure,
                        &local_vars,
                        &reg_vars,
                        deopt_site,
                    )
                };
                write_reg(builder, &reg_vars, *dst, out);
            }
            Instruction::Div {
                dst,
                lhs,
                rhs,
                feedback_index,
            } => {
                let left = read_reg(builder, &reg_vars, *lhs);
                let right = read_reg(builder, &reg_vars, *rhs);
                let hint = get_hint(*feedback_index);
                // JS division always returns f64 (even 4/2 → 2.0), so Int32 hint
                // still needs the numeric path for div-by-zero → Infinity handling.
                let guarded = match hint {
                    SpecializationHint::Float64 => {
                        type_guards::emit_guarded_f64_div(builder, left, right)
                    }
                    _ => type_guards::emit_guarded_numeric_div(builder, left, right),
                };
                let out = lower_guarded_with_bailout(
                    builder,
                    guarded,
                    ctx_ptr,
                    pc,
                    BailoutReason::TypeGuardFailure,
                    &local_vars,
                    &reg_vars,
                    deopt_site,
                );
                write_reg(builder, &reg_vars, *dst, out);
            }
            // Quickened arithmetic: keep fast path fully native; bail out on mismatch.
            Instruction::AddInt32 { dst, lhs, rhs, .. } => {
                let left = read_reg(builder, &reg_vars, *lhs);
                let right = read_reg(builder, &reg_vars, *rhs);
                let guarded =
                    type_guards::emit_guarded_i32_arith(builder, ArithOp::Add, left, right);
                let out = lower_guarded_with_bailout(
                    builder,
                    guarded,
                    ctx_ptr,
                    pc,
                    BailoutReason::TypeGuardFailure,
                    &local_vars,
                    &reg_vars,
                    deopt_site,
                );
                write_reg(builder, &reg_vars, *dst, out);
            }
            Instruction::SubInt32 { dst, lhs, rhs, .. } => {
                let left = read_reg(builder, &reg_vars, *lhs);
                let right = read_reg(builder, &reg_vars, *rhs);
                let guarded =
                    type_guards::emit_guarded_i32_arith(builder, ArithOp::Sub, left, right);
                let out = lower_guarded_with_bailout(
                    builder,
                    guarded,
                    ctx_ptr,
                    pc,
                    BailoutReason::TypeGuardFailure,
                    &local_vars,
                    &reg_vars,
                    deopt_site,
                );
                write_reg(builder, &reg_vars, *dst, out);
            }
            Instruction::MulInt32 { dst, lhs, rhs, .. } => {
                let left = read_reg(builder, &reg_vars, *lhs);
                let right = read_reg(builder, &reg_vars, *rhs);
                let guarded =
                    type_guards::emit_guarded_i32_arith(builder, ArithOp::Mul, left, right);
                let out = lower_guarded_with_bailout(
                    builder,
                    guarded,
                    ctx_ptr,
                    pc,
                    BailoutReason::TypeGuardFailure,
                    &local_vars,
                    &reg_vars,
                    deopt_site,
                );
                write_reg(builder, &reg_vars, *dst, out);
            }
            Instruction::DivInt32 { dst, lhs, rhs, .. } => {
                let left = read_reg(builder, &reg_vars, *lhs);
                let right = read_reg(builder, &reg_vars, *rhs);
                let guarded = type_guards::emit_guarded_i32_div(builder, left, right);
                let out = lower_guarded_with_bailout(
                    builder,
                    guarded,
                    ctx_ptr,
                    pc,
                    BailoutReason::TypeGuardFailure,
                    &local_vars,
                    &reg_vars,
                    deopt_site,
                );
                write_reg(builder, &reg_vars, *dst, out);
            }
            Instruction::AddNumber { dst, lhs, rhs, .. } => {
                let left = read_reg(builder, &reg_vars, *lhs);
                let right = read_reg(builder, &reg_vars, *rhs);
                let guarded =
                    type_guards::emit_guarded_numeric_arith(builder, ArithOp::Add, left, right);
                let out = lower_guarded_with_bailout(
                    builder,
                    guarded,
                    ctx_ptr,
                    pc,
                    BailoutReason::TypeGuardFailure,
                    &local_vars,
                    &reg_vars,
                    deopt_site,
                );
                write_reg(builder, &reg_vars, *dst, out);
            }
            Instruction::SubNumber { dst, lhs, rhs, .. } => {
                let left = read_reg(builder, &reg_vars, *lhs);
                let right = read_reg(builder, &reg_vars, *rhs);
                let guarded =
                    type_guards::emit_guarded_numeric_arith(builder, ArithOp::Sub, left, right);
                let out = lower_guarded_with_bailout(
                    builder,
                    guarded,
                    ctx_ptr,
                    pc,
                    BailoutReason::TypeGuardFailure,
                    &local_vars,
                    &reg_vars,
                    deopt_site,
                );
                write_reg(builder, &reg_vars, *dst, out);
            }
            Instruction::Mod { dst, lhs, rhs } => {
                let left = read_reg(builder, &reg_vars, *lhs);
                let right = read_reg(builder, &reg_vars, *rhs);
                let guarded = type_guards::emit_guarded_i32_mod(builder, left, right);
                let out = lower_guarded_with_bailout(
                    builder,
                    guarded,
                    ctx_ptr,
                    pc,
                    BailoutReason::TypeGuardFailure,
                    &local_vars,
                    &reg_vars,
                    deopt_site,
                );
                write_reg(builder, &reg_vars, *dst, out);
            }
            Instruction::Neg { dst, src } => {
                let val = read_reg(builder, &reg_vars, *src);
                let guarded = type_guards::emit_guarded_i32_neg(builder, val);
                let out = lower_guarded_with_bailout(
                    builder,
                    guarded,
                    ctx_ptr,
                    pc,
                    BailoutReason::TypeGuardFailure,
                    &local_vars,
                    &reg_vars,
                    deopt_site,
                );
                write_reg(builder, &reg_vars, *dst, out);
            }
            Instruction::Inc { dst, src } => {
                let val = read_reg(builder, &reg_vars, *src);
                let guarded = type_guards::emit_guarded_i32_inc(builder, val);
                let out = lower_guarded_with_bailout(
                    builder,
                    guarded,
                    ctx_ptr,
                    pc,
                    BailoutReason::TypeGuardFailure,
                    &local_vars,
                    &reg_vars,
                    deopt_site,
                );
                write_reg(builder, &reg_vars, *dst, out);
            }
            Instruction::Dec { dst, src } => {
                let val = read_reg(builder, &reg_vars, *src);
                let guarded = type_guards::emit_guarded_i32_dec(builder, val);
                let out = lower_guarded_with_bailout(
                    builder,
                    guarded,
                    ctx_ptr,
                    pc,
                    BailoutReason::TypeGuardFailure,
                    &local_vars,
                    &reg_vars,
                    deopt_site,
                );
                write_reg(builder, &reg_vars, *dst, out);
            }
            Instruction::BitAnd { dst, lhs, rhs } => {
                let left = read_reg(builder, &reg_vars, *lhs);
                let right = read_reg(builder, &reg_vars, *rhs);
                let guarded =
                    type_guards::emit_guarded_i32_bitwise(builder, BitwiseOp::And, left, right);
                let out = lower_guarded_with_bailout(
                    builder,
                    guarded,
                    ctx_ptr,
                    pc,
                    BailoutReason::TypeGuardFailure,
                    &local_vars,
                    &reg_vars,
                    deopt_site,
                );
                write_reg(builder, &reg_vars, *dst, out);
            }
            Instruction::BitOr { dst, lhs, rhs } => {
                let left = read_reg(builder, &reg_vars, *lhs);
                let right = read_reg(builder, &reg_vars, *rhs);
                let guarded =
                    type_guards::emit_guarded_i32_bitwise(builder, BitwiseOp::Or, left, right);
                let out = lower_guarded_with_bailout(
                    builder,
                    guarded,
                    ctx_ptr,
                    pc,
                    BailoutReason::TypeGuardFailure,
                    &local_vars,
                    &reg_vars,
                    deopt_site,
                );
                write_reg(builder, &reg_vars, *dst, out);
            }
            Instruction::BitXor { dst, lhs, rhs } => {
                let left = read_reg(builder, &reg_vars, *lhs);
                let right = read_reg(builder, &reg_vars, *rhs);
                let guarded =
                    type_guards::emit_guarded_i32_bitwise(builder, BitwiseOp::Xor, left, right);
                let out = lower_guarded_with_bailout(
                    builder,
                    guarded,
                    ctx_ptr,
                    pc,
                    BailoutReason::TypeGuardFailure,
                    &local_vars,
                    &reg_vars,
                    deopt_site,
                );
                write_reg(builder, &reg_vars, *dst, out);
            }
            Instruction::Shl { dst, lhs, rhs } => {
                let left = read_reg(builder, &reg_vars, *lhs);
                let right = read_reg(builder, &reg_vars, *rhs);
                let guarded =
                    type_guards::emit_guarded_i32_bitwise(builder, BitwiseOp::Shl, left, right);
                let out = lower_guarded_with_bailout(
                    builder,
                    guarded,
                    ctx_ptr,
                    pc,
                    BailoutReason::TypeGuardFailure,
                    &local_vars,
                    &reg_vars,
                    deopt_site,
                );
                write_reg(builder, &reg_vars, *dst, out);
            }
            Instruction::Shr { dst, lhs, rhs } => {
                let left = read_reg(builder, &reg_vars, *lhs);
                let right = read_reg(builder, &reg_vars, *rhs);
                let guarded =
                    type_guards::emit_guarded_i32_bitwise(builder, BitwiseOp::Shr, left, right);
                let out = lower_guarded_with_bailout(
                    builder,
                    guarded,
                    ctx_ptr,
                    pc,
                    BailoutReason::TypeGuardFailure,
                    &local_vars,
                    &reg_vars,
                    deopt_site,
                );
                write_reg(builder, &reg_vars, *dst, out);
            }
            Instruction::Ushr { dst, lhs, rhs } => {
                let left = read_reg(builder, &reg_vars, *lhs);
                let right = read_reg(builder, &reg_vars, *rhs);
                let guarded =
                    type_guards::emit_guarded_i32_bitwise(builder, BitwiseOp::Ushr, left, right);
                let out = lower_guarded_with_bailout(
                    builder,
                    guarded,
                    ctx_ptr,
                    pc,
                    BailoutReason::TypeGuardFailure,
                    &local_vars,
                    &reg_vars,
                    deopt_site,
                );
                write_reg(builder, &reg_vars, *dst, out);
            }
            Instruction::BitNot { dst, src } => {
                let val = read_reg(builder, &reg_vars, *src);
                let guarded = type_guards::emit_guarded_i32_bitnot(builder, val);
                let out = lower_guarded_with_bailout(
                    builder,
                    guarded,
                    ctx_ptr,
                    pc,
                    BailoutReason::TypeGuardFailure,
                    &local_vars,
                    &reg_vars,
                    deopt_site,
                );
                write_reg(builder, &reg_vars, *dst, out);
            }
            Instruction::Eq { dst, lhs, rhs } => {
                let left = read_reg(builder, &reg_vars, *lhs);
                let right = read_reg(builder, &reg_vars, *rhs);
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
                    &local_vars,
                    &reg_vars,
                    deopt_site,
                );
                write_reg(builder, &reg_vars, *dst, out);
            }
            Instruction::Ne { dst, lhs, rhs } => {
                let left = read_reg(builder, &reg_vars, *lhs);
                let right = read_reg(builder, &reg_vars, *rhs);
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
                    &local_vars,
                    &reg_vars,
                    deopt_site,
                );
                write_reg(builder, &reg_vars, *dst, out);
            }
            Instruction::StrictEq { dst, lhs, rhs } => {
                let left = read_reg(builder, &reg_vars, *lhs);
                let right = read_reg(builder, &reg_vars, *rhs);
                let out = type_guards::emit_strict_eq(builder, left, right, false);
                write_reg(builder, &reg_vars, *dst, out);
            }
            Instruction::StrictNe { dst, lhs, rhs } => {
                let left = read_reg(builder, &reg_vars, *lhs);
                let right = read_reg(builder, &reg_vars, *rhs);
                let out = type_guards::emit_strict_eq(builder, left, right, true);
                write_reg(builder, &reg_vars, *dst, out);
            }
            Instruction::Lt { dst, lhs, rhs } => {
                let left = read_reg(builder, &reg_vars, *lhs);
                let right = read_reg(builder, &reg_vars, *rhs);
                let guarded = type_guards::emit_guarded_numeric_cmp(
                    builder,
                    IntCC::SignedLessThan,
                    FloatCC::LessThan,
                    left,
                    right,
                );
                let out = lower_guarded_with_bailout(
                    builder,
                    guarded,
                    ctx_ptr,
                    pc,
                    BailoutReason::TypeGuardFailure,
                    &local_vars,
                    &reg_vars,
                    deopt_site,
                );
                write_reg(builder, &reg_vars, *dst, out);
            }
            Instruction::Le { dst, lhs, rhs } => {
                let left = read_reg(builder, &reg_vars, *lhs);
                let right = read_reg(builder, &reg_vars, *rhs);
                let guarded = type_guards::emit_guarded_numeric_cmp(
                    builder,
                    IntCC::SignedLessThanOrEqual,
                    FloatCC::LessThanOrEqual,
                    left,
                    right,
                );
                let out = lower_guarded_with_bailout(
                    builder,
                    guarded,
                    ctx_ptr,
                    pc,
                    BailoutReason::TypeGuardFailure,
                    &local_vars,
                    &reg_vars,
                    deopt_site,
                );
                write_reg(builder, &reg_vars, *dst, out);
            }
            Instruction::Gt { dst, lhs, rhs } => {
                let left = read_reg(builder, &reg_vars, *lhs);
                let right = read_reg(builder, &reg_vars, *rhs);
                let guarded = type_guards::emit_guarded_numeric_cmp(
                    builder,
                    IntCC::SignedGreaterThan,
                    FloatCC::GreaterThan,
                    left,
                    right,
                );
                let out = lower_guarded_with_bailout(
                    builder,
                    guarded,
                    ctx_ptr,
                    pc,
                    BailoutReason::TypeGuardFailure,
                    &local_vars,
                    &reg_vars,
                    deopt_site,
                );
                write_reg(builder, &reg_vars, *dst, out);
            }
            Instruction::Ge { dst, lhs, rhs } => {
                let left = read_reg(builder, &reg_vars, *lhs);
                let right = read_reg(builder, &reg_vars, *rhs);
                let guarded = type_guards::emit_guarded_numeric_cmp(
                    builder,
                    IntCC::SignedGreaterThanOrEqual,
                    FloatCC::GreaterThanOrEqual,
                    left,
                    right,
                );
                let out = lower_guarded_with_bailout(
                    builder,
                    guarded,
                    ctx_ptr,
                    pc,
                    BailoutReason::TypeGuardFailure,
                    &local_vars,
                    &reg_vars,
                    deopt_site,
                );
                write_reg(builder, &reg_vars, *dst, out);
            }
            Instruction::Not { dst, src } => {
                let val = read_reg(builder, &reg_vars, *src);
                let truthy = type_guards::emit_is_truthy(builder, val);
                let is_falsy = builder.ins().icmp_imm(IntCC::Equal, truthy, 0);
                let out = type_guards::emit_bool_to_nanbox(builder, is_falsy);
                write_reg(builder, &reg_vars, *dst, out);
            }
            Instruction::Jump { offset } => {
                let target = jump_target(pc, offset.offset(), instruction_count)?;
                let target_block = resolve_target(pc, target);
                builder.ins().jump(target_block, &[]);
                continue;
            }
            Instruction::JumpIfTrue { cond, offset } => {
                let jump_to = jump_target(pc, offset.offset(), instruction_count)?;
                let jump_block = resolve_target(pc, jump_to);
                let fallthrough = pc + 1;
                let condition_kind = classify_jump_condition(instructions_ref, pc, *cond);
                match condition_kind {
                    JumpConditionKind::Constant(true) => {
                        builder.ins().jump(jump_block, &[]);
                    }
                    JumpConditionKind::Constant(false) => {
                        if fallthrough < instruction_count {
                            let ft_block = resolve_target(pc, fallthrough);
                            builder.ins().jump(ft_block, &[]);
                        } else {
                            builder.ins().jump(exit, &[]);
                        }
                    }
                    JumpConditionKind::BoxedBoolean | JumpConditionKind::Generic => {
                        let cond_val = read_reg(builder, &reg_vars, *cond);
                        let is_truthy = emit_jump_truthy_value(builder, condition_kind, cond_val);
                        if fallthrough < instruction_count {
                            let ft_block = resolve_target(pc, fallthrough);
                            builder
                                .ins()
                                .brif(is_truthy, jump_block, &[], ft_block, &[]);
                        } else {
                            builder.ins().brif(is_truthy, jump_block, &[], exit, &[]);
                        }
                    }
                }
                continue;
            }
            Instruction::JumpIfFalse { cond, offset } => {
                let jump_to = jump_target(pc, offset.offset(), instruction_count)?;
                let jump_block = resolve_target(pc, jump_to);
                let fallthrough = pc + 1;
                let condition_kind = classify_jump_condition(instructions_ref, pc, *cond);
                match condition_kind {
                    JumpConditionKind::Constant(true) => {
                        if fallthrough < instruction_count {
                            let ft_block = resolve_target(pc, fallthrough);
                            builder.ins().jump(ft_block, &[]);
                        } else {
                            builder.ins().jump(exit, &[]);
                        }
                    }
                    JumpConditionKind::Constant(false) => {
                        builder.ins().jump(jump_block, &[]);
                    }
                    JumpConditionKind::BoxedBoolean | JumpConditionKind::Generic => {
                        let cond_val = read_reg(builder, &reg_vars, *cond);
                        let is_truthy = emit_jump_truthy_value(builder, condition_kind, cond_val);
                        if fallthrough < instruction_count {
                            let ft_block = resolve_target(pc, fallthrough);
                            builder
                                .ins()
                                .brif(is_truthy, ft_block, &[], jump_block, &[]);
                        } else {
                            builder.ins().brif(is_truthy, exit, &[], jump_block, &[]);
                        }
                    }
                }
                continue;
            }
            Instruction::JumpIfNullish { src, offset } => {
                let src_val = read_reg(builder, &reg_vars, *src);
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
                let src_val = read_reg(builder, &reg_vars, *src);
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
                let out = read_reg(builder, &reg_vars, *src);
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
                let obj_val = read_reg(builder, &reg_vars, *obj);

                // Try monomorphic fast path based on compile-time IC snapshot
                let mono_ic = ic_snapshot.get(*ic_index as usize).and_then(|ic| {
                    if let InlineCacheState::Monomorphic {
                        shape_id,
                        offset,
                        depth: 0,
                        ..
                    } = ic
                    {
                        Some((*shape_id, *offset))
                    } else {
                        None
                    }
                });
                // Polymorphic: extract own-property entries with offset < 8
                let poly_entries: Option<Vec<(u64, u32)>> =
                    ic_snapshot.get(*ic_index as usize).and_then(|ic| {
                        if let InlineCacheState::Polymorphic { count, entries } = ic {
                            let v: Vec<(u64, u32)> = entries[..(*count as usize)]
                                .iter()
                                .filter(|e| e.2 == 0 && (e.3 as usize) < 8)
                                .map(|e| (e.0, e.3))
                                .collect();
                            if v.len() >= 2 { Some(v) } else { None }
                        } else {
                            None
                        }
                    });
                let mono_ref = helpers.and_then(|h| h.get(HelperKind::GetPropMono));
                let layout = crate::runtime_helpers::jsobject_layout();

                let result = if let (Some((shape_id, offset)), Some(lo)) = (mono_ic, layout) {
                    if (offset as usize) < 8 {
                        emit_inline_prop_read(
                            builder,
                            full_ref,
                            obj_val,
                            ctx_ptr,
                            shape_id,
                            offset,
                            name.index(),
                            *ic_index,
                            &lo,
                        )
                    } else if let Some(mono_helper) = mono_ref {
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
                    }
                } else if let (Some(ref pe), Some(lo)) = (poly_entries, layout) {
                    // Polymorphic inline reads: 2-4 shape linear scan
                    emit_polymorphic_inline_read(
                        builder,
                        full_ref,
                        obj_val,
                        ctx_ptr,
                        pe,
                        name.index(),
                        *ic_index,
                        &lo,
                    )
                } else if let (Some((shape_id, offset)), Some(mono_helper)) = (mono_ic, mono_ref) {
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
                } else if let Some(lo) = layout {
                    // Cold IC: use runtime IC probe — reads probe table at
                    // runtime. After the first iteration warms up the IC,
                    // subsequent iterations take the inline fast path.
                    emit_runtime_ic_probe_read(
                        builder,
                        full_ref,
                        obj_val,
                        ctx_ptr,
                        name.index(),
                        *ic_index,
                        &lo,
                        pc,
                        &local_vars,
                        &reg_vars,
                        deopt_site,
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
                write_reg(builder, &reg_vars, *dst, result);
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
                let obj_val = read_local(builder, &local_vars, *local_idx);

                let mono_ic = ic_snapshot.get(*ic_index as usize).and_then(|ic| {
                    if let InlineCacheState::Monomorphic {
                        shape_id,
                        offset,
                        depth: 0,
                        ..
                    } = ic
                    {
                        Some((*shape_id, *offset))
                    } else {
                        None
                    }
                });
                let mono_ref = helpers.and_then(|h| h.get(HelperKind::GetPropMono));
                let layout = crate::runtime_helpers::jsobject_layout();

                let result = if let (Some((shape_id, offset)), Some(lo)) = (mono_ic, layout) {
                    if (offset as usize) < 8 {
                        emit_inline_prop_read(
                            builder,
                            full_ref,
                            obj_val,
                            ctx_ptr,
                            shape_id,
                            offset,
                            name.index(),
                            *ic_index,
                            &lo,
                        )
                    } else if let Some(mono_helper) = mono_ref {
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
                    }
                } else if let (Some((shape_id, offset)), Some(mono_helper)) = (mono_ic, mono_ref) {
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
                } else if let Some(lo) = layout {
                    emit_runtime_ic_probe_read(
                        builder,
                        full_ref,
                        obj_val,
                        ctx_ptr,
                        name.index(),
                        *ic_index,
                        &lo,
                        pc,
                        &local_vars,
                        &reg_vars,
                        deopt_site,
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
                write_reg(builder, &reg_vars, *dst, result);
            }
            Instruction::SetPropConst {
                obj,
                name,
                val,
                ic_index,
            } => {
                let full_ref = helpers
                    .and_then(|h| h.get(HelperKind::SetPropConst))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let obj_val = read_reg(builder, &reg_vars, *obj);
                let value = read_reg(builder, &reg_vars, *val);

                // Try monomorphic fast path based on compile-time IC snapshot
                let mono_ic = ic_snapshot.get(*ic_index as usize).and_then(|ic| {
                    if let InlineCacheState::Monomorphic {
                        shape_id,
                        offset,
                        depth: 0,
                        ..
                    } = ic
                    {
                        Some((*shape_id, *offset))
                    } else {
                        None
                    }
                });
                let mono_ref = helpers.and_then(|h| h.get(HelperKind::SetPropMono));
                let barrier_ref = helpers.and_then(|h| h.get(HelperKind::GcWriteBarrier));

                let layout = crate::runtime_helpers::jsobject_layout();

                if let (Some((shape_id, offset)), Some(lo)) = (mono_ic, layout) {
                    if (offset as usize) < 8 {
                        // Inline write: direct Cranelift store + barrier, no full helper
                        emit_inline_prop_write(
                            builder,
                            full_ref,
                            barrier_ref,
                            obj_val,
                            value,
                            ctx_ptr,
                            shape_id,
                            offset,
                            name.index(),
                            *ic_index,
                            &lo,
                            pc,
                            &local_vars,
                            &reg_vars,
                            deopt_site,
                        );
                    } else if let Some(mono_helper) = mono_ref {
                        emit_mono_set_with_fallback(
                            builder,
                            mono_helper,
                            full_ref,
                            obj_val,
                            value,
                            ctx_ptr,
                            shape_id,
                            offset,
                            name.index(),
                            *ic_index,
                        );
                    } else {
                        let name_idx = builder.ins().iconst(types::I64, name.index() as i64);
                        let ic_idx = builder.ins().iconst(types::I64, *ic_index as i64);
                        builder
                            .ins()
                            .call(full_ref, &[ctx_ptr, obj_val, name_idx, value, ic_idx]);
                    }
                } else if let (Some((shape_id, offset)), Some(mono_helper)) = (mono_ic, mono_ref) {
                    emit_mono_set_with_fallback(
                        builder,
                        mono_helper,
                        full_ref,
                        obj_val,
                        value,
                        ctx_ptr,
                        shape_id,
                        offset,
                        name.index(),
                        *ic_index,
                    );
                } else if let Some(lo) = layout {
                    // Cold IC: runtime IC probe for writes
                    emit_runtime_ic_probe_write(
                        builder,
                        full_ref,
                        barrier_ref,
                        obj_val,
                        value,
                        ctx_ptr,
                        name.index(),
                        *ic_index,
                        &lo,
                        pc,
                        &local_vars,
                        &reg_vars,
                        deopt_site,
                    );
                } else {
                    let name_idx = builder.ins().iconst(types::I64, name.index() as i64);
                    let ic_idx = builder.ins().iconst(types::I64, *ic_index as i64);
                    let call = builder
                        .ins()
                        .call(full_ref, &[ctx_ptr, obj_val, name_idx, value, ic_idx]);
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
            }
            Instruction::Call {
                dst,
                func,
                argc,
                ic_index,
            } => {
                // Check if this call site has a statically resolved inline candidate
                if let Some(candidate) = inline_sites.get(&pc) {
                    let callee = candidate.callee;
                    let callee_instrs = callee.instructions.read();
                    let callee_instr_count = callee_instrs.len();

                    if callee_instr_count == 0 {
                        // Empty function → return undefined
                        let undef_val =
                            builder.ins().iconst(types::I64, type_guards::TAG_UNDEFINED);
                        write_reg(builder, &reg_vars, *dst, undef_val);
                    } else {
                        // Create callee register and local variables
                        let callee_reg_count = callee.register_count as usize;
                        let mut callee_reg_vars = Vec::with_capacity(callee_reg_count);
                        for _ in 0..callee_reg_count {
                            callee_reg_vars.push(builder.declare_var(types::I64));
                        }
                        let callee_local_count = callee.local_count as usize;
                        let mut callee_local_vars = Vec::with_capacity(callee_local_count);
                        for _ in 0..callee_local_count {
                            callee_local_vars.push(builder.declare_var(types::I64));
                        }

                        // Initialize callee registers to undefined
                        let undef_val =
                            builder.ins().iconst(types::I64, type_guards::TAG_UNDEFINED);
                        for &callee_reg_var in &callee_reg_vars {
                            builder.def_var(callee_reg_var, undef_val);
                        }

                        // Map caller args → callee param locals
                        let callee_param_count = callee.param_count as usize;
                        for (idx, &callee_local_var) in callee_local_vars.iter().enumerate() {
                            let init = if idx < callee_param_count && idx < (*argc as usize) {
                                // Read argument from caller's register layout
                                // Args are in registers func.0+1, func.0+2, ...
                                read_reg(builder, &reg_vars, Register(func.0 + 1 + idx as u16))
                            } else {
                                undef_val
                            };
                            builder.def_var(callee_local_var, init);
                        }

                        // Create blocks for callee instructions + continuation
                        let mut callee_blocks = Vec::with_capacity(callee_instr_count);
                        for _ in 0..callee_instr_count {
                            callee_blocks.push(builder.create_block());
                        }
                        let mut callee_known_funcs: std::collections::HashMap<u16, u32> =
                            std::collections::HashMap::new();
                        let continuation = builder.create_block();
                        builder.append_block_param(continuation, types::I64);

                        // Jump to first callee block
                        builder.ins().jump(callee_blocks[0], &[]);

                        // Translate callee bytecode using callee's slots
                        for (ci, callee_inst) in callee_instrs.iter().enumerate() {
                            builder.switch_to_block(callee_blocks[ci]);
                            if let Some(dst_reg) = instruction_dst_register(callee_inst) {
                                callee_known_funcs.remove(&dst_reg);
                            }

                            match callee_inst {
                                // Returns → jump to continuation with value
                                Instruction::Return { src } => {
                                    let out = read_reg(builder, &callee_reg_vars, *src);
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
                                    write_reg(builder, &callee_reg_vars, *d, v);
                                }
                                Instruction::LoadNull { dst: d } => {
                                    let v = builder.ins().iconst(types::I64, type_guards::TAG_NULL);
                                    write_reg(builder, &callee_reg_vars, *d, v);
                                }
                                Instruction::LoadTrue { dst: d } => {
                                    let v = builder.ins().iconst(types::I64, type_guards::TAG_TRUE);
                                    write_reg(builder, &callee_reg_vars, *d, v);
                                }
                                Instruction::LoadFalse { dst: d } => {
                                    let v =
                                        builder.ins().iconst(types::I64, type_guards::TAG_FALSE);
                                    write_reg(builder, &callee_reg_vars, *d, v);
                                }
                                Instruction::LoadInt8 { dst: d, value } => {
                                    let v = type_guards::emit_box_int32_const(
                                        builder,
                                        i32::from(*value),
                                    );
                                    write_reg(builder, &callee_reg_vars, *d, v);
                                }
                                Instruction::LoadInt32 { dst: d, value } => {
                                    let v = type_guards::emit_box_int32_const(builder, *value);
                                    write_reg(builder, &callee_reg_vars, *d, v);
                                }
                                Instruction::LoadConst { dst: d, idx } => {
                                    if let Some(bits) = resolve_const_bits(constants, *idx) {
                                        let v = builder.ins().iconst(types::I64, bits);
                                        write_reg(builder, &callee_reg_vars, *d, v);
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
                                    let v = read_local(builder, &callee_local_vars, *idx);
                                    write_reg(builder, &callee_reg_vars, *d, v);
                                }
                                Instruction::GetUpvalue { dst: d, idx } => {
                                    let Some(capture) = callee.upvalues.get(idx.index() as usize)
                                    else {
                                        let undef_ret = builder
                                            .ins()
                                            .iconst(types::I64, type_guards::TAG_UNDEFINED);
                                        builder
                                            .ins()
                                            .jump(continuation, &[BlockArg::Value(undef_ret)]);
                                        continue;
                                    };
                                    match capture {
                                        UpvalueCapture::Local(local_idx) => {
                                            let v = read_local(builder, &local_vars, *local_idx);
                                            if let Some(&func_idx) = candidate
                                                .local_func_snapshot
                                                .get(&local_idx.index())
                                            {
                                                callee_known_funcs.insert(d.0, func_idx);
                                            }
                                            write_reg(builder, &callee_reg_vars, *d, v);
                                        }
                                        UpvalueCapture::Upvalue(_) => {
                                            let undef_ret = builder
                                                .ins()
                                                .iconst(types::I64, type_guards::TAG_UNDEFINED);
                                            builder
                                                .ins()
                                                .jump(continuation, &[BlockArg::Value(undef_ret)]);
                                            continue;
                                        }
                                    }
                                }
                                Instruction::SetLocal { idx, src } => {
                                    let v = read_reg(builder, &callee_reg_vars, *src);
                                    write_local(builder, &callee_local_vars, *idx, v);
                                }
                                Instruction::Move { dst: d, src } => {
                                    let v = read_reg(builder, &callee_reg_vars, *src);
                                    if let Some(&func_idx) = callee_known_funcs.get(&src.0) {
                                        callee_known_funcs.insert(d.0, func_idx);
                                    }
                                    write_reg(builder, &callee_reg_vars, *d, v);
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
                                    let left = read_reg(builder, &callee_reg_vars, *lhs);
                                    let right = read_reg(builder, &callee_reg_vars, *rhs);
                                    let callee_feedback = callee.feedback_vector.read();
                                    let hint = SpecializationHint::from_type_flags(
                                        callee_feedback
                                            .get(*feedback_index as usize)
                                            .map(|m| &m.type_observations),
                                    );
                                    let likely_string = add_likely_string_concat(
                                        callee_instrs,
                                        constants,
                                        ci,
                                        *lhs,
                                        *rhs,
                                    );
                                    let effective_hint = if likely_string {
                                        SpecializationHint::Generic
                                    } else {
                                        hint
                                    };
                                    let arith_hint =
                                        if matches!(effective_hint, SpecializationHint::Int32) {
                                            SpecializationHint::Numeric
                                        } else {
                                            effective_hint
                                        };
                                    let guarded = type_guards::emit_specialized_arith(
                                        builder,
                                        ArithOp::Add,
                                        left,
                                        right,
                                        arith_hint,
                                    );
                                    let out =
                                        if matches!(effective_hint, SpecializationHint::Generic) {
                                            let generic_ref =
                                                helpers.and_then(|h| h.get(HelperKind::GenericAdd));
                                            lower_guarded_with_generic_fallback(
                                                builder,
                                                guarded,
                                                generic_ref,
                                                &[ctx_ptr, left, right],
                                                ctx_ptr,
                                                pc,
                                                BailoutReason::HelperReturnedSentinel,
                                                &local_vars,
                                                &reg_vars,
                                                deopt_site,
                                            )
                                        } else {
                                            lower_guarded_with_bailout(
                                                builder,
                                                guarded,
                                                ctx_ptr,
                                                pc,
                                                BailoutReason::TypeGuardFailure,
                                                &local_vars,
                                                &reg_vars,
                                                deopt_site,
                                            )
                                        };
                                    write_reg(builder, &callee_reg_vars, *d, out);
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
                                    let left = read_reg(builder, &callee_reg_vars, *lhs);
                                    let right = read_reg(builder, &callee_reg_vars, *rhs);
                                    let callee_feedback = callee.feedback_vector.read();
                                    let hint = SpecializationHint::from_type_flags(
                                        callee_feedback
                                            .get(*feedback_index as usize)
                                            .map(|m| &m.type_observations),
                                    );
                                    let arith_hint = if matches!(hint, SpecializationHint::Int32) {
                                        SpecializationHint::Numeric
                                    } else {
                                        hint
                                    };
                                    let guarded = type_guards::emit_specialized_arith(
                                        builder,
                                        ArithOp::Sub,
                                        left,
                                        right,
                                        arith_hint,
                                    );
                                    let out = if matches!(hint, SpecializationHint::Generic) {
                                        let generic_ref =
                                            helpers.and_then(|h| h.get(HelperKind::GenericSub));
                                        lower_guarded_with_generic_fallback(
                                            builder,
                                            guarded,
                                            generic_ref,
                                            &[ctx_ptr, left, right],
                                            ctx_ptr,
                                            pc,
                                            BailoutReason::HelperReturnedSentinel,
                                            &local_vars,
                                            &reg_vars,
                                            deopt_site,
                                        )
                                    } else {
                                        lower_guarded_with_bailout(
                                            builder,
                                            guarded,
                                            ctx_ptr,
                                            pc,
                                            BailoutReason::TypeGuardFailure,
                                            &local_vars,
                                            &reg_vars,
                                            deopt_site,
                                        )
                                    };
                                    write_reg(builder, &callee_reg_vars, *d, out);
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
                                    let left = read_reg(builder, &callee_reg_vars, *lhs);
                                    let right = read_reg(builder, &callee_reg_vars, *rhs);
                                    let callee_feedback = callee.feedback_vector.read();
                                    let hint = SpecializationHint::from_type_flags(
                                        callee_feedback
                                            .get(*feedback_index as usize)
                                            .map(|m| &m.type_observations),
                                    );
                                    let arith_hint = if matches!(hint, SpecializationHint::Int32) {
                                        SpecializationHint::Numeric
                                    } else {
                                        hint
                                    };
                                    let guarded = type_guards::emit_specialized_arith(
                                        builder,
                                        ArithOp::Mul,
                                        left,
                                        right,
                                        arith_hint,
                                    );
                                    let out = if matches!(hint, SpecializationHint::Generic) {
                                        let generic_ref =
                                            helpers.and_then(|h| h.get(HelperKind::GenericMul));
                                        lower_guarded_with_generic_fallback(
                                            builder,
                                            guarded,
                                            generic_ref,
                                            &[ctx_ptr, left, right],
                                            ctx_ptr,
                                            pc,
                                            BailoutReason::HelperReturnedSentinel,
                                            &local_vars,
                                            &reg_vars,
                                            deopt_site,
                                        )
                                    } else {
                                        lower_guarded_with_bailout(
                                            builder,
                                            guarded,
                                            ctx_ptr,
                                            pc,
                                            BailoutReason::TypeGuardFailure,
                                            &local_vars,
                                            &reg_vars,
                                            deopt_site,
                                        )
                                    };
                                    write_reg(builder, &callee_reg_vars, *d, out);
                                }
                                // Inc/Dec
                                Instruction::Inc { dst: d, src } => {
                                    let val = read_reg(builder, &callee_reg_vars, *src);
                                    let guarded = type_guards::emit_guarded_i32_inc(builder, val);
                                    let out = lower_guarded_with_bailout(
                                        builder,
                                        guarded,
                                        ctx_ptr,
                                        pc,
                                        BailoutReason::TypeGuardFailure,
                                        &local_vars,
                                        &reg_vars,
                                        deopt_site,
                                    );
                                    write_reg(builder, &callee_reg_vars, *d, out);
                                }
                                Instruction::Dec { dst: d, src } => {
                                    let val = read_reg(builder, &callee_reg_vars, *src);
                                    let guarded = type_guards::emit_guarded_i32_dec(builder, val);
                                    let out = lower_guarded_with_bailout(
                                        builder,
                                        guarded,
                                        ctx_ptr,
                                        pc,
                                        BailoutReason::TypeGuardFailure,
                                        &local_vars,
                                        &reg_vars,
                                        deopt_site,
                                    );
                                    write_reg(builder, &callee_reg_vars, *d, out);
                                }
                                // Comparisons
                                Instruction::Lt { dst: d, lhs, rhs } => {
                                    let left = read_reg(builder, &callee_reg_vars, *lhs);
                                    let right = read_reg(builder, &callee_reg_vars, *rhs);
                                    let guarded = type_guards::emit_guarded_numeric_cmp(
                                        builder,
                                        IntCC::SignedLessThan,
                                        FloatCC::LessThan,
                                        left,
                                        right,
                                    );
                                    let out = lower_guarded_with_bailout(
                                        builder,
                                        guarded,
                                        ctx_ptr,
                                        pc,
                                        BailoutReason::TypeGuardFailure,
                                        &local_vars,
                                        &reg_vars,
                                        deopt_site,
                                    );
                                    write_reg(builder, &callee_reg_vars, *d, out);
                                }
                                Instruction::Le { dst: d, lhs, rhs } => {
                                    let left = read_reg(builder, &callee_reg_vars, *lhs);
                                    let right = read_reg(builder, &callee_reg_vars, *rhs);
                                    let guarded = type_guards::emit_guarded_numeric_cmp(
                                        builder,
                                        IntCC::SignedLessThanOrEqual,
                                        FloatCC::LessThanOrEqual,
                                        left,
                                        right,
                                    );
                                    let out = lower_guarded_with_bailout(
                                        builder,
                                        guarded,
                                        ctx_ptr,
                                        pc,
                                        BailoutReason::TypeGuardFailure,
                                        &local_vars,
                                        &reg_vars,
                                        deopt_site,
                                    );
                                    write_reg(builder, &callee_reg_vars, *d, out);
                                }
                                Instruction::Gt { dst: d, lhs, rhs } => {
                                    let left = read_reg(builder, &callee_reg_vars, *lhs);
                                    let right = read_reg(builder, &callee_reg_vars, *rhs);
                                    let guarded = type_guards::emit_guarded_numeric_cmp(
                                        builder,
                                        IntCC::SignedGreaterThan,
                                        FloatCC::GreaterThan,
                                        left,
                                        right,
                                    );
                                    let out = lower_guarded_with_bailout(
                                        builder,
                                        guarded,
                                        ctx_ptr,
                                        pc,
                                        BailoutReason::TypeGuardFailure,
                                        &local_vars,
                                        &reg_vars,
                                        deopt_site,
                                    );
                                    write_reg(builder, &callee_reg_vars, *d, out);
                                }
                                Instruction::Ge { dst: d, lhs, rhs } => {
                                    let left = read_reg(builder, &callee_reg_vars, *lhs);
                                    let right = read_reg(builder, &callee_reg_vars, *rhs);
                                    let guarded = type_guards::emit_guarded_numeric_cmp(
                                        builder,
                                        IntCC::SignedGreaterThanOrEqual,
                                        FloatCC::GreaterThanOrEqual,
                                        left,
                                        right,
                                    );
                                    let out = lower_guarded_with_bailout(
                                        builder,
                                        guarded,
                                        ctx_ptr,
                                        pc,
                                        BailoutReason::TypeGuardFailure,
                                        &local_vars,
                                        &reg_vars,
                                        deopt_site,
                                    );
                                    write_reg(builder, &callee_reg_vars, *d, out);
                                }
                                Instruction::StrictEq { dst: d, lhs, rhs } => {
                                    let left = read_reg(builder, &callee_reg_vars, *lhs);
                                    let right = read_reg(builder, &callee_reg_vars, *rhs);
                                    let out =
                                        type_guards::emit_strict_eq(builder, left, right, false);
                                    write_reg(builder, &callee_reg_vars, *d, out);
                                }
                                Instruction::StrictNe { dst: d, lhs, rhs } => {
                                    let left = read_reg(builder, &callee_reg_vars, *lhs);
                                    let right = read_reg(builder, &callee_reg_vars, *rhs);
                                    let out =
                                        type_guards::emit_strict_eq(builder, left, right, true);
                                    write_reg(builder, &callee_reg_vars, *d, out);
                                }
                                Instruction::Not { dst: d, src } => {
                                    let val = read_reg(builder, &callee_reg_vars, *src);
                                    let truthy = type_guards::emit_is_truthy(builder, val);
                                    let is_falsy = builder.ins().icmp_imm(IntCC::Equal, truthy, 0);
                                    let out = type_guards::emit_bool_to_nanbox(builder, is_falsy);
                                    write_reg(builder, &callee_reg_vars, *d, out);
                                }
                                Instruction::Call {
                                    dst: d,
                                    func: callee_func,
                                    argc: callee_argc,
                                    ..
                                } => {
                                    let known_func_index =
                                        callee_known_funcs.get(&callee_func.0).copied();
                                    if let Some(expected_idx) = known_func_index
                                        && let Some(callee) = expected_idx
                                            .checked_sub(1)
                                            .and_then(|idx| module_func_by_index.get(&idx))
                                            .copied()
                                        && let Some(result) = try_emit_binary_leaf_arith_call(
                                            builder,
                                            callee,
                                            &callee_reg_vars,
                                            *callee_func,
                                            u16::from(*callee_argc),
                                            helpers,
                                            ctx_ptr,
                                            pc,
                                            &local_vars,
                                            &reg_vars,
                                            deopt_site,
                                        )
                                    {
                                        write_reg(builder, &callee_reg_vars, *d, result);
                                    } else {
                                        let callee_val =
                                            read_reg(builder, &callee_reg_vars, *callee_func);
                                        let argc_val =
                                            builder.ins().iconst(types::I64, *callee_argc as i64);
                                        let argv_ptr = if *callee_argc > 0 {
                                            let slot = builder.create_sized_stack_slot(
                                                StackSlotData::new(
                                                    StackSlotKind::ExplicitSlot,
                                                    (*callee_argc as u32) * 8,
                                                    8,
                                                ),
                                            );
                                            for i in 0..(*callee_argc as u16) {
                                                let arg_val = read_reg(
                                                    builder,
                                                    &callee_reg_vars,
                                                    Register(callee_func.0 + 1 + i),
                                                );
                                                builder.ins().stack_store(
                                                    arg_val,
                                                    slot,
                                                    (i as i32) * 8,
                                                );
                                            }
                                            builder.ins().stack_addr(types::I64, slot, 0)
                                        } else {
                                            builder.ins().iconst(types::I64, 0)
                                        };
                                        let call = if let Some(expected_idx) = known_func_index {
                                            let helper_ref = helpers
                                                .and_then(|h| h.get(HelperKind::CallMono))
                                                .or_else(|| {
                                                    helpers.and_then(|h| {
                                                        h.get(HelperKind::CallFunction)
                                                    })
                                                })
                                                .ok_or_else(|| unsupported(pc, instruction))?;
                                            let expected = builder
                                                .ins()
                                                .iconst(types::I64, expected_idx as i64);
                                            builder.ins().call(
                                                helper_ref,
                                                &[
                                                    ctx_ptr, callee_val, argc_val, argv_ptr,
                                                    expected,
                                                ],
                                            )
                                        } else {
                                            let helper_ref = helpers
                                                .and_then(|h| h.get(HelperKind::CallFunction))
                                                .ok_or_else(|| unsupported(pc, instruction))?;
                                            builder.ins().call(
                                                helper_ref,
                                                &[ctx_ptr, callee_val, argc_val, argv_ptr],
                                            )
                                        };
                                        let result = builder.inst_results(call)[0];
                                        let bail_block = builder.create_block();
                                        let continue_block = builder.create_block();
                                        let sentinel =
                                            builder.ins().iconst(types::I64, BAILOUT_SENTINEL);
                                        let is_bailout =
                                            builder.ins().icmp(IntCC::Equal, result, sentinel);
                                        builder.ins().brif(
                                            is_bailout,
                                            bail_block,
                                            &[],
                                            continue_block,
                                            &[],
                                        );

                                        builder.switch_to_block(bail_block);
                                        emit_bailout_return_with_state(
                                            builder,
                                            ctx_ptr,
                                            pc,
                                            BailoutReason::HelperReturnedSentinel,
                                            &local_vars,
                                            &reg_vars,
                                            deopt_site,
                                        );

                                        builder.switch_to_block(continue_block);
                                        write_reg(builder, &callee_reg_vars, *d, result);
                                    }
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
                                        let condition_kind =
                                            classify_jump_condition(&callee_instrs, ci, *cond);
                                        match condition_kind {
                                            JumpConditionKind::Constant(true) => {
                                                builder.ins().jump(callee_blocks[target], &[]);
                                            }
                                            JumpConditionKind::Constant(false) => {
                                                builder.ins().jump(ft_block, &[]);
                                            }
                                            JumpConditionKind::BoxedBoolean
                                            | JumpConditionKind::Generic => {
                                                let cond_val =
                                                    read_reg(builder, &callee_reg_vars, *cond);
                                                let is_truthy = emit_jump_truthy_value(
                                                    builder,
                                                    condition_kind,
                                                    cond_val,
                                                );
                                                builder.ins().brif(
                                                    is_truthy,
                                                    callee_blocks[target],
                                                    &[],
                                                    ft_block,
                                                    &[],
                                                );
                                            }
                                        }
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
                                        let condition_kind =
                                            classify_jump_condition(&callee_instrs, ci, *cond);
                                        match condition_kind {
                                            JumpConditionKind::Constant(true) => {
                                                builder.ins().jump(ft_block, &[]);
                                            }
                                            JumpConditionKind::Constant(false) => {
                                                builder.ins().jump(callee_blocks[target], &[]);
                                            }
                                            JumpConditionKind::BoxedBoolean
                                            | JumpConditionKind::Generic => {
                                                let cond_val =
                                                    read_reg(builder, &callee_reg_vars, *cond);
                                                let is_truthy = emit_jump_truthy_value(
                                                    builder,
                                                    condition_kind,
                                                    cond_val,
                                                );
                                                builder.ins().brif(
                                                    is_truthy,
                                                    ft_block,
                                                    &[],
                                                    callee_blocks[target],
                                                    &[],
                                                );
                                            }
                                        }
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
                        write_reg(builder, &reg_vars, *dst, inline_result);
                    }
                } else {
                    // No inline candidate → use runtime helper.
                    // Check FFI call info first (takes priority over JS monomorphic).
                    let ffi_info_ptr: Option<u64> = if *ic_index > 0 {
                        ffi_call_info_snapshot
                            .get(*ic_index as usize)
                            .copied()
                            .filter(|&p| p != 0)
                    } else {
                        None
                    };

                    // Check call target feedback for monomorphic JS dispatch.
                    let mono_func_index: Option<u32> = if ffi_info_ptr.is_none() && *ic_index > 0 {
                        call_target_snapshot.get(*ic_index as usize).and_then(
                            |(func_idx_plus1, _mod_id)| {
                                if *func_idx_plus1 == 0 || *func_idx_plus1 == u32::MAX {
                                    None // uninit or megamorphic
                                } else {
                                    Some(*func_idx_plus1)
                                }
                            },
                        )
                    } else {
                        None
                    };

                    let callee_val = read_reg(builder, &reg_vars, *func);
                    let argc_val = builder.ins().iconst(types::I64, *argc as i64);

                    // Build argument array on the stack
                    let argv_ptr = if *argc > 0 {
                        let slot = builder.create_sized_stack_slot(StackSlotData::new(
                            StackSlotKind::ExplicitSlot,
                            (*argc as u32) * 8,
                            8,
                        ));
                        for i in 0..(*argc as u16) {
                            let arg_val = read_reg(builder, &reg_vars, Register(func.0 + 1 + i));
                            builder.ins().stack_store(arg_val, slot, (i as i32) * 8);
                        }
                        builder.ins().stack_addr(types::I64, slot, 0)
                    } else {
                        builder.ins().iconst(types::I64, 0) // null pointer
                    };

                    let (_call, result) = if let Some(ffi_ptr) = ffi_info_ptr {
                        // FFI fast path: use CallFfi with cached FfiCallInfo pointer
                        let helper_ref = helpers
                            .and_then(|h| h.get(HelperKind::CallFfi))
                            .or_else(|| helpers.and_then(|h| h.get(HelperKind::CallFunction)));
                        let helper_ref = helper_ref.ok_or_else(|| unsupported(pc, instruction))?;
                        let ffi_info_val = builder.ins().iconst(types::I64, ffi_ptr as i64);
                        let call = builder.ins().call(
                            helper_ref,
                            &[ctx_ptr, callee_val, argc_val, argv_ptr, ffi_info_val],
                        );
                        (call, builder.inst_results(call)[0])
                    } else if let Some(expected_idx) = mono_func_index {
                        // Monomorphic: use CallMono with expected function_index hint
                        let helper_ref = helpers
                            .and_then(|h| h.get(HelperKind::CallMono))
                            .or_else(|| helpers.and_then(|h| h.get(HelperKind::CallFunction)));
                        let helper_ref = helper_ref.ok_or_else(|| unsupported(pc, instruction))?;
                        let expected = builder.ins().iconst(types::I64, expected_idx as i64);
                        let call = builder.ins().call(
                            helper_ref,
                            &[ctx_ptr, callee_val, argc_val, argv_ptr, expected],
                        );
                        (call, builder.inst_results(call)[0])
                    } else {
                        // Polymorphic/unknown: regular CallFunction
                        let helper_ref = helpers
                            .and_then(|h| h.get(HelperKind::CallFunction))
                            .ok_or_else(|| unsupported(pc, instruction))?;
                        let call = builder
                            .ins()
                            .call(helper_ref, &[ctx_ptr, callee_val, argc_val, argv_ptr]);
                        (call, builder.inst_results(call)[0])
                    };

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
                    write_reg(builder, &reg_vars, *dst, result);
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
                write_reg(builder, &reg_vars, *dst, result);
            }
            Instruction::NewArray {
                dst,
                len,
                packed: _,
            } => {
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
                write_reg(builder, &reg_vars, *dst, result);
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
                write_reg(builder, &reg_vars, *dst, result);
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
                let val = read_reg(builder, &reg_vars, *src);
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
                let result = emit_inline_get_upvalue(builder, ctx_ptr, idx.index());
                write_reg(builder, &reg_vars, *dst, result);
            }
            Instruction::SetUpvalue { idx, src } => {
                let helper_ref = helpers
                    .and_then(|h| h.get(HelperKind::SetUpvalue))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let idx_val = builder.ins().iconst(types::I64, idx.index() as i64);
                let val = read_reg(builder, &reg_vars, *src);
                emit_helper_call_with_bailout(builder, helper_ref, &[ctx_ptr, idx_val, val]);
            }
            // --- Trivial opcodes (no helper needed) ---
            Instruction::Pop => {
                // No-op in register VM — Pop is a stack concept
            }
            Instruction::Dup { dst, src } => {
                // Same as Move
                let v = read_reg(builder, &reg_vars, *src);
                write_reg(builder, &reg_vars, *dst, v);
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
                ..
            } => {
                let obj_val = read_reg(builder, &reg_vars, *obj);
                let mono_ref = helpers.and_then(|h| h.get(HelperKind::GetPropMono));
                let result = if let Some(mono_helper) = mono_ref {
                    let merge_block = builder.create_block();
                    builder.append_block_param(merge_block, types::I64);

                    let shape_const = builder.ins().iconst(types::I64, *shape_id as i64);
                    let offset_const = builder.ins().iconst(types::I64, *offset as i64);
                    let mono_call = builder
                        .ins()
                        .call(mono_helper, &[obj_val, shape_const, offset_const]);
                    let mono_result = builder.inst_results(mono_call)[0];

                    let sentinel = builder.ins().iconst(types::I64, BAILOUT_SENTINEL);
                    let is_bailout = builder.ins().icmp(IntCC::Equal, mono_result, sentinel);
                    let bail_block = builder.create_block();
                    let ok_block = builder.create_block();
                    builder
                        .ins()
                        .brif(is_bailout, bail_block, &[], ok_block, &[]);

                    builder.switch_to_block(ok_block);
                    builder
                        .ins()
                        .jump(merge_block, &[BlockArg::Value(mono_result)]);

                    builder.switch_to_block(bail_block);
                    emit_bailout_return(builder);

                    builder.switch_to_block(merge_block);
                    builder.block_params(merge_block)[0]
                } else {
                    // Fallback bailout
                    emit_bailout_return(builder);
                    builder.ins().iconst(types::I64, type_guards::TAG_UNDEFINED)
                };
                write_reg(builder, &reg_vars, *dst, result);
            }
            Instruction::SetPropQuickened {
                obj,
                val,
                shape_id,
                offset,
                name,
                ic_index,
            } => {
                let obj_val = read_reg(builder, &reg_vars, *obj);
                let value = read_reg(builder, &reg_vars, *val);
                let layout = crate::runtime_helpers::jsobject_layout();
                let barrier_ref = helpers.and_then(|h| h.get(HelperKind::GcWriteBarrier));
                let full_ref = helpers.and_then(|h| h.get(HelperKind::SetPropConst));

                if let (Some(lo), Some(fr)) = (layout, full_ref) {
                    if (*offset as usize) < 8 {
                        // Inline write: direct store + barrier
                        emit_inline_prop_write(
                            builder,
                            fr,
                            barrier_ref,
                            obj_val,
                            value,
                            ctx_ptr,
                            *shape_id,
                            *offset,
                            name.index(),
                            *ic_index,
                            &lo,
                            pc,
                            &local_vars,
                            &reg_vars,
                            deopt_site,
                        );
                    } else {
                        // Overflow slot — use SetPropMono helper
                        let mono_ref = helpers.and_then(|h| h.get(HelperKind::SetPropMono));
                        if let Some(mono_helper) = mono_ref {
                            emit_mono_set_with_fallback(
                                builder,
                                mono_helper,
                                fr,
                                obj_val,
                                value,
                                ctx_ptr,
                                *shape_id,
                                *offset,
                                name.index(),
                                *ic_index,
                            );
                        } else {
                            emit_bailout_return(builder);
                        }
                    }
                } else {
                    // Fallback: SetPropMono helper or bail
                    let mono_ref = helpers.and_then(|h| h.get(HelperKind::SetPropMono));
                    if let Some(mono_helper) = mono_ref {
                        let shape_const = builder.ins().iconst(types::I64, *shape_id as i64);
                        let offset_const = builder.ins().iconst(types::I64, *offset as i64);
                        let mono_call = builder
                            .ins()
                            .call(mono_helper, &[obj_val, shape_const, offset_const, value]);
                        let mono_result = builder.inst_results(mono_call)[0];

                        let sentinel = builder.ins().iconst(types::I64, BAILOUT_SENTINEL);
                        let is_bailout = builder.ins().icmp(IntCC::Equal, mono_result, sentinel);
                        let bail_block = builder.create_block();
                        let continue_block = builder.create_block();
                        builder
                            .ins()
                            .brif(is_bailout, bail_block, &[], continue_block, &[]);

                        builder.switch_to_block(bail_block);
                        emit_bailout_return(builder);

                        builder.switch_to_block(continue_block);
                    } else {
                        emit_bailout_return(builder);
                    }
                }
            }
            // --- Quickened string/array.length — bail out to interpreter ---
            Instruction::GetPropString { .. } | Instruction::GetArrayLength { .. } => {
                emit_bailout_return(builder);
            }
            // --- LoadThis ---
            Instruction::LoadThis { dst } => {
                let helper_ref = helpers
                    .and_then(|h| h.get(HelperKind::LoadThis))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let result = emit_helper_call_with_bailout(builder, helper_ref, &[ctx_ptr]);
                write_reg(builder, &reg_vars, *dst, result);
            }
            // --- CloseUpvalue ---
            Instruction::CloseUpvalue { local_idx } => {
                if !captured_locals.contains(&local_idx.index()) {
                    continue;
                }
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
                let val = read_reg(builder, &reg_vars, *src);
                let result = emit_helper_call_with_bailout(builder, helper_ref, &[ctx_ptr, val]);
                write_reg(builder, &reg_vars, *dst, result);
            }
            Instruction::TypeOfName { dst, name } => {
                let helper_ref = helpers
                    .and_then(|h| h.get(HelperKind::TypeOfName))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let name_idx = builder.ins().iconst(types::I64, name.index() as i64);
                let result =
                    emit_helper_call_with_bailout(builder, helper_ref, &[ctx_ptr, name_idx]);
                write_reg(builder, &reg_vars, *dst, result);
            }
            // --- Pow ---
            Instruction::Pow { dst, lhs, rhs } => {
                let helper_ref = helpers
                    .and_then(|h| h.get(HelperKind::Pow))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let left = read_reg(builder, &reg_vars, *lhs);
                let right = read_reg(builder, &reg_vars, *rhs);
                let result =
                    emit_helper_call_with_bailout(builder, helper_ref, &[ctx_ptr, left, right]);
                write_reg(builder, &reg_vars, *dst, result);
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
                let obj_val = read_reg(builder, &reg_vars, *obj);
                let key_val = read_reg(builder, &reg_vars, *key);
                let ic_idx = builder.ins().iconst(types::I64, *ic_index as i64);
                let result = emit_helper_call_with_bailout(
                    builder,
                    helper_ref,
                    &[ctx_ptr, obj_val, key_val, ic_idx],
                );
                write_reg(builder, &reg_vars, *dst, result);
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
                let obj_val = read_reg(builder, &reg_vars, *obj);
                let key_val = read_reg(builder, &reg_vars, *key);
                let value = read_reg(builder, &reg_vars, *val);
                let ic_idx = builder.ins().iconst(types::I64, *ic_index as i64);
                emit_helper_call_with_bailout(
                    builder,
                    helper_ref,
                    &[ctx_ptr, obj_val, key_val, value, ic_idx],
                );
            }
            Instruction::GetElemInt { dst, obj, index } => {
                let full_ref = helpers
                    .and_then(|h| h.get(HelperKind::GetElem))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let obj_val = read_reg(builder, &reg_vars, *obj);
                let idx_val = read_reg(builder, &reg_vars, *index);
                let dense_ref = helpers.and_then(|h| h.get(HelperKind::GetElemDense));
                let result = if let Some(dense_helper) = dense_ref {
                    emit_inline_dense_elem_with_fallback(
                        builder,
                        dense_helper,
                        full_ref,
                        obj_val,
                        idx_val,
                        ctx_ptr,
                        0,
                        pc,
                        &local_vars,
                        &reg_vars,
                        deopt_site,
                    )
                } else {
                    let ic_idx = builder.ins().iconst(types::I64, 0);
                    emit_helper_call_with_bailout(
                        builder,
                        full_ref,
                        &[ctx_ptr, obj_val, idx_val, ic_idx],
                    )
                };
                write_reg(builder, &reg_vars, *dst, result);
            }
            // --- GetElem / SetElem ---
            Instruction::GetElem {
                dst,
                arr,
                idx,
                ic_index,
            } => {
                let full_ref = helpers
                    .and_then(|h| h.get(HelperKind::GetElem))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let obj_val = read_reg(builder, &reg_vars, *arr);
                let idx_val = read_reg(builder, &reg_vars, *idx);
                let dense_ref = helpers.and_then(|h| h.get(HelperKind::GetElemDense));
                let result = if let Some(dense_helper) = dense_ref {
                    emit_inline_dense_elem_with_fallback(
                        builder,
                        dense_helper,
                        full_ref,
                        obj_val,
                        idx_val,
                        ctx_ptr,
                        *ic_index,
                        pc,
                        &local_vars,
                        &reg_vars,
                        deopt_site,
                    )
                } else {
                    let ic_idx = builder.ins().iconst(types::I64, *ic_index as i64);
                    emit_helper_call_with_bailout(
                        builder,
                        full_ref,
                        &[ctx_ptr, obj_val, idx_val, ic_idx],
                    )
                };
                write_reg(builder, &reg_vars, *dst, result);
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
                let obj_val = read_reg(builder, &reg_vars, *arr);
                let idx_val = read_reg(builder, &reg_vars, *idx);
                let value = read_reg(builder, &reg_vars, *val);
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
                let obj_val = read_reg(builder, &reg_vars, *obj);
                let key_val = read_reg(builder, &reg_vars, *key);
                let result = emit_helper_call_with_bailout(
                    builder,
                    helper_ref,
                    &[ctx_ptr, obj_val, key_val],
                );
                write_reg(builder, &reg_vars, *dst, result);
            }
            // --- DefineProperty ---
            Instruction::DefineProperty { obj, key, val } => {
                let helper_ref = helpers
                    .and_then(|h| h.get(HelperKind::DefineProperty))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let obj_val = read_reg(builder, &reg_vars, *obj);
                let key_val = read_reg(builder, &reg_vars, *key);
                let value = read_reg(builder, &reg_vars, *val);
                emit_helper_call_with_bailout(
                    builder,
                    helper_ref,
                    &[ctx_ptr, obj_val, key_val, value],
                );
            }
            Instruction::SetPrototype { .. } => {
                return Err(unsupported(pc, instruction));
            }
            // --- Throw ---
            Instruction::Throw { src } => {
                let helper_ref = helpers
                    .and_then(|h| h.get(HelperKind::ThrowValue))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let val = read_reg(builder, &reg_vars, *src);
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
                let callee_val = read_reg(builder, &reg_vars, *func);
                let argc_val = builder.ins().iconst(types::I64, *argc as i64);
                let argv_ptr = if *argc > 0 {
                    let slot = builder.create_sized_stack_slot(StackSlotData::new(
                        StackSlotKind::ExplicitSlot,
                        (*argc as u32) * 8,
                        8,
                    ));
                    for i in 0..(*argc as u16) {
                        let arg_val = read_reg(builder, &reg_vars, Register(func.0 + 1 + i));
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
                write_reg(builder, &reg_vars, *dst, result);
            }
            // --- CallMethod ---
            Instruction::CallMethod {
                dst,
                obj,
                method,
                argc,
                ic_index,
            } => {
                // Fast path: recognize common built-in methods at compile time
                const PUSH_UTF16: [u16; 4] = [112, 117, 115, 104]; // "push"
                const POP_UTF16: [u16; 3] = [112, 111, 112]; // "pop"

                let push_ref = helpers.and_then(|h| h.get(HelperKind::ArrayPush));
                let pop_ref = helpers.and_then(|h| h.get(HelperKind::ArrayPop));

                if *argc == 1
                    && is_const_utf16(constants, *method, &PUSH_UTF16)
                    && let Some(push_helper) = push_ref
                {
                    // arr.push(val) → ArrayPush(arr, val) → new length
                    let obj_val = read_reg(builder, &reg_vars, *obj);
                    let arg_val = read_reg(builder, &reg_vars, Register(obj.0 + 1));
                    let push_call = builder.ins().call(push_helper, &[obj_val, arg_val]);
                    let push_result = builder.inst_results(push_call)[0];

                    // If ArrayPush returns BAILOUT_SENTINEL, fall to generic CallMethod
                    let sentinel = builder.ins().iconst(types::I64, BAILOUT_SENTINEL);
                    let is_bail = builder.ins().icmp(IntCC::Equal, push_result, sentinel);
                    let fast_ok = builder.create_block();
                    let slow_block = builder.create_block();
                    let merge_block = builder.create_block();
                    builder.append_block_param(merge_block, types::I64);
                    builder.ins().brif(is_bail, slow_block, &[], fast_ok, &[]);

                    builder.switch_to_block(fast_ok);
                    builder
                        .ins()
                        .jump(merge_block, &[BlockArg::Value(push_result)]);

                    builder.switch_to_block(slow_block);
                    let helper_ref = helpers
                        .and_then(|h| h.get(HelperKind::CallMethod))
                        .ok_or_else(|| unsupported(pc, instruction))?;
                    let method_name_idx = builder.ins().iconst(types::I64, method.index() as i64);
                    let argc_val = builder.ins().iconst(types::I64, 1_i64);
                    let slot = builder.create_sized_stack_slot(StackSlotData::new(
                        StackSlotKind::ExplicitSlot,
                        8,
                        8,
                    ));
                    builder.ins().stack_store(arg_val, slot, 0);
                    let argv_ptr = builder.ins().stack_addr(types::I64, slot, 0);
                    let ic_idx = builder.ins().iconst(types::I64, *ic_index as i64);
                    let slow_result = emit_helper_call_with_bailout(
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
                    builder
                        .ins()
                        .jump(merge_block, &[BlockArg::Value(slow_result)]);

                    builder.switch_to_block(merge_block);
                    let result = builder.block_params(merge_block)[0];
                    write_reg(builder, &reg_vars, *dst, result);
                } else if *argc == 0
                    && is_const_utf16(constants, *method, &POP_UTF16)
                    && let Some(pop_helper) = pop_ref
                {
                    // arr.pop() → ArrayPop(arr) → popped value
                    let obj_val = read_reg(builder, &reg_vars, *obj);
                    let pop_call = builder.ins().call(pop_helper, &[obj_val]);
                    let pop_result = builder.inst_results(pop_call)[0];

                    let sentinel = builder.ins().iconst(types::I64, BAILOUT_SENTINEL);
                    let is_bail = builder.ins().icmp(IntCC::Equal, pop_result, sentinel);
                    let fast_ok = builder.create_block();
                    let slow_block = builder.create_block();
                    let merge_block = builder.create_block();
                    builder.append_block_param(merge_block, types::I64);
                    builder.ins().brif(is_bail, slow_block, &[], fast_ok, &[]);

                    builder.switch_to_block(fast_ok);
                    builder
                        .ins()
                        .jump(merge_block, &[BlockArg::Value(pop_result)]);

                    builder.switch_to_block(slow_block);
                    let helper_ref = helpers
                        .and_then(|h| h.get(HelperKind::CallMethod))
                        .ok_or_else(|| unsupported(pc, instruction))?;
                    let method_name_idx = builder.ins().iconst(types::I64, method.index() as i64);
                    let argc_val = builder.ins().iconst(types::I64, 0_i64);
                    let argv_ptr = builder.ins().iconst(types::I64, 0);
                    let ic_idx = builder.ins().iconst(types::I64, *ic_index as i64);
                    let slow_result = emit_helper_call_with_bailout(
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
                    builder
                        .ins()
                        .jump(merge_block, &[BlockArg::Value(slow_result)]);

                    builder.switch_to_block(merge_block);
                    let result = builder.block_params(merge_block)[0];
                    write_reg(builder, &reg_vars, *dst, result);
                } else {
                    // toString() fast path: skip method resolution for primitives
                    const TO_STRING_UTF16: [u16; 8] = [116, 111, 83, 116, 114, 105, 110, 103];
                    let tostring_helper =
                        helpers.and_then(|h| h.get(HelperKind::PrimitiveToString));

                    if *argc == 0
                        && is_const_utf16(constants, *method, &TO_STRING_UTF16)
                        && let Some(ts_helper) = tostring_helper
                    {
                        let obj_val = read_reg(builder, &reg_vars, *obj);

                        // Fast: try PrimitiveToString (1-arg, no ctx)
                        let ts_call = builder.ins().call(ts_helper, &[obj_val]);
                        let ts_result = builder.inst_results(ts_call)[0];

                        let sentinel = builder.ins().iconst(types::I64, BAILOUT_SENTINEL);
                        let is_bail = builder.ins().icmp(IntCC::Equal, ts_result, sentinel);
                        let fast_ok = builder.create_block();
                        let slow_block = builder.create_block();
                        let merge_block = builder.create_block();
                        builder.append_block_param(merge_block, types::I64);
                        builder.ins().brif(is_bail, slow_block, &[], fast_ok, &[]);

                        builder.switch_to_block(fast_ok);
                        builder
                            .ins()
                            .jump(merge_block, &[BlockArg::Value(ts_result)]);

                        // Slow: full CallMethod for objects
                        builder.switch_to_block(slow_block);
                        let helper_ref = helpers
                            .and_then(|h| h.get(HelperKind::CallMethod))
                            .ok_or_else(|| unsupported(pc, instruction))?;
                        let method_name_idx =
                            builder.ins().iconst(types::I64, method.index() as i64);
                        let argc_val = builder.ins().iconst(types::I64, 0_i64);
                        let argv_ptr = builder.ins().iconst(types::I64, 0);
                        let ic_idx = builder.ins().iconst(types::I64, *ic_index as i64);
                        let slow_result = emit_helper_call_with_bailout(
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
                        builder
                            .ins()
                            .jump(merge_block, &[BlockArg::Value(slow_result)]);

                        builder.switch_to_block(merge_block);
                        let result = builder.block_params(merge_block)[0];
                        write_reg(builder, &reg_vars, *dst, result);
                    } else {
                        // Try IC-accelerated method call: inline resolve + CallWithReceiver
                        let mono_ic = ic_snapshot.get(*ic_index as usize).and_then(|ic| {
                            if let InlineCacheState::Monomorphic {
                                shape_id,
                                offset,
                                depth: 0,
                                ..
                            } = ic
                            {
                                Some((*shape_id, *offset))
                            } else {
                                None
                            }
                        });
                        let layout = crate::runtime_helpers::jsobject_layout();
                        let get_prop_ref = helpers.and_then(|h| h.get(HelperKind::GetPropConst));
                        let call_recv_ref =
                            helpers.and_then(|h| h.get(HelperKind::CallWithReceiver));

                        if let (Some((shape_id, offset)), Some(lo), Some(gp_ref), Some(cr_ref)) =
                            (mono_ic, layout, get_prop_ref, call_recv_ref)
                        {
                            if (offset as usize) < 8 {
                                // Inline method resolution + CallWithReceiver
                                let obj_val = read_reg(builder, &reg_vars, *obj);

                                // Build argv before branching (stack slots are function-scoped)
                                let argc_val = builder.ins().iconst(types::I64, *argc as i64);
                                let argv_ptr = if *argc > 0 {
                                    let slot = builder.create_sized_stack_slot(StackSlotData::new(
                                        StackSlotKind::ExplicitSlot,
                                        (*argc as u32) * 8,
                                        8,
                                    ));
                                    for i in 0..(*argc as u16) {
                                        let arg_val =
                                            read_reg(builder, &reg_vars, Register(obj.0 + 1 + i));
                                        builder.ins().stack_store(arg_val, slot, (i as i32) * 8);
                                    }
                                    builder.ins().stack_addr(types::I64, slot, 0)
                                } else {
                                    builder.ins().iconst(types::I64, 0)
                                };

                                // Inline property read to resolve method
                                let method_val = emit_inline_prop_read(
                                    builder,
                                    gp_ref,
                                    obj_val,
                                    ctx_ptr,
                                    shape_id,
                                    offset,
                                    method.index(),
                                    *ic_index,
                                    &lo,
                                );

                                // Call resolved method with receiver
                                let result = emit_helper_call_with_bailout(
                                    builder,
                                    cr_ref,
                                    &[ctx_ptr, method_val, obj_val, argc_val, argv_ptr],
                                );
                                write_reg(builder, &reg_vars, *dst, result);
                            } else {
                                // Overflow slot — fall to full CallMethod
                                let helper_ref = helpers
                                    .and_then(|h| h.get(HelperKind::CallMethod))
                                    .ok_or_else(|| unsupported(pc, instruction))?;
                                let obj_val = read_reg(builder, &reg_vars, *obj);
                                let method_name_idx =
                                    builder.ins().iconst(types::I64, method.index() as i64);
                                let argc_val = builder.ins().iconst(types::I64, *argc as i64);
                                let argv_ptr = if *argc > 0 {
                                    let slot = builder.create_sized_stack_slot(StackSlotData::new(
                                        StackSlotKind::ExplicitSlot,
                                        (*argc as u32) * 8,
                                        8,
                                    ));
                                    for i in 0..(*argc as u16) {
                                        let arg_val =
                                            read_reg(builder, &reg_vars, Register(obj.0 + 1 + i));
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
                                write_reg(builder, &reg_vars, *dst, result);
                            }
                        } else if let (Some(lo), Some(gp_ref2), Some(cr_ref2)) = (
                            crate::runtime_helpers::jsobject_layout(),
                            helpers.and_then(|h| h.get(HelperKind::GetPropConst)),
                            helpers.and_then(|h| h.get(HelperKind::CallWithReceiver)),
                        ) {
                            // Cold IC: runtime IC probe → inline resolve → CallWithReceiver
                            let obj_val = read_reg(builder, &reg_vars, *obj);

                            // Build argv before branching
                            let argc_val = builder.ins().iconst(types::I64, *argc as i64);
                            let argv_ptr = if *argc > 0 {
                                let slot = builder.create_sized_stack_slot(StackSlotData::new(
                                    StackSlotKind::ExplicitSlot,
                                    (*argc as u32) * 8,
                                    8,
                                ));
                                for i in 0..(*argc as u16) {
                                    let arg_val =
                                        read_reg(builder, &reg_vars, Register(obj.0 + 1 + i));
                                    builder.ins().stack_store(arg_val, slot, (i as i32) * 8);
                                }
                                builder.ins().stack_addr(types::I64, slot, 0)
                            } else {
                                builder.ins().iconst(types::I64, 0)
                            };

                            // Runtime IC probe read resolves method
                            // (inline on probe hit, GetPropConst on miss)
                            let method_val = emit_runtime_ic_probe_read(
                                builder,
                                gp_ref2,
                                obj_val,
                                ctx_ptr,
                                method.index(),
                                *ic_index,
                                &lo,
                                pc,
                                &local_vars,
                                &reg_vars,
                                deopt_site,
                            );

                            // Call resolved method with receiver
                            let result = emit_helper_call_with_bailout(
                                builder,
                                cr_ref2,
                                &[ctx_ptr, method_val, obj_val, argc_val, argv_ptr],
                            );
                            write_reg(builder, &reg_vars, *dst, result);
                        } else {
                            // No layout available — full CallMethod
                            let helper_ref = helpers
                                .and_then(|h| h.get(HelperKind::CallMethod))
                                .ok_or_else(|| unsupported(pc, instruction))?;
                            let obj_val = read_reg(builder, &reg_vars, *obj);
                            let method_name_idx =
                                builder.ins().iconst(types::I64, method.index() as i64);
                            let argc_val = builder.ins().iconst(types::I64, *argc as i64);
                            let argv_ptr = if *argc > 0 {
                                let slot = builder.create_sized_stack_slot(StackSlotData::new(
                                    StackSlotKind::ExplicitSlot,
                                    (*argc as u32) * 8,
                                    8,
                                ));
                                for i in 0..(*argc as u16) {
                                    let arg_val =
                                        read_reg(builder, &reg_vars, Register(obj.0 + 1 + i));
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
                            write_reg(builder, &reg_vars, *dst, result);
                        }
                    }
                }
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
                let callee_val = read_reg(builder, &reg_vars, *func);
                let this_val = read_reg(builder, &reg_vars, *this);
                let argc_val = builder.ins().iconst(types::I64, *argc as i64);
                let argv_ptr = if *argc > 0 {
                    let slot = builder.create_sized_stack_slot(StackSlotData::new(
                        StackSlotKind::ExplicitSlot,
                        (*argc as u32) * 8,
                        8,
                    ));
                    for i in 0..(*argc as u16) {
                        let arg_val = read_reg(builder, &reg_vars, Register(func.0 + 1 + i));
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
                write_reg(builder, &reg_vars, *dst, result);
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
                let obj_val = read_reg(builder, &reg_vars, *obj);
                let key_val = read_reg(builder, &reg_vars, *key);
                let argc_val = builder.ins().iconst(types::I64, *argc as i64);
                let argv_ptr = if *argc > 0 {
                    let slot = builder.create_sized_stack_slot(StackSlotData::new(
                        StackSlotKind::ExplicitSlot,
                        (*argc as u32) * 8,
                        8,
                    ));
                    // args start after key register
                    for i in 0..(*argc as u16) {
                        let arg_val = read_reg(builder, &reg_vars, Register(key.0 + 1 + i));
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
                write_reg(builder, &reg_vars, *dst, result);
            }
            // --- ToNumber / ToString / RequireCoercible ---
            Instruction::ToNumber { dst, src } => {
                let helper_ref = helpers
                    .and_then(|h| h.get(HelperKind::ToNumber))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let val = read_reg(builder, &reg_vars, *src);
                let result = emit_helper_call_with_bailout(builder, helper_ref, &[ctx_ptr, val]);
                write_reg(builder, &reg_vars, *dst, result);
            }
            Instruction::ToString { dst, src } => {
                let helper_ref = helpers
                    .and_then(|h| h.get(HelperKind::JsToString))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let val = read_reg(builder, &reg_vars, *src);
                let result = emit_helper_call_with_bailout(builder, helper_ref, &[ctx_ptr, val]);
                write_reg(builder, &reg_vars, *dst, result);
            }
            Instruction::RequireCoercible { src } => {
                // Inline: bail if value is null or undefined (pure IR, no helper call)
                let val = read_reg(builder, &reg_vars, *src);
                let is_nullish = type_guards::emit_is_nullish(builder, val);
                let ok_block = builder.create_block();
                let bail_block = builder.create_block();
                builder
                    .ins()
                    .brif(is_nullish, bail_block, &[], ok_block, &[]);
                builder.switch_to_block(bail_block);
                emit_bailout_return_with_state(
                    builder,
                    ctx_ptr,
                    pc,
                    BailoutReason::TypeGuardFailure,
                    &local_vars,
                    &reg_vars,
                    deopt_site,
                );
                builder.switch_to_block(ok_block);
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
                let left = read_reg(builder, &reg_vars, *lhs);
                let right = read_reg(builder, &reg_vars, *rhs);
                let ic_idx = builder.ins().iconst(types::I64, *ic_index as i64);
                let result = emit_helper_call_with_bailout(
                    builder,
                    helper_ref,
                    &[ctx_ptr, left, right, ic_idx],
                );
                write_reg(builder, &reg_vars, *dst, result);
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
                let left = read_reg(builder, &reg_vars, *lhs);
                let right = read_reg(builder, &reg_vars, *rhs);
                let ic_idx = builder.ins().iconst(types::I64, *ic_index as i64);
                let result = emit_helper_call_with_bailout(
                    builder,
                    helper_ref,
                    &[ctx_ptr, left, right, ic_idx],
                );
                write_reg(builder, &reg_vars, *dst, result);
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
                let obj_val = read_reg(builder, &reg_vars, *obj);
                let key_val = read_reg(builder, &reg_vars, *key);
                let func_val = read_reg(builder, &reg_vars, *func);
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
                let obj_val = read_reg(builder, &reg_vars, *obj);
                let key_val = read_reg(builder, &reg_vars, *key);
                let func_val = read_reg(builder, &reg_vars, *func);
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
                let obj_val = read_reg(builder, &reg_vars, *obj);
                let key_val = read_reg(builder, &reg_vars, *key);
                let val_val = read_reg(builder, &reg_vars, *val);
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
                let dst_val = read_reg(builder, &reg_vars, *dst);
                let src_val = read_reg(builder, &reg_vars, *src);
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
                write_reg(builder, &reg_vars, *dst, result);
            }
            // --- CreateArguments ---
            Instruction::CreateArguments { dst } => {
                // Arguments object needs frame info. Always bails out.
                let helper_ref = helpers
                    .and_then(|h| h.get(HelperKind::CreateArguments))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let result = emit_helper_call_with_bailout(builder, helper_ref, &[ctx_ptr]);
                write_reg(builder, &reg_vars, *dst, result);
            }
            // --- GetIterator ---
            Instruction::GetIterator { dst, src } => {
                let helper_ref = helpers
                    .and_then(|h| h.get(HelperKind::GetIterator))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let src_val = read_reg(builder, &reg_vars, *src);
                let result =
                    emit_helper_call_with_bailout(builder, helper_ref, &[ctx_ptr, src_val]);
                write_reg(builder, &reg_vars, *dst, result);
            }
            // --- IteratorNext ---
            Instruction::IteratorNext { dst, done, iter } => {
                // Call helper: returns value, writes done to ctx.secondary_result
                let helper_ref = helpers
                    .and_then(|h| h.get(HelperKind::IteratorNext))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let iter_val = read_reg(builder, &reg_vars, *iter);
                let result =
                    emit_helper_call_with_bailout(builder, helper_ref, &[ctx_ptr, iter_val]);
                write_reg(builder, &reg_vars, *dst, result);
                // Read done flag from ctx.secondary_result
                let done_val = builder.ins().load(
                    types::I64,
                    MemFlags::trusted(),
                    ctx_ptr,
                    crate::runtime_helpers::JIT_CTX_SECONDARY_RESULT_OFFSET,
                );
                write_reg(builder, &reg_vars, *done, done_val);
            }
            // --- IteratorClose ---
            Instruction::IteratorClose { iter } => {
                let helper_ref = helpers
                    .and_then(|h| h.get(HelperKind::IteratorClose))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let iter_val = read_reg(builder, &reg_vars, *iter);
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
                let callee_val = read_reg(builder, &reg_vars, *func);
                let spread_val = read_reg(builder, &reg_vars, *spread);
                let argc_val = builder.ins().iconst(types::I64, *argc as i64);

                // Build argv on stack for regular args
                if *argc > 0 {
                    let slot = builder.create_sized_stack_slot(StackSlotData::new(
                        StackSlotKind::ExplicitSlot,
                        (*argc as u32) * 8,
                        8,
                    ));
                    for i in 0..(*argc as u16) {
                        let arg = read_reg(builder, &reg_vars, Register(func.0 + 1 + i));
                        builder.ins().stack_store(arg, slot, (i as i32) * 8);
                    }
                    let argv = builder.ins().stack_addr(types::I64, slot, 0);
                    let result = emit_helper_call_with_bailout(
                        builder,
                        helper_ref,
                        &[ctx_ptr, callee_val, argc_val, argv, spread_val],
                    );
                    write_reg(builder, &reg_vars, *dst, result);
                } else {
                    let zero = builder.ins().iconst(types::I64, 0);
                    let result = emit_helper_call_with_bailout(
                        builder,
                        helper_ref,
                        &[ctx_ptr, callee_val, argc_val, zero, spread_val],
                    );
                    write_reg(builder, &reg_vars, *dst, result);
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
                let callee_val = read_reg(builder, &reg_vars, *func);
                let spread_val = read_reg(builder, &reg_vars, *spread);
                let argc_val = builder.ins().iconst(types::I64, *argc as i64);

                if *argc > 0 {
                    let slot = builder.create_sized_stack_slot(StackSlotData::new(
                        StackSlotKind::ExplicitSlot,
                        (*argc as u32) * 8,
                        8,
                    ));
                    for i in 0..(*argc as u16) {
                        let arg = read_reg(builder, &reg_vars, Register(func.0 + 1 + i));
                        builder.ins().stack_store(arg, slot, (i as i32) * 8);
                    }
                    let argv = builder.ins().stack_addr(types::I64, slot, 0);
                    let result = emit_helper_call_with_bailout(
                        builder,
                        helper_ref,
                        &[ctx_ptr, callee_val, argc_val, argv, spread_val],
                    );
                    write_reg(builder, &reg_vars, *dst, result);
                } else {
                    let zero = builder.ins().iconst(types::I64, 0);
                    let result = emit_helper_call_with_bailout(
                        builder,
                        helper_ref,
                        &[ctx_ptr, callee_val, argc_val, zero, spread_val],
                    );
                    write_reg(builder, &reg_vars, *dst, result);
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
                let obj_val = read_reg(builder, &reg_vars, *obj);
                let key_val = read_reg(builder, &reg_vars, *key);
                let spread_val = read_reg(builder, &reg_vars, *spread);
                let ic_idx = builder.ins().iconst(types::I64, *ic_index as i64);
                let result = emit_helper_call_with_bailout(
                    builder,
                    helper_ref,
                    &[ctx_ptr, obj_val, key_val, spread_val, ic_idx],
                );
                write_reg(builder, &reg_vars, *dst, result);
            }
            // --- TailCall ---
            Instruction::TailCall { func, argc } => {
                let helper_ref = helpers
                    .and_then(|h| h.get(HelperKind::TailCallHelper))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let callee_val = read_reg(builder, &reg_vars, *func);
                let argc_val = builder.ins().iconst(types::I64, *argc as i64);

                if *argc > 0 {
                    let slot = builder.create_sized_stack_slot(StackSlotData::new(
                        StackSlotKind::ExplicitSlot,
                        (*argc as u32) * 8,
                        8,
                    ));
                    for i in 0..(*argc as u16) {
                        let arg = read_reg(builder, &reg_vars, Register(func.0 + 1 + i));
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
                write_reg(builder, &reg_vars, *dst, result);
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
                let ctor_val = read_reg(builder, &reg_vars, *ctor);
                let super_val = match super_class {
                    Some(reg) => read_reg(builder, &reg_vars, *reg),
                    None => builder.ins().iconst(types::I64, type_guards::TAG_UNDEFINED),
                };
                let name_idx = builder.ins().iconst(types::I64, name.0 as i64);
                let result = emit_helper_call_with_bailout(
                    builder,
                    helper_ref,
                    &[ctx_ptr, ctor_val, super_val, name_idx],
                );
                write_reg(builder, &reg_vars, *dst, result);
            }
            Instruction::GetSuper { dst } => {
                let helper_ref = helpers
                    .and_then(|h| h.get(HelperKind::GetSuper))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let result = emit_helper_call_with_bailout(builder, helper_ref, &[ctx_ptr]);
                write_reg(builder, &reg_vars, *dst, result);
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
                        let arg = read_reg(builder, &reg_vars, Register(args.0 + i));
                        builder.ins().stack_store(arg, slot, (i as i32) * 8);
                    }
                    let argv = builder.ins().stack_addr(types::I64, slot, 0);
                    let result = emit_helper_call_with_bailout(
                        builder,
                        helper_ref,
                        &[ctx_ptr, argc_val, argv],
                    );
                    write_reg(builder, &reg_vars, *dst, result);
                } else {
                    let zero = builder.ins().iconst(types::I64, 0);
                    let result = emit_helper_call_with_bailout(
                        builder,
                        helper_ref,
                        &[ctx_ptr, argc_val, zero],
                    );
                    write_reg(builder, &reg_vars, *dst, result);
                }
            }
            Instruction::GetSuperProp { dst, name } => {
                let helper_ref = helpers
                    .and_then(|h| h.get(HelperKind::GetSuperProp))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let name_idx = builder.ins().iconst(types::I64, name.0 as i64);
                let result =
                    emit_helper_call_with_bailout(builder, helper_ref, &[ctx_ptr, name_idx]);
                write_reg(builder, &reg_vars, *dst, result);
            }
            Instruction::SetHomeObject { func, obj } => {
                let helper_ref = helpers
                    .and_then(|h| h.get(HelperKind::SetHomeObject))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let func_val = read_reg(builder, &reg_vars, *func);
                let obj_val = read_reg(builder, &reg_vars, *obj);
                let result = emit_helper_call_with_bailout(
                    builder,
                    helper_ref,
                    &[ctx_ptr, func_val, obj_val],
                );
                // SetHomeObject returns the new function value — write back to func register
                write_reg(builder, &reg_vars, *func, result);
            }
            Instruction::CallSuperForward { dst } => {
                let helper_ref = helpers
                    .and_then(|h| h.get(HelperKind::CallSuperForward))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let result = emit_helper_call_with_bailout(builder, helper_ref, &[ctx_ptr]);
                write_reg(builder, &reg_vars, *dst, result);
            }
            Instruction::CallSuperSpread { dst, args } => {
                let helper_ref = helpers
                    .and_then(|h| h.get(HelperKind::CallSuperSpread))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let args_val = read_reg(builder, &reg_vars, *args);
                let result =
                    emit_helper_call_with_bailout(builder, helper_ref, &[ctx_ptr, args_val]);
                write_reg(builder, &reg_vars, *dst, result);
            }
            Instruction::Yield { dst, .. } => {
                let helper_ref = helpers
                    .and_then(|h| h.get(HelperKind::YieldOp))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let result = emit_helper_call_with_bailout(builder, helper_ref, &[ctx_ptr]);
                write_reg(builder, &reg_vars, *dst, result);
            }
            Instruction::Await { dst, .. } => {
                let helper_ref = helpers
                    .and_then(|h| h.get(HelperKind::AwaitOp))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let result = emit_helper_call_with_bailout(builder, helper_ref, &[ctx_ptr]);
                write_reg(builder, &reg_vars, *dst, result);
            }
            Instruction::AsyncClosure { dst, func } => {
                let helper_ref = helpers
                    .and_then(|h| h.get(HelperKind::AsyncClosure))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let func_idx = builder.ins().iconst(types::I64, func.0 as i64);
                let result =
                    emit_helper_call_with_bailout(builder, helper_ref, &[ctx_ptr, func_idx]);
                write_reg(builder, &reg_vars, *dst, result);
            }
            Instruction::GeneratorClosure { dst, func } => {
                let helper_ref = helpers
                    .and_then(|h| h.get(HelperKind::GeneratorClosure))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let func_idx = builder.ins().iconst(types::I64, func.0 as i64);
                let result =
                    emit_helper_call_with_bailout(builder, helper_ref, &[ctx_ptr, func_idx]);
                write_reg(builder, &reg_vars, *dst, result);
            }
            Instruction::AsyncGeneratorClosure { dst, func } => {
                let helper_ref = helpers
                    .and_then(|h| h.get(HelperKind::AsyncGeneratorClosure))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let func_idx = builder.ins().iconst(types::I64, func.0 as i64);
                let result =
                    emit_helper_call_with_bailout(builder, helper_ref, &[ctx_ptr, func_idx]);
                write_reg(builder, &reg_vars, *dst, result);
            }
            Instruction::CallEval { dst, code } => {
                let helper_ref = helpers
                    .and_then(|h| h.get(HelperKind::CallEval))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let code_val = read_reg(builder, &reg_vars, *code);
                let result =
                    emit_helper_call_with_bailout(builder, helper_ref, &[ctx_ptr, code_val]);
                write_reg(builder, &reg_vars, *dst, result);
            }
            Instruction::Import { dst, module } => {
                let helper_ref = helpers
                    .and_then(|h| h.get(HelperKind::ImportOp))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let module_idx = builder.ins().iconst(types::I64, module.index() as i64);
                let result =
                    emit_helper_call_with_bailout(builder, helper_ref, &[ctx_ptr, module_idx]);
                write_reg(builder, &reg_vars, *dst, result);
            }
            Instruction::Export { name, src } => {
                let helper_ref = helpers
                    .and_then(|h| h.get(HelperKind::ExportOp))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let name_idx = builder.ins().iconst(types::I64, name.index() as i64);
                let src_val = read_reg(builder, &reg_vars, *src);
                emit_helper_call_with_bailout(builder, helper_ref, &[ctx_ptr, name_idx, src_val]);
            }
            Instruction::GetAsyncIterator { dst, src } => {
                let helper_ref = helpers
                    .and_then(|h| h.get(HelperKind::GetAsyncIterator))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let src_val = read_reg(builder, &reg_vars, *src);
                let result =
                    emit_helper_call_with_bailout(builder, helper_ref, &[ctx_ptr, src_val]);
                write_reg(builder, &reg_vars, *dst, result);
            }
            // --- Superinstructions: handle natively when possible ---
            Instruction::GetLocal2 {
                dst1,
                idx1,
                dst2,
                idx2,
            } => {
                // Two GetLocals fused into one dispatch
                let val1 = read_local(builder, &local_vars, *idx1);
                write_reg(builder, &reg_vars, *dst1, val1);
                let val2 = read_local(builder, &local_vars, *idx2);
                write_reg(builder, &reg_vars, *dst2, val2);
            }
            Instruction::IncLocal { .. } => {
                // IncLocal involves numeric coercion — bail out to interpreter
                emit_bailout_return(builder);
            }
            Instruction::ForInNext { dst, obj, offset } => {
                let helper_ref = helpers
                    .and_then(|h| h.get(HelperKind::ForInNext))
                    .ok_or_else(|| unsupported(pc, instruction))?;
                let obj_val = read_reg(builder, &reg_vars, *obj);
                let result =
                    emit_helper_call_with_bailout(builder, helper_ref, &[ctx_ptr, obj_val]);
                write_reg(builder, &reg_vars, *dst, result);

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

        // Fallthrough: only emit jump when next PC is in a different block.
        // Terminators (Jump, Return, etc.) already emitted their own control flow
        // via `continue` above, so this code is only reached for non-terminators.
        let next_pc = pc + 1;
        if next_pc >= instruction_count {
            let undef = builder.ins().iconst(types::I64, type_guards::TAG_UNDEFINED);
            builder.ins().return_(&[undef]);
        } else if blocks[next_pc] != blocks[pc] {
            // Next PC is in a different block — emit explicit jump
            let ft_block = resolve_target(pc, next_pc);
            builder.ins().jump(ft_block, &[]);
        }
        // else: next PC is in the same merged block, no jump needed
    }

    // --- Emit loop versioning pre-headers and optimized bodies ---
    for vl in &versioned {
        // Pre-header: check that all relevant registers are int32
        builder.switch_to_block(vl.pre_header);

        // Check each register
        let mut all_int32 = None;
        for &reg_idx in &vl.check_registers {
            if (reg_idx as usize) < reg_count {
                let val = builder.use_var(reg_vars[reg_idx as usize]);
                let is_i32 = type_guards::emit_is_int32(builder, val);
                all_int32 = Some(match all_int32 {
                    None => is_i32,
                    Some(prev) => builder.ins().band(prev, is_i32),
                });
            }
        }
        // Also check tracked locals
        for (&local_idx, _) in &vl.local_to_i32 {
            let val = builder.use_var(local_vars[local_idx as usize]);
            let is_i32 = type_guards::emit_is_int32(builder, val);
            all_int32 = Some(match all_int32 {
                None => is_i32,
                Some(prev) => builder.ins().band(prev, is_i32),
            });
        }

        if let Some(check) = all_int32 {
            // Branch: all int32 → unbox block, otherwise → guarded
            let unbox_block = builder.create_block();
            builder
                .ins()
                .brif(check, unbox_block, &[], blocks[vl.header_pc], &[]);

            // Unbox checked registers into raw i32 Variables
            builder.switch_to_block(unbox_block);
            for (j, &reg_idx) in vl.check_registers.iter().enumerate() {
                if (reg_idx as usize) < reg_count {
                    let boxed = builder.use_var(reg_vars[reg_idx as usize]);
                    let raw = type_guards::emit_unbox_int32(builder, boxed);
                    builder.def_var(vl.i32_vars[j], raw);
                }
            }
            // Unbox tracked locals into raw i32 Variables
            for (&local_idx, &j) in &vl.local_to_i32 {
                let boxed = builder.use_var(local_vars[local_idx as usize]);
                let raw = type_guards::emit_unbox_int32(builder, boxed);
                builder.def_var(vl.i32_local_vars[j], raw);
            }

            // Shape hoisting: verify object shapes and cache raw pointers.
            // After this, loop body can load from inline_slots without any checks.
            if !vl.shape_hoisted_ptrs.is_empty() {
                if let Some(lo) = crate::runtime_helpers::jsobject_layout() {
                    use crate::type_guards::{PAYLOAD_MASK, PTR_MASK};
                    let tag_mask = builder.ins().iconst(types::I64, PTR_MASK);
                    let expected_obj_tag = builder.ins().iconst(types::I64, 0x7FFC_0000_0000_0000_u64 as i64);
                    let payload_mask = builder.ins().iconst(types::I64, PAYLOAD_MASK);

                    // Collect shape_id for each hoisted obj register from IC snapshot
                    for (&obj_reg, &ptr_var) in &vl.shape_hoisted_ptrs {
                        // Find the shape_id from the first GetPropConst on this register
                        let shape_id = instructions_ref[vl.header_pc..=vl.back_edge_pc]
                            .iter()
                            .find_map(|inst| match inst {
                                Instruction::GetPropConst { obj, ic_index, .. } if obj.0 == obj_reg => {
                                    ic_snapshot.get(*ic_index as usize).and_then(|ic| {
                                        if let InlineCacheState::Monomorphic { shape_id, depth: 0, .. } = ic {
                                            Some(*shape_id)
                                        } else {
                                            None
                                        }
                                    })
                                }
                                _ => None,
                            });
                        let Some(sid) = shape_id else { continue };

                        let obj_val = builder.use_var(reg_vars[obj_reg as usize]);
                        // Tag check
                        let tag = builder.ins().band(obj_val, tag_mask);
                        let is_obj = builder.ins().icmp(IntCC::Equal, tag, expected_obj_tag);
                        let tag_ok = builder.create_block();
                        builder.ins().brif(is_obj, tag_ok, &[], blocks[vl.header_pc], &[]);
                        builder.switch_to_block(tag_ok);
                        // Extract pointer
                        let obj_ptr = builder.ins().band(obj_val, payload_mask);
                        // Shape check
                        let shape_tag = builder.ins().load(types::I64, MemFlags::trusted(), obj_ptr, 0);
                        let expected_shape = builder.ins().iconst(types::I64, sid as i64);
                        let shape_ok = builder.ins().icmp(IntCC::Equal, shape_tag, expected_shape);
                        let shape_ok_block = builder.create_block();
                        builder.ins().brif(shape_ok, shape_ok_block, &[], blocks[vl.header_pc], &[]);
                        builder.switch_to_block(shape_ok_block);
                        // Cache the raw pointer for loop body use
                        builder.def_var(ptr_var, obj_ptr);
                    }

                    let _ = lo; // used for inline_slots_data offset in loop body
                }
            }

            builder.ins().jump(vl.opt_blocks[0], &[]);
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
                // --- Constant loads ---
                // Non-int32 constants targeting a tracked register bail to guarded path.
                Instruction::LoadUndefined { dst } => {
                    if vl.reg_to_i32.contains_key(&dst.0) {
                        materialize_all_i32(builder, &reg_vars, &local_vars, vl);
                        builder.ins().jump(blocks[body_pc], &[]);
                        continue;
                    }
                    let v = builder.ins().iconst(types::I64, type_guards::TAG_UNDEFINED);
                    write_reg(builder, &reg_vars, *dst, v);
                }
                Instruction::LoadNull { dst } => {
                    if vl.reg_to_i32.contains_key(&dst.0) {
                        materialize_all_i32(builder, &reg_vars, &local_vars, vl);
                        builder.ins().jump(blocks[body_pc], &[]);
                        continue;
                    }
                    let v = builder.ins().iconst(types::I64, type_guards::TAG_NULL);
                    write_reg(builder, &reg_vars, *dst, v);
                }
                Instruction::LoadTrue { dst } => {
                    if vl.reg_to_i32.contains_key(&dst.0) {
                        materialize_all_i32(builder, &reg_vars, &local_vars, vl);
                        builder.ins().jump(blocks[body_pc], &[]);
                        continue;
                    }
                    let v = builder.ins().iconst(types::I64, type_guards::TAG_TRUE);
                    write_reg(builder, &reg_vars, *dst, v);
                }
                Instruction::LoadFalse { dst } => {
                    if vl.reg_to_i32.contains_key(&dst.0) {
                        materialize_all_i32(builder, &reg_vars, &local_vars, vl);
                        builder.ins().jump(blocks[body_pc], &[]);
                        continue;
                    }
                    let v = builder.ins().iconst(types::I64, type_guards::TAG_FALSE);
                    write_reg(builder, &reg_vars, *dst, v);
                }
                Instruction::LoadInt8 { dst, value } => {
                    if vl.reg_to_i32.contains_key(&dst.0) {
                        let raw = builder.ins().iconst(types::I32, i32::from(*value) as i64);
                        write_reg_i32(builder, &reg_vars, vl, *dst, raw);
                    } else {
                        let v = type_guards::emit_box_int32_const(builder, i32::from(*value));
                        write_reg(builder, &reg_vars, *dst, v);
                    }
                }
                Instruction::LoadInt32 { dst, value } => {
                    if vl.reg_to_i32.contains_key(&dst.0) {
                        let raw = builder.ins().iconst(types::I32, *value as i64);
                        write_reg_i32(builder, &reg_vars, vl, *dst, raw);
                    } else {
                        let v = type_guards::emit_box_int32_const(builder, *value);
                        write_reg(builder, &reg_vars, *dst, v);
                    }
                }
                Instruction::LoadConst { dst, idx } => {
                    if vl.reg_to_i32.contains_key(&dst.0) {
                        // Can't determine const type at compile time; bail
                        materialize_all_i32(builder, &reg_vars, &local_vars, vl);
                        builder.ins().jump(blocks[body_pc], &[]);
                        continue;
                    }
                    if let Some(bits) = resolve_const_bits(constants, *idx) {
                        let v = builder.ins().iconst(types::I64, bits);
                        write_reg(builder, &reg_vars, *dst, v);
                    } else {
                        materialize_all_i32(builder, &reg_vars, &local_vars, vl);
                        builder.ins().jump(blocks[body_pc], &[]);
                        continue;
                    }
                }
                // --- Variable access ---
                Instruction::GetLocal { dst, idx } => {
                    if let Some(&lj) = vl.local_to_i32.get(&idx.index()) {
                        // Local has i32 shadow — read directly as i32
                        let raw = builder.use_var(vl.i32_local_vars[lj]);
                        if vl.reg_to_i32.contains_key(&dst.0) {
                            write_reg_i32(builder, &reg_vars, vl, *dst, raw);
                        } else {
                            let boxed = type_guards::emit_box_int32(builder, raw);
                            write_reg(builder, &reg_vars, *dst, boxed);
                        }
                    } else {
                        let v = read_local(builder, &local_vars, *idx);
                        if vl.reg_to_i32.contains_key(&dst.0) {
                            let raw = type_guards::emit_unbox_int32(builder, v);
                            write_reg_i32(builder, &reg_vars, vl, *dst, raw);
                        } else {
                            write_reg(builder, &reg_vars, *dst, v);
                        }
                    }
                }
                Instruction::SetLocal { idx, src } => {
                    if let Some(&lj) = vl.local_to_i32.get(&idx.index()) {
                        // Local has i32 shadow — write raw i32, skip boxing
                        let raw = read_reg_i32(builder, &reg_vars, vl, *src);
                        builder.def_var(vl.i32_local_vars[lj], raw);
                    } else {
                        let v = read_reg_versioned(builder, &reg_vars, vl, *src);
                        write_local(builder, &local_vars, *idx, v);
                    }
                }
                Instruction::GetLocal2 {
                    dst1,
                    idx1,
                    dst2,
                    idx2,
                } => {
                    // First local
                    if let Some(&lj) = vl.local_to_i32.get(&idx1.index()) {
                        let raw = builder.use_var(vl.i32_local_vars[lj]);
                        if vl.reg_to_i32.contains_key(&dst1.0) {
                            write_reg_i32(builder, &reg_vars, vl, *dst1, raw);
                        } else {
                            let boxed = type_guards::emit_box_int32(builder, raw);
                            write_reg(builder, &reg_vars, *dst1, boxed);
                        }
                    } else {
                        let v = read_local(builder, &local_vars, *idx1);
                        if vl.reg_to_i32.contains_key(&dst1.0) {
                            let raw = type_guards::emit_unbox_int32(builder, v);
                            write_reg_i32(builder, &reg_vars, vl, *dst1, raw);
                        } else {
                            write_reg(builder, &reg_vars, *dst1, v);
                        }
                    }
                    // Second local
                    if let Some(&lj) = vl.local_to_i32.get(&idx2.index()) {
                        let raw = builder.use_var(vl.i32_local_vars[lj]);
                        if vl.reg_to_i32.contains_key(&dst2.0) {
                            write_reg_i32(builder, &reg_vars, vl, *dst2, raw);
                        } else {
                            let boxed = type_guards::emit_box_int32(builder, raw);
                            write_reg(builder, &reg_vars, *dst2, boxed);
                        }
                    } else {
                        let v = read_local(builder, &local_vars, *idx2);
                        if vl.reg_to_i32.contains_key(&dst2.0) {
                            let raw = type_guards::emit_unbox_int32(builder, v);
                            write_reg_i32(builder, &reg_vars, vl, *dst2, raw);
                        } else {
                            write_reg(builder, &reg_vars, *dst2, v);
                        }
                    }
                }
                Instruction::Move { dst, src } => {
                    if vl.reg_to_i32.contains_key(&dst.0) {
                        let raw = read_reg_i32(builder, &reg_vars, vl, *src);
                        write_reg_i32(builder, &reg_vars, vl, *dst, raw);
                    } else {
                        let v = read_reg_versioned(builder, &reg_vars, vl, *src);
                        write_reg(builder, &reg_vars, *dst, v);
                    }
                }
                // --- Raw i32 arithmetic (wrapping if truncated, overflow-checked otherwise) ---
                // Uses backwards truncation analysis: if ALL consumers of the result
                // are bitwise ops (or themselves wrapping), overflow checks are eliminated.
                Instruction::Add { dst, lhs, rhs, .. }
                | Instruction::AddInt32 { dst, lhs, rhs, .. } => {
                    let left = read_reg_i32(builder, &reg_vars, vl, *lhs);
                    let right = read_reg_i32(builder, &reg_vars, vl, *rhs);
                    if vl.wrapping_pcs.contains(&body_pc) {
                        let result = builder.ins().iadd(left, right);
                        write_reg_i32(builder, &reg_vars, vl, *dst, result);
                    } else {
                        let guarded =
                            type_guards::emit_raw_i32_arith(builder, ArithOp::Add, left, right);
                        builder.switch_to_block(guarded.slow_block);
                        materialize_all_i32(builder, &reg_vars, &local_vars, vl);
                        builder.ins().jump(blocks[body_pc], &[]);
                        builder.switch_to_block(guarded.merge_block);
                        write_reg_i32(builder, &reg_vars, vl, *dst, guarded.result);
                    }
                }
                Instruction::Sub { dst, lhs, rhs, .. }
                | Instruction::SubInt32 { dst, lhs, rhs, .. } => {
                    let left = read_reg_i32(builder, &reg_vars, vl, *lhs);
                    let right = read_reg_i32(builder, &reg_vars, vl, *rhs);
                    if vl.wrapping_pcs.contains(&body_pc) {
                        let result = builder.ins().isub(left, right);
                        write_reg_i32(builder, &reg_vars, vl, *dst, result);
                    } else {
                        let guarded =
                            type_guards::emit_raw_i32_arith(builder, ArithOp::Sub, left, right);
                        builder.switch_to_block(guarded.slow_block);
                        materialize_all_i32(builder, &reg_vars, &local_vars, vl);
                        builder.ins().jump(blocks[body_pc], &[]);
                        builder.switch_to_block(guarded.merge_block);
                        write_reg_i32(builder, &reg_vars, vl, *dst, guarded.result);
                    }
                }
                Instruction::Mul { dst, lhs, rhs, .. }
                | Instruction::MulInt32 { dst, lhs, rhs, .. } => {
                    let left = read_reg_i32(builder, &reg_vars, vl, *lhs);
                    let right = read_reg_i32(builder, &reg_vars, vl, *rhs);
                    if vl.wrapping_pcs.contains(&body_pc) {
                        let result = builder.ins().imul(left, right);
                        write_reg_i32(builder, &reg_vars, vl, *dst, result);
                    } else {
                        let guarded =
                            type_guards::emit_raw_i32_arith(builder, ArithOp::Mul, left, right);
                        builder.switch_to_block(guarded.slow_block);
                        materialize_all_i32(builder, &reg_vars, &local_vars, vl);
                        builder.ins().jump(blocks[body_pc], &[]);
                        builder.switch_to_block(guarded.merge_block);
                        write_reg_i32(builder, &reg_vars, vl, *dst, guarded.result);
                    }
                }
                // --- Inc/Dec (raw i32, overflow only) ---
                Instruction::Inc { dst, src } => {
                    let val = read_reg_i32(builder, &reg_vars, vl, *src);
                    let guarded = type_guards::emit_raw_i32_inc(builder, val);
                    builder.switch_to_block(guarded.slow_block);
                    materialize_all_i32(builder, &reg_vars, &local_vars, vl);
                    builder.ins().jump(blocks[body_pc], &[]);
                    builder.switch_to_block(guarded.merge_block);
                    write_reg_i32(builder, &reg_vars, vl, *dst, guarded.result);
                }
                Instruction::Dec { dst, src } => {
                    let val = read_reg_i32(builder, &reg_vars, vl, *src);
                    let guarded = type_guards::emit_raw_i32_dec(builder, val);
                    builder.switch_to_block(guarded.slow_block);
                    materialize_all_i32(builder, &reg_vars, &local_vars, vl);
                    builder.ins().jump(blocks[body_pc], &[]);
                    builder.switch_to_block(guarded.merge_block);
                    write_reg_i32(builder, &reg_vars, vl, *dst, guarded.result);
                }
                // --- Raw i32 comparisons ---
                // Produce NaN-boxed booleans; bail if dst is a tracked i32 register
                // (unless the compare is fused into the subsequent branch).
                Instruction::Lt { dst, lhs, rhs } => {
                    if can_fuse_versioned_compare_branch(instructions_ref, body_pc, *dst) {
                        // Fused into subsequent JumpIfTrue/JumpIfFalse — emit fall-through.
                        if body_idx + 1 < body_len {
                            builder.ins().jump(vl.opt_blocks[body_idx + 1], &[]);
                        }
                        continue;
                    }
                    if vl.reg_to_i32.contains_key(&dst.0) {
                        materialize_all_i32(builder, &reg_vars, &local_vars, vl);
                        builder.ins().jump(blocks[body_pc], &[]);
                        continue;
                    }
                    let left = read_reg_i32(builder, &reg_vars, vl, *lhs);
                    let right = read_reg_i32(builder, &reg_vars, vl, *rhs);
                    let out =
                        type_guards::emit_raw_i32_cmp(builder, IntCC::SignedLessThan, left, right);
                    write_reg(builder, &reg_vars, *dst, out);
                }
                Instruction::Le { dst, lhs, rhs } => {
                    if can_fuse_versioned_compare_branch(instructions_ref, body_pc, *dst) {
                        if body_idx + 1 < body_len {
                            builder.ins().jump(vl.opt_blocks[body_idx + 1], &[]);
                        }
                        continue;
                    }
                    if vl.reg_to_i32.contains_key(&dst.0) {
                        materialize_all_i32(builder, &reg_vars, &local_vars, vl);
                        builder.ins().jump(blocks[body_pc], &[]);
                        continue;
                    }
                    let left = read_reg_i32(builder, &reg_vars, vl, *lhs);
                    let right = read_reg_i32(builder, &reg_vars, vl, *rhs);
                    let out = type_guards::emit_raw_i32_cmp(
                        builder,
                        IntCC::SignedLessThanOrEqual,
                        left,
                        right,
                    );
                    write_reg(builder, &reg_vars, *dst, out);
                }
                Instruction::Gt { dst, lhs, rhs } => {
                    if can_fuse_versioned_compare_branch(instructions_ref, body_pc, *dst) {
                        if body_idx + 1 < body_len {
                            builder.ins().jump(vl.opt_blocks[body_idx + 1], &[]);
                        }
                        continue;
                    }
                    if vl.reg_to_i32.contains_key(&dst.0) {
                        materialize_all_i32(builder, &reg_vars, &local_vars, vl);
                        builder.ins().jump(blocks[body_pc], &[]);
                        continue;
                    }
                    let left = read_reg_i32(builder, &reg_vars, vl, *lhs);
                    let right = read_reg_i32(builder, &reg_vars, vl, *rhs);
                    let out = type_guards::emit_raw_i32_cmp(
                        builder,
                        IntCC::SignedGreaterThan,
                        left,
                        right,
                    );
                    write_reg(builder, &reg_vars, *dst, out);
                }
                Instruction::Ge { dst, lhs, rhs } => {
                    if can_fuse_versioned_compare_branch(instructions_ref, body_pc, *dst) {
                        if body_idx + 1 < body_len {
                            builder.ins().jump(vl.opt_blocks[body_idx + 1], &[]);
                        }
                        continue;
                    }
                    if vl.reg_to_i32.contains_key(&dst.0) {
                        materialize_all_i32(builder, &reg_vars, &local_vars, vl);
                        builder.ins().jump(blocks[body_pc], &[]);
                        continue;
                    }
                    let left = read_reg_i32(builder, &reg_vars, vl, *lhs);
                    let right = read_reg_i32(builder, &reg_vars, vl, *rhs);
                    let out = type_guards::emit_raw_i32_cmp(
                        builder,
                        IntCC::SignedGreaterThanOrEqual,
                        left,
                        right,
                    );
                    write_reg(builder, &reg_vars, *dst, out);
                }
                // --- Raw i32 bitwise ops (never overflow, no slow path) ---
                Instruction::BitOr { dst, lhs, rhs } => {
                    let left = read_reg_i32(builder, &reg_vars, vl, *lhs);
                    let right = read_reg_i32(builder, &reg_vars, vl, *rhs);
                    let result = builder.ins().bor(left, right);
                    write_reg_i32(builder, &reg_vars, vl, *dst, result);
                }
                Instruction::BitXor { dst, lhs, rhs } => {
                    let left = read_reg_i32(builder, &reg_vars, vl, *lhs);
                    let right = read_reg_i32(builder, &reg_vars, vl, *rhs);
                    let result = builder.ins().bxor(left, right);
                    write_reg_i32(builder, &reg_vars, vl, *dst, result);
                }
                Instruction::BitAnd { dst, lhs, rhs } => {
                    let left = read_reg_i32(builder, &reg_vars, vl, *lhs);
                    let right = read_reg_i32(builder, &reg_vars, vl, *rhs);
                    let result = builder.ins().band(left, right);
                    write_reg_i32(builder, &reg_vars, vl, *dst, result);
                }
                Instruction::BitNot { dst, src } => {
                    let val = read_reg_i32(builder, &reg_vars, vl, *src);
                    let result = builder.ins().bnot(val);
                    write_reg_i32(builder, &reg_vars, vl, *dst, result);
                }
                Instruction::Shl { dst, lhs, rhs } => {
                    let left = read_reg_i32(builder, &reg_vars, vl, *lhs);
                    let right = read_reg_i32(builder, &reg_vars, vl, *rhs);
                    // JS spec: shift amount is masked to 5 bits (0..31)
                    let mask = builder.ins().iconst(types::I32, 0x1F);
                    let shift = builder.ins().band(right, mask);
                    let result = builder.ins().ishl(left, shift);
                    write_reg_i32(builder, &reg_vars, vl, *dst, result);
                }
                Instruction::Shr { dst, lhs, rhs } => {
                    let left = read_reg_i32(builder, &reg_vars, vl, *lhs);
                    let right = read_reg_i32(builder, &reg_vars, vl, *rhs);
                    let mask = builder.ins().iconst(types::I32, 0x1F);
                    let shift = builder.ins().band(right, mask);
                    let result = builder.ins().sshr(left, shift);
                    write_reg_i32(builder, &reg_vars, vl, *dst, result);
                }
                Instruction::Ushr { dst, lhs, rhs } => {
                    if vl.reg_to_i32.contains_key(&dst.0) {
                        // Unsigned shift result may exceed signed i32 range
                        materialize_all_i32(builder, &reg_vars, &local_vars, vl);
                        builder.ins().jump(blocks[body_pc], &[]);
                        continue;
                    }
                    let left = read_reg_i32(builder, &reg_vars, vl, *lhs);
                    let right = read_reg_i32(builder, &reg_vars, vl, *rhs);
                    let mask = builder.ins().iconst(types::I32, 0x1F);
                    let shift = builder.ins().band(right, mask);
                    let result = builder.ins().ushr(left, shift);
                    let boxed = type_guards::emit_box_int32(builder, result);
                    write_reg(builder, &reg_vars, *dst, boxed);
                }
                // --- Strict eq/ne ---
                // Produce NaN-boxed boolean; bail if dst is tracked.
                // Uses read_reg_versioned for potentially-tracked operands.
                Instruction::StrictEq { dst, lhs, rhs } => {
                    // Check fusion FIRST — if the result only feeds a branch,
                    // we don't need to materialize even if dst is a tracked i32.
                    if vl.reg_to_i32.contains_key(&lhs.0)
                        && vl.reg_to_i32.contains_key(&rhs.0)
                        && can_fuse_versioned_compare_branch(instructions_ref, body_pc, *dst)
                    {
                        if body_idx + 1 < body_len {
                            builder.ins().jump(vl.opt_blocks[body_idx + 1], &[]);
                        }
                        continue;
                    }
                    if vl.reg_to_i32.contains_key(&dst.0) {
                        materialize_all_i32(builder, &reg_vars, &local_vars, vl);
                        builder.ins().jump(blocks[body_pc], &[]);
                        continue;
                    }
                    let left = read_reg_versioned(builder, &reg_vars, vl, *lhs);
                    let right = read_reg_versioned(builder, &reg_vars, vl, *rhs);
                    let out = type_guards::emit_strict_eq(builder, left, right, false);
                    write_reg(builder, &reg_vars, *dst, out);
                }
                Instruction::StrictNe { dst, lhs, rhs } => {
                    // Check fusion FIRST (same as StrictEq).
                    if vl.reg_to_i32.contains_key(&lhs.0)
                        && vl.reg_to_i32.contains_key(&rhs.0)
                        && can_fuse_versioned_compare_branch(instructions_ref, body_pc, *dst)
                    {
                        if body_idx + 1 < body_len {
                            builder.ins().jump(vl.opt_blocks[body_idx + 1], &[]);
                        }
                        continue;
                    }
                    if vl.reg_to_i32.contains_key(&dst.0) {
                        materialize_all_i32(builder, &reg_vars, &local_vars, vl);
                        builder.ins().jump(blocks[body_pc], &[]);
                        continue;
                    }
                    let left = read_reg_versioned(builder, &reg_vars, vl, *lhs);
                    let right = read_reg_versioned(builder, &reg_vars, vl, *rhs);
                    let out = type_guards::emit_strict_eq(builder, left, right, true);
                    write_reg(builder, &reg_vars, *dst, out);
                }
                // --- Not ---
                Instruction::Not { dst, src } => {
                    if vl.reg_to_i32.contains_key(&dst.0) {
                        materialize_all_i32(builder, &reg_vars, &local_vars, vl);
                        builder.ins().jump(blocks[body_pc], &[]);
                        continue;
                    }
                    let val = read_reg_versioned(builder, &reg_vars, vl, *src);
                    let truthy = type_guards::emit_is_truthy(builder, val);
                    let is_falsy = builder.ins().icmp_imm(IntCC::Equal, truthy, 0);
                    let out = type_guards::emit_bool_to_nanbox(builder, is_falsy);
                    write_reg(builder, &reg_vars, *dst, out);
                }
                // --- Control flow in optimized body ---
                Instruction::Jump { offset } => {
                    let target = jump_target(body_pc, offset.offset(), instruction_count)?;
                    // Back-edge → stay in optimized; exit → materialize and leave
                    if target == vl.header_pc {
                        builder.ins().jump(vl.opt_blocks[0], &[]);
                    } else if target >= vl.header_pc && target <= vl.back_edge_pc {
                        builder
                            .ins()
                            .jump(vl.opt_blocks[target - vl.header_pc], &[]);
                    } else {
                        materialize_all_i32(builder, &reg_vars, &local_vars, vl);
                        builder.ins().jump(blocks[target], &[]);
                    }
                    continue;
                }
                Instruction::JumpIfTrue { cond, offset } => {
                    let jump_to = jump_target(body_pc, offset.offset(), instruction_count)?;
                    let jump_in_loop = jump_to >= vl.header_pc && jump_to <= vl.back_edge_pc;
                    let jump_block = if jump_in_loop {
                        vl.opt_blocks[jump_to - vl.header_pc]
                    } else {
                        builder.create_block() // interpose for materialization
                    };
                    let fallthrough = body_pc + 1;
                    let ft_in_loop = fallthrough >= vl.header_pc && fallthrough <= vl.back_edge_pc;
                    let ft_block = if ft_in_loop {
                        vl.opt_blocks[fallthrough - vl.header_pc]
                    } else if fallthrough < instruction_count {
                        builder.create_block() // interpose for materialization
                    } else {
                        exit
                    };
                    if let Some(is_truthy) = try_emit_versioned_fused_compare_condition(
                        builder,
                        &reg_vars,
                        vl,
                        instructions_ref,
                        body_pc,
                        *cond,
                    ) {
                        builder
                            .ins()
                            .brif(is_truthy, jump_block, &[], ft_block, &[]);
                    } else {
                        let condition_kind =
                            classify_jump_condition(instructions_ref, body_pc, *cond);
                        match condition_kind {
                            JumpConditionKind::Constant(true) => {
                                builder.ins().jump(jump_block, &[]);
                            }
                            JumpConditionKind::Constant(false) => {
                                builder.ins().jump(ft_block, &[]);
                            }
                            JumpConditionKind::BoxedBoolean | JumpConditionKind::Generic => {
                                let cond_val = read_reg_versioned(builder, &reg_vars, vl, *cond);
                                let is_truthy =
                                    emit_jump_truthy_value(builder, condition_kind, cond_val);
                                builder
                                    .ins()
                                    .brif(is_truthy, jump_block, &[], ft_block, &[]);
                            }
                        }
                    }
                    // Emit interpose blocks that materialize before leaving the loop
                    if !jump_in_loop {
                        builder.switch_to_block(jump_block);
                        materialize_all_i32(builder, &reg_vars, &local_vars, vl);
                        builder.ins().jump(blocks[jump_to], &[]);
                    }
                    if !ft_in_loop && fallthrough < instruction_count {
                        builder.switch_to_block(ft_block);
                        materialize_all_i32(builder, &reg_vars, &local_vars, vl);
                        builder.ins().jump(blocks[fallthrough], &[]);
                    }
                    continue;
                }
                Instruction::JumpIfFalse { cond, offset } => {
                    let jump_to = jump_target(body_pc, offset.offset(), instruction_count)?;
                    let jump_in_loop = jump_to >= vl.header_pc && jump_to <= vl.back_edge_pc;
                    let jump_block = if jump_in_loop {
                        vl.opt_blocks[jump_to - vl.header_pc]
                    } else {
                        builder.create_block()
                    };
                    let fallthrough = body_pc + 1;
                    let ft_in_loop = fallthrough >= vl.header_pc && fallthrough <= vl.back_edge_pc;
                    let ft_block = if ft_in_loop {
                        vl.opt_blocks[fallthrough - vl.header_pc]
                    } else if fallthrough < instruction_count {
                        builder.create_block()
                    } else {
                        exit
                    };
                    if let Some(is_truthy) = try_emit_versioned_fused_compare_condition(
                        builder,
                        &reg_vars,
                        vl,
                        instructions_ref,
                        body_pc,
                        *cond,
                    ) {
                        builder
                            .ins()
                            .brif(is_truthy, ft_block, &[], jump_block, &[]);
                    } else {
                        let condition_kind =
                            classify_jump_condition(instructions_ref, body_pc, *cond);
                        match condition_kind {
                            JumpConditionKind::Constant(true) => {
                                builder.ins().jump(ft_block, &[]);
                            }
                            JumpConditionKind::Constant(false) => {
                                builder.ins().jump(jump_block, &[]);
                            }
                            JumpConditionKind::BoxedBoolean | JumpConditionKind::Generic => {
                                let cond_val = read_reg_versioned(builder, &reg_vars, vl, *cond);
                                let is_truthy =
                                    emit_jump_truthy_value(builder, condition_kind, cond_val);
                                builder
                                    .ins()
                                    .brif(is_truthy, ft_block, &[], jump_block, &[]);
                            }
                        }
                    }
                    if !jump_in_loop {
                        builder.switch_to_block(jump_block);
                        materialize_all_i32(builder, &reg_vars, &local_vars, vl);
                        builder.ins().jump(blocks[jump_to], &[]);
                    }
                    if !ft_in_loop && fallthrough < instruction_count {
                        builder.switch_to_block(ft_block);
                        materialize_all_i32(builder, &reg_vars, &local_vars, vl);
                        builder.ins().jump(blocks[fallthrough], &[]);
                    }
                    continue;
                }
                Instruction::JumpIfNullish { src, offset } => {
                    let src_val = read_reg_versioned(builder, &reg_vars, vl, *src);
                    let is_nullish = type_guards::emit_is_nullish(builder, src_val);
                    let jump_to = jump_target(body_pc, offset.offset(), instruction_count)?;
                    let jump_in_loop = jump_to >= vl.header_pc && jump_to <= vl.back_edge_pc;
                    let jump_block = if jump_in_loop {
                        vl.opt_blocks[jump_to - vl.header_pc]
                    } else {
                        builder.create_block()
                    };
                    let fallthrough = body_pc + 1;
                    let ft_in_loop = fallthrough >= vl.header_pc && fallthrough <= vl.back_edge_pc;
                    let ft_block = if ft_in_loop {
                        vl.opt_blocks[fallthrough - vl.header_pc]
                    } else if fallthrough < instruction_count {
                        builder.create_block()
                    } else {
                        exit
                    };
                    builder
                        .ins()
                        .brif(is_nullish, jump_block, &[], ft_block, &[]);
                    if !jump_in_loop {
                        builder.switch_to_block(jump_block);
                        materialize_all_i32(builder, &reg_vars, &local_vars, vl);
                        builder.ins().jump(blocks[jump_to], &[]);
                    }
                    if !ft_in_loop && fallthrough < instruction_count {
                        builder.switch_to_block(ft_block);
                        materialize_all_i32(builder, &reg_vars, &local_vars, vl);
                        builder.ins().jump(blocks[fallthrough], &[]);
                    }
                    continue;
                }
                Instruction::JumpIfNotNullish { src, offset } => {
                    let src_val = read_reg_versioned(builder, &reg_vars, vl, *src);
                    let is_nullish = type_guards::emit_is_nullish(builder, src_val);
                    let jump_to = jump_target(body_pc, offset.offset(), instruction_count)?;
                    let jump_in_loop = jump_to >= vl.header_pc && jump_to <= vl.back_edge_pc;
                    let jump_block = if jump_in_loop {
                        vl.opt_blocks[jump_to - vl.header_pc]
                    } else {
                        builder.create_block()
                    };
                    let fallthrough = body_pc + 1;
                    let ft_in_loop = fallthrough >= vl.header_pc && fallthrough <= vl.back_edge_pc;
                    let ft_block = if ft_in_loop {
                        vl.opt_blocks[fallthrough - vl.header_pc]
                    } else if fallthrough < instruction_count {
                        builder.create_block()
                    } else {
                        exit
                    };
                    builder
                        .ins()
                        .brif(is_nullish, ft_block, &[], jump_block, &[]);
                    if !jump_in_loop {
                        builder.switch_to_block(jump_block);
                        materialize_all_i32(builder, &reg_vars, &local_vars, vl);
                        builder.ins().jump(blocks[jump_to], &[]);
                    }
                    if !ft_in_loop && fallthrough < instruction_count {
                        builder.switch_to_block(ft_block);
                        materialize_all_i32(builder, &reg_vars, &local_vars, vl);
                        builder.ins().jump(blocks[fallthrough], &[]);
                    }
                    continue;
                }
                // --- Return ---
                Instruction::Return { src } => {
                    let out = read_reg_versioned(builder, &reg_vars, vl, *src);
                    builder.ins().return_(&[out]);
                    continue;
                }
                Instruction::ReturnUndefined => {
                    let undef = builder.ins().iconst(types::I64, type_guards::TAG_UNDEFINED);
                    builder.ins().return_(&[undef]);
                    continue;
                }
                // --- CloseUpvalue: no-op if not captured, bail otherwise ---
                Instruction::CloseUpvalue { local_idx } => {
                    if captured_locals.contains(&local_idx.index()) {
                        // Actually captured — bail to guarded path for real close
                        materialize_all_i32(builder, &reg_vars, &local_vars, vl);
                        builder.ins().jump(blocks[body_pc], &[]);
                        continue;
                    }
                    // Not captured — treat as no-op in the versioned path
                }
                Instruction::Nop => {}
                // --- Inline property reads in versioned loops ---
                // If shape was hoisted to pre-header: direct load, ZERO checks (~1 instruction).
                // Otherwise: full inline read with shape check (~11 instructions).
                Instruction::GetPropConst { dst, obj, name, ic_index } => {
                    let mono_ic = ic_snapshot.get(*ic_index as usize).and_then(|ic| {
                        if let InlineCacheState::Monomorphic { shape_id, offset, depth: 0, .. } = ic {
                            Some((*shape_id, *offset))
                        } else {
                            None
                        }
                    });
                    let layout = crate::runtime_helpers::jsobject_layout();

                    // LICM fast path: shape pre-verified in pre-header → direct load
                    if let (Some((_sid, offset)), Some(lo), Some(&ptr_var)) =
                        (mono_ic, layout, vl.shape_hoisted_ptrs.get(&obj.0))
                    {
                        if (offset as usize) < 8 {
                            let obj_ptr = builder.use_var(ptr_var);
                            let value_byte_offset = lo.inline_slots_data + (offset as i32) * 8;
                            let prop_val = builder.ins().load(
                                types::I64, MemFlags::trusted(), obj_ptr, value_byte_offset,
                            );
                            if vl.reg_to_i32.contains_key(&dst.0) {
                                let is_i32 = type_guards::emit_is_int32(builder, prop_val);
                                let ok = builder.create_block();
                                let bail = builder.create_block();
                                builder.ins().brif(is_i32, ok, &[], bail, &[]);
                                builder.switch_to_block(bail);
                                materialize_all_i32(builder, &reg_vars, &local_vars, vl);
                                builder.ins().jump(blocks[body_pc], &[]);
                                builder.switch_to_block(ok);
                                let raw_i32 = type_guards::emit_unbox_int32(builder, prop_val);
                                let j = vl.reg_to_i32[&dst.0];
                                builder.def_var(vl.i32_vars[j], raw_i32);
                                write_reg(builder, &reg_vars, *dst, prop_val);
                            } else {
                                write_reg(builder, &reg_vars, *dst, prop_val);
                            }
                        } else {
                            materialize_all_i32(builder, &reg_vars, &local_vars, vl);
                            builder.ins().jump(blocks[body_pc], &[]);
                            continue;
                        }
                    } else if let (Some((shape_id, offset)), Some(lo)) = (mono_ic, layout) {
                        // Non-hoisted: full inline read with shape check
                        if (offset as usize) < 8 {
                            let gp_ref = helpers.and_then(|h| h.get(HelperKind::GetPropConst));
                            if let Some(gp) = gp_ref {
                                let obj_val = read_reg_versioned(builder, &reg_vars, vl, *obj);
                                let prop_val = emit_inline_prop_read(
                                    builder, gp, obj_val, ctx_ptr,
                                    shape_id, offset, name.index(), *ic_index, &lo,
                                );
                                if vl.reg_to_i32.contains_key(&dst.0) {
                                    let is_i32 = type_guards::emit_is_int32(builder, prop_val);
                                    let ok = builder.create_block();
                                    let bail = builder.create_block();
                                    builder.ins().brif(is_i32, ok, &[], bail, &[]);
                                    builder.switch_to_block(bail);
                                    materialize_all_i32(builder, &reg_vars, &local_vars, vl);
                                    builder.ins().jump(blocks[body_pc], &[]);
                                    builder.switch_to_block(ok);
                                    let raw_i32 = type_guards::emit_unbox_int32(builder, prop_val);
                                    let j = vl.reg_to_i32[&dst.0];
                                    builder.def_var(vl.i32_vars[j], raw_i32);
                                    write_reg(builder, &reg_vars, *dst, prop_val);
                                } else {
                                    write_reg(builder, &reg_vars, *dst, prop_val);
                                }
                            } else {
                                materialize_all_i32(builder, &reg_vars, &local_vars, vl);
                                builder.ins().jump(blocks[body_pc], &[]);
                                continue;
                            }
                        } else {
                            materialize_all_i32(builder, &reg_vars, &local_vars, vl);
                            builder.ins().jump(blocks[body_pc], &[]);
                            continue;
                        }
                    } else {
                        materialize_all_i32(builder, &reg_vars, &local_vars, vl);
                        builder.ins().jump(blocks[body_pc], &[]);
                        continue;
                    }
                }
                Instruction::GetLocalProp { dst, local_idx, name, ic_index } => {
                    let get_prop_ref = helpers.and_then(|h| h.get(HelperKind::GetPropConst));
                    let layout = crate::runtime_helpers::jsobject_layout();
                    let mono_ic = ic_snapshot.get(*ic_index as usize).and_then(|ic| {
                        if let InlineCacheState::Monomorphic { shape_id, offset, depth: 0, .. } = ic {
                            Some((*shape_id, *offset))
                        } else {
                            None
                        }
                    });
                    if let (Some((shape_id, offset)), Some(lo), Some(gp_ref)) = (mono_ic, layout, get_prop_ref) {
                        if (offset as usize) < 8 {
                            let obj_val = read_local(builder, &local_vars, *local_idx);
                            let prop_val = emit_inline_prop_read(
                                builder, gp_ref, obj_val, ctx_ptr,
                                shape_id, offset, name.index(), *ic_index, &lo,
                            );
                            if vl.reg_to_i32.contains_key(&dst.0) {
                                let is_i32 = type_guards::emit_is_int32(builder, prop_val);
                                let ok = builder.create_block();
                                let bail = builder.create_block();
                                builder.ins().brif(is_i32, ok, &[], bail, &[]);

                                builder.switch_to_block(bail);
                                materialize_all_i32(builder, &reg_vars, &local_vars, vl);
                                builder.ins().jump(blocks[body_pc], &[]);

                                builder.switch_to_block(ok);
                                let raw_i32 = type_guards::emit_unbox_int32(builder, prop_val);
                                let j = vl.reg_to_i32[&dst.0];
                                builder.def_var(vl.i32_vars[j], raw_i32);
                                write_reg(builder, &reg_vars, *dst, prop_val);
                            } else {
                                write_reg(builder, &reg_vars, *dst, prop_val);
                            }
                        } else {
                            materialize_all_i32(builder, &reg_vars, &local_vars, vl);
                            builder.ins().jump(blocks[body_pc], &[]);
                            continue;
                        }
                    } else {
                        materialize_all_i32(builder, &reg_vars, &local_vars, vl);
                        builder.ins().jump(blocks[body_pc], &[]);
                        continue;
                    }
                }
                // --- Anything else: materialize and transfer to guarded version ---
                _ => {
                    materialize_all_i32(builder, &reg_vars, &local_vars, vl);
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
                    materialize_all_i32(builder, &reg_vars, &local_vars, vl);
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
    use otter_vm_bytecode::function::UpvalueCapture;
    use otter_vm_bytecode::operand::{FunctionIndex, JumpOffset, LocalIndex, Register};
    use otter_vm_compiler::Compiler;

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

    #[test]
    fn captured_local_analysis_finds_nested_closure_locals() {
        let outer = Function::builder()
            .name("outer")
            .register_count(1)
            .instruction(Instruction::Closure {
                dst: Register(0),
                func: FunctionIndex(1),
            })
            .instruction(Instruction::AsyncClosure {
                dst: Register(0),
                func: FunctionIndex(2),
            })
            .instruction(Instruction::ReturnUndefined)
            .build();

        let inner_plain = Function::builder()
            .name("inner_plain")
            .upvalues(vec![UpvalueCapture::Local(LocalIndex(3))])
            .instruction(Instruction::ReturnUndefined)
            .build();

        let inner_async = Function::builder()
            .name("inner_async")
            .upvalues(vec![
                UpvalueCapture::Local(LocalIndex(5)),
                UpvalueCapture::Upvalue(LocalIndex(1)),
            ])
            .instruction(Instruction::ReturnUndefined)
            .build();

        let captured = resolve_locally_captured_locals(
            &outer.instructions.read(),
            &[(0, outer.clone()), (1, inner_plain), (2, inner_async)],
        );

        assert!(captured.contains(&3));
        assert!(captured.contains(&5));
        assert!(!captured.contains(&1));
        assert!(!captured.contains(&7));
    }

    #[test]
    fn inline_candidate_resolution_accepts_local_upvalue_calls() {
        let outer = Function::builder()
            .name("outer")
            .register_count(8)
            .local_count(4)
            .instruction(Instruction::Closure {
                dst: Register(0),
                func: FunctionIndex(1),
            })
            .instruction(Instruction::SetLocal {
                idx: LocalIndex(2),
                src: Register(0),
            })
            .instruction(Instruction::Closure {
                dst: Register(1),
                func: FunctionIndex(2),
            })
            .instruction(Instruction::SetLocal {
                idx: LocalIndex(3),
                src: Register(1),
            })
            .instruction(Instruction::Closure {
                dst: Register(2),
                func: FunctionIndex(3),
            })
            .instruction(Instruction::SetLocal {
                idx: LocalIndex(0),
                src: Register(2),
            })
            .instruction(Instruction::GetLocal {
                dst: Register(3),
                idx: LocalIndex(0),
            })
            .instruction(Instruction::GetLocal {
                dst: Register(4),
                idx: LocalIndex(1),
            })
            .instruction(Instruction::LoadInt32 {
                dst: Register(5),
                value: 7,
            })
            .instruction(Instruction::Call {
                dst: Register(6),
                func: Register(3),
                argc: 2,
                ic_index: 0,
            })
            .instruction(Instruction::Return { src: Register(6) })
            .build();

        let add = Function::builder()
            .name("add")
            .register_count(3)
            .local_count(2)
            .instruction(Instruction::GetLocal {
                dst: Register(0),
                idx: LocalIndex(0),
            })
            .instruction(Instruction::GetLocal {
                dst: Register(1),
                idx: LocalIndex(1),
            })
            .instruction(Instruction::Add {
                dst: Register(2),
                lhs: Register(0),
                rhs: Register(1),
                feedback_index: 0,
            })
            .instruction(Instruction::Return { src: Register(2) })
            .feedback_vector_size(1)
            .build();

        let mul = Function::builder()
            .name("mul")
            .register_count(3)
            .local_count(2)
            .instruction(Instruction::GetLocal {
                dst: Register(0),
                idx: LocalIndex(0),
            })
            .instruction(Instruction::GetLocal {
                dst: Register(1),
                idx: LocalIndex(1),
            })
            .instruction(Instruction::Mul {
                dst: Register(2),
                lhs: Register(0),
                rhs: Register(1),
                feedback_index: 0,
            })
            .instruction(Instruction::Return { src: Register(2) })
            .feedback_vector_size(1)
            .build();

        let call_chain = Function::builder()
            .name("call_chain")
            .register_count(7)
            .local_count(2)
            .upvalues(vec![
                UpvalueCapture::Local(LocalIndex(2)),
                UpvalueCapture::Local(LocalIndex(3)),
            ])
            .instruction(Instruction::GetUpvalue {
                dst: Register(0),
                idx: LocalIndex(0),
            })
            .instruction(Instruction::GetUpvalue {
                dst: Register(1),
                idx: LocalIndex(1),
            })
            .instruction(Instruction::GetLocal {
                dst: Register(2),
                idx: LocalIndex(0),
            })
            .instruction(Instruction::GetLocal {
                dst: Register(3),
                idx: LocalIndex(1),
            })
            .instruction(Instruction::Call {
                dst: Register(4),
                func: Register(0),
                argc: 2,
                ic_index: 0,
            })
            .instruction(Instruction::Sub {
                dst: Register(5),
                lhs: Register(3),
                rhs: Register(2),
                feedback_index: 0,
            })
            .instruction(Instruction::Call {
                dst: Register(6),
                func: Register(1),
                argc: 2,
                ic_index: 0,
            })
            .instruction(Instruction::Return { src: Register(6) })
            .feedback_vector_size(1)
            .build();

        let module_functions = vec![(0, outer.clone()), (1, add), (2, mul), (3, call_chain)];
        let inline_sites = resolve_inline_candidates(&outer.instructions.read(), &module_functions);

        let candidate = inline_sites
            .get(&9)
            .expect("outer call site should keep local-upvalue closure inlineable");
        assert_eq!(candidate.function_index, 3);
        assert_eq!(candidate.local_func_snapshot.get(&2), Some(&2));
        assert_eq!(candidate.local_func_snapshot.get(&3), Some(&3));
    }

    #[test]
    fn inline_candidate_resolution_tracks_get_local2_function_values() {
        let outer = Function::builder()
            .name("outer")
            .register_count(6)
            .local_count(3)
            .instruction(Instruction::Closure {
                dst: Register(0),
                func: FunctionIndex(1),
            })
            .instruction(Instruction::SetLocal {
                idx: LocalIndex(2),
                src: Register(0),
            })
            .instruction(Instruction::GetLocal2 {
                dst1: Register(1),
                idx1: LocalIndex(2),
                dst2: Register(2),
                idx2: LocalIndex(0),
            })
            .instruction(Instruction::GetLocal {
                dst: Register(3),
                idx: LocalIndex(1),
            })
            .instruction(Instruction::Call {
                dst: Register(4),
                func: Register(1),
                argc: 2,
                ic_index: 0,
            })
            .instruction(Instruction::Return { src: Register(4) })
            .build();

        let callee = Function::builder()
            .name("callee")
            .register_count(3)
            .local_count(2)
            .instruction(Instruction::GetLocal {
                dst: Register(0),
                idx: LocalIndex(0),
            })
            .instruction(Instruction::GetLocal {
                dst: Register(1),
                idx: LocalIndex(1),
            })
            .instruction(Instruction::Add {
                dst: Register(2),
                lhs: Register(0),
                rhs: Register(1),
                feedback_index: 0,
            })
            .instruction(Instruction::Return { src: Register(2) })
            .feedback_vector_size(1)
            .build();

        let module_functions = vec![(0, outer.clone()), (1, callee)];
        let inline_sites = resolve_inline_candidates(&outer.instructions.read(), &module_functions);

        assert!(
            inline_sites.contains_key(&4),
            "GetLocal2 should preserve known closure values through the call site"
        );
    }

    #[test]
    fn osr_headers_include_non_qualifying_property_loops() {
        use otter_vm_bytecode::operand::{ConstantIndex, JumpOffset};

        let instructions = vec![
            Instruction::GetLocal {
                dst: Register(0),
                idx: LocalIndex(0),
            },
            Instruction::GetPropConst {
                dst: Register(1),
                obj: Register(0),
                name: ConstantIndex(0),
                ic_index: 0,
            },
            Instruction::Jump {
                offset: JumpOffset(-2),
            },
        ];

        let loops = loop_analysis::detect_loops(&instructions, &[]);
        assert_eq!(loops.len(), 1);
        assert!(!loops[0].qualifies);
        assert_eq!(collect_osr_loop_headers(&loops), vec![0]);
    }

    #[test]
    fn classify_jump_condition_detects_boolean_producers() {
        let instructions = vec![
            Instruction::StrictEq {
                dst: Register(3),
                lhs: Register(1),
                rhs: Register(2),
            },
            Instruction::JumpIfFalse {
                cond: Register(3),
                offset: JumpOffset(1),
            },
        ];

        assert_eq!(
            classify_jump_condition(&instructions, 1, Register(3)),
            JumpConditionKind::BoxedBoolean
        );
    }

    #[test]
    fn classify_jump_condition_detects_constant_booleans() {
        let instructions = vec![
            Instruction::LoadFalse { dst: Register(0) },
            Instruction::JumpIfTrue {
                cond: Register(0),
                offset: JumpOffset(1),
            },
        ];

        assert_eq!(
            classify_jump_condition(&instructions, 1, Register(0)),
            JumpConditionKind::Constant(false)
        );
    }

    #[test]
    fn versioned_compare_branch_fusion_allows_dead_boolean_temp() {
        let instructions = vec![
            Instruction::StrictEq {
                dst: Register(3),
                lhs: Register(1),
                rhs: Register(2),
            },
            Instruction::JumpIfFalse {
                cond: Register(3),
                offset: JumpOffset(1),
            },
            Instruction::ReturnUndefined,
        ];

        assert!(can_fuse_versioned_compare_branch(
            &instructions,
            0,
            Register(3)
        ));
    }

    #[test]
    fn versioned_compare_branch_fusion_rejects_live_boolean_temp() {
        let instructions = vec![
            Instruction::StrictEq {
                dst: Register(3),
                lhs: Register(1),
                rhs: Register(2),
            },
            Instruction::JumpIfFalse {
                cond: Register(3),
                offset: JumpOffset(2),
            },
            Instruction::Return { src: Register(3) },
        ];

        assert!(!can_fuse_versioned_compare_branch(
            &instructions,
            0,
            Register(3)
        ));
    }

    #[test]
    fn real_compiled_nested_closure_shape_exposes_inline_snapshot_gap() {
        let module = Compiler::new()
            .compile(
                r#"
                function outer(a, b) {
                    function add(x, y) { return x + y; }
                    function mul(x, y) { return x * y; }
                    function callChain(x, y) { return mul(add(x, y), y - x); }
                    return callChain(a, b);
                }
                outer(3, 7);
                "#,
                "inline-shape.js",
                false,
            )
            .expect("source should compile");

        let module_functions: Vec<(u32, Function)> = module
            .functions
            .iter()
            .enumerate()
            .map(|(idx, func)| (idx as u32, func.clone()))
            .collect();

        let outer_idx = module
            .functions
            .iter()
            .position(|func| func.name.as_deref() == Some("outer"))
            .expect("outer function should exist") as u32;
        let outer = module
            .function(outer_idx)
            .expect("outer function should be accessible");

        let inline_sites = resolve_inline_candidates(&outer.instructions.read(), &module_functions);
        let candidate = inline_sites
            .values()
            .find(|candidate| candidate.callee.name.as_deref() == Some("callChain"))
            .expect("callChain should be an inline candidate from real compiled bytecode");

        let call_chain_idx = module
            .functions
            .iter()
            .position(|func| func.name.as_deref() == Some("callChain"))
            .expect("callChain function should exist");
        let call_chain = &module.functions[call_chain_idx];
        for capture in &call_chain.upvalues {
            let UpvalueCapture::Local(local_idx) = capture else {
                panic!("callChain should capture direct parent locals");
            };
            assert!(
                candidate
                    .local_func_snapshot
                    .contains_key(&local_idx.index()),
                "real compiled outer function should preserve captured closure local {} for inline propagation",
                local_idx.index()
            );
        }
    }

    #[test]
    fn real_compiled_add_and_mul_are_leaf_inlineable() {
        let module = Compiler::new()
            .compile(
                r#"
                function outer(a, b) {
                    function add(x, y) { return x + y; }
                    function mul(x, y) { return x * y; }
                    function callChain(x, y) { return mul(add(x, y), y - x); }
                    return callChain(a, b);
                }
                outer(3, 7);
                "#,
                "inline-leaf-shape.js",
                false,
            )
            .expect("source should compile");

        let add = module
            .functions
            .iter()
            .find(|func| func.name.as_deref() == Some("add"))
            .expect("add should exist");
        let mul = module
            .functions
            .iter()
            .find(|func| func.name.as_deref() == Some("mul"))
            .expect("mul should exist");
        let call_chain = module
            .functions
            .iter()
            .find(|func| func.name.as_deref() == Some("callChain"))
            .expect("callChain should exist");

        assert!(can_inline_leaf_function(add));
        assert!(can_inline_leaf_function(mul));
        assert!(!can_inline_leaf_function(call_chain));
    }

    #[test]
    fn real_compiled_binary_leaf_specs_preserve_param_order() {
        let module = Compiler::new()
            .compile(
                r#"
                function outer(a, b) {
                    function add(x, y) { return x + y; }
                    function mul(x, y) { return x * y; }
                    return add(a, mul(a, b));
                }
                outer(3, 7);
                "#,
                "inline-leaf-spec.js",
                false,
            )
            .expect("source should compile");

        let add = module
            .functions
            .iter()
            .find(|func| func.name.as_deref() == Some("add"))
            .expect("add should exist");
        let mul = module
            .functions
            .iter()
            .find(|func| func.name.as_deref() == Some("mul"))
            .expect("mul should exist");

        let add_spec = match_binary_leaf_arith(add).expect("add should match binary leaf shape");
        assert!(matches!(add_spec.op, ArithOp::Add));
        assert_eq!(add_spec.lhs_param, 0);
        assert_eq!(add_spec.rhs_param, 1);
        assert!(add_spec.uses_generic_fallback);

        let mul_spec = match_binary_leaf_arith(mul).expect("mul should match binary leaf shape");
        assert!(matches!(mul_spec.op, ArithOp::Mul));
        assert_eq!(mul_spec.lhs_param, 0);
        assert_eq!(mul_spec.rhs_param, 1);
        assert!(mul_spec.uses_generic_fallback);
    }

    #[test]
    fn real_compiled_objects_phase_exposes_loop_shape() {
        let module = Compiler::new()
            .compile(
                r#"
                function objectsPhase(mult) {
                    const count = 10_000 * mult;
                    const objs = new Array(count);
                    for (let i = 0; i < count; i++) {
                        objs[i] = { a: i, b: i + 1, c: i + 2 };
                    }

                    let sum = 0;
                    const loops = 200 * mult;
                    for (let l = 0; l < loops; l++) {
                        for (let i = 0; i < count; i++) {
                            const obj = objs[i];
                            sum += obj.a + obj.b;
                            obj.b = obj.b + 1;
                            obj.c = obj.b + obj.a;
                        }
                    }
                    return sum;
                }
                objectsPhase(1);
                "#,
                "objects-phase-shape.js",
                false,
            )
            .expect("source should compile");

        let objects_phase = module
            .functions
            .iter()
            .find(|func| func.name.as_deref() == Some("objectsPhase"))
            .expect("objectsPhase should exist");

        let instructions = objects_phase.instructions.read();
        let feedback_snapshot: Vec<_> = objects_phase
            .feedback_vector
            .read()
            .iter()
            .map(|m| m.type_observations)
            .collect();
        let loops = loop_analysis::detect_loops(&instructions, &feedback_snapshot);
        let has_backward_jump = instructions.iter().any(|instruction| {
            matches!(
                instruction,
                Instruction::Jump { offset }
                    | Instruction::JumpIfTrue { offset, .. }
                    | Instruction::JumpIfFalse { offset, .. }
                    | Instruction::JumpIfNullish { offset, .. }
                    | Instruction::JumpIfNotNullish { offset, .. }
                    | Instruction::ForInNext { offset, .. }
                    if offset.0 < 0
            )
        });

        assert!(has_backward_jump);
        assert_eq!(
            loops
                .iter()
                .map(|info| (info.header_pc, info.back_edge_pc, info.qualifies))
                .collect::<Vec<_>>(),
            vec![(12, 38, false), (53, 87, false), (47, 93, false)]
        );
    }

    #[test]
    fn backwards_truncation_direct_bitor() {
        use otter_vm_bytecode::operand::Register;
        // (x + y) | 0 — Add consumed by BitOr → wrapping
        let instructions = vec![
            Instruction::Add {
                dst: Register(0),
                lhs: Register(1),
                rhs: Register(2),
                feedback_index: 0,
            },
            Instruction::LoadInt32 {
                dst: Register(3),
                value: 0,
            },
            Instruction::BitOr {
                dst: Register(4),
                lhs: Register(0),
                rhs: Register(3),
            },
        ];
        let wrapping = build_wrapping_set(&instructions, 0, 2);
        assert!(
            wrapping.contains(&0),
            "Add consumed by BitOr should be wrapping"
        );
    }

    #[test]
    fn backwards_truncation_chain() {
        use otter_vm_bytecode::operand::Register;
        // (x * 3 + 1) | 0 — Mul→Add→BitOr chain, both Mul and Add should be wrapping
        let instructions = vec![
            // PC 0: Mul r0 = r1 * r2
            Instruction::Mul {
                dst: Register(0),
                lhs: Register(1),
                rhs: Register(2),
                feedback_index: 0,
            },
            // PC 1: Add r0 = r0 + r3
            Instruction::Add {
                dst: Register(0),
                lhs: Register(0),
                rhs: Register(3),
                feedback_index: 1,
            },
            // PC 2: LoadInt32 r4 = 0
            Instruction::LoadInt32 {
                dst: Register(4),
                value: 0,
            },
            // PC 3: BitOr r5 = r0 | r4
            Instruction::BitOr {
                dst: Register(5),
                lhs: Register(0),
                rhs: Register(4),
            },
        ];
        let wrapping = build_wrapping_set(&instructions, 0, 3);
        assert!(
            wrapping.contains(&1),
            "Add consumed by BitOr should be wrapping"
        );
        assert!(
            wrapping.contains(&0),
            "Mul consumed by wrapping Add should be wrapping"
        );
    }

    #[test]
    fn backwards_truncation_non_bitwise_consumer() {
        use otter_vm_bytecode::operand::Register;
        // x + y consumed by Lt (comparison) → NOT wrapping
        let instructions = vec![
            Instruction::Add {
                dst: Register(0),
                lhs: Register(1),
                rhs: Register(2),
                feedback_index: 0,
            },
            Instruction::Lt {
                dst: Register(3),
                lhs: Register(0),
                rhs: Register(4),
            },
        ];
        let wrapping = build_wrapping_set(&instructions, 0, 1);
        assert!(
            !wrapping.contains(&0),
            "Add consumed by Lt should NOT be wrapping"
        );
    }

    #[test]
    fn backwards_truncation_mixed_consumers() {
        use otter_vm_bytecode::operand::Register;
        // x + y consumed by BOTH BitOr AND Lt → NOT wrapping (not ALL consumers are bitwise)
        let instructions = vec![
            Instruction::Add {
                dst: Register(0),
                lhs: Register(1),
                rhs: Register(2),
                feedback_index: 0,
            },
            Instruction::BitOr {
                dst: Register(3),
                lhs: Register(0),
                rhs: Register(4),
            },
            Instruction::Lt {
                dst: Register(5),
                lhs: Register(0),
                rhs: Register(6),
            },
        ];
        let wrapping = build_wrapping_set(&instructions, 0, 2);
        assert!(
            !wrapping.contains(&0),
            "Add with mixed consumers should NOT be wrapping"
        );
    }

    #[test]
    fn math_phase_bytecode_shows_local_access_pattern() {
        let module = Compiler::new()
            .compile(
                r#"
                function mathPhase() {
                    let acc = 0;
                    const iterations = 100;
                    for (let i = 0; i < iterations; i++) {
                        acc = (acc + i) | 0;
                        acc ^= (acc << 1);
                        if ((acc & 1) === 0) {
                            acc = (acc * 3 + 1) | 0;
                        }
                    }
                    return acc;
                }
                mathPhase();
                "#,
                "math-phase-locals.js",
                false,
            )
            .expect("source should compile");

        let math = module
            .functions
            .iter()
            .find(|func| func.name.as_deref() == Some("mathPhase"))
            .expect("mathPhase should exist");

        let instructions = math.instructions.read();
        // Dump bytecode for diagnostic
        for (i, instr) in instructions.iter().enumerate() {
            eprintln!("{:04}: {:?}", i, instr);
        }
        // Verify the loop exists and has some form of local access
        let has_get_local = instructions
            .iter()
            .any(|i| matches!(i, Instruction::GetLocal { .. }));
        let has_get_local2 = instructions
            .iter()
            .any(|i| matches!(i, Instruction::GetLocal2 { .. }));
        let has_set_local = instructions
            .iter()
            .any(|i| matches!(i, Instruction::SetLocal { .. }));
        eprintln!(
            "GetLocal: {}, GetLocal2: {}, SetLocal: {}",
            has_get_local, has_get_local2, has_set_local
        );
        // At least one form of local access should exist
        assert!(
            has_get_local || has_get_local2 || has_set_local,
            "math loop should have local variable access"
        );
    }
}
