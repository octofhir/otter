//! Host bodies and activation preparation for the classifying call trampoline.
//!
//! # Contents
//! - [`host_call_entry`] runs a [`NativeFrameKind::Host`] frame: native bodies,
//!   proxy traps, objects with an internal native `[[Call]]`, and the call
//!   errors the trampoline selected.
//! - [`prepare_activation`] converts a sloppy primitive receiver or creates a
//!   base constructor's receiver on an already published bytecode frame.
//! - [`derived_construct_result`] applies the derived-constructor return rule
//!   when a generated body returned a non-object.
//! - [`promote_entered_function`] runs the optimizing promotion policy for a
//!   generation whose canonical source work reached its absolute target.
//!
//! # Invariants
//! The trampoline classified the callee; these bodies dispatch only on the
//! host kind it wrote into `header.function_id`. Every input is a traced slot
//! of the published frame and is re-read after each allocation or reentry.
//! A host body that must run JavaScript next stores the child's callee,
//! receiver and actuals in its own frame, records its resumption state in
//! `header.pc` and returns `Continue`; it never executes a bytecode callee
//! itself. A child span points into this frame and is copied by the
//! trampoline before any allocation. A native body receives the source owner
//! of its innermost JavaScript caller (GetActiveScriptOrModule), never a
//! different chunk the activation was entered through. Native bodies and
//! semantic Host errors project once while the source/root and creation realm
//! remain published; failed materialization returns the exact error behind a
//! final Fatal pair.
//!
//! # See also
//! - [`crate::native_abi::call_trampoline`] for classification and linkage.
//! - [`super::call_dispatch`] for the interpreter entry on the same stack.

use crate::native_abi::CommittedValueError;
use crate::{
    ActivationStack, ExecutionContext, Interpreter, Value, VmError,
    native_abi::{
        CallRequest, Frame, HOST_FRAME_REGISTER_COUNT, HostCallKind, JitCtx, NativeFrameFlags,
        NativeFrameKind, NativeResultDomain, NativeResultPair, NativeResultStatus,
    },
    native_function::{NativeCallTarget, VmIntrinsicFunction},
};
use smallvec::SmallVec;

/// Resumption states stored in a host frame's `header.pc`.
const HOST_START: u32 = 0;
/// A child call's completion is the host body's own completion.
const HOST_FORWARD_CHILD: u32 = 1;
/// A proxy `construct` trap returned; its result must be an object.
const HOST_PROXY_CONSTRUCT_RESULT: u32 = 2;

/// Host-frame register holding a proxy target across its observable trap lookup.
const PROXY_TARGET_REGISTER: usize = 0;
/// Host-frame register holding a proxy handler across its trap lookup.
const PROXY_HANDLER_REGISTER: usize = 1;
/// First of three contiguous trap actuals staged for the child call.
const TRAP_ARGUMENTS_REGISTER: usize = 2;
const _: () = assert!(TRAP_ARGUMENTS_REGISTER + 3 <= HOST_FRAME_REGISTER_COUNT as usize);

/// One step of a host body.
enum HostStep {
    /// The body completed in the one execution result domain.
    Complete(NativeResultPair),
    /// `ctx.pending_call` holds a child request; resume afterwards.
    Child,
}

struct HostTurn<'a> {
    vm: &'a mut Interpreter,
    stack: &'a mut ActivationStack,
    context: Option<&'a ExecutionContext>,
    frame: *mut Frame,
}

