//! Interpreter observation facade over the heap's canonical hidden-class owner.
//!
//! # Contents
//! - `ShapeRuntime` installs the heap's isolate state and owns the optional observer.
//! - Preparation and lookup delegate to `shape_interner` through the supplied heap.
//!
//! # Invariants
//! - This facade owns no shape table, GC handle or heap backpointer.
//! - The heap's one `IsolateNames` owns all shape identity and lifetime state.
//! - Observation runs after table borrows end and never reenters the interpreter.
//!
//! # See also
//! - `super::shape_interner` owns canonical children, roots and weak maintenance.
//! - `crate::isolate_names` is the heap-owned VM payload.

use super::descriptor::PropertyFlags;
use super::shape_body::{ShapeBody, ShapeHandle};
use super::shape_interner::ShapeInterner;
use super::{ShapeId, ShapeState};
use crate::inspect::{ShapeTransitionEvent, ShapeTransitionObserver};
use crate::isolate_names::IsolateNames;
use crate::property_atom::NameInterner;
use otter_gc::GcHeap;
use otter_gc::heap::RootSlotVisitor;
#[cfg(test)]
use otter_gc::raw::RawGc;

/// Optional transition observation; the heap owns every shape table.
#[derive(Default)]
pub(crate) struct ShapeRuntime {
    observer: Option<Box<dyn ShapeTransitionObserver>>,
}

impl std::fmt::Debug for ShapeRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShapeRuntime")
            .field("observer_installed", &self.observer.is_some())
            .finish()
    }
}

impl ShapeRuntime {
    pub(crate) fn new(heap: &mut GcHeap) -> Result<Self, otter_gc::OutOfMemory> {
        IsolateNames::install(heap, NameInterner::default());
        let root = ShapeInterner::new_root(
            heap,
            super::shape_body::ShapePrototype::Null,
            super::DEFAULT_INLINE_CAPACITY,
            ShapeHandle::null(),
            ShapeState::ORDINARY,
            &mut |_| {},
        )?;
        super::shape_body::set_null_root(heap, root);
        Ok(Self::default())
    }

    pub(crate) fn restored_shell(heap: &mut GcHeap, names: NameInterner) -> Self {
        IsolateNames::install(heap, names);
        Self::default()
    }

    pub(crate) fn set_observer(&mut self, observer: Option<Box<dyn ShapeTransitionObserver>>) {
        self.observer = observer;
    }

    pub(crate) fn transitions_for_snapshot(&self, heap: &GcHeap) -> Vec<(ShapeId, ShapeHandle)> {
        ShapeInterner::transitions_for_snapshot(heap)
    }

    pub(crate) fn register_shape(&self, heap: &GcHeap, shape: ShapeHandle) {
        ShapeInterner::register_shape(heap, shape);
    }

    pub(crate) fn new_root(
        &self,
        heap: &mut GcHeap,
        prototype: super::shape_body::ShapePrototype,
        inline_capacity: usize,
        previous_layout_root: ShapeHandle,
        state: ShapeState,
        external_visit: &mut RootSlotVisitor<'_>,
    ) -> Result<ShapeHandle, otter_gc::OutOfMemory> {
        ShapeInterner::new_root(
            heap,
            prototype,
            inline_capacity,
            previous_layout_root,
            state,
            external_visit,
        )
    }

    pub(crate) fn finish_slack_tracking(
        &self,
        heap: &mut GcHeap,
        root: ShapeHandle,
        required: usize,
        external_visit: &mut RootSlotVisitor<'_>,
    ) -> Result<(), otter_gc::OutOfMemory> {
        ShapeInterner::finish_slack_tracking(heap, root, required, external_visit)
    }

    pub(crate) fn state_with_roots(
        &self,
        heap: &mut GcHeap,
        source: ShapeHandle,
        state: ShapeState,
        external_visit: &mut RootSlotVisitor<'_>,
    ) -> Result<ShapeHandle, otter_gc::OutOfMemory> {
        ShapeInterner::state_with_roots(heap, source, state, external_visit)
    }

