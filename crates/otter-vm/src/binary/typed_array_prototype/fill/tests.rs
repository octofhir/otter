//! Moving-GC proof for the native BigInt fill preparation boundary.
//!
//! # Contents
//! - Full collection in either bound's actual native valueOf callback.
//! - Young receiver/payload relocation, wrapped bytes and exact scoped-root cleanup.
//!
//! # Invariants
//! - Source and payload are young before calling the installed fill native.
//! - Exactly one selected callback collects; assertions prove actual movement.
//! - Only the returned Local remains after the native call, with no leaked anchors.
//!
//! # See also
//! - `super::run` owns the anchored payload and live receiver.

use crate::{Interpreter, NativeCallInfo, NativeCtx, NativeError, Value, binary::TypedArrayKind};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[test]
fn bigint_fill_moves_receiver_and_pending_payload_in_both_bound_coercions() {
    for kind in [TypedArrayKind::BigInt64, TypedArrayKind::BigUint64] {
        for bound in ["start", "end"] {
            let mut vm = Interpreter::new().unwrap();
            vm.gc_heap.set_gc_stress(0, false);
            let context = vm
                .link_module(
                    crate::test_support::minimal_bytecode_module("typed-fill-preparation"),
                    crate::source_registry::SourceRegistry::default(),
                )
                .unwrap();
            vm.force_gc().unwrap();
            let calls = Arc::new(AtomicUsize::new(0));
            let observed = calls.clone();
            NativeCtx::with_host_context(
                &mut vm,
                NativeCallInfo::default_call(),
                Some(&context),
                |ctx| {
                    ctx.scope(|mut scope| -> Result<(), NativeError> {
                        let buffer = scope.array_buffer_from_bytes(vec![0; 32])?;
                        let view = scope.typed_array_view(buffer, kind, 0, 4)?;
                        let method = scope.get(view, "fill")?;
                        let coercion = scope.object()?;
                        let index = if bound == "start" { 0 } else { 4 };
                        let collecting = scope.native_closure(
                            "CollectFillBound",
                            0,
                            &[],
                            move |ctx, _, _| {
                                observed.fetch_add(1, Ordering::Relaxed);
                                ctx.interp_mut().force_gc().map_err(NativeError::from)?;
                                Ok(Value::number_i32(index))
                            },
                        )?;
                        scope.set(coercion, "valueOf", collecting)?;
                        let zero = scope.number(0.0);
                        let four = scope.number(4.0);
                        let payload = scope.bigint_i128(-(1i128 << 70) - 719)?;
                        let before_view = scope
                            .raw(view)
                            .as_typed_array(scope.context().heap())
                            .unwrap()
                            .handle();
                        let before_payload = scope
                            .raw(payload)
                            .as_raw_gc()
                            .unwrap()
                            .checked_cast::<crate::bigint::BigIntBody>()
                            .unwrap();
                        // SAFETY: both scoped values are current, with no allocation in these reads.
                        unsafe {
                            assert!((*before_view.as_header_ptr()).is_young());
                            assert!((*before_payload.as_header_ptr()).is_young());
                        }
                        let before_roots = scope.with_turn_parts(|interp, _| {
                            (
                                interp.handle_arena.len(),
                                interp.iteration_anchors_for_trace().len(),
                            )
                        });
                        let cycles = scope.context().heap().gc_cycle_counts();
                        let args = if bound == "start" {
                            [payload, coercion, four]
                        } else {
                            [payload, zero, coercion]
                        };
                        let result = scope.call(method, view, &args)?;
                        assert_eq!(scope.raw(result), scope.raw(view));
                        assert_ne!(scope.raw(view).as_raw_gc().unwrap().0, before_view.offset());
                        assert_ne!(
                            scope.raw(payload).as_raw_gc().unwrap().0,
                            before_payload.offset()
                        );
                        assert!(scope.context().heap().gc_cycle_counts().1 > cycles.1);
                        assert_eq!(
                            scope.with_turn_parts(|interp, _| (
                                interp.handle_arena.len(),
                                interp.iteration_anchors_for_trace().len()
                            )),
                            (before_roots.0 + 1, before_roots.1)
                        );
                        assert_eq!(
                            scope.array_buffer_bytes(buffer).unwrap(),
                            (-719i64).to_le_bytes().repeat(4)
                        );
                        Ok(())
                    })
                },
            )
            .unwrap();
            assert_eq!(calls.load(Ordering::Relaxed), 1, "{kind:?}: {bound}");
            assert_eq!(vm.handle_arena.len(), 0);
        }
    }
}