impl HostTurn<'_> {
    fn frame(&self) -> &Frame {
        // SAFETY: the trampoline keeps this frame published for the entry.
        unsafe { &*self.frame }
    }

    fn frame_mut(&mut self) -> &mut Frame {
        // SAFETY: the trampoline keeps this frame published for the entry and
        // no other reference to it is live across this short borrow.
        unsafe { &mut *self.frame }
    }

    fn register(&self, index: usize) -> Value {
        self.frame()
            .registers
            .get(index)
            .copied()
            .unwrap_or(Value::UNDEFINED)
    }

    fn set_register(&mut self, index: usize, value: Value) {
        if let Some(slot) = self.frame_mut().registers.get_mut(index) {
            *slot = value;
        }
    }

    fn actuals_ptr(&self) -> *const Value {
        self.frame().actuals
    }

    fn actuals(&self) -> SmallVec<[Value; 8]> {
        let count = self.frame().argument_count as usize;
        // SAFETY: the trampoline initialized `argument_count` actual slots.
        unsafe { std::slice::from_raw_parts(self.actuals_ptr(), count) }
            .iter()
            .copied()
            .collect()
    }

    fn is_construct(&self) -> bool {
        self.frame()
            .header
            .flags
            .contains(NativeFrameFlags::CONSTRUCT)
    }

    /// §9.4.1 GetActiveScriptOrModule for a native body: the source owner of
    /// the innermost JavaScript activation below this host frame. Built-in
    /// frames carry no script, so intermediate Host frames are skipped; a
    /// generated caller names the inlined function at its exact return anchor.
    /// `None` keeps the trampoline's admitted context, which already owns that
    /// function or is the only context of a host-entered activation.
    fn caller_source_owner(&self) -> Option<ExecutionContext> {
        let mut child = self.frame;
        loop {
            // SAFETY: every published record stays live and linked to its
            // caller while this host frame is published.
            let callee = unsafe { &*child };
            let caller = callee.caller_frame();
            // SAFETY: as above; a null caller ends the published chain.
            let frame = unsafe { caller.as_ref() }?;
            if frame.header.kind == NativeFrameKind::Host {
                child = caller;
                continue;
            }
            let function_id = if frame.code_object_id == 0 || callee.caller_return_pc == 0 {
                frame.header.function_id
            } else {
                self.vm
                    .jit_code_registry
                    .return_pc_record(u64::from(frame.code_object_id), callee.caller_return_pc)
                    .and_then(|record| record.inline_frames.last())
                    .map_or(frame.header.function_id, |inline| inline.function_id)
            };
            if self
                .context
                .is_some_and(|context| context.covers_function(function_id))
            {
                return None;
            }
            return self.vm.function_context(self.context, function_id).ok();
        }
    }
}

