//! Tests for the object-owned proof that hidden classes authorize named lookup.
//!
//! # Contents
//! - Host and mapped-arguments opacity through prototype changes.
//! - Ordinary symbol sidecars and String wrapper lookup semantics.
//! - Rust own-slot hits share generated ordinary-state proofs.
//!
//! # Invariants
//! - Installing or changing a prototype cannot hide host or virtual-key lookup.
//! - Sidecar allocation alone does not invalidate ordinary named lookup.
//!
//! # See also
//! - `ObjectBody::chain_link_opaque` and generated CacheIR shape guards.

use super::*;

struct HostPayload;

impl HostObjectData for HostPayload {}

fn opaque(object: JsObject, heap: &GcHeap) -> bool {
    heap.read_payload(object, |body| body.chain_link_opaque)
}

fn assert_prototype_changes_preserve_opacity(
    object: JsObject,
    prototype: JsObject,
    heap: &mut GcHeap,
    expected: bool,
) {
    assert_eq!(opaque(object, heap), expected);
    assert!(set_prototype_value(
        object,
        heap,
        Some(Value::object(prototype))
    ));
    assert_eq!(opaque(object, heap), expected);
    assert!(set_prototype_value(object, heap, None));
    assert_eq!(opaque(object, heap), expected);
}

#[test]
fn host_lookup_opacity_survives_prototype_changes_for_every_allocator() {
    for allocator in 0..3 {
        let mut heap = GcHeap::new().expect("heap");
        let prototype = alloc_object_old_for_fixture(&mut heap).expect("prototype");
        let env = alloc_object_old_for_fixture(&mut heap).expect("namespace environment");
        let mut roots = |_: &mut dyn FnMut(*mut RawGc)| {};
        let shape =
            shape_body::alloc_root_shape_body_with_roots(&mut heap, &mut roots).expect("shape");
        let object = match allocator {
            0 => alloc_host_object_with_roots(&mut heap, HostPayload, &mut roots),
            1 => alloc_host_object_with_shape_roots(&mut heap, shape, HostPayload, &mut roots),
            2 => alloc_traced_host_object_with_shape_roots(
                &mut heap,
                shape,
                ModuleNamespaceData::new(env, "file:///namespace.mjs".into()),
                &mut roots,
            ),
            _ => unreachable!(),
        }
        .expect("host object");
        // No allocating operation follows the young host object's creation.
        assert_prototype_changes_preserve_opacity(object, prototype, &mut heap, true);
    }
}

#[test]
fn mapped_argument_lookup_opacity_survives_prototype_changes() {
    let mut heap = GcHeap::new().expect("heap");
    let object = alloc_object_old_for_fixture(&mut heap).expect("arguments");
    let prototype = alloc_object_old_for_fixture(&mut heap).expect("prototype");
    let cell = crate::alloc_upvalue(&mut heap, Value::number_i32(1)).expect("mapped cell");
    let retained_cell =
        crate::alloc_upvalue(&mut heap, Value::number_i32(2)).expect("retained mapped cell");
    assert!(!opaque(object, &heap));
    install_mapped_arguments(
        object,
        &mut heap,
        vec![
            MappedArgumentEntry {
                key: "0".into(),
                cell,
            },
            MappedArgumentEntry {
                key: "1".into(),
                cell: retained_cell,
            },
        ],
    );
    assert_prototype_changes_preserve_opacity(object, prototype, &mut heap, true);
    heap.with_payload(object, |body| remove_mapped_argument(body, "0"));
    assert_eq!(
        heap.read_payload(object, |body| mapped_argument_cell(body, "1")),
        Some(retained_cell),
    );
    assert_prototype_changes_preserve_opacity(object, prototype, &mut heap, true);
}

#[test]
fn symbol_sidecars_preserve_ordinary_named_lookup() {
    let mut heap = GcHeap::new().expect("heap");
    let object = alloc_object_old_for_fixture(&mut heap).expect("object");
    let prototype = alloc_object_old_for_fixture(&mut heap).expect("prototype");
    let symbol = JsSymbol::new(&mut heap, None).expect("symbol");
    assert!(define_own_symbol_property(
        object,
        &mut heap,
        symbol,
        PropertyDescriptor::data(Value::number_i32(1), false, false, true),
    ));
    assert!(heap.read_payload(object, |body| !body.exotic.is_null()));
    assert_prototype_changes_preserve_opacity(object, prototype, &mut heap, false);
}

