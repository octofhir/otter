//! Executed native-only and bytecode job admission, realm and moving-root proofs.
//!
//! # Contents
//! - Native callbacks with honest absent source and exact bytecode child sources.
//! - Direct, bound and Proxy callback realms independent of the drain realm.
//! - Scoped async/argument/capture aliases through actual minor collection.
//! - Disposed jobs, handled/unhandled reporters and fatal queue stopping.
//!
//! # Invariants
//! Callback observations are owned Rust data; assertions execute outside the
//! native ABI. Every retained VM value uses a production scope or persistent
//! root. Claimed motion uses the real collector inside the tested operation.
//!
//! # See also
//! - `super::Interpreter::reaction_realm` admits the current callback record.
//! - `crate::interp::exec` owns the single drain and reporting extent.

use super::*;
use crate::marshal::MarshalCx;
use crate::promise::JsPromise;
use crate::{Local, NativeCallInfo, NativeCtx, NativeError, NativeScope};
use otter_bytecode::{Constant, FunctionCodeBuilder, Op, Operand};
use std::sync::{Arc, Mutex};

#[derive(Debug, PartialEq)]
struct Observation {
    source: Option<u32>,
    realm: u32,
    moved: bool,
    aliases: bool,
    marker: f64,
}

/// Allocate a pending Promise under the runtime roots and park it in `scope`.
fn pending_promise<'s>(scope: &mut NativeScope<'s, '_>) -> Result<Local<'s>, NativeError> {
    let handle =
        crate::promise_dispatch::pending_runtime_rooted(scope.context().interp_mut(), &[], &[])?;
    Ok(scope.value(Value::promise(handle)))
}

fn record<T>(sink: &Mutex<Vec<T>>, value: T) -> Result<(), NativeError> {
    sink.lock()
        .map_err(|_| NativeError::InvalidOperand)?
        .push(value);
    Ok(())
}

fn number_source(vm: &mut Interpreter, name: &str, number: f64) -> ExecutionContext {
    let mut module = crate::test_support::minimal_bytecode_module(name);
    module.constants.push(Constant::Number {
        bits: number.to_bits(),
    });
    let mut code = FunctionCodeBuilder::new();
    code.push(
        Op::LoadNumber,
        &[Operand::Register(0), Operand::ConstIndex(0)],
    );
    code.push(Op::ReturnValue, &[Operand::Register(0)]);
    module.functions[0].code = code.finish();
    vm.link_module(module, crate::source_registry::SourceRegistry::default())
        .expect("actual verifier-valid source")
}

#[test]
fn native_none_job_moves_arguments_captures_and_async_aliases_without_inventing_source() {
    let mut vm = Interpreter::new().expect("job fixture bootstrap");
    vm.gc_heap_mut().set_gc_stress(0, true);
    let observed = Arc::new(Mutex::new(Vec::new()));
    let sink = observed.clone();
    let ambient_root =
        NativeCtx::with_host_context(&mut vm, NativeCallInfo::default_call(), None, |ctx| {
            ctx.scope(|mut scope| -> Result<_, NativeError> {
                let child = scope.object()?;
                let marker = scope.number(719.0);
                scope.define(
                    child,
                    "marker",
                    marker,
                    crate::object::PropertyFlags::data_default(),
                )?;
                scope.define(
                    child,
                    "self",
                    child,
                    crate::object::PropertyFlags::data_default(),
                )?;
                let callback = scope.native_closure(
                    "none-origin",
                    1,
                    &[child],
                    move |ctx, args, captures| {
                        let source = ctx.execution_context().map(ExecutionContext::function_base);
                        let realm = ctx.interp_mut().active_host_realm_id();
                        ctx.scope(|mut scope| {
                            let argument = scope.argument(args, 0);
                            let capture = scope.value(captures[0]);
                            let async_value = scope.context().async_context();
                            let async_value = scope.value(async_value);
                            let old = scope
                                .raw(argument)
                                .as_object()
                                .ok_or(NativeError::InvalidOperand)?
                                .offset();
                            let before = scope.context().heap().gc_cycle_counts();
                            scope
                                .context()
                                .interp_mut()
                                .collect_minor_tracing_runtime_roots();
                            let current = scope.raw(argument);
                            let marker = scope.get(argument, "marker")?;
                            let link = scope.get(argument, "self")?;
                            record(
                                &sink,
                                Observation {
                                    source,
                                    realm,
                                    moved: current
                                        .as_object()
                                        .ok_or(NativeError::InvalidOperand)?
                                        .offset()
                                        != old
                                        && scope.context().heap().gc_cycle_counts().0 > before.0,
                                    aliases: current == scope.raw(capture)
                                        && current == scope.raw(async_value)
                                        && current == scope.raw(link),
                                    marker: scope
                                        .raw(marker)
                                        .as_f64()
                                        .ok_or(NativeError::InvalidOperand)?,
                                },
                            )?;
                            Ok(Value::undefined())
                        })
                    },
                )?;
                let child_value = scope.raw(child);
                scope.context().set_async_context(child_value);
                scope.queue_microtask(callback, &[child])?;
                let ambient = scope.object()?;
                let ambient_value = scope.raw(ambient);
                let root = scope
                    .context()
                    .interp_mut()
                    .persistent_root_insert(ambient_value);
                scope.context().set_async_context(ambient_value);
                Ok(root)
            })
        })
        .expect("real native producer");
    vm.drain_microtasks(|_, _| Ok(false))
        .expect("actual absent-source callback");
    assert_eq!(
        observed.lock().unwrap().as_slice(),
        &[Observation {
            source: None,
            realm: 0,
            moved: true,
            aliases: true,
            marker: 719.0,
        }]
    );
    assert_eq!(
        vm.async_context(),
        vm.persistent_root_get(ambient_root).unwrap()
    );
    vm.persistent_root_remove(ambient_root);
}