/// Bind the trampoline-published activation for one host/prepare entry.
fn with_turn(
    ctx: *mut JitCtx,
    body: impl FnOnce(&mut HostTurn<'_>, &mut JitCtx) -> Result<HostStep, CommittedValueError>,
) -> NativeResultPair {
    // SAFETY: the trampoline retains this context, its services and its frame.
    let ctx = unsafe { &mut *ctx };
    let Some(activation) = ctx.checked_activation().copied() else {
        return NativeResultPair::fatal_internal();
    };
    let (Some(vm), Some(stack)) = (unsafe { activation.vm.as_mut() }, unsafe {
        activation.stack.as_mut()
    }) else {
        return NativeResultPair::fatal_internal();
    };
    let frame = ctx.native_frame;
    if frame.is_null() {
        return NativeResultPair::fatal_internal();
    }
    let mut turn = HostTurn {
        vm,
        stack,
        context: unsafe { activation.context.as_ref() },
        frame,
    };
    let result = body(&mut turn, ctx);
    match result {
        Ok(HostStep::Complete(pair)) => pair,
        Ok(HostStep::Child) => NativeResultPair::continue_execution(),
        Err(error) => finish_vm_error(turn.vm, turn.stack, turn.context, ctx, error),
    }
}

/// Complete a semantic failure in its actual published owner.
/// Host bodies are completed call extents: an allocation failure here cannot
/// return to an earlier child/projection phase. JavaScript activation preparation
/// retains direct-source OOM materialization and its canonical catch handling.
fn finish_vm_error(
    vm: &mut Interpreter,
    stack: &ActivationStack,
    context: Option<&ExecutionContext>,
    ctx: &mut JitCtx,
    error: CommittedValueError,
) -> NativeResultPair {
    // The producer owns the disposition: local source semantics materialize
    // once; a completed child or pure admission failure is already terminal.
    let projected = match error {
        CommittedValueError::JavaScript(error) => {
            vm.vm_error_to_throwable_with_stack_roots(context, stack, &error)
        }
        CommittedValueError::Fatal(error) => Err(error),
    };
    if projected.is_ok() {
        vm.record_throw_site();
    }
    finish_projection(vm, ctx, projected)
}

/// Commit a projection while the exact published source and realm are live.
/// A failed Error allocation is already the final failure, so no caller may
/// send its `Fatal` result through JavaScript materialization again.
fn finish_projection(
    vm: &mut Interpreter,
    ctx: &mut JitCtx,
    projected: Result<Value, VmError>,
) -> NativeResultPair {
    match projected {
        Ok(exception) => NativeResultPair::throw_value(exception),
        Err(error) => {
            vm.pending_uncaught_throw = None;
            if let Some(slot) = unsafe { ctx.error.as_mut() } {
                *slot = Some(error);
            }
            NativeResultPair::fatal_internal()
        }
    }
}

fn finish_native_result(
    vm: &mut Interpreter,
    stack: &ActivationStack,
    context: Option<&ExecutionContext>,
    ctx: &mut JitCtx,
    result: Result<Value, crate::NativeError>,
) -> HostStep {
    HostStep::Complete(match result {
        Ok(value) => NativeResultPair::success(value),
        Err(error) => {
            let projected =
                crate::error_ops::native_error_to_throwable_with_stack(vm, stack, context, error);
            if projected.is_ok() {
                vm.record_throw_site();
            }
            finish_projection(vm, ctx, projected)
        }
    })
}

/// Publish a classify request whose span lives in the current host frame.
fn request_child(
    ctx: &mut JitCtx,
    turn: &mut HostTurn<'_>,
    callee: Value,
    receiver: Value,
    new_target: Option<Value>,
    arguments: *const Value,
    argument_count: usize,
    resume: u32,
) -> Result<HostStep, CommittedValueError> {
    let mut request = CallRequest::EMPTY;
    request.callee = callee;
    request.receiver = receiver;
    if let Some(new_target) = new_target {
        request.new_target = new_target;
        request.header.flags = NativeFrameFlags::from_bits(NativeFrameFlags::CONSTRUCT);
    }
    request.arguments = arguments;
    request.argument_count = u32::try_from(argument_count)
        .map_err(|_| CommittedValueError::Fatal(VmError::InvalidOperand))?;
    ctx.pending_call = request;
    turn.frame_mut().header.pc = resume;
    Ok(HostStep::Child)
}

/// Run one trampoline-selected host frame.
pub(crate) extern "C" fn host_call_entry(ctx: *mut JitCtx) -> NativeResultPair {
    // SAFETY: the trampoline retains this context and its published frame.
    let (completion, state) = unsafe {
        let ctx = &mut *ctx;
        let completion = ctx.completion;
        ctx.completion = NativeResultPair::success(Value::UNDEFINED);
        ctx.completion_destination = u32::MAX;
        (completion, (*ctx.native_frame).header.pc)
    };
    if state != HOST_START {
        return match completion.validate(NativeResultDomain::Execution) {
            Some(NativeResultStatus::Success) if state == HOST_PROXY_CONSTRUCT_RESULT => {
                if completion.payload_value().is_object_type() {
                    completion
                } else {
                    with_turn(ctx, |turn, _| {
                        Err(CommittedValueError::JavaScript(
                            turn.vm.err_type(
                                "Proxy construct trap returned non-object"
                                    .to_string()
                                    .into(),
                            ),
                        ))
                    })
                }
            }
            Some(
                NativeResultStatus::Success | NativeResultStatus::Throw | NativeResultStatus::Fatal,
            ) => completion,
            _ => NativeResultPair::fatal_internal(),
        };
    }
    with_turn(ctx, |turn, ctx| {
        let kind = HostCallKind::from_code(turn.frame().header.function_id)
            .ok_or(VmError::InvalidOperand)
            .map_err(|error| CommittedValueError::Fatal(error.into()))?;
        match kind {
            HostCallKind::Native => {
                let native = turn
                    .frame()
                    .self_value
                    .as_native_function()
                    .ok_or(VmError::InvalidOperand)
                    .map_err(|error| CommittedValueError::Fatal(error.into()))?;
                if turn.is_construct() {
                    native_construct(turn, ctx, native)
                } else {
                    native_call(turn, ctx, native)
                }
            }
            HostCallKind::Proxy => proxy_entry(turn, ctx),
            HostCallKind::Other => other_entry(turn, ctx),
            HostCallKind::ClassCall => Err(CommittedValueError::JavaScript(
                turn.vm.err_type(
                    "Class constructor cannot be invoked without 'new'"
                        .to_string()
                        .into(),
                ),
            )),
            HostCallKind::NotConstructor => Err(CommittedValueError::JavaScript(
                turn.vm
                    .err_type("function is not a constructor".to_string().into()),
            )),
        }
    })
}

/// `[[Call]]` of a native body with the frame's actuals.
fn native_call(
    turn: &mut HostTurn<'_>,
    ctx: &mut JitCtx,
    native: crate::native_function::NativeFunction,
) -> Result<HostStep, CommittedValueError> {
    let call = native.call_target(&turn.vm.gc_heap);
    if let NativeCallTarget::VmIntrinsic(intrinsic) = call {
        if intrinsic == VmIntrinsicFunction::FunctionPrototypeCall {
            // §20.2.3.3 — the receiver is the target; its first actual is
            // the target's receiver and the rest are its actuals, in place.
            let target = turn.frame().this_value;
            if !turn.vm.is_callable_runtime(&target) {
                return Err(CommittedValueError::JavaScript(VmError::NotCallable));
            }
            let count = turn.frame().argument_count as usize;
            let receiver = if count == 0 {
                Value::UNDEFINED
            } else {
                // SAFETY: at least one initialized actual.
                unsafe { *turn.actuals_ptr() }
            };
            let (arguments, count) = if count == 0 {
                (turn.actuals_ptr(), 0)
            } else {
                // SAFETY: the span starts inside the initialized window.
                (unsafe { turn.actuals_ptr().add(1) }, count - 1)
            };
            return request_child(
                ctx,
                turn,
                target,
                receiver,
                None,
                arguments,
                count,
                HOST_FORWARD_CHILD,
            );
        }
        let receiver = turn.frame().this_value;
        let args = turn.actuals();
        let realm_global = turn.vm.native_target_realm_global(&native);
        let source = turn.caller_source_owner();
        let (vm, stack, context) = (
            &mut *turn.vm,
            &mut *turn.stack,
            source
                .as_ref()
                .or(turn.context)
                .ok_or(VmError::InvalidOperand)
                .map_err(|error| CommittedValueError::Fatal(error.into()))?,
        );
        let invoke = |vm: &mut Interpreter| {
            let result = vm.run_vm_intrinsic_sync_rooted(stack, context, intrinsic, receiver, args);
            Ok(HostStep::Complete(match result {
                Ok(value) => NativeResultPair::success(value),
                Err(error) => finish_vm_error(vm, stack, Some(context), ctx, error),
            }))
        };
        return if let Some(global) = realm_global {
            vm.with_host_realm_global(global, invoke)
                .map_err(CommittedValueError::Fatal)
        } else {
            invoke(vm).map_err(CommittedValueError::Fatal)
        };
    }
    turn.vm
        .record_runtime_native_call()
        .map_err(|error| CommittedValueError::Fatal(error.into()))?;
    let realm_global = turn.vm.native_target_realm_global(&native);
    let callee = turn.frame().self_value;
    let receiver = turn.frame().this_value;
    let args = turn.actuals();
    let source = turn.caller_source_owner();
    let (vm, stack, context) = (
        &mut *turn.vm,
        &mut *turn.stack,
        source.as_ref().or(turn.context),
    );
    let invoke = |vm: &mut Interpreter| {
        let result = crate::call_ops::invoke_native_call_with_roots(
            vm,
            stack,
            context,
            call,
            receiver,
            &[&callee],
            args.as_slice(),
        );
        Ok(finish_native_result(vm, stack, context, ctx, result))
    };
    if let Some(global) = realm_global {
        vm.with_host_realm_global(global, invoke)
            .map_err(CommittedValueError::Fatal)
    } else {
        invoke(vm).map_err(CommittedValueError::Fatal)
    }
}

/// `[[Construct]]` of a native body: receiver creation from `new.target`
/// unless the constructor allocates its own result.
fn native_construct(
    turn: &mut HostTurn<'_>,
    ctx: &mut JitCtx,
    native: crate::native_function::NativeFunction,
) -> Result<HostStep, CommittedValueError> {
    if !native.is_constructable(&turn.vm.gc_heap) {
        return Err(CommittedValueError::JavaScript(VmError::NotCallable));
    }
    native_construct_with(turn, ctx, native)
}

fn native_construct_with(
    turn: &mut HostTurn<'_>,
    ctx: &mut JitCtx,
    native: crate::native_function::NativeFunction,
) -> Result<HostStep, CommittedValueError> {
    let context = turn
        .context
        .ok_or(VmError::InvalidOperand)
        .map_err(|error| CommittedValueError::Fatal(error.into()))?;
    turn.vm
        .record_runtime_construct_call()
        .map_err(CommittedValueError::Fatal)?;
    let callee = turn.frame().self_value;
    let new_target = turn.frame().new_target_value;
    if turn.vm.native_receiverless_constructor(&callee).is_some() {
        let args = turn.actuals();
        return invoke_native_construct(
            turn,
            ctx,
            native,
            &Value::UNDEFINED,
            &new_target,
            false,
            args.as_slice(),
        )
        .map_err(CommittedValueError::Fatal);
    }
    let proto = turn
        .vm
        .construct_prototype_for_callee(turn.stack, context, &new_target)?;
    // GetPrototypeFromConstructor falls back to the constructor's own
    // intrinsic default; `Date` keeps its intrinsic slots on that object.
    let date_default = proto.is_none() && native.name_is(&turn.vm.gc_heap, "Date");
    let used_object_prototype_fallback = proto.is_none() && !date_default;
    let proto = match proto {
        Some(proto) => proto,
        None if date_default => turn
            .vm
            .constructor_prototype_value("Date")
            .map_err(|error| CommittedValueError::Fatal(error.into()))?,
        None => turn
            .vm
            .constructor_prototype_value("Object")
            .map_err(|error| CommittedValueError::Fatal(error.into()))?,
    };
    // The frame's receiver slot roots the prototype across the allocation.
    turn.frame_mut().this_value = proto;
    let mut receiver = turn
        .vm
        .alloc_stack_rooted_object_with_extra_roots(turn.stack, &[])
        .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
    let proto = turn.frame().this_value;
    if !crate::object::set_prototype_value(&mut receiver, &mut turn.vm.gc_heap, Some(proto))
        .map_err(|error| CommittedValueError::JavaScript(error.into()))?
    {
        return Err(CommittedValueError::JavaScript(VmError::TypeError));
    }
    turn.frame_mut().this_value = Value::object(receiver);
    let this_value = turn.frame().this_value;
    let new_target = turn.frame().new_target_value;
    let native = turn
        .frame()
        .self_value
        .as_native_function()
        .or_else(|| {
            turn.frame().self_value.as_object().and_then(|object| {
                crate::object::constructor_native(object, &turn.vm.gc_heap)
                    .and_then(|value| value.as_native_function())
            })
        })
        .ok_or(VmError::InvalidOperand)
        .map_err(|error| CommittedValueError::Fatal(error.into()))?;
    let args = turn.actuals();
    invoke_native_construct(
        turn,
        ctx,
        native,
        &this_value,
        &new_target,
        used_object_prototype_fallback,
        args.as_slice(),
    )
    .map_err(CommittedValueError::Fatal)
}

/// Only the body and its error projection enter the native creation realm.
/// Receiver/prototype preparation above retains its own ordinary VM semantics.
fn invoke_native_construct(
    turn: &mut HostTurn<'_>,
    ctx: &mut JitCtx,
    native: crate::NativeFunction,
    this_value: &Value,
    new_target: &Value,
    used_object_prototype_fallback: bool,
    args: &[Value],
) -> Result<HostStep, VmError> {
    turn.vm.record_runtime_native_call()?;
    let realm_global = turn.vm.native_target_realm_global(&native);
    let source = turn.caller_source_owner();
    let (vm, stack, context) = (
        &mut *turn.vm,
        &mut *turn.stack,
        source
            .as_ref()
            .or(turn.context)
            .ok_or(VmError::InvalidOperand)?,
    );
    let mut invoke = |vm: &mut Interpreter| {
        let result = vm.invoke_native_construct_rooted(
            stack,
            context,
            native,
            this_value,
            new_target,
            used_object_prototype_fallback,
            args,
        );
        Ok(finish_native_result(vm, stack, Some(context), ctx, result))
    };
    if let Some(global) = realm_global {
        vm.with_host_realm_global(global, invoke)
    } else {
        invoke(vm)
    }
}

/// An object with an internal native `[[Call]]`/`[[Construct]]`, or a value
/// that has neither.
fn other_entry(turn: &mut HostTurn<'_>, ctx: &mut JitCtx) -> Result<HostStep, CommittedValueError> {
    let callee = turn.frame().self_value;
    let Some(object) = callee.as_object() else {
        return Err(CommittedValueError::JavaScript(VmError::NotCallable));
    };
    if turn.is_construct() {
        // §7.3.15 Construct requires IsConstructor; an object without
        // [[Construct]] is an ordinary TypeError at the `new` site.
        let native = crate::object::constructor_native(object, &turn.vm.gc_heap)
            .and_then(|value| value.as_native_function())
            .ok_or(VmError::NotCallable)
            .map_err(CommittedValueError::JavaScript)?;
        return native_construct_with(turn, ctx, native);
    }
    let native = crate::object::call_native(object, &turn.vm.gc_heap)
        .and_then(|value| value.as_native_function())
        .ok_or(VmError::NotCallable)
        .map_err(CommittedValueError::JavaScript)?;
    let call = native.call_target(&turn.vm.gc_heap);
    turn.vm
        .record_runtime_native_call()
        .map_err(CommittedValueError::Fatal)?;
    let realm_global = turn.vm.native_target_realm_global(&native);
    let receiver = turn.frame().this_value;
    let args = turn.actuals();
    let source = turn.caller_source_owner();
    let (vm, stack, context) = (
        &mut *turn.vm,
        &mut *turn.stack,
        source.as_ref().or(turn.context),
    );
    let invoke = |vm: &mut Interpreter| {
        let result = crate::call_ops::invoke_native_call_with_roots(
            vm,
            stack,
            context,
            call,
            receiver,
            &[&callee],
            args.as_slice(),
        );
        Ok(finish_native_result(vm, stack, context, ctx, result))
    };
    if let Some(global) = realm_global {
        vm.with_host_realm_global(global, invoke)
            .map_err(CommittedValueError::Fatal)
    } else {
        invoke(vm).map_err(CommittedValueError::Fatal)
    }
}

/// §10.5.12 `[[Call]]` and §10.5.13 `[[Construct]]` of a proxy.
fn proxy_entry(turn: &mut HostTurn<'_>, ctx: &mut JitCtx) -> Result<HostStep, CommittedValueError> {
    let context = turn.context;
    let construct = turn.is_construct();
    let proxy = turn
        .frame()
        .self_value
        .as_proxy()
        .ok_or(VmError::InvalidOperand)
        .map_err(|error| CommittedValueError::Fatal(error.into()))?;
    let admitted = if construct {
        crate::abstract_ops::is_constructor(
            &turn.frame().self_value,
            context
                .ok_or(VmError::InvalidOperand)
                .map_err(|error| CommittedValueError::Fatal(error.into()))?,
            &turn.vm.gc_heap,
        )
    } else {
        proxy.is_callable(&turn.vm.gc_heap)
    };
    if !admitted {
        return Err(CommittedValueError::JavaScript(VmError::NotCallable));
    }
    if proxy.is_revoked(&turn.vm.gc_heap) {
        let message = if construct {
            "Cannot perform 'construct' on a revoked proxy"
        } else {
            "Cannot perform 'apply' on a proxy that has been revoked"
        };
        return Err(CommittedValueError::JavaScript(
            turn.vm.err_type(message.to_string().into()),
        ));
    }
    // [[ProxyTarget]] is captured before the observable GetMethod; a getter
    // may revoke the proxy, and both outcomes use this captured target.
    let target = proxy.target(&turn.vm.gc_heap);
    let handler = proxy.handler(&turn.vm.gc_heap);
    turn.set_register(PROXY_TARGET_REGISTER, target);
    turn.set_register(PROXY_HANDLER_REGISTER, handler);
    let trap_key = crate::VmPropertyKey::String(if construct { "construct" } else { "apply" });
    let trap = match turn
        .vm
        .ordinary_get_value(turn.stack, context, handler, handler, &trap_key, 0)?
    {
        crate::VmGetOutcome::Value(value) => value,
        crate::VmGetOutcome::InvokeGetter { getter } => {
            let handler = turn.register(PROXY_HANDLER_REGISTER);
            turn.vm
                .run_callable_sync_rooted(turn.stack, context, &getter, handler, SmallVec::new())
                .map_err(CommittedValueError::completed_call)?
        }
    };
    if trap.is_nullish() {
        let target = turn.register(PROXY_TARGET_REGISTER);
        let count = turn.frame().argument_count as usize;
        let receiver = turn.frame().this_value;
        // §10.5.13 step 7: `new.target` is forwarded unchanged.
        let new_target = construct.then(|| turn.frame().new_target_value);
        let arguments = turn.actuals_ptr();
        let origin = if construct {
            std::mem::replace(&mut turn.frame_mut().super_origin, 0)
        } else {
            0
        };
        let step = request_child(
            ctx,
            turn,
            target,
            receiver,
            new_target,
            arguments,
            count,
            HOST_FORWARD_CHILD,
        )?;
        // This is precisely the transparent default [[Construct]] branch.
        // Observable user traps and their nested new calls use EMPTY instead.
        ctx.pending_call.super_origin = origin;
        return Ok(step);
    }
    if !turn.vm.is_callable_runtime(&trap) {
        let message = if construct {
            "Proxy construct trap is not callable"
        } else {
            "Proxy apply trap is not callable"
        };
        return Err(CommittedValueError::JavaScript(
            turn.vm.err_type(message.to_string().into()),
        ));
    }
    // The trap rides the receiver slot of the frame across the allocation.
    let receiver = turn.frame().this_value;
    turn.set_register(TRAP_ARGUMENTS_REGISTER + 1, receiver);
    turn.frame_mut().this_value = trap;
    let args = turn.actuals();
    let argv = turn
        .vm
        .alloc_stack_rooted_array_from_values(turn.stack, args.iter().copied(), &[], &args)
        .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
    let trap = turn.frame().this_value;
    let receiver = turn.register(TRAP_ARGUMENTS_REGISTER + 1);
    turn.frame_mut().this_value = receiver;
    let target = turn.register(PROXY_TARGET_REGISTER);
    let handler = turn.register(PROXY_HANDLER_REGISTER);
    let (second, third) = if construct {
        (Value::array(argv), turn.frame().new_target_value)
    } else {
        (receiver, Value::array(argv))
    };
    turn.set_register(TRAP_ARGUMENTS_REGISTER, target);
    turn.set_register(TRAP_ARGUMENTS_REGISTER + 1, second);
    turn.set_register(TRAP_ARGUMENTS_REGISTER + 2, third);
    // SAFETY: three contiguous initialized registers inside the host window.
    let arguments = unsafe {
        turn.frame()
            .registers
            .as_mut_ptr()
            .add(TRAP_ARGUMENTS_REGISTER)
    };
    request_child(
        ctx,
        turn,
        trap,
        handler,
        None,
        arguments,
        3,
        if construct {
            HOST_PROXY_CONSTRUCT_RESULT
        } else {
            HOST_FORWARD_CHILD
        },
    )
}

/// Convert a sloppy receiver or create a base constructor's receiver on the
/// published bytecode frame, before its body runs.
pub extern "C" fn prepare_activation(ctx: *mut JitCtx) -> NativeResultPair {
    let result = with_turn(ctx, |turn, _| {
        let function_id = turn.frame().header.function_id;
        let owner = turn
            .vm
            .function_context(turn.context, function_id)
            .map_err(|error| CommittedValueError::Fatal(error.into()))?;
        if turn.is_construct() {
            turn.vm
                .record_runtime_construct_call()
                .map_err(|error| CommittedValueError::Fatal(error.into()))?;
            let callee = turn.frame().self_value;
            let new_target = turn.frame().new_target_value;
            let receiver = turn.vm.jit_prepare_base_construct_receiver(
                turn.stack,
                &owner,
                function_id,
                callee,
                new_target,
            )?;
            turn.frame_mut().this_value = receiver;
            return Ok(HostStep::Complete(NativeResultPair::success(
                Value::UNDEFINED,
            )));
        }
        let function = owner
            .exec_function(function_id)
            .ok_or(VmError::InvalidOperand)
            .map_err(|error| CommittedValueError::Fatal(error.into()))?;
        let receiver = turn.frame().this_value;
        let receiver = turn
            .vm
            .this_for_bytecode_call_stack_rooted(function, turn.stack, receiver, &[])
            .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
        turn.frame_mut().this_value = receiver;
        Ok(HostStep::Complete(NativeResultPair::success(
            Value::UNDEFINED,
        )))
    });
    // Success carries no value: the trampoline enters the frame next.
    match result.validate(NativeResultDomain::Execution) {
        Some(NativeResultStatus::Success) => NativeResultPair::success(Value::UNDEFINED),
        _ => result,
    }
}

/// Promote the function of the published frame, whose selected generation
/// reached its source-work target before this entry.
///
/// Compilation runs no JavaScript; the published frame roots every value of
/// the pending activation. A promoted body publishes through the function's
/// permanent entry cell, so this activation keeps its selected generation and
/// every later entry, from any caller, takes the new one.
///
/// # Safety
/// `ctx` must be the live context of the running compiled entry, with the
/// entering frame published.
pub unsafe extern "C" fn promote_entered_function(ctx: *mut JitCtx) -> NativeResultPair {
    // SAFETY: the call entry retains this context and its published frame.
    let ctx = unsafe { &mut *ctx };
    let Some(activation) = ctx.checked_activation().copied() else {
        return NativeResultPair::success(Value::UNDEFINED);
    };
    // SAFETY: the activation's services are live for the whole entry.
    // SAFETY: the call entry published this frame before the call.
    let function_id = unsafe { (*ctx.native_frame).header.function_id };
    let Some(context) = (unsafe { activation.owner_context(function_id) }) else {
        return NativeResultPair::fatal_internal();
    };
    let Some(vm) = (unsafe { activation.vm.as_mut() }) else {
        return NativeResultPair::fatal_internal();
    };
    vm.promote_entered_function(&context, function_id);
    NativeResultPair::success(Value::UNDEFINED)
}

/// §10.2.2 step 10–12 for a generated derived constructor whose body returned
/// a non-object.
pub extern "C" fn derived_construct_result(ctx: *mut JitCtx, value: u64) -> NativeResultPair {
    let value = Value::from_abi_bits(value);
    with_turn(ctx, |turn, _| {
        let this_value = turn.frame().this_value;
        turn.vm
            .jit_derived_construct_result(value, this_value)
            .map(|value| HostStep::Complete(NativeResultPair::success(value)))
            .map_err(CommittedValueError::JavaScript)
    })
}
