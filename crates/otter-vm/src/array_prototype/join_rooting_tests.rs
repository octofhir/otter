//! Join releases its exact root extent after collecting abrupt operations.
//!
//! # Contents
//! - Actual native getters and element coercions that collect and throw.
//! - Exact handle-arena/iteration-anchor cleanup, source motion and throw identity.
//!
//! # Invariants
//! - Collection occurs inside the actual child callable, with fresh young inputs.
//! - Assertions stay outside the native callback.
//! - An error cannot leave hidden receiver handles in an enclosing scope.
//!
//! # See also
//! - `super::Interpreter::array_join` owns join's rooting and reads.

use super::*;

#[test]
fn join_abrupt_get_and_coercion_release_roots_after_actual_collection() {
    for getter in [true, false] {
        let mut vm = Interpreter::new().unwrap();
        vm.gc_heap.set_gc_stress(0, false);
        let context = vm
            .link_module(
                crate::test_support::minimal_bytecode_module("join-rooting"),
                crate::source_registry::SourceRegistry::default(),
            )
            .unwrap();
        vm.force_gc().unwrap();
        NativeCtx::with_host_context(
            &mut vm,
            crate::NativeCallInfo::default_call(),
            Some(&context),
            |ctx| {
                ctx.scope(|mut scope| -> Result<(), NativeError> {
                    let marker = scope.object()?;
                    let source = scope.object()?;
                    let length = scope.number(1.0);
                    scope.set(source, "length", length)?;
                    let throwing = scope.native_closure(
                        "JoinCollectingThrow",
                        0,
                        &[marker],
                        |ctx, _, captures| {
                            ctx.scope(|mut scope| {
                                let marker = scope.value(captures[0]);
                                scope
                                    .context()
                                    .interp_mut()
                                    .force_gc()
                                    .map_err(NativeError::from)?;
                                let marker = scope.raw(marker);
                                Err(scope.context().throw_value("JoinCollectingThrow", marker))
                            })
                        },
                    )?;
                    if getter {
                        let undefined = scope.undefined();
                        scope.define_accessor(
                            source,
                            "0",
                            throwing,
                            undefined,
                            object::PropertyFlags::new(false, true, true),
                        )?;
                    } else {
                        let element = scope.object()?;
                        scope.set(element, "toString", throwing)?;
                        scope.set(source, "0", element)?;
                    }
                    let original_source = scope.raw(source).as_object().unwrap().offset();
                    assert!(unsafe {
                        (*scope.raw(source).as_object().unwrap().as_header_ptr()).is_young()
                    });
                    let before_gc = scope.context().heap().gc_cycle_counts().1;
                    for _ in 0..3 {
                        scope.with_turn_parts(|interp, stack| {
                            let handles = interp.handle_arena.len();
                            let anchors = interp.iteration_anchors_for_trace().len();
                            let source = interp.escape_scoped(source);
                            let error = interp
                                .array_join(stack, &context, source, None, &mut [])
                                .expect_err("actual collecting getter/coercion throws");
                            assert!(matches!(error, CommittedValueError::JavaScript(_)));
                            assert_eq!(
                                interp.handle_arena.len(),
                                handles,
                                "join must close its root extent"
                            );
                            assert_eq!(interp.iteration_anchors_for_trace().len(), anchors);
                            let reason = interp.take_pending_uncaught_throw().unwrap();
                            assert_eq!(reason, interp.escape_scoped(marker));
                        });
                        scope.context().clear_pending_error();
                    }
                    assert!(scope.context().heap().gc_cycle_counts().1 > before_gc);
                    assert_ne!(
                        scope.raw(source).as_object().unwrap().offset(),
                        original_source
                    );
                    Ok(())
                })
            },
        )
        .unwrap();
        assert_eq!(vm.handle_arena.len(), 0);
    }
}