#[test]
fn native_none_calls_and_getters_resolve_the_exact_callee_chunk_after_foreign_linking() {
    let mut vm = Interpreter::new().expect("exact source bootstrap");
    let origin = number_source(&mut vm, "native-child-origin.js", 719.25);
    let foreign = number_source(&mut vm, "foreign-child-source.js", 997.5);
    assert_ne!(origin.function_base(), foreign.function_base());
    let callee = Value::function(origin.function_base());
    let resolved = vm.callable_context(None, callee).unwrap().unwrap();
    assert_eq!(resolved.function_base(), origin.function_base());
    vm.gc_heap_mut().set_gc_stress(1, true);
    NativeCtx::with_host_context(&mut vm, NativeCallInfo::default_call(), None, |ctx| {
        ctx.scope(|mut scope| -> Result<(), NativeError> {
            let callee = scope.value(callee);
            let receiver = scope.undefined();
            let value = scope.call(callee, receiver, &[])?;
            assert_eq!(scope.raw(value).as_f64(), Some(719.25));
            let object = scope.object()?;
            let object_value = scope.raw(object);
            let getter = scope.raw(callee);
            let mut current = object_value.as_object().unwrap();
            crate::object::define_own_property_in_place(
                &mut current,
                scope.context().heap_mut(),
                "answer",
                crate::object::PropertyDescriptor::accessor(Some(getter), None, true, true),
            )
            .map_err(NativeError::from)?;
            // Definition is a collecting owner with the object value rooted by
            // the scoped arena. Its public local is re-read before Get.
            let answer = scope.get(object, "answer")?;
            assert_eq!(scope.raw(answer).as_f64(), Some(719.25));
            let handler = scope.object()?;
            let trap = scope.native_closure("none-get-trap", 3, &[], |ctx, args, _| {
                ctx.scope(|mut scope| {
                    let target = scope.argument(args, 0);
                    let value = scope.get(target, "answer")?;
                    Ok(scope.finish(value))
                })
            })?;
            scope.define(
                handler,
                "get",
                trap,
                crate::object::PropertyFlags::data_default(),
            )?;
            let proxy = scope.proxy(object, handler)?;
            let answer = scope.get(proxy, "answer")?;
            assert_eq!(scope.raw(answer).as_f64(), Some(719.25));
            let mut cx = MarshalCx::new(scope);
            let answer = cx
                .call(callee, receiver, &[])
                .map_err(|error| error.into_native("child"))?;
            assert_eq!(cx.as_f64(answer), Some(719.25));
            Ok(())
        })
    })
    .expect("actual None Get/call/Proxy/Marshal entry");
    assert_eq!(
        vm.callable_context(None, Value::function(u32::MAX))
            .unwrap_err(),
        VmError::InvalidOperand
    );
}

#[test]
fn direct_bound_and_proxy_native_jobs_use_creation_realm_and_disposed_jobs_do_not_run() {
    let mut vm = Interpreter::new().expect("realm jobs bootstrap");
    vm.gc_heap_mut().set_gc_stress(0, true);
    let realm = vm.create_host_realm().expect("additional realm");
    let observed = Arc::new(Mutex::new(Vec::new()));
    let sink = observed.clone();
    let roots = vm
        .with_host_realm(realm, |vm| {
            NativeCtx::with_host_context(vm, NativeCallInfo::default_call(), None, |ctx| {
                ctx.scope(|mut scope| -> Result<_, NativeError> {
                    let callback =
                        scope.native_closure("creation-realm", 0, &[], move |ctx, _, _| {
                            let source =
                                ctx.execution_context().map(ExecutionContext::function_base);
                            let realm = ctx.interp_mut().active_host_realm_id();
                            ctx.scope(|mut scope| {
                                let current = scope.context().async_context();
                                let current = scope.value(current);
                                let old = scope
                                    .raw(current)
                                    .as_object()
                                    .ok_or(NativeError::InvalidOperand)?
                                    .offset();
                                scope
                                    .context()
                                    .interp_mut()
                                    .collect_minor_tracing_runtime_roots();
                                let marker = scope.get(current, "marker")?;
                                record(
                                    &sink,
                                    (
                                        source,
                                        realm,
                                        scope.raw(marker).as_f64(),
                                        scope
                                            .raw(current)
                                            .as_object()
                                            .ok_or(NativeError::InvalidOperand)?
                                            .offset()
                                            != old,
                                    ),
                                )?;
                                Ok(Value::undefined())
                            })
                        })?;
                    let current = scope.raw(callback);
                    let bound = crate::test_support::alloc_bound_function(
                        scope.context().interp_mut(),
                        current,
                        Value::undefined(),
                        &[],
                    )
                    .map_err(|error| {
                        crate::native_function::vm_to_native_error(
                            scope.context().interp_mut(),
                            error,
                            "bound fixture",
                        )
                    })?;
                    let bound = scope.value(Value::bound_function(bound));
                    let handler = scope.object()?;
                    let proxy = scope.proxy(callback, handler)?;
                    let values = [scope.raw(callback), scope.raw(bound), scope.raw(proxy)];
                    Ok(values
                        .map(|value| scope.context().interp_mut().persistent_root_insert(value)))
                })
            })
            .map_err(|error| crate::native_to_vm_error(vm, error))
        })
        .expect("actual additional-realm factories");
    // Admission occurs in the default realm, after the callback was created
    // in another realm. Neither registration nor drain chooses its realm.
    for root in roots {
        let callback = vm.persistent_root_get(root).unwrap();
        NativeCtx::with_host_context(&mut vm, NativeCallInfo::default_call(), None, |ctx| {
            ctx.scope(|mut scope| -> Result<(), NativeError> {
                let callback = scope.value(callback);
                let async_value = scope.object()?;
                let marker = scope.number(317.0);
                scope.define(
                    async_value,
                    "marker",
                    marker,
                    crate::object::PropertyFlags::data_default(),
                )?;
                let current = scope.raw(async_value);
                scope.context().set_async_context(current);
                scope.queue_microtask(callback, &[])
            })
        })
        .expect("default realm queues foreign callback");
        vm.persistent_root_remove(root);
        vm.set_async_context(Value::undefined());
        vm.drain_microtasks(|_, _| Ok(false))
            .expect("creation-realm callback body");
    }
    assert_eq!(vm.active_host_realm_id(), 0);
    assert!(vm.async_context().is_undefined());
    let rows = observed.lock().unwrap();
    assert_eq!(rows.len(), 3);
    assert!(
        rows.iter()
            .all(|(source, id, marker, moved)| source.is_none()
                && *id == realm.0
                && *marker == Some(317.0)
                && *moved),
        "{rows:?}"
    );
    drop(rows);
    let doomed = vm.create_host_realm().unwrap();
    let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let calls = count.clone();
    vm.with_host_realm(doomed, |vm| {
        NativeCtx::with_host_context(vm, NativeCallInfo::default_call(), None, |ctx| {
            ctx.scope(|mut scope| {
                let callback = scope.native_closure("disposed-job", 0, &[], move |_, _, _| {
                    calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Ok(Value::undefined())
                })?;
                scope.queue_microtask(callback, &[])
            })
        })
        .map_err(|error| crate::native_to_vm_error(vm, error))
    })
    .unwrap();
    assert!(vm.dispose_host_realm(doomed));
    vm.force_gc()
        .expect("disposed-origin queue is still rooted before skip");
    vm.drain_microtasks(|_, _| Ok(false)).unwrap();
    assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert!(!vm.microtasks().has_any_pending());
}