    pub(crate) fn unpin_turn_shapes(&self, heap: &GcHeap) {
        ShapeInterner::unpin_turn_shapes(heap);
    }

    pub(crate) fn handle_for_id(&self, heap: &GcHeap, id: ShapeId) -> Option<ShapeHandle> {
        ShapeInterner::handle_for_id(heap, id)
    }

    pub(crate) fn id_for_handle(&self, heap: &GcHeap, shape: ShapeHandle) -> ShapeId {
        heap.read_payload(shape, ShapeBody::id)
    }

    pub(crate) fn child_if_cached(
        &mut self,
        heap: &GcHeap,
        parent: ShapeHandle,
        key: &str,
        flags: PropertyFlags,
        is_accessor: bool,
    ) -> Option<ShapeHandle> {
        let child = ShapeInterner::child_if_cached(heap, parent, key, flags, is_accessor)?;
        self.notify_observer(heap, parent, child, key, true);
        Some(child)
    }

    pub(crate) fn child_with_roots(
        &mut self,
        heap: &mut GcHeap,
        parent: ShapeHandle,
        key: &str,
        flags: PropertyFlags,
        is_accessor: bool,
        external_visit: &mut RootSlotVisitor<'_>,
    ) -> Result<ShapeHandle, otter_gc::OutOfMemory> {
        let (child, reused) =
            ShapeInterner::child_with_roots(heap, parent, key, flags, is_accessor, external_visit)?;
        self.notify_observer(heap, parent, child, key, reused);
        Ok(child)
    }

