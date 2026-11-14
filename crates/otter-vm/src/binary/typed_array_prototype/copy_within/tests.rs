//! Full byte and moving-GC proofs for directional copyWithin batches.
//!
//! # Contents
//! - Copies spanning multiple 64 KiB boundaries in both overlap directions.
//! - Actual young receiver and bound relocation in each coercion.
//! - Independent immutable-source byte oracle and exact scoped-root cleanup.
//!
//! # Invariants
//! - Expected bytes are computed from the original snapshot, never an in-place copy.
//! - No element is decoded; every byte inside and outside the view is checked.
//! - Exactly one selected callback forces collection in the installed native.
//!
//! # See also
//! - `super::run` owns preparation, live validation and copy batches.

use crate::{Interpreter, NativeCallInfo, NativeCtx, NativeError, Value, binary::TypedArrayKind};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[test]
fn overlap_batches_preserve_every_byte_after_each_bound_moves_the_receiver() {
    const VIEW_BYTES: usize = 2 * 65536 + 48;
    for kind in [TypedArrayKind::Float64, TypedArrayKind::BigUint64] {
        for backward in [false, true] {
            for collecting_bound in 0..3 {
                let original: Vec<u8> = (0..VIEW_BYTES + 16)
                    .map(|i| ((i * 37 + 11) % 251) as u8)
                    .collect();
                let elements = VIEW_BYTES / 8;
                let indices = if backward {
                    [1, 0, elements - 1]
                } else {
                    [0, 1, elements]
                };
                let source = 8 + indices[1] * 8;
                let target = 8 + indices[0] * 8;
                let count = (elements - 1) * 8;
                let mut expected = original.clone();
                // The oracle reads an immutable snapshot, independently of the
                // candidate's overlapping in-place move and batch direction.
                expected[target..target + count].copy_from_slice(&original[source..source + count]);
                let mut vm = Interpreter::new().unwrap();
                vm.gc_heap.set_gc_stress(0, false);
                let context = vm
                    .link_module(
                        crate::test_support::minimal_bytecode_module("copy-within-gc"),
                        crate::source_registry::SourceRegistry::default(),
                    )
                    .unwrap();
                vm.force_gc().unwrap();
                let calls = Arc::new(AtomicUsize::new(0));
                NativeCtx::with_host_context(
                    &mut vm,
                    NativeCallInfo::default_call(),
                    Some(&context),
                    |ctx| {
                        ctx.scope(|mut scope| -> Result<(), NativeError> {
                            let buffer = scope.array_buffer_from_bytes(original)?;
                            let view = scope.typed_array_view(buffer, kind, 8, elements)?;
                            let method = scope.get(view, "copyWithin")?;
                            let mut args = Vec::new();
                            for (index, number) in indices.into_iter().enumerate() {
                                let argument = scope.object()?;
                                let calls = calls.clone();
                                let coercer = scope.native_closure(
                                    "CopyBound",
                                    0,
                                    &[],
                                    move |ctx, _, _| {
                                        if index == collecting_bound {
                                            calls.fetch_add(1, Ordering::Relaxed);
                                            ctx.interp_mut()
                                                .force_gc()
                                                .map_err(NativeError::from)?;
                                        }
                                        Ok(Value::number_f64(number as f64))
                                    },
                                )?;
                                scope.set(argument, "valueOf", coercer)?;
                                args.push(argument);
                            }
                            let before_view = scope
                                .raw(view)
                                .as_typed_array(scope.context().heap())
                                .unwrap()
                                .handle();
                            let before_buffer = scope
                                .raw(buffer)
                                .as_raw_gc()
                                .unwrap()
                                .checked_cast::<crate::binary::LocalArrayBufferBodyGc>()
                                .unwrap();
                            // SAFETY: the scoped cells are current and no allocation intervenes.
                            unsafe {
                                assert!((*before_view.as_header_ptr()).is_young());
                                assert!(!(*before_buffer.as_header_ptr()).is_young());
                            }
                            let before_args: Vec<_> = args
                                .iter()
                                .map(|arg| {
                                    let object = scope.raw(*arg).as_object().unwrap();
                                    // SAFETY: this scoped bound is current, with no intervening allocation.
                                    assert!(unsafe { (*object.as_header_ptr()).is_young() });
                                    object.offset()
                                })
                                .collect();
                            let roots = scope.with_turn_parts(|interp, _| {
                                (
                                    interp.handle_arena.len(),
                                    interp.iteration_anchors_for_trace().len(),
                                )
                            });
                            let cycles = scope.context().heap().gc_cycle_counts();
                            let result = scope.call(method, view, &args)?;
                            assert_eq!(scope.raw(result), scope.raw(view));
                            assert_ne!(
                                scope.raw(view).as_raw_gc().unwrap().0,
                                before_view.offset()
                            );
                            // ArrayBuffer bodies are allocated old by their owner.
                            assert_eq!(
                                scope.raw(buffer).as_raw_gc().unwrap().0,
                                before_buffer.offset()
                            );
                            for (arg, before) in args.iter().zip(before_args) {
                                assert_ne!(scope.raw(*arg).as_object().unwrap().offset(), before);
                            }
                            assert!(scope.context().heap().gc_cycle_counts().1 > cycles.1);
                            assert_eq!(scope.array_buffer_bytes(buffer).unwrap(), expected);
                            assert_eq!(
                                scope.with_turn_parts(|interp, _| (
                                    interp.handle_arena.len(),
                                    interp.iteration_anchors_for_trace().len()
                                )),
                                (roots.0 + 1, roots.1)
                            );
                            Ok(())
                        })
                    },
                )
                .unwrap();
                assert_eq!(calls.load(Ordering::Relaxed), 1);
                assert_eq!(vm.handle_arena.len(), 0);
            }
        }
    }
}

#[test]
fn interrupted_copy_cannot_commit_the_first_byte_in_either_direction() {
    for backward in [false, true] {
        let mut vm = Interpreter::new().unwrap();
        let context = vm
            .link_module(
                crate::test_support::minimal_bytecode_module("copy-interrupt"),
                crate::source_registry::SourceRegistry::default(),
            )
            .unwrap();
        NativeCtx::with_host_context(
            &mut vm,
            NativeCallInfo::default_call(),
            Some(&context),
            |ctx| {
                ctx.scope(|mut scope| -> Result<(), NativeError> {
                    let original: Vec<u8> = (0..64).collect();
                    let buffer = scope.array_buffer_from_bytes(original.clone())?;
                    let view = scope.typed_array_view(buffer, TypedArrayKind::Uint8, 0, 64)?;
                    // Arm cancellation inside the native entry, after the caller's
                    // polls, then enter the actual copy owner with primitive bounds.
                    let method =
                        scope.native_closure("InterruptedCopy", 3, &[], |ctx, args, _| {
                            ctx.interp_mut().interrupt_handle().interrupt();
                            let result = super::run(ctx, args);
                            ctx.interp_mut().interrupt_handle().reset();
                            assert!(matches!(result, Err(NativeError::Interrupted)));
                            Ok(Value::undefined())
                        })?;
                    let target = scope.number(if backward { 1.0 } else { 0.0 });
                    let start = scope.number(if backward { 0.0 } else { 1.0 });
                    let end = scope.number(if backward { 63.0 } else { 64.0 });
                    scope.call(method, view, &[target, start, end])?;
                    assert_eq!(scope.array_buffer_bytes(buffer).unwrap(), original);
                    Ok(())
                })
            },
        )
        .unwrap();
    }
}
