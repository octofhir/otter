//! Tests for the object-owned proof that hidden classes authorize named lookup.
//!
//! # Contents
//! - Host and mapped-arguments immutable opacity through prototype changes.
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
    heap.read_payload(object, |body| body.chain_link_opaque())
}

fn assert_prototype_changes_preserve_opacity(
    object: &mut JsObject,
    prototype: JsObject,
    heap: &mut GcHeap,
    expected: bool,
) {
    assert_eq!(opaque(*object, heap), expected);
    assert!(
        set_prototype_value(object, heap, Some(Value::object(prototype)))
            .expect("prototype state fixture")
    );
    assert_eq!(opaque(*object, heap), expected);
    assert!(set_prototype_value(object, heap, None).expect("prototype state fixture"));
    assert_eq!(opaque(*object, heap), expected);
}

#[test]
fn host_lookup_opacity_survives_prototype_changes_for_every_allocator() {
    for allocator in 0..3 {
        let mut heap = GcHeap::new().expect("heap");
        let prototype = alloc_object_old_for_fixture(&mut heap).expect("prototype");
        let env = alloc_object_old_for_fixture(&mut heap).expect("namespace environment");
        let mut roots = |_: &mut dyn FnMut(*mut RawGc)| {};
        let shape = shape_body::alloc_root_shape_body_with_roots(
            &mut heap,
            shape_body::ShapePrototype::Null,
            DEFAULT_INLINE_CAPACITY,
            ShapeHandle::null(),
            ShapeState::ORDINARY,
            &mut roots,
        )
        .expect("shape");
        let mut object = match allocator {
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
        // The mutation owner roots the actual young receiver through every
        // potentially collecting prototype/state allocation.
        assert_prototype_changes_preserve_opacity(&mut object, prototype, &mut heap, true);
    }
}

/// A parameter-scope context holding `values` in its slots.
fn parameter_context(heap: &mut GcHeap, values: &[Value]) -> crate::context::ContextHandle {
    let context = crate::context::alloc_context_with_roots(
        heap,
        crate::context::ContextShape {
            scope_function_id: 0,
            scope_index: 0,
            slot_count: values.len() as u16,
            has_extension: false,
        },
        Value::undefined(),
        |_| false,
        &mut |_| {},
    )
    .expect("parameter context");
    for (slot, value) in values.iter().enumerate() {
        assert!(crate::context::write_slot(
            heap,
            context,
            slot as u16,
            *value
        ));
    }
    context
}

#[test]
fn mapped_argument_lookup_opacity_survives_prototype_changes() {
    let mut heap = GcHeap::new().expect("heap");
    let mut object = alloc_object_old_for_fixture(&mut heap).expect("arguments");
    let prototype = alloc_object_old_for_fixture(&mut heap).expect("prototype");
    let context = parameter_context(&mut heap, &[Value::number_i32(1), Value::number_i32(2)]);
    assert!(!opaque(object, &heap));
    install_mapped_arguments(
        &mut object,
        &mut heap,
        MappedArguments {
            context,
            entries: vec![
                MappedArgumentEntry {
                    key: "0".into(),
                    slot: 0,
                },
                MappedArgumentEntry {
                    key: "1".into(),
                    slot: 1,
                },
            ],
        },
    )
    .expect("install_mapped_arguments fixture");
    assert_prototype_changes_preserve_opacity(&mut object, prototype, &mut heap, true);
    heap.with_payload(object, |body| remove_mapped_argument(body, "0"));
    assert_eq!(
        heap.read_payload(object, |body| mapped_argument_cell(body, "1")),
        Some((context, 1)),
    );
    assert_eq!(
        heap.read_payload(object, |body| mapped_argument_cell(body, "0")),
        None,
    );
    assert_prototype_changes_preserve_opacity(&mut object, prototype, &mut heap, true);
}

#[test]
fn symbol_sidecars_preserve_ordinary_named_lookup() {
    let mut heap = GcHeap::new().expect("heap");
    let mut object = alloc_object_old_for_fixture(&mut heap).expect("object");
    let prototype = alloc_object_old_for_fixture(&mut heap).expect("prototype");
    let symbol = JsSymbol::new(&mut heap, None).expect("symbol");
    assert!(
        define_own_symbol_property(
            object,
            &mut heap,
            symbol,
            PropertyDescriptor::data(Value::number_i32(1), false, false, true),
        )
        .expect("descriptor fixture allocation")
    );
    assert!(heap.read_payload(object, |body| !body.exotic.is_null()));
    assert_prototype_changes_preserve_opacity(&mut object, prototype, &mut heap, false);
}

#[test]
fn string_wrapper_lookup_opacity_survives_prototype_changes() {
    let mut heap = GcHeap::new().expect("heap");
    let mut object = alloc_object_old_for_fixture(&mut heap).expect("wrapper");
    let prototype = alloc_object_old_for_fixture(&mut heap).expect("prototype");
    let string = JsString::from_str("x", &mut heap).expect("string");
    set_string_data(&mut object, &mut heap, string).expect("set_string_data fixture");
    assert_prototype_changes_preserve_opacity(&mut object, prototype, &mut heap, true);
}

fn shaped_own_slot_fixture() -> (crate::Interpreter, JsObject) {
    let mut interpreter = crate::Interpreter::new().expect("fixture interpreter bootstrap");
    let mut object = alloc_object_old_for_fixture(interpreter.gc_heap_mut()).expect("object");
    interpreter
        .create_data_property(&mut object, "0", Value::number_i32(7))
        .expect("shaped own property construction");
    interpreter.migrate_slow_to_fast(&mut object);
    assert!(interpreter.gc_heap().read_payload(object, |body| {
        !body.is_dictionary() && !body.state().is_opaque()
    }));
    (interpreter, object)
}

#[test]
fn own_data_hits_allow_symbols_and_retire_when_descriptor_authority_changes() {
    let (mut interpreter, mut object) = shaped_own_slot_fixture();
    let names = crate::property_atom::NameInterner::default();
    let atom = crate::property_atom::PropertyAtom::new(names.intern("0"));
    let key = AtomizedPropertyKey::new(atom, "0");
    let hit = lookup_own_atom(object, interpreter.gc_heap(), key)
        .hit
        .expect("own slot");
    let symbol = JsSymbol::new(interpreter.gc_heap_mut(), None).expect("symbol");
    assert!(
        define_own_symbol_property(
            object,
            interpreter.gc_heap_mut(),
            symbol,
            PropertyDescriptor::data(Value::number_i32(1), false, false, true),
        )
        .expect("descriptor fixture allocation")
    );
    assert!(interpreter.gc_heap().read_payload(object, |body| {
        !body.exotic.is_null() && !body.chain_link_opaque()
    }));
    assert_eq!(
        load_own_data_slot_by_shape(object, interpreter.gc_heap(), hit),
        Some(Value::number_i32(7)),
    );
    assert_eq!(
        load_own_data_slot_atom(object, interpreter.gc_heap(), key, hit),
        Some(Value::number_i32(7)),
    );

    // Materialization installs a dictionary shape. Every old immutable-shape
    // hit declines; a newly captured dictionary hit checks live metadata.
    materialize_slots(&mut object, interpreter.gc_heap_mut()).expect("materialize_slots fixture");
    assert!(interpreter.gc_heap().read_payload(object, |body| {
        body.shape != hit.shape && body.is_dictionary()
    }));
    assert_eq!(
        load_own_data_slot_by_shape(object, interpreter.gc_heap(), hit),
        None,
    );
    assert_eq!(
        load_own_data_slot_atom(object, interpreter.gc_heap(), key, hit),
        None,
    );
    let current = lookup_own_atom(object, interpreter.gc_heap(), key)
        .hit
        .expect("dictionary hit");
    assert_eq!(
        load_own_data_slot_atom(object, interpreter.gc_heap(), key, current),
        Some(Value::number_i32(7))
    );
}

#[test]
fn own_data_hits_preserve_mapped_argument_values() {
    let (mut interpreter, mut object) = shaped_own_slot_fixture();
    let names = crate::property_atom::NameInterner::default();
    let atom = crate::property_atom::PropertyAtom::new(names.intern("0"));
    let key = AtomizedPropertyKey::new(atom, "0");
    let hit = lookup_own_atom(object, interpreter.gc_heap(), key)
        .hit
        .expect("own slot");
    let context = parameter_context(interpreter.gc_heap_mut(), &[Value::number_i32(41)]);
    install_mapped_arguments(
        &mut object,
        interpreter.gc_heap_mut(),
        MappedArguments {
            context,
            entries: vec![MappedArgumentEntry {
                key: "0".into(),
                slot: 0,
            }],
        },
    )
    .expect("install_mapped_arguments fixture");
    assert!(interpreter.gc_heap().read_payload(object, |body| {
        body.shape != hit.shape && body.chain_link_opaque()
    }));
    assert_eq!(
        load_own_data_slot_by_shape(object, interpreter.gc_heap(), hit),
        None,
    );
    assert_eq!(
        load_own_data_slot_atom(object, interpreter.gc_heap(), key, hit),
        None,
    );
    let current = lookup_own_atom(object, interpreter.gc_heap(), key)
        .hit
        .expect("mapped hit");
    assert_ne!(current.shape, hit.shape);
    assert_eq!(
        load_own_data_slot_by_shape(object, interpreter.gc_heap(), current),
        None
    );
    assert_eq!(
        load_own_data_slot_atom(object, interpreter.gc_heap(), key, current),
        Some(Value::number_i32(41))
    );
}
