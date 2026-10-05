//! Promise callback completion and collecting constructor-lookup proofs.
//!
//! # Contents
//! - Actual allocator OOM escaping a Promise.try callback stays a final failure.
//! - A collecting Proxy GetMethod hands the target accessor the current rooted promise.
//!
//! # Invariants
//! All retained values use production scopes. Callback observations are owned
//! Rust data, and assertions run outside native dispatch. Direct source OOM
//! catchability is unchanged; these tests exercise a completed callback boundary.
//!
//! # See also
//! - `super::static_try_generic` owns callback completion classification.
//! - `super::Interpreter::promise_resolve_constructor_of` owns the rooted Get.

use super::*;
use crate::{Local, NativeCallInfo, NativeScope, VmError, source_registry::SourceRegistry};
use std::sync::{Arc, Mutex};

/// Allocate a pending Promise under the runtime roots and park it in `scope`.
fn pending_promise<'s>(scope: &mut NativeScope<'s, '_>) -> Result<Local<'s>, NativeError> {
    let handle = pending_runtime_rooted(scope.context().interp_mut(), &[], &[])?;
    Ok(scope.value(Value::promise(handle)))
}

fn admitted_source(vm: &mut Interpreter) -> ExecutionContext {
    vm.link_module(
        crate::test_support::minimal_bytecode_module("promise-completed-boundary.js"),
        SourceRegistry::default(),
    )
    .expect("verifier-valid synthetic source admission")
}

#[test]
fn promise_try_completed_actual_allocator_oom_never_becomes_capability_rejection() {
    let mut vm = Interpreter::with_string_heap_cap(4 * 1024 * 1024)
        .expect("Promise.try cap fixture bootstrap");
    let source = admitted_source(&mut vm);
    vm.gc_heap_mut().set_gc_stress(0, true);
    let observations = Arc::new(Mutex::new(Vec::new()));
    let sink = observations.clone();
    let oversized = "x".repeat(8 * 1024 * 1024);
    let result = NativeCtx::with_host_context(
        &mut vm,
        NativeCallInfo::default_call(),
        Some(&source),
        |ctx| {
            ctx.scope(|mut scope| -> Result<Value, NativeError> {
                let constructor = scope.global("Promise").ok_or(NativeError::InvalidOperand)?;
                let callback = scope.native_closure(
                    "actual failed callback allocation",
                    0,
                    &[],
                    move |ctx, _, _| {
                        // Internal VM fixture: the actual string allocator owns
                        // the refusal, and the canonical mapper owns transport.
                        let failure = match crate::JsString::from_str(&oversized, ctx.heap_mut()) {
                            Ok(_) => return Err(NativeError::InvalidOperand),
                            Err(error) => VmError::from(error),
                        };
                        sink.lock()
                            .map_err(|_| NativeError::InvalidOperand)?
                            .push(failure);
                        Err(crate::native_function::vm_to_native_error(
                            ctx.interp_mut(),
                            failure,
                            "actual callback allocation",
                        ))
                    },
                )?;
                let constructor = scope.raw(constructor);
                let callback = scope.raw(callback);
                scope.with_turn_parts(|interp, stack| {
                    static_try_generic(
                        interp,
                        stack,
                        Some(source.clone()),
                        constructor,
                        &[callback],
                    )
                })
            })
        },
    );
    let error = result.expect_err("completed actual OOM must leave Promise.try");
    let NativeError::ExecutionFailure(failure) = &error else {
        panic!("actual callback OOM lost its completed transport: {error:?}");
    };
    let rows = observations.lock().unwrap();
    assert_eq!(rows.len(), 1, "callback executes exactly once");
    assert_eq!(failure.error, rows[0], "actual OOM tuple remains exact");
    assert!(matches!(failure.error, VmError::OutOfMemory { .. }));
    assert!(failure.is_fatal());
    assert!(error.is_fatal());
}