#[test]
fn reporter_scope_preserves_moved_throw_on_unhandled_or_catchable_failure() {
    for catchable_failure in [false, true] {
        let mut vm = Interpreter::new().expect("reporter roots bootstrap");
        vm.gc_heap_mut().set_gc_stress(0, true);
        let original =
            NativeCtx::with_host_context(&mut vm, NativeCallInfo::default_call(), None, |ctx| {
                ctx.scope(|mut scope| -> Result<_, NativeError> {
                    let original = scope.object()?;
                    let marker = scope.number(719.0);
                    scope.define(
                        original,
                        "marker",
                        marker,
                        crate::object::PropertyFlags::data_default(),
                    )?;
                    let callback = scope.native_closure(
                        "original-throw",
                        0,
                        &[original],
                        |ctx, _, captures| {
                            ctx.interp_mut().set_pending_uncaught_throw(captures[0]);
                            Err(NativeError::Thrown {
                                name: "original",
                                message: "original identity".into(),
                            })
                        },
                    )?;
                    scope.queue_microtask(callback, &[])?;
                    let value = scope.raw(original);
                    Ok(scope.context().interp_mut().persistent_root_insert(value))
                })
            })
            .unwrap();
        let before = vm
            .persistent_root_get(original)
            .unwrap()
            .as_object()
            .unwrap()
            .offset();
        let error = vm
            .drain_microtasks(|ctx, error| {
                let source = ctx.execution_context().is_none();
                ctx.scope(|mut scope| {
                    let value = scope
                        .context()
                        .interp_mut()
                        .pending_uncaught_throw
                        .ok_or(NativeError::InvalidOperand)?;
                    let value = scope.value(value);
                    scope
                        .context()
                        .interp_mut()
                        .collect_minor_tracing_runtime_roots();
                    let marker = scope.get(value, "marker")?;
                    if !source
                        || error.error != VmError::Uncaught
                        || scope.raw(marker).as_f64() != Some(719.0)
                    {
                        return Err(NativeError::InvalidOperand);
                    }
                    if catchable_failure {
                        Err(NativeError::TypeError {
                            name: "reporter",
                            reason: "suppressed reporter failure".into(),
                        })
                    } else {
                        Ok(false)
                    }
                })
            })
            .expect_err("original unhandled completion leaves the checkpoint");
        assert_eq!(error.error, VmError::Uncaught);
        // The callback threw a JavaScript value: the host boundary projected
        // that pending identity once, consuming its in-flight host text, so the
        // job completion carries the value alone. A leaked reporter failure
        // would instead surface its own `reporter: ...` Uncaught detail.
        assert_eq!(error.detail, None);
        assert_eq!(error.message(), VmError::Uncaught.to_string());
        let current = vm.persistent_root_get(original).unwrap();
        assert_ne!(current.as_object().unwrap().offset(), before);
        assert_eq!(vm.pending_uncaught_throw, Some(current));
        assert_eq!(vm.error_detail(), error.detail);
        vm.persistent_root_remove(original);
    }
}

#[test]
fn fatal_reporter_stops_current_checkpoint_and_deliberate_later_drain_keeps_fifo() {
    let mut vm = Interpreter::new().expect("fatal reporter bootstrap");
    let calls = Arc::new(Mutex::new(Vec::new()));
    let followups = calls.clone();
    NativeCtx::with_host_context(&mut vm, NativeCallInfo::default_call(), None, |ctx| {
        ctx.scope(|mut scope| -> Result<(), NativeError> {
            let thrower = scope.native_closure("throw-before-exit", 0, &[], |_, _, _| {
                Err(NativeError::Thrown {
                    name: "job",
                    message: "report me".into(),
                })
            })?;
            let second = scope.native_closure("later-job", 0, &[], move |_, _, _| {
                record(&followups, 2)?;
                Ok(Value::undefined())
            })?;
            scope.queue_microtask(thrower, &[])?;
            scope.queue_microtask(second, &[])
        })
    })
    .unwrap();
    let error = vm
        .drain_microtasks(|_, _| Err(NativeError::Exit { code: 27 }))
        .unwrap_err();
    assert_eq!(error.error, VmError::Exit { code: 27 });
    assert!(calls.lock().unwrap().is_empty());
    assert!(vm.microtasks().has_any_pending());
    vm.drain_microtasks(|_, _| Ok(false))
        .expect("deliberate later checkpoint");
    assert_eq!(*calls.lock().unwrap(), vec![2]);
}

