//! Default implementation of the tier-up hook.
//!
//! Implements [`otter_vm::interpreter::TierUpHook`] using the existing
//! [`code_cache`](crate::code_cache) and compile pipeline ([`compile_function_with_feedback`](crate::pipeline::compile_function_with_feedback)).
//!
//! The interpreter calls `execute_cached()` on every bytecode-closure entry.
//! - If the function has machine code installed in the thread-local
//!   [`code_cache`](crate::code_cache), we build a [`JitContext`] that points at
//!   the caller's register window and transfer control through the compiled
//!   function pointer.
//! - If the function has no cached code, we return `NotCompiled` so the
//!   interpreter falls back to bytecode execution (and decrements the hotness
//!   budget).
//!
//! `try_compile()` is invoked after the interpreter observes the hotness
//! budget has crossed zero. We compile synchronously (JSC-Baseline-style,
//! ~0.5-1ms per function) and insert into the code cache, so that the next
//! [`execute_cached()`] call hits the fast path.
//!
//! All `unsafe` is encapsulated inside this file; the `otter-vm` interpreter
//! (which sets `#![forbid(unsafe_code)]`) sees only safe trait methods.

use std::ptr::null_mut;
use std::sync::Arc;

use otter_vm::feedback::FeedbackSlotId;
use otter_vm::interpreter::{TierUpExecResult, TierUpHook};
use otter_vm::module::{FunctionIndex, Module};
use otter_vm::value::RegisterValue;
use otter_vm::{Function, RuntimeState};

use crate::BAILOUT_SENTINEL;
use crate::code_cache;
use crate::config::jit_config;
use crate::context::JitContext;
use crate::pipeline::{compile_function, compile_function_with_feedback};
use crate::{BailoutReason, Tier, telemetry};

/// Default tier-up hook backed by the thread-local code cache and the
/// synchronous compile pipeline.
///
/// Stateless — all state lives in the thread-local
/// [`code_cache`](crate::code_cache) and in per-function telemetry. A single
/// `Arc<DefaultTierUpHook>` is installed into the runtime at startup.
#[derive(Debug, Default)]
pub struct DefaultTierUpHook;

impl DefaultTierUpHook {
    /// Returns an `Arc` containing a fresh [`DefaultTierUpHook`].
    #[must_use]
    pub fn new_arc() -> Arc<dyn TierUpHook> {
        Arc::new(Self)
    }
}

/// On a JIT bailout, demote the arithmetic feedback slot (if any)
/// attached to the bailout PC to [`ArithmeticFeedback::Any`] and
/// invalidate the cached stencil.
///
/// The stencil invalidation is what breaks the "trust-int32
/// bailout loop": without it, the next tier-up call would re-issue
/// the same cached code and bail at the same PC forever. With it,
/// the next tier-up call runs `try_compile` against the demoted
/// feedback and emits the guarded variant for the offending op.
///
/// The function also demotes the arithmetic slot even when the
/// bailout PC isn't an arithmetic op — the `feedback_map.get()`
/// lookup returns `None` in that case and the demote is a no-op. The
/// invalidation still runs, covering deopts from `Ldar` or
/// `CompareAcc` paths where the next recompile with unchanged
/// feedback might still pick the same shape.
fn demote_and_invalidate_on_bailout(
    module: &Module,
    function_index: FunctionIndex,
    function_ptr: *const Function,
    bailout_pc: u32,
    runtime_ptr: *mut (),
) {
    if runtime_ptr.is_null() {
        return;
    }
    // SAFETY: runtime_ptr is a live `*mut RuntimeState` held by the
    // interpreter for the duration of the hook call.
    let runtime = unsafe { &mut *(runtime_ptr as *mut RuntimeState) };
    if let Some(function) = module.function(function_index)
        && let Some(bytecode_slot) = function.bytecode().feedback().get(bailout_pc)
        && let Some(fv) = runtime.feedback_vector_mut(function_index)
    {
        fv.demote_arithmetic_to_any(FeedbackSlotId(bytecode_slot.0));
    }
    // Invalidate the cached stencil regardless of whether we found a
    // feedback slot to demote — on recompile, `try_compile` will
    // consult the (possibly demoted) feedback and emit a guarded
    // stencil for the offending PCs.
    code_cache::invalidate(function_ptr);
}

