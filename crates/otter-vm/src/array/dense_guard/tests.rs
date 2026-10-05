//! Dense own-slot guards through mutation, reentry, and snapshot relocation.
//!
//! # Contents
//! - Default and source-realm boxed/numeric slabs under every stress stride.
//! - Descriptor, accessor, sparse, and JSON-source rejection and removal.
//! - Collecting inherited hole getters and restored indexed writability.
//!
//! # Invariants
//! - A prototype-only sidecar admits present own slots, never inherited holes.
//! - Tests keep moving values in production handle scopes and reread them there.
//! - Snapshot restoration rebuilds flags before publishing native eligibility.
//!
//! # See also
//! - [`super`] for the cached eligibility owner.
//! - [`crate::property_dispatch`] for committed element lookup.

use super::*;
use crate::object::PartialPropertyDescriptor;
use crate::{
    ExecutionContext, Interpreter, NativeCallInfo, NativeCtx, NativeError, Value, VmError,
};

fn context(interp: &mut Interpreter) -> ExecutionContext {
    interp
        .link_module(
            crate::test_support::minimal_bytecode_module("dense-own-guard.js"),
            crate::source_registry::SourceRegistry::default(),
        )
        .expect("verifier-valid dense own-slot fixture")
}

fn assert_family(interp: &Interpreter, value: Value, expected: JitElementFamily) {
    let array = value.as_array().expect("array receiver");
    assert_eq!(interp.element_family_of(value), expected);
    interp.gc_heap().read_payload(array, |body| {
        #[cfg(debug_assertions)]
        assert!(
            body.element_cache_is_current(),
            "all native caches are current"
        );
        assert_eq!(body.dense_own_guard.get(), body.current_dense_own_guard());
    });
}

