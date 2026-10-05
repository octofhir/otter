//! Exact fatal/control projection and fallible JavaScript error materialization.
//!
//! # Contents
//! - Existing NativeError/JsError round trips without allocation or reclassification.
//! - Realm-intrinsic exception classes after mutable global replacement.
//! - The sole pending user-throw root through real moving collection.
//! - Actual materialization OOM, moved child aliases and host-job terminal cleanup.
//!
//! # Invariants
//! - Every claimed relocation is performed by the real collector.
//! - Host and binding values use ordinary NativeCtx handle scopes.
//! - No mock allocator, fallback rejection value or extra fatal-error latch is used.
//!
//! # See also
//! - `super::native_error_to_throwable_with_stack` owns native materialization.
//! - `crate::host_completion::HostCompletionJob` returns the turn's RunError.

use crate::marshal::{JsError, MarshalCx};
use crate::{
    ActivationStack, ErrorDetail, ErrorKind, Interpreter, NativeCallInfo, NativeCtx, NativeError,
    RunError, Value, VmError,
};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[test]
fn imported_native_errors_keep_exact_classes_control_and_structural_identity() {
    let mut vm = Interpreter::new().expect("error boundary bootstrap");
    let cases = [
        (VmError::MissingReturn, NativeError::MissingReturn),
        (VmError::InvalidOperand, NativeError::InvalidOperand),
        (VmError::Interrupted, NativeError::Interrupted),
        (VmError::Exit { code: 27 }, NativeError::Exit { code: 27 }),
        (
            vm.err_budget("real budget detail".into()),
            NativeError::BudgetExceeded {
                reason: "real budget detail".into(),
            },
        ),
    ];
    let before = vm.gc_heap_mut().stats().allocated_bytes;
    for (error, expected) in cases {
        assert!(error.is_fatal());
        let native = crate::native_function::vm_to_native_error(&vm, error, "original operation");
        assert_eq!(native, expected);
        let imported = JsError::from_native(native);
        assert_eq!(
            imported.clone().into_native("outer operation"),
            expected,
            "imported native data is the same carrier, including its operation and payload"
        );
        let restored = super::native_to_vm_error(&mut vm, imported.into_native("outer operation"));
        assert_eq!(restored, error);
        assert_eq!(
            vm.vm_error_to_throwable_with_stack_roots(None, &ActivationStack::new(), &restored),
            Err(error)
        );
    }
    assert_eq!(
        vm.gc_heap_mut().stats().allocated_bytes,
        before,
        "fatal/control projection creates no JavaScript error object"
    );
    assert!(
        !VmError::TypeMismatch.is_fatal(),
        "existing normative wrong-type producers remain catchable"
    );
    assert!(
        !VmError::OutOfMemory {
            requested_bytes: 57,
            heap_limit_bytes: 4096
        }
        .is_fatal()
    );

    let _ = vm.take_error_detail();
    let absent_coded = crate::native_function::vm_to_native_error(&vm, VmError::Coded, "coded");
    assert_eq!(
        absent_coded,
        NativeError::SpecError {
            kind: ErrorKind::Error,
            message: VmError::Coded.to_string(),
        }
    );
    let imported = JsError::from_native(absent_coded.clone()).into_native("outer");
    assert_eq!(imported, absent_coded);
    let restored = super::native_to_vm_error(&mut vm, imported);
    assert_eq!(restored, VmError::Uncaught);
    let value = vm
        .vm_error_to_throwable_with_stack_roots(None, &ActivationStack::new(), &restored)
        .expect("class-preserving coded fallback materialization");
    assert!(crate::object::has_in_proto_chain(
        value.as_object().unwrap(),
        vm.gc_heap(),
        vm.error_classes.prototype(ErrorKind::Error)
    ));

    for native in [
        NativeError::SyntaxError {
            name: "parse",
            reason: "syntax detail".into(),
        },
        NativeError::ReferenceError {
            name: "lookup",
            reason: "reference detail".into(),
        },
        NativeError::URIError {
            name: "decode",
            reason: "URI detail".into(),
        },
        NativeError::RangeError {
            name: "range",
            reason: "range detail".into(),
        },
        NativeError::OutOfMemory {
            name: "allocation",
            requested_bytes: 719,
            heap_limit_bytes: 4096,
        },
    ] {
        assert_eq!(
            JsError::from_native(native.clone()).into_native("outer operation"),
            native
        );
    }
}

