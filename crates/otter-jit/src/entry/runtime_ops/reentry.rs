//! VM re-entry stubs for exceptions and non-call runtime operations.
//!
//! # Contents
//! - Propagated-throw resumption in live compiled callers.
//! - Reentrant construction and closure/function creation.
//! - Reentrant equality, numeric-family, and unary-coercion completion.
//! - Cooperative backedge polling.
//!
//! # Invariants
//! Every entry receives a live JIT context whose frame/register roots are
//! published for the entire call. Errors are parked in the shared context slot.
//!
//! # See also
//! - `super::super::abi` — machine-visible entry context.
//! - `super::calls` — plain/method-call adapters and direct-call lifecycle.

use otter_vm::{JitExceptionOutcome, VmError};

use super::super::{
    JitCtx, JitRet, STATUS_BAILED, STATUS_CONTINUE, STATUS_RETURNED, STATUS_THREW,
};

pub(crate) fn park_jit_error(ctx: &mut JitCtx, err: VmError) {
    // SAFETY: every `JitCtx` is built with an initialized error slot that lives
    // for the compiled entry's dynamic extent; nested direct-call contexts copy
    // the same pointer.
    unsafe {
        *ctx.error = Some(err);
    }
}

/// Try to deliver an uncaught compiled-callee throw to the current compiled
/// caller. On success, publish the catch/finally PC so status `2` from a call
/// bridge becomes a committed bailout rather than a replay of the call site.
pub(super) fn try_resume_caller_throw(
    ctx: &mut JitCtx,
    is_uncaught: bool,
) -> Result<bool, VmError> {
    if !is_uncaught {
        return Ok(false);
    }
    let activation = ctx.checked_activation().ok_or(VmError::InvalidOperand)?;
    let vm = unsafe { &mut *activation.vm_ptr() };
    let context = unsafe { &*activation.context_ptr() };
    let stack = unsafe { &mut *activation.stack_ptr() };
    let Some(pc) = vm.jit_resume_caller_throw(context, stack, ctx.frame_index)? else {
        return Ok(false);
    };
    // SAFETY: runtime-capable JIT contexts always publish the current native
    // frame for the full compiled entry dynamic extent.
    let native_frame = unsafe { ctx.native_frame.as_mut() }.ok_or(VmError::InvalidOperand)?;
    native_frame.header.pc = pc;
    Ok(true)
}

/// Convert an interpreter-style catchable VM error into its JavaScript error
/// object and publish the selected catch/finally continuation when present.
fn try_materialize_compiled_error(ctx: &mut JitCtx, err: VmError) -> Result<bool, VmError> {
    let activation = ctx.checked_activation().ok_or(VmError::InvalidOperand)?;
    let vm = unsafe { &mut *activation.vm_ptr() };
    let context = unsafe { &*activation.context_ptr() };
    let stack = unsafe { &mut *activation.stack_ptr() };
    let Some(pc) = vm.jit_materialize_error_from_compiled(context, stack, ctx.frame_index, err)?
    else {
        return Ok(false);
    };
    // SAFETY: runtime-capable JIT contexts publish this native frame for the
    // full compiled entry dynamic extent.
    let native_frame = unsafe { ctx.native_frame.as_mut() }.ok_or(VmError::InvalidOperand)?;
    native_frame.header.pc = pc;
    Ok(true)
}

