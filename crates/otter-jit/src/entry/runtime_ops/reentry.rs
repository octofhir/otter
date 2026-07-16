//! VM re-entry stubs for exceptions and non-call runtime operations.
//!
//! # Contents
//! - Propagated-throw resumption in live compiled callers.
//! - Reentrant construction and closure/function creation.
//! - Reentrant equality, typed numeric-family, and unary-coercion completion.
//! - Cooperative backedge polling.
//!
//! # Invariants
//! Every entry receives a live JIT context whose canonical
//! [`NativeFrame`](otter_vm::native_abi::NativeFrame) publishes frame/register
//! roots for the entire call. Raw numeric opcodes and unary-coercion modes are
//! decoded exactly once at this ABI edge; coercion hint constants are resolved
//! through the canonical frame owner before VM semantics receive typed
//! requests. Errors are parked in the shared context slot.
//!
//! # See also
//! - `super::super::abi` — machine-visible entry context.
//! - `super::calls` — plain/method-call adapters and direct-call lifecycle.

use otter_vm::{
    JitExceptionOutcome, NumericRuntimeOp, UnaryCoercionOp, UnaryPrimitiveHint, VmError,
};

use super::super::{JitCtx, JitRet, STATUS_BAILED, STATUS_CONTINUE, STATUS_RETURNED, STATUS_THREW};
use super::decode_register;