    fn notify_observer(
        &mut self,
        heap: &GcHeap,
        parent: ShapeHandle,
        child: ShapeHandle,
        key: &str,
        reused: bool,
    ) {
        if let Some(observer) = self.observer.as_deref_mut() {
            observer.on_transition(&ShapeTransitionEvent {
                from_shape_id: heap.read_payload(parent, ShapeBody::id).raw(),
                to_shape_id: heap.read_payload(child, ShapeBody::id).raw(),
                key: key.to_string(),
                reused,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reuses_child_transition_and_interned_key() {
        let mut heap = GcHeap::new().expect("heap");
        let mut runtime = ShapeRuntime::new(&mut heap).expect("runtime");
        let mut roots = |_visitor: &mut dyn FnMut(*mut RawGc)| {};

        let flags = PropertyFlags::data_default();
        let root = super::super::shape_body::null_root(&heap);
        let first = runtime
            .child_with_roots(&mut heap, root, "x", flags, false, &mut roots)
            .expect("first child");
        let second = runtime
            .child_with_roots(&mut heap, root, "x", flags, false, &mut roots)
            .expect("second child");

        assert_eq!(first, second);
        assert_eq!(IsolateNames::of(&heap).key_string_count(), 1);
        let atom = crate::isolate_names::names(&heap).lookup("x");
        assert_eq!(
            super::super::shape_body::shape_slot_of_atom(&heap, first, atom)
                .map(|slot| slot.offset),
            Some(0)
        );
    }

    #[test]
    fn repeated_registration_pins_each_shape_once_and_reopens_after_a_turn() {
        let mut heap = GcHeap::new().expect("heap");
        let mut runtime = ShapeRuntime::new(&mut heap).expect("runtime");
        let root = super::super::shape_body::null_root(&heap);
        let dictionary = super::super::shape_body::dictionary_of(root);
        let mut roots = |_visitor: &mut dyn FnMut(*mut RawGc)| {};
        let child = runtime
            .child_with_roots(
                &mut heap,
                root,
                "retained",
                PropertyFlags::data_default(),
                false,
                &mut roots,
            )
            .expect("child");
        for _ in 0..10_000 {
            runtime.register_shape(&heap, child);
            runtime.register_shape(&heap, dictionary);
        }
        let mut expected = [root, dictionary, child];
        expected.sort_by_key(|shape| heap.read_payload(*shape, ShapeBody::id).raw());
        let pinned = || {
            let mut values: Vec<_> = ShapeInterner::pinned_shapes(&heap);
            values.sort_by_key(|shape| heap.read_payload(*shape, ShapeBody::id).raw());
            values
        };
        assert_eq!(pinned(), expected);
        let mut slots = 0;
        IsolateNames::of(&heap)
            .shapes()
            .trace_roots(&mut |_| slots += 1);
        assert_eq!(slots, expected.len());
        let mut key_roots = 0;
        otter_gc::IsolateData::visit_old_roots(IsolateNames::of(&heap), &mut |_| key_roots += 1);
        assert_eq!(
            key_roots,
            IsolateNames::of(&heap).key_string_count() + expected.len()
        );
        assert_eq!(IsolateNames::of(&heap).key_string_count(), 1);
        let dictionary_id = heap.read_payload(dictionary, ShapeBody::id);
        assert!(runtime.handle_for_id(&heap, dictionary_id).is_none());

        runtime.unpin_turn_shapes(&heap);
        assert!(pinned().is_empty());
        runtime.register_shape(&heap, child);
        assert_eq!(pinned(), expected);
    }

    #[test]
    fn interpreter_roots_shape_runtime_across_force_gc() {
        let mut interp = crate::Interpreter::new().expect("fixture interpreter bootstrap");
        let root = interp.null_prototype_root();
        let first = interp.shape_child(root, "x").expect("child");
        let first_id = interp.gc_heap.read_payload(first, ShapeBody::id);
        assert_eq!(
            interp
                .shape_runtime
                .handle_for_id(&interp.gc_heap, first_id),
            Some(first)
        );
        assert_eq!(interp.shape_offset_of(first, "x"), Some(0));

        interp.force_gc().expect("force GC");

        let root = interp.null_prototype_root();
        let second = interp.shape_child(root, "x").expect("child after gc");
        assert_eq!(
            interp
                .shape_runtime
                .handle_for_id(&interp.gc_heap, first_id),
            Some(second)
        );
        assert_eq!(interp.shape_offset_of(second, "x"), Some(0));
    }

    #[test]
    fn unnamed_shapes_are_collected_after_their_turn() {
        let mut interp = crate::Interpreter::new().expect("fixture interpreter bootstrap");
        let root = interp.null_prototype_root();
        let unused = interp.shape_child(root, "only_in_tables").expect("child");
        let unused_id = interp.gc_heap.read_payload(unused, ShapeBody::id);
        let kept = interp.shape_child(root, "kept").expect("child");
        let kept_id = interp.gc_heap.read_payload(kept, ShapeBody::id);
        assert_ne!(kept_id, unused_id);
        let mut object = interp
            .alloc_runtime_rooted_object_with_roots(&[], &[])
            .expect("object");
        interp
            .create_data_property(&mut object, "kept", crate::Value::boolean(true))
            .expect("store");
        // The global object keeps the object, and so its shape, alive.
        let mut global = interp.global_this;
        interp
            .create_data_property(&mut global, "holder", crate::Value::object(object))
            .expect("hold object");

        // Pinned while the (implicit) turn that created them is open.
        interp.force_gc().expect("force GC");
        assert!(
            interp
                .shape_runtime
                .handle_for_id(&interp.gc_heap, unused_id)
                .is_some()
        );

        interp.shape_runtime.unpin_turn_shapes(&interp.gc_heap);
        interp.force_gc().expect("force GC");
        assert!(
            interp
                .shape_runtime
                .handle_for_id(&interp.gc_heap, unused_id)
                .is_none()
        );
        assert!(
            interp
                .shape_runtime
                .handle_for_id(&interp.gc_heap, kept_id)
                .is_some()
        );
    }
}

#[cfg(test)]
#[path = "shape_interner_tests.rs"]
mod interner_tests;
