//! Actual Get/Call cleanup and moving incoming-completion proofs.
//!
//! # Contents
//! - Absent or rooted incoming throws through collecting `return` getters/calls.
//! - Catchable suppression and exact fatal/control cleanup propagation.
//! - Unlinked/disposed generator admission before throw-preserving Close.
//! - Actual allocator refusal retaining distinct local and completed native domains.
//!
//! # Invariants
//! Cleanup runs through the production call boundary with honest absent source.
//! Incoming identity is retained only in a traced scope. Collecting-cleanup
//! callbacks move the receiver before producing their completion. Admission
//! fixtures never dispatch a generator body. New proof assertions stay outside
//! the callback; allocator causes come from a real heap cap refusal.
//!
//! # See also
//! - `super::Interpreter::preserving_iterator_throw_completion`.

use crate::{Interpreter, NativeCallInfo, NativeCtx, NativeError, Value, VmError};
use std::sync::{Arc, Mutex};

#[test]
fn collecting_return_get_and_call_preserve_incoming_throw_or_exact_absence() {
    for getter in [false, true] {
        for has_incoming in [false, true] {
            let mut vm = Interpreter::new().expect("IteratorClose fixture bootstrap");
            vm.gc_heap_mut().set_gc_stress(0, true);
            let observations = Arc::new(Mutex::new(Vec::new()));
            let sink = observations.clone();
            let (before, root) = NativeCtx::with_host_context(
                &mut vm,
                NativeCallInfo::default_call(),
                None,
                |ctx| {
                    ctx.scope(|mut scope| -> Result<_, NativeError> {
                        let original = scope.object()?;
                        let marker = scope.number(719.0);
                        scope.define(
                            original,
                            "marker",
                            marker,
                            crate::object::PropertyFlags::data_default(),
                        )?;
                        let iterator = scope.object()?;
                        let cleanup = scope.native_closure(
                            "collecting cleanup",
                            0,
                            &[],
                            move |ctx, _, _| {
                                let absent_source = ctx.execution_context().is_none();
                                ctx.scope(|mut scope| {
                                    let receiver = scope.this();
                                    let old = scope
                                        .raw(receiver)
                                        .as_object()
                                        .ok_or(NativeError::InvalidOperand)?
                                        .offset();
                                    scope
                                        .context()
                                        .interp_mut()
                                        .collect_minor_tracing_runtime_roots();
                                    let moved = scope
                                        .raw(receiver)
                                        .as_object()
                                        .ok_or(NativeError::InvalidOperand)?
                                        .offset()
                                        != old;
                                    sink.lock()
                                        .map_err(|_| NativeError::InvalidOperand)?
                                        .push((absent_source, moved));
                                    Err(NativeError::SpecError {
                                        kind: crate::ErrorKind::SyntaxError,
                                        message: "subordinate cleanup".into(),
                                    })
                                })
                            },
                        )?;
                        if getter {
                            let mut object = scope.raw(iterator).as_object().unwrap();
                            let callable = scope.raw(cleanup);
                            crate::object::define_own_property_in_place(
                                &mut object,
                                scope.context().heap_mut(),
                                "return",
                                crate::object::PropertyDescriptor::accessor(
                                    Some(callable),
                                    None,
                                    true,
                                    true,
                                ),
                            )
                            .map_err(NativeError::from)?;
                        } else {
                            scope.define(
                                iterator,
                                "return",
                                cleanup,
                                crate::object::PropertyFlags::data_default(),
                            )?;
                        }
                        let original = scope.raw(original);
                        let before = original.as_object().unwrap().offset();
                        let root = scope
                            .context()
                            .interp_mut()
                            .persistent_root_insert(original);
                        let detail = if has_incoming {
                            scope
                                .context()
                                .interp_mut()
                                .set_pending_uncaught_throw(original);
                            let _ = scope
                                .context()
                                .interp_mut()
                                .err_uncaught("original completion".into());
                            scope.context().interp_mut().error_detail()
                        } else {
                            None
                        };
                        let current = scope.raw(iterator);
                        scope
                            .with_turn_parts(|interp, stack| {
                                interp.iterator_close_discarding_completion(stack, None, &current)
                            })
                            .map_err(|error| {
                                error.into_native(
                                    scope.context().interp_mut(),
                                    "IteratorClose fixture",
                                )
                            })?;
                        let interp = scope.context().interp_mut();
                        if has_incoming {
                            assert_eq!(
                                interp.pending_uncaught_throw,
                                interp.persistent_root_get(root)
                            );
                        } else {
                            assert!(interp.pending_uncaught_throw.is_none());
                        }
                        assert_eq!(interp.error_detail(), detail);
                        assert!(interp.pending_throw_provenance.is_none());
                        Ok((before, root))
                    })
                },
            )
            .expect("catchable close is subordinate to the incoming completion");
            assert_eq!(observations.lock().unwrap().as_slice(), &[(true, true)]);
            assert_ne!(
                vm.persistent_root_get(root)
                    .unwrap()
                    .as_object()
                    .unwrap()
                    .offset(),
                before
            );
            assert!(vm.gc_heap().gc_cycle_counts().0 > 0);
            vm.persistent_root_remove(root);
        }
    }
}