fn moving_own_slots(interp: &mut Interpreter, stride: u32) -> Result<(), VmError> {
    let context = context(interp);
    NativeCtx::with_host_context(
        interp,
        NativeCallInfo::default_call(),
        Some(&context),
        |ctx| {
            ctx.scope(|mut scope| {
                let boxed = scope.array(0)?;
                let child = scope.object()?;
                let marker = scope.number(731.0);
                scope.set(child, "marker", marker)?;
                scope.set_index(boxed, 0, child)?;
                scope.set_index(boxed, 1, child)?;
                let numeric = scope.array(0)?;
                let negative_zero = scope.number(-0.0);
                let nan = scope.number(f64::NAN);
                scope.set_index(numeric, 0, negative_zero)?;
                scope.set_index(numeric, 1, nan)?;
                let holey = scope.array(2)?;
                scope.set_index(holey, 0, negative_zero)?;
                let holey_family = scope.with_turn_parts(|interp, _| {
                    if interp.active_realm_is_extra {
                        JitElementFamily::Generic
                    } else {
                        JitElementFamily::DenseHoleyFloat64
                    }
                });
                let (old_array, old_child, old_boxed_base, old_numeric_base) = scope
                    .with_turn_parts(|interp, _| {
                        let boxed = interp.escape_scoped(boxed);
                        let numeric = interp.escape_scoped(numeric);
                        assert_family(interp, boxed, JitElementFamily::DenseTagged);
                        assert_family(interp, numeric, JitElementFamily::DenseFloat64);
                        assert_family(interp, interp.escape_scoped(holey), holey_family);
                        let array = boxed.as_array().unwrap();
                        assert_eq!(
                            crate::array::prototype_override(array, interp.gc_heap()).is_some(),
                            interp.active_realm_is_extra,
                            "additional source realm is represented by a prototype-only sidecar"
                        );
                        let old_boxed_base = interp
                            .gc_heap()
                            .read_payload(array, |body| body.elements_ptr.get());
                        let old_numeric_base = interp
                            .gc_heap()
                            .read_payload(numeric.as_array().unwrap(), |body| {
                                body.elements_ptr.get()
                            });
                        interp.gc_heap_mut().set_gc_stress(stride, false);
                        (
                            array.offset(),
                            interp.escape_scoped(child).as_object().unwrap().offset(),
                            old_boxed_base,
                            old_numeric_base,
                        )
                    });
                let before =
                    scope.with_turn_parts(|interp, _| interp.gc_heap_mut().gc_stats().clone());
                for _ in 0..16 {
                    scope.scope(|mut temporary| temporary.object().map(|_| ()))?;
                }
                scope.with_turn_parts(|interp, _| {
                    let after = interp.gc_heap_mut().gc_stats().clone();
                    assert!(after.minor_gc_cycles > before.minor_gc_cycles);
                    assert!(after.minor_root_slots_scanned > before.minor_root_slots_scanned);
                    assert!(after.minor_slot_updates > before.minor_slot_updates);
                    let boxed = interp.escape_scoped(boxed);
                    let numeric = interp.escape_scoped(numeric);
                    let child = interp.escape_scoped(child);
                    assert_ne!(boxed.as_array().unwrap().offset(), old_array);
                    assert_ne!(child.as_object().unwrap().offset(), old_child);
                    assert_family(interp, boxed, JitElementFamily::DenseTagged);
                    assert_family(interp, numeric, JitElementFamily::DenseFloat64);
                    let holey = interp.escape_scoped(holey);
                    assert_family(interp, holey, holey_family);
                    assert!(!crate::array::has_own_element(
                        holey.as_array().unwrap(),
                        interp.gc_heap(),
                        1
                    ));
                    let boxed = boxed.as_array().unwrap();
                    let numeric = numeric.as_array().unwrap();
                    assert_ne!(
                        interp
                            .gc_heap()
                            .read_payload(boxed, |body| body.elements_ptr.get()),
                        old_boxed_base
                    );
                    assert_ne!(
                        interp
                            .gc_heap()
                            .read_payload(numeric, |body| body.elements_ptr.get()),
                        old_numeric_base
                    );
                    assert_eq!(crate::array::get(boxed, interp.gc_heap(), 0), child);
                    assert_eq!(crate::array::get(boxed, interp.gc_heap(), 1), child);
                    assert_eq!(
                        crate::object::get(child.as_object().unwrap(), interp.gc_heap(), "marker"),
                        Some(Value::number_i32(731))
                    );
                    assert_eq!(
                        crate::array::get(numeric, interp.gc_heap(), 0)
                            .as_f64()
                            .unwrap()
                            .to_bits(),
                        (-0.0f64).to_bits()
                    );
                    assert!(
                        crate::array::get(numeric, interp.gc_heap(), 1)
                            .as_f64()
                            .unwrap()
                            .is_nan()
                    );
                    interp.gc_heap_mut().set_gc_stress(0, false);
                });
                Ok::<_, NativeError>(())
            })
        },
    )
    .expect("moving present own slots");
    Ok(())
}

#[test]
fn prototype_only_own_slots_keep_boxed_aliases_and_numeric_bits_after_real_gc() {
    let plan = crate::jit::JitElementAccess::packed_double_array();
    assert!(plan.is_packed_double_array());
    assert_eq!(
        plan.guards[0],
        Some(crate::jit::JitBodyGuard::clear(
            (std::mem::size_of::<otter_gc::GcHeader>()
                + crate::array::ARRAY_BODY_DENSE_OWN_GUARD_OFFSET) as u32,
            crate::jit::JitGuardWidth::Byte,
        ))
    );
    for stride in 1..=16 {
        let mut interp = Interpreter::new().expect("fixture interpreter bootstrap");
        interp.gc_heap_mut().set_gc_stress(0, false);
        moving_own_slots(&mut interp, stride).unwrap();
        let realm = interp.create_host_realm().expect("additional source realm");
        interp
            .with_host_realm(realm, |interp| moving_own_slots(interp, stride))
            .unwrap();
    }
}