/// Rebuild an inlined callee's interpreter frame at a deopt exit.
///
/// The optimized code has already restored the caller's registers into its
/// window; this hands the caller back to the interpreter's own `Op::Call` path
/// at `call_pc`, so the frame the interpreter gets is exactly the one a real
/// call would have built, fast-forwarded to `callee_pc`. The emitted code then
/// restores the callee's registers into the returned window.
///
/// Returns the new frame's register-window base, or `0` when the call path
/// raised — a stack overflow the interpreter would have raised at this same
/// call — with the error parked for the throw epilogue.
pub(crate) extern "C" fn jit_deopt_reify_frame_stub(
    ctx: *mut JitCtx,
    call_pc: u64,
    callee_pc: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let vm = unsafe { &mut *ctx.activation().vm_ptr() };
    let stack = unsafe { &mut *ctx.activation().stack_ptr() };
    let context = unsafe { &*ctx.activation().context_ptr() };
    match unsafe {
        vm.jit_deopt_reify_inlined_frame(context, stack, call_pc as u32, callee_pc as u32)
    } {
        Ok(registers) => registers as u64,
        Err(err) => {
            park_jit_error(ctx, err);
            0
        }
    }
}

/// Shared throw-epilogue resolver.
///
/// A value transition (property/element/global/loose-equality/coercion resolves
/// its miss in the VM and, on a JavaScript throw, parks the error and branches
/// to the compiled frame's throw epilogue. Before the epilogue propagates the
/// throw to its caller, this delivers the parked error to the frame's own
/// structured-exception handlers exactly as an interpreted throw at that PC
/// would: a `try` in the same compiled function then catches it. Returns
/// [`STATUS_BAILED`] with the frame's published PC advanced to the catch or
/// finally continuation when a local handler takes the throw, and
/// [`STATUS_THREW`] (error re-parked) when it escapes this frame.
pub(crate) extern "C" fn jit_resolve_threw_stub(ctx: *mut JitCtx) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract; the error slot is initialized
    // for the compiled entry's dynamic extent.
    let ctx = unsafe { &mut *ctx };
    let Some(err) = (unsafe { (*ctx.error).take() }) else {
        return STATUS_THREW;
    };
    // A parked `Uncaught` names a value already staged in `pending_uncaught_throw`
    // by a deeper frame; anything else is a fresh VM error to materialize here.
    let materialized = if matches!(err, VmError::Uncaught) {
        try_resume_caller_throw(ctx, true)
    } else {
        try_materialize_compiled_error(ctx, err)
    };
    match materialized {
        Ok(true) => STATUS_BAILED,
        Ok(false) => {
            park_jit_error(ctx, err);
            STATUS_THREW
        }
        Err(unwind_err) => {
            park_jit_error(ctx, unwind_err);
            STATUS_THREW
        }
    }
}

