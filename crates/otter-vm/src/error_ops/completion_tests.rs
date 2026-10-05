//! Completed native/VM error transport through actual synchronous Host entries.
//!
//! # Contents
//! - Real oversized error allocation through call and nested call/construct.
//! - Moved rooted aliases and exact hard-cap refusal fields.
//! - Owned completion validation, source/detail restoration and thread safety.
//!
//! # Invariants
//! - Calls enter the canonical linked context/activation/trampoline owner.
//! - Callbacks never assert or unwind; all checks run outside the native ABI.
//! - The same completed allocation failure cannot become a second JS Error.
//!
//! # See also
//! - `super::native_error_to_throwable_with_stack` owns the sole projection.
//! - `crate::native_function::NativeError::ExecutionFailure` owns imported data.

use crate::{
    ActivationStack, ErrorDetail, Interpreter, NativeCall, NativeCallInfo, NativeCtx, NativeError,
    RunError, StackFrameSnapshot, Value, VmError,
};

const CAP: u64 = 4 * 1024 * 1024;

fn oversized_error(_ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    Err(NativeError::SyntaxError {
        name: "completed native source",
        reason: "m".repeat(8 * 1024 * 1024),
    })
}

#[test]
fn synchronous_and_nested_call_construct_preserve_real_materialization_oom() {
    for mode in 0..3 {
        let mut vm = Interpreter::with_string_heap_cap(CAP).expect("capped completion bootstrap");
        vm.gc_heap_mut().set_gc_stress(0, true);
        let context = vm
            .link_module(
                crate::test_support::minimal_bytecode_module("native-completion"),
                crate::source_registry::SourceRegistry::default(),
            )
            .expect("verified ordinary context");
        NativeCtx::with_host_context(
            &mut vm,
            NativeCallInfo::default_call(),
            Some(&context),
            |ctx| {
                ctx.scope(|mut scope| {
                    let child = scope.object().expect("young child");
                    let marker = scope.number(719.0);
                    scope.set(child, "marker", marker).expect("rooted marker");
                    let alias = scope.value(scope.raw(child));
                    let target = if mode == 2 {
                        scope
                            .context()
                            .interp_mut()
                            .native_constructor_from_call_host_rooted(
                                "CompletedConstructor",
                                1,
                                NativeCall::Static(oversized_error),
                                &[],
                                &[],
                            )
                            .expect("actual native constructor")
                    } else {
                        crate::native_function::native_value_static(
                            scope.context().heap_mut(),
                            "completedInner",
                            1,
                            oversized_error,
                        )
                        .expect("actual inner native")
                    };
                    let inner = scope.value(target);
                    let outer = if mode == 0 {
                        inner
                    } else {
                        let native = crate::NativeFunction::new(
                            scope.context().heap_mut(),
                            "completedOuter",
                            move |ctx, args, _| {
                                ctx.scope(|mut scope| {
                                    let target = scope.argument(args, 0);
                                    let child = scope.argument(args, 1);
                                    let receiver = scope.undefined();
                                    let result = if mode == 2 {
                                        scope.construct(target, &[child])
                                    } else {
                                        scope.call(target, receiver, &[child])
                                    }?;
                                    Ok(scope.finish(result))
                                })
                            },
                        )
                        .expect("actual outer native");
                        scope.value(Value::native_function(native))
                    };
                    // All setup allocations precede this observed refusal; the
                    // child remains young until the actual oversized request.
                    let old = scope.raw(child).as_object().unwrap().offset();
                    let before = scope.context().heap().gc_cycle_counts();
                    let receiver = scope.undefined();
                    let args = if mode == 0 {
                        vec![child]
                    } else {
                        vec![inner, child]
                    };
                    let error = scope
                        .call(outer, receiver, &args)
                        .expect_err("completed error build must escape every nested boundary");
                    let NativeError::ExecutionFailure(failure) = error else {
                        panic!("expected owned completed failure, got {error:?}");
                    };
                    let VmError::OutOfMemory {
                        requested_bytes,
                        heap_limit_bytes,
                    } = failure.error
                    else {
                        panic!("actual failed Error allocation: {failure:?}");
                    };
                    assert!(requested_bytes > CAP);
                    assert_eq!(heap_limit_bytes, CAP);
                    assert!(failure.is_fatal());
                    assert!(
                        failure.detail.is_none(),
                        "refusal cannot inherit SyntaxError detail"
                    );
                    assert!(scope.context().heap().gc_cycle_counts().1 > before.1);
                    assert_ne!(scope.raw(child).as_object().unwrap().offset(), old);
                    assert_eq!(scope.raw(child), scope.raw(alias));
                    let marker = scope.get(child, "marker").unwrap();
                    assert_eq!(scope.number_value(marker).unwrap(), 719.0);
                    assert!(
                        scope
                            .context()
                            .interp_mut()
                            .pending_uncaught_throw
                            .is_none()
                    );
                });
            },
        );
    }
}

#[test]
fn owned_completed_failures_restore_exact_payload_and_reject_catchable_inputs() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<RunError>();
    assert_send_sync::<NativeError>();
    let mut vm = Interpreter::new().expect("owned completion bootstrap");
    let frames = vec![StackFrameSnapshot {
        function_id: 719,
        function_name: "owned completion".into(),
        module: "completed-native.js".into(),
        span: (31, 57),
        source_position: None,
    }];
    let failure = RunError {
        error: VmError::BudgetExceeded,
        frames: frames.clone(),
        detail: Some(ErrorDetail::Message("original nested budget".into())),
    };
    assert!(failure.is_fatal());
    let before = vm.gc_heap().stats().allocated_bytes;
    assert_eq!(
        super::native_error_to_throwable_with_stack(
            &mut vm,
            &ActivationStack::new(),
            None,
            NativeError::ExecutionFailure(failure.clone()),
        ),
        Err(VmError::BudgetExceeded),
    );
    assert_eq!(vm.error_detail(), failure.detail);
    assert_eq!(vm.pending_frames_for_test(), Some(&frames));
    assert_eq!(vm.gc_heap().stats().allocated_bytes, before);
    for error in [
        VmError::SyntaxError,
        VmError::Uncaught,
        VmError::StackOverflow { limit: 19 },
    ] {
        let malformed = NativeError::ExecutionFailure(RunError::bare(error));
        assert!(
            malformed.is_fatal(),
            "malformed imported completion is structural"
        );
        assert_eq!(
            super::native_error_to_throwable_with_stack(
                &mut vm,
                &ActivationStack::new(),
                None,
                malformed
            ),
            Err(VmError::InvalidOperand),
        );
        assert!(vm.error_detail().is_none());
        assert!(vm.pending_throw_provenance.is_none());
        assert!(vm.pending_uncaught_throw.is_none());
    }
    assert_eq!(vm.gc_heap().stats().allocated_bytes, before);
}
