//! Array mutation preparation, relocation and numeric bulk admission.
//!
//! # Contents
//! - Length getter/number coercion, start/end and empty-result GC boundaries.
//! - Current source/result/payload identities and exact scoped-root cleanup.
//! - Numeric bulk admission accounts for stores and checks cancellation.
//!
//! # Invariants
//! - Collecting preparation cases start with young source and payload values.
//! - Their selected callback forces full GC inside the actual native operation;
//!   source and payload move, with assertions outside the callback.
//! - Numeric bulk checks prove admission, charged stores and cancellation.
//!
//! # See also
//! - `super::mutate` owns preparation and live mutations.

use super::*;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[test]
fn numeric_bulk_admission_charges_stores_and_observes_cancellation() {
    let mut vm = Interpreter::new().unwrap();
    NativeCtx::with_host_context(
        &mut vm,
        crate::NativeCallInfo::default_call(),
        None,
        |ctx| {
            ctx.scope(|mut scope| -> Result<(), NativeError> {
                let buffer = scope.array_buffer_from_bytes(vec![0; 16])?;
                let view =
                    scope.typed_array_view(buffer, crate::binary::TypedArrayKind::Uint16, 2, 5)?;
                scope.with_turn_parts(|interp, _| {
                    let raw = interp.escape_scoped(view);
                    let before = interp.work_budget_stats().work_units_executed;
                    let cycles = interp.gc_heap().gc_cycle_counts();
                    assert!(
                        interp
                            .try_fill_typed_array(raw, Value::number_i32(719), 1, 4)
                            .unwrap()
                    );
                    assert_eq!(interp.work_budget_stats().work_units_executed - before, 3);
                    assert_eq!(interp.gc_heap().gc_cycle_counts(), cycles);
                    interp.interrupt_handle().interrupt();
                    assert!(matches!(
                        interp.try_fill_typed_array(raw, Value::number_i32(19), 1, 4),
                        Err(VmError::Interrupted)
                    ));
                    interp.interrupt_handle().reset();
                });
                assert_eq!(
                    scope.array_buffer_bytes(buffer).unwrap(),
                    [0, 0, 0, 0, 207, 2, 207, 2, 207, 2, 0, 0, 0, 0, 0, 0]
                );
                Ok(())
            })
        },
    )
    .unwrap();
}

