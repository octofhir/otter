//! Cross-owner proofs for the heap's one shape interner.
//!
//! # Contents
//! - Facade and heap-only state operations reuse actual layout identities.
//! - Full and split collection prune weak entries without interpreter roots.
//! - Restored layouts and canonical keys remain reusable after relocation.
//! - Collecting prototype publication permits recursive state preparation.
//!
//! # Invariants
//! - Receivers and moving prototypes live in production handle scopes.
//! - A released shape is inspected only through its weak identity lookup.
//! - Heap-only cases collect without an interpreter root source.
//!
//! # See also
//! - `super` exposes transition observation over the heap-owned interner.

use crate::Value;
use crate::isolate_names::IsolateNames;
use crate::object::{self, PropertyFlags, ShapeHandle, ShapeState, shape_body};
use otter_gc::{GcHeap, HandleScope};

use super::ShapeRuntime;

fn fixture() -> (GcHeap, ShapeRuntime) {
    let mut heap = GcHeap::new().expect("heap");
    heap.set_gc_stress(0, false);
    object::register_gc_traceables(&mut heap);
    let runtime = ShapeRuntime::new(&mut heap).expect("shape owner");
    (heap, runtime)
}

fn child(
    runtime: &mut ShapeRuntime,
    heap: &mut GcHeap,
    parent: ShapeHandle,
    key: &str,
) -> ShapeHandle {
    runtime
        .child_with_roots(
            heap,
            parent,
            key,
            PropertyFlags::data_default(),
            false,
            &mut |_| {},
        )
        .expect("interned child")
}

#[test]
fn shape_interner_facade_and_heap_only_state_share_exact_children() {
    let (mut heap, mut runtime) = fixture();
    let root = shape_body::null_root(&heap);
    let source = child(&mut runtime, &mut heap, root, "value");
    // SAFETY: every local ends before the heap and no handle escapes this test.
    let scope = unsafe { HandleScope::from_ptr(heap.handle_stack_ptr()) };
    let first = scope.local(
        object::alloc_object_with_shape_roots(&mut heap, source, &mut |_| {}).expect("first"),
    );
    let second = scope.local(
        object::alloc_object_with_shape_roots(&mut heap, source, &mut |_| {}).expect("second"),
    );
    for (receiver, value) in [(&first, 317), (&second, 719)] {
        let mut current = receiver.get();
        assert!(
            object::ordinary_set_data_property(
                &mut current,
                &mut heap,
                "value",
                Value::number_i32(value)
            )
            .expect("existing field")
        );
        object::prevent_extensions(&mut current, &mut heap).expect("heap-only state mutation");
        assert_eq!(current, receiver.get());
        assert!(!object::is_extensible(current, &heap));
        assert_eq!(
            object::get_own(current, &heap, "value"),
            Some(Value::number_i32(value))
        );
    }
    let final_shape = object::shape(first.get(), &heap);
    assert_eq!(final_shape, object::shape(second.get(), &heap));
    assert_ne!(final_shape, source);
    let state_root = shape_body::parent_of(final_shape);
    assert_eq!(
        shape_body::state_of(state_root),
        ShapeState::ORDINARY.with_extensible(false)
    );
    assert_eq!(
        child(&mut runtime, &mut heap, state_root, "value"),
        final_shape,
        "a heap-only replay must populate the same interner the facade reads"
    );
}

#[test]
fn shape_interner_heap_roots_and_weak_sweep_cover_full_and_split_collection() {
    let (mut heap, mut runtime) = fixture();
    let root = shape_body::null_root(&heap);
    let kept = child(&mut runtime, &mut heap, root, "kept");
    let kept_id = shape_body::id_of(kept);
    let discarded = child(&mut runtime, &mut heap, root, "discardedFull");
    let discarded_id = shape_body::id_of(discarded);
    // SAFETY: this old shape is rooted through both real collection services.
    let scope = unsafe { HandleScope::from_ptr(heap.handle_stack_ptr()) };
    let kept = scope.local(kept);

    heap.collect_full(&mut |_| {})
        .expect("pinned full collection");
    assert_eq!(runtime.handle_for_id(&heap, discarded_id), Some(discarded));
    runtime.unpin_turn_shapes(&heap);
    heap.collect_full(&mut |_| {})
        .expect("weak full collection");
    assert!(runtime.handle_for_id(&heap, discarded_id).is_none());
    assert_eq!(runtime.handle_for_id(&heap, kept_id), Some(kept.get()));
    assert_eq!(shape_body::null_root(&heap), root);

    let discarded = child(&mut runtime, &mut heap, root, "discardedSplit");
    let discarded_id = shape_body::id_of(discarded);
    runtime.unpin_turn_shapes(&heap);
    heap.start_incremental_mark_phase(&mut |_| {})
        .expect("split start");
    let _ = heap.incremental_mark_step(1);
    heap.finish_incremental_mark_phase(&mut |_| {})
        .expect("split finish");
    // The physical sweep must prune the interner even when a split driver
    // does not run the optional JavaScript weak-reference service.
    heap.sweep_phase();
    assert!(runtime.handle_for_id(&heap, discarded_id).is_none());
    assert_eq!(runtime.handle_for_id(&heap, kept_id), Some(kept.get()));
    assert!(
        runtime
            .child_if_cached(
                &heap,
                root,
                "discardedSplit",
                PropertyFlags::data_default(),
                false
            )
            .is_none()
    );
    assert!(
        IsolateNames::of(&heap)
            .key_string(crate::isolate_names::names(&heap).lookup("discardedSplit"))
            .is_some()
    );
}