struct RejectionProbe {
    observations: Arc<Mutex<Vec<(Option<u32>, f64, bool, bool)>>>,
    fail: bool,
}

impl crate::promise_rejection::PromiseRejectionHook for RejectionProbe {
    fn notify(
        &self,
        ctx: &mut NativeCtx<'_>,
        promise: Value,
        reason: Value,
        handled: bool,
    ) -> Result<(), NativeError> {
        if handled {
            return Ok(());
        }
        let source = ctx.execution_context().map(ExecutionContext::function_base);
        ctx.scope(|mut scope| {
            let promise = scope.value(promise);
            let reason = scope.value(reason);
            let current = scope.context().async_context();
            let current = scope.value(current);
            let old = scope
                .raw(current)
                .as_object()
                .ok_or(NativeError::InvalidOperand)?
                .offset();
            scope
                .context()
                .interp_mut()
                .collect_minor_tracing_runtime_roots();
            let marker = scope.get(current, "marker")?;
            let payload = scope
                .raw(promise)
                .as_promise()
                .ok_or(NativeError::InvalidOperand)?;
            let crate::promise::PromiseState::Rejected(retained) =
                payload.state(scope.context().heap())
            else {
                return Err(NativeError::InvalidOperand);
            };
            record(
                &self.observations,
                (
                    source,
                    scope
                        .raw(marker)
                        .as_f64()
                        .ok_or(NativeError::InvalidOperand)?,
                    retained == scope.raw(reason),
                    scope
                        .raw(current)
                        .as_object()
                        .ok_or(NativeError::InvalidOperand)?
                        .offset()
                        != old,
                ),
            )?;
            if self.fail {
                Err(NativeError::InvalidOperand)
            } else {
                Ok(())
            }
        })
    }
}

#[test]
fn rejection_checkpoint_uses_each_actual_operation_source_async_and_restores_after_failure() {
    for fail in [false, true] {
        let mut vm = Interpreter::new().expect("rejection origin bootstrap");
        vm.gc_heap_mut().set_gc_stress(0, true);
        let first = number_source(&mut vm, "first-rejection-origin.js", 317.0);
        let second = number_source(&mut vm, "second-rejection-origin.js", 719.0);
        let observed = Arc::new(Mutex::new(Vec::new()));
        vm.set_promise_rejection_hook(crate::promise_rejection::PromiseRejectionHookHandle::new(
            RejectionProbe {
                observations: observed.clone(),
                fail,
            },
        ));
        for (source, marker) in [(&first, 317.0), (&second, 719.0)] {
            NativeCtx::with_host_context(
                &mut vm,
                NativeCallInfo::default_call(),
                Some(source),
                |ctx| {
                    ctx.scope(|mut scope| -> Result<(), NativeError> {
                        let async_value = scope.object()?;
                        let marker = scope.number(marker);
                        scope.define(
                            async_value,
                            "marker",
                            marker,
                            crate::object::PropertyFlags::data_default(),
                        )?;
                        let current = scope.raw(async_value);
                        scope.context().set_async_context(current);
                        let callback =
                            scope.native_closure("reject-in-job", 0, &[], |ctx, _, _| {
                                ctx.scope(|mut scope| {
                                    let reason = scope.object()?;
                                    let _promise = scope.promise_rejected(reason)?;
                                    Ok(Value::undefined())
                                })
                            })?;
                        scope.queue_microtask(callback, &[])
                    })
                },
            )
            .unwrap();
        }
        let ambient =
            NativeCtx::with_host_context(&mut vm, NativeCallInfo::default_call(), None, |ctx| {
                ctx.scope(|mut scope| -> Result<_, NativeError> {
                    let ambient = scope.object()?;
                    let current = scope.raw(ambient);
                    scope.context().set_async_context(current);
                    Ok(scope.context().interp_mut().persistent_root_insert(current))
                })
            })
            .unwrap();
        let before = vm.gc_heap().gc_cycle_counts();
        let result = vm.drain_microtasks(|_, _| Ok(false));
        if fail {
            assert_eq!(result.unwrap_err().error, VmError::InvalidOperand);
        } else {
            result.unwrap();
        }
        assert!(vm.gc_heap().gc_cycle_counts().0 > before.0);
        assert_eq!(vm.async_context(), vm.persistent_root_get(ambient).unwrap());
        let rows = observed.lock().unwrap();
        assert_eq!(rows[0], (Some(first.function_base()), 317.0, true, true));
        assert_eq!(rows.len(), if fail { 1 } else { 2 });
        if !fail {
            // The first notification promotes the next origin, so its own
            // second collection need not move again. Alias and operation
            // context/marker still prove it did not inherit the first origin.
            assert_eq!(rows[1].0, Some(second.function_base()));
            assert_eq!(rows[1].1, 719.0);
            assert!(rows[1].2, "second reason aliases its moved promise payload");
        }
        drop(rows);
        vm.persistent_root_remove(ambient);
    }
}

struct ReentrantRejectionProbe {
    observations: Arc<Mutex<Vec<u8>>>,
}