#[test]
fn descriptor_accessor_sparse_and_source_mutations_refresh_dense_eligibility() {
    let mut interp = Interpreter::new().expect("fixture interpreter bootstrap");
    interp.gc_heap_mut().set_gc_stress(0, false);
    let context = context(&mut interp);
    NativeCtx::with_host_context(
        &mut interp,
        NativeCallInfo::default_call(),
        Some(&context),
        |ctx| {
            ctx.scope(|mut scope| {
                let array = scope.array(0)?;
                let value = scope.boolean(true);
                scope.set_index(array, 0, value)?;
                let prototype = scope.object()?;
                scope.with_turn_parts(|interp, stack| {
                    let receiver = interp.escape_scoped(array);
                    let prototype = interp.escape_scoped(prototype);
                    crate::array::set_prototype_override(
                        receiver.as_array().unwrap(),
                        interp.gc_heap_mut(),
                        Some(prototype),
                    )
                    .unwrap();
                    assert_family(
                        interp,
                        interp.escape_scoped(array),
                        JitElementFamily::DenseTagged,
                    );
                    let receiver = interp.escape_scoped(array);
                    assert!(
                        interp
                            .define_own_property_value(
                                stack,
                                &context,
                                &receiver,
                                &crate::VmPropertyKey::String("0"),
                                PartialPropertyDescriptor {
                                    writable: Some(false),
                                    ..Default::default()
                                }
                            )
                            .unwrap()
                    );
                });
                scope.with_turn_parts(|interp, stack| {
                    let value = interp.escape_scoped(array);
                    let receiver = value.as_array().unwrap();
                    assert_family(interp, value, JitElementFamily::Generic);
                    assert!(
                        !crate::array::get_property_flags(receiver, interp.gc_heap(), "0")
                            .unwrap()
                            .writable()
                    );
                    crate::array::clear_property_flags(receiver, interp.gc_heap_mut(), "0");
                    assert_family(interp, value, JitElementFamily::DenseTagged);
                    assert!(
                        interp
                            .define_own_property_value(
                                stack,
                                &context,
                                &value,
                                &crate::VmPropertyKey::String("0"),
                                PartialPropertyDescriptor {
                                    get: Some(Value::undefined()),
                                    set: Some(Value::undefined()),
                                    enumerable: Some(true),
                                    configurable: Some(true),
                                    ..Default::default()
                                }
                            )
                            .unwrap()
                    );
                    assert_family(
                        interp,
                        interp.escape_scoped(array),
                        JitElementFamily::Generic,
                    );
                });
                scope.with_turn_parts(|interp, stack| {
                    let value = interp.escape_scoped(array);
                    assert_family(interp, value, JitElementFamily::Generic);
                    assert!(
                        interp
                            .define_own_property_value(
                                stack,
                                &context,
                                &value,
                                &crate::VmPropertyKey::String("length"),
                                PartialPropertyDescriptor {
                                    value: Some(Value::number_i32(0)),
                                    ..Default::default()
                                }
                            )
                            .unwrap()
                    );
                });
                scope.set_index(array, 0, value)?;
                scope.with_turn_parts(|interp, _| {
                    assert_family(
                        interp,
                        interp.escape_scoped(array),
                        JitElementFamily::DenseTagged,
                    )
                });
                scope.set_index(array, 1_000_000, value)?;
                scope.with_turn_parts(|interp, stack| {
                    let value = interp.escape_scoped(array);
                    assert_family(interp, value, JitElementFamily::Generic);
                    assert!(
                        interp
                            .define_own_property_value(
                                stack,
                                &context,
                                &value,
                                &crate::VmPropertyKey::String("length"),
                                PartialPropertyDescriptor {
                                    value: Some(Value::number_i32(1)),
                                    ..Default::default()
                                }
                            )
                            .unwrap()
                    );
                });
                scope.with_turn_parts(|interp, _| {
                    let value = interp.escape_scoped(array);
                    assert_family(interp, value, JitElementFamily::DenseTagged);
                    crate::array::prevent_extensions(
                        value.as_array().unwrap(),
                        interp.gc_heap_mut(),
                    );
                    assert_family(
                        interp,
                        interp.escape_scoped(array),
                        JitElementFamily::Generic,
                    );
                    let parsed = crate::array::from_elements_with_source_and_roots(
                        interp.gc_heap_mut(),
                        [Value::number_i32(1)],
                        std::sync::Arc::from(&b"[1]"[..]),
                        &mut |_| {},
                    )
                    .unwrap();
                    assert_family(interp, Value::array(parsed), JitElementFamily::Generic);
                    assert_eq!(
                        crate::array::clean_source_bytes(parsed, interp.gc_heap()).as_deref(),
                        Some(&b"[1]"[..])
                    );
                    crate::array::set(parsed, interp.gc_heap_mut(), 0, Value::number_i32(2))
                        .unwrap();
                    assert_family(interp, Value::array(parsed), JitElementFamily::Generic);
                    assert!(crate::array::clean_source_bytes(parsed, interp.gc_heap()).is_none());
                });
                Ok::<_, NativeError>(())
            })
        },
    )
    .expect("descriptor mutation fixture");
}

