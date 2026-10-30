//! Direct entry of a static native body from generated code.
//!
//! A generated call whose callee is a NativeFunction with a plain static body
//! enters [`native_exit_entry`] through the JavaScript call ABI, with its
//! actual span still on the caller's stack. The entry publishes one exit
//! record — a [`NativeFrameKind::Host`] frame on this native stack that links
//! the generated caller and its exact return address — runs the body over the
//! span in place, and returns the completion straight to the caller. Nothing
//! is classified, copied, or staged in the request mailbox.
//!
//! # Contents
//! - [`native_exit_entry`] — the Rust half of the native entry selector in
//!   [`crate::native_abi::call_trampoline`].
//!
//! # Invariants
//! - The selector proved a `[[Call]]` of a NativeFunction whose kind is
//!   [`crate::native_function::NativeEntryKind::Static`], no handed-over tail
//!   anchor, and room on the native stack; construction, every other kind,
//!   and an overflowing stack take the Host kernel.
//! - A native stamped with another realm, or any call while an extra realm
//!   is active, runs with the native's realm active, as the Host kernel runs
//!   it, on a cold path.
//! - The exit record is the innermost published frame for the whole body.
//!   It traces the callee, the receiver and the actual span in place, and
//!   its one register is the call metadata's receiver slot the body reads,
//!   so no root is registered per call; the
//!   caller's own homes are traced through its safepoint at the recorded
//!   return address. After any collection the record, not a Rust local,
//!   holds the current receiver and callee.
//! - The record is unpublished before returning, also on a throw or a fatal
//!   completion, and failures project while it is still published, so their
//!   stacks name the caller at its call site.
//!
//! # See also
//! - [`super::host_call`] for the Host kernel this entry shortcuts.
//! - [`crate::native_abi::Frame::return_pc_into_caller`] for the anchor.

use crate::Value;
use crate::native_abi::{
    Frame, HostCallKind, JitCtx, NO_CALLER_RETURN_PC, NativeFrameFlags, NativeFrameKind,
    NativeResultPair, VmFrameHeader,
};
use crate::runtime_cx::{NativeCallInfo, NativeContext, NativeCtx};

use super::host_call::{CallerContext, finish_fatal, finish_native_pair};

/// Run the static body of the NativeFunction `callee` as a `[[Call]]` with
/// `receiver` and the `count` actuals at `actuals`; `return_pc` is the
/// generated caller's return address.
///
/// # Safety
/// Only the native entry selector calls this, under its proofs (module
/// docs): `ctx` is the live context of the current entry and `actuals`
/// holds `count` initialized values that stay in place until it returns.
pub(crate) unsafe extern "C" fn native_exit_entry(
    ctx: *mut JitCtx,
    callee: u64,
    receiver: u64,
    count: u32,
    actuals: *mut Value,
    return_pc: u64,
) -> NativeResultPair {
    // SAFETY: the selector's caller owns the context for this call.
    let ctx = unsafe { &mut *ctx };
    let Some(activation) = ctx.checked_activation().copied() else {
        return NativeResultPair::fatal_internal();
    };
    // SAFETY: the published activation owns its VM and stack for this
    // synchronous entry, and nothing else borrows them meanwhile.
    let (Some(vm), Some(stack)) = (unsafe { activation.vm.as_mut() }, unsafe {
        activation.stack.as_mut()
    }) else {
        return NativeResultPair::fatal_internal();
    };
    // SAFETY: as above; the admitted context outlives the entry.
    let admitted = unsafe { activation.context.as_ref() };
    let (callee, receiver) = (Value::from_bits(callee), Value::from_bits(receiver));
    let Some(native) = callee.as_native_function() else {
        return NativeResultPair::fatal_internal();
    };
    let Some(body) = native.static_fn(&vm.gc_heap) else {
        return NativeResultPair::fatal_internal();
    };
    let caller = ctx.native_frame;
    // SAFETY: a non-null frame cell names the published calling record.
    let generated_caller = !caller.is_null() && unsafe { (*caller).code_object_id } != 0;
    // The body reads its receiver from the call metadata; the record's one
    // register is that slot, so the frame walk keeps it current.
    let call_info = NativeCallInfo::call(receiver);
    let mut record = Frame::new(
        VmFrameHeader {
            function_id: HostCallKind::Native as u32,
            pc: 0,
            register_count: 1,
            kind: NativeFrameKind::Host,
            flags: NativeFrameFlags::empty(),
        },
        call_info.this_slot() as u64,
        callee,
        receiver,
    );
    record.set_incoming_arguments(actuals, count);
    record.caller = caller as u64;
    record.caller_return_pc = if generated_caller {
        return_pc
    } else {
        NO_CALLER_RETURN_PC
    };
    let frame = &raw mut record;
    ctx.native_frame = frame;
    let pair = run_body(vm, stack, admitted, ctx, frame, &call_info, native, body);
    ctx.native_frame = caller;
    pair
}

