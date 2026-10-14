//! Field-summary publication through births, mutations and relocation.
//!
//! # Contents
//! - Exact Int32 values irreversibly widen into canonical packed unions.
//! - Low-byte comparisons identify unchanged words within monotone histories.
//! - Defining shapes share summaries across append descendants.
//! - Independent lineages and native scoped writes observe live values.
//!
//! # Invariants
//! - Allocating fixtures keep every object and shape in a handle scope.
//! - Relocation checks compare actual rewritten offsets and retained identities.
//! - No interior field address survives allocation or collection.
//!
//! # See also
//! - `super` owns the packed contract and non-collecting native observer.

use super::*;
use crate::object::{self, PropertyFlags, ShapeState};
use crate::property_atom::AtomId;
use crate::string::alloc_flat_string_body_with_roots;
use otter_gc::{EmptyRoots, GcHeap, HandleScope};

fn capacity_root(heap: &mut GcHeap, capacity: usize) -> ShapeHandle {
    let previous = shape_body::null_root(heap);
    let root = shape_body::alloc_root_shape_body_with_roots(
        heap,
        shape_body::ShapePrototype::Null,
        capacity,
        previous,
        ShapeState::ORDINARY,
        &mut |_| {},
    )
    .expect("capacity root");
    shape_body::set_null_root(heap, root);
    root
}

fn child(heap: &mut GcHeap, parent: ShapeHandle, index: u32, accessor: bool) -> ShapeHandle {
    // SAFETY: this scope and all fixture handles end before their heap.
    let scope = unsafe { HandleScope::from_ptr(heap.handle_stack_ptr()) };
    let parent = scope.local(parent);
    let key: Vec<u16> = format!("field{index}").encode_utf16().collect();
    let key = alloc_flat_string_body_with_roots(heap, &key, &mut |_| {}).expect("key");
    let key = scope.local(key);
    shape_body::alloc_child_shape_body_with_roots(
        heap,
        parent.get(),
        key.get(),
        AtomId::from_global(index + 1000),
        PropertyFlags::data_default(),
        accessor,
        &mut |_| {},
    )
    .expect("child shape")
}

fn sample(shape: ShapeHandle) -> u64 {
    shape_id_sample(shape_body::id_of(shape).raw())
}

#[test]
fn packed_unions_are_canonical_and_saturate() {
    let first = shape_id_sample(7);
    let second = shape_id_sample(3);
    let expected = NULL
        | UNDEFINED
        | (2 << SHAPE_COUNT_SHIFT)
        | (3 << FIRST_SHAPE_SHIFT)
        | (7 << SECOND_SHAPE_SHIFT);
    assert_eq!(merge(first | NULL, second | UNDEFINED), expected);
    assert_eq!(merge(second | UNDEFINED, first | NULL), expected);
    assert_eq!(merge(expected, first), expected);
    assert_eq!(merge(expected, shape_id_sample(11)), TOP);
    assert_eq!(shape_id_sample(0), TOP);
    assert_eq!(shape_id_sample(SHAPE_MASK + 1), TOP);
    assert_eq!(
        shape_id_sample(SHAPE_MASK),
        (1 << SHAPE_COUNT_SHIFT) | (SHAPE_MASK << FIRST_SHAPE_SHIFT)
    );
    assert_eq!(merge(expected, TOP), TOP);
    assert_eq!(merge(TOP, 0), TOP);
    assert_eq!(merge(NULL, 1 << 5), TOP);
    assert_eq!(merge(expected, INT32), expected | INT32);
    assert_eq!(
        merge(INT32 | NULL, INT32 | UNDEFINED),
        INT32 | NULL | UNDEFINED
    );
    assert!(is_canonical(expected));
}

#[test]
fn malformed_counts_and_identity_order_saturate() {
    let one = 1 << SHAPE_COUNT_SHIFT;
    let two = 2 << SHAPE_COUNT_SHIFT;
    for malformed in [
        one,
        3 << FIRST_SHAPE_SHIFT,
        two | (3 << FIRST_SHAPE_SHIFT),
        two | (3 << SECOND_SHAPE_SHIFT),
        SHAPE_COUNT_MASK | (3 << FIRST_SHAPE_SHIFT) | (7 << SECOND_SHAPE_SHIFT),
        two | (7 << FIRST_SHAPE_SHIFT) | (3 << SECOND_SHAPE_SHIFT),
        two | (3 << FIRST_SHAPE_SHIFT) | (3 << SECOND_SHAPE_SHIFT),
        TOP | NULL,
        1 << 5,
        CONST_INT32 | NULL,
        int32_sample(3) | NULL,
        int32_sample(3) | (1 << 40),
    ] {
        assert!(!is_canonical(malformed));
        assert_eq!(merge(malformed, NULL), TOP);
        assert_eq!(merge(UNDEFINED, malformed), TOP);
    }
    for canonical in [
        0,
        NULL,
        UNDEFINED,
        NULL | UNDEFINED,
        INT32,
        INT32 | NULL | UNDEFINED,
        int32_sample(0),
        int32_sample(i32::MIN),
        int32_sample(i32::MAX),
        shape_id_sample(3),
        TOP,
    ] {
        assert!(is_canonical(canonical));
    }
}