impl crate::promise_rejection::PromiseRejectionHook for ReentrantRejectionProbe {
    fn notify(
        &self,
        ctx: &mut NativeCtx<'_>,
        _: Value,
        _: Value,
        handled: bool,
    ) -> Result<(), NativeError> {
        if handled {
            return Ok(());
        }
        record(&self.observations, 1)?;
        let observations = self.observations.clone();
        ctx.scope(|mut scope| {
            let followup = scope.native_closure("hook followup", 0, &[], move |_, _, _| {
                record(&observations, 3)?;
                Ok(Value::undefined())
            })?;
            scope.queue_microtask(followup, &[])
        })?;
        let outcome = ctx.interp_mut().drain_microtasks(|_, _| Ok(false));
        outcome.map_err(|error| {
            crate::native_function::vm_to_native_error(
                ctx.interp_mut(),
                error.error,
                "nested checkpoint",
            )
        })?;
        record(&self.observations, 2)
    }
}

#[test]
fn rejection_hook_reentrant_drain_is_absorbed_by_the_existing_outer_checkpoint() {
    let mut vm = Interpreter::new().expect("reentrant checkpoint bootstrap");
    let observations = Arc::new(Mutex::new(Vec::new()));
    vm.set_promise_rejection_hook(crate::promise_rejection::PromiseRejectionHookHandle::new(
        ReentrantRejectionProbe {
            observations: observations.clone(),
        },
    ));
    NativeCtx::with_host_context(&mut vm, NativeCallInfo::default_call(), None, |ctx| {
        ctx.scope(|mut scope| {
            let reason = scope.number(719.0);
            let _promise = scope.promise_rejected(reason)?;
            Ok::<_, NativeError>(())
        })
    })
    .unwrap();
    vm.drain_microtasks(|_, _| Ok(false))
        .expect("one complete outer drain");
    assert_eq!(observations.lock().unwrap().as_slice(), &[1, 2, 3]);
    assert!(!vm.microtasks().has_any_pending());
}

#[test]
fn finalization_jobs_keep_creation_realm_async_and_held_aliases_through_actual_gc() {
    let mut vm = Interpreter::new().expect("finalization origin bootstrap");
    vm.gc_heap_mut().set_gc_stress(0, true);
    let realm = vm.create_host_realm().unwrap();
    let observations = Arc::new(Mutex::new(Vec::new()));
    let sink = observations.clone();
    let (registry_root, original_offset) = vm
        .with_host_realm(realm, |vm| {
            NativeCtx::with_host_context(vm, NativeCallInfo::default_call(), None, |ctx| {
                ctx.scope(|mut scope| -> Result<_, NativeError> {
                    let origin = scope.object()?;
                    let marker = scope.number(317.0);
                    scope.define(
                        origin,
                        "marker",
                        marker,
                        crate::object::PropertyFlags::data_default(),
                    )?;
                    let current = scope.raw(origin);
                    scope.context().set_async_context(current);
                    let original_offset = current.as_object().unwrap().offset();
                    let cleanup = scope.native_closure(
                        "finalization origin",
                        1,
                        &[],
                        move |ctx, args, _| {
                            let source =
                                ctx.execution_context().map(ExecutionContext::function_base);
                            let realm_id = ctx.interp_mut().active_host_realm_id();
                            ctx.scope(|mut scope| {
                                let held = scope.argument(args, 0);
                                let current = scope.context().async_context();
                                let current = scope.value(current);
                                let marker = scope.get(current, "marker")?;
                                let child = scope.object()?;
                                let before = scope
                                    .raw(child)
                                    .as_object()
                                    .ok_or(NativeError::InvalidOperand)?
                                    .offset();
                                scope
                                    .context()
                                    .interp_mut()
                                    .collect_minor_tracing_runtime_roots();
                                record(
                                    &sink,
                                    (
                                        source,
                                        realm_id,
                                        scope.raw(held) == scope.raw(current),
                                        scope.raw(marker).as_f64(),
                                        scope
                                            .raw(current)
                                            .as_object()
                                            .ok_or(NativeError::InvalidOperand)?
                                            .offset(),
                                        scope
                                            .raw(child)
                                            .as_object()
                                            .ok_or(NativeError::InvalidOperand)?
                                            .offset()
                                            != before,
                                    ),
                                )?;
                                Ok(Value::undefined())
                            })
                        },
                    )?;
                    let callback = scope.raw(cleanup);
                    let registry = scope
                        .context()
                        .alloc_finalization_registry(callback, None, &[], &[])
                        .map_err(|error| {
                            crate::native_function::vm_to_native_error(
                                scope.context().interp_mut(),
                                error,
                                "finalization fixture",
                            )
                        })?;
                    let registry = scope.value(Value::finalization_registry(registry));
                    let target = scope.object()?;
                    let current = scope.raw(target);
                    let held = scope.raw(origin);
                    let registry_value = scope.raw(registry);
                    crate::weak_refs::finalization_registry_register(
                        registry_value
                            .as_finalization_registry()
                            .ok_or(NativeError::InvalidOperand)?,
                        scope.context().heap_mut(),
                        &current,
                        held,
                        None,
                    )
                    .map_err(|error| {
                        crate::native_function::vm_to_native_error(
                            scope.context().interp_mut(),
                            error,
                            "register fixture",
                        )
                    })?;
                    let root = scope
                        .context()
                        .interp_mut()
                        .persistent_root_insert(registry_value);
                    Ok((root, original_offset))
                })
            })
            .map_err(|error| crate::native_to_vm_error(vm, error))
        })
        .expect("real registry producer");
    let ambient =
        NativeCtx::with_host_context(&mut vm, NativeCallInfo::default_call(), None, |ctx| {
            ctx.scope(|mut scope| -> Result<_, NativeError> {
                let ambient = scope.object()?;
                let current = scope.raw(ambient);
                scope.context().set_async_context(current);
                Ok(scope.context().interp_mut().persistent_root_insert(current))
            })
        })
        .unwrap();
    let before = vm.gc_heap().gc_cycle_counts();
    vm.force_gc().expect("actual weak target collection");
    assert!(vm.gc_heap().gc_cycle_counts().1 > before.1);
    assert!(
        observations.lock().unwrap().is_empty(),
        "GC only enqueues cleanup"
    );
    vm.drain_microtasks(|_, _| Ok(false))
        .expect("admitted finalization callback");
    let rows = observations.lock().unwrap();
    assert_eq!(rows.len(), 1);
    let (source, id, aliases, marker, current_offset, child_moved) = rows[0];
    assert_eq!(source, None);
    assert_eq!(id, realm.0);
    assert!(aliases);
    assert_eq!(marker, Some(317.0));
    assert_ne!(current_offset, original_offset);
    assert!(
        child_moved,
        "the actual callback also collects with active roots"
    );
    assert_eq!(vm.active_host_realm_id(), 0);
    assert_eq!(vm.async_context(), vm.persistent_root_get(ambient).unwrap());
    drop(rows);
    vm.persistent_root_remove(ambient);
    vm.persistent_root_remove(registry_root);
}

