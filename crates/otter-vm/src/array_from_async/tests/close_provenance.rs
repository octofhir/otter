//! IteratorClose preserves exactly the incoming completion through real GC.
//!
//! # Contents
//! - Actual return getter/call throws with moving incoming identity and aliases.
//! - Structural cleanup errors supersede the parked incoming provenance.
//!
//! # Invariants
//! - Production Get/Call and the real collector run inside the tested close.
//! - Scoped handles own every retained VM value; callbacks never assert.
//! - Promise rejection reasons do not manufacture a pending thrown value.
//!
//! # See also
//! - `super::super::Interpreter::collect_close_iterator` owns suppression.
//! - Runtime async_native_methods covers the later fatal reaction drain.

use super::*;

#[test]
fn catchable_return_get_and_call_move_and_restore_exact_incoming_provenance() {
    for getter in [false, true] {
        close_with_incoming(getter, false, true);
    }
}

#[test]
fn structural_return_get_and_call_do_not_restore_the_incoming_completion() {
    for getter in [false, true] {
        close_with_incoming(getter, true, true);
    }
}

#[test]
fn ordinary_rejection_reason_does_not_manufacture_pending_cleanup_provenance() {
    for getter in [false, true] {
        close_with_incoming(getter, false, false);
    }
}

fn close_with_incoming(getter: bool, fatal: bool, incoming: bool) {
    let mut vm = Interpreter::new().expect("close provenance bootstrap");
    let context = vm
        .link_module(
            crate::test_support::minimal_bytecode_module("fromAsync-close-provenance.js"),
            crate::source_registry::SourceRegistry::default(),
        )
        .expect("verified native source context");
    vm.gc_heap.set_gc_stress(0, true);
    vm.force_gc()
        .expect("settled bootstrap before young inputs");
    let mut stack = ActivationStack::new();
    let calls = Arc::new(AtomicUsize::new(0));
    vm.with_handle_scope(|vm, scope| {
        let state = vm.scoped_object_bare(scope).expect("state");
        let iterator = vm.scoped_object_bare(scope).expect("iterator");
        let original = vm.scoped_object_bare(scope).expect("young exception");
        put(vm, original, "marker", Value::number_i32(719));
        let current = vm.escape_scoped(original);
        put(vm, original, "self", current);
        let alias = vm.scoped_value(scope, vm.escape_scoped(original));
        // Distinct reason proves the helper restores the real pending owner,
        // rather than synthesizing pending state from its rejection argument.
        let reason = vm.scoped_object_bare(scope).expect("ordinary rejection");
        put(vm, reason, "marker", Value::number_i32(997));
        let close = crate::native_function::NativeFunction::new(vm.gc_heap_mut(), "close", {
            let calls = calls.clone();
            move |ctx, _, _| {
                calls.fetch_add(1, Ordering::SeqCst);
                ctx.interp_mut().collect_minor_tracing_runtime_roots();
                if fatal {
                    Err(NativeError::InvalidOperand)
                } else {
                    Err(NativeError::TypeError {
                        name: "cleanup",
                        reason: "suppressed cleanup provenance".into(),
                    })
                }
            }
        })
        .expect("actual cleanup native");
        let close = vm.scoped_value(scope, Value::native_function(close));
        let st = vm.push_iteration_anchor(vm.escape_scoped(state)) - 1;
        vm.with_runtime_turn(&mut stack, |turn| {
            let (vm, stack) = turn.into_parts();
            if getter {
                let current = vm.escape_scoped(iterator);
                assert!(
                    vm.define_own_property_value(
                        stack,
                        &context,
                        &current,
                        &VmPropertyKey::String("return"),
                        crate::object::PartialPropertyDescriptor {
                            get: Some(vm.escape_scoped(close)),
                            configurable: Some(true),
                            ..Default::default()
                        },
                    )
                    .expect("actual return accessor")
                );
            } else {
                let current = vm.escape_scoped(close);
                put(vm, iterator, "return", current);
            }
            let current = vm.escape_scoped(iterator);
            put(vm, state, slot::ITERATOR, current);
            let frames = vec![crate::StackFrameSnapshot {
                function_id: context.function_base(),
                function_name: "original source".into(),
                module: "original-provenance.js".into(),
                span: (31, 57),

                source_position: None,
            }];
            assert!(vm.pending_uncaught_throw.is_none());
            assert!(vm.error_detail().is_none());
            assert!(vm.pending_throw_provenance.is_none());
            if incoming {
                vm.set_pending_uncaught_throw(vm.escape_scoped(original));
                let _ = vm.err_uncaught("original completion".into());
                vm.set_uncaught_frames(frames.clone());
            }
            let before = vm.gc_heap.gc_cycle_counts();
            let old_offset = vm.escape_scoped(original).as_object().unwrap().offset();
            let result = vm.collect_close_iterator(stack, &context, st, vm.escape_scoped(reason));
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            assert!(vm.gc_heap.gc_cycle_counts().0 > before.0);
            let current = vm.escape_scoped(original);
            assert_ne!(current.as_object().unwrap().offset(), old_offset);
            assert_eq!(current, vm.escape_scoped(alias));
            assert_eq!(
                crate::object::get_own(current.as_object().unwrap(), vm.gc_heap(), "self"),
                Some(current)
            );
            assert_eq!(
                crate::object::get_own(current.as_object().unwrap(), vm.gc_heap(), "marker"),
                Some(Value::number_i32(719))
            );
            assert!(
                vm.state_get(vm.escape_scoped(state), slot::ITERATOR)
                    .is_undefined()
            );
            if fatal {
                assert!(
                    matches!(
                        result,
                        Err(crate::CommittedValueError::Fatal(VmError::InvalidOperand))
                    ),
                    "{result:?}"
                );
                assert!(vm.pending_uncaught_throw.is_none());
                assert!(vm.error_detail().is_none());
                assert_ne!(vm.pending_frames_for_test(), Some(&frames));
            } else if incoming {
                result.expect("incoming catchable error wins");
                assert_eq!(vm.pending_uncaught_throw, Some(current));
                assert_eq!(
                    vm.error_detail(),
                    Some(crate::ErrorDetail::Uncaught("original completion".into()))
                );
                assert_eq!(vm.pending_frames_for_test(), Some(&frames));
            } else {
                result.expect("ordinary rejection keeps precedence");
                assert!(
                    vm.pending_uncaught_throw.is_none(),
                    "reason is not a pending throw"
                );
                assert!(vm.error_detail().is_none(), "cleanup diagnostic is retired");
                assert!(
                    vm.pending_throw_provenance.is_none(),
                    "cleanup frames are retired"
                );
            }
        });
        vm.pop_iteration_anchors_to(st);
    });
}