#[test]
fn low_byte_equality_identifies_the_full_word_within_one_history() {
    let samples = [
        0,
        NULL,
        UNDEFINED,
        INT32,
        int32_sample(0),
        int32_sample(-7),
        int32_sample(i32::MIN),
        int32_sample(i32::MAX),
        shape_id_sample(7),
        shape_id_sample(3),
        shape_id_sample(11),
        TOP,
    ];
    let mut states = vec![0];
    let mut cursor = 0;
    while cursor < states.len() {
        let state = states[cursor];
        cursor += 1;
        for sample in samples {
            let next = merge(state, sample);
            assert!(is_canonical(next));
            if !states.contains(&next) {
                states.push(next);
            }
        }
    }
    assert_eq!(states.len(), 61);
    for captured in states {
        let mut history = vec![captured];
        let mut cursor = 0;
        while cursor < history.len() {
            let state = history[cursor];
            cursor += 1;
            for sample in samples {
                let next = merge(state, sample);
                if next as u8 == captured as u8 {
                    assert_eq!(
                        next, captured,
                        "a low-byte guard preserves exact identities"
                    );
                }
                if !history.contains(&next) {
                    history.push(next);
                }
            }
        }
    }
}

#[test]
fn constant_int32_values_widen_once_without_confusing_shape_payloads() {
    for value in [0, 7, -7, i32::MIN, i32::MAX] {
        let exact = int32_sample(value);
        assert!(is_canonical(exact));
        assert_eq!(int32_constant(exact), Some(value));
        assert!(is_int32_only(exact));
        assert_eq!(merge(0, exact), exact);
        assert_eq!(merge(exact, exact), exact);
        assert_eq!(merge(exact, int32_sample(value.wrapping_add(1))), INT32);
        assert_eq!(merge(INT32, exact), INT32);
        assert_eq!(merge(exact, NULL), INT32 | NULL);
        assert_eq!(merge(UNDEFINED, exact), INT32 | UNDEFINED);
        assert_eq!(merge(exact, shape_id_sample(7)), INT32 | shape_id_sample(7));
        assert_eq!(merge(shape_id_sample(7), exact), INT32 | shape_id_sample(7));
        assert_ne!(exact as u8, INT32 as u8);
    }
    assert_eq!(int32_constant(INT32), None);
    assert_eq!(int32_constant(shape_id_sample(7)), None);
    assert!(!is_int32_only(INT32 | NULL));
}

#[test]
fn append_descendants_share_only_defining_data_cells() {
    let mut heap = object::fixture_heap();
    // SAFETY: all handles remain inside this heap's fixture scope.
    let scope = unsafe { HandleScope::from_ptr(heap.handle_stack_ptr()) };
    let root = scope.local(object::fixture_root_shape(&mut heap).unwrap());
    let first = scope.local(child(&mut heap, root.get(), 0, false));
    let second = scope.local(child(&mut heap, first.get(), 1, false));
    let accessor = scope.local(child(&mut heap, second.get(), 2, true));
    assert_eq!(owner(second.get(), 0), Some(first.get()));
    assert_eq!(owner(accessor.get(), 1), Some(second.get()));
    assert_eq!(owner(accessor.get(), 2), None);
    assert_eq!(owner(accessor.get(), 3), None);
    assert_eq!(owner(root.get(), 0), None);
    assert_eq!(owner(shape_body::dictionary_of(root.get()), 0), None);
    assert_eq!(owner(ShapeHandle::null(), 0), None);
    observe_owner(first.get(), Value::NULL);
    observe_owner(first.get(), Value::UNDEFINED);
    assert_eq!(snapshot(owner(second.get(), 0).unwrap()), NULL | UNDEFINED);
    assert_eq!(observer_owner(second.get(), 0), Some(first.get()));
    observe_owner(first.get(), Value::number_i32(3));
    assert_eq!(snapshot(first.get()), INT32 | NULL | UNDEFINED);
    assert_eq!(observer_owner(second.get(), 0), Some(first.get()));
    observe_owner(first.get(), Value::number_f64(3.5));
    assert_eq!(snapshot(first.get()), TOP);
    assert_eq!(observer_owner(second.get(), 0), Some(ShapeHandle::null()));
    assert_eq!(observer_owner(second.get(), 99), None);
}