impl TierUpHook for DefaultTierUpHook {
    fn execute_cached(
        &self,
        module: &Module,
        function_index: FunctionIndex,
        registers_base: *mut RegisterValue,
        register_count: usize,
        this_raw: u64,
        runtime_ptr: *mut (),
        interrupt_flag: *const u8,
    ) -> TierUpExecResult {
        let function = match module.function(function_index) {
            Some(f) => f,
            None => return TierUpExecResult::NotCompiled,
        };
        let function_ptr: *const Function = function;

        // Fast lookup in the thread-local code cache.
        let Some(entry) = code_cache::get(function_ptr) else {
            return TierUpExecResult::NotCompiled;
        };

        let register_count_u32 = u32::try_from(register_count).unwrap_or(u32::MAX);

        // Best-effort resolution of the TypedHeap slots base for IC fast paths.
        // We must never dereference `runtime_ptr` if it is null. Otherwise it is
        // a valid `*mut RuntimeState` owned by the caller (the interpreter).
        let heap_slots_base: *const () = if runtime_ptr.is_null() {
            std::ptr::null()
        } else {
            // SAFETY: `runtime_ptr` is a live `*mut RuntimeState`, provided by
            // the interpreter immediately before invoking this hook.
            let runtime = unsafe { &*(runtime_ptr as *const RuntimeState) };
            runtime.heap().slots_ptr()
        };

        let mut ctx = JitContext {
            registers_base: registers_base.cast::<u64>(),
            local_count: register_count_u32,
            register_count: register_count_u32,
            constants: std::ptr::null(),
            this_raw,
            interrupt_flag,
            interpreter: std::ptr::null(),
            vm_ctx: null_mut(),
            function_ptr: function_ptr.cast::<()>(),
            upvalues_ptr: std::ptr::null(),
            upvalue_count: 0,
            callee_raw: RegisterValue::undefined().raw_bits(),
            home_object_raw: RegisterValue::undefined().raw_bits(),
            proto_epoch: 0,
            bailout_reason: 0,
            bailout_pc: 0,
            secondary_result: 0,
            module_ptr: (module as *const Module).cast::<()>(),
            runtime_ptr,
            heap_slots_base,
            accumulator_raw: RegisterValue::undefined().raw_bits(),
        };

        telemetry::record_jit_entry();
        telemetry::record_function_jit_entry(function.name().unwrap_or("<anonymous>"), 1);

        // SAFETY: `entry` is a function pointer produced by our own compiler
        // pipeline; it has the documented `extern "C" fn(*mut JitContext) -> u64`
        // ABI. The ctx has been constructed with valid pointers above, and the
        // register buffer outlives this call (held by the caller's
        // `Activation`). Bailout state and return value are written into `ctx`.
        let raw_result = unsafe {
            let func: unsafe extern "C" fn(*mut JitContext) -> u64 = std::mem::transmute(entry);
            func(&mut ctx)
        };

        if raw_result == BAILOUT_SENTINEL {
            code_cache::record_deopt(function_ptr);
            // M_JIT_C.2.5: demote the bailout PC's arithmetic feedback
            // slot so the next recompile falls back to the guarded
            // variant. Also invalidates the cached stencil so the next
            // tier-up call actually recompiles.
            demote_and_invalidate_on_bailout(
                module,
                function_index,
                function_ptr,
                ctx.bailout_pc,
                runtime_ptr,
            );

            // Anti-thrash: after too many deopts, drop the compiled code and
            // blacklist the function so the interpreter stops asking us to
            // recompile it.
            let max_deopts = jit_config().max_deopts_before_blacklist;
            // We don't have the exact deopt_count accessor on the cache; use
            // the existing flush_unstable to sweep the one entry if it's
            // over budget. Cheaper than maintaining a new accessor and still
            // bounded at one function per call.
            let _ = code_cache::flush_unstable(max_deopts);
            // If this function was flushed, also blacklist it so we don't
            // retry compile on the very next tier-up window.
            if !code_cache::contains(function_ptr) && !runtime_ptr.is_null() {
                // SAFETY: runtime_ptr is valid for the duration of the
                // hook call — the interpreter holds a &mut RuntimeState.
                let runtime = unsafe { &mut *(runtime_ptr as *mut RuntimeState) };
                runtime.blacklist_for_tier_up(function_index);
            }

            return TierUpExecResult::Bailout {
                resume_pc: ctx.bailout_pc,
                reason: ctx.bailout_reason,
                accumulator_raw: ctx.accumulator_raw,
            };
        }

        let value = RegisterValue::from_raw_bits(raw_result).unwrap_or_default();
        TierUpExecResult::Return(value)
    }