#[test]
fn binding_error_materialization_uses_pinned_intrinsics_after_globals_change() {
    let mut vm = Interpreter::new().expect("intrinsic error bootstrap");
    NativeCtx::with_host_context(&mut vm, NativeCallInfo::default_call(), None, |ctx| {
        ctx.scope(|scope| {
            let mut cx = MarshalCx::new(scope);
            let global = cx.global_this();
            let changed = cx.number(719.0);
            for name in [
                "Error",
                "TypeError",
                "RangeError",
                "SyntaxError",
                "ReferenceError",
                "URIError",
            ] {
                cx.define(
                    global,
                    name,
                    changed,
                    crate::object::PropertyFlags::data_default(),
                )
                .unwrap();
            }
            cx.ctx().interp_mut().gc_heap_mut().set_gc_stress(1, true);
            let before = cx.heap().gc_cycle_counts();
            let cases = [
                (
                    ErrorKind::SyntaxError,
                    NativeError::SyntaxError {
                        name: "source",
                        reason: "syntax payload".into(),
                    },
                ),
                (
                    ErrorKind::ReferenceError,
                    NativeError::ReferenceError {
                        name: "source",
                        reason: "reference payload".into(),
                    },
                ),
                (
                    ErrorKind::URIError,
                    NativeError::URIError {
                        name: "source",
                        reason: "URI payload".into(),
                    },
                ),
            ];
            for (kind, error) in cases {
                let expected = match &error {
                    NativeError::SyntaxError { name, reason }
                    | NativeError::ReferenceError { name, reason }
                    | NativeError::URIError { name, reason } => format!("{name}: {reason}"),
                    _ => unreachable!(),
                };
                let value = cx
                    .error_value(JsError::from_native(error))
                    .expect("intrinsic error allocation");
                let message = cx.get(value, "message").unwrap();
                assert_eq!(cx.to_string_spec(message).unwrap(), expected);
                let current = cx.escape(value).as_object().unwrap();
                let prototype = cx.ctx().interp_mut().error_classes.prototype(kind);
                assert!(crate::object::has_in_proto_chain(
                    current,
                    cx.heap(),
                    prototype
                ));
            }
            let after = cx.heap().gc_cycle_counts();
            assert!(
                after.0 > before.0 || after.1 > before.1,
                "real collection inside error materialization"
            );
        })
    });
}

#[test]
fn user_throw_identity_is_owned_by_the_pending_root_through_real_moving_gc() {
    let mut vm = Interpreter::new().expect("throw root bootstrap");
    vm.gc_heap_mut().set_gc_stress(0, false);
    let (imported, original) =
        NativeCtx::with_host_context(&mut vm, NativeCallInfo::default_call(), None, |ctx| {
            ctx.scope(|scope| {
                let mut cx = MarshalCx::new(scope);
                let object = cx.object().unwrap();
                let marker = cx.number(997.0);
                cx.define(
                    object,
                    "marker",
                    marker,
                    crate::object::PropertyFlags::data_default(),
                )
                .unwrap();
                let raw = cx.escape(object);
                let original = raw.as_object().unwrap().offset();
                let error = cx.ctx().throw_value("thrower", raw);
                (JsError::from_native(error), original)
            })
        });
    // The building scope has ended. The pending exception is now the sole
    // JavaScript root keeping this object alive; the imported error is owned Rust data.
    let before = vm.gc_heap().gc_cycle_counts();
    vm.collect_minor_tracing_runtime_roots();
    assert!(vm.gc_heap().gc_cycle_counts().0 > before.0);
    let expected = vm.pending_uncaught_throw.expect("live pending exception");
    assert_ne!(expected.as_object().unwrap().offset(), original);
    NativeCtx::with_host_context(&mut vm, NativeCallInfo::default_call(), None, |ctx| {
        ctx.scope(|scope| {
            let mut cx = MarshalCx::new(scope);
            let thrown = cx.error_value(imported).unwrap();
            assert_eq!(
                cx.escape(thrown),
                expected,
                "exact current object identity, without reconstruction"
            );
            let marker = cx.get(thrown, "marker").unwrap();
            assert_eq!(cx.as_f64(marker), Some(997.0));
            assert!(cx.ctx().interp_mut().pending_uncaught_throw.is_none());
        })
    });
    let symbol = crate::symbol::JsSymbol::new(vm.gc_heap_mut(), None).unwrap();
    let original = Value::symbol(symbol);
    let error =
        NativeCtx::with_host_context(&mut vm, NativeCallInfo::default_call(), None, |ctx| {
            ctx.throw_value("symbol thrower", original)
        });
    vm.force_gc()
        .expect("real full collection while only pending throw owns the symbol");
    let expected = vm.pending_uncaught_throw.expect("pending symbol");
    assert!(expected.is_symbol());
    NativeCtx::with_host_context(&mut vm, NativeCallInfo::default_call(), None, |ctx| {
        ctx.scope(|scope| {
            let mut cx = MarshalCx::new(scope);
            let thrown = cx.error_value(JsError::from_native(error)).unwrap();
            assert_eq!(cx.escape(thrown), expected, "exact Symbol identity");
        })
    });
}

