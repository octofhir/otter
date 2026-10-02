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
//!   generation the trampoline found past its break-even.
//!
//! # Invariants
//! The trampoline classified the callee; these bodies dispatch only on the
//! host kind it wrote into `header.function_id`. Every input is a traced slot
//! of the published frame and is re-read after each allocation or reentry.
//! A host body that must run JavaScript next stores the child's callee,
//! receiver and actuals in its own frame, records its resumption state in
//! `header.pc` and returns `Continue`; it never executes a bytecode callee
//! itself. A child span points into this frame and is copied by the
//! trampoline before any allocation.
//!
//! # See also
//! - [`crate::native_abi::call_trampoline`] for classification and linkage.
//! - [`super::call_dispatch`] for the interpreter entry on the same stack.

use crate::{
    ActivationStack, ExecutionContext, Interpreter, Value, VmError,
    native_abi::{
        CallRequest, Frame, HOST_FRAME_REGISTER_COUNT, HostCallKind, JitCtx, NativeFrameFlags,
        NativeResultDomain, NativeResultPair, NativeResultStatus,
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
    /// The body completed with this value.
    Return(Value),
    /// `ctx.pending_call` holds a child request; resume afterwards.
    Child,
}

struct HostTurn<'a> {
    vm: &'a mut Interpreter,
    stack: &'a mut ActivationStack,
    context: &'a ExecutionContext,
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
        self.frame().registers.get(index).copied().unwrap_or(Value::UNDEFINED)
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
        self.frame().header.flags.contains(NativeFrameFlags::CONSTRUCT)
    }
}

/// Bind the trampoline-published activation for one host/prepare entry.
fn with_turn(
    ctx: *mut JitCtx,
    body: impl FnOnce(&mut HostTurn<'_>, &mut JitCtx) -> Result<HostStep, VmError>,
) -> NativeResultPair {
    // SAFETY: the trampoline retains this context, its services and its frame.
    let ctx = unsafe { &mut *ctx };
    let Some(activation) = ctx.checked_activation().copied() else {
        return NativeResultPair::fatal_internal();
    };
    let (Some(vm), Some(stack), Some(context)) = (
        unsafe { activation.vm.as_mut() },
        unsafe { activation.stack.as_mut() },
        unsafe { activation.context.as_ref() },
    ) else {
        return NativeResultPair::fatal_internal();
    };
    let frame = ctx.native_frame;
    if frame.is_null() {
        return NativeResultPair::fatal_internal();
    }
    let mut turn = HostTurn {
        vm,
        stack,
        context,
        frame,
    };
    let result = body(&mut turn, ctx);
    match result {
        Ok(HostStep::Return(value)) => NativeResultPair::success(value),
        Ok(HostStep::Child) => NativeResultPair::continue_execution(),
        Err(VmError::Uncaught) => match turn.vm.pending_uncaught_throw.take() {
            Some(exception) => NativeResultPair::throw_value(exception),
            None => NativeResultPair::fatal_internal(),
        },
        Err(error) => {
            turn.vm.pending_uncaught_throw = None;
            if let Some(slot) = unsafe { ctx.error.as_mut() } {
                *slot = Some(error);
            }
            NativeResultPair::fatal_internal()
        }
    }
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
) -> Result<HostStep, VmError> {
    let mut request = CallRequest::EMPTY;
    request.callee = callee;
    request.receiver = receiver;
    if let Some(new_target) = new_target {
        request.new_target = new_target;
        request.header.flags = NativeFrameFlags::from_bits(NativeFrameFlags::CONSTRUCT);
    }
    request.arguments = arguments;
    request.argument_count = u32::try_from(argument_count).map_err(|_| VmError::InvalidOperand)?;
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
                        Err(turn.vm.err_type(
                            "Proxy construct trap returned non-object".to_string().into(),
                        ))
                    })
                }
            }
            Some(NativeResultStatus::Success | NativeResultStatus::Throw | NativeResultStatus::Fatal) => {
                completion
            }
            _ => NativeResultPair::fatal_internal(),
        };
    }
    with_turn(ctx, |turn, ctx| {
        let kind = HostCallKind::from_code(turn.frame().header.function_id)
            .ok_or(VmError::InvalidOperand)?;
        match kind {
            HostCallKind::Native => {
                let native = turn
                    .frame()
                    .self_value
                    .as_native_function()
                    .ok_or(VmError::InvalidOperand)?;
                if turn.is_construct() {
                    native_construct(turn, native).map(HostStep::Return)
                } else {
                    native_call(turn, ctx, native)
                }
            }
            HostCallKind::Proxy => proxy_entry(turn, ctx),
            HostCallKind::Other => other_entry(turn),
            HostCallKind::ClassCall => Err(turn.vm.err_type(
                "Class constructor cannot be invoked without 'new'"
                    .to_string()
                    .into(),
            )),
            HostCallKind::NotConstructor => Err(turn.vm.err_type(
                "function is not a constructor".to_string().into(),
            )),
        }
    })
}