/// The body over the published exit record `frame`, completed into one pair.
#[inline(always)]
fn run_body(
    vm: &mut crate::Interpreter,
    stack: &mut crate::ActivationStack,
    admitted: Option<&crate::ExecutionContext>,
    ctx: &mut JitCtx,
    frame: *mut Frame,
    call_info: &NativeCallInfo,
    native: crate::NativeFunction,
    body: crate::native_function::NativeFastFn,
) -> NativeResultPair {
    if let Err(error) = vm.record_runtime_native_call() {
        return finish_fatal(vm, ctx, error);
    }
    let caller = CallerContext::of_host_frame(admitted, frame);
    // SAFETY: the record stays published for the body and traces its span
    // and receiver in place.
    let (receiver, arguments) = unsafe {
        let record = &*frame;
        let count = record.argument_count as usize;
        let window = if count == 0 {
            crate::ValueWindow::EMPTY
        } else {
            crate::ValueWindow::from_raw_parts(record.actuals, count)
        };
        (record.this_value, window)
    };
    if vm.active_realm_is_extra || native.realm_global(&vm.gc_heap).is_some() {
        return run_in_realm(vm, stack, &caller, ctx, native, body, receiver, arguments);
    }
    let turn = crate::runtime_cx::RuntimeTurn::from_rooted_parts(vm, stack);
    let mut native_ctx =
        NativeCtx::with_native_context(turn, call_info, NativeContext::Caller(&caller));
    let result = body(&mut native_ctx, arguments);
    let (vm, stack) = native_ctx.cx.into_parts();
    match result {
        Ok(value) => NativeResultPair::success(value),
        Err(error) => fail(vm, stack, &caller, ctx, error),
    }
}

/// [`run_body`] with the native's realm active around the body.
#[cold]
#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn run_in_realm(
    vm: &mut crate::Interpreter,
    stack: &mut crate::ActivationStack,
    caller: &CallerContext<'_>,
    ctx: &mut JitCtx,
    native: crate::NativeFunction,
    body: crate::native_function::NativeFastFn,
    receiver: Value,
    arguments: crate::ValueWindow<'_>,
) -> NativeResultPair {
    let mut invoke = |vm: &mut crate::Interpreter| {
        Ok(crate::call_ops::with_frame_native_ctx(
            vm,
            stack,
            NativeContext::Caller(caller),
            receiver,
            |native_ctx| body(native_ctx, arguments),
        ))
    };
    let result = match vm.native_target_realm_global(&native) {
        Some(global) => vm.with_host_realm_global(global, invoke),
        None => invoke(vm),
    };
    match result {
        Ok(Ok(value)) => NativeResultPair::success(value),
        Ok(Err(error)) => fail(vm, stack, caller, ctx, error),
        Err(error) => finish_fatal(vm, ctx, error),
    }
}

/// A body's failure, projected through the caller's source.
#[cold]
#[inline(never)]
fn fail(
    vm: &mut crate::Interpreter,
    stack: &mut crate::ActivationStack,
    caller: &CallerContext<'_>,
    ctx: &mut JitCtx,
    error: crate::NativeError,
) -> NativeResultPair {
    let context = caller.get(vm);
    finish_native_pair(vm, stack, context, ctx, Err(error))
}
