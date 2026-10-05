//! Persistent property banks through copying, relocation and layout changes.
//!
//! # Contents
//! - Grown suffix slabs independently own copied young-child edges.
//! - Capacity 0/4/64 objects preserve values and descriptors across deletion,
//!   prototype replacement, sealing and freezing with rooted fallible owners.
//!
//! # Invariants
//! - Moving-GC assertions inspect rewritten child offsets and payloads.
//! - Object storage is inspected only after an allocation or collection ends.
//! - No interior field address survives an allocating operation.
//!
//! # See also
//! - [`super::field_location`] owns bank-relative geometry.
//! - [`super::reserve_slot_capacity`] publishes initialized suffix storage.

use super::*;
use otter_gc::{EmptyRoots, HandleScope, SafeTraceable};

struct Child {
    marker: u64,
}

impl SafeTraceable for Child {
    const TYPE_TAG: u8 = 0xe8;

    fn trace_slots_safe(&mut self, _visitor: &mut SlotVisitor<'_>) {}
}

fn capacity_root(heap: &mut GcHeap, capacity: usize) -> ShapeHandle {
    let head = fixture_root_shape(heap).expect("fixture root");
    if let Some(root) = shape_body::root_for_layout(head, capacity, ShapeState::ORDINARY) {
        return root;
    }
    let root = shape_body::alloc_root_shape_body_with_roots(
        heap,
        shape_body::ShapePrototype::Null,
        capacity,
        head,
        ShapeState::ORDINARY,
        &mut |_| {},
    )
    .expect("capacity root");
    shape_body::set_null_root(heap, root);
    root
}

fn seeded_object(heap: &mut GcHeap, capacity: usize, fields: usize) -> JsObject {
    let root = capacity_root(heap, capacity);
    let mut object = alloc_object_body_old(heap, empty_object_body(root)).expect("old owner");
    for index in 0..fields {
        assert!(
            define_own_property_in_place(
                &mut object,
                heap,
                &format!("p{index}"),
                PropertyDescriptor::data(Value::number_i32(index as i32 + 10), true, true, true)
            )
            .expect("fixture property allocation")
        );
    }
    object
}

#[test]
fn grown_suffix_owns_copied_children_before_publication() {
    for capacity in [0, 4, 64] {
        let mut heap = GcHeap::new().expect("heap");
        // This fixture needs a definitely young child at its explicit minor
        // collection; production stress is exercised by the other proofs.
        heap.set_gc_stress(0, false);
        // SAFETY: the heap outlives the scope and every handle in it.
        let scope = unsafe { HandleScope::from_ptr(heap.handle_stack_ptr()) };
        let fields = capacity.max(4) + 5;
        let owner = scope.local(seeded_object(&mut heap, capacity, fields));
        let old_slab = scope.local(heap.read_payload(owner.get(), |body| body.slab));
        let overflow_slot = capacity;
        let original_child;
        {
            // SAFETY: child scope ends before the outer scope and the heap.
            let children = unsafe { HandleScope::from_ptr(heap.handle_stack_ptr()) };
            let child = children.local(heap.alloc(Child { marker: 317 }).expect("young child"));
            original_child = child.get().offset();
            // SAFETY: the new child is live in this scope.
            assert!(unsafe { (*child.get().as_header_ptr()).is_young() });
            let mut object = owner.get();
            assert!(
                ordinary_set_data_property(
                    &mut object,
                    &mut heap,
                    &format!("p{overflow_slot}"),
                    Value::from_other_gc(child.get().raw())
                )
                .expect("fixture assignment allocation")
            );
            let current_capacity = heap.read_payload(object, ObjectBody::slab_capacity);
            reserve_slot_capacity(&mut object, &mut heap, current_capacity + 1, &mut [])
                .expect("suffix growth");
            assert_eq!(object, owner.get());
        }
        let grown = scope.local(heap.read_payload(owner.get(), |body| body.slab));
        assert_ne!(grown.get(), old_slab.get());
        // Keep the grown slab rooted independently and put the still-valid
        // original suffix back on its owner. The owner's remembered scan can
        // now rewrite only the old copy: the grown slab must own its edges.
        heap.with_payload(owner.get(), |body| {
            body.slab = old_slab.get();
            body.debug_verify_field_layout();
        });
        let before = heap.gc_cycle_counts().0;
        heap.collect_minor(EmptyRoots).expect("real child move");
        assert_eq!(heap.gc_cycle_counts().0, before + 1);
        let copied = heap.read_payload(grown.get(), |body| {
            // SAFETY: growth copied the owner's first live suffix word.
            unsafe { *body.words_ptr() }
        });
        let stored =
            get_own(owner.get(), &heap, &format!("p{overflow_slot}")).expect("source child");
        assert_eq!(copied, stored, "each slab rewrites its own copied word");
        let child = copied
            .as_raw_gc()
            .and_then(|raw| raw.checked_cast::<Child>())
            .expect("live copied child");
        assert_ne!(child.offset(), original_child, "actual relocation");
        assert_eq!(heap.read_payload(child, |body| body.marker), 317);
        heap.collect_minor(EmptyRoots).expect("second child move");
        heap.collect_full(&mut |_| {}).expect("full slab tracing");
        let copied = heap.read_payload(grown.get(), |body| unsafe { *body.words_ptr() });
        let child = copied
            .as_raw_gc()
            .and_then(|raw| raw.checked_cast::<Child>())
            .expect("copied child after full GC");
        assert_eq!(heap.read_payload(child, |body| body.marker), 317);
    }
}

#[test]
fn persistent_banks_rewrite_distinct_children_through_growth_and_delete() {
    for capacity in [0, 4, 64] {
        let mut heap = GcHeap::new().expect("heap");
        heap.set_gc_stress(0, false);
        // SAFETY: the heap outlives this scope and its ordinary owner.
        let scope = unsafe { HandleScope::from_ptr(heap.handle_stack_ptr()) };
        let fields = capacity.max(4) + 2;
        let owner = scope.local(seeded_object(&mut heap, capacity, fields));
        let suffix_key = format!("p{}", capacity.max(4));
        let initial_offsets;
        {
            // SAFETY: both child handles end before the outer scope.
            let children = unsafe { HandleScope::from_ptr(heap.handle_stack_ptr()) };
            let inline = children.local(heap.alloc(Child { marker: 719 }).expect("prefix child"));
            let suffix = children.local(heap.alloc(Child { marker: 997 }).expect("suffix child"));
            initial_offsets = [inline.get().offset(), suffix.get().offset()];
            let mut object = owner.get();
            assert!(
                ordinary_set_data_property(
                    &mut object,
                    &mut heap,
                    "p0",
                    Value::from_other_gc(inline.get().raw())
                )
                .expect("fixture assignment allocation")
            );
            assert!(
                ordinary_set_data_property(
                    &mut object,
                    &mut heap,
                    &suffix_key,
                    Value::from_other_gc(suffix.get().raw())
                )
                .expect("fixture assignment allocation")
            );
        }
        heap.collect_minor(EmptyRoots)
            .expect("move both bank children");
        for ((key, marker), original) in [("p0", 719), (suffix_key.as_str(), 997)]
            .into_iter()
            .zip(initial_offsets)
        {
            let raw = get_own(owner.get(), &heap, key)
                .unwrap()
                .as_raw_gc()
                .unwrap();
            let child = raw.checked_cast::<Child>().expect("moved bank child");
            assert_ne!(child.offset(), original, "actual {key} relocation");
            assert_eq!(heap.read_payload(child, |body| body.marker), marker);
        }
        // The first collection may have promoted those survivors. Replace
        // both fields with new nursery children so the next minor proves
        // relocation through the copied suffix and shifted bank geometry.
        let fresh_offsets;
        {
            // SAFETY: these roots close before the outer owner scope.
            let children = unsafe { HandleScope::from_ptr(heap.handle_stack_ptr()) };
            let inline = children.local(heap.alloc(Child { marker: 719 }).expect("fresh prefix"));
            let suffix = children.local(heap.alloc(Child { marker: 997 }).expect("fresh suffix"));
            for child in [inline.get(), suffix.get()] {
                // SAFETY: both child handles are rooted in this scope.
                assert!(unsafe { (*child.as_header_ptr()).is_young() });
            }
            fresh_offsets = [inline.get().offset(), suffix.get().offset()];
            let mut object = owner.get();
            assert!(
                ordinary_set_data_property(
                    &mut object,
                    &mut heap,
                    "p0",
                    Value::from_other_gc(inline.get().raw())
                )
                .expect("fixture assignment allocation")
            );
            assert!(
                ordinary_set_data_property(
                    &mut object,
                    &mut heap,
                    &suffix_key,
                    Value::from_other_gc(suffix.get().raw())
                )
                .expect("fixture assignment allocation")
            );
        }
        let mut object = owner.get();
        let current_capacity = heap.read_payload(object, ObjectBody::slab_capacity);
        reserve_slot_capacity(&mut object, &mut heap, current_capacity + 1, &mut [])
            .expect("copy rewritten suffix");
        assert!(delete(&mut object, &mut heap, "p1").expect("delete fixture"));
        heap.collect_minor(EmptyRoots)
            .expect("move after bank shift");
        for ((key, marker), previous) in [("p0", 719), (suffix_key.as_str(), 997)]
            .into_iter()
            .zip(fresh_offsets)
        {
            let raw = get_own(owner.get(), &heap, key)
                .unwrap()
                .as_raw_gc()
                .unwrap();
            let child = raw.checked_cast::<Child>().expect("shifted bank child");
            assert_ne!(child.offset(), previous, "second actual {key} relocation");
            assert_eq!(heap.read_payload(child, |body| body.marker), marker);
        }
        heap.read_payload(owner.get(), |body| {
            body.debug_verify_field_layout();
            assert_eq!(body.inline_capacity(), capacity);
        });
    }
}

#[test]
fn dictionary_cross_bank_delete_and_integrity_keep_capacity_and_descriptors() {
    for capacity in [0, 4, 64] {
        let mut heap = GcHeap::new().expect("heap");
        // SAFETY: the heap outlives the scope and its rooted objects.
        let scope = unsafe { HandleScope::from_ptr(heap.handle_stack_ptr()) };
        let fields = capacity.max(4) + 2;
        let owner = scope.local(seeded_object(&mut heap, capacity, fields));
        let prototype = scope.local(alloc_object_old_for_fixture(&mut heap).expect("prototype"));
        let boundary = capacity.max(1);
        let descriptor_key = format!("p{boundary}");
        assert!(
            define_own_property(
                owner.get(),
                &mut heap,
                &descriptor_key,
                PropertyDescriptor::data(Value::number_i32(719), false, false, true),
            )
            .expect("descriptor fixture allocation")
        );
        // Deletion below the boundary shifts the first suffix field into the
        // final inline word when the prefix is nonempty.
        assert!(delete(&mut owner.get(), &mut heap, "p0").expect("delete fixture"));
        assert!(is_dictionary(owner.get(), &heap));
        assert!(
            set_prototype_value(
                &mut owner.get(),
                &mut heap,
                Some(Value::object(prototype.get())),
            )
            .expect("set_prototype_value fixture")
        );
        heap.read_payload(owner.get(), |body| {
            body.debug_verify_field_layout();
            assert_eq!(body.inline_capacity(), capacity);
            if capacity != 0 {
                assert_eq!(
                    body.location_for_slot(capacity - 1),
                    FieldLocation::inline(capacity as u32 - 1)
                );
                assert_eq!(body.slot_word(capacity - 1), Value::number_i32(719));
            }
        });
        let descriptor =
            get_own_descriptor(owner.get(), &heap, &descriptor_key).expect("shifted descriptor");
        assert!(!descriptor.writable());
        assert!(!descriptor.enumerable());
        assert!(descriptor.configurable());
        assert!(
            matches!(descriptor.kind, DescriptorKind::Data { value } if value == Value::number_i32(719))
        );
        assert_eq!(get_own(owner.get(), &heap, "p0"), None);
        for index in 1..fields {
            let expected = if index == boundary {
                719
            } else {
                index as i32 + 10
            };
            assert_eq!(
                get_own(owner.get(), &heap, &format!("p{index}")),
                Some(Value::number_i32(expected))
            );
        }
        seal(&mut owner.get(), &mut heap).expect("seal fixture");
        assert!(!delete(&mut owner.get(), &mut heap, &descriptor_key).expect("delete fixture"));
        freeze(&mut owner.get(), &mut heap).expect("freeze fixture");
        assert!(!is_extensible(owner.get(), &heap));
        let descriptor =
            get_own_descriptor(owner.get(), &heap, &descriptor_key).expect("frozen descriptor");
        assert!(!descriptor.writable() && !descriptor.enumerable() && !descriptor.configurable());
        heap.collect_full(&mut |_| {})
            .expect("integrity transition GC");
        heap.read_payload(owner.get(), |body| {
            body.debug_verify_field_layout();
            assert_eq!(body.inline_capacity(), capacity);
        });
        assert_eq!(
            get_own(owner.get(), &heap, &descriptor_key),
            Some(Value::number_i32(719))
        );
    }
}