#[test]
fn collecting_return_get_and_call_fatal_cleanup_replaces_incoming_completion() {
    for getter in [false, true] {
        for fatal in [
            VmError::InvalidOperand,
            VmError::Exit { code: 27 },
            VmError::MissingReturn,
        ] {
            let mut vm = Interpreter::new().expect("fatal IteratorClose fixture bootstrap");
            vm.gc_heap_mut().set_gc_stress(0, true);
            let calls = Arc::new(Mutex::new(Vec::new()));
            let sink = calls.clone();
            let result = NativeCtx::with_host_context(
                &mut vm,
                NativeCallInfo::default_call(),
                None,
                |ctx| {
                    ctx.scope(|mut scope| -> Result<_, NativeError> {
                        let original = scope.object()?;
                        let iterator = scope.object()?;
                        let cleanup =
                            scope.native_closure("fatal cleanup", 0, &[], move |ctx, _, _| {
                                ctx.scope(|mut scope| {
                                    let receiver = scope.this();
                                    let before = scope
                                        .raw(receiver)
                                        .as_object()
                                        .ok_or(NativeError::InvalidOperand)?
                                        .offset();
                                    scope
                                        .context()
                                        .interp_mut()
                                        .collect_minor_tracing_runtime_roots();
                                    let moved = scope
                                        .raw(receiver)
                                        .as_object()
                                        .ok_or(NativeError::InvalidOperand)?
                                        .offset()
                                        != before;
                                    sink.lock()
                                        .map_err(|_| NativeError::InvalidOperand)?
                                        .push(moved);
                                    Err(match fatal {
                                        VmError::InvalidOperand => NativeError::InvalidOperand,
                                        VmError::MissingReturn => NativeError::MissingReturn,
                                        VmError::Exit { code } => NativeError::Exit { code },
                                        _ => return Err(NativeError::InvalidOperand),
                                    })
                                })
                            })?;
                        if getter {
                            let mut object = scope.raw(iterator).as_object().unwrap();
                            let callable = scope.raw(cleanup);
                            crate::object::define_own_property_in_place(
                                &mut object,
                                scope.context().heap_mut(),
                                "return",
                                crate::object::PropertyDescriptor::accessor(
                                    Some(callable),
                                    None,
                                    true,
                                    true,
                                ),
                            )
                            .map_err(NativeError::from)?;
                        } else {
                            scope.define(
                                iterator,
                                "return",
                                cleanup,
                                crate::object::PropertyFlags::data_default(),
                            )?;
                        }
                        let original = scope.raw(original);
                        scope
                            .context()
                            .interp_mut()
                            .set_pending_uncaught_throw(original);
                        let _ = scope
                            .context()
                            .interp_mut()
                            .err_uncaught("original completion".into());
                        let current = scope.raw(iterator);
                        let result = scope.with_turn_parts(|interp, stack| {
                            interp.iterator_close_discarding_completion(stack, None, &current)
                        });
                        // The preservation extent took the original pending
                        // root before cleanup; fatal return must not restore it.
                        assert!(
                            scope
                                .context()
                                .interp_mut()
                                .pending_uncaught_throw
                                .is_none()
                        );
                        Ok(result)
                    })
                },
            )
            .expect("fixture setup returns an owned cleanup outcome");
            assert!(
                matches!(result, Err(crate::CommittedValueError::Fatal(error)) if error == fatal)
            );
            assert_eq!(calls.lock().unwrap().as_slice(), &[true]);
            assert!(vm.gc_heap().gc_cycle_counts().0 > 0);
        }
    }
}

