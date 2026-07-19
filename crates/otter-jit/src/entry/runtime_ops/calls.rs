//! Native activation and generated-call deoptimization entries.
//!
//! # Contents
//! - Native activation publication and release for nested compiled calls.
//! - Closure validation for scratch-frame inline calls.
//! - Generated-call cold deoptimization.
//!
//! # Invariants
//! - Every entry receives a live [`JitCtx`] whose frame/register roots remain
//!   published across allocating or reentrant VM work.
//! - JavaScript exceptions are parked in the shared context slot and never
//!   unwind through generated machine frames.
//!
//! # See also
//! - `super::reentry` — shared throw resumption and error parking.
//! - `otter_vm::interp::jit_call` — VM-owned generated-call accounting.

use otter_vm::VmError;

use super::super::{JitCtx, JitRet, STATUS_RETURNED, STATUS_THREW};
use super::reentry::park_jit_error;

/// Publish one machine-constructed [`JitCtx`] before its compiled entry can
/// reach an allocating/reentrant safepoint. Returns `0` on success and parks a
/// stack-overflow error in the shared slot on failure.
pub(crate) extern "C" fn jit_push_native_activation_stub(ctx: *mut JitCtx) -> u64 {
    // SAFETY: the caller has fully initialized `ctx` on its native stack and
    // keeps it live until the matching pop stub.
    let ctx = unsafe { &mut *ctx };
    let vm = unsafe { &mut *ctx.activation().vm_ptr() };
    let Some(frame) = (unsafe { ctx.native_frame.as_mut() }) else {
        park_jit_error(ctx, VmError::InvalidOperand);
        return 1;
    };
    // SAFETY: the complete canonical frame remains live on the native stack
    // until the matching pop; the VM publishes and traces it as one unit.
    match unsafe { vm.jit_push_native_frame(frame) } {
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
    let callee = match ctx
        .active_frame_mut()
        .and_then(|frame| frame.read(callee_reg as u16))
    {
        Ok(callee) => callee,
        Err(_) => return 0,
    };
    let vm = unsafe { &mut *ctx.activation().vm_ptr() };
    vm.jit_inline_closure_upvalues(callee, expected_fid as u32)
        .unwrap_or(0)
}

/// Cold deoptimization for one already-started generated stack call.
///
/// The native frame stays published for this entire transition. The VM copies
/// its tagged window into interpreter-owned storage, resumes at the exact
/// published PC, and returns the completed value without replaying `Call`.
pub(crate) extern "C" fn jit_deopt_stack_call_stub(
    ctx: *mut JitCtx,
    callee_frame: *const otter_vm::native_abi::NativeFrame,
    caller_function_id: u64,
    caller_call_pc: u64,
    callee_code_object_id: u64,
    caller_code_object_id: u64,
    call_kind: u64,
) -> JitRet {
    // SAFETY: generated linkage keeps both context and stack frame live and
    // published until this function returns.
    let ctx = unsafe { &mut *ctx };
    let Some(callee_frame) = (unsafe { callee_frame.as_ref() }).copied() else {
        park_jit_error(ctx, VmError::InvalidOperand);
        return JitRet {
            value: 0,
            status: STATUS_THREW,
        };
    };
    let (Ok(caller_function_id), Ok(caller_call_pc)) = (
        u32::try_from(caller_function_id),
        u32::try_from(caller_call_pc),
    ) else {
        park_jit_error(ctx, VmError::InvalidOperand);
        return JitRet {
            value: 0,
            status: STATUS_THREW,
        };
    };
    let call_kind = match call_kind {
        0 => otter_vm::JitDirectCallKind::Plain,
        1 => otter_vm::JitDirectCallKind::Method,
        _ => {
            park_jit_error(ctx, VmError::InvalidOperand);
            return JitRet {
                value: 0,
                status: STATUS_THREW,
            };
        }
    };
    let activation = ctx.activation();
    let vm = unsafe { &mut *activation.vm_ptr() };
    let stack = unsafe { &mut *activation.stack_ptr() };
    let context = unsafe { &*activation.context_ptr() };
    match vm.jit_deopt_materialize_stack_call(
        context,
        stack,
        callee_frame,
        caller_function_id,
        caller_call_pc,
        callee_code_object_id,
        caller_code_object_id,
        call_kind,
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