fn generator_source(vm: &mut Interpreter, asynchronous: bool) -> ExecutionContext {
    let mut module = crate::test_support::minimal_bytecode_module("native-generator-origin.js");
    module.constants.push(Constant::Number {
        bits: 719.25f64.to_bits(),
    });
    let mut code = FunctionCodeBuilder::new();
    code.push(Op::GeneratorStart, &[]);
    code.push(
        Op::LoadNumber,
        &[Operand::Register(0), Operand::ConstIndex(0)],
    );
    code.push(Op::ReturnValue, &[Operand::Register(0)]);
    module.functions[0].code = code.finish();
    module.functions[0].is_generator = true;
    module.functions[0].is_async = asynchronous;
    // The runtime keys AsyncGenerator entry and its Promise-returning
    // next/return/throw on this flag, exactly as the compiler emits it.
    module.functions[0].is_async_generator = asynchronous;
    vm.link_module(module, crate::source_registry::SourceRegistry::default())
        .expect("actual generator verifier contract")
}

#[test]
fn native_none_generator_next_return_throw_resolve_parked_source_and_move_return_aliases() {
    let mut vm = Interpreter::new().expect("native generator bootstrap");
    let realm = vm.create_host_realm().unwrap();
    let source = vm
        .with_host_realm(realm, |vm| Ok(generator_source(vm, false)))
        .unwrap();
    let foreign = number_source(&mut vm, "foreign-generator-drain.js", 997.0);
    assert_ne!(source.function_base(), foreign.function_base());
    vm.gc_heap_mut().set_gc_stress(1, true);
    NativeCtx::with_host_context(&mut vm, NativeCallInfo::default_call(), None, |ctx| {
        ctx.scope(|mut scope| -> Result<(), NativeError> {
            let factory = scope.value(Value::function(source.function_base()));
            let undefined = scope.undefined();
            let generator = scope.call(factory, undefined, &[])?;
            let next = scope.get(generator, "next")?;
            let result = scope.call(next, generator, &[])?;
            let done = scope.get(result, "done")?;
            let value = scope.get(result, "value")?;
            assert_eq!(scope.raw(done).as_boolean(), Some(true));
            assert_eq!(scope.raw(value).as_f64(), Some(719.25));
            let result = scope.call(next, generator, &[])?;
            let done = scope.get(result, "done")?;
            let value = scope.get(result, "value")?;
            assert_eq!(scope.raw(done).as_boolean(), Some(true));
            assert!(scope.raw(value).is_undefined());
            let generator = scope.call(factory, undefined, &[])?;
            let method = scope.get(generator, "return")?;
            let child = scope.object()?;
            let marker = scope.number(317.0);
            scope.define(
                child,
                "marker",
                marker,
                crate::object::PropertyFlags::data_default(),
            )?;
            scope.define(
                child,
                "self",
                child,
                crate::object::PropertyFlags::data_default(),
            )?;
            let before = scope.raw(child).as_object().unwrap().offset();
            let cycles = scope.context().heap().gc_cycle_counts();
            let result = scope.call(method, generator, &[child])?;
            let retained = scope.get(result, "value")?;
            let link = scope.get(retained, "self")?;
            assert_eq!(scope.raw(retained), scope.raw(child));
            assert_eq!(scope.raw(link), scope.raw(child));
            assert_ne!(scope.raw(child).as_object().unwrap().offset(), before);
            assert!(scope.context().heap().gc_cycle_counts().0 > cycles.0);
            let generator = scope.call(factory, undefined, &[])?;
            let method = scope.get(generator, "throw")?;
            let error = scope
                .call(method, generator, &[child])
                .expect_err("original native throw completion");
            assert!(matches!(error, NativeError::Thrown { .. }));
            let current_child = scope.raw(child);
            assert_eq!(
                scope.context().interp_mut().pending_uncaught_throw,
                Some(current_child)
            );
            assert!(scope.context().execution_context().is_none());
            assert_eq!(scope.context().interp_mut().active_host_realm_id(), 0);
            Ok(())
        })
    })
    .expect("actual NativeScope calls into linked parked generator");
}

#[test]
fn native_none_async_generator_completion_and_then_use_existing_optional_job_owner() {
    let mut vm = Interpreter::new().expect("native async generator bootstrap");
    let source = generator_source(&mut vm, true);
    vm.gc_heap_mut().set_gc_stress(1, true);
    let observations = Arc::new(Mutex::new(Vec::new()));
    let sink = observations.clone();
    NativeCtx::with_host_context(&mut vm, NativeCallInfo::default_call(), None, |ctx| {
        ctx.scope(|mut scope| -> Result<(), NativeError> {
            let factory = scope.value(Value::function(source.function_base()));
            let undefined = scope.undefined();
            let generator = scope.call(factory, undefined, &[])?;
            let method = scope.get(generator, "next")?;
            let promise = scope.call(method, generator, &[])?;
            let observer =
                scope.native_closure("async generator result", 1, &[], move |ctx, args, _| {
                    let source = ctx.execution_context().is_none();
                    ctx.scope(|mut scope| {
                        let result = scope.argument(args, 0);
                        let value = scope.get(result, "value")?;
                        let done = scope.get(result, "done")?;
                        record(
                            &sink,
                            (
                                source,
                                scope.raw(value).as_f64(),
                                scope.raw(done).as_boolean(),
                            ),
                        )?;
                        Ok(Value::undefined())
                    })
                })?;
            let then = scope.get(promise, "then")?;
            let _downstream = scope.call(then, promise, &[observer])?;
            Ok(())
        })
    })
    .expect("native None async generator and Promise.then admission");
    vm.drain_microtasks(|_, _| Ok(false))
        .expect("actual result reaction");
    assert_eq!(
        observations.lock().unwrap().as_slice(),
        &[(true, Some(719.25), Some(true))]
    );
}

