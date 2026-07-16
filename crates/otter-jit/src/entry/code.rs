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
    HoltStack, Interpreter, JitExecOutcome, Value, VmError, VmRuntimeActivation,
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
    has_safepoints: bool,
) -> JitExecOutcome {
    {
        let stack = activation.stack_ptr().cast::<HoltStack>();
        let vm = activation.vm_ptr().cast::<Interpreter>();
        // SAFETY: `activation.stack_ptr()` is a valid `*mut HoltStack` for this call.
        let regs =
            Interpreter::jit_frame_regs_ptr(unsafe { &mut *stack }, activation.frame_index());
        // SAFETY: `activation.vm_ptr()`/`activation.stack_ptr()` are valid for this call and not aliased
        // by a live `&mut` (the VM froze its borrows); read the self closure up
        // front so a `MakeFunction`-of-self needs no Rust round-trip.
        let self_closure =
            unsafe { (*vm).jit_frame_self_closure_bits(&*stack, activation.frame_index()) };
        // SAFETY: same validity/aliasing contract as `self_closure` above.
        let this_value = unsafe { (*vm).jit_frame_this_bits(&*stack, activation.frame_index()) };
        // SAFETY: same validity/aliasing contract; the spine `Box` outlives this
        // entry (frame-owned), and the cells it holds are old-space (immobile).
        let upvalues_ptr =
            Interpreter::jit_frame_upvalues_ptr(unsafe { &*stack }, activation.frame_index());
        // SAFETY: `vm` is a valid `*mut Interpreter` for this entry and not
        // aliased by a live `&mut` (the VM froze its borrows); these return the
        // stable base / `reg_top` address of the flat JIT register stack.
        let reg_stack_base = unsafe { (*vm).jit_reg_stack_base() };
        let reg_top_ptr = unsafe { (*vm).jit_reg_top_ptr() };
        // SAFETY: same contract; the activation array is isolate-owned, never
        // resized, and outlives every compiled activation.
        let activation_base = unsafe { (*vm).jit_native_activation_base() };
        let activation_top_ptr = unsafe { (*vm).jit_native_activation_top_addr() };
        let activation_limit = unsafe { (*vm).jit_native_activation_limit() };
        let sync_reentry_depth_ptr = unsafe { (*vm).jit_sync_reentry_depth_ptr() };
        let sync_reentry_limit = unsafe { (*vm).jit_sync_reentry_limit() };
        let gc_heap = unsafe { (*vm).jit_gc_heap_ptr() };
        let interrupt_flag = unsafe { (*vm).jit_interrupt_flag_ptr() };
        let backedge_fuel = unsafe { (*vm).jit_backedge_fuel_ptr() };
        let flags = if has_safepoints {
            NativeFrameFlags::from_bits(NativeFrameFlags::HAS_SAFEPOINTS)
        } else {
            NativeFrameFlags::empty()
        };
        let mut native_frame = NativeFrame {
            header: VmFrameHeader {
                function_id,
                code_block_id: function_id,
                pc: 0,
                register_count,
                kind: NativeFrameKind::Baseline,
                flags,
            },
            previous_frame: 0,
            register_base: regs as u64,
            argument_base: 0,
            feedback_base: 0,
            code_object_id,
            this_value_bits: this_value,
            new_target_bits: Value::undefined().to_bits(),
            return_register: u32::MAX,
            cold_state_index: u32::MAX,
            argument_count: 0,
            reserved0: 0,
            feedback_id: 0,
        };
        let mut thread = VmThread::empty();
        thread.current_frame = std::ptr::addr_of_mut!(native_frame) as u64;
        thread.runtime_context = std::ptr::addr_of!(activation) as u64;
        // SAFETY: `vm` is a valid `*mut Interpreter` for this entry; the header
        // and entry columns are isolate-owned and outlive every activation.
        thread.runtime_stub_table = unsafe { (*vm).jit_runtime_stub_table_addr() };
        // SAFETY: the boxed registry cell is isolate-owned and address-stable;
        // its view resolves safepoints for any installed code object, so a
        // nested compiled callee's allocating stubs root through real maps.
        thread.code_registry = unsafe { (*vm).jit_code_registry_view_addr() };
        thread.interrupt_cell = interrupt_flag as u64;
        thread.gc_heap = gc_heap as u64;
        thread.backedge_fuel_cell = backedge_fuel as u64;
        thread.sync_reentry_depth_cell = sync_reentry_depth_ptr as u64;
        thread.sync_reentry_limit = sync_reentry_limit;
        let mut error = None;
        let mut ctx = JitCtx {
            regs,
            self_closure,
            this_value,
            thread: std::ptr::addr_of_mut!(thread),
            native_frame: std::ptr::addr_of_mut!(native_frame),
            frame_index: activation.frame_index(),
            upvalues_ptr,
            error: &mut error,
            direct_entry_addr: 0,
            direct_regs: std::ptr::null_mut(),
            direct_self_closure: 0,
            direct_this_value: 0,
            direct_frame_index: 0,
            direct_upvalues_ptr: 0,
            direct_frame_ids: 0,
            direct_frame_meta: 0,
            direct_code_object_id: 0,
            reg_stack_base,
            reg_top_ptr,
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
