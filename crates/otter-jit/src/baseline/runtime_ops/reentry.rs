//! VM re-entry and compiled-call lifecycle stubs.
//!
//! # Contents
//! - Native activation publication and release.
//! - Direct-call preparation, completion, and bailout recovery.
//! - Cooperative backedge polling.
//!
//! # Invariants
//! Every entry receives a live JIT context whose frame/register roots are
//! published for the entire call. Errors are parked in the shared context slot.
//!
//! # See also
//! - `super::super::abi` — machine-visible entry context.

use otter_vm::{Value, VmError};

use super::super::{
    JitCtx, JitRet, STATUS_RETURNED, STATUS_THREW, refresh_jit_collection_method_ics,
    unpack_method_arg_regs,
};

pub(crate) fn park_jit_error(ctx: &mut JitCtx, err: VmError) {
    // SAFETY: every `JitCtx` is built with an initialized error slot that lives
    // for the compiled entry's dynamic extent; nested direct-call contexts copy
    // the same pointer.
    unsafe {
        *ctx.error = Some(err);
    }
}

/// Publish one machine-constructed [`JitCtx`] before its compiled entry can
/// reach an allocating/reentrant safepoint. Returns `0` on success and parks a
/// stack-overflow error in the shared slot on failure.
pub(crate) extern "C" fn jit_push_native_activation_stub(ctx: *mut JitCtx) -> u64 {
    // SAFETY: the caller has fully initialized `ctx` on its native stack and
    // keeps it live until the matching pop stub.
    let ctx = unsafe { &mut *ctx };
    let vm = unsafe { &mut *ctx.activation().vm_ptr() };
    // SAFETY: both fields live inside `ctx`, whose native allocation remains
    // live across the compiled callee's dynamic extent.
    match unsafe {
        vm.jit_push_native_activation(
            std::ptr::addr_of_mut!(ctx.self_closure),
            std::ptr::addr_of_mut!(ctx.this_value),
        )
    } {
        Ok(()) => 0,
        Err(err) => {
            park_jit_error(ctx, err);
            1
        }
    }
}

/// Release the topmost native JIT activation before its `JitCtx` stack record
/// is discarded.
pub(crate) extern "C" fn jit_pop_native_activation_stub(ctx: *mut JitCtx) -> u64 {
    // SAFETY: the active context and its interpreter pointer are live by ABI.
    let ctx = unsafe { &mut *ctx };
    let vm = unsafe { &mut *ctx.activation().vm_ptr() };
    vm.jit_pop_native_activation();
    0
}

/// Validate a closure callee for scratch-frame inlining and return its captured
/// upvalue-spine base, or `0` when the site must take the normal call path.
pub(crate) extern "C" fn jit_inline_closure_upvalues_stub(
    ctx: *mut JitCtx,
    callee_reg: u64,
    expected_fid: u64,
) -> usize {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let vm = unsafe { &mut *ctx.activation().vm_ptr() };
    // SAFETY: `callee_reg` comes from a bytecode register operand inside the
    // active frame window.
    let callee_bits = unsafe { *ctx.regs.add(callee_reg as usize) };
    vm.jit_inline_closure_upvalues(Value::from_bits(callee_bits), expected_fid as u32)
        .unwrap_or(0)
}

/// Prepare a direct compiled **method** call (`recv.name(args…)`). Same
/// `ctx.direct_*` / status contract as [`jit_prepare_direct_call_stub`], but
/// status `2` means "ineligible — use the in-place full method-call stub"
/// rather than "bail to the interpreter" (a native/polymorphic method in a hot
/// loop must keep running compiled).
#[allow(clippy::too_many_arguments)]
pub(crate) extern "C" fn jit_prepare_direct_method_call_stub(
    ctx: *mut JitCtx,
    recv: u64,
    name_idx: u64,
    site: u64,
    argc: u64,
    packed_args: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let vm = unsafe { &mut *ctx.activation().vm_ptr() };
    let stack = unsafe { &mut *ctx.activation().stack_ptr() };
    let context = unsafe { &*ctx.activation().context_ptr() };
    let all = unpack_method_arg_regs(packed_args);
    let argc = (argc as usize).min(all.len());
    let status = match vm.jit_prepare_direct_method_call(
        context,
        stack,
        ctx.frame_index,
        recv as u16,
        name_idx as u32,
        site as usize,
        &all[..argc],
        ctx.regs.cast::<otter_vm::Value>().cast_const(),
    ) {
        Ok(Some(prepared)) => {
            ctx.direct_entry_addr = prepared.entry_addr;
            ctx.direct_regs = prepared.regs;
            ctx.direct_self_closure = prepared.self_closure;
            ctx.direct_this_value = prepared.this_value;
            ctx.direct_frame_index = prepared.frame_index;
            ctx.direct_upvalues_ptr = prepared.upvalues_ptr;
            0
        }
        Ok(None) => 2,
        Err(err) => {
            park_jit_error(ctx, err);
            1
        }
    };
    refresh_jit_collection_method_ics(ctx, vm);
    status
}

