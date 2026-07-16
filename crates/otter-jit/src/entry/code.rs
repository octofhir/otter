//! Shared VM entry invocation for compiled code.
//!
//! # Contents
//! - [`enter_compiled`] — builds the `JitCtx` for one baseline or optimizing
//!   activation and maps its returned status to a [`JitExecOutcome`].
//!
//! # Invariants
//! - Entry pointers are called only through the frozen compiled-entry ABI
//!   (`extern "C" fn(*mut JitCtx) -> JitRet`).
//! - The native frame published here carries the exact register window and
//!   the isolate-owned stub-table/registry addresses for the full call.
//!
//! # See also
//! - [`super::abi`] defines the entry and context layouts.
//! - `crate::template::code` and [`crate::optimizing`] own finalized code
//!   objects that call this.

use super::{
    JitCtx, JitEntry, STATUS_BAILED, STATUS_RETURNED, STATUS_THREW, jit_pop_native_activation_stub,
    jit_push_native_activation_stub,
};
use otter_vm::{
    ActivationStack, ActiveFrameMut, Interpreter, JitExecOutcome, Value, VmError,
    VmRuntimeActivation,
    native_abi::{NativeFrame, NativeFrameFlags, NativeFrameKind, VmFrameHeader, VmThread},
};

/// Build the `JitCtx` for `activation` and invoke compiled code at `entry`, mapping
/// the returned status to a [`JitExecOutcome`].
///
/// Shared across entry kinds: the function-entry and loop-header OSR paths use
/// the identical [`JitEntry`] ABI (`extern "C" fn(*mut JitCtx) -> JitRet`) and
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
        // This is the remaining interpreter-to-native entry adapter. It reads
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
        let activation_top_ptr = unsafe { (*vm).jit_native_activation_top_addr() };
        let activation_limit = unsafe { (*vm).jit_native_activation_limit() };
        let gc_heap = unsafe { (*vm).jit_gc_heap_ptr() };
        let interrupt_flag = unsafe { (*vm).jit_interrupt_flag_ptr() };
        let backedge_fuel = unsafe { (*vm).jit_backedge_fuel_ptr() };
        let flags = if has_safepoints {
            NativeFrameFlags::from_bits(NativeFrameFlags::HAS_SAFEPOINTS)
        } else {
            NativeFrameFlags::empty()
        };
        let mut native_frame = NativeFrame::new(
            VmFrameHeader {
                function_id,
                code_block_id: function_id,
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
        native_frame.set_materialized_activation(activation.frame_index() as u32);
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
        let mut error = None;
        let mut ctx = JitCtx {
            thread: std::ptr::addr_of_mut!(thread),
            native_frame: std::ptr::addr_of_mut!(native_frame),
            error: &mut error,
            direct_call: std::mem::MaybeUninit::uninit(),
            activation_base: activation_base.cast(),
            activation_top_ptr,
            activation_limit,
        };
        // SAFETY: the mapping is live and `entry` was emitted with the
        // `JitEntry` ABI.
        let entry: JitEntry = unsafe { std::mem::transmute(entry) };
        let activation_status = jit_push_native_activation_stub(&mut ctx);
        if activation_status != 0 {
            return JitExecOutcome::Threw(error.take().unwrap_or(VmError::InvalidOperand));
        }
        let ret = entry(&mut ctx);
        let _ = jit_pop_native_activation_stub(&mut ctx);
        match ret.status {
            STATUS_RETURNED => JitExecOutcome::Returned(Value::from_bits(ret.value)),
            STATUS_BAILED => JitExecOutcome::Bailed(native_frame.header.pc),
            STATUS_THREW => JitExecOutcome::Threw(error.take().unwrap_or(VmError::InvalidOperand)),
            _ => JitExecOutcome::Threw(VmError::InvalidOperand),
        }
    }
}
