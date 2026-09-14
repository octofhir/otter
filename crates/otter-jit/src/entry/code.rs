//! Shared VM entry invocation for compiled code.
//!
//! # Contents
//! - [`enter_compiled`] — builds the `JitCtx` for one baseline or optimizing
//!   activation and maps its `NativeResultPair` to a [`JitExecOutcome`].
//!
//! # Invariants
//! - Entry pointers are called only through the frozen compiled-entry ABI
//!   (`extern "C" fn(*mut JitCtx) -> NativeResultPair`).
//! - The native frame published here carries the exact register window and
//!   the isolate-owned stub-table/registry addresses for the full call.
//! - Post-entry feedback reconciliation and collector rewriting of a boxed
//!   return/throw payload are one VM-owned transaction. The JIT never receives
//!   a root token or observes an intermediate stale payload.
//!
//! # See also
//! - [`super::abi`] defines the entry and context layouts.
//! - `crate::template::code` and [`crate::optimizing`] own finalized code
//!   objects that call this.

use super::{JitCtx, JitEntry, jit_pop_native_activation_stub, jit_push_native_activation_stub};
use otter_vm::{
    ActivationStack, Interpreter, JitExecOutcome, VmError, VmRuntimeActivation,
    native_abi::{
        ActiveFrameMut, NativeFrame, NativeFrameFlags, NativeFrameKind, NativeResultDomain,
        NativeResultStatus, VmFrameHeader, VmThread,
    },
};

