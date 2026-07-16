//! Compiled plain/method-call ABI adapters and direct-call lifecycle stubs.
//!
//! # Contents
//! - Native activation publication and release for nested compiled calls.
//! - Closure validation for scratch-frame inline calls.
//! - Plain and method direct-call preparation plus generic in-place fallback.
//! - Returned, bailed, thrown, and self-call completion adapters.
//!
//! # Invariants
//! - Every entry receives a live [`JitCtx`] whose frame/register roots remain
//!   published across allocating or reentrant VM work.
//! - A successful prepare publishes the callee frame before staging its
//!   machine entry state in `ctx.direct_call`.
//! - Every prepared direct call is paired with exactly one returned, bailed,
//!   or abort completion that releases the VM-owned frame lifecycle state.
//! - JavaScript exceptions are parked in the shared context slot and never
//!   unwind through generated machine frames.
//!
//! # See also
//! - `super::reentry` — shared throw resumption and error parking.
//! - [`crate::arm64::CallTrampoline`] — tier-independent call lifecycle.
//! - `otter_vm::interp::jit_call` — VM-owned target resolution and frame work.

use otter_vm::{Value, VmError};

use super::super::{JitCtx, JitRet, STATUS_RETURNED, STATUS_THREW};
use super::reentry::{park_jit_error, try_resume_caller_throw};

/// Commit the VM's single prepared-call record to the machine-visible context.
///
/// Both plain and method resolution deliberately share this status mapping;
/// their only difference is how the VM resolves the callable before producing
/// the record.
fn stage_prepared_call(
    ctx: &mut JitCtx,
    result: Result<Option<otter_vm::jit::JitPreparedDirectCall>, VmError>,
) -> u64 {
    match result {
        Ok(Some(prepared)) => {
            ctx.direct_call.write(prepared);
            0
        }
        Ok(None) => 2,
        Err(err) => {
            park_jit_error(ctx, err);
            1
        }
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

/// Prepare a direct compiled **plain** call (`callee(args…)`).
///
/// Fills `ctx.direct_call` and returns `0` on success; `1` parks a thrown error
/// in the ctx; `2` means "ineligible — bail to the interpreter", which runs
/// the call with full semantics.
pub(crate) extern "C" fn jit_prepare_direct_call_stub(
    ctx: *mut JitCtx,
    callee_reg: u64,
    argc: u64,
    packed_args: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let vm = unsafe { &mut *ctx.activation().vm_ptr() };
    let stack = unsafe { &mut *ctx.activation().stack_ptr() };
    let context = unsafe { &*ctx.activation().context_ptr() };
    let mut inline_args = [0u16; crate::entry::MAX_METHOD_ARGS];
    let args = crate::entry::decode_packed_arg_regs(argc as usize, packed_args, &mut inline_args);
    let prepared = vm.jit_prepare_direct_call(
        context,
        stack,
        ctx.frame_index,
        callee_reg as u16,
        args,
        ctx.regs.cast::<otter_vm::Value>().cast_const(),
    );
    stage_prepared_call(ctx, prepared)
}

/// Prepare a direct compiled **method** call (`recv.name(args…)`). Same
/// `ctx.direct_call` / status contract as [`jit_prepare_direct_call_stub`], but
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
    let mut inline_args = [0u16; crate::entry::MAX_METHOD_ARGS];
    let args = crate::entry::decode_packed_arg_regs(argc as usize, packed_args, &mut inline_args);
    let prepared = vm.jit_prepare_direct_method_call(
        context,
        stack,
        ctx.frame_index,
        recv as u16,
        name_idx as u32,
        site as usize,
        args,
        ctx.regs.cast::<otter_vm::Value>().cast_const(),
    );
    stage_prepared_call(ctx, prepared)
}

/// Complete one full `CallMethodValue` in the VM after the direct-call
/// prepare reported an ineligible resolution. `0` = destination written and
/// the compiled caller continues, `1` = threw (including a resolved missing or
/// non-callable method), `2` = an exotic receiver whose bespoke opcode branch
/// has not started and must run in the interpreter (exact side exit).
pub(crate) extern "C" fn jit_call_method_generic_stub(
    ctx: *mut JitCtx,
    dst: u64,
    recv: u64,
    name_idx: u64,
    site: u64,
    argc: u64,
    packed_args: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let Some(activation) = ctx.checked_activation() else {
        return 2;
    };
    let vm = unsafe { &mut *activation.vm_ptr() };
    let stack = unsafe { &mut *activation.stack_ptr() };
    let context = unsafe { &*activation.context_ptr() };
    let mut inline_args = [0u16; crate::entry::MAX_METHOD_ARGS];
    let args = crate::entry::decode_packed_arg_regs(argc as usize, packed_args, &mut inline_args);
    match vm.jit_runtime_call_method_in_place(
        context,
        stack,
        ctx.frame_index,
        dst as u16,
        recv as u16,
        name_idx as u32,
        site as usize,
        args,
        ctx.regs.cast::<otter_vm::Value>(),
    ) {
        Ok(true) => 0,
        Ok(false) => 2,
        Err(err) => match try_resume_caller_throw(ctx, matches!(err, VmError::Uncaught)) {
            Ok(true) => 2,
            Ok(false) => {
                park_jit_error(ctx, err);
                1
            }
            Err(unwind_err) => {
                park_jit_error(ctx, unwind_err);
                1
            }
        },
    }
}

/// Complete one full plain `Call` in the VM after the direct-call prepare
/// reported an ineligible callee. `0` = destination written and the compiled
/// caller continues, `1` = threw, `2` = non-callable (exact side exit; the
/// interpreter owns the thrown error).
pub(crate) extern "C" fn jit_call_generic_stub(
    ctx: *mut JitCtx,
    dst: u64,
    callee: u64,
    argc: u64,
    packed_args: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let Some(activation) = ctx.checked_activation() else {
        return 2;
    };
    let vm = unsafe { &mut *activation.vm_ptr() };
    let context = unsafe { &*activation.context_ptr() };
    let mut inline_args = [0u16; crate::entry::MAX_METHOD_ARGS];
    let args = crate::entry::decode_packed_arg_regs(argc as usize, packed_args, &mut inline_args);
    match vm.jit_runtime_call_in_place(
        context,
        dst as u16,
        callee as u16,
        args,
        ctx.regs.cast::<otter_vm::Value>(),
    ) {
        Ok(true) => 0,
        Ok(false) => 2,
        Err(err) => match try_resume_caller_throw(ctx, matches!(err, VmError::Uncaught)) {
            Ok(true) => 2,
            Ok(false) => {
                park_jit_error(ctx, err);
                1
            }
            Err(unwind_err) => {
                park_jit_error(ctx, unwind_err);
                1
            }
        },
    }
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
    // SAFETY: direct callees share the caller's initialized error slot.
    let can_resume = unsafe { &*ctx.error }
        .as_ref()
        .is_some_and(|err| matches!(err, VmError::Uncaught));
    let resume = try_resume_caller_throw(ctx, can_resume);
    let vm = unsafe { &mut *ctx.activation().vm_ptr() };
    let stack = unsafe { &mut *ctx.activation().stack_ptr() };
    vm.jit_abort_direct_call(stack, callee_frame_index as usize);
    match resume {
        Ok(true) => {
            // SAFETY: this throw was absorbed by the caller handler.
            unsafe {
                *ctx.error = None;
            }
            2
        }
        Ok(false) => 0,
        Err(err) => {
            park_jit_error(ctx, err);
            1
        }
    }
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
