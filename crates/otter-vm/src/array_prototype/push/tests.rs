//! Real moving-GC proofs for native scoped push.
//!
//! # Contents
//! - Multiargument slab growth in the default and an additional realm.
//! - A collecting inherited setter followed by further appends and length Set.
//!
//! # Invariants
//! - Pending children have no fixture handles during the native call: the
//!   production push scope owns their roots until they enter the receiver.
//! - Every stride collects inside push and changes the receiver/child offsets.
//! - A later minor collection keeps children alive through the receiver alone.
//!
//! # See also
//! - [`super`] for the production scope and spec driver.

use super::*;
use crate::NativeCallInfo;

const CHILDREN: usize = 9;

struct StressPrime {
    _word: u64,
}

impl otter_gc::SafeTraceable for StressPrime {
    const TYPE_TAG: u8 = 0xeb;

    fn trace_slots_safe(&mut self, _visitor: &mut otter_gc::raw::SlotVisitor<'_>) {}
}

fn collecting_setter(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    ctx.scope(|mut scope| {
        let receiver = scope.this();
        let argument = scope.argument(args, 0);
        let before = scope.with_turn_parts(|interp, _| interp.gc_heap().gc_cycle_counts().0);
        // Real allocations in the reentrant native, enough to collect at
        // every configured stride. Nested scopes bound temporary root storage.
        for _ in 0..16 {
            scope.scope(|mut child| child.object().map(|_| ()))?;
        }
        let after = scope.with_turn_parts(|interp, _| interp.gc_heap().gc_cycle_counts().0);
        assert!(after > before, "the indexed setter must actually collect");
        scope.set(receiver, "seen", argument)?;
        let calls = scope.get(receiver, "setterCalls")?;
        let count = scope.raw(calls).as_f64().unwrap_or(0.0);
        let calls = scope.number(count + 1.0);
        scope.set(receiver, "setterCalls", calls)?;
        let result = scope.undefined();
        Ok(scope.finish(result))
    })
}

fn prepare_inputs(
    interp: &mut Interpreter,
    context: &ExecutionContext,
    setter: bool,
) -> (Value, Vec<Value>, u32, Vec<u32>) {
    NativeCtx::with_host_context(
        interp,
        NativeCallInfo::default_call(),
        Some(context),
        |ctx| {
            ctx.scope(|mut scope| {
                let receiver = scope.array(0)?;
                let mut children = Vec::new();
                for index in 0..CHILDREN {
                    let child = scope.object()?;
                    let marker = scope.number((731 + index) as f64);
                    scope.set(child, "marker", marker)?;
                    children.push(child);
                }
                if setter {
                    let setter = scope.native_call(
                        "collectingIndexSetter",
                        1,
                        crate::NativeCall::Static(collecting_setter),
                    )?;
                    scope.with_turn_parts(|interp, stack| {
                        let setter = interp.escape_scoped(setter);
                        let prototype = interp.constructor_prototype_value("Array").unwrap();
                        // A genuine inherited index setter keeps the receiver
                        // empty and trips the production element protector.
                        assert!(
                            interp
                                .define_own_property_value(
                                    stack,
                                    context,
                                    &prototype,
                                    &crate::VmPropertyKey::String("0"),
                                    crate::object::PartialPropertyDescriptor {
                                        get: Some(Value::undefined()),
                                        set: Some(setter),
                                        enumerable: Some(true),
                                        configurable: Some(true),
                                        ..Default::default()
                                    },
                                )
                                .expect("define inherited collecting setter")
                        );
                        let receiver = interp.escape_scoped(receiver).as_array().unwrap();
                        assert_eq!(crate::array::len(receiver, interp.gc_heap()), 0);
                        assert!(crate::array::get_accessor(receiver, interp.gc_heap(), "0").is_none());
                        assert!(interp.array_index_accessor_protector);
                    });
                }
                let receiver = scope.raw(receiver);
                if !setter {
                    scope.with_turn_parts(|interp, _| {
                        assert_eq!(
                            crate::array::can_fast_fill_dense_range(
                                receiver.as_array().unwrap(),
                                interp.gc_heap(),
                                0,
                                CHILDREN + 1
                            ),
                            !interp.active_realm_is_extra,
                            "default realm takes dense append; source-realm override takes generic Set"
                        );
                    });
                }
                let mut arguments: Vec<Value> =
                    children.iter().map(|child| scope.raw(*child)).collect();
                let offsets = arguments
                    .iter()
                    .map(|value| value.as_object().unwrap().offset())
                    .collect();
                arguments.push(arguments[0]);
                Ok::<_, NativeError>((
                    receiver,
                    arguments,
                    receiver.as_array().unwrap().offset(),
                    offsets,
                ))
            })
        },
    )
    .expect("young native push inputs")
}

fn assert_contents(
    interp: &mut Interpreter,
    receiver: Value,
    original_children: &[u32],
    setter: bool,
    extra_realm: bool,
) {
    let array = receiver.as_array().expect("push receiver");
    assert_eq!(crate::array::len(array, interp.gc_heap()), CHILDREN + 1);
    let prototype = crate::array::prototype_override(array, interp.gc_heap());
    if extra_realm {
        assert_eq!(
            prototype,
            Some(interp.constructor_prototype_value("Array").unwrap())
        );
    } else {
        assert_eq!(prototype, None);
    }
    let first = if setter {
        assert_eq!(
            crate::array::get_named_property(array, interp.gc_heap(), "setterCalls"),
            Some(Value::number_i32(1))
        );
        crate::array::get_named_property(array, interp.gc_heap(), "seen")
            .expect("setter stored its argument")
    } else {
        crate::array::get(array, interp.gc_heap(), 0)
    };
    assert_eq!(
        crate::array::get(array, interp.gc_heap(), CHILDREN),
        first,
        "first and final argument remain aliases"
    );
    for (index, &old_offset) in original_children.iter().enumerate() {
        let child = if index == 0 {
            first
        } else {
            crate::array::get(array, interp.gc_heap(), index)
        };
        let object = child.as_object().expect("exact child type");
        assert_ne!(
            object.offset(),
            old_offset,
            "child {index} really moved inside push"
        );
        assert_eq!(
            crate::object::get(object, interp.gc_heap(), "marker"),
            Some(Value::number_i32(731 + index as i32))
        );
    }
}

fn moving_push_case(
    interp: &mut Interpreter,
    stride: u32,
    extra_realm: bool,
    setter: bool,
) -> Result<(), VmError> {
    let context = interp
        .link_module(
            crate::test_support::minimal_bytecode_module("scoped-native-push.js"),
            crate::source_registry::SourceRegistry::default(),
        )
        .expect("verifier-valid scoped native push fixture");
    interp.gc_heap_mut().set_gc_stress(0, false);
    let (receiver, arguments, old_receiver, original_children) =
        prepare_inputs(interp, &context, setter);
    interp.gc_heap_mut().set_gc_stress(stride, false);
    let before_prime = interp.gc_heap().gc_cycle_counts().0;
    // Prime the genuine eligible-allocation counter without collecting. The
    // first slab reservation (or setter allocation) then collects inside push.
    for _ in 1..stride {
        interp.gc_heap_mut().alloc(StressPrime { _word: 0 })?;
    }
    assert_eq!(interp.gc_heap().gc_cycle_counts().0, before_prime);
    let receiver = NativeCtx::with_host_context(
        interp,
        NativeCallInfo::call(receiver),
        Some(&context),
        |ctx| {
            let before = ctx.with_turn_parts(|interp, _| interp.gc_heap_mut().gc_stats().clone());
            let result = native_push(ctx, &arguments).expect("native scoped push");
            let after = ctx.with_turn_parts(|interp, _| interp.gc_heap_mut().gc_stats().clone());
            assert_eq!(result.as_f64(), Some((CHILDREN + 1) as f64));
            assert!(
                after.minor_gc_cycles > before.minor_gc_cycles,
                "stride {stride} must collect inside native push"
            );
            assert!(after.minor_root_slots_scanned > before.minor_root_slots_scanned);
            assert!(
                after.minor_slot_updates >= before.minor_slot_updates + CHILDREN as u64 + 1,
                "receiver and every pending child root must be rewritten"
            );
            *ctx.this_value()
        },
    );
    assert_ne!(
        receiver.as_array().unwrap().offset(),
        old_receiver,
        "push evacuated the receiver"
    );
    assert_contents(interp, receiver, &original_children, setter, extra_realm);
    // The production call and its pending argument handles have ended. Keep
    // only the receiver; its stored child edges must survive another scavenge.
    interp.with_handle_scope(|interp, scope| {
        let receiver = interp.scoped_value(scope, receiver);
        interp.gc_heap_mut().set_gc_stress(0, false);
        interp.collect_minor_tracing_runtime_roots();
        let receiver = interp.escape_scoped(receiver);
        assert_contents(interp, receiver, &original_children, setter, extra_realm);
    });
    Ok(())
}

fn all_strides(extra_realm: bool, setter: bool) {
    for stride in 1..=16 {
        let mut interp = Interpreter::new().expect("fixture interpreter bootstrap");
        interp.gc_heap_mut().set_gc_stress(0, false);
        if extra_realm {
            let realm = interp.create_host_realm().expect("additional realm");
            interp
                .with_host_realm(realm, |interp| {
                    moving_push_case(interp, stride, true, setter)
                })
                .expect("additional realm push");
        } else {
            moving_push_case(&mut interp, stride, false, setter).expect("default realm push");
        }
    }
}

#[test]
fn native_push_roots_all_arguments_through_default_realm_slab_growth() {
    all_strides(false, false);
}

#[test]
fn native_push_roots_all_arguments_through_additional_realm_generic_growth() {
    all_strides(true, false);
}

#[test]
fn native_push_rereads_receiver_and_pending_arguments_after_collecting_setter() {
    all_strides(false, true);
    all_strides(true, true);
}