/// Build the `JitCtx` for `activation` and invoke compiled code at `entry`, mapping
/// the returned status to a [`JitExecOutcome`].
///
/// Shared across entry kinds: the function-entry and loop-header OSR paths use
/// the identical [`JitEntry`] ABI (`extern "C" fn(*mut JitCtx) -> NativeResultPair`) and
/// the same `JitCtx` construction, differing only in which instruction the
/// prologue branches to. Lives free (it uses no compiled-code state) so any
/// [`JitFunctionCode`](otter_vm::JitFunctionCode) implementation can reuse it.
///
/// # Safety
/// `entry` must point at a prologue emitted with the [`JitEntry`] ABI inside a
/// live executable mapping that outlives the call, and `activation` must uphold the
/// [`VmRuntimeActivation`](otter_vm::VmRuntimeActivation) contract.
pub(crate) unsafe fn enter_compiled(
    activation: VmRuntimeActivation,
    entry: *const u8,
    code_object_id: u64,
    function_id: u32,
    register_count: u16,
    kind: NativeFrameKind,
    has_safepoints: bool,
) -> JitExecOutcome {
    {
        let stack = activation.stack_ptr().cast::<ActivationStack>();
        let vm = activation.vm_ptr().cast::<Interpreter>();
        // This interpreter-to-native entry boundary reads
        // the materialized activation once through the tier-neutral API; the
        // resulting NativeFrame is the sole machine-visible state thereafter.
        let (regs, self_value, this_value, upvalue_base, upvalue_count) = {
            // SAFETY: `activation.stack_ptr()` names the exclusively frozen
            // interpreter stack for this compiled-entry transaction.
            let stack_ref = unsafe { &mut *stack };
            let frame = &mut stack_ref[activation.frame_index()];
            let active = ActiveFrameMut::materialized(frame);
            let regs = active.register_base_ptr().cast::<u64>();
            let upvalue_base = if active.upvalue_count() == 0 {
                0
            } else {
                active.upvalue_base_ptr() as u64
            };
            (
                regs,
                active.self_value(),
                active.this_value(),
                upvalue_base,
                active.upvalue_count() as u32,
            )
        };
        // SAFETY: same contract; the activation array is isolate-owned, never
        // resized, and outlives every compiled activation.
        let activation_base = unsafe { (*vm).jit_native_activation_base() };
        let machine_roots_ptr = unsafe { (*vm).jit_machine_roots_addr() };
        let activation_top_ptr = unsafe { (*vm).jit_native_activation_top_addr() };
        let activation_limit = unsafe { (*vm).jit_generated_activation_limit() };
        let gc_heap = unsafe { (*vm).jit_gc_heap_ptr() };
        let marking_flag = unsafe { (*vm).jit_marking_flag_ptr() };
        let receiver_alloc = unsafe { (*vm).jit_receiver_allocation_window() };
        let runtime_stats = unsafe { (*vm).jit_runtime_stats_mut_ptr() };
        let interrupt_flag = unsafe { (*vm).jit_interrupt_flag_ptr() };
        let backedge_fuel = unsafe { (*vm).jit_backedge_fuel_ptr() };
        let global_this_offset = unsafe { (*vm).jit_global_this_offset_addr() };
        let global_lexical_epoch = unsafe { (*vm).jit_global_lexical_epoch_addr() };
        let native_stack_marker = 0_u8;
        let native_stack_limit = unsafe {
            (*vm).jit_native_stack_limit(std::ptr::from_ref(&native_stack_marker).addr())
        };
        let flags = if has_safepoints {
            NativeFrameFlags::from_bits(NativeFrameFlags::HAS_SAFEPOINTS)
        } else {
            NativeFrameFlags::empty()
        };
        let mut native_frame = NativeFrame::new(
            VmFrameHeader {
                function_id,
                pc: 0,
                register_count,
                kind,
                flags,
            },
            regs as u64,
            self_value,
            this_value,
        );
        native_frame.set_upvalue_window(upvalue_base, upvalue_count);
        if let Err(error) = activation.initialize_native_frame_state(&mut native_frame) {
            return JitExecOutcome::Fatal(error);
        }
        let mut thread = VmThread::empty();
        thread.current_frame = std::ptr::addr_of_mut!(native_frame) as u64;
        thread.current_code_object_id = code_object_id;
        thread.runtime_context = std::ptr::addr_of!(activation) as u64;
        // SAFETY: the boxed registry cell is isolate-owned and address-stable;
        // its view resolves safepoints for any installed code object, so a
        // nested compiled callee's allocating stubs root through real maps.
        thread.code_registry = unsafe { (*vm).jit_code_registry_view_addr() };
        thread.interrupt_cell = interrupt_flag as u64;
        thread.gc_heap = gc_heap as u64;
        thread.backedge_fuel_cell = backedge_fuel as u64;
        thread.global_lexical_epoch_cell = global_lexical_epoch as u64;
        thread.marking_flag_cell = marking_flag as u64;
        let mut error = None;
        let mut ctx = JitCtx {
            thread: std::ptr::addr_of_mut!(thread),
            native_frame: std::ptr::addr_of_mut!(native_frame),
            error: &mut error,
            activation_base: activation_base.cast(),
            activation_top_ptr,
            activation_limit,
            machine_roots_ptr,
            receiver_alloc,
            runtime_stats,
            global_this_offset,
            native_stack_limit,
            generated_feedback_clean: 1,
        };
        // SAFETY: the mapping is live and `entry` was emitted with the
        // `JitEntry` ABI.
        let entry: JitEntry = unsafe { std::mem::transmute(entry) };
        let activation_status = jit_push_native_activation_stub(&mut ctx);
        if activation_status != NativeResultStatus::Success as u64 {
            return JitExecOutcome::Fatal(error.take().unwrap_or(VmError::InvalidOperand));
        }
        let ret =
            unsafe { activation.with_native_eval_env_owner(ctx.native_frame, || entry(&mut ctx)) };
        let pop_status = jit_pop_native_activation_stub(&mut ctx);
        if pop_status != NativeResultStatus::Success as u64 {
            return JitExecOutcome::Fatal(error.take().unwrap_or(VmError::InvalidOperand));
        }
        let ret = match ret {
            Ok(ret) => ret,
            Err(error) => return JitExecOutcome::Fatal(error),
        };
        // Feedback repair may compile and collect after the native activation
        // is unpublished. The VM owns that complete transaction and returns
        // the same physical result carrier with any boxed payload rewritten by
        // the collector.
        let ret = match activation.finish_compiled_entry(ret, ctx.generated_feedback_clean == 0) {
            Ok(ret) => ret,
            Err(error) => return JitExecOutcome::Fatal(error),
        };
        let status = ret.validate(NativeResultDomain::Compiled);
        match status {
            Some(NativeResultStatus::Success) if error.is_none() => {
                JitExecOutcome::Returned(ret.payload_value())
            }
            Some(NativeResultStatus::SideExit) if error.is_none() => {
                match ret.side_exit_payload() {
                    Some(exit) if exit.logical_pc() == native_frame.header.pc => {
                        debug_assert_eq!(exit.logical_pc(), native_frame.header.pc);
                        JitExecOutcome::Bailed(exit)
                    }
                    Some(_) | None => JitExecOutcome::Fatal(VmError::InvalidOperand),
                }
            }
            Some(NativeResultStatus::Throw) if error.is_none() => {
                JitExecOutcome::Throw(ret.payload_value())
            }
            Some(NativeResultStatus::Fatal) => {
                JitExecOutcome::Fatal(error.take().unwrap_or(VmError::InvalidOperand))
            }
            Some(
                NativeResultStatus::Success
                | NativeResultStatus::SideExit
                | NativeResultStatus::Throw
                | NativeResultStatus::Continue
                | NativeResultStatus::OutOfMemory,
            )
            | None => JitExecOutcome::Fatal(error.take().unwrap_or(VmError::InvalidOperand)),
        }
    }
}