#[test]
fn materialization_oom_keeps_actual_allocation_fields_and_collector_updated_aliases() {
    let cap = 4 * 1024 * 1024;
    let mut vm = Interpreter::with_string_heap_cap(cap).expect("capped error bootstrap");
    vm.gc_heap_mut().set_gc_stress(0, false);
    NativeCtx::with_host_context(&mut vm, NativeCallInfo::default_call(), None, |ctx| {
        ctx.scope(|scope| {
            let mut cx = MarshalCx::new(scope);
            let child = cx.object().unwrap();
            let marker = cx.number(719.0);
            cx.define(
                child,
                "marker",
                marker,
                crate::object::PropertyFlags::data_default(),
            )
            .unwrap();
            let alias = cx.park(cx.escape(child));
            let original = cx.escape(child).as_object().unwrap().offset();
            let before = cx.heap().gc_cycle_counts();
            let error = cx
                .error_value(JsError::Native(NativeError::SyntaxError {
                    name: "large diagnostic",
                    reason: "m".repeat(cap as usize),
                }))
                .unwrap_err();
            let native = error.into_native("host completion");
            let NativeError::ExecutionFailure(RunError {
                error:
                    VmError::OutOfMemory {
                        requested_bytes,
                        heap_limit_bytes,
                    },
                ..
            }) = native
            else {
                panic!("materialization must return its actual OOM: {native:?}");
            };
            assert!(requested_bytes > cap, "exact rejected string footprint");
            assert_eq!(heap_limit_bytes, cap);
            assert!(
                cx.heap().gc_cycle_counts().1 > before.1,
                "the tested string allocation really attempted cap collection"
            );
            assert_ne!(
                cx.escape(child).as_object().unwrap().offset(),
                original,
                "young child moved during materialization refusal"
            );
            assert_eq!(cx.escape(child), cx.escape(alias));
            let marker = cx.get(child, "marker").unwrap();
            assert_eq!(cx.as_f64(marker), Some(719.0));
            assert!(
                cx.ctx().interp_mut().pending_uncaught_throw.is_none(),
                "no fabricated rejection value"
            );
        })
    });
}

#[test]
fn host_job_result_and_cancellation_use_one_terminal_owner() {
    let mut vm = Interpreter::new().expect("completion bootstrap");
    let detail = ErrorDetail::Message("exact job budget".into());
    let frames = vec![crate::StackFrameSnapshot {
        function_id: 719,
        function_name: "completion".into(),
        module: "<host-job>".into(),
        span: (31, 57),

        source_position: None,
    }];
    let job = crate::host_completion::HostCompletionJob::new({
        let detail = detail.clone();
        let frames = frames.clone();
        move |_| {
            Err(RunError {
                error: VmError::BudgetExceeded,
                frames,
                detail: Some(detail),
            })
        }
    });
    let error = job
        .run(&mut vm)
        .expect_err("host job must return its failure");
    assert_eq!(error.error, VmError::BudgetExceeded);
    assert_eq!(error.detail, Some(detail));
    assert_eq!(error.frames, frames);
    assert_eq!(error.message(), "exact job budget");

    let root = vm.persistent_root_insert(Value::number_i32(31));
    let runs = Arc::new(AtomicUsize::new(0));
    let cancels = Arc::new(AtomicUsize::new(0));
    let job = crate::host_completion::HostCompletionJob::new_with_cancel(
        {
            let runs = Arc::clone(&runs);
            move |_| {
                runs.fetch_add(1, Ordering::Relaxed);
                Err(RunError::bare(VmError::InvalidOperand))
            }
        },
        {
            let cancels = Arc::clone(&cancels);
            move |vm| {
                cancels.fetch_add(1, Ordering::Relaxed);
                assert!(vm.persistent_root_remove(root).is_some());
            }
        },
    );
    job.cancel(&mut vm);
    assert_eq!(runs.load(Ordering::Relaxed), 0);
    assert_eq!(cancels.load(Ordering::Relaxed), 1);
    assert!(vm.persistent_root_remove(root).is_none());
}