#[test]
fn string_wrapper_lookup_opacity_survives_prototype_changes() {
    let mut heap = GcHeap::new().expect("heap");
    let mut object = alloc_object_old_for_fixture(&mut heap).expect("wrapper");
    let prototype = alloc_object_old_for_fixture(&mut heap).expect("prototype");
    let string = JsString::from_str("x", &mut heap).expect("string");
    set_string_data(&mut object, &mut heap, string);
    assert_prototype_changes_preserve_opacity(object, prototype, &mut heap, true);
}

fn shaped_own_slot_fixture() -> (crate::Interpreter, JsObject) {
    let mut interpreter = crate::Interpreter::new();
    let mut object = alloc_object_old_for_fixture(interpreter.gc_heap_mut()).expect("object");
    set(
        &mut object,
        interpreter.gc_heap_mut(),
        "0",
        Value::number_i32(7),
    );
    interpreter.migrate_slow_to_fast(&mut object);
    assert!(interpreter.gc_heap().read_payload(object, |body| {
        !body.shape.is_null() && !body.slot_attrs_overridden
    }));
    (interpreter, object)
}

#[test]
fn own_data_hits_allow_symbols_but_reject_descriptor_overrides() {
    let (mut interpreter, object) = shaped_own_slot_fixture();
    let names = crate::property_atom::NameInterner::default();
    let atom = crate::property_atom::PropertyAtom::new(names.intern("0"));
    let key = AtomizedPropertyKey::new(atom, "0");
    let hit = lookup_own_atom(object, interpreter.gc_heap(), key)
        .hit
        .expect("own slot");
    let symbol = JsSymbol::new(interpreter.gc_heap_mut(), None).expect("symbol");
    assert!(define_own_symbol_property(
        object,
        interpreter.gc_heap_mut(),
        symbol,
        PropertyDescriptor::data(Value::number_i32(1), false, false, true),
    ));
    assert!(interpreter.gc_heap().read_payload(object, |body| {
        !body.exotic.is_null() && !body.chain_link_opaque
    }));
    assert_eq!(
        load_own_data_slot_by_shape(object, interpreter.gc_heap(), hit),
        Some(Value::number_i32(7)),
    );
    assert_eq!(
        load_own_data_slot_atom(object, interpreter.gc_heap(), key, hit),
        Some(Value::number_i32(7)),
    );

    // Materialized descriptors cease to be justified by the same shape.
    // The shape-only path must decline; the atom path checks live metadata.
    materialize_slots(object, interpreter.gc_heap_mut());
    assert!(interpreter.gc_heap().read_payload(object, |body| {
        body.shape == hit.shape && body.slot_attrs_overridden
    }));
    assert_eq!(
        load_own_data_slot_by_shape(object, interpreter.gc_heap(), hit),
        None,
    );
    assert_eq!(
        load_own_data_slot_atom(object, interpreter.gc_heap(), key, hit),
        Some(Value::number_i32(7)),
    );
}

#[test]
fn own_data_hits_preserve_mapped_argument_values() {
    let (mut interpreter, object) = shaped_own_slot_fixture();
    let names = crate::property_atom::NameInterner::default();
    let atom = crate::property_atom::PropertyAtom::new(names.intern("0"));
    let key = AtomizedPropertyKey::new(atom, "0");
    let hit = lookup_own_atom(object, interpreter.gc_heap(), key)
        .hit
        .expect("own slot");
    let cell = crate::alloc_upvalue(interpreter.gc_heap_mut(), Value::number_i32(41))
        .expect("mapped cell");
    install_mapped_arguments(
        object,
        interpreter.gc_heap_mut(),
        vec![MappedArgumentEntry {
            key: "0".into(),
            cell,
        }],
    );
    assert!(interpreter.gc_heap().read_payload(object, |body| {
        body.shape == hit.shape && body.chain_link_opaque
    }));
    assert_eq!(
        load_own_data_slot_by_shape(object, interpreter.gc_heap(), hit),
        None,
    );
    assert_eq!(
        load_own_data_slot_atom(object, interpreter.gc_heap(), key, hit),
        Some(Value::number_i32(41)),
    );
}