fn collecting_hole_getter(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    ctx.scope(|mut scope| {
        let receiver = scope.this();
        let before = scope.with_turn_parts(|interp, _| interp.gc_heap().gc_cycle_counts().0);
        let result = scope.object()?;
        let marker = scope.number(913.0);
        scope.set(result, "marker", marker)?;
        for _ in 0..16 {
            scope.scope(|mut temporary| temporary.object().map(|_| ()))?;
        }
        let after = scope.with_turn_parts(|interp, _| interp.gc_heap().gc_cycle_counts().0);
        assert!(
            after > before,
            "the committed inherited getter really collects"
        );
        let calls = scope.get(receiver, "getterCalls")?;
        let count = scope.raw(calls).as_f64().unwrap_or(0.0);
        let calls = scope.number(count + 1.0);
        scope.set(receiver, "getterCalls", calls)?;
        scope.set(receiver, "getterResult", result)?;
        Ok(scope.finish(result))
    })
}

#[test]
fn default_and_custom_prototype_holes_commit_collecting_getter_exactly_once() {
    for custom in [false, true] {
        for stride in 1..=16 {
            let mut interp = Interpreter::new().expect("fixture interpreter bootstrap");
            interp.gc_heap_mut().set_gc_stress(0, false);
            let context = context(&mut interp);
            NativeCtx::with_host_context(
                &mut interp,
                NativeCallInfo::default_call(),
                Some(&context),
                |ctx| {
                    ctx.scope(|mut scope| {
                        let array = scope.array(2)?;
                        let value = scope.boolean(true);
                        scope.set_index(array, 0, value)?;
                        let getter = scope.native_call(
                            "collectingHoleGetter",
                            0,
                            crate::NativeCall::Static(collecting_hole_getter),
                        )?;
                        let prototype = if custom {
                            scope.object()?
                        } else {
                            let value = scope.with_turn_parts(|interp, _| {
                                interp.constructor_prototype_value("Array").unwrap()
                            });
                            scope.value(value)
                        };
                        scope.with_turn_parts(|interp, stack| {
                            let receiver = interp.escape_scoped(array);
                            let prototype = interp.escape_scoped(prototype);
                            if custom {
                                crate::array::set_prototype_override(
                                    receiver.as_array().unwrap(),
                                    interp.gc_heap_mut(),
                                    Some(prototype),
                                )
                                .unwrap();
                            }
                            assert!(
                                interp
                                    .define_own_property_value(
                                        stack,
                                        &context,
                                        &prototype,
                                        &crate::VmPropertyKey::String("1"),
                                        PartialPropertyDescriptor {
                                            get: Some(interp.escape_scoped(getter)),
                                            set: Some(Value::undefined()),
                                            enumerable: Some(true),
                                            configurable: Some(true),
                                            ..Default::default()
                                        }
                                    )
                                    .unwrap()
                            );
                        });
                        let (old_offset, before) = scope.with_turn_parts(|interp, _| {
                            let receiver = interp.escape_scoped(array);
                            assert_family(interp, receiver, JitElementFamily::DenseTagged);
                            assert!(!crate::array::has_own_element(
                                receiver.as_array().unwrap(),
                                interp.gc_heap(),
                                1
                            ));
                            interp.gc_heap_mut().set_gc_stress(stride, false);
                            (
                                receiver.as_array().unwrap().offset(),
                                interp.gc_heap_mut().gc_stats().clone(),
                            )
                        });
                        let result = scope
                            .with_turn_parts(|interp, stack| {
                                interp.load_element_values(
                                    stack,
                                    &context,
                                    interp.escape_scoped(array),
                                    Value::number_i32(1),
                                )
                            })
                            .expect("committed hole Get");
                        let result = scope.value(result);
                        scope.with_turn_parts(|interp, _| {
                            let after = interp.gc_heap_mut().gc_stats().clone();
                            assert!(after.minor_gc_cycles > before.minor_gc_cycles);
                            assert!(after.minor_slot_updates > before.minor_slot_updates);
                            let receiver = interp.escape_scoped(array);
                            assert_ne!(receiver.as_array().unwrap().offset(), old_offset);
                            assert_family(interp, receiver, JitElementFamily::Generic);
                            let array = receiver.as_array().unwrap();
                            assert_eq!(
                                crate::array::get_named_property(
                                    array,
                                    interp.gc_heap(),
                                    "getterCalls"
                                ),
                                Some(Value::number_i32(1))
                            );
                            assert_eq!(
                                crate::array::get_named_property(
                                    array,
                                    interp.gc_heap(),
                                    "getterResult"
                                ),
                                Some(interp.escape_scoped(result))
                            );
                            assert_eq!(
                                crate::object::get(
                                    interp.escape_scoped(result).as_object().unwrap(),
                                    interp.gc_heap(),
                                    "marker"
                                ),
                                Some(Value::number_i32(913))
                            );
                            assert!(!crate::array::has_own_element(array, interp.gc_heap(), 1));
                        });
                        Ok::<_, NativeError>(())
                    })
                },
            )
            .expect("collecting inherited hole lookup");
        }
    }
}