#[test]
fn mutation_preparation_moves_inputs_and_keeps_exact_aliases_and_root_extent() {
    for case in [
        "reverse-empty",
        "reverse-pair",
        "fill-empty",
        "fill-length",
        "fill-length-number",
        "fill-start",
        "fill-end",
    ] {
        let mut vm = Interpreter::new().unwrap();
        vm.gc_heap.set_gc_stress(0, false);
        let context = vm
            .link_module(
                crate::test_support::minimal_bytecode_module("mutate-preparation"),
                crate::source_registry::SourceRegistry::default(),
            )
            .unwrap();
        vm.force_gc().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        NativeCtx::with_host_context(
            &mut vm,
            crate::NativeCallInfo::default_call(),
            Some(&context),
            |ctx| {
                ctx.scope(|mut scope| -> Result<(), NativeError> {
                    let payload = scope.object()?;
                    let other = scope.object()?;
                    let dense = matches!(case, "fill-start" | "fill-end");
                    let source = if dense {
                        scope.array(2)?
                    } else {
                        scope.object()?
                    };
                    let two = scope.number(2.0);
                    if !dense {
                        scope.set(source, "length", two)?;
                    }
                    let result_number =
                        if matches!(case, "reverse-empty" | "fill-empty" | "fill-start") {
                            0
                        } else {
                            2
                        };
                    let observed = observed.clone();
                    let collecting = scope.native_closure(
                        "MutatePreparationCollect",
                        0,
                        &[],
                        move |ctx, _, _| {
                            observed.fetch_add(1, Ordering::Relaxed);
                            ctx.interp_mut().force_gc().map_err(NativeError::from)?;
                            Ok(Value::number_i32(result_number))
                        },
                    )?;
                    let start = scope.object()?;
                    let end = scope.object()?;
                    let constant_zero =
                        scope.native_closure("MutateStart", 0, &[], |_, _, _| {
                            Ok(Value::number_i32(0))
                        })?;
                    let constant_two = scope
                        .native_closure("MutateEnd", 0, &[], |_, _, _| Ok(Value::number_i32(2)))?;
                    scope.set(
                        start,
                        "valueOf",
                        if case == "fill-start" {
                            collecting
                        } else {
                            constant_zero
                        },
                    )?;
                    scope.set(
                        end,
                        "valueOf",
                        if case == "fill-end" {
                            collecting
                        } else {
                            constant_two
                        },
                    )?;
                    if matches!(
                        case,
                        "reverse-empty" | "reverse-pair" | "fill-empty" | "fill-length"
                    ) {
                        let undefined = scope.undefined();
                        scope.define_accessor(
                            source,
                            "length",
                            collecting,
                            undefined,
                            object::PropertyFlags::new(false, true, true),
                        )?;
                    } else if case == "fill-length-number" {
                        let length = scope.object()?;
                        scope.set(length, "valueOf", collecting)?;
                        scope.set(source, "length", length)?;
                    }
                    if case == "reverse-pair" {
                        scope.set(source, "0", payload)?;
                        scope.set(source, "1", other)?;
                    }
                    let marker = scope.number(719.0);
                    scope.set(payload, "marker", marker)?;
                    let array = scope.global("Array").unwrap();
                    let prototype = scope.get(array, "prototype")?;
                    let reverse = case.starts_with("reverse");
                    let method = scope.get(prototype, if reverse { "reverse" } else { "fill" })?;
                    let raw = scope.raw(source);
                    let before_source = if dense {
                        raw.as_array().unwrap().offset()
                    } else {
                        raw.as_object().unwrap().offset()
                    };
                    // SAFETY: these current handles are read without an intervening allocation.
                    assert!(unsafe {
                        if dense {
                            (*raw.as_array().unwrap().as_header_ptr()).is_young()
                        } else {
                            (*raw.as_object().unwrap().as_header_ptr()).is_young()
                        }
                    });
                    let before_payload = scope.raw(payload).as_object().unwrap().offset();
                    // SAFETY: the scoped payload is current and no allocation occurs in this read.
                    assert!(unsafe {
                        (*scope.raw(payload).as_object().unwrap().as_header_ptr()).is_young()
                    });
                    let before_gc = scope.context().heap().gc_cycle_counts().1;
                    if dense {
                        scope.with_turn_parts(|interp, _| {
                            let array = interp.escape_scoped(source).as_array().unwrap();
                            assert!(crate::array::can_fast_fill_dense_range(
                                array,
                                interp.gc_heap(),
                                0,
                                2
                            ));
                        });
                    }
                    let before_roots = scope.with_turn_parts(|interp, _| {
                        (
                            interp.handle_arena.len(),
                            interp.iteration_anchors_for_trace().len(),
                        )
                    });
                    let result = if reverse {
                        scope.call(method, source, &[])?
                    } else {
                        scope.call(method, source, &[payload, start, end])?
                    };
                    assert_eq!(
                        scope.raw(result),
                        scope.raw(source),
                        "{case}: actual receiver returned"
                    );
                    assert_eq!(
                        scope.with_turn_parts(|interp, _| (
                            interp.handle_arena.len(),
                            interp.iteration_anchors_for_trace().len()
                        )),
                        (before_roots.0 + 1, before_roots.1),
                        "{case}: only the returned Local is retained"
                    );
                    assert!(scope.context().heap().gc_cycle_counts().1 > before_gc);
                    let raw = scope.raw(source);
                    assert_ne!(
                        if dense {
                            raw.as_array().unwrap().offset()
                        } else {
                            raw.as_object().unwrap().offset()
                        },
                        before_source,
                        "{case}: source moved"
                    );
                    assert_ne!(
                        scope.raw(payload).as_object().unwrap().offset(),
                        before_payload,
                        "{case}: pending payload moved"
                    );
                    if case == "reverse-pair" {
                        let first = scope.get(source, "0")?;
                        let last = scope.get(source, "1")?;
                        assert_eq!(scope.raw(first), scope.raw(other));
                        assert_eq!(scope.raw(last), scope.raw(payload));
                    } else if !reverse && case != "fill-empty" {
                        let first = scope.get(source, "0")?;
                        let last = scope.get(source, "1")?;
                        assert_eq!(scope.raw(first), scope.raw(payload));
                        assert_eq!(scope.raw(last), scope.raw(payload));
                        let marker = scope.get(first, "marker")?;
                        assert_eq!(scope.raw(marker).as_f64(), Some(719.0));
                    }
                    Ok(())
                })
            },
        )
        .unwrap();
        assert_eq!(
            calls.load(Ordering::Relaxed),
            1,
            "{case}: exactly one collecting callback"
        );
        assert_eq!(vm.handle_arena.len(), 0);
    }
}
