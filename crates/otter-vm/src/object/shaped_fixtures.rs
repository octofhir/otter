//! Ordinary shape premises for heap-only inline-cache regression fixtures.
//!
//! # Contents
//! - `append_shaped_data_for_fixture` builds and publishes an actual child shape.
//!
//! # Invariants
//! - The raw dictionary transition fixture stays a separate deliberate refusal.
//! - Receivers are non-moving old fixture cells and remain rooted during allocation.
//! - Inputs are immediate primitives; no unrooted moving value crosses allocation.
//! - The production shape allocator and transition installer own all publication.
//!
//! # See also
//! - `super::shape_transition` implements the real replay contracts.

use super::{JsObject, PropertyFlags, shape, shape_body};
use crate::Value;
use crate::property_atom::AtomizedPropertyKey;
use crate::string::{JsStringId, alloc_flat_string_body_with_roots};
use otter_gc::GcHeap;

pub(crate) fn append_shaped_data_for_fixture(
    object: JsObject,
    heap: &mut GcHeap,
    key: AtomizedPropertyKey<'_>,
    value: Value,
) {
    assert!(
        value.as_number().is_some()
            || value.as_boolean().is_some()
            || value.is_null()
            || value.is_undefined(),
        "fixture inputs are immediate"
    );
    assert!(!super::is_dictionary(object, heap));
    // SAFETY: the scope belongs to this heap, remains live through the complete
    // append, and all caller-owned receiver cells were allocated in old space.
    let scope = unsafe { otter_gc::HandleScope::from_ptr(heap.handle_stack_ptr()) };
    let receiver = scope.local(object);
    let name: Vec<u16> = key.name().encode_utf16().collect();
    let name = alloc_flat_string_body_with_roots(
        heap,
        JsStringId::new(key.atom().id().raw()),
        &name,
        &mut |_| {},
    )
    .expect("fixture shape key");
    let name = scope.local(name);
    let child = shape_body::alloc_child_shape_body_with_roots(
        heap,
        shape(receiver.get(), heap),
        name.get(),
        key.atom().id(),
        PropertyFlags::data_default(),
        false,
        &mut |_| {},
    )
    .expect("actual child shape");
    let child = scope.local(child);
    super::capture_store_property_transition_with_shape(
        receiver.get(),
        heap,
        key,
        &value,
        child.get(),
    )
    .expect("actual ordinary shaped append");
    assert_eq!(receiver.get(), object, "old receiver is stationary");
    assert!(!super::is_dictionary(object, heap));
    assert_eq!(super::get_own(object, heap, key.name()), Some(value));
}

#[test]
fn final_delete_retires_old_load_proof_and_restores_valid_parent_identity() {
    let mut heap = super::fixture_heap();
    let mut object = super::alloc_object_old_for_fixture(&mut heap).expect("old receiver");
    let parent = shape(object, &heap);
    let key = crate::property_atom::AtomizedPropertyKey::new(
        crate::property_atom::PropertyAtom::new(crate::property_atom::AtomId::from_global(7)),
        "x",
    );
    append_shaped_data_for_fixture(object, &mut heap, key, Value::boolean(true));
    let resolved =
        crate::cache_ir::resolve_atom_data_slot(object, &heap, key).expect("old own proof");
    let old = crate::property_ic::PropertyIcSlot::new(crate::property_ic::PropertyIcKind::Load);
    old.install(
        crate::property_ic::IcHandler::load_resolved(shape(object, &heap), &resolved)
            .expect("own handler"),
    );
    assert_eq!(old.probe_load(object, &heap), Some(Value::boolean(true)));
    assert!(super::delete(&mut object, &mut heap, "x").expect("actual final delete"));
    assert_eq!(shape(object, &heap), parent);
    assert!(!super::is_dictionary(object, &heap));
    assert_eq!(old.probe_load(object, &heap), None);
    assert_eq!(super::get_own(object, &heap, "x"), None);
    append_shaped_data_for_fixture(object, &mut heap, key, Value::boolean(false));
    let current =
        crate::cache_ir::resolve_atom_data_slot(object, &heap, key).expect("new actual proof");
    let fresh = crate::property_ic::PropertyIcSlot::new(crate::property_ic::PropertyIcKind::Load);
    fresh.install(
        crate::property_ic::IcHandler::load_resolved(shape(object, &heap), &current)
            .expect("own handler"),
    );
    assert_eq!(fresh.probe_load(object, &heap), Some(Value::boolean(false)));
}
