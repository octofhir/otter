//! Default species arrays remain current through length growth and callbacks.
//!
//! # Contents
//! - Direct default-species preparation under real minor GC preserves length,
//!   rooted input aliases, and the original allocator error.
//! - The actual native map driver retains its result and input identities
//!   through species growth and collecting callbacks.
//!
//! # Invariants
//! - Collection is performed by the production allocation or actual callback.
//! - Native callbacks contain no assertions or panic-based observations.
//! - Tests use the existing arena, source owner, native call and array builders.
//!
//! # See also
//! - `super::Interpreter::array_create_with_length` delegates to `scoped_array`.

use super::*;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[test]
fn default_species_length_growth_returns_current_body_and_preserves_actual_oom() {
    let mut vm = Interpreter::new().expect("species bootstrap");
    vm.gc_heap.set_gc_stress(0, false);
    vm.force_gc().expect("settle nursery before fresh inputs");
    vm.with_handle_scope(|vm, scope| {
        let source = vm.scoped_array(scope, 0).expect("fresh source");
        let child = vm.scoped_object_bare(scope).expect("pending argument");
        let mut object = vm.escape_scoped(child).as_object().unwrap();
        vm.create_data_property(&mut object, "marker", Value::number_i32(719))
            .expect("argument marker");
        let source_alias = vm.scoped_value(scope, vm.escape_scoped(source));
        let child_alias = vm.scoped_value(scope, vm.escape_scoped(child));
        let old_source = vm.escape_scoped(source).as_array().unwrap().offset();
        let old_child = vm.escape_scoped(child).as_object().unwrap().offset();
        assert!(unsafe {
            (*vm.escape_scoped(source).as_array().unwrap().as_header_ptr()).is_young()
        });
        assert!(unsafe {
            (*vm.escape_scoped(child).as_object().unwrap().as_header_ptr()).is_young()
        });
        let before = vm.gc_heap.gc_cycle_counts();
        let pending = [vm.escape_scoped(child)];
        let source_value = vm.escape_scoped(source);
        vm.gc_heap.set_gc_stress(1, false);
        let result = vm.array_create_with_length(source_value, 29, &[&pending]);
        vm.gc_heap.set_gc_stress(0, false);
        let result = result.expect("default species length preparation");
        let result = vm.scoped_value(scope, result);
        assert!(vm.gc_heap.gc_cycle_counts().0 > before.0);
        let array = vm.escape_scoped(result).as_array().unwrap();
        assert_eq!(crate::array::len(array, vm.gc_heap()), 29);
        assert!(!crate::array::has_own_element(array, vm.gc_heap(), 28));
        assert_ne!(
            vm.escape_scoped(source).as_array().unwrap().offset(),
            old_source
        );
        assert_ne!(
            vm.escape_scoped(child).as_object().unwrap().offset(),
            old_child
        );
        assert_eq!(vm.escape_scoped(source), vm.escape_scoped(source_alias));
        assert_eq!(vm.escape_scoped(child), vm.escape_scoped(child_alias));
        assert_eq!(
            crate::object::get_own(
                vm.escape_scoped(child).as_object().unwrap(),
                vm.gc_heap(),
                "marker"
            ),
            Some(Value::number_i32(719))
        );
        vm.force_gc().expect("retain current species body");
        assert_eq!(
            crate::array::len(vm.escape_scoped(result).as_array().unwrap(), vm.gc_heap()),
            29
        );
    });

    let cap = 4 * 1024 * 1024;
    let mut vm = Interpreter::with_string_heap_cap(cap).expect("capped species bootstrap");
    vm.gc_heap.set_gc_stress(0, false);
    vm.force_gc().expect("settle cap ledger");
    let before_anchors = vm.iteration_anchors_for_trace().len();
    let error = vm
        .array_create_with_length(Value::undefined(), 1 << 20, &[])
        .expect_err("physical slab is larger than the cap");
    match error {
        VmError::OutOfMemory {
            requested_bytes,
            heap_limit_bytes: actual,
            ..
        } => {
            assert!(requested_bytes > cap);
            assert_eq!(actual, cap);
        }
        other => panic!("actual allocator cause was replaced: {other:?}"),
    }
    assert_eq!(vm.iteration_anchors_for_trace().len(), before_anchors);
}