/// `[[Call]]` of a native body with the frame's actuals.
fn native_call(
    turn: &mut HostTurn<'_>,
    ctx: &mut JitCtx,
    native: crate::native_function::NativeFunction,
) -> Result<HostStep, VmError> {
    let call = native.call_target(&turn.vm.gc_heap);
    if let NativeCallTarget::VmIntrinsic(intrinsic) = call {
        if intrinsic == VmIntrinsicFunction::FunctionPrototypeCall {
            // §20.2.3.3 — the receiver is the target; its first actual is
            // the target's receiver and the rest are its actuals, in place.
            let target = turn.frame().this_value;
            if !turn.vm.is_callable_runtime(&target) {
                return Err(VmError::NotCallable);
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
        let (vm, stack, context) = (&mut *turn.vm, &mut *turn.stack, turn.context);
        return if let Some(global) = realm_global
            && global != vm.global_this
        {
            vm.with_host_realm_global(global, |interp| {
                interp.run_vm_intrinsic_sync_rooted(stack, context, intrinsic, receiver, args)
            })
        } else {
            vm.run_vm_intrinsic_sync_rooted(stack, context, intrinsic, receiver, args)
        }
        .map(HostStep::Return);
    }
    turn.vm.record_runtime_native_call()?;
    let realm_global = turn.vm.native_target_realm_global(&native);
    let callee = turn.frame().self_value;
    let receiver = turn.frame().this_value;
    let args = turn.actuals();
    crate::call_ops::invoke_native_call_with_roots(
        turn.vm,
        turn.stack,
        turn.context,
        call,
        realm_global,
        receiver,
        &[&callee],
        args.as_slice(),
    )
    .map(HostStep::Return)
}

/// `[[Construct]]` of a native body: receiver creation from `new.target`
/// unless the constructor allocates its own result.
fn native_construct(
    turn: &mut HostTurn<'_>,
    native: crate::native_function::NativeFunction,
) -> Result<Value, VmError> {
    if !native.is_constructable(&turn.vm.gc_heap) {
        return Err(VmError::NotCallable);
    }
    native_construct_with(turn, native)
}

fn native_construct_with(
    turn: &mut HostTurn<'_>,
    native: crate::native_function::NativeFunction,
) -> Result<Value, VmError> {
    turn.vm.record_runtime_construct_call()?;
    let callee = turn.frame().self_value;
    let new_target = turn.frame().new_target_value;
    if turn.vm.native_receiverless_constructor(&callee).is_some() {
        let args = turn.actuals();
        return turn.vm.invoke_native_construct_rooted(
            turn.stack,
            turn.context,
            native,
            &Value::UNDEFINED,
            &new_target,
            false,
            args.as_slice(),
        );
    }
    let proto = turn
        .vm
        .construct_prototype_for_callee(turn.stack, turn.context, &new_target)?;
    // GetPrototypeFromConstructor falls back to the constructor's own
    // intrinsic default; `Date` keeps its intrinsic slots on that object.
    let date_default = proto.is_none() && native.name_is(&turn.vm.gc_heap, "Date");
    let used_object_prototype_fallback = proto.is_none() && !date_default;
    let proto = match proto {
        Some(proto) => proto,
        None if date_default => turn.vm.constructor_prototype_value("Date")?,
        None => turn.vm.constructor_prototype_value("Object")?,
    };
    // The frame's receiver slot roots the prototype across the allocation.
    turn.frame_mut().this_value = proto;
    let receiver = turn.vm.alloc_stack_rooted_object_with_extra_roots(turn.stack, &[])?;
    let proto = turn.frame().this_value;
    crate::object::set_prototype_value(receiver, &mut turn.vm.gc_heap, Some(proto));
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
        .ok_or(VmError::InvalidOperand)?;
    let args = turn.actuals();
    turn.vm.invoke_native_construct_rooted(
        turn.stack,
        turn.context,
        native,
        &this_value,
        &new_target,
        used_object_prototype_fallback,
        args.as_slice(),
    )
}

/// An object with an internal native `[[Call]]`/`[[Construct]]`, or a value
/// that has neither.
fn other_entry(turn: &mut HostTurn<'_>) -> Result<HostStep, VmError> {
    let callee = turn.frame().self_value;
    let Some(object) = callee.as_object() else {
        return Err(VmError::NotCallable);
    };
    if turn.is_construct() {
        let native = crate::object::constructor_native(object, &turn.vm.gc_heap)
            .and_then(|value| value.as_native_function())
            .ok_or(VmError::NotCallable)?;
        return native_construct_with(turn, native).map(HostStep::Return);
    }
    let native = crate::object::call_native(object, &turn.vm.gc_heap)
        .and_then(|value| value.as_native_function())
        .ok_or(VmError::NotCallable)?;
    let call = native.call_target(&turn.vm.gc_heap);
    turn.vm.record_runtime_native_call()?;
    let realm_global = turn.vm.native_target_realm_global(&native);
    let receiver = turn.frame().this_value;
    let args = turn.actuals();
    crate::call_ops::invoke_native_call_with_roots(
        turn.vm,
        turn.stack,
        turn.context,
        call,
        realm_global,
        receiver,
        &[&callee],
        args.as_slice(),
    )
    .map(HostStep::Return)
}

/// §10.5.12 `[[Call]]` and §10.5.13 `[[Construct]]` of a proxy.
fn proxy_entry(turn: &mut HostTurn<'_>, ctx: &mut JitCtx) -> Result<HostStep, VmError> {
    let construct = turn.is_construct();
    let proxy = turn
        .frame()
        .self_value
        .as_proxy()
        .ok_or(VmError::InvalidOperand)?;
    let admitted = if construct {
        crate::abstract_ops::is_constructor(&turn.frame().self_value, turn.context, &turn.vm.gc_heap)
    } else {
        proxy.is_callable(&turn.vm.gc_heap)
    };
    if !admitted {
        return Err(VmError::NotCallable);
    }
    if proxy.is_revoked(&turn.vm.gc_heap) {
        let message = if construct {
            "Cannot perform 'construct' on a revoked proxy"
        } else {
            "Cannot perform 'apply' on a proxy that has been revoked"
        };
        return Err(turn.vm.err_type(message.to_string().into()));
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
        .ordinary_get_value(turn.stack, turn.context, handler, handler, &trap_key, 0)?
    {
        crate::VmGetOutcome::Value(value) => value,
        crate::VmGetOutcome::InvokeGetter { getter } => {
            let handler = turn.register(PROXY_HANDLER_REGISTER);
            turn.vm
                .run_callable_sync_rooted(turn.stack, turn.context, &getter, handler, SmallVec::new())?
        }
    };
    if trap.is_nullish() {
        let target = turn.register(PROXY_TARGET_REGISTER);
        let count = turn.frame().argument_count as usize;
        let receiver = turn.frame().this_value;
        // §10.5.13 step 7: `new.target` is forwarded unchanged.
        let new_target = construct.then(|| turn.frame().new_target_value);
        let arguments = turn.actuals_ptr();
        return request_child(
            ctx,
            turn,
            target,
            receiver,
            new_target,
            arguments,
            count,
            HOST_FORWARD_CHILD,
        );
    }
    if !turn.vm.is_callable_runtime(&trap) {
        let message = if construct {
            "Proxy construct trap is not callable"
        } else {
            "Proxy apply trap is not callable"
        };
        return Err(turn.vm.err_type(message.to_string().into()));
    }
    // The trap rides the receiver slot of the frame across the allocation.
    let receiver = turn.frame().this_value;
    turn.set_register(TRAP_ARGUMENTS_REGISTER + 1, receiver);
    turn.frame_mut().this_value = trap;
    let args = turn.actuals();
    let argv = turn
        .vm
        .alloc_stack_rooted_array_from_values(turn.stack, args.iter().copied(), &[], &args)?;
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
    let arguments = unsafe { turn.frame().registers.as_mut_ptr().add(TRAP_ARGUMENTS_REGISTER) };
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
            .context
            .for_function(function_id)
            .map_err(|_| VmError::InvalidOperand)?;
        if turn.is_construct() {
            turn.vm.record_runtime_construct_call()?;
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
            return Ok(HostStep::Return(Value::UNDEFINED));
        }
        let function = owner
            .exec_function(function_id)
            .ok_or(VmError::InvalidOperand)?;
        let receiver = turn.frame().this_value;
        let receiver =
            turn.vm
                .this_for_bytecode_call_stack_rooted(function, turn.stack, receiver, &[])?;
        turn.frame_mut().this_value = receiver;
        Ok(HostStep::Return(Value::UNDEFINED))
    });
    // Success carries no value: the trampoline enters the frame next.
    match result.validate(NativeResultDomain::Execution) {
        Some(NativeResultStatus::Success) => NativeResultPair::success(Value::UNDEFINED),
        _ => result,
    }
}

/// Promote the function of the published frame, whose selected generation
/// reached its break-even on this entry.
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
    let (Some(vm), Some(context)) =
        (unsafe { activation.vm.as_mut() }, unsafe { activation.context.as_ref() })
    else {
        return NativeResultPair::success(Value::UNDEFINED);
    };
    // SAFETY: the call entry published this frame before the call.
    let function_id = unsafe { (*ctx.native_frame).header.function_id };
    vm.promote_entered_function(context, function_id);
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
            .map(HostStep::Return)
    })
}