#[test]
fn native_none_async_from_sync_next_and_missing_return_use_optional_child_and_job_owners() {
    let mut vm = Interpreter::new().expect("native adapter bootstrap");
    let source = number_source(&mut vm, "adapter-producer.js", 719.25);
    vm.gc_heap_mut().set_gc_stress(0, true);
    let observations = Arc::new(Mutex::new(Vec::new()));
    let sink = observations.clone();
    NativeCtx::with_host_context(&mut vm, NativeCallInfo::default_call(), None, |ctx| {
        ctx.scope(|mut scope| -> Result<(), NativeError> {
            let iterator = scope.object()?;
            let next = scope.native_closure("native sync next", 0, &[], |ctx, _, _| {
                if ctx.execution_context().is_some() {
                    return Err(NativeError::InvalidOperand);
                }
                ctx.scope(|mut scope| {
                    let child = scope.object()?;
                    let marker = scope.number(719.0);
                    scope.define(
                        child,
                        "marker",
                        marker,
                        crate::object::PropertyFlags::data_default(),
                    )?;
                    scope.define(
                        child,
                        "self",
                        child,
                        crate::object::PropertyFlags::data_default(),
                    )?;
                    let result = scope.iterator_result(child, false)?;
                    Ok(scope.finish(result))
                })
            })?;
            scope.define(
                iterator,
                "next",
                next,
                crate::object::PropertyFlags::data_default(),
            )?;
            let current = scope.raw(iterator);
            let adapter = scope.with_turn_parts(|interp, stack| {
                interp
                    .create_async_from_sync_iterator(stack, &source, current)
                    .map_err(|error| error.into_native(interp, "adapter producer"))
            })?;
            let adapter = scope.value(adapter);
            let next = scope.get(adapter, "next")?;
            let result = scope.call(next, adapter, &[])?;
            let observer =
                scope.native_closure("native adapter observe", 1, &[], move |ctx, args, _| {
                    let absent_source = ctx.execution_context().is_none();
                    ctx.scope(|mut scope| {
                        let result = scope.argument(args, 0);
                        let child = scope.get(result, "value")?;
                        let before = scope
                            .raw(child)
                            .as_object()
                            .ok_or(NativeError::InvalidOperand)?
                            .offset();
                        scope
                            .context()
                            .interp_mut()
                            .collect_minor_tracing_runtime_roots();
                        let done = scope.get(result, "done")?;
                        let marker = scope.get(child, "marker")?;
                        let link = scope.get(child, "self")?;
                        record(
                            &sink,
                            (
                                absent_source,
                                scope.raw(done).as_boolean(),
                                scope.raw(marker).as_f64(),
                                scope.raw(child) == scope.raw(link),
                                scope
                                    .raw(child)
                                    .as_object()
                                    .ok_or(NativeError::InvalidOperand)?
                                    .offset()
                                    != before,
                            ),
                        )?;
                        Ok(Value::undefined())
                    })
                })?;
            let then = scope.get(result, "then")?;
            let _ = scope.call(then, result, &[observer])?;
            Ok(())
        })
    })
    .expect("real native-only adapter next/then");
    vm.drain_microtasks(|_, _| Ok(false))
        .expect("adapter result through sole job owner");
    assert_eq!(
        observations.lock().unwrap().as_slice(),
        &[(true, Some(false), Some(719.0), true, true)]
    );

    NativeCtx::with_host_context(&mut vm, NativeCallInfo::default_call(), None, |ctx| {
        ctx.scope(|mut scope| -> Result<(), NativeError> {
            let iterator = scope.object()?;
            let next = scope.native_closure("finished sync next", 0, &[], |ctx, _, _| {
                ctx.scope(|mut scope| {
                    let undefined = scope.undefined();
                    let result = scope.iterator_result(undefined, true)?;
                    Ok(scope.finish(result))
                })
            })?;
            scope.define(
                iterator,
                "next",
                next,
                crate::object::PropertyFlags::data_default(),
            )?;
            let current = scope.raw(iterator);
            let adapter = scope.with_turn_parts(|interp, stack| {
                interp
                    .create_async_from_sync_iterator(stack, &source, current)
                    .map_err(|error| error.into_native(interp, "adapter producer"))
            })?;
            let adapter = scope.value(adapter);
            let method = scope.get(adapter, "return")?;
            let child = scope.object()?;
            let marker = scope.number(317.0);
            scope.define(
                child,
                "marker",
                marker,
                crate::object::PropertyFlags::data_default(),
            )?;
            let before = scope.raw(child).as_object().unwrap().offset();
            scope.context().heap_mut().set_gc_stress(1, true);
            let promise = scope.call(method, adapter, &[child])?;
            let handle = scope
                .raw(promise)
                .as_promise()
                .ok_or(NativeError::InvalidOperand)?;
            let crate::promise::PromiseState::Fulfilled(result) =
                handle.state(scope.context().heap())
            else {
                return Err(NativeError::InvalidOperand);
            };
            let result = scope.value(result);
            let value = scope.get(result, "value")?;
            let done = scope.get(result, "done")?;
            assert_eq!(scope.raw(value), scope.raw(child));
            assert_eq!(scope.raw(done).as_boolean(), Some(true));
            assert_ne!(scope.raw(child).as_object().unwrap().offset(), before);
            Ok(())
        })
    })
    .expect("missing return has no fake source requirement or swallowed Promise allocation");
}