#[test]
fn shape_interner_restored_children_and_canonical_keys_reuse_image_identity() {
    let mut donor = crate::Interpreter::new().expect("donor");
    donor.gc_heap.set_tenure_all(true);
    let root = donor.null_prototype_root();
    let source = child(
        &mut donor.shape_runtime,
        &mut donor.gc_heap,
        root,
        "restoredField",
    );
    let receiver = object::alloc_object_with_shape_roots(&mut donor.gc_heap, source, &mut |_| {})
        .expect("image receiver");
    let mut global = donor.global_this;
    donor
        .create_data_property(
            &mut global,
            "shapeInternerReceiver",
            Value::object(receiver),
        )
        .expect("hold image receiver");
    let image = donor.capture_isolate_snapshot().expect("capture image");
    let mut restored = crate::Interpreter::from_isolate_snapshot(
        &image,
        &otter_resource::ResourceAccount::default(),
    )
    .expect("restore image");
    let receiver = object::get_own(
        restored.global_this,
        &restored.gc_heap,
        "shapeInternerReceiver",
    )
    .and_then(|value| value.as_object())
    .expect("restored receiver");
    let retained = object::shape(receiver, &restored.gc_heap);
    let root = restored.null_prototype_root();
    assert_eq!(shape_body::parent_of(retained), root);
    let key = restored
        .gc_heap
        .read_payload(retained, shape_body::ShapeBody::transition_key);
    let keys_before = IsolateNames::of(&restored.gc_heap).key_string_count();
    assert_eq!(
        child(
            &mut restored.shape_runtime,
            &mut restored.gc_heap,
            root,
            "restoredField"
        ),
        retained,
        "the restored transition must select the image's existing child"
    );
    let other_root = restored
        .shape_runtime
        .new_root(
            &mut restored.gc_heap,
            shape_body::ShapePrototype::Null,
            0,
            ShapeHandle::null(),
            ShapeState::ORDINARY,
            &mut |_| {},
        )
        .expect("distinct capacity root");
    let other = child(
        &mut restored.shape_runtime,
        &mut restored.gc_heap,
        other_root,
        "restoredField",
    );
    assert_eq!(
        restored
            .gc_heap
            .read_payload(other, shape_body::ShapeBody::transition_key),
        key
    );
    assert_eq!(
        IsolateNames::of(&restored.gc_heap).key_string_count(),
        keys_before
    );
}

#[test]
fn shape_interner_collecting_young_prototype_publication_can_reenter_state_owner() {
    struct Reclaimable([u64; 16384]);
    impl otter_gc::SafeTraceable for Reclaimable {
        const TYPE_TAG: u8 = 0xe8;
        fn trace_slots_safe(&mut self, _: &mut otter_gc::raw::SlotVisitor<'_>) {
            let _ = self.0[0];
        }
    }
    let mut heap = GcHeap::with_max_heap_bytes(4 * 1024 * 1024).expect("capped heap");
    heap.set_gc_stress(0, false);
    object::register_gc_traceables(&mut heap);
    let mut runtime = ShapeRuntime::new(&mut heap).expect("shape owner");
    let root = shape_body::null_root(&heap);
    let prototype_shape = child(&mut runtime, &mut heap, root, "prototypeField");
    // SAFETY: the handle arena and moving prototype live through publication.
    let scope = unsafe { HandleScope::from_ptr(heap.handle_stack_ptr()) };
    let prototype = scope.local(
        object::alloc_object_with_shape_roots(&mut heap, prototype_shape, &mut |_| {})
            .expect("young prototype"),
    );
    // Old shape allocation does not enter the nursery stress trigger. Fund
    // this exact root preparation with unreachable old payload, then consume
    // the remaining cap without collecting: its first old allocation must GC.
    let _ = heap
        .alloc_old(Reclaimable([0; 16384]))
        .expect("reclaimable root-preparation budget");
    let reserved = heap.max_heap_bytes() - heap.tracked_bytes();
    heap.reserve_bytes_no_collect(reserved)
        .expect("fill cap without collection");
    let before_offset = prototype.get().offset();
    let before_cycles = heap.gc_cycle_counts();
    let instance_root = object::heap_instance_root(
        object::ObjectPrototype::Object(prototype.get()),
        &mut heap,
        4,
        ShapeState::ORDINARY,
        &mut |_| {},
    )
    .expect("collecting and recursive prototype publication");
    heap.release_bytes(reserved);
    assert!(
        heap.gc_cycle_counts().1 > before_cycles.1,
        "root preparation itself must full-collect"
    );
    assert_ne!(
        prototype.get().offset(),
        before_offset,
        "the young prototype actually moved"
    );
    assert_eq!(
        shape_body::prototype_of(instance_root),
        shape_body::ShapePrototype::Object(prototype.get())
    );
    assert!(object::state(prototype.get(), &heap).is_prototype());
    let repeated = object::heap_instance_root(
        object::ObjectPrototype::Object(prototype.get()),
        &mut heap,
        4,
        ShapeState::ORDINARY,
        &mut |_| {},
    )
    .expect("cached prototype root");
    assert_eq!(repeated, instance_root);
}
