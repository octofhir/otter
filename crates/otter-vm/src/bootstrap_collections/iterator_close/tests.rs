//! Real Iterator.return failure and moving incoming-completion proofs.
//!
//! # Contents
//! - Catchable cleanup suppression with collector-updated original identity.
//! - Structural cleanup failure through the actual native return method.
//!
//! # Invariants
//! - The production IteratorClose performs a real native call and minor GC.
//! - Every caller value is a scoped handle; callbacks keep only Rust counters.
//! - The pending throw, alias, detail and provenance are checked after return.
//!
//! # See also
//! - `super::preserving_completion` owns the preservation extent.

use super::*;
use crate::{Interpreter, NativeCallInfo, NativeCtx};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[test]
fn actual_iterator_return_moves_original_identity_and_preserves_catchable_completion() {
    check(false);
}

#[test]
fn actual_iterator_return_structural_failure_supersedes_original_without_erasure() {
    check(true);
}

fn check(fatal: bool) {
    let mut vm = Interpreter::new().expect("IteratorClose error fixture");
    let context = vm
        .link_module(
            crate::test_support::minimal_bytecode_module("iterator-close.js"),
            crate::source_registry::SourceRegistry::default(),
        )
        .expect("verified actual native-call source");
    vm.gc_heap_mut().set_gc_stress(0, true);
    let calls = Arc::new(AtomicUsize::new(0));
    let observed_calls = calls.clone();
    NativeCtx::with_host_context(
        &mut vm,
        NativeCallInfo::default_call(),
        Some(&context),
        |ctx| {
            ctx.scope(|mut scope| {
                let original = scope.object().expect("young original exception");
                let marker = scope.number(719.0);
                scope.set(original, "marker", marker).unwrap();
                scope.set(original, "self", original).unwrap();
                let alias = scope.value(scope.raw(original));
                let iterator = scope.object().expect("rooted iterator");
                let callback = scope
                    .context()
                    .native_value(
                        "IteratorCloseProbe",
                        smallvec::SmallVec::new(),
                        move |ctx, _args, _captures| {
                            observed_calls.fetch_add(1, Ordering::SeqCst);
                            ctx.interp_mut().collect_minor_tracing_runtime_roots();
                            if fatal {
                                Err(NativeError::InvalidOperand)
                            } else {
                                Err(NativeError::TypeError {
                                    name: "Iterator.return",
                                    reason: "suppressed cleanup".into(),
                                })
                            }
                        },
                    )
                    .expect("actual native Iterator.return");
                let callback = scope.value(callback);
                scope.set(iterator, "return", callback).unwrap();
                let old_offset = scope.raw(original).as_object().unwrap().offset();
                let current = scope.raw(original);
                let frames = vec![crate::StackFrameSnapshot {
                    function_id: context.function_base(),
                    function_name: "original source".into(),
                    module: "iterator-close.js".into(),
                    span: (31, 57),

                    source_position: None,
                }];
                let interp = scope.context().interp_mut();
                interp.set_pending_uncaught_throw(current);
                let _ = interp.err_uncaught("original completion".into());
                interp.set_uncaught_frames(frames.clone());
                let before = interp.gc_heap().gc_cycle_counts();
                let result =
                    super::preserving_completion(&mut scope, Some(&context), iterator, "Set");
                assert_eq!(
                    calls.load(Ordering::SeqCst),
                    1,
                    "actual Iterator.return called once"
                );
                assert!(
                    scope.context().heap().gc_cycle_counts().0 > before.0,
                    "collector ran inside actual return"
                );
                assert_ne!(
                    scope.raw(original).as_object().unwrap().offset(),
                    old_offset
                );
                assert_eq!(scope.raw(original), scope.raw(alias));
                let self_value = scope.get(original, "self").unwrap();
                assert_eq!(scope.raw(self_value), scope.raw(original));
                let marker = scope.get(original, "marker").unwrap();
                assert_eq!(scope.raw(marker).as_number().unwrap().as_f64(), 719.0);
                let current = scope.raw(original);
                let interp = scope.context().interp_mut();
                if fatal {
                    assert_eq!(result, Err(NativeError::InvalidOperand));
                    assert!(
                        interp.pending_uncaught_throw.is_none(),
                        "incoming exception is not restored over fatal cleanup"
                    );
                    assert!(interp.error_detail().is_none());
                    assert_ne!(
                        interp.pending_frames_for_test(),
                        Some(&frames),
                        "incoming provenance is not restored over fatal cleanup"
                    );
                } else {
                    result.expect("catchable cleanup loses to the incoming throw");
                    assert_eq!(interp.pending_uncaught_throw, Some(current));
                    assert_eq!(
                        interp.error_detail(),
                        Some(crate::ErrorDetail::Uncaught("original completion".into()))
                    );
                    assert_eq!(interp.pending_frames_for_test(), Some(&frames));
                }
            })
        },
    );
}