#[test]
fn actual_native_map_preserves_species_output_and_aliases_across_collecting_callbacks() {
    let mut vm = Interpreter::new().expect("map bootstrap");
    vm.gc_heap.set_gc_stress(0, false);
    let context = vm
        .link_module(
            crate::test_support::minimal_bytecode_module("species-map-rooting"),
            crate::source_registry::SourceRegistry::default(),
        )
        .expect("actual admitted source");
    vm.force_gc().expect("settle nursery before map inputs");
    let calls = Arc::new(AtomicUsize::new(0));
    let callback_calls = Arc::clone(&calls);
    let anchor_count = vm.iteration_anchors_for_trace().len();
    NativeCtx::with_host_context(
        &mut vm,
        crate::NativeCallInfo::default_call(),
        Some(&context),
        |ctx| {
            ctx.scope(|mut scope| -> Result<(), NativeError> {
                let callback = scope
                    .context()
                    .native_value("SpeciesMapCollect", SmallVec::new(), move |ctx, args, _| {
                        callback_calls.fetch_add(1, Ordering::SeqCst);
                        ctx.scope(|mut scope| {
                            let value =
                                scope.value(args.first().copied().unwrap_or(Value::undefined()));
                            let index =
                                scope.value(args.get(1).copied().unwrap_or(Value::undefined()));
                            let source =
                                scope.value(args.get(2).copied().unwrap_or(Value::undefined()));
                            let this_value = *scope.context().this_value();
                            let receiver = scope.value(this_value);
                            scope
                                .context()
                                .interp_mut()
                                .force_gc()
                                .map_err(NativeError::from)?;
                            let pair = scope.array(2)?;
                            scope.set_index(pair, 0, value)?;
                            scope.set_index(pair, 1, index)?;
                            // Reads after the callback collection exercise the actual
                            // receiver and callback-O handles, without assertions in ABI code.
                            let _ = scope.array_length(source)?;
                            let _ = scope.get(receiver, "marker")?;
                            Ok(scope.finish(pair))
                        })
                    })
                    .map_err(NativeError::from)?;
                let callback = scope.value(callback);
                let source = scope.array(29)?;
                let child = scope.object()?;
                let marker = scope.number(719.0);
                scope.define(
                    child,
                    "marker",
                    marker,
                    crate::object::PropertyFlags::default(),
                )?;
                let alias = scope.value(scope.raw(child));
                for index in 0..29 {
                    scope.set_index(source, index, child)?;
                }
                let method = scope.get(source, "map")?;
                let before_source = scope.raw(source).as_array().unwrap().offset();
                let before_child = scope.raw(child).as_object().unwrap().offset();
                assert!(unsafe {
                    (*scope.raw(source).as_array().unwrap().as_header_ptr()).is_young()
                });
                assert!(unsafe {
                    (*scope.raw(child).as_object().unwrap().as_header_ptr()).is_young()
                });
                let before = scope.context().heap().gc_cycle_counts();
                scope.context().heap_mut().set_gc_stress(1, false);
                let result = scope.call(method, source, &[callback, child]);
                scope.context().heap_mut().set_gc_stress(0, false);
                let result = result?;
                assert_eq!(calls.load(Ordering::SeqCst), 29);
                assert!(scope.context().heap().gc_cycle_counts().0 > before.0);
                assert!(scope.context().heap().gc_cycle_counts().1 >= before.1 + 29);
                assert_ne!(
                    scope.raw(source).as_array().unwrap().offset(),
                    before_source
                );
                assert_ne!(scope.raw(child).as_object().unwrap().offset(), before_child);
                assert_eq!(scope.raw(child), scope.raw(alias));
                assert_eq!(scope.array_length(result)?, 29);
                let observed_marker = scope.get(child, "marker")?;
                assert_eq!(scope.raw(observed_marker).as_f64(), Some(719.0));
                for index in 0..29 {
                    let pair = scope.index(result, index)?;
                    assert_eq!(scope.array_length(pair)?, 2);
                    let value = scope.index(pair, 0)?;
                    let actual_index = scope.index(pair, 1)?;
                    assert_eq!(scope.raw(value), scope.raw(child));
                    assert_eq!(scope.raw(actual_index).as_f64(), Some(index as f64));
                    let original = scope.index(source, index)?;
                    assert_eq!(scope.raw(original), scope.raw(child));
                }
                scope
                    .context()
                    .interp_mut()
                    .force_gc()
                    .map_err(NativeError::from)?;
                assert_eq!(scope.array_length(result)?, 29);
                assert_eq!(scope.raw(child), scope.raw(alias));
                Ok(())
            })
        },
    )
    .expect("actual map completion");
    assert_eq!(vm.iteration_anchors_for_trace().len(), anchor_count);
    assert_eq!(calls.load(Ordering::SeqCst), 29);
}