/// Complete one structured-exception opcode, including TDZ `ReferenceError`
/// materialization. Unlike ordinary status-word transitions this returns the
/// full compiled-entry pair: a committed handler mutation may continue, resume
/// at a dynamic logical PC, return a value, or propagate a parked error.
pub(crate) extern "C" fn jit_exception_op_stub(
    ctx: *mut JitCtx,
    opcode: u64,
    arg0: u64,
    arg1: u64,
    arg2: u64,
) -> JitRet {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let vm = unsafe { &mut *ctx.activation().vm_ptr() };
    let stack = unsafe { &mut *ctx.activation().stack_ptr() };
    let context = unsafe { &*ctx.activation().context_ptr() };
    match vm.jit_runtime_exception_op(
        context,
        stack,
        ctx.frame_index,
        opcode as u8,
        arg0,
        arg1,
        arg2,
    ) {
        Ok(JitExceptionOutcome::Continue) => JitRet {
            value: 0,
            status: STATUS_CONTINUE,
        },
        Ok(JitExceptionOutcome::Resume(pc)) => JitRet {
            value: u64::from(pc),
            status: STATUS_BAILED,
        },
        Ok(JitExceptionOutcome::Return(value)) => JitRet {
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

/// Complete one iterator-lifecycle opcode. `0` means the VM committed the
/// opcode and the template may fall through; `1` reports a parked throw; `2`
/// is reserved for an absent activation and therefore remains an exact
/// pre-effect side exit.
pub(crate) extern "C" fn jit_iterator_op_stub(
    ctx: *mut JitCtx,
    opcode: u64,
    arg0: u64,
    arg1: u64,
    arg2: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let Some(activation) = ctx.checked_activation() else {
        return STATUS_BAILED;
    };
    let vm = unsafe { &mut *activation.vm_ptr() };
    let stack = unsafe { &mut *activation.stack_ptr() };
    let context = unsafe { &*activation.context_ptr() };
    match vm.jit_runtime_iterator_op(
        context,
        stack,
        ctx.frame_index,
        opcode as u8,
        arg0,
        arg1,
        arg2,
    ) {
        Ok(()) => 0,
        Err(err) => {
            park_jit_error(ctx, err);
            STATUS_THREW
        }
    }
}

/// Complete one static intrinsic-call opcode (`ArrayBufferCall`,
/// `SharedArrayBufferCall`, `BigIntCall`, `DataViewCall`). `0` means the VM
/// committed the opcode and the template may fall through; `1` reports a parked
/// throw; `2` remains an exact pre-effect side exit for an absent activation.
pub(crate) extern "C" fn jit_static_call_op_stub(
    ctx: *mut JitCtx,
    opcode: u64,
    packed_head: u64,
    method: u64,
    packed_args: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let Some(activation) = ctx.checked_activation() else {
        return STATUS_BAILED;
    };
    let vm = unsafe { &mut *activation.vm_ptr() };
    let stack = unsafe { &mut *activation.stack_ptr() };
    let context = unsafe { &*activation.context_ptr() };
    match vm.jit_runtime_static_call_op(
        context,
        stack,
        ctx.frame_index,
        opcode as u8,
        packed_head,
        method,
        packed_args,
    ) {
        Ok(()) => 0,
        Err(err) => {
            park_jit_error(ctx, err);
            STATUS_THREW
        }
    }
}

/// Complete one dynamic control-family opcode (`LoadShadowedUpvalue`). `0`
/// means the VM committed the opcode and the template may fall through;
/// `STATUS_BAILED` is an absent-activation pre-effect side exit and
/// `STATUS_THREW` reports a parked error.
pub(crate) extern "C" fn jit_control_op_stub(
    ctx: *mut JitCtx,
    opcode: u64,
    arg0: u64,
    arg1: u64,
    arg2: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let Some(activation) = ctx.checked_activation() else {
        return STATUS_BAILED;
    };
    let vm = unsafe { &mut *activation.vm_ptr() };
    let stack = unsafe { &mut *activation.stack_ptr() };
    let context = unsafe { &*activation.context_ptr() };
    match vm.jit_runtime_control_op(
        context,
        stack,
        ctx.frame_index,
        opcode as u8,
        arg0,
        arg1,
        arg2,
    ) {
        Ok(()) => 0,
        Err(err) => {
            park_jit_error(ctx, err);
            STATUS_THREW
        }
    }
}

/// Complete one spread/call-family opcode. A synchronous callee throw is
/// first offered to the live compiled caller's handler; successful resumption
/// publishes that handler PC and returns `STATUS_BAILED` so the call site is
/// never replayed.
pub(crate) extern "C" fn jit_spread_call_op_stub(
    ctx: *mut JitCtx,
    opcode: u64,
    arg0: u64,
    arg1: u64,
    arg2: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let Some(activation) = ctx.checked_activation() else {
        return STATUS_BAILED;
    };
    let vm = unsafe { &mut *activation.vm_ptr() };
    let stack = unsafe { &mut *activation.stack_ptr() };
    let context = unsafe { &*activation.context_ptr() };
    match vm.jit_runtime_spread_call_op(
        context,
        stack,
        ctx.frame_index,
        opcode as u8,
        arg0,
        arg1,
        arg2,
    ) {
        Ok(()) => 0,
        Err(err) => match try_resume_caller_throw(ctx, matches!(err, VmError::Uncaught)) {
            Ok(true) => STATUS_BAILED,
            Ok(false) => {
                park_jit_error(ctx, err);
                STATUS_THREW
            }
            Err(unwind_err) => {
                park_jit_error(ctx, unwind_err);
                STATUS_THREW
            }
        },
    }
}

/// Complete one class/value-family opcode. Dynamic evaluation, function
/// construction, and numeric coercion may synchronously throw from JavaScript;
/// offer uncaught values to the compiled caller before parking the error.
pub(crate) extern "C" fn jit_class_value_op_stub(
    ctx: *mut JitCtx,
    opcode: u64,
    arg0: u64,
    arg1: u64,
    arg2: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let Some(activation) = ctx.checked_activation() else {
        return STATUS_BAILED;
    };
    let vm = unsafe { &mut *activation.vm_ptr() };
    let stack = unsafe { &mut *activation.stack_ptr() };
    let context = unsafe { &*activation.context_ptr() };
    match vm.jit_runtime_class_value_op(
        context,
        stack,
        ctx.frame_index,
        opcode as u8,
        arg0,
        arg1,
        arg2,
    ) {
        Ok(()) => 0,
        Err(err) => {
            let materialized = if matches!(err, VmError::Uncaught) {
                try_resume_caller_throw(ctx, true)
            } else {
                try_materialize_compiled_error(ctx, err)
            };
            match materialized {
                Ok(true) => STATUS_BAILED,
                Ok(false) => {
                    park_jit_error(ctx, err);
                    STATUS_THREW
                }
                Err(unwind_err) => {
                    park_jit_error(ctx, unwind_err);
                    STATUS_THREW
                }
            }
        }
    }
}

/// Complete one synchronous module-family opcode. Promise-producing module
/// evaluation/import opcodes never call this stub and remain exact side exits.
pub(crate) extern "C" fn jit_module_op_stub(
    ctx: *mut JitCtx,
    opcode: u64,
    arg0: u64,
    arg1: u64,
    arg2: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let Some(activation) = ctx.checked_activation() else {
        return STATUS_BAILED;
    };
    let vm = unsafe { &mut *activation.vm_ptr() };
    let stack = unsafe { &mut *activation.stack_ptr() };
    let context = unsafe { &*activation.context_ptr() };
    match vm.jit_runtime_module_op(
        context,
        stack,
        ctx.frame_index,
        opcode as u8,
        arg0,
        arg1,
        arg2,
    ) {
        Ok(()) => 0,
        Err(err) => {
            let materialized = if matches!(err, VmError::Uncaught) {
                try_resume_caller_throw(ctx, true)
            } else {
                try_materialize_compiled_error(ctx, err)
            };
            match materialized {
                Ok(true) => STATUS_BAILED,
                Ok(false) => {
                    park_jit_error(ctx, err);
                    STATUS_THREW
                }
                Err(unwind_err) => {
                    park_jit_error(ctx, unwind_err);
                    STATUS_THREW
                }
            }
        }
    }
}

/// Complete one variadic construction opcode (`ArrayConstruct`, `ArrayFrom`,
/// `ArrayOf`, `QueueMicrotask`). `0` means the VM committed the opcode and the
/// template may fall through; `1` reports a parked throw; `2` remains an exact
/// pre-effect side exit for an absent activation.
pub(crate) extern "C" fn jit_variadic_op_stub(
    ctx: *mut JitCtx,
    opcode: u64,
    prefix: u64,
    count: u64,
    packed_args: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let Some(activation) = ctx.checked_activation() else {
        return STATUS_BAILED;
    };
    let vm = unsafe { &mut *activation.vm_ptr() };
    let stack = unsafe { &mut *activation.stack_ptr() };
    let context = unsafe { &*activation.context_ptr() };
    match vm.jit_runtime_variadic_op(
        context,
        stack,
        ctx.frame_index,
        opcode as u8,
        prefix,
        count,
        packed_args,
    ) {
        Ok(()) => 0,
        Err(err) => {
            park_jit_error(ctx, err);
            STATUS_THREW
        }
    }
}

/// Complete one class-construction opcode (`BindThisValue`, `ClassCheck`,
/// `SetFunctionName`). `0` means the VM committed the opcode and the template
/// may fall through; `1` reports a parked throw; `2` remains an exact pre-effect
/// side exit for an absent activation.
pub(crate) extern "C" fn jit_class_op_stub(
    ctx: *mut JitCtx,
    opcode: u64,
    arg0: u64,
    arg1: u64,
    arg2: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let Some(activation) = ctx.checked_activation() else {
        return STATUS_BAILED;
    };
    let vm = unsafe { &mut *activation.vm_ptr() };
    let stack = unsafe { &mut *activation.stack_ptr() };
    let context = unsafe { &*activation.context_ptr() };
    match vm.jit_runtime_class_op(
        context,
        stack,
        ctx.frame_index,
        opcode as u8,
        arg0,
        arg1,
        arg2,
    ) {
        Ok(()) => 0,
        Err(err) => {
            park_jit_error(ctx, err);
            STATUS_THREW
        }
    }
}

/// Complete one structural object opcode (`ForInKeys`, `CopyDataProperties`).
/// `0` means the VM committed the opcode and the template may fall through; `1`
/// reports a parked throw; `2` remains an exact pre-effect side exit for an
/// absent activation.
pub(crate) extern "C" fn jit_structural_op_stub(
    ctx: *mut JitCtx,
    opcode: u64,
    arg0: u64,
    arg1: u64,
    _reserved: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let Some(activation) = ctx.checked_activation() else {
        return STATUS_BAILED;
    };
    let vm = unsafe { &mut *activation.vm_ptr() };
    let stack = unsafe { &mut *activation.stack_ptr() };
    let context = unsafe { &*activation.context_ptr() };
    match vm.jit_runtime_structural_op(context, stack, ctx.frame_index, opcode as u8, arg0, arg1) {
        Ok(()) => 0,
        Err(err) => {
            park_jit_error(ctx, err);
            STATUS_THREW
        }
    }
}

/// Complete one allocating-construction opcode (`CollectRest`, `NewError`,
/// `NewBuiltinError`, `ArrayPush`). `0` means the VM committed the opcode and the
/// template may fall through; `1` reports a parked throw; `2` remains an exact
/// pre-effect side exit for an absent activation.
pub(crate) extern "C" fn jit_construct_op_stub(
    ctx: *mut JitCtx,
    opcode: u64,
    arg0: u64,
    arg1: u64,
    arg2: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let Some(activation) = ctx.checked_activation() else {
        return STATUS_BAILED;
    };
    let vm = unsafe { &mut *activation.vm_ptr() };
    let stack = unsafe { &mut *activation.stack_ptr() };
    let context = unsafe { &*activation.context_ptr() };
    match vm.jit_runtime_construct_op(
        context,
        stack,
        ctx.frame_index,
        opcode as u8,
        arg0,
        arg1,
        arg2,
    ) {
        Ok(()) => 0,
        Err(err) => {
            park_jit_error(ctx, err);
            STATUS_THREW
        }
    }
}

/// Complete one static value-load opcode (`MathLoad`, `SymbolLoad`,
/// `TemporalLoad`, `LoadBigInt`, `GetStringIndex`). `0` means the VM committed
/// the opcode and the template may fall through; `1` reports a parked throw; `2`
/// remains an exact pre-effect side exit for an absent activation.
pub(crate) extern "C" fn jit_value_load_op_stub(
    ctx: *mut JitCtx,
    opcode: u64,
    arg0: u64,
    arg1: u64,
    arg2: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let Some(activation) = ctx.checked_activation() else {
        return STATUS_BAILED;
    };
    let vm = unsafe { &mut *activation.vm_ptr() };
    let stack = unsafe { &mut *activation.stack_ptr() };
    let context = unsafe { &*activation.context_ptr() };
    match vm.jit_runtime_value_load_op(
        context,
        stack,
        ctx.frame_index,
        opcode as u8,
        arg0,
        arg1,
        arg2,
    ) {
        Ok(()) => 0,
        Err(err) => {
            park_jit_error(ctx, err);
            STATUS_THREW
        }
    }
}

/// Complete one private-member opcode (`PrivateGet`, `PrivateSet`,
/// `PrivateBrandCheck`). `0` means the VM committed the opcode and the template
/// may fall through; `1` reports a parked throw; `2` remains an exact pre-effect
/// side exit for an absent activation.
pub(crate) extern "C" fn jit_private_op_stub(
    ctx: *mut JitCtx,
    opcode: u64,
    arg0: u64,
    arg1: u64,
    arg2: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let Some(activation) = ctx.checked_activation() else {
        return STATUS_BAILED;
    };
    let vm = unsafe { &mut *activation.vm_ptr() };
    let stack = unsafe { &mut *activation.stack_ptr() };
    let context = unsafe { &*activation.context_ptr() };
    match vm.jit_runtime_private_op(
        context,
        stack,
        ctx.frame_index,
        opcode as u8,
        arg0,
        arg1,
        arg2,
    ) {
        Ok(()) => 0,
        Err(err) => {
            park_jit_error(ctx, err);
            STATUS_THREW
        }
    }
}

/// Complete one `super` property opcode (`LoadSuperProperty`,
/// `LoadSuperElement`, `SetSuperProperty`, `SetSuperElement`). `0` means the VM
/// committed the opcode and the template may fall through; `1` reports a parked
/// throw; `2` remains an exact pre-effect side exit for an absent activation.
pub(crate) extern "C" fn jit_super_op_stub(
    ctx: *mut JitCtx,
    opcode: u64,
    arg0: u64,
    arg1: u64,
    arg2: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let Some(activation) = ctx.checked_activation() else {
        return STATUS_BAILED;
    };
    let vm = unsafe { &mut *activation.vm_ptr() };
    let stack = unsafe { &mut *activation.stack_ptr() };
    let context = unsafe { &*activation.context_ptr() };
    match vm.jit_runtime_super_op(
        context,
        stack,
        ctx.frame_index,
        opcode as u8,
        arg0,
        arg1,
        arg2,
    ) {
        Ok(()) => 0,
        Err(err) => {
            park_jit_error(ctx, err);
            STATUS_THREW
        }
    }
}

/// Complete one scalar value-query/coercion opcode (`ToObject`,
/// `ToPropertyKey`, `TypeOf`, `LoadNewTarget`, `SameValue`, `IsArray`,
/// `ArrayLength`, `LoadLength`). `0` means the VM committed the opcode and the
/// template may fall through; `1` reports a parked throw; `2` remains an exact
/// pre-effect side exit for an absent activation.
pub(crate) extern "C" fn jit_scalar_op_stub(
    ctx: *mut JitCtx,
    opcode: u64,
    arg0: u64,
    arg1: u64,
    arg2: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let Some(activation) = ctx.checked_activation() else {
        return STATUS_BAILED;
    };
    let vm = unsafe { &mut *activation.vm_ptr() };
    let stack = unsafe { &mut *activation.stack_ptr() };
    let context = unsafe { &*activation.context_ptr() };
    match vm.jit_runtime_scalar_op(
        context,
        stack,
        ctx.frame_index,
        opcode as u8,
        arg0,
        arg1,
        arg2,
    ) {
        Ok(()) => 0,
        Err(err) => {
            park_jit_error(ctx, err);
            STATUS_THREW
        }
    }
}

/// Complete one `delete` opcode (`DeleteProperty`, `DeleteElement`,
/// `DeleteDynamic`). `0` means the VM committed the opcode and the template may
/// fall through; `1` reports a parked throw; `2` remains an exact pre-effect
/// side exit for an absent activation.
pub(crate) extern "C" fn jit_delete_op_stub(
    ctx: *mut JitCtx,
    opcode: u64,
    arg0: u64,
    arg1: u64,
    arg2: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let Some(activation) = ctx.checked_activation() else {
        return STATUS_BAILED;
    };
    let vm = unsafe { &mut *activation.vm_ptr() };
    let stack = unsafe { &mut *activation.stack_ptr() };
    let context = unsafe { &*activation.context_ptr() };
    match vm.jit_runtime_delete_op(
        context,
        stack,
        ctx.frame_index,
        opcode as u8,
        arg0,
        arg1,
        arg2,
    ) {
        Ok(()) => 0,
        Err(err) => {
            park_jit_error(ctx, err);
            STATUS_THREW
        }
    }
}

/// Complete one object property-protocol opcode (`Instanceof`, `HasProperty`,
/// `GetPrototype`, `SetPrototype`). `0` means the VM committed the opcode and
/// the template may fall through; `1` reports a parked throw; `2` remains an
/// exact pre-effect side exit for an absent activation.
pub(crate) extern "C" fn jit_object_protocol_op_stub(
    ctx: *mut JitCtx,
    opcode: u64,
    arg0: u64,
    arg1: u64,
    arg2: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let Some(activation) = ctx.checked_activation() else {
        return STATUS_BAILED;
    };
    let vm = unsafe { &mut *activation.vm_ptr() };
    let stack = unsafe { &mut *activation.stack_ptr() };
    let context = unsafe { &*activation.context_ptr() };
    match vm.jit_runtime_object_protocol_op(
        context,
        stack,
        ctx.frame_index,
        opcode as u8,
        arg0,
        arg1,
        arg2,
    ) {
        Ok(()) => 0,
        Err(err) => {
            park_jit_error(ctx, err);
            STATUS_THREW
        }
    }
}

/// Complete one global-access opcode (`LoadGlobalThis`,
/// `LoadGlobalOrUndefined`, `StoreGlobalBinding`, `StoreGlobalChecked`). `0`
/// means the VM committed the opcode and the template may fall through; `1`
/// reports a parked throw; `2` remains an exact pre-effect side exit for an
/// absent activation.
pub(crate) extern "C" fn jit_global_op_stub(
    ctx: *mut JitCtx,
    opcode: u64,
    arg0: u64,
    arg1: u64,
    arg2: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let Some(activation) = ctx.checked_activation() else {
        return STATUS_BAILED;
    };
    let vm = unsafe { &mut *activation.vm_ptr() };
    let stack = unsafe { &mut *activation.stack_ptr() };
    let context = unsafe { &*activation.context_ptr() };
    match vm.jit_runtime_global_op(
        context,
        stack,
        ctx.frame_index,
        opcode as u8,
        arg0,
        arg1,
        arg2,
    ) {
        Ok(()) => 0,
        Err(err) => {
            park_jit_error(ctx, err);
            STATUS_THREW
        }
    }
}

/// Complete one `Op::BindFunction`. `packed_meta` is
/// `dst | callee<<16 | this<<32 | argc<<48`; `packed_args` holds the bound-arg
/// registers. `0` means the VM committed the bind and the template may fall
/// through; `1` reports a parked throw; `2` remains an exact pre-effect side
/// exit for an absent activation.
pub(crate) extern "C" fn jit_bind_function_stub(
    ctx: *mut JitCtx,
    packed_meta: u64,
    packed_args: u64,
    _reserved0: u64,
    _reserved1: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let Some(activation) = ctx.checked_activation() else {
        return STATUS_BAILED;
    };
    let vm = unsafe { &mut *activation.vm_ptr() };
    let stack = unsafe { &mut *activation.stack_ptr() };
    let context = unsafe { &*activation.context_ptr() };
    match vm.jit_runtime_bind_function(context, stack, ctx.frame_index, packed_meta, packed_args) {
        Ok(()) => 0,
        Err(err) => {
            park_jit_error(ctx, err);
            STATUS_THREW
        }
    }
}

/// Complete one full `Op::New` construct in the VM. `0` = destination
/// written and the compiled caller continues, `1` = threw, `2` = a
/// non-constructor callee or no live activation (exact side exit; the
/// interpreter owns the thrown error).
pub(crate) extern "C" fn jit_construct_stub(
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
    match vm.jit_runtime_construct_in_place(
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

/// Complete one full loose-equality opcode in the VM. `0` = destination
/// written, `1` = threw (coercion raised), `2` = no live activation
/// (isolate-less probe harness; exact side exit).
pub(crate) extern "C" fn jit_loose_eq_stub(
    ctx: *mut JitCtx,
    dst: u64,
    lhs: u64,
    rhs: u64,
    negate: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let Some(activation) = ctx.checked_activation() else {
        return 2;
    };
    let vm = unsafe { &mut *activation.vm_ptr() };
    let context = unsafe { &*activation.context_ptr() };
    match vm.jit_runtime_loose_equal_in_place(
        context,
        dst as u16,
        lhs as u16,
        rhs as u16,
        negate != 0,
        ctx.regs.cast::<otter_vm::Value>(),
    ) {
        Ok(()) => 0,
        Err(err) => {
            park_jit_error(ctx, err);
            1
        }
    }
}

/// Complete one `ToPrimitive`/`ToNumeric` opcode in the VM. `0` means the
/// destination was written, `1` means a coercion hook threw, and `2` is
/// reserved for an isolate-less probe context with no published activation.
pub(crate) extern "C" fn jit_coerce_unary_stub(
    ctx: *mut JitCtx,
    dst: u64,
    src: u64,
    numeric: u64,
    hint_index: u64,
    function_id: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let Some(activation) = ctx.checked_activation() else {
        return 2;
    };
    let vm = unsafe { &mut *activation.vm_ptr() };
    let context = unsafe { &*activation.context_ptr() };
    match vm.jit_runtime_coerce_unary_in_place(
        context,
        dst as u16,
        src as u16,
        numeric != 0,
        hint_index as u32,
        function_id as u32,
        ctx.regs.cast::<otter_vm::Value>(),
    ) {
        Ok(()) => 0,
        Err(err) => {
            park_jit_error(ctx, err);
            1
        }
    }
}

/// Complete one numeric-family opcode in the VM. `0` means the destination
/// was committed, `1` means the operation threw, and `2` is reserved for an
/// isolate-less compiled-entry probe where no VM activation exists.
pub(crate) extern "C" fn jit_numeric_op_stub(
    ctx: *mut JitCtx,
    dst: u64,
    lhs: u64,
    rhs_or_delta: u64,
    opcode: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let Some(activation) = ctx.checked_activation() else {
        return 2;
    };
    let vm = unsafe { &mut *activation.vm_ptr() };
    let stack = unsafe { &mut *activation.stack_ptr() };
    let context = unsafe { &*activation.context_ptr() };
    match vm.jit_runtime_numeric_op(
        context,
        stack,
        ctx.frame_index,
        dst as u16,
        lhs as u16,
        rhs_or_delta,
        opcode as u8,
    ) {
        Ok(()) => 0,
        Err(err) => {
            park_jit_error(ctx, err);
            1
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
    let vm_ptr = ctx
        .checked_activation()
        .map_or(std::ptr::null_mut(), |activation| activation.vm_ptr());
    if vm_ptr.is_null() {
        return 0;
    }
    // SAFETY: a published activation names a live interpreter for this entry.
    let vm = unsafe { &mut *vm_ptr };
    match vm.jit_backedge_poll() {
        Ok(()) => 0,
        Err(err) => {
            park_jit_error(ctx, err);
            1
        }
    }
}