#[test]
fn allocating_vm_throwable_failure_consumes_original_detail_but_preserves_source_frames() {
    let cap = 4 * 1024 * 1024;
    let mut vm = Interpreter::with_string_heap_cap(cap).expect("capped throwable bootstrap");
    vm.gc_heap_mut().set_gc_stress(0, true);
    NativeCtx::with_host_context(&mut vm, NativeCallInfo::default_call(), None, |ctx| {
        ctx.scope(|mut scope| {
            let child = scope.object().expect("young throwable alias");
            let marker = scope.number(997.0);
            scope.set(child, "marker", marker).unwrap();
            let alias = scope.value(scope.raw(child));
            let old_offset = scope.raw(child).as_object().unwrap().offset();
            let frames = vec![crate::StackFrameSnapshot {
                function_id: 719,
                function_name: "original throw".into(),
                module: "<throwable-oom>".into(),
                span: (31, 57),

                source_position: None,
            }];
            let interp = scope.context().interp_mut();
            interp.pending_uncaught_frames = Some(frames.clone());
            let original = interp.err_syntax("m".repeat(cap as usize).into());
            let before = interp.gc_heap().gc_cycle_counts();
            let error = interp
                .vm_error_to_throwable_with_stack_roots(None, &ActivationStack::new(), &original)
                .expect_err("actual VM throwable string exceeds configured cap");
            assert!(
                matches!(error, VmError::OutOfMemory { requested_bytes, heap_limit_bytes }
                if requested_bytes > cap && heap_limit_bytes == cap),
                "{error:?}"
            );
            assert!(interp.gc_heap().gc_cycle_counts().1 > before.1);
            assert!(
                interp.error_detail().is_none(),
                "OOM cannot inherit the consumed syntax detail"
            );
            assert_eq!(interp.pending_uncaught_frames.as_ref(), Some(&frames));
            assert!(interp.pending_uncaught_throw.is_none());
            assert_ne!(scope.raw(child).as_object().unwrap().offset(), old_offset);
            assert_eq!(scope.raw(child), scope.raw(alias));
            let marker = scope.get(child, "marker").unwrap();
            assert_eq!(scope.raw(marker).as_number().unwrap().as_f64(), 997.0);
        })
    });
}

#[test]
fn original_native_oom_with_headroom_has_the_canonical_catchable_range_class() {
    let mut vm = Interpreter::new().expect("original OOM class fixture");
    vm.gc_heap_mut().set_gc_stress(1, true);
    let before = vm.gc_heap().gc_cycle_counts();
    let value = super::native_error_to_throwable_with_stack(
        &mut vm,
        &ActivationStack::new(),
        None,
        NativeError::OutOfMemory {
            name: "original allocation",
            requested_bytes: 719,
            heap_limit_bytes: 4096,
        },
    )
    .expect("an original OOM can be caught when diagnostic headroom exists");
    NativeCtx::with_host_context(&mut vm, NativeCallInfo::default_call(), None, |ctx| {
        ctx.scope(|mut scope| {
            let value = scope.value(value);
            let raw = scope.raw(value);
            let prototype = scope
                .context()
                .interp_mut()
                .error_classes
                .prototype(ErrorKind::RangeError);
            assert!(crate::object::has_in_proto_chain(
                raw.as_object().unwrap(),
                scope.context().heap(),
                prototype
            ));
            let name = scope.get(value, "name").unwrap();
            assert_eq!(
                scope
                    .raw(name)
                    .as_string(scope.context().heap())
                    .unwrap()
                    .to_lossy_string(scope.context().heap()),
                "RangeError"
            );
            let message = scope.get(value, "message").unwrap();
            let message = scope
                .raw(message)
                .as_string(scope.context().heap())
                .unwrap()
                .to_lossy_string(scope.context().heap());
            assert!(
                message.contains("719") && message.contains("4096"),
                "{message}"
            );
        })
    });
    let after = vm.gc_heap().gc_cycle_counts();
    assert!(
        after.0 > before.0 || after.1 > before.1,
        "real scoped diagnostic collection"
    );
}