pub(crate) fn park_jit_error(ctx: &mut JitCtx, err: VmError) {
    // SAFETY: every `JitCtx` is built with an initialized error slot that lives
    // for the compiled entry's dynamic extent; nested direct calls reuse the
    // same context and slot.
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
    let Ok(frame_index) = ctx.materialized_frame_index() else {
        // A frameless caller has no interpreter handler stack yet. Preserve
        // the original throw so the enclosing native-call edge can propagate
        // or materialize it without replacing it with InvalidOperand.
        return Ok(false);
    };
    let activation = ctx.checked_activation().ok_or(VmError::InvalidOperand)?;
    let vm = unsafe { &mut *activation.vm_ptr() };
    let context = unsafe { &*activation.context_ptr() };
    let stack = unsafe { &mut *activation.stack_ptr() };
    let Some(pc) = vm.jit_resume_caller_throw(context, stack, frame_index)? else {
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
    let Ok(frame_index) = ctx.materialized_frame_index() else {
        return Ok(false);
    };
    let activation = ctx.checked_activation().ok_or(VmError::InvalidOperand)?;
    let vm = unsafe { &mut *activation.vm_ptr() };
    let context = unsafe { &*activation.context_ptr() };
    let stack = unsafe { &mut *activation.stack_ptr() };
    let Some(pc) = vm.jit_materialize_error_from_compiled(context, stack, frame_index, err)? else {
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
    let frame_index = match ctx.materialized_frame_index() {
        Ok(index) => index,
        Err(_) => {
            return JitRet {
                value: 0,
                status: STATUS_BAILED,
            };
        }
    };
    let vm = unsafe { &mut *ctx.activation().vm_ptr() };
    let stack = unsafe { &mut *ctx.activation().stack_ptr() };
    let context = unsafe { &*ctx.activation().context_ptr() };
    match vm.jit_runtime_exception_op(context, stack, frame_index, opcode as u8, arg0, arg1, arg2) {
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
    let frame_index = match ctx.materialized_frame_index() {
        Ok(index) => index,
        Err(_) => return STATUS_BAILED,
    };
    let Some(activation) = ctx.checked_activation() else {
        return STATUS_BAILED;
    };
    let vm = unsafe { &mut *activation.vm_ptr() };
    let stack = unsafe { &mut *activation.stack_ptr() };
    let context = unsafe { &*activation.context_ptr() };
    match vm.jit_runtime_iterator_op(context, stack, frame_index, opcode as u8, arg0, arg1, arg2) {
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
    let frame_index = match ctx.materialized_frame_index() {
        Ok(index) => index,
        Err(_) => return STATUS_BAILED,
    };
    let Some(activation) = ctx.checked_activation() else {
        return STATUS_BAILED;
    };
    let vm = unsafe { &mut *activation.vm_ptr() };
    let stack = unsafe { &mut *activation.stack_ptr() };
    let context = unsafe { &*activation.context_ptr() };
    match vm.jit_runtime_static_call_op(
        context,
        stack,
        frame_index,
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
    let frame_index = match ctx.materialized_frame_index() {
        Ok(index) => index,
        Err(_) => return STATUS_BAILED,
    };
    let Some(activation) = ctx.checked_activation() else {
        return STATUS_BAILED;
    };
    let vm = unsafe { &mut *activation.vm_ptr() };
    let stack = unsafe { &mut *activation.stack_ptr() };
    let context = unsafe { &*activation.context_ptr() };
    match vm.jit_runtime_control_op(context, stack, frame_index, opcode as u8, arg0, arg1, arg2) {
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
    let frame_index = match ctx.materialized_frame_index() {
        Ok(index) => index,
        Err(_) => return STATUS_BAILED,
    };
    let Some(activation) = ctx.checked_activation() else {
        return STATUS_BAILED;
    };
    let vm = unsafe { &mut *activation.vm_ptr() };
    let stack = unsafe { &mut *activation.stack_ptr() };
    let context = unsafe { &*activation.context_ptr() };
    match vm.jit_runtime_spread_call_op(context, stack, frame_index, opcode as u8, arg0, arg1, arg2)
    {
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
    let frame_index = match ctx.materialized_frame_index() {
        Ok(index) => index,
        Err(_) => return STATUS_BAILED,
    };
    let Some(activation) = ctx.checked_activation() else {
        return STATUS_BAILED;
    };
    let vm = unsafe { &mut *activation.vm_ptr() };
    let stack = unsafe { &mut *activation.stack_ptr() };
    let context = unsafe { &*activation.context_ptr() };
    match vm.jit_runtime_class_value_op(context, stack, frame_index, opcode as u8, arg0, arg1, arg2)
    {
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
    let frame_index = match ctx.materialized_frame_index() {
        Ok(index) => index,
        Err(_) => return STATUS_BAILED,
    };
    let Some(activation) = ctx.checked_activation() else {
        return STATUS_BAILED;
    };
    let vm = unsafe { &mut *activation.vm_ptr() };
    let stack = unsafe { &mut *activation.stack_ptr() };
    let context = unsafe { &*activation.context_ptr() };
    match vm.jit_runtime_module_op(context, stack, frame_index, opcode as u8, arg0, arg1, arg2) {
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
    let frame_index = match ctx.materialized_frame_index() {
        Ok(index) => index,
        Err(_) => return STATUS_BAILED,
    };
    let Some(activation) = ctx.checked_activation() else {
        return STATUS_BAILED;
    };
    let vm = unsafe { &mut *activation.vm_ptr() };
    let stack = unsafe { &mut *activation.stack_ptr() };
    let context = unsafe { &*activation.context_ptr() };
    match vm.jit_runtime_variadic_op(
        context,
        stack,
        frame_index,
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
    let frame_index = match ctx.materialized_frame_index() {
        Ok(index) => index,
        Err(_) => return STATUS_BAILED,
    };
    let Some(activation) = ctx.checked_activation() else {
        return STATUS_BAILED;
    };
    let vm = unsafe { &mut *activation.vm_ptr() };
    let stack = unsafe { &mut *activation.stack_ptr() };
    let context = unsafe { &*activation.context_ptr() };
    match vm.jit_runtime_class_op(context, stack, frame_index, opcode as u8, arg0, arg1, arg2) {
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
    let frame_index = match ctx.materialized_frame_index() {
        Ok(index) => index,
        Err(_) => return STATUS_BAILED,
    };
    let Some(activation) = ctx.checked_activation() else {
        return STATUS_BAILED;
    };
    let vm = unsafe { &mut *activation.vm_ptr() };
    let stack = unsafe { &mut *activation.stack_ptr() };
    let context = unsafe { &*activation.context_ptr() };
    match vm.jit_runtime_structural_op(context, stack, frame_index, opcode as u8, arg0, arg1) {
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
    let frame_index = match ctx.materialized_frame_index() {
        Ok(index) => index,
        Err(_) => return STATUS_BAILED,
    };
    let Some(activation) = ctx.checked_activation() else {
        return STATUS_BAILED;
    };
    let vm = unsafe { &mut *activation.vm_ptr() };
    let stack = unsafe { &mut *activation.stack_ptr() };
    let context = unsafe { &*activation.context_ptr() };
    match vm.jit_runtime_construct_op(context, stack, frame_index, opcode as u8, arg0, arg1, arg2) {
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
    let frame_index = match ctx.materialized_frame_index() {
        Ok(index) => index,
        Err(_) => return STATUS_BAILED,
    };
    let Some(activation) = ctx.checked_activation() else {
        return STATUS_BAILED;
    };
    let vm = unsafe { &mut *activation.vm_ptr() };
    let stack = unsafe { &mut *activation.stack_ptr() };
    let context = unsafe { &*activation.context_ptr() };
    match vm.jit_runtime_value_load_op(context, stack, frame_index, opcode as u8, arg0, arg1, arg2)
    {
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
    let frame_index = match ctx.materialized_frame_index() {
        Ok(index) => index,
        Err(_) => return STATUS_BAILED,
    };
    let Some(activation) = ctx.checked_activation() else {
        return STATUS_BAILED;
    };
    let vm = unsafe { &mut *activation.vm_ptr() };
    let stack = unsafe { &mut *activation.stack_ptr() };
    let context = unsafe { &*activation.context_ptr() };
    match vm.jit_runtime_private_op(context, stack, frame_index, opcode as u8, arg0, arg1, arg2) {
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
    let frame_index = match ctx.materialized_frame_index() {
        Ok(index) => index,
        Err(_) => return STATUS_BAILED,
    };
    let Some(activation) = ctx.checked_activation() else {
        return STATUS_BAILED;
    };
    let vm = unsafe { &mut *activation.vm_ptr() };
    let stack = unsafe { &mut *activation.stack_ptr() };
    let context = unsafe { &*activation.context_ptr() };
    match vm.jit_runtime_super_op(context, stack, frame_index, opcode as u8, arg0, arg1, arg2) {
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
    let frame_index = match ctx.materialized_frame_index() {
        Ok(index) => index,
        Err(_) => return STATUS_BAILED,
    };
    let Some(activation) = ctx.checked_activation() else {
        return STATUS_BAILED;
    };
    let vm = unsafe { &mut *activation.vm_ptr() };
    let stack = unsafe { &mut *activation.stack_ptr() };
    let context = unsafe { &*activation.context_ptr() };
    match vm.jit_runtime_scalar_op(context, stack, frame_index, opcode as u8, arg0, arg1, arg2) {
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
    let frame_index = match ctx.materialized_frame_index() {
        Ok(index) => index,
        Err(_) => return STATUS_BAILED,
    };
    let Some(activation) = ctx.checked_activation() else {
        return STATUS_BAILED;
    };
    let vm = unsafe { &mut *activation.vm_ptr() };
    let stack = unsafe { &mut *activation.stack_ptr() };
    let context = unsafe { &*activation.context_ptr() };
    match vm.jit_runtime_delete_op(context, stack, frame_index, opcode as u8, arg0, arg1, arg2) {
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
    let frame_index = match ctx.materialized_frame_index() {
        Ok(index) => index,
        Err(_) => return STATUS_BAILED,
    };
    let Some(activation) = ctx.checked_activation() else {
        return STATUS_BAILED;
    };
    let vm = unsafe { &mut *activation.vm_ptr() };
    let stack = unsafe { &mut *activation.stack_ptr() };
    let context = unsafe { &*activation.context_ptr() };
    match vm.jit_runtime_object_protocol_op(
        context,
        stack,
        frame_index,
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
    let frame_index = match ctx.materialized_frame_index() {
        Ok(index) => index,
        Err(_) => return STATUS_BAILED,
    };
    let Some(activation) = ctx.checked_activation() else {
        return STATUS_BAILED;
    };
    let vm = unsafe { &mut *activation.vm_ptr() };
    let stack = unsafe { &mut *activation.stack_ptr() };
    let context = unsafe { &*activation.context_ptr() };
    match vm.jit_runtime_global_op(context, stack, frame_index, opcode as u8, arg0, arg1, arg2) {
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
    let frame_index = match ctx.materialized_frame_index() {
        Ok(index) => index,
        Err(_) => return STATUS_BAILED,
    };
    let Some(activation) = ctx.checked_activation() else {
        return STATUS_BAILED;
    };
    let vm = unsafe { &mut *activation.vm_ptr() };
    let stack = unsafe { &mut *activation.stack_ptr() };
    let context = unsafe { &*activation.context_ptr() };
    match vm.jit_runtime_bind_function(context, stack, frame_index, packed_meta, packed_args) {
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
    let regs = match ctx.register_base() {
        Ok(regs) => regs,
        Err(err) => {
            park_jit_error(ctx, err);
            return 1;
        }
    };
    let Some(activation) = ctx.checked_activation() else {
        return 2;
    };
    let vm = unsafe { &mut *activation.vm_ptr() };
    let context = unsafe { &*activation.context_ptr() };
    let mut inline_args = [0u16; crate::entry::MAX_METHOD_ARGS];
    let args = crate::entry::decode_packed_arg_regs(argc as usize, packed_args, &mut inline_args);
    match vm.jit_runtime_construct_in_place(context, dst as u16, callee as u16, args, regs) {
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
    let regs = match ctx.register_base() {
        Ok(regs) => regs,
        Err(err) => {
            park_jit_error(ctx, err);
            return 1;
        }
    };
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
        regs,
    ) {
        Ok(()) => 0,
        Err(err) => {
            park_jit_error(ctx, err);
            1
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AbiUnaryCoercion {
    ToPrimitive { hint_index: u32 },
    ToNumeric,
}

fn decode_unary_coercion(numeric: u64, hint_index: u64) -> Result<AbiUnaryCoercion, VmError> {
    match numeric {
        0 => Ok(AbiUnaryCoercion::ToPrimitive {
            hint_index: u32::try_from(hint_index).map_err(|_| VmError::InvalidOperand)?,
        }),
        1 => Ok(AbiUnaryCoercion::ToNumeric),
        _ => Err(VmError::InvalidOperand),
    }
}

fn complete_unary_coercion(
    ctx: &mut JitCtx,
    dst: u16,
    src: u16,
    request: AbiUnaryCoercion,
) -> Result<(), VmError> {
    let activation = ctx.checked_activation().ok_or(VmError::InvalidOperand)?;
    let vm_ptr = activation.vm_ptr();
    let context_ptr = activation.context_ptr();
    let mut frame = ctx.active_frame_mut()?;
    let context = unsafe { &*context_ptr };
    let operation = match request {
        AbiUnaryCoercion::ToNumeric => UnaryCoercionOp::ToNumeric,
        AbiUnaryCoercion::ToPrimitive { hint_index } => {
            let token = context
                .string_constant_str_for_function(frame.function_id(), hint_index)
                .ok_or(VmError::InvalidOperand)?;
            let hint = UnaryPrimitiveHint::from_token(token).ok_or(VmError::InvalidOperand)?;
            UnaryCoercionOp::ToPrimitive { hint }
        }
    };
    // SAFETY: the published activation retains both pointers for this compiled
    // entry's dynamic extent. ActiveFrame performs copied reads and a final
    // single-slot commit, with no exclusive register slice spanning coercion.
    unsafe { &mut *vm_ptr }.jit_runtime_coerce_unary(context, &mut frame, dst, src, operation)
}

/// Complete one `ToPrimitive`/`ToNumeric` opcode in the VM. `0` means the
/// destination was written and `1` means ABI decoding or coercion threw. A
/// published canonical activation is part of the operation contract.
pub(crate) extern "C" fn jit_coerce_unary_stub(
    ctx: *mut JitCtx,
    dst: u64,
    src: u64,
    numeric: u64,
    hint_index: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let result = (|| {
        complete_unary_coercion(
            ctx,
            decode_register(dst)?,
            decode_register(src)?,
            decode_unary_coercion(numeric, hint_index)?,
        )
    })();
    match result {
        Ok(()) => 0,
        Err(err) => {
            park_jit_error(ctx, err);
            1
        }
    }
}

fn complete_numeric_op(
    ctx: &mut JitCtx,
    dst: u16,
    lhs: u16,
    operation: NumericRuntimeOp,
) -> Result<(), VmError> {
    let activation = ctx.checked_activation().ok_or(VmError::InvalidOperand)?;
    let vm_ptr = activation.vm_ptr();
    let context_ptr = activation.context_ptr();
    let mut frame = ctx.active_frame_mut()?;
    // SAFETY: the published activation retains both pointers for this compiled
    // entry's dynamic extent. ActiveFrame retains raw validated windows and
    // performs no long-lived register borrow across numeric execution.
    unsafe { &mut *vm_ptr }.jit_runtime_numeric_op(
        unsafe { &*context_ptr },
        &mut frame,
        dst,
        lhs,
        operation,
    )
}

/// Complete one numeric-family opcode in the VM. `0` means the destination
/// was committed and `1` means decoding or execution threw. Unlike the former
/// adapter, this path has no isolate-less/HoltStack bailout mode: a published
/// [`NativeFrame`](otter_vm::native_abi::NativeFrame) and VM activation are
/// part of the runtime-op contract.
pub(crate) extern "C" fn jit_numeric_op_stub(
    ctx: *mut JitCtx,
    dst: u64,
    lhs: u64,
    rhs_or_delta: u64,
    opcode: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let decoded = (|| {
        Ok((
            decode_register(dst)?,
            decode_register(lhs)?,
            NumericRuntimeOp::decode_abi(opcode, rhs_or_delta)?,
        ))
    })();
    let (dst, lhs, operation) = match decoded {
        Ok(decoded) => decoded,
        Err(err) => {
            park_jit_error(ctx, err);
            return 1;
        }
    };
    match complete_numeric_op(ctx, dst, lhs, operation) {
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
    let frame_index = match ctx.materialized_frame_index() {
        Ok(index) => index,
        Err(err) => {
            park_jit_error(ctx, err);
            return 1;
        }
    };
    let vm = unsafe { &mut *ctx.activation().vm_ptr() };
    let stack = unsafe { &mut *ctx.activation().stack_ptr() };
    let context = unsafe { &*ctx.activation().context_ptr() };
    match vm.jit_runtime_make_function(context, stack, frame_index, dst as u16, idx as u32) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use otter_vm::{
        Value,
        native_abi::{NativeFrame, NativeFrameKind, VmFrameHeader, VmThread},
    };

    fn with_frameless_ctx(test: impl FnOnce(&mut JitCtx, &mut Option<VmError>)) {
        let mut registers = [Value::undefined()];
        let mut frame = NativeFrame::new(
            VmFrameHeader {
                function_id: 7,
                code_block_id: 7,
                pc: 0,
                register_count: 1,
                kind: NativeFrameKind::Baseline,
                flags: Default::default(),
            },
            registers.as_mut_ptr() as u64,
            Value::function(7),
            Value::undefined(),
        );
        frame.set_native_owner(41);
        let mut thread = VmThread::empty();
        thread.current_frame = std::ptr::addr_of_mut!(frame) as u64;
        let mut error = None;
        let mut ctx = JitCtx {
            thread: std::ptr::addr_of_mut!(thread),
            native_frame: std::ptr::addr_of_mut!(frame),
            error: std::ptr::addr_of_mut!(error),
            direct_call: std::mem::MaybeUninit::uninit(),
            activation_base: std::ptr::null_mut(),
            activation_top_ptr: std::ptr::null_mut(),
            activation_limit: 0,
        };
        test(&mut ctx, &mut error);
    }

    #[test]
    fn frameless_complex_transition_is_an_exact_pre_effect_bail() {
        with_frameless_ctx(|ctx, error| {
            let status = jit_iterator_op_stub(ctx, 0, 0, 0, 0);
            assert_eq!(status, STATUS_BAILED);
            assert!(error.is_none());
            assert_eq!(ctx.native_owner_id(), Ok(41));
        });
    }

    #[test]
    fn frameless_throw_resolution_preserves_the_original_error() {
        with_frameless_ctx(|ctx, error| {
            *error = Some(VmError::InvalidOperand);
            let status = jit_resolve_threw_stub(ctx);
            assert_eq!(status, STATUS_THREW);
            assert!(matches!(error, Some(VmError::InvalidOperand)));
        });
    }

    #[test]
    fn unary_coercion_mode_is_decoded_once_at_the_abi_edge() {
        assert_eq!(
            decode_unary_coercion(1, u64::MAX).expect("ToNumeric mode"),
            AbiUnaryCoercion::ToNumeric
        );
        assert_eq!(
            decode_unary_coercion(0, 12).expect("ToPrimitive mode"),
            AbiUnaryCoercion::ToPrimitive { hint_index: 12 }
        );
    }

    #[test]
    fn unary_coercion_decoder_rejects_invalid_words() {
        assert!(matches!(
            decode_unary_coercion(2, 0),
            Err(VmError::InvalidOperand)
        ));
        assert!(matches!(
            decode_unary_coercion(0, u64::from(u32::MAX) + 1),
            Err(VmError::InvalidOperand)
        ));
        assert!(matches!(
            decode_register(u64::from(u16::MAX) + 1),
            Err(VmError::InvalidOperand)
        ));
    }
}