#[test]
fn top_level_await_drain_returns_the_exact_owned_fatal_detail() {
    let mut vm = Interpreter::new().expect("TLA detail fixture bootstrap");
    let mut module = crate::test_support::minimal_bytecode_module("tla-detail.js");
    module.functions[0].is_async = true;
    let source = vm
        .link_module(module, crate::source_registry::SourceRegistry::default())
        .expect("actual async entry source");
    NativeCtx::with_host_context(&mut vm, NativeCallInfo::default_call(), None, |ctx| {
        ctx.scope(|mut scope| {
            let callback = scope.native_closure("TLA exact budget", 0, &[], |_, _, _| {
                Err(NativeError::BudgetExceeded {
                    reason: "TLA exact budget detail".into(),
                })
            })?;
            scope.queue_microtask(callback, &[])
        })
    })
    .expect("actual queued fatal callback");
    let error = vm
        .run(&source)
        .expect_err("TLA must retain the drain failure");
    assert_eq!(error.error, crate::VmError::BudgetExceeded);
    assert!(
        matches!(error.detail.as_ref(), Some(crate::ErrorDetail::Message(message))
        if message.as_ref() == "TLA exact budget detail")
    );
    assert_eq!(error.message(), "TLA exact budget detail");
    assert!(error.is_fatal());
}

#[test]
fn async_from_sync_fatal_promise_lookup_never_starts_iterator_close() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let mut vm = Interpreter::new().expect("adapter fatal-close bootstrap");
    let source = number_source(&mut vm, "adapter-fatal-producer.js", 719.25);
    vm.gc_heap_mut().set_gc_stress(0, true);
    let closes = Arc::new(AtomicUsize::new(0));
    let observations = Arc::new(Mutex::new(Vec::new()));
    let close_count = closes.clone();
    let sink = observations.clone();
    NativeCtx::with_host_context(&mut vm, NativeCallInfo::default_call(), None, |ctx| {
        ctx.scope(|mut scope| -> Result<(), NativeError> {
            let promise = pending_promise(&mut scope)?;
            let getter = scope.native_closure(
                "fatal Promise constructor",
                0,
                &[promise],
                move |ctx, _, captures| {
                    let receiver = *ctx.this_value();
                    ctx.scope(|mut scope| {
                        let receiver = scope.value(receiver);
                        let captured = scope.value(captures[0]);
                        let before = scope.raw(receiver).to_bits();
                        let cycles = scope.context().heap().gc_cycle_counts();
                        scope
                            .context()
                            .interp_mut()
                            .collect_minor_tracing_runtime_roots();
                        record(
                            &sink,
                            (
                                scope.raw(receiver).to_bits() != before,
                                scope.raw(receiver) == scope.raw(captured),
                                scope.context().heap().gc_cycle_counts().0 > cycles.0,
                            ),
                        )?;
                        Err(NativeError::Exit { code: 42 })
                    })
                },
            )?;
            let constructor = scope.global("Promise").ok_or(NativeError::InvalidOperand)?;
            let prototype = scope.get(constructor, "prototype")?;
            let undefined = scope.undefined();
            scope.define_accessor(
                prototype,
                "constructor",
                getter,
                undefined,
                crate::object::PropertyFlags::new(false, false, true),
            )?;
            let iterator = scope.object()?;
            let next =
                scope.native_closure("promise sync next", 0, &[promise], |ctx, _, captures| {
                    ctx.scope(|mut scope| {
                        let promise = scope.value(captures[0]);
                        let result = scope.iterator_result(promise, false)?;
                        Ok(scope.finish(result))
                    })
                })?;
            let close =
                scope.native_closure("must not close after fatal", 0, &[], move |ctx, _, _| {
                    close_count.fetch_add(1, Ordering::SeqCst);
                    ctx.scope(|mut scope| {
                        let undefined = scope.undefined();
                        let result = scope.iterator_result(undefined, true)?;
                        Ok(scope.finish(result))
                    })
                })?;
            scope.define(
                iterator,
                "next",
                next,
                crate::object::PropertyFlags::data_default(),
            )?;
            scope.define(
                iterator,
                "return",
                close,
                crate::object::PropertyFlags::data_default(),
            )?;
            let current = scope.raw(iterator);
            let adapter = scope.with_turn_parts(|interp, stack| {
                interp
                    .create_async_from_sync_iterator(stack, &source, current)
                    .map_err(|error| error.into_native(interp, "adapter producer"))
            })?;
            let adapter = scope.value(adapter);
            let next = scope.get(adapter, "next")?;
            let error = scope
                .call(next, adapter, &[])
                .expect_err("actual Exit from Promise constructor getter");
            assert_eq!(error.exit_code(), Some(42));
            assert!(error.is_fatal());
            Ok(())
        })
    })
    .expect("outside callback fatal-close observation");
    assert_eq!(
        closes.load(Ordering::SeqCst),
        0,
        "fatal must precede any observable Iterator.return"
    );
    // Promise bodies are allocated in non-moving old space
    // (`PurePromise::pending_with_roots`; see
    // `promise_fulfilled_of_allocates_the_body_in_old_space`), so the actual
    // minor collection runs without relocating the receiver. The proof is
    // that the collection ran and the reloaded receiver still aliases the
    // captured promise.
    assert_eq!(
        observations.lock().unwrap().as_slice(),
        &[(false, true, true)],
        "original promise/receiver survive an actual collection and keep their capture alias"
    );
}
