//! Rooted immutable object-state preparation and publication.
//!
//! # Contents
//! - `prepare_state_shape` copies one immutable lineage into a new state.
//! - `install_state_shape` publishes a prepared same-geometry variant.
//! - `transition_state` is the heap-only rooted state-mutation owner.
//!
//! # Invariants
//! - No allocation occurs inside an object/shape payload borrow.
//! - Existing cells retain capacity, field numbering and descriptor authority.
//! - Provisional state and acquired prototype role are monotonic within a family.
//! - Source and partial result are rooted through actual collections. Moving key
//!   handles are read from source shapes after each allocating operation.
//! - Allocation failure propagates before publication; no cap bypass is used.
//! - Prototype validity is retired before publication; state lives only in shape.
//!
//! # See also
//! - `super::shape_runtime` caches state variants under stable shape identities.
//! - `crate::interp::shapes` prepares descriptor variants using the same roots.

use super::{JsObject, ShapeHandle, ShapeState, shape_body};
use crate::rooting::RootScopeExt;
use otter_gc::{GcHeap, HandleScope, RootScope, heap::RootSlotVisitor};

pub(super) fn prepare_state_shape(
    heap: &mut GcHeap,
    source: ShapeHandle,
    state: ShapeState,
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<ShapeHandle, otter_gc::OutOfMemory> {
    let current = shape_body::state_of(source);
    assert_eq!(
        current.is_dictionary(),
        state.is_dictionary(),
        "state changes preserve storage"
    );
    assert!(
        !current.is_provisional() || state.is_provisional(),
        "a provisional lineage is never finalized in place"
    );
    assert!(
        !current.is_prototype() || state.is_prototype(),
        "prototype role is monotonic"
    );
    if current == state {
        return Ok(source);
    }
    // SAFETY: the heap and its handle stack outlive this cold preparation and
    // no Local escapes; the returned old shape is installed without a safepoint.
    let scope = unsafe { HandleScope::from_ptr(heap.handle_stack_ptr()) };
    let source = scope.local(source);
    let capacity = shape_body::inline_capacity_of(source.get());
    let mut nodes = Vec::new();
    if !current.is_dictionary() {
        let mut cursor = source.get();
        while !heap.read_payload(cursor, shape_body::ShapeBody::is_root) {
            nodes.push(cursor);
            cursor = heap.read_payload(cursor, shape_body::ShapeBody::parent);
        }
    }
    let root_state = state.with_dictionary(false);
    let prototype = shape_body::prototype_of(source.get());
    let root = if !current.is_provisional() {
        super::heap_instance_root(
            super::object_prototype_of_shape(prototype),
            heap,
            capacity,
            root_state,
            external_visit,
        )?
    } else {
        // Constructor families own provisional roots. A state mutation cannot
        // merge sampling families or keep an old family through the root cache.
        shape_body::alloc_root_shape_body_with_roots(
            heap,
            prototype,
            capacity,
            ShapeHandle::null(),
            root_state,
            external_visit,
        )?
    };
    let mut result = scope.local(root);
    if current.is_dictionary() {
        return Ok(shape_body::dictionary_of(result.get()));
    }
    for node in nodes.into_iter().rev() {
        // Shapes do not move; the rooted source keeps this chain alive. Its
        // key slot was rewritten by any collection during root preparation.
        let (key, atom, flags, accessor) = heap.read_payload(node, |body| {
            (
                body.transition_key(),
                body.transition_atom(),
                body.own_flags(),
                body.own_is_accessor(),
            )
        });
        let next = shape_body::alloc_child_shape_body_with_roots(
            heap,
            result.get(),
            key,
            atom,
            flags,
            accessor,
            external_visit,
        )?;
        result = scope.local(next);
    }
    Ok(result.get())
}

pub(super) fn install_state_shape(object: JsObject, heap: &mut GcHeap, shape: ShapeHandle) {
    heap.with_payload(object, |body| {
        assert_eq!(
            body.inline_capacity(),
            shape_body::inline_capacity_of(shape),
            "state transition changed footprint"
        );
        assert_eq!(
            body.is_dictionary(),
            shape_body::is_dictionary_of(shape),
            "state transition changed storage"
        );
        if body.shape != shape {
            body.invalidate_prototype_proofs();
            body.shape = shape;
        }
    });
    heap.record_write(object, &shape);
}

pub(super) fn transition_state(
    object: &mut JsObject,
    heap: &mut GcHeap,
    state: ShapeState,
) -> Result<(), otter_gc::OutOfMemory> {
    let source = super::shape(*object, heap);
    let mut roots = RootScope::new(heap);
    // SAFETY: the caller-owned receiver slot stays stationary and outlives the
    // scope. Every collection rewrites the actual slot which is published below.
    unsafe {
        roots.add_object(object);
    }
    let prepared = prepare_state_shape(heap, source, state, &mut |_| {})?;
    install_state_shape(*object, heap, prepared);
    Ok(())
}