#[test]
fn restored_indexed_readonly_flags_refresh_guard_after_metadata_rebuild() {
    let mut source = Interpreter::new().expect("fixture interpreter bootstrap");
    source.gc_heap_mut().set_gc_stress(0, false);
    let context = context(&mut source);
    NativeCtx::with_host_context(
        &mut source,
        NativeCallInfo::default_call(),
        Some(&context),
        |ctx| {
            ctx.scope(|mut scope| {
                let array = scope.array(0)?;
                let value = scope.boolean(true);
                scope.set_index(array, 0, value)?;
                scope.with_turn_parts(|interp, stack| {
                    let receiver = interp.escape_scoped(array);
                    assert!(
                        interp
                            .define_own_property_value(
                                stack,
                                &context,
                                &receiver,
                                &crate::VmPropertyKey::String("0"),
                                PartialPropertyDescriptor {
                                    writable: Some(false),
                                    ..Default::default()
                                }
                            )
                            .unwrap()
                    );
                });
                let global = scope.global_this();
                scope.set(global, "denseGuardReadonly", array)?;
                Ok::<_, NativeError>(())
            })
        },
    )
    .expect("snapshot donor array");
    source
        .force_gc()
        .expect("evacuate nursery for snapshot capture");
    let snapshot = source
        .capture_isolate_snapshot()
        .expect("indexed flag metadata is capturable");
    assert!(
        snapshot
            .array_sidecar_flags
            .iter()
            .flatten()
            .any(|(key, writable, _, _)| key == "0" && !writable)
    );
    let mut restored =
        Interpreter::from_isolate_snapshot(&snapshot, &otter_resource::ResourceAccount::default())
            .expect("restore indexed flags");
    NativeCtx::with_host_context(&mut restored, NativeCallInfo::default_call(), None, |ctx| {
        ctx.scope(|mut scope| {
            let array = scope
                .global("denseGuardReadonly")
                .expect("restored global array");
            scope.with_turn_parts(|interp, _| {
                let receiver = interp.escape_scoped(array);
                assert_family(interp, receiver, JitElementFamily::Generic);
                assert_eq!(
                    interp
                        .gc_heap()
                        .read_payload(receiver.as_array().unwrap(), |body| body
                            .dense_own_guard
                            .get()),
                    1
                );
                assert!(
                    !crate::array::get_property_flags(
                        receiver.as_array().unwrap(),
                        interp.gc_heap(),
                        "0"
                    )
                    .unwrap()
                    .writable()
                );
            });
            let replacement = scope.boolean(false);
            scope.set_index(array, 0, replacement)?;
            let actual = scope.index(array, 0)?;
            assert_eq!(scope.raw(actual), Value::boolean(true));
            Ok::<_, NativeError>(())
        })
    })
    .expect("restored readonly own slot stays readonly");
}
