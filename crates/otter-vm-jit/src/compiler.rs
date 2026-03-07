//! Baseline JIT compiler wrapper around Cranelift.

use cranelift_codegen::ir::{AbiParam, UserFuncName, types};
use cranelift_codegen::isa::CallConv;
use cranelift_codegen::settings::Configurable;
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{Linkage, Module, ModuleError, default_libcall_names};
use otter_vm_bytecode::instruction::Instruction;
use otter_vm_bytecode::{Constant, Function};

use crate::runtime_helpers::{HelperFuncIds, HelperRefs, RuntimeHelpers};
use crate::translator;

/// Result of compiling a bytecode function to native code.
#[derive(Debug)]
pub struct JitCompileArtifact {
    /// Entry pointer for compiled native code.
    pub code_ptr: *const u8,
    _owned_code: Option<OwnedJitCode>,
}

// SAFETY: `code_ptr` always points into `_owned_code`'s JITModule allocation.
// Moving the artifact between threads does not invalidate the pointer, and the
// machine code remains owned until the artifact is dropped.
unsafe impl Send for JitCompileArtifact {}

struct OwnedJitCode {
    module: Option<JITModule>,
}

impl std::fmt::Debug for OwnedJitCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("OwnedJitCode(..)")
    }
}

impl OwnedJitCode {
    fn new(module: JITModule) -> Self {
        Self {
            module: Some(module),
        }
    }
}

impl Drop for OwnedJitCode {
    fn drop(&mut self) {
        if let Some(module) = self.module.take() {
            unsafe {
                // SAFETY: compiled entry removal only happens after the runtime
                // stops publishing the function pointer. No code should execute
                // from this module after ownership is dropped.
                module.free_memory();
            }
        }
    }
}

impl JitCompileArtifact {
    #[doc(hidden)]
    pub fn from_raw_ptr(code_ptr: *const u8) -> Self {
        Self {
            code_ptr,
            _owned_code: None,
        }
    }
}

/// Metadata for a single deopt-capable bytecode site.
///
/// # GC Safety (C1 invariant)
///
/// This struct must contain only plain scalar types and index containers.
/// No `GcRef`, `Value`, `JsObject`, `JsString`, or other GC-managed types
/// are permitted. This ensures deopt metadata can be stored, cloned, and
/// transmitted across threads without interacting with GC rooting.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeoptResumeSite {
    /// Bytecode program counter.
    pub bytecode_pc: u32,
    /// Native code offset for the deopt point. Not wired yet.
    pub native_offset: Option<u32>,
    /// Live virtual register indices required for precise resume.
    pub live_registers: Vec<u16>,
    /// Live local indices required for precise resume.
    pub live_locals: Vec<u16>,
}

/// Compile-time deopt metadata scaffold for a function.
///
/// # GC Safety (C1 invariant)
///
/// Contains only scalar metadata. See [`DeoptResumeSite`] for invariant details.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeoptMetadata {
    /// Sorted list of deopt-capable sites.
    pub sites: Vec<DeoptResumeSite>,
}

// Compile-time GC safety assertions: ensure deopt types are Send + Sync
// (impossible if they contained GcRef which is !Send + !Sync).
const _: () = {
    fn assert_send_sync<T: Send + Sync>() {}
    fn check() {
        assert_send_sync::<DeoptResumeSite>();
        assert_send_sync::<DeoptMetadata>();
    }
};

impl DeoptMetadata {
    /// Check whether metadata has a site for the provided bytecode pc.
    pub fn has_site(&self, pc: u32) -> bool {
        self.sites.iter().any(|site| site.bytecode_pc == pc)
    }