#[test]
fn promise_constructor_lookup_reloads_moved_receiver_after_proxy_getmethod() {
    let mut vm = Interpreter::new().expect("Promise constructor Get bootstrap");
    let source = admitted_source(&mut vm);
    vm.gc_heap_mut().set_gc_stress(0, true);
    let observations = Arc::new(Mutex::new(Vec::new()));
    let handler_sink = observations.clone();
    let target_sink = observations.clone();
    NativeCtx::with_host_context(
        &mut vm,
        NativeCallInfo::default_call(),
        Some(&source),
        |ctx| {
            ctx.scope(|mut scope| -> Result<(), NativeError> {
                let promise = pending_promise(&mut scope)?;
                let target = scope.object()?;
                let target_getter = scope.native_closure(
                    "target constructor accessor",
                    0,
                    &[promise],
                    move |ctx, _, captures| {
                        let receiver = *ctx.this_value();
                        ctx.scope(|mut scope| {
                            let receiver = scope.value(receiver);
                            let promise = scope.value(captures[0]);
                            target_sink
                                .lock()
                                .map_err(|_| NativeError::InvalidOperand)?
                                .push(("target", scope.raw(receiver) == scope.raw(promise), true));
                            let constructor =
                                scope.global("Promise").ok_or(NativeError::InvalidOperand)?;
                            Ok(scope.finish(constructor))
                        })
                    },
                )?;
                let undefined = scope.undefined();
                scope.define_accessor(
                    target,
                    "constructor",
                    target_getter,
                    undefined,
                    crate::object::PropertyFlags::new(false, false, true),
                )?;
                let handler = scope.object()?;
                let handler_getter = scope.native_closure(
                    "collecting get trap accessor",
                    0,
                    &[promise],
                    move |ctx, _, captures| {
                        ctx.scope(|mut scope| {
                            let promise = scope.value(captures[0]);
                            let before = scope.raw(promise).to_bits();
                            let cycles = scope.context().heap().gc_cycle_counts();
                            scope
                                .context()
                                .interp_mut()
                                .collect_minor_tracing_runtime_roots();
                            let moved = scope.raw(promise).to_bits() != before;
                            let collected = scope.context().heap().gc_cycle_counts().0 > cycles.0;
                            handler_sink
                                .lock()
                                .map_err(|_| NativeError::InvalidOperand)?
                                .push(("handler", moved, collected));
                            // GetMethod treats undefined as an absent trap;
                            // the real Proxy path then exposes target accessor.
                            Ok(Value::undefined())
                        })
                    },
                )?;
                scope.define_accessor(
                    handler,
                    "get",
                    handler_getter,
                    undefined,
                    crate::object::PropertyFlags::new(false, false, true),
                )?;
                let proxy = scope.proxy(target, handler)?;
                let current_promise = scope.raw(promise);
                let current_proxy = scope.raw(proxy);
                scope.with_turn_parts(|interp, _| {
                    current_promise
                        .as_promise()
                        .ok_or(NativeError::InvalidOperand)?
                        .set_prototype_override(interp.gc_heap_mut(), Some(current_proxy));
                    Ok::<_, NativeError>(())
                })?;
                let current = scope.raw(promise);
                let resolved = scope.with_turn_parts(|interp, stack| {
                    interp
                        .promise_resolve_value(stack, Some(&source), current)
                        .map_err(|error| error.into_native(interp, "PromiseResolve fixture"))
                })?;
                let resolved = scope.value(resolved);
                assert_eq!(scope.raw(resolved), scope.raw(promise));
                Ok(())
            })
        },
    )
    .expect("actual PromiseResolve through collecting Proxy lookup");
    // Promise bodies are allocated in non-moving old space
    // (`PurePromise::pending_with_roots`; see
    // `promise_fulfilled_of_allocates_the_body_in_old_space`): the trap's
    // actual minor collection runs without relocating the promise.
    assert_eq!(
        observations.lock().unwrap().as_slice(),
        &[("handler", false, true), ("target", true, true)],
        "GetMethod collects once, then getter receives the current rooted promise"
    );
}