#[test]
fn shaped_births_observe_inline_suffix_and_live_undefined() {
    for capacity in [0, 4] {
        let mut heap = object::fixture_heap();
        heap.set_gc_stress(0, false);
        // SAFETY: fixture births keep their complete source chain rooted.
        let scope = unsafe { HandleScope::from_ptr(heap.handle_stack_ptr()) };
        let root = scope.local(capacity_root(&mut heap, capacity));
        let target = scope.local(
            object::alloc_object_with_roots(&mut heap, root.get(), &mut |_| {}).expect("target"),
        );
        let mut layout = root.clone();
        let count = capacity + 2;
        for index in 0..count {
            layout = scope.local(child(&mut heap, layout.get(), index as u32, false));
        }
        let mut values = vec![Value::object(target.get()); count];
        let object = scope.local(
            object::alloc_object_with_shape_and_values_roots(
                &mut heap,
                layout.get(),
                &mut values,
                otter_gc::SitePlacement::Young(0),
                &mut |_| {},
            )
            .expect("complete shaped birth"),
        );
        for (index, value) in values.iter().enumerate() {
            assert_eq!(
                snapshot(owner(layout.get(), index as u32).unwrap()),
                sample(root.get())
            );
            assert_eq!(object::layout_slot(object.get(), &heap, index), *value);
        }
        let _undefined = scope.local(
            object::alloc_object_with_shape_roots(&mut heap, layout.get(), &mut |_| {})
                .expect("undefined shaped birth"),
        );
        for index in 0..count {
            assert_eq!(
                snapshot(owner(layout.get(), index as u32).unwrap()),
                sample(root.get()) | UNDEFINED
            );
        }
    }
}

#[test]
fn publication_observes_self_references_after_the_shape_changes() {
    let mut heap = object::fixture_heap();
    // SAFETY: the rooted receiver and shapes stay live through preparation.
    let scope = unsafe { HandleScope::from_ptr(heap.handle_stack_ptr()) };
    let receiver = scope.local(object::alloc_object_old_for_fixture(&mut heap).unwrap());
    let source = object::shape(receiver.get(), &heap);
    let first = scope.local(child(&mut heap, source, 0, false));
    heap.with_payload(receiver.get(), |body| {
        body.adopt_shape(first.get());
        body.push_slab_word(0, Value::object(receiver.get()));
    });
    assert_eq!(snapshot(first.get()), sample(first.get()));
    assert!(!shape_body::is_stable(source));
    let changed = scope.local(
        object::state_transition::prepare_state_shape(
            &mut heap,
            first.get(),
            ShapeState::ORDINARY.with_extensible(false),
            &mut |_| {},
        )
        .expect("independent state lineage"),
    );
    assert_eq!(snapshot(owner(changed.get(), 0).unwrap()), 0);
    object::state_transition::install_state_shape(receiver.get(), &mut heap, changed.get());
    assert_eq!(
        snapshot(owner(changed.get(), 0).unwrap()),
        sample(changed.get())
    );
    assert!(!shape_body::is_stable(first.get()));
    assert!(shape_body::is_stable(changed.get()));
}

#[test]
fn live_stores_widen_two_shapes_and_alias_changes_retire_target_stability() {
    let mut heap = object::fixture_heap();
    // SAFETY: every receiver, target and shape is held through preparation.
    let scope = unsafe { HandleScope::from_ptr(heap.handle_stack_ptr()) };
    let source = object::fixture_root_shape(&mut heap).unwrap();
    let field = scope.local(child(&mut heap, source, 0, false));
    let receiver = scope
        .local(object::alloc_object_with_shape_roots(&mut heap, field.get(), &mut |_| {}).unwrap());
    let mut shapes = Vec::new();
    let mut targets = Vec::new();
    for capacity in [1, 2, 3] {
        let shape = scope.local(capacity_root(&mut heap, capacity));
        let target = scope
            .local(object::alloc_object_with_roots(&mut heap, shape.get(), &mut |_| {}).unwrap());
        shapes.push(shape);
        targets.push(target);
    }
    object::write_layout_slot(
        receiver.get(),
        &mut heap,
        0,
        Value::object(targets[0].get()),
    );
    object::write_layout_slot(receiver.get(), &mut heap, 0, Value::NULL);
    object::write_layout_slot(
        receiver.get(),
        &mut heap,
        0,
        Value::object(targets[1].get()),
    );
    let observed = merge(
        UNDEFINED | NULL | sample(shapes[0].get()),
        sample(shapes[1].get()),
    );
    assert_eq!(snapshot(field.get()), observed);
    let changed = scope.local(child(&mut heap, shapes[1].get(), 0, false));
    heap.with_payload(targets[1].get(), |body| {
        body.adopt_shape(changed.get());
        body.push_slab_word(0, Value::NULL);
    });
    assert!(!shape_body::is_stable(shapes[1].get()));
    assert_eq!(
        snapshot(field.get()),
        observed,
        "alias mutation does not rewrite history"
    );
    object::write_layout_slot(
        receiver.get(),
        &mut heap,
        0,
        Value::object(targets[2].get()),
    );
    assert_eq!(snapshot(field.get()), TOP);
    object::write_layout_slot(receiver.get(), &mut heap, 0, Value::NULL);
    assert_eq!(snapshot(field.get()), TOP, "widening never narrows");
}