    fn execute_cached_at_pc(
        &self,
        module: &Module,
        function_index: FunctionIndex,
        registers_base: *mut RegisterValue,
        register_count: usize,
        this_raw: u64,
        runtime_ptr: *mut (),
        interrupt_flag: *const u8,
        byte_pc: u32,
        accumulator_raw: u64,
    ) -> TierUpExecResult {
        let function = match module.function(function_index) {
            Some(f) => f,
            None => return TierUpExecResult::NotCompiled,
        };
        let function_ptr: *const Function = function;

        // Resolve the OSR trampoline offset for this PC. Returns None if
        // the function isn't cached or this PC isn't a registered loop
        // header.
        let Some(osr_offset) = code_cache::osr_native_offset(function_ptr, byte_pc) else {
            return TierUpExecResult::NotCompiled;
        };
        let Some(entry_base) = code_cache::get(function_ptr) else {
            return TierUpExecResult::NotCompiled;
        };

        let register_count_u32 = u32::try_from(register_count).unwrap_or(u32::MAX);

        let heap_slots_base: *const () = if runtime_ptr.is_null() {
            std::ptr::null()
        } else {
            // SAFETY: `runtime_ptr` is a live `*mut RuntimeState`, provided by
            // the interpreter immediately before invoking this hook.
            let runtime = unsafe { &*(runtime_ptr as *const RuntimeState) };
            runtime.heap().slots_ptr()
        };

        let mut ctx = JitContext {
            registers_base: registers_base.cast::<u64>(),
            local_count: register_count_u32,
            register_count: register_count_u32,
            constants: std::ptr::null(),
            this_raw,
            interrupt_flag,
            interpreter: std::ptr::null(),
            vm_ctx: null_mut(),
            function_ptr: function_ptr.cast::<()>(),
            upvalues_ptr: std::ptr::null(),
            upvalue_count: 0,
            callee_raw: RegisterValue::undefined().raw_bits(),
            home_object_raw: RegisterValue::undefined().raw_bits(),
            proto_epoch: 0,
            bailout_reason: 0,
            bailout_pc: 0,
            secondary_result: 0,
            module_ptr: (module as *const Module).cast::<()>(),
            runtime_ptr,
            heap_slots_base,
            // OSR trampoline reads this and copies it into the pinned
            // accumulator register before jumping into the loop body.
            accumulator_raw,
        };

        telemetry::record_jit_entry();
        telemetry::record_function_jit_entry(function.name().unwrap_or("<anonymous>"), 1);

        // SAFETY: `entry_base + osr_offset` falls inside the same
        // executable buffer as the function's main entry, owned by the
        // `CompiledFunction` that produced both pointers. The trampoline
        // at this offset has the same `extern "C" fn(*mut JitContext) -> u64`
        // ABI as the main entry — it shares the prologue + body +
        // epilogue. `ctx` is initialised above with valid pointers and
        // a register buffer that outlives this call (held by the
        // caller's `Activation`).
        let raw_result = unsafe {
            let trampoline = entry_base.add(osr_offset as usize);
            let func: unsafe extern "C" fn(*mut JitContext) -> u64 =
                std::mem::transmute(trampoline);
            func(&mut ctx)
        };

        if raw_result == BAILOUT_SENTINEL {
            code_cache::record_deopt(function_ptr);
            // Mirror the function-entry bailout path: demote the
            // bailout PC's arithmetic feedback slot and invalidate the
            // cached stencil so a trust-int32 mid-loop bailout can't
            // loop on the same stencil.
            demote_and_invalidate_on_bailout(
                module,
                function_index,
                function_ptr,
                ctx.bailout_pc,
                runtime_ptr,
            );
            return TierUpExecResult::Bailout {
                resume_pc: ctx.bailout_pc,
                reason: ctx.bailout_reason,
                accumulator_raw: ctx.accumulator_raw,
            };
        }

        let value = RegisterValue::from_raw_bits(raw_result).unwrap_or_default();
        TierUpExecResult::Return(value)
    }

    fn try_compile(
        &self,
        module: &Module,
        function_index: FunctionIndex,
        runtime_ptr: *mut (),
    ) -> bool {
        if runtime_ptr.is_null() {
            return false;
        }
        let Some(function) = module.function(function_index) else {
            return false;
        };
        let function_ptr: *const Function = function;

        // Idempotent: if another path already compiled and cached this
        // function, we're done.
        if code_cache::contains(function_ptr) {
            return true;
        }

        // SAFETY: runtime_ptr is a live `*mut RuntimeState` held by the
        // interpreter across this entire hook invocation.
        let runtime = unsafe { &mut *(runtime_ptr as *mut RuntimeState) };

        // Prefer feedback-aware compilation when we have persistent feedback
        // for this function (normal case after first call returns). Fall
        // back to the profile-free path otherwise.
        let feedback = runtime.feedback_vector(function_index).cloned();
        let result = match feedback {
            Some(ref fv) => compile_function_with_feedback(function, fv),
            None => compile_function(function),
        };

        match result {
            Ok(compiled) => {
                code_cache::insert_with_tier(function_ptr, compiled, Tier::Baseline);
                true
            }
            Err(_err) => {
                // Permanent failure: prevent further retries.
                runtime.blacklist_for_tier_up(function_index);
                false
            }
        }
    }
}

// Small helper: downcast a `BailoutReason` from its raw u32 representation
// while staying in release. Currently unused but kept for future telemetry
// wiring.
#[allow(dead_code)]
#[inline]
fn reason_from_raw(reason: u32) -> BailoutReason {
    BailoutReason::from_raw(reason).unwrap_or(BailoutReason::Unsupported)
}