pub(crate) extern "C" fn jit_finish_direct_call_returned_stub(
    ctx: *mut JitCtx,
    dst: u64,
    callee_frame_index: u64,
    value: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let vm = unsafe { &mut *ctx.activation().vm_ptr() };
    let stack = unsafe { &mut *ctx.activation().stack_ptr() };
    match vm.jit_finish_direct_call_returned(
        stack,
        ctx.frame_index,
        callee_frame_index as usize,
        dst as u16,
        Value::from_bits(value),
    ) {
        Ok(()) => 0,
        Err(err) => {
            park_jit_error(ctx, err);
            1
        }
    }
}

pub(crate) extern "C" fn jit_finish_direct_call_bailed_stub(
    ctx: *mut JitCtx,
    dst: u64,
    callee_frame_index: u64,
    resume_pc: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let vm = unsafe { &mut *ctx.activation().vm_ptr() };
    let stack = unsafe { &mut *ctx.activation().stack_ptr() };
    let context = unsafe { &*ctx.activation().context_ptr() };
    match vm.jit_finish_direct_call_bailed(
        context,
        stack,
        ctx.frame_index,
        callee_frame_index as usize,
        dst as u16,
        resume_pc as u32,
    ) {
        Ok(()) => 0,
        Err(err) => {
            park_jit_error(ctx, err);
            1
        }
    }
}

pub(crate) extern "C" fn jit_abort_direct_call_stub(
    ctx: *mut JitCtx,
    callee_frame_index: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let vm = unsafe { &mut *ctx.activation().vm_ptr() };
    let stack = unsafe { &mut *ctx.activation().stack_ptr() };
    vm.jit_abort_direct_call(stack, callee_frame_index as usize);
    0
}

/// Bridge stub for a *frameless* self-recursive callee that bailed: rebuild an
/// interpreter frame from the live register-stack window and run it to
/// completion. Returns the callee's value in `x0` with `STATUS_RETURNED`, or
/// `STATUS_THREW` (error parked in `ctx`) on an uncaught throw.
pub(crate) extern "C" fn jit_self_call_bail_stub(
    ctx: *mut JitCtx,
    resume_pc: u64,
    regcount: u64,
) -> JitRet {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let vm = unsafe { &mut *ctx.activation().vm_ptr() };
    let stack = unsafe { &mut *ctx.activation().stack_ptr() };
    let context = unsafe { &*ctx.activation().context_ptr() };
    match vm.jit_self_call_bail(
        context,
        stack,
        ctx.frame_index,
        resume_pc as u32,
        regcount as usize,
    ) {
        Ok(value) => JitRet {
            value: value.to_bits(),
            status: STATUS_RETURNED,
        },
        Err(err) => {
            park_jit_error(ctx, err);
            JitRet {
                value: 0,
                status: STATUS_THREW,
            }
        }
    }
}

/// Complete a frameless direct-method callee after its compiled entry bailed.
/// The VM rebuilds the callee frame from the rooted flat register window and
/// the already-resolved method value; no bytecode instruction is decoded here.
pub(crate) extern "C" fn jit_direct_method_call_bail_stub(
    ctx: *mut JitCtx,
    resume_pc: u64,
    regcount: u64,
    callee: u64,
    this: u64,
) -> JitRet {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let vm = unsafe { &mut *ctx.activation().vm_ptr() };
    let stack = unsafe { &mut *ctx.activation().stack_ptr() };
    let context = unsafe { &*ctx.activation().context_ptr() };
    match vm.jit_direct_method_call_bail(
        context,
        stack,
        resume_pc as u32,
        regcount as usize,
        Value::from_bits(callee),
        Value::from_bits(this),
    ) {
        Ok(value) => JitRet {
            value: value.to_bits(),
            status: STATUS_RETURNED,
        },
        Err(err) => {
            park_jit_error(ctx, err);
            JitRet {
                value: 0,
                status: STATUS_THREW,
            }
        }
    }
}

/// Bridge stub: build a `MakeFunction` closure from compiled code. Returns `0`
/// on success, `1` when construction threw (error parked in `ctx`).
pub(crate) extern "C" fn jit_make_fn_stub(ctx: *mut JitCtx, dst: u64, idx: u64) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let vm = unsafe { &mut *ctx.activation().vm_ptr() };
    let stack = unsafe { &mut *ctx.activation().stack_ptr() };
    let context = unsafe { &*ctx.activation().context_ptr() };
    match vm.jit_runtime_make_function(context, stack, ctx.frame_index, dst as u16, idx as u32) {
        Ok(()) => 0,
        Err(err) => {
            park_jit_error(ctx, err);
            1
        }
    }
}

/// Poll VM interrupts and runtime budget on compiled back-edges. Mirrors the
/// interpreter's cooperative checkpoint so watchdogs and budget rejection apply
/// equally after a loop tiers up through OSR.
pub(crate) extern "C" fn jit_backedge_poll_stub(ctx: *mut JitCtx) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    if ctx.activation().vm_ptr().is_null() {
        return 0;
    }
    let vm = unsafe { &mut *ctx.activation().vm_ptr() };
    match vm.jit_backedge_poll() {
        Ok(()) => 0,
        Err(err) => {
            park_jit_error(ctx, err);
            1
        }
    }
}