#[test]
fn relocation_rewrites_values_without_rewriting_field_identities() {
    let mut heap = object::fixture_heap();
    heap.set_gc_stress(0, false);
    // SAFETY: receiver, target and shapes remain rooted during collection.
    let scope = unsafe { HandleScope::from_ptr(heap.handle_stack_ptr()) };
    let root = scope.local(object::fixture_root_shape(&mut heap).unwrap());
    let layout = scope.local(child(&mut heap, root.get(), 0, false));
    let receiver = scope.local(
        object::alloc_object_with_shape_roots(&mut heap, layout.get(), &mut |_| {}).unwrap(),
    );
    let target =
        scope.local(object::alloc_object_with_roots(&mut heap, root.get(), &mut |_| {}).unwrap());
    let original = target.get().offset();
    object::write_layout_slot(receiver.get(), &mut heap, 0, Value::object(target.get()));
    let observed = snapshot(layout.get());
    assert_eq!(observed, UNDEFINED | sample(root.get()));
    heap.collect_minor(EmptyRoots).expect("moving collection");
    let value = object::layout_slot(receiver.get(), &heap, 0)
        .as_object()
        .unwrap();
    assert_ne!(value.offset(), original, "actual target move");
    assert_eq!(value, target.get());
    assert_eq!(snapshot(layout.get()), observed);
    heap.collect_full(&mut |_| {}).expect("full tracing");
    assert_eq!(snapshot(layout.get()), observed);
}

#[test]
fn native_observation_rejects_invalid_words_and_widens_without_collection() {
    let mut heap = object::fixture_heap();
    // SAFETY: the defining shape remains rooted during synchronous leaf calls.
    let scope = unsafe { HandleScope::from_ptr(heap.handle_stack_ptr()) };
    let root = object::fixture_root_shape(&mut heap).unwrap();
    let field = scope.local(child(&mut heap, root, 0, false));
    let bits = Value::NULL.to_abi_bits();
    let miss = crate::native_abi::NativeResultPair::miss();
    assert_eq!(
        observe_native(std::ptr::null_mut(), field.get().offset() as u64, bits),
        miss
    );
    for invalid in [0, 1, u32::MAX as u64 + 1] {
        assert_eq!(observe_native(&mut heap, invalid, bits), miss);
    }
    let cycles = heap.gc_cycle_counts();
    assert_eq!(
        observe_native(&mut heap, field.get().offset() as u64, bits),
        crate::native_abi::NativeResultPair::success(Value::UNDEFINED)
    );
    assert_eq!(snapshot(field.get()), NULL);
    assert_eq!(heap.gc_cycle_counts(), cycles);
}

#[test]
fn native_scoped_slot_writes_widen_the_birth_contract() {
    let mut interp = crate::Interpreter::new().expect("fixture interpreter");
    crate::NativeCtx::with_host_context(
        &mut interp,
        crate::NativeCallInfo::default_call(),
        None,
        |ctx| {
            ctx.scope(|mut scope| {
                let layout = scope.object_layout(&["link"]).expect("native layout");
                let receiver = scope.object_of_layout(layout).expect("receiver");
                let target = scope.object().expect("target");
                let receiver_raw = scope.raw(receiver).as_object().unwrap();
                let receiver_shape = scope
                    .with_turn_parts(|interp, _| object::shape(receiver_raw, interp.gc_heap()));
                let field = owner(receiver_shape, 0).unwrap();
                assert_eq!(snapshot(field), UNDEFINED);
                scope
                    .set_slot(receiver, 0, target)
                    .expect("native slot write");
                let target_raw = scope.raw(target).as_object().unwrap();
                let target_shape =
                    scope.with_turn_parts(|interp, _| object::shape(target_raw, interp.gc_heap()));
                assert_eq!(snapshot(field), UNDEFINED | sample(target_shape));
                let null = scope.null();
                scope.set_slot(receiver, 0, null).expect("nullable link");
                assert_eq!(snapshot(field), UNDEFINED | NULL | sample(target_shape));
                let array = scope.array(0).expect("exotic target");
                scope.set_slot(receiver, 0, array).expect("exotic store");
                assert_eq!(snapshot(field), TOP);
                scope
                    .set_slot(receiver, 0, target)
                    .expect("later ordinary store");
                assert_eq!(snapshot(field), TOP);
            });
        },
    );
}
