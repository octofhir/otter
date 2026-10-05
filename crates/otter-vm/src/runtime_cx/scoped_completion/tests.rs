//! Real moving-collector and typed-result scoped completion proofs.
//!
//! # Contents
//! - Fatal async extents restore collector-rewritten ambient identities.
//! - Absent/undefined/object throws retain the exact scoped domain.
//!
//! # Invariants
//! All moving values use production scopes and the existing persistent table.
//! No collection hook, raw root provider or synthetic forwarding is installed.
//!
//! # See also
//! - super owns the public scoped handoff methods tested here.

use crate::{Interpreter, NativeCallInfo, NativeCtx, NativeError};

#[test]
fn scoped_async_context_restores_actual_moved_ambient_after_typed_fatal_result() {
    let mut vm = Interpreter::new().expect("scoped completion fixture bootstrap");
    vm.gc_heap_mut().set_gc_stress(0, true);
    NativeCtx::with_host_context(&mut vm, NativeCallInfo::default_call(), None, |ctx| {
        ctx.scope(|mut scope| {
            let ambient = scope.object().expect("young ambient context");
            let ambient_root = scope.persistent_root_insert(ambient);
            scope.set_async_context(ambient);
            let origin = scope.object().expect("young origin context");
            let origin_root = scope.persistent_root_insert(origin);
            let outcome = scope.with_async_context(origin, |ctx| {
                let before = ctx
                    .persistent_root_get(ambient_root)
                    .unwrap()
                    .as_object()
                    .unwrap()
                    .offset();
                let origin_before = ctx.async_context().as_object().unwrap().offset();
                let stats = ctx.interp_mut().gc_stats_snapshot();
                ctx.interp_mut().force_gc().expect("real moving collection");
                assert!(
                    ctx.interp_mut().gc_stats_snapshot().minor_gc_cycles > stats.minor_gc_cycles
                );
                assert_ne!(
                    before,
                    ctx.persistent_root_get(ambient_root)
                        .unwrap()
                        .as_object()
                        .unwrap()
                        .offset()
                );
                assert_ne!(
                    origin_before,
                    ctx.async_context().as_object().unwrap().offset()
                );
                assert_eq!(
                    ctx.async_context(),
                    ctx.persistent_root_get(origin_root).unwrap()
                );
                Err::<(), _>(NativeError::Exit { code: 27 })
            });
            assert!(matches!(outcome, Err(NativeError::Exit { code: 27 })));
            let current = scope.async_context();
            assert!(scope.strict_equals(current, ambient));
            let ambient_alias = scope.take_persistent_root(ambient_root).unwrap();
            let origin_alias = scope.take_persistent_root(origin_root).unwrap();
            assert!(scope.strict_equals(ambient_alias, ambient));
            assert!(scope.strict_equals(origin_alias, origin));
        });
    });
}

#[test]
fn scoped_pending_throw_distinguishes_absent_and_undefined_then_restores_moved_object() {
    let mut vm = Interpreter::new().expect("pending completion fixture bootstrap");
    vm.gc_heap_mut().set_gc_stress(0, true);
    NativeCtx::with_host_context(&mut vm, NativeCallInfo::default_call(), None, |ctx| {
        ctx.clear_pending_error();
        ctx.scope(|mut scope| {
            assert!(scope.take_pending_uncaught_throw().is_none());
            let undefined = scope.undefined();
            scope.set_pending_uncaught_throw(undefined);
            let pending = scope
                .take_pending_uncaught_throw()
                .expect("actual thrown undefined");
            assert!(scope.is_undefined(pending));
            assert!(scope.take_pending_uncaught_throw().is_none());
            let original = scope.object().expect("young original throw");
            scope.set_pending_uncaught_throw(original);
            let pending = scope.take_pending_uncaught_throw().unwrap();
            let ambient = scope.async_context();
            scope.with_async_context(ambient, |ctx| {
                ctx.interp_mut()
                    .force_gc()
                    .expect("collect with only scoped original throw");
            });
            assert!(scope.strict_equals(pending, original));
            scope.set_pending_uncaught_throw(pending);
        });
        let original = ctx.interp_mut().take_pending_uncaught_throw().unwrap();
        assert!(original.as_object().is_some());
        ctx.interp_mut().set_pending_uncaught_throw(original);
        ctx.clear_pending_error();
        assert!(ctx.interp_mut().take_pending_uncaught_throw().is_none());
        assert!(ctx.interp_mut().take_error_detail().is_none());
        assert!(!ctx.interp_mut().take_uncaught_from_promise_rejection());
    });
}