#[test]
fn flat_splice_and_concat_keep_collecting_inputs_and_release_abrupt_species_anchors() {
    for operation in ["flat", "splice", "concat"] {
        let mut vm = Interpreter::new().expect("species consumer bootstrap");
        vm.gc_heap.set_gc_stress(0, false);
        let context = vm
            .link_module(
                crate::test_support::minimal_bytecode_module("species-consumer-rooting"),
                crate::source_registry::SourceRegistry::default(),
            )
            .expect("actual consumer source");
        vm.force_gc()
            .expect("settle before normal young consumer inputs");
        let observed = Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
        NativeCtx::with_host_context(
            &mut vm,
            crate::NativeCallInfo::default_call(),
            Some(&context),
            |ctx| {
                ctx.scope(|mut scope| -> Result<(), NativeError> {
                    let first_observed = Arc::clone(&observed);
                    let first = scope.native_closure(
                        "SpeciesFirstCoercion",
                        0,
                        &[],
                        move |ctx, _, _| {
                            first_observed
                                .lock()
                                .map_err(|_| NativeError::InvalidOperand)?
                                .push(1);
                            ctx.interp_mut().force_gc().map_err(NativeError::from)?;
                            Ok(Value::number_i32(if operation == "flat" { 1 } else { 0 }))
                        },
                    )?;
                    let second_observed = Arc::clone(&observed);
                    let second = scope.native_closure(
                        "SpeciesSecondCoercion",
                        0,
                        &[],
                        move |ctx, _, _| {
                            second_observed
                                .lock()
                                .map_err(|_| NativeError::InvalidOperand)?
                                .push(2);
                            ctx.interp_mut().force_gc().map_err(NativeError::from)?;
                            Ok(Value::number_i32(1))
                        },
                    )?;
                    let child = scope.object()?;
                    let alias = scope.value(scope.raw(child));
                    let marker = scope.number(719.0);
                    scope.define(
                        child,
                        "marker",
                        marker,
                        crate::object::PropertyFlags::data_default(),
                    )?;
                    let array_ctor = scope.global("Array").expect("canonical Array");
                    let prototype = scope.get(array_ctor, "prototype")?;
                    let method = scope.get(prototype, operation)?;
                    let before_cycles = scope.context().heap().gc_cycle_counts();

                    if operation == "flat" {
                        let source = scope.object()?;
                        let nested = scope.array(1)?;
                        scope.set_index(nested, 0, child)?;
                        scope.set(source, "0", nested)?;
                        let undefined = scope.undefined();
                        scope.define_accessor(
                            source,
                            "length",
                            first,
                            undefined,
                            crate::object::PropertyFlags::new(false, true, true),
                        )?;
                        let depth = scope.object()?;
                        scope.set(depth, "valueOf", second)?;
                        let before_source = scope.raw(source).as_object().unwrap().offset();
                        let before_child = scope.raw(child).as_object().unwrap().offset();
                        assert!(unsafe {
                            (*scope.raw(source).as_object().unwrap().as_header_ptr()).is_young()
                        });
                        assert!(unsafe {
                            (*scope.raw(child).as_object().unwrap().as_header_ptr()).is_young()
                        });
                        let result = scope.call(method, source, &[depth])?;
                        assert_eq!(observed.lock().unwrap().as_slice(), &[1, 2]);
                        assert_ne!(
                            scope.raw(source).as_object().unwrap().offset(),
                            before_source
                        );
                        assert_ne!(scope.raw(child).as_object().unwrap().offset(), before_child);
                        assert_eq!(scope.array_length(result)?, 1);
                        let value = scope.index(result, 0)?;
                        assert_eq!(scope.raw(value), scope.raw(child));
                    } else if operation == "splice" {
                        let source = scope.array(2)?;
                        let old_first = scope.number(31.0);
                        let old_second = scope.number(57.0);
                        scope.set_index(source, 0, old_first)?;
                        scope.set_index(source, 1, old_second)?;
                        let start = scope.object()?;
                        let count = scope.object()?;
                        scope.set(start, "valueOf", first)?;
                        scope.set(count, "valueOf", second)?;
                        let before_source = scope.raw(source).as_array().unwrap().offset();
                        let before_child = scope.raw(child).as_object().unwrap().offset();
                        assert!(unsafe {
                            (*scope.raw(source).as_array().unwrap().as_header_ptr()).is_young()
                        });
                        assert!(unsafe {
                            (*scope.raw(child).as_object().unwrap().as_header_ptr()).is_young()
                        });
                        let removed = scope.call(method, source, &[start, count, child])?;
                        assert_eq!(observed.lock().unwrap().as_slice(), &[1, 2]);
                        assert_ne!(
                            scope.raw(source).as_array().unwrap().offset(),
                            before_source
                        );
                        assert_ne!(scope.raw(child).as_object().unwrap().offset(), before_child);
                        assert_eq!(scope.array_length(removed)?, 1);
                        let removed_value = scope.index(removed, 0)?;
                        assert_eq!(scope.raw(removed_value).as_f64(), Some(31.0));
                        assert_eq!(scope.array_length(source)?, 2);
                        let inserted = scope.index(source, 0)?;
                        let retained = scope.index(source, 1)?;
                        assert_eq!(scope.raw(inserted), scope.raw(child));
                        assert_eq!(scope.raw(retained).as_f64(), Some(57.0));
                    } else {
                        let source = scope.array(1)?;
                        scope.set_index(source, 0, child)?;
                        let throwing_observed = Arc::clone(&observed);
                        let throwing = scope.native_closure(
                            "SpeciesAbruptConstructor",
                            0,
                            &[child],
                            move |ctx, _, captures| {
                                throwing_observed
                                    .lock()
                                    .map_err(|_| NativeError::InvalidOperand)?
                                    .push(3);
                                ctx.scope(|mut scope| {
                                    let original = scope.value(captures[0]);
                                    scope
                                        .context()
                                        .interp_mut()
                                        .force_gc()
                                        .map_err(NativeError::from)?;
                                    let original = scope.raw(original);
                                    Err(scope
                                        .context()
                                        .throw_value("SpeciesAbruptConstructor", original))
                                })
                            },
                        )?;
                        let undefined = scope.undefined();
                        scope.define_accessor(
                            source,
                            "constructor",
                            throwing,
                            undefined,
                            crate::object::PropertyFlags::new(false, false, true),
                        )?;
                        let before_source = scope.raw(source).as_array().unwrap().offset();
                        let before_child = scope.raw(child).as_object().unwrap().offset();
                        assert!(unsafe {
                            (*scope.raw(source).as_array().unwrap().as_header_ptr()).is_young()
                        });
                        assert!(unsafe {
                            (*scope.raw(child).as_object().unwrap().as_header_ptr()).is_young()
                        });
                        for _ in 0..3 {
                            let anchors = scope
                                .context()
                                .interp_mut()
                                .iteration_anchors_for_trace()
                                .len();
                            let error = scope
                                .call(method, source, &[child])
                                .expect_err("actual species getter throws");
                            assert!(matches!(error, NativeError::Thrown { .. }));
                            assert_eq!(
                                scope
                                    .context()
                                    .interp_mut()
                                    .iteration_anchors_for_trace()
                                    .len(),
                                anchors
                            );
                            let reason = scope
                                .context()
                                .interp_mut()
                                .take_pending_uncaught_throw()
                                .expect("exact original thrown value");
                            assert_eq!(reason, scope.raw(child));
                            scope.context().clear_pending_error();
                        }
                        assert_eq!(observed.lock().unwrap().as_slice(), &[3, 3, 3]);
                        assert_ne!(
                            scope.raw(source).as_array().unwrap().offset(),
                            before_source
                        );
                        assert_ne!(scope.raw(child).as_object().unwrap().offset(), before_child);
                        let value = scope.index(source, 0)?;
                        assert_eq!(scope.raw(value), scope.raw(child));
                    }
                    assert!(scope.context().heap().gc_cycle_counts().1 > before_cycles.1);
                    assert_eq!(scope.raw(child), scope.raw(alias));
                    let marker = scope.get(child, "marker")?;
                    assert_eq!(scope.raw(marker).as_f64(), Some(719.0));
                    scope
                        .context()
                        .interp_mut()
                        .force_gc()
                        .map_err(NativeError::from)?;
                    assert_eq!(scope.raw(child), scope.raw(alias));
                    Ok(())
                })
            },
        )
        .expect("collecting species consumer completion");
        assert!(vm.iteration_anchors_for_trace().is_empty());
    }
}