#[test]
fn generator_source_and_disposed_realm_admission_are_not_suppressed_by_throw_close() {
    for disposed_realm in [false, true] {
        let mut vm = Interpreter::new().expect("generator admission fixture bootstrap");
        let realm = vm.create_host_realm().expect("generator creation realm");
        let (linked, mut function) = vm
            .with_host_realm(realm, |interp| {
                let module = crate::test_support::minimal_bytecode_module("generator-admission.js");
                let function = module.functions[0].clone();
                // The fixture only needs a verified source owner. The parked
                // frame is rejected before dispatch and never runs this body.
                Ok((
                    interp.link_module(module, crate::source_registry::SourceRegistry::default()),
                    function,
                ))
            })
            .expect("enter actual creation realm");
        let context = linked.expect("verified source linked in creation realm");
        function.id = if disposed_realm {
            context.main().id
        } else {
            // Explicit malformed parked source tests admission, rather than
            // presenting a fabricated native or entered-bytecode proof.
            u32::MAX
        };
        let frame = vm
            .test_frame_for_function(&function)
            .expect("parked fixture frame");
        let parked = vm.park_active_frame(&frame);
        let generator = crate::generator::JsGenerator::new(vm.gc_heap_mut(), parked)
            .expect("generator state allocation");
        let root = vm.persistent_root_insert(Value::generator(generator));
        if disposed_realm {
            assert!(vm.dispose_host_realm(realm));
            assert!(!vm.has_host_realm(realm));
        }
        let (result, owns_frame, incoming_absent) = NativeCtx::with_host_context(
            &mut vm,
            NativeCallInfo::default_call(),
            Some(&context),
            |ctx| {
                ctx.scope(|mut scope| -> Result<_, NativeError> {
                    let value = scope
                        .context()
                        .interp_mut()
                        .persistent_root_get(root)
                        .ok_or(NativeError::InvalidOperand)?;
                    let generator = scope.value(value);
                    let current_generator = scope.raw(generator);
                    let iterator = scope
                        .with_turn_parts(|interp, stack| {
                            interp.wrap_iterator_method_result(&context, stack, current_generator)
                        })
                        .map_err(|error| {
                            error.into_native(
                                scope.context().interp_mut(),
                                "generator admission fixture",
                            )
                        })?;
                    let iterator = scope.value(iterator);
                    let current = scope.raw(iterator);
                    let result = scope.with_turn_parts(|interp, stack| {
                        interp.set_pending_uncaught_throw(Value::number_i32(719));
                        let _ = interp.err_uncaught("incoming throw".into());
                        interp.iterator_close_discarding_completion(stack, Some(&context), &current)
                    });
                    let owner = scope
                        .raw(generator)
                        .as_generator()
                        .ok_or(NativeError::InvalidOperand)?;
                    let interp = scope.context().interp_mut();
                    Ok((
                        result,
                        owner.has_frame(interp.gc_heap()),
                        interp.pending_uncaught_throw.is_none(),
                    ))
                })
            },
        )
        .expect("return owned admission result");
        assert!(matches!(
            result,
            Err(crate::CommittedValueError::Fatal(VmError::InvalidOperand))
        ));
        assert!(
            owns_frame,
            "admission must precede parked-frame consumption"
        );
        assert!(
            incoming_absent,
            "terminal admission must not restore incoming throw"
        );
        assert_eq!(vm.active_host_realm_id(), 0);
        vm.persistent_root_remove(root);
    }
}

#[test]
fn actual_local_allocator_refusal_and_completed_oom_keep_distinct_native_endpoints() {
    let cap = 4 * 1024 * 1024;
    let mut vm = Interpreter::with_string_heap_cap(cap).expect("capped native endpoint fixture");
    let text = "x".repeat(8 * 1024 * 1024);
    let refusal = crate::JsString::from_str(&text, vm.gc_heap_mut())
        .expect_err("actual string allocation exceeds the heap cap");
    let error: VmError = refusal.into();
    let VmError::OutOfMemory {
        requested_bytes,
        heap_limit_bytes,
    } = error
    else {
        panic!("allocator refusal must retain its exact OOM cause");
    };
    assert!(requested_bytes > cap);
    assert_eq!(heap_limit_bytes, cap);
    let local =
        crate::CommittedValueError::JavaScript(error).into_native(&mut vm, "local iterator result");
    assert!(matches!(local, NativeError::OutOfMemory {
        name: "local iterator result", requested_bytes: requested,
        heap_limit_bytes: limit,
    } if requested == requested_bytes && limit == heap_limit_bytes));
    assert!(
        !local.is_fatal(),
        "local native refusal retains authored OOM policy"
    );
    let terminal =
        crate::CommittedValueError::Fatal(error).into_native(&mut vm, "completed generator body");
    assert!(terminal.is_fatal());
    let NativeError::ExecutionFailure(completion) = terminal else {
        panic!("completed OOM must retain the sole owned execution domain");
    };
    assert_eq!(completion.error, error);
    assert!(completion.is_fatal());
}