    /// Return deopt metadata for a specific bytecode pc.
    pub fn site(&self, pc: u32) -> Option<&DeoptResumeSite> {
        self.sites.iter().find(|site| site.bytecode_pc == pc)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LivenessState {
    registers: Vec<bool>,
    locals: Vec<bool>,
}

impl LivenessState {
    fn new(register_count: usize, local_count: usize) -> Self {
        Self {
            registers: vec![false; register_count],
            locals: vec![false; local_count],
        }
    }

    fn union_with(&mut self, other: &Self) {
        for (dst, src) in self.registers.iter_mut().zip(&other.registers) {
            *dst |= *src;
        }
        for (dst, src) in self.locals.iter_mut().zip(&other.locals) {
            *dst |= *src;
        }
    }

    fn kill_register(&mut self, register: u16) {
        if let Some(slot) = self.registers.get_mut(register as usize) {
            *slot = false;
        }
    }

    fn kill_local(&mut self, local: u16) {
        if let Some(slot) = self.locals.get_mut(local as usize) {
            *slot = false;
        }
    }

    fn mark_register(&mut self, register: u16) {
        if let Some(slot) = self.registers.get_mut(register as usize) {
            *slot = true;
        }
    }

    fn mark_local(&mut self, local: u16) {
        if let Some(slot) = self.locals.get_mut(local as usize) {
            *slot = true;
        }
    }

    fn mark_register_range(&mut self, start: u16, count: u16) {
        for offset in 0..count {
            self.mark_register(start.saturating_add(offset));
        }
    }

    fn mark_all_locals(&mut self) {
        self.locals.fill(true);
    }

    fn to_site(&self, bytecode_pc: u32) -> DeoptResumeSite {
        DeoptResumeSite {
            bytecode_pc,
            native_offset: None,
            live_registers: self
                .registers
                .iter()
                .enumerate()
                .filter_map(|(idx, live)| live.then_some(idx as u16))
                .collect(),
            live_locals: self
                .locals
                .iter()
                .enumerate()
                .filter_map(|(idx, live)| live.then_some(idx as u16))
                .collect(),
        }
    }
}

fn instruction_can_deopt(instruction: &Instruction) -> bool {
    !matches!(
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
            | Instruction::Jump { .. }
            | Instruction::JumpIfTrue { .. }
            | Instruction::JumpIfFalse { .. }
            | Instruction::JumpIfNullish { .. }
            | Instruction::JumpIfNotNullish { .. }
            | Instruction::Return { .. }
            | Instruction::ReturnUndefined
            | Instruction::Nop
    )
}

fn instruction_may_throw(instruction: &Instruction) -> bool {
    !matches!(
        instruction,
        Instruction::LoadUndefined { .. }
            | Instruction::LoadNull { .. }
            | Instruction::LoadTrue { .. }
            | Instruction::LoadFalse { .. }
            | Instruction::LoadInt8 { .. }
            | Instruction::LoadInt32 { .. }
            | Instruction::GetLocal { .. }
            | Instruction::SetLocal { .. }
            | Instruction::GetUpvalue { .. }
            | Instruction::SetUpvalue { .. }
            | Instruction::LoadThis { .. }
            | Instruction::CloseUpvalue { .. }
            | Instruction::Jump { .. }
            | Instruction::JumpIfTrue { .. }
            | Instruction::JumpIfFalse { .. }
            | Instruction::JumpIfNullish { .. }
            | Instruction::JumpIfNotNullish { .. }
            | Instruction::TryStart { .. }
            | Instruction::TryEnd
            | Instruction::Catch { .. }
            | Instruction::Move { .. }
            | Instruction::Nop
            | Instruction::Pop
            | Instruction::Dup { .. }
    )
}

fn instruction_successors(
    pc: usize,
    instruction: &Instruction,
    instruction_count: usize,
    catch_target: Option<usize>,
) -> [Option<usize>; 3] {
    let fallthrough = (pc + 1 < instruction_count).then_some(pc + 1);
    let jump = |offset: i32| {
        let target = pc as i64 + offset as i64;
        (0..instruction_count as i64)
            .contains(&target)
            .then_some(target as usize)
    };

    let mut successors = match instruction {
        Instruction::Jump { offset } => [jump(offset.0), None, None],
        Instruction::JumpIfTrue { offset, .. }
        | Instruction::JumpIfFalse { offset, .. }
        | Instruction::JumpIfNullish { offset, .. }
        | Instruction::JumpIfNotNullish { offset, .. }
        | Instruction::ForInNext { offset, .. } => [jump(offset.0), fallthrough, None],
        Instruction::Return { .. }
        | Instruction::ReturnUndefined
        | Instruction::TailCall { .. }
        | Instruction::Throw { .. } => [None, None, None],
        _ => [fallthrough, None, None],
    };

    if let Some(catch_pc) = catch_target
        && instruction_may_throw(instruction)
        && !successors.contains(&Some(catch_pc))
    {
        for slot in &mut successors {
            if slot.is_none() {
                *slot = Some(catch_pc);
                break;
            }
        }
    }

    successors
}

fn apply_instruction_uses(instruction: &Instruction, state: &mut LivenessState) {
    match instruction {
        Instruction::GetLocal { idx, .. } | Instruction::GetLocalProp { local_idx: idx, .. } => {
            state.mark_local(idx.index());
        }
        Instruction::SetLocal { idx, src } => {
            state.mark_register(src.0);
            state.kill_local(idx.index());
        }
        Instruction::SetUpvalue { src, .. }
        | Instruction::SetGlobal { src, .. }
        | Instruction::RequireCoercible { src }
        | Instruction::Throw { src }
        | Instruction::GetIterator { src, .. }
        | Instruction::GetAsyncIterator { src, .. }
        | Instruction::IteratorClose { iter: src }
        | Instruction::Yield { src, .. }
        | Instruction::Await { src, .. }
        | Instruction::Export { src, .. }
        | Instruction::Move { src, .. }
        | Instruction::Dup { src, .. } => {
            state.mark_register(src.0);
        }
        Instruction::CloseUpvalue { local_idx } => {
            state.mark_local(local_idx.index());
        }
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
        | Instruction::In { lhs, rhs, .. }
        | Instruction::AddInt32 { lhs, rhs, .. }
        | Instruction::SubInt32 { lhs, rhs, .. }
        | Instruction::MulInt32 { lhs, rhs, .. }
        | Instruction::DivInt32 { lhs, rhs, .. }
        | Instruction::AddNumber { lhs, rhs, .. }
        | Instruction::SubNumber { lhs, rhs, .. } => {
            state.mark_register(lhs.0);
            state.mark_register(rhs.0);
        }
        Instruction::Neg { src, .. }
        | Instruction::Inc { src, .. }
        | Instruction::Dec { src, .. }
        | Instruction::BitNot { src, .. }
        | Instruction::Not { src, .. }
        | Instruction::TypeOf { src, .. }
        | Instruction::ToNumber { src, .. }
        | Instruction::ToString { src, .. } => {
            state.mark_register(src.0);
        }
        Instruction::GetProp { obj, key, .. }
        | Instruction::DeleteProp { obj, key, .. }
        | Instruction::GetElem {
            arr: obj, idx: key, ..
        } => {
            state.mark_register(obj.0);
            state.mark_register(key.0);
        }
        Instruction::SetProp { obj, key, val, .. }
        | Instruction::DefineProperty { obj, key, val }
        | Instruction::DefineMethod { obj, key, val }
        | Instruction::SetElem {
            arr: obj,
            idx: key,
            val,
            ..
        } => {
            state.mark_register(obj.0);
            state.mark_register(key.0);
            state.mark_register(val.0);
        }
        Instruction::DefineGetter { obj, key, func }
        | Instruction::DefineSetter { obj, key, func } => {
            state.mark_register(obj.0);
            state.mark_register(key.0);
            state.mark_register(func.0);
        }
        Instruction::GetPropConst { obj, .. } | Instruction::GetPropQuickened { obj, .. } => {
            state.mark_register(obj.0);
        }
        Instruction::SetPropConst { obj, val, .. }
        | Instruction::SetPropQuickened { obj, val, .. } => {
            state.mark_register(obj.0);
            state.mark_register(val.0);
        }
        Instruction::Spread { src, .. } => {
            state.mark_register(src.0);
        }
        Instruction::Call { func, argc, .. }
        | Instruction::TailCall { func, argc }
        | Instruction::Construct { func, argc, .. } => {
            state.mark_register(func.0);
            state.mark_register_range(func.0.saturating_add(1), *argc as u16);
        }
        Instruction::CallSpread {
            func, argc, spread, ..
        }
        | Instruction::ConstructSpread {
            func, argc, spread, ..
        } => {
            state.mark_register(func.0);
            state.mark_register_range(func.0.saturating_add(1), *argc as u16);
            state.mark_register(spread.0);
        }
        Instruction::CallWithReceiver {
            func, this, argc, ..
        } => {
            state.mark_register(func.0);
            state.mark_register(this.0);
            state.mark_register_range(func.0.saturating_add(1), *argc as u16);
        }
        Instruction::CallMethod { obj, argc, .. } => {
            state.mark_register(obj.0);
            state.mark_register_range(obj.0.saturating_add(1), *argc as u16);
        }
        Instruction::CallMethodComputed { obj, key, argc, .. } => {
            state.mark_register(obj.0);
            state.mark_register(key.0);
            state.mark_register_range(obj.0.saturating_add(2), *argc as u16);
        }
        Instruction::CallMethodComputedSpread {
            obj, key, spread, ..
        } => {
            state.mark_register(obj.0);
            state.mark_register(key.0);
            state.mark_register(spread.0);
        }
        Instruction::CallEval { code, .. } => {
            state.mark_register(code.0);
        }
        Instruction::JumpIfTrue { cond, .. } | Instruction::JumpIfFalse { cond, .. } => {
            state.mark_register(cond.0);
        }
        Instruction::JumpIfNullish { src, .. } | Instruction::JumpIfNotNullish { src, .. } => {
            state.mark_register(src.0);
        }
        Instruction::IteratorNext { iter, .. } => {
            state.mark_register(iter.0);
        }
        Instruction::ForInNext { obj, .. } => {
            state.mark_register(obj.0);
        }
        Instruction::DefineClass {
            ctor, super_class, ..
        } => {
            state.mark_register(ctor.0);
            if let Some(super_class) = super_class {
                state.mark_register(super_class.0);
            }
        }
        Instruction::SetHomeObject { func, obj } => {
            state.mark_register(func.0);
            state.mark_register(obj.0);
        }
        Instruction::CallSuper { args, argc, .. } => {
            state.mark_register_range(args.0, *argc as u16);
        }
        Instruction::CallSuperForward { .. } | Instruction::CreateArguments { .. } => {
            state.mark_all_locals();
        }
        Instruction::CallSuperSpread { args, .. } => {
            state.mark_register(args.0);
        }
        Instruction::Return { src } => {
            state.mark_register(src.0);
        }
        Instruction::LoadUndefined { .. }
        | Instruction::LoadNull { .. }
        | Instruction::LoadTrue { .. }
        | Instruction::LoadFalse { .. }
        | Instruction::LoadInt8 { .. }
        | Instruction::LoadInt32 { .. }
        | Instruction::LoadConst { .. }
        | Instruction::GetUpvalue { .. }
        | Instruction::GetGlobal { .. }
        | Instruction::LoadThis { .. }
        | Instruction::DeclareGlobalVar { .. }
        | Instruction::TypeOfName { .. }
        | Instruction::NewObject { .. }
        | Instruction::NewArray { .. }
        | Instruction::Closure { .. }
        | Instruction::ReturnUndefined
        | Instruction::TryStart { .. }
        | Instruction::TryEnd
        | Instruction::Catch { .. }
        | Instruction::Jump { .. }
        | Instruction::GetSuper { .. }
        | Instruction::GetSuperProp { .. }
        | Instruction::AsyncClosure { .. }
        | Instruction::GeneratorClosure { .. }
        | Instruction::AsyncGeneratorClosure { .. }
        | Instruction::Nop
        | Instruction::Debugger
        | Instruction::Pop
        | Instruction::Import { .. } => {}
    }
}

fn apply_instruction_defs(instruction: &Instruction, state: &mut LivenessState) {
    match instruction {
        Instruction::SetLocal { idx, .. } => state.kill_local(idx.index()),
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
        | Instruction::Pow { dst, .. }
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
        | Instruction::Eq { dst, .. }
        | Instruction::StrictEq { dst, .. }
        | Instruction::Ne { dst, .. }
        | Instruction::StrictNe { dst, .. }
        | Instruction::Lt { dst, .. }
        | Instruction::Le { dst, .. }
        | Instruction::Gt { dst, .. }
        | Instruction::Ge { dst, .. }
        | Instruction::Not { dst, .. }
        | Instruction::TypeOf { dst, .. }
        | Instruction::TypeOfName { dst, .. }
        | Instruction::InstanceOf { dst, .. }
        | Instruction::In { dst, .. }
        | Instruction::ToNumber { dst, .. }
        | Instruction::ToString { dst, .. }
        | Instruction::GetProp { dst, .. }
        | Instruction::GetPropConst { dst, .. }
        | Instruction::DeleteProp { dst, .. }
        | Instruction::NewObject { dst }
        | Instruction::NewArray { dst, .. }
        | Instruction::GetElem { dst, .. }
        | Instruction::Spread { dst, .. }
        | Instruction::Closure { dst, .. }
        | Instruction::Call { dst, .. }
        | Instruction::CallMethod { dst, .. }
        | Instruction::CreateArguments { dst }
        | Instruction::CallEval { dst, .. }
        | Instruction::CallWithReceiver { dst, .. }
        | Instruction::CallMethodComputed { dst, .. }
        | Instruction::Construct { dst, .. }
        | Instruction::CallSpread { dst, .. }
        | Instruction::ConstructSpread { dst, .. }
        | Instruction::CallMethodComputedSpread { dst, .. }
        | Instruction::Catch { dst }
        | Instruction::GetIterator { dst, .. }
        | Instruction::GetAsyncIterator { dst, .. }
        | Instruction::IteratorNext { dst, .. }
        | Instruction::ForInNext { dst, .. }
        | Instruction::DefineClass { dst, .. }
        | Instruction::GetSuper { dst }
        | Instruction::GetSuperProp { dst, .. }
        | Instruction::CallSuper { dst, .. }
        | Instruction::CallSuperForward { dst }
        | Instruction::CallSuperSpread { dst, .. }
        | Instruction::Yield { dst, .. }
        | Instruction::Await { dst, .. }
        | Instruction::AsyncClosure { dst, .. }
        | Instruction::GeneratorClosure { dst, .. }
        | Instruction::AsyncGeneratorClosure { dst, .. }
        | Instruction::Move { dst, .. }
        | Instruction::Dup { dst, .. }
        | Instruction::Import { dst, .. }
        | Instruction::AddInt32 { dst, .. }
        | Instruction::SubInt32 { dst, .. }
        | Instruction::MulInt32 { dst, .. }
        | Instruction::DivInt32 { dst, .. }
        | Instruction::AddNumber { dst, .. }
        | Instruction::SubNumber { dst, .. }
        | Instruction::GetPropQuickened { dst, .. }
        | Instruction::GetLocalProp { dst, .. } => state.kill_register(dst.0),
        Instruction::SetUpvalue { .. }
        | Instruction::SetGlobal { .. }
        | Instruction::CloseUpvalue { .. }
        | Instruction::DeclareGlobalVar { .. }
        | Instruction::RequireCoercible { .. }
        | Instruction::SetProp { .. }
        | Instruction::SetPropConst { .. }
        | Instruction::DefineProperty { .. }
        | Instruction::DefineGetter { .. }
        | Instruction::DefineSetter { .. }
        | Instruction::DefineMethod { .. }
        | Instruction::SetElem { .. }
        | Instruction::TailCall { .. }
        | Instruction::Return { .. }
        | Instruction::ReturnUndefined
        | Instruction::Jump { .. }
        | Instruction::JumpIfTrue { .. }
        | Instruction::JumpIfFalse { .. }
        | Instruction::JumpIfNullish { .. }
        | Instruction::JumpIfNotNullish { .. }
        | Instruction::TryStart { .. }
        | Instruction::TryEnd
        | Instruction::Throw { .. }
        | Instruction::IteratorClose { .. }
        | Instruction::SetHomeObject { .. }
        | Instruction::Nop
        | Instruction::Debugger
        | Instruction::Pop
        | Instruction::Export { .. }
        | Instruction::SetPropQuickened { .. } => {}
    }
}

pub(crate) fn build_deopt_metadata(function: &Function) -> DeoptMetadata {
    let instructions = function.instructions.read();
    let instruction_count = instructions.len();
    if instruction_count == 0 {
        return DeoptMetadata::default();
    }

    let mut active_catch_targets = vec![None; instruction_count];
    let mut try_stack = Vec::new();
    for (pc, instruction) in instructions.iter().enumerate() {
        active_catch_targets[pc] = try_stack.last().copied();
        match instruction {
            Instruction::TryStart { catch_offset } => {
                let target = pc as i64 + catch_offset.0 as i64;
                if (0..instruction_count as i64).contains(&target) {
                    try_stack.push(target as usize);
                }
            }
            Instruction::TryEnd => {
                let _ = try_stack.pop();
            }
            _ => {}
        }
    }

    let register_count = function.register_count as usize;
    let local_count = function.local_count as usize;
    let mut live_in = vec![LivenessState::new(register_count, local_count); instruction_count];
    let mut changed = true;

    while changed {
        changed = false;
        for pc in (0..instruction_count).rev() {
            let instruction = &instructions[pc];
            let mut live_out = LivenessState::new(register_count, local_count);
            for succ in
                instruction_successors(pc, instruction, instruction_count, active_catch_targets[pc])
                    .into_iter()
                    .flatten()
            {
                live_out.union_with(&live_in[succ]);
            }

            let mut next_live_in = live_out;
            apply_instruction_defs(instruction, &mut next_live_in);
            apply_instruction_uses(instruction, &mut next_live_in);

            if live_in[pc] != next_live_in {
                live_in[pc] = next_live_in;
                changed = true;
            }
        }
    }

    let sites = instructions
        .iter()
        .enumerate()
        .filter(|(_, instruction)| instruction_can_deopt(instruction))
        .map(|(pc, _)| live_in[pc].to_site(pc as u32))
        .collect();
    DeoptMetadata { sites }
}

/// Errors produced by the baseline JIT compiler.
#[derive(Debug, thiserror::Error)]
pub enum JitError {
    /// Cranelift module-level error.
    #[error("cranelift module error: {0}")]
    Module(Box<ModuleError>),

    /// Failed to create the JIT builder.
    #[error("jit builder initialization failed: {0}")]
    Builder(String),

    /// Bytecode instruction is not supported by the baseline translator yet.
    #[error("unsupported instruction at pc {pc}: {opcode}")]
    UnsupportedInstruction { pc: usize, opcode: String },

    /// Jump target is outside the bytecode function bounds.
    #[error("invalid jump target from pc {pc} with offset {offset} (len={instruction_count})")]
    InvalidJumpTarget {
        pc: usize,
        offset: i32,
        instruction_count: usize,
    },
}

impl From<ModuleError> for JitError {
    fn from(value: ModuleError) -> Self {
        Self::Module(Box::new(value))
    }
}

/// Minimal Cranelift-backed JIT compiler.
pub struct JitCompiler {
    function_builder_ctx: FunctionBuilderContext,
    context: cranelift_codegen::Context,
    next_function_id: u64,
    runtime_helpers: Option<RuntimeHelpers>,
    /// Host calling convention (SystemV on Linux, AppleAarch64 on macOS ARM64, etc.)
    host_call_conv: CallConv,
}

/// Build a `JITBuilder` with host-native ISA and `opt_level=speed`.
fn make_optimized_jit_builder() -> Result<(JITBuilder, CallConv), JitError> {
    let mut flag_builder = cranelift_codegen::settings::builder();
    // Enable optimizations: DCE, instruction combining, register allocation.
    // Note: egraph-based rewrites are enabled by default in Cranelift 0.129
    // when opt_level >= speed.
    flag_builder
        .set("opt_level", "speed")
        .expect("valid cranelift setting");
    let flags = cranelift_codegen::settings::Flags::new(flag_builder);
    let isa = cranelift_native::builder()
        .map_err(|e| JitError::Builder(format!("cranelift-native ISA: {e}")))?
        .finish(flags)
        .map_err(|e| JitError::Builder(format!("cranelift ISA finish: {e}")))?;
    let call_conv = isa.default_call_conv();
    let builder = JITBuilder::with_isa(isa, default_libcall_names());
    Ok((builder, call_conv))
}

impl JitCompiler {
    /// Create a new baseline JIT compiler instance (no runtime helpers).
    pub fn new() -> Result<Self, JitError> {
        let (_builder, call_conv) = make_optimized_jit_builder()?;
        Ok(Self {
            function_builder_ctx: FunctionBuilderContext::new(),
            context: cranelift_codegen::Context::new(),
            next_function_id: 0,
            runtime_helpers: None,
            host_call_conv: call_conv,
        })
    }

    /// Create a JIT compiler with runtime helper support.
    ///
    /// Runtime helpers enable compilation of property access, function calls,
    /// and other complex operations that require VM context.
    pub fn new_with_helpers(helpers: RuntimeHelpers) -> Result<Self, JitError> {
        let (_builder, call_conv) = make_optimized_jit_builder()?;
        Ok(Self {
            function_builder_ctx: FunctionBuilderContext::new(),
            context: cranelift_codegen::Context::new(),
            next_function_id: 0,
            runtime_helpers: Some(helpers),
            host_call_conv: call_conv,
        })
    }

    /// Host-native calling convention for this compiler.
    pub fn host_call_conv(&self) -> CallConv {
        self.host_call_conv
    }

    /// Whether runtime helpers are available for compilation.
    pub fn has_helpers(&self) -> bool {
        self.runtime_helpers.is_some()
    }

    fn make_module(&self) -> Result<(JITModule, Option<HelperFuncIds>), JitError> {
        let (mut builder, _) = make_optimized_jit_builder()?;
        if let Some(helpers) = &self.runtime_helpers {
            helpers.register_symbols(&mut builder);
        }
        let mut module = JITModule::new(builder);
        let helper_func_ids = self
            .runtime_helpers
            .as_ref()
            .map(|helpers| {
                HelperFuncIds::declare_with_call_conv(helpers, &mut module, self.host_call_conv)
            })
            .transpose()?;
        Ok((module, helper_func_ids))
    }

    /// Compile a bytecode function into native code.
    ///
    /// Baseline translation currently supports a subset of bytecode instructions.
    /// Unsupported instructions return `JitError::UnsupportedInstruction`.
    pub fn compile_function(
        &mut self,
        function: &Function,
    ) -> Result<JitCompileArtifact, JitError> {
        self.compile_function_with_constants(function, &[])
    }

    /// Compile a bytecode function with constant pool access.
    pub fn compile_function_with_constants(
        &mut self,
        function: &Function,
        constants: &[Constant],
    ) -> Result<JitCompileArtifact, JitError> {
        let (artifact, _) =
            self.compile_function_with_constants_and_metadata(function, constants)?;
        Ok(artifact)
    }

    /// Compile a bytecode function and return JIT artifact plus deopt metadata scaffold.
    pub fn compile_function_with_constants_and_metadata(
        &mut self,
        function: &Function,
        constants: &[Constant],
    ) -> Result<(JitCompileArtifact, DeoptMetadata), JitError> {
        self.compile_function_with_inlining(function, constants, &[])
    }

    /// Compile a bytecode function with inlining candidates from the same module.
    ///
    /// `module_functions` contains `(function_index, Function)` pairs for small
    /// functions eligible for inlining at call sites.
    pub fn compile_function_with_inlining(
        &mut self,
        function: &Function,
        constants: &[Constant],
        module_functions: &[(u32, Function)],
    ) -> Result<(JitCompileArtifact, DeoptMetadata), JitError> {
        let (mut module, helper_func_ids) = self.make_module()?;
        let mut signature = module.make_signature();
        // Signature: (ctx: I64, args_ptr: I64, argc: I32) -> I64
        signature.params.push(AbiParam::new(types::I64)); // ctx pointer
        signature.params.push(AbiParam::new(types::I64)); // args_ptr
        signature.params.push(AbiParam::new(types::I32)); // argc
        signature.returns.push(AbiParam::new(types::I64));

        let name = format!(
            "otter_jit_{}_{}",
            function.display_name().replace(['<', '>'], "_"),
            self.next_function_id
        );
        self.next_function_id = self.next_function_id.saturating_add(1);

        let func_id = module.declare_function(&name, Linkage::Local, &signature)?;

        self.context.func = cranelift_codegen::ir::Function::with_name_signature(
            UserFuncName::user(0, func_id.as_u32()),
            signature,
        );

        // Declare helper function refs for this compilation if available
        let helper_refs = helper_func_ids
            .as_ref()
            .map(|func_ids| HelperRefs::declare(func_ids, &mut module, &mut self.context.func));

        {
            let mut builder =
                FunctionBuilder::new(&mut self.context.func, &mut self.function_builder_ctx);
            translator::translate_function_with_constants(
                &mut builder,
                function,
                constants,
                helper_refs.as_ref(),
                module_functions,
            )?;
            builder.finalize();
        }

        module.define_function(func_id, &mut self.context)?;
        module.clear_context(&mut self.context);
        module.finalize_definitions()?;

        let code_ptr = module.get_finalized_function(func_id);
        let metadata = build_deopt_metadata(function);
        Ok((
            JitCompileArtifact {
                code_ptr,
                _owned_code: Some(OwnedJitCode::new(module)),
            },
            metadata,
        ))
    }

    /// Execute a compiled artifact as `extern "C" fn(*mut u8, *const i64, u32) -> i64`.
    ///
    /// This is intended for translator unit tests. Passes null ctx pointer.
    pub fn execute_compiled_i64(&self, artifact: JitCompileArtifact) -> i64 {
        self.execute_compiled_i64_with_args(artifact, &[])
    }

    /// Execute a compiled artifact with pre-boxed NaN-value arguments.
    ///
    /// Passes null ctx pointer (sufficient for arithmetic-only functions).
    pub fn execute_compiled_i64_with_args(
        &self,
        artifact: JitCompileArtifact,
        args: &[i64],
    ) -> i64 {
        let func: extern "C" fn(*mut u8, *const i64, u32) -> i64 = unsafe {
            // SAFETY: Artifacts are produced by this compiler with signature
            // `(*mut u8, *const i64, u32) -> i64`.
            std::mem::transmute(artifact.code_ptr)
        };
        func(std::ptr::null_mut(), args.as_ptr(), args.len() as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use otter_vm_bytecode::{
        Constant, ConstantIndex, Instruction, JumpOffset, LocalIndex, Register,
    };

    fn boxed_i32(n: i32) -> i64 {
        crate::type_guards::TAG_INT32 | ((n as u32) as i64)
    }

    fn boxed_bool(value: bool) -> i64 {
        if value {
            crate::type_guards::TAG_TRUE
        } else {
            crate::type_guards::TAG_FALSE
        }
    }

    fn site(metadata: &DeoptMetadata, pc: u32) -> &DeoptResumeSite {
        metadata
            .sites
            .iter()
            .find(|site| site.bytecode_pc == pc)
            .unwrap_or_else(|| panic!("missing deopt site for pc {pc}"))
    }

    #[test]
    fn basic_compile() {
        let function = Function::builder()
            .name("basic_compile")
            .register_count(1)
            .instruction(Instruction::LoadInt32 {
                dst: Register(0),
                value: 7,
            })
            .instruction(Instruction::Return { src: Register(0) })
            .build();

        let mut jit = JitCompiler::new().expect("jit initialization should succeed");
        let artifact = jit
            .compile_function(&function)
            .expect("function compilation should succeed");

        assert!(!artifact.code_ptr.is_null());
    }

    #[test]
    fn deopt_metadata_marks_only_deopt_capable_sites() {
        let function = Function::builder()
            .name("deopt_metadata_sites")
            .register_count(3)
            .instruction(Instruction::LoadInt32 {
                dst: Register(0),
                value: 1,
            }) // pc 0 (no deopt)
            .instruction(Instruction::LoadInt32 {
                dst: Register(1),
                value: 2,
            }) // pc 1 (no deopt)
            .instruction(Instruction::Add {
                dst: Register(2),
                lhs: Register(0),
                rhs: Register(1),
                feedback_index: 0,
            }) // pc 2 (can deopt)
            .instruction(Instruction::Return { src: Register(2) }) // pc 3 (no deopt)
            .feedback_vector_size(1)
            .build();

        let metadata = build_deopt_metadata(&function);
        assert_eq!(metadata.sites.len(), 1);
        assert_eq!(metadata.sites[0].bytecode_pc, 2);
        assert_eq!(metadata.sites[0].live_registers, vec![0, 1]);
        assert!(metadata.sites[0].live_locals.is_empty());
        assert!(metadata.has_site(2));
        assert!(!metadata.has_site(0));
    }

    #[test]
    fn deopt_metadata_marks_contiguous_call_arguments_live() {
        let function = Function::builder()
            .name("deopt_call_args")
            .local_count(4)
            .register_count(5)
            .instruction(Instruction::GetLocal {
                dst: Register(1),
                idx: LocalIndex(1),
            })
            .instruction(Instruction::GetLocal {
                dst: Register(2),
                idx: LocalIndex(2),
            })
            .instruction(Instruction::GetLocal {
                dst: Register(3),
                idx: LocalIndex(3),
            })
            .instruction(Instruction::Call {
                dst: Register(4),
                func: Register(1),
                argc: 2,
                ic_index: 0,
            })
            .instruction(Instruction::Return { src: Register(4) })
            .build();

        let metadata = build_deopt_metadata(&function);
        let call_site = site(&metadata, 3);
        assert_eq!(call_site.live_registers, vec![1, 2, 3]);
        assert!(call_site.live_locals.is_empty());
    }

    #[test]
    fn deopt_metadata_keeps_catch_only_locals_live() {
        let function = Function::builder()
            .name("deopt_catch_locals")
            .local_count(1)
            .register_count(4)
            .instruction(Instruction::LoadInt32 {
                dst: Register(0),
                value: 7,
            }) // pc 0
            .instruction(Instruction::SetLocal {
                idx: LocalIndex(0),
                src: Register(0),
            }) // pc 1
            .instruction(Instruction::TryStart {
                catch_offset: JumpOffset(3),
            }) // pc 2 -> catch at pc 5
            .instruction(Instruction::GetGlobal {
                dst: Register(1),
                name: ConstantIndex(0),
                ic_index: 0,
            }) // pc 3
            .instruction(Instruction::TryEnd) // pc 4
            .instruction(Instruction::Catch { dst: Register(2) }) // pc 5
            .instruction(Instruction::GetLocal {
                dst: Register(3),
                idx: LocalIndex(0),
            }) // pc 6
            .instruction(Instruction::Return { src: Register(3) }) // pc 7
            .feedback_vector_size(1)
            .build();

        let metadata = build_deopt_metadata(&function);
        let get_global_site = site(&metadata, 3);
        assert!(get_global_site.live_registers.is_empty());
        assert_eq!(get_global_site.live_locals, vec![0]);
    }

    #[test]
    fn compile_with_metadata_returns_scaffold_sites() {
        let function = Function::builder()
            .name("compile_with_metadata")
            .register_count(1)
            .instruction(Instruction::LoadInt32 {
                dst: Register(0),
                value: 7,
            })
            .instruction(Instruction::Return { src: Register(0) })
            .build();

        let mut jit = JitCompiler::new().expect("jit initialization should succeed");
        let (artifact, metadata) = jit
            .compile_function_with_constants_and_metadata(&function, &[])
            .expect("function compilation with metadata should succeed");

        assert!(!artifact.code_ptr.is_null());
        assert!(metadata.sites.is_empty());
    }

    #[test]
    fn translation_arithmetic_returns_value() {
        let function = Function::builder()
            .name("translation_arithmetic")
            .register_count(4)
            .instruction(Instruction::LoadInt32 {
                dst: Register(0),
                value: 2,
            })
            .instruction(Instruction::LoadInt32 {
                dst: Register(1),
                value: 3,
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

        let mut jit = JitCompiler::new().expect("jit initialization should succeed");
        let artifact = jit
            .compile_function(&function)
            .expect("translation should succeed");

        assert_eq!(jit.execute_compiled_i64(artifact), boxed_i32(5));
    }

    #[test]
    fn translation_control_flow_jump_if_true() {
        let function = Function::builder()
            .name("translation_control_flow")
            .register_count(3)
            .instruction(Instruction::LoadInt32 {
                dst: Register(0),
                value: 1,
            }) // pc 0
            .instruction(Instruction::JumpIfTrue {
                cond: Register(0),
                offset: JumpOffset(3),
            }) // pc 1 -> pc 4
            .instruction(Instruction::LoadInt32 {
                dst: Register(1),
                value: 7,
            }) // pc 2
            .instruction(Instruction::Return { src: Register(1) }) // pc 3
            .instruction(Instruction::LoadInt32 {
                dst: Register(2),
                value: 42,
            }) // pc 4
            .instruction(Instruction::Return { src: Register(2) }) // pc 5
            .build();

        let mut jit = JitCompiler::new().expect("jit initialization should succeed");
        let artifact = jit
            .compile_function(&function)
            .expect("translation should succeed");

        assert_eq!(jit.execute_compiled_i64(artifact), boxed_i32(42));
    }

    #[test]
    fn translation_locals_get_set_roundtrip() {
        let function = Function::builder()
            .name("translation_locals")
            .register_count(2)
            .local_count(1)
            .instruction(Instruction::LoadInt32 {
                dst: Register(0),
                value: 11,
            })
            .instruction(Instruction::SetLocal {
                idx: LocalIndex(0),
                src: Register(0),
            })
            .instruction(Instruction::GetLocal {
                dst: Register(1),
                idx: LocalIndex(0),
            })
            .instruction(Instruction::Return { src: Register(1) })
            .build();

        let mut jit = JitCompiler::new().expect("jit initialization should succeed");
        let artifact = jit
            .compile_function(&function)
            .expect("translation should succeed");

        assert_eq!(jit.execute_compiled_i64(artifact), boxed_i32(11));
    }

    #[test]
    fn translation_reads_param_from_argv() {
        let function = Function::builder()
            .name("translation_param_from_argv")
            .param_count(1)
            .local_count(1)
            .register_count(3)
            .instruction(Instruction::GetLocal {
                dst: Register(0),
                idx: LocalIndex(0),
            })
            .instruction(Instruction::LoadInt32 {
                dst: Register(1),
                value: 1,
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

        let mut jit = JitCompiler::new().expect("jit initialization should succeed");
        let artifact = jit
            .compile_function(&function)
            .expect("translation should succeed");

        let argv = [boxed_i32(41)];
        assert_eq!(
            jit.execute_compiled_i64_with_args(artifact, &argv),
            boxed_i32(42)
        );
    }

    #[test]
    fn translation_control_flow_jump_if_nullish() {
        let function = Function::builder()
            .name("translation_jump_if_nullish")
            .register_count(3)
            .instruction(Instruction::LoadNull { dst: Register(0) }) // pc 0
            .instruction(Instruction::JumpIfNullish {
                src: Register(0),
                offset: JumpOffset(3),
            }) // pc 1 -> pc 4
            .instruction(Instruction::LoadInt32 {
                dst: Register(1),
                value: 7,
            }) // pc 2
            .instruction(Instruction::Return { src: Register(1) }) // pc 3
            .instruction(Instruction::LoadInt32 {
                dst: Register(2),
                value: 42,
            }) // pc 4
            .instruction(Instruction::Return { src: Register(2) }) // pc 5
            .build();

        let mut jit = JitCompiler::new().expect("jit initialization should succeed");
        let artifact = jit
            .compile_function(&function)
            .expect("translation should succeed");

        assert_eq!(jit.execute_compiled_i64(artifact), boxed_i32(42));
    }

    #[test]
    fn translation_load_const_number() {
        let function = Function::builder()
            .name("translation_load_const_number")
            .register_count(1)
            .instruction(Instruction::LoadConst {
                dst: Register(0),
                idx: ConstantIndex(0),
            })
            .instruction(Instruction::Return { src: Register(0) })
            .build();

        let constants = [Constant::number(123.5)];
        let mut jit = JitCompiler::new().expect("jit initialization should succeed");
        let artifact = jit
            .compile_function_with_constants(&function, &constants)
            .expect("translation should succeed");

        assert_eq!(
            jit.execute_compiled_i64(artifact),
            123.5_f64.to_bits() as i64
        );
    }

    #[test]
    fn translation_add_f64_fast_path() {
        let function = Function::builder()
            .name("translation_add_f64_fast_path")
            .register_count(3)
            .instruction(Instruction::LoadConst {
                dst: Register(0),
                idx: ConstantIndex(0),
            })
            .instruction(Instruction::LoadConst {
                dst: Register(1),
                idx: ConstantIndex(1),
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
        // Set feedback: f64 operands observed
        function.feedback_vector.write()[0]
            .type_observations
            .observe_number();

        let constants = [Constant::number(1.5), Constant::number(2.25)];
        let mut jit = JitCompiler::new().expect("jit initialization should succeed");
        let artifact = jit
            .compile_function_with_constants(&function, &constants)
            .expect("translation should succeed");

        assert_eq!(
            jit.execute_compiled_i64(artifact),
            3.75_f64.to_bits() as i64
        );
    }

    #[test]
    fn translation_add_mixed_numeric_fast_path() {
        let function = Function::builder()
            .name("translation_add_mixed_numeric_fast_path")
            .register_count(3)
            .instruction(Instruction::LoadInt32 {
                dst: Register(0),
                value: 1,
            })
            .instruction(Instruction::LoadConst {
                dst: Register(1),
                idx: ConstantIndex(0),
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
        // Set feedback: mixed int32 + number observed
        function.feedback_vector.write()[0]
            .type_observations
            .observe_int32();
        function.feedback_vector.write()[0]
            .type_observations
            .observe_number();

        let constants = [Constant::number(2.5)];
        let mut jit = JitCompiler::new().expect("jit initialization should succeed");
        let artifact = jit
            .compile_function_with_constants(&function, &constants)
            .expect("translation should succeed");

        assert_eq!(jit.execute_compiled_i64(artifact), 3.5_f64.to_bits() as i64);
    }

    #[test]
    fn translation_div_int32_non_exact_uses_numeric_fast_path() {
        let function = Function::builder()
            .name("translation_div_int32_non_exact_uses_numeric_fast_path")
            .register_count(3)
            .instruction(Instruction::LoadInt32 {
                dst: Register(0),
                value: 7,
            })
            .instruction(Instruction::LoadInt32 {
                dst: Register(1),
                value: 2,
            })
            .instruction(Instruction::Div {
                dst: Register(2),
                lhs: Register(0),
                rhs: Register(1),
                feedback_index: 0,
            })
            .instruction(Instruction::Return { src: Register(2) })
            .feedback_vector_size(1)
            .build();

        let mut jit = JitCompiler::new().expect("jit initialization should succeed");
        let artifact = jit
            .compile_function(&function)
            .expect("translation should succeed");

        assert_eq!(jit.execute_compiled_i64(artifact), 3.5_f64.to_bits() as i64);
    }

    #[test]
    fn translation_add_int32_overflow_uses_numeric_fast_path() {
        let function = Function::builder()
            .name("translation_add_int32_overflow_uses_numeric_fast_path")
            .register_count(3)
            .instruction(Instruction::LoadInt32 {
                dst: Register(0),
                value: i32::MAX,
            })
            .instruction(Instruction::LoadInt32 {
                dst: Register(1),
                value: 1,
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
        // Set feedback: both int32 and number observed (overflow produces f64)
        function.feedback_vector.write()[0]
            .type_observations
            .observe_int32();
        function.feedback_vector.write()[0]
            .type_observations
            .observe_number();

        let mut jit = JitCompiler::new().expect("jit initialization should succeed");
        let artifact = jit
            .compile_function(&function)
            .expect("translation should succeed");

        assert_eq!(
            jit.execute_compiled_i64(artifact),
            2147483648.0_f64.to_bits() as i64
        );
    }

    #[test]
    fn translation_div_by_zero_uses_numeric_fast_path() {
        let function = Function::builder()
            .name("translation_div_by_zero_uses_numeric_fast_path")
            .register_count(3)
            .instruction(Instruction::LoadInt32 {
                dst: Register(0),
                value: 1,
            })
            .instruction(Instruction::LoadInt32 {
                dst: Register(1),
                value: 0,
            })
            .instruction(Instruction::Div {
                dst: Register(2),
                lhs: Register(0),
                rhs: Register(1),
                feedback_index: 0,
            })
            .instruction(Instruction::Return { src: Register(2) })
            .feedback_vector_size(1)
            .build();

        let mut jit = JitCompiler::new().expect("jit initialization should succeed");
        let artifact = jit
            .compile_function(&function)
            .expect("translation should succeed");

        assert_eq!(
            jit.execute_compiled_i64(artifact),
            f64::INFINITY.to_bits() as i64
        );
    }

    #[test]
    fn translation_lt_f64_fast_path() {
        let function = Function::builder()
            .name("translation_lt_f64_fast_path")
            .register_count(3)
            .instruction(Instruction::LoadConst {
                dst: Register(0),
                idx: ConstantIndex(0),
            })
            .instruction(Instruction::LoadConst {
                dst: Register(1),
                idx: ConstantIndex(1),
            })
            .instruction(Instruction::Lt {
                dst: Register(2),
                lhs: Register(0),
                rhs: Register(1),
            })
            .instruction(Instruction::Return { src: Register(2) })
            .build();

        let constants = [Constant::number(1.5), Constant::number(2.25)];
        let mut jit = JitCompiler::new().expect("jit initialization should succeed");
        let artifact = jit
            .compile_function_with_constants(&function, &constants)
            .expect("translation should succeed");

        assert_eq!(jit.execute_compiled_i64(artifact), boxed_bool(true));
    }

    #[test]
    fn translation_lt_mixed_numeric_fast_path() {
        let function = Function::builder()
            .name("translation_lt_mixed_numeric_fast_path")
            .register_count(3)
            .instruction(Instruction::LoadInt32 {
                dst: Register(0),
                value: 2,
            })
            .instruction(Instruction::LoadConst {
                dst: Register(1),
                idx: ConstantIndex(0),
            })
            .instruction(Instruction::Lt {
                dst: Register(2),
                lhs: Register(0),
                rhs: Register(1),
            })
            .instruction(Instruction::Return { src: Register(2) })
            .build();

        let constants = [Constant::number(2.5)];
        let mut jit = JitCompiler::new().expect("jit initialization should succeed");
        let artifact = jit
            .compile_function_with_constants(&function, &constants)
            .expect("translation should succeed");

        assert_eq!(jit.execute_compiled_i64(artifact), boxed_bool(true));
    }

    #[test]
    fn translation_bitor_numeric_to_int32_fast_path() {
        let function = Function::builder()
            .name("translation_bitor_numeric_to_int32_fast_path")
            .register_count(3)
            .instruction(Instruction::LoadConst {
                dst: Register(0),
                idx: ConstantIndex(0),
            })
            .instruction(Instruction::LoadInt32 {
                dst: Register(1),
                value: 0,
            })
            .instruction(Instruction::BitOr {
                dst: Register(2),
                lhs: Register(0),
                rhs: Register(1),
            })
            .instruction(Instruction::Return { src: Register(2) })
            .build();

        let constants = [Constant::number(4_294_967_297.0)];
        let mut jit = JitCompiler::new().expect("jit initialization should succeed");
        let artifact = jit
            .compile_function_with_constants(&function, &constants)
            .expect("translation should succeed");

        assert_eq!(jit.execute_compiled_i64(artifact), boxed_i32(1));
    }

    #[test]
    fn translation_bitnot_numeric_to_int32_fast_path() {
        let function = Function::builder()
            .name("translation_bitnot_numeric_to_int32_fast_path")
            .register_count(2)
            .instruction(Instruction::LoadConst {
                dst: Register(0),
                idx: ConstantIndex(0),
            })
            .instruction(Instruction::BitNot {
                dst: Register(1),
                src: Register(0),
            })
            .instruction(Instruction::Return { src: Register(1) })
            .build();

        let constants = [Constant::number(1.9)];
        let mut jit = JitCompiler::new().expect("jit initialization should succeed");
        let artifact = jit
            .compile_function_with_constants(&function, &constants)
            .expect("translation should succeed");

        assert_eq!(jit.execute_compiled_i64(artifact), boxed_i32(-2));
    }

    #[test]
    fn translation_ushr_high_bit_returns_number() {
        let function = Function::builder()
            .name("translation_ushr_high_bit_returns_number")
            .register_count(3)
            .instruction(Instruction::LoadConst {
                dst: Register(0),
                idx: ConstantIndex(0),
            })
            .instruction(Instruction::LoadInt32 {
                dst: Register(1),
                value: 0,
            })
            .instruction(Instruction::Ushr {
                dst: Register(2),
                lhs: Register(0),
                rhs: Register(1),
            })
            .instruction(Instruction::Return { src: Register(2) })
            .build();

        let constants = [Constant::number(-1.0)];
        let mut jit = JitCompiler::new().expect("jit initialization should succeed");
        let artifact = jit
            .compile_function_with_constants(&function, &constants)
            .expect("translation should succeed");

        assert_eq!(
            jit.execute_compiled_i64(artifact),
            4_294_967_295.0_f64.to_bits() as i64
        );
    }

    #[test]
    fn translation_strict_eq_mixed_numeric_zero() {
        let function = Function::builder()
            .name("translation_strict_eq_mixed_numeric_zero")
            .register_count(3)
            .instruction(Instruction::LoadInt32 {
                dst: Register(0),
                value: 0,
            })
            .instruction(Instruction::LoadConst {
                dst: Register(1),
                idx: ConstantIndex(0),
            })
            .instruction(Instruction::StrictEq {
                dst: Register(2),
                lhs: Register(0),
                rhs: Register(1),
            })
            .instruction(Instruction::Return { src: Register(2) })
            .build();

        let constants = [Constant::number(-0.0)];
        let mut jit = JitCompiler::new().expect("jit initialization should succeed");
        let artifact = jit
            .compile_function_with_constants(&function, &constants)
            .expect("translation should succeed");

        assert_eq!(jit.execute_compiled_i64(artifact), boxed_bool(true));
    }

    #[test]
    fn translation_reports_unsupported_instruction() {
        let function = Function::builder()
            .name("translation_unsupported")
            .register_count(1)
            .instruction(Instruction::LoadConst {
                dst: Register(0),
                idx: ConstantIndex(0),
            })
            .instruction(Instruction::Return { src: Register(0) })
            .build();

        let mut jit = JitCompiler::new().expect("jit initialization should succeed");
        let err = jit
            .compile_function(&function)
            .expect_err("unsupported bytecode should fail translation");

        match err {
            JitError::UnsupportedInstruction { pc, opcode } => {
                assert_eq!(pc, 0);
                assert_eq!(opcode, "LoadConst");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn deopt_state_dump_captures_locals_and_registers_on_bailout() {
        // This function adds a boolean and an int32. The Add fast path in JIT
        // handles numeric values (int32/f64), but boolean is non-numeric and
        // must deopt. This validates that deopt state dumping captures locals
        // and registers correctly at the bailout site.
        //
        // Layout before Add: local[0] exists in the frame, reg[0] holds the
        // parameter copy, reg[1] holds the int32 constant. Bailout happens at
        // pc 2 (Add) when type guard fails.

        let function = Function::builder()
            .name("deopt_state_dump_test")
            .param_count(1)
            .local_count(1)
            .register_count(3)
            .instruction(Instruction::GetLocal {
                dst: Register(0),
                idx: LocalIndex(0),
            }) // pc 0
            .instruction(Instruction::LoadInt32 {
                dst: Register(1),
                value: 10,
            }) // pc 1
            .instruction(Instruction::Add {
                dst: Register(2),
                lhs: Register(0),
                rhs: Register(1),
                feedback_index: 0,
            }) // pc 2 - may bailout
            .instruction(Instruction::Return { src: Register(2) }) // pc 3
            .feedback_vector_size(1)
            .build();
        // Only int32 observed → JIT generates int32-only fast path
        function.feedback_vector.write()[0]
            .type_observations
            .observe_int32();

        let mut jit = JitCompiler::new().expect("jit init");
        let artifact = jit
            .compile_function(&function)
            .expect("compilation should succeed");

        // Set up a fake JitContext with deopt buffers
        let mut deopt_locals = vec![0_i64; 1]; // 1 local
        let mut deopt_regs = vec![0_i64; 3]; // 3 registers

        // Build a minimal context-like buffer matching JitContext layout.
        // The context is 136 bytes (through deopt_regs_count).
        let mut ctx_buf = vec![0_u8; 136];
        // Write deopt_locals_ptr at offset 104
        let locals_ptr = deopt_locals.as_mut_ptr() as u64;
        ctx_buf[104..112].copy_from_slice(&locals_ptr.to_ne_bytes());
        // Write deopt_locals_count at offset 112
        ctx_buf[112..116].copy_from_slice(&1_u32.to_ne_bytes());
        // Write deopt_regs_ptr at offset 120
        let regs_ptr = deopt_regs.as_mut_ptr() as u64;
        ctx_buf[120..128].copy_from_slice(&regs_ptr.to_ne_bytes());
        // Write deopt_regs_count at offset 128
        ctx_buf[128..132].copy_from_slice(&3_u32.to_ne_bytes());
        // Write bailout_pc = -1 at offset 96 (so we can detect if it was written)
        ctx_buf[96..104].copy_from_slice(&(-1_i64).to_ne_bytes());

        // Call with a boolean value, which fails numeric Add guard → bailout.
        let argv = [boxed_bool(true)];

        let func: extern "C" fn(*mut u8, *const i64, u32) -> i64 =
            unsafe { std::mem::transmute(artifact.code_ptr) };
        let result = func(ctx_buf.as_mut_ptr(), argv.as_ptr(), 1);

        // Should have bailed out
        assert_eq!(result, crate::bailout::BAILOUT_SENTINEL);

        // Check bailout_pc was written (offset 96)
        let bailout_pc = i64::from_ne_bytes(ctx_buf[96..104].try_into().unwrap());
        assert_eq!(bailout_pc, 2, "bailout should happen at pc 2 (Add)");

        // local[0] is dead at pc 2: the interpreter will resume at Add and use
        // reg[0], not reload from the local slot.
        assert_eq!(
            deopt_locals[0], 0,
            "dead local[0] should not be materialized into the deopt buffer"
        );

        // Check deopt registers: reg[0] should contain the boolean param,
        // reg[1] should contain int32(10).
        assert_eq!(
            deopt_regs[0],
            boxed_bool(true),
            "reg[0] should contain GetLocal result (boolean true)"
        );
        assert_eq!(
            deopt_regs[1],
            boxed_i32(10),
            "reg[1] should contain LoadInt32 value 10"
        );
        assert_eq!(
            deopt_regs[2], 0,
            "dead reg[2] should not be materialized into the deopt buffer"
        );
    }
}
