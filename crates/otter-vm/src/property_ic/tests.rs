use super::{IcHandler, IcHandlerKind, InstallOutcome, PropertyIcKind, PropertyIcSlot};
use crate::object::{self, PropertyDescriptor};
use crate::property_atom::{AtomId, AtomizedPropertyKey, PropertyAtom};
use crate::{Value, jit::JitCacheIrOp};

fn fresh_heap() -> otter_gc::GcHeap {
    otter_gc::GcHeap::new().expect("init heap")
}

/// The fixture atom of each property name.
fn atom_of(name: &str) -> u32 {
    match name {
        "own" => 6,
        "x" => 7,
        "y" => 8,
        "z" => 9,
        "a" => 10,
        "b" => 11,
        "c" => 12,
        "d" => 13,
        "e" => 14,
        _ => panic!("fixture atom name"),
    }
}

fn key<'a>(name: &'a str) -> AtomizedPropertyKey<'a> {
    AtomizedPropertyKey::new(PropertyAtom::new(AtomId::from_global(atom_of(name))), name)
}

/// Actual shaped append, rather than raw construction's dictionary store.
fn shaped_data_fixture(
    object: object::JsObject,
    heap: &mut otter_gc::GcHeap,
    name: &str,
    value: Value,
) {
    object::append_shaped_data_for_fixture(object, heap, key(name), value);
}

/// Install the real mapped-arguments lookup producer on an existing old
/// fixture object. Its context is rooted by the production installer while
/// sidecar/state preparation allocates; no raw header latch is fabricated.
fn install_mapped_lookup(obj: &mut object::JsObject, heap: &mut otter_gc::GcHeap) {
    // SAFETY: the heap and its handle stack outlive this fixture call;
    // the scoped receiver cannot escape and is read after allocation.
    let scope = unsafe { otter_gc::HandleScope::from_ptr(heap.handle_stack_ptr()) };
    let receiver = scope.local(*obj);
    let context = crate::context::alloc_context_with_roots(
        heap,
        crate::context::ContextShape {
            scope_function_id: 0,
            scope_index: 0,
            slot_count: 1,
            has_extension: false,
        },
        Value::undefined(),
        |_| false,
        &mut |_| {},
    )
    .expect("mapped parameter context");
    assert!(crate::context::write_slot(
        heap,
        context,
        0,
        Value::boolean(true)
    ));
    *obj = receiver.get();
    object::install_mapped_arguments(
        obj,
        heap,
        object::MappedArguments {
            context,
            entries: vec![object::MappedArgumentEntry {
                key: "virtual".into(),
                slot: 0,
            }],
        },
    )
    .expect("actual mapped lookup installation");
    assert!(object::state(*obj, heap).is_opaque());
}

fn own_load_handler(obj: object::JsObject, heap: &otter_gc::GcHeap, name: &str) -> IcHandler {
    let resolved = crate::cache_ir::resolve_atom_data_slot(obj, heap, key(name)).expect("own data");
    IcHandler::load_resolved(object::keyed_shape(obj, heap), &resolved).expect("own handler")
}

/// `count` objects with pairwise distinct shapes, each owning `x`.
fn distinct_receivers(heap: &mut otter_gc::GcHeap, count: usize) -> Vec<object::JsObject> {
    const PREFIX: [&str; 5] = ["a", "b", "c", "d", "e"];
    (0..count)
        .map(|index| {
            let obj = object::alloc_object_old_for_fixture(heap).unwrap();
            shaped_data_fixture(obj, heap, PREFIX[index], Value::null());
            shaped_data_fixture(obj, heap, "x", Value::number_i32(index as i32));
            obj
        })
        .collect()
}

#[test]
fn attempted_uncacheable_site_is_distinct_from_cold_and_can_later_attach() {
    let mut heap = fresh_heap();
    let slot = PropertyIcSlot::new(PropertyIcKind::Load);
    assert!(!slot.attempted());
    assert!(slot.record_attempt());
    assert!(slot.attempted());
    assert_eq!(slot.entry_count(), 0);
    assert!(!slot.record_attempt());

    let obj = object::alloc_object_old_for_fixture(&mut heap).unwrap();
    shaped_data_fixture(obj, &mut heap, "x", Value::boolean(true));
    assert_eq!(
        slot.install(own_load_handler(obj, &heap, "x")),
        InstallOutcome::Installed
    );
    assert_eq!(slot.entry_count(), 1);
    assert!(slot.attempted());
    assert_eq!(slot.probe_load(obj, &heap), Some(Value::boolean(true)));
}

#[test]
fn slot_grows_until_capacity_then_becomes_terminally_megamorphic() {
    let mut heap = fresh_heap();
    let receivers = distinct_receivers(&mut heap, 5);
    let slot = PropertyIcSlot::new(PropertyIcKind::Load);
    for (index, obj) in receivers.iter().take(4).enumerate() {
        assert_eq!(
            slot.install(own_load_handler(*obj, &heap, "x")),
            InstallOutcome::Installed
        );
        assert_eq!(slot.entry_count(), index + 1);
    }
    for (index, obj) in receivers.iter().take(4).enumerate() {
        assert_eq!(
            slot.probe_load(*obj, &heap),
            Some(Value::number_i32(index as i32))
        );
    }
    assert_eq!(
        slot.install(own_load_handler(receivers[4], &heap, "x")),
        InstallOutcome::BecameMegamorphic
    );
    assert!(slot.is_megamorphic());
    assert_eq!(slot.entry_count(), 0);
    assert_eq!(slot.probe_load(receivers[0], &heap), None);
    assert_eq!(
        slot.install(own_load_handler(receivers[0], &heap, "x")),
        InstallOutcome::Unchanged
    );
    assert_eq!(slot.stats().load_installs, 4);
    assert_eq!(slot.stats().load_disables, 1);
}

#[test]
fn same_receiver_shape_heals_in_place() {
    let mut heap = fresh_heap();
    let obj = object::alloc_object_old_for_fixture(&mut heap).unwrap();
    shaped_data_fixture(obj, &mut heap, "x", Value::boolean(true));
    let slot = PropertyIcSlot::new(PropertyIcKind::Load);
    for _ in 0..8 {
        assert_eq!(
            slot.install(own_load_handler(obj, &heap, "x")),
            InstallOutcome::Installed
        );
    }
    assert_eq!(slot.entry_count(), 1);
    assert!(!slot.is_megamorphic());
}

#[test]
fn generated_inline_pair_names_the_first_own_field_entry() {
    let mut heap = fresh_heap();
    let receivers = distinct_receivers(&mut heap, 2);
    let slot = PropertyIcSlot::new(PropertyIcKind::Load);
    assert_eq!(
        slot.inline_shape.load(std::sync::atomic::Ordering::Relaxed),
        super::EMPTY_SHAPE
    );
    slot.install(own_load_handler(receivers[0], &heap, "x"));
    slot.install(own_load_handler(receivers[1], &heap, "x"));
    let shape = object::keyed_shape(receivers[0], &heap).offset();
    assert_eq!(
        slot.inline_shape.load(std::sync::atomic::Ordering::Relaxed),
        shape
    );
    assert_eq!(
        slot.inline_field.load(std::sync::atomic::Ordering::Relaxed),
        object::FieldLocation::inline(1).cache_key()
    );
}

#[test]
fn nonexistent_handler_answers_undefined_until_the_chain_gains_the_key() {
    let mut heap = fresh_heap();
    // SAFETY: the scope belongs to the stationary heap and ends before it.
    let roots = unsafe { otter_gc::HandleScope::from_ptr(heap.handle_stack_ptr()) };
    let mut proto = object::alloc_object_old_for_fixture(&mut heap).unwrap();
    let _proto_root = roots.local(proto);
    shaped_data_fixture(proto, &mut heap, "y", Value::null());
    let mut receiver = object::alloc_object_old_for_fixture(&mut heap).unwrap();
    let _receiver_root = roots.local(receiver);
    assert!(object::set_prototype(&mut receiver, &mut heap, Some(proto)).unwrap());
    let validity =
        crate::cache_ir::resolve_absent_atom(receiver, &heap, key("x")).expect("absent chain");
    assert!(validity.is_some(), "an ordinary chain carries its proof");
    let slot = PropertyIcSlot::new(PropertyIcKind::Load);
    slot.install(
        IcHandler::load_nonexistent(object::keyed_shape(receiver, &heap), validity).unwrap(),
    );
    assert_eq!(slot.probe_load(receiver, &heap), Some(Value::undefined()));

    assert!(
        object::ordinary_set_data_property(&mut proto, &mut heap, "x", Value::boolean(true))
            .unwrap()
    );
    assert_eq!(slot.probe_load(receiver, &heap), None, "proof died");
    assert!(crate::cache_ir::resolve_absent_atom(receiver, &heap, key("x")).is_none());
}

#[test]
fn null_prototype_absence_needs_no_proof() {
    let mut heap = fresh_heap();
    let mut obj = object::alloc_object_old_for_fixture(&mut heap).unwrap();
    assert!(object::set_prototype(&mut obj, &mut heap, None).unwrap());
    shaped_data_fixture(obj, &mut heap, "y", Value::null());
    assert_eq!(
        crate::cache_ir::resolve_absent_atom(obj, &heap, key("x")).map(|v| v.is_none()),
        Some(true)
    );
    assert!(crate::cache_ir::resolve_absent_atom(obj, &heap, key("y")).is_none());
}

#[test]
fn direct_prototype_load_ic_rejects_dictionary_prototype() {
    let mut heap = fresh_heap();
    // SAFETY: this scope belongs to the stationary heap and ends before
    // it; every old fixture cell stays traced through collecting metadata.
    let fixture_roots = unsafe { otter_gc::HandleScope::from_ptr(heap.handle_stack_ptr()) };
    let mut proto = object::alloc_object_old_for_fixture(&mut heap).unwrap();
    let _proto_root = fixture_roots.local(proto);
    shaped_data_fixture(proto, &mut heap, "x", Value::boolean(true));
    shaped_data_fixture(proto, &mut heap, "y", Value::null());
    shaped_data_fixture(proto, &mut heap, "z", Value::null());
    let mut receiver = object::alloc_object_old_for_fixture(&mut heap).unwrap();
    let _receiver_root = fixture_roots.local(receiver);
    assert!(
        object::set_prototype(&mut receiver, &mut heap, Some(proto))
            .expect("fixture prototype transition")
    );
    let resolved =
        crate::cache_ir::resolve_atom_data_slot(receiver, &heap, key("x")).expect("load ic");
    let slot = PropertyIcSlot::new(PropertyIcKind::Load);
    let handler = IcHandler::load_resolved(object::keyed_shape(receiver, &heap), &resolved)
        .expect("prototype handler");
    assert_eq!(handler.kind, IcHandlerKind::PrototypeField);
    slot.install(handler);
    assert_eq!(slot.probe_load(receiver, &heap), Some(Value::boolean(true)));

    assert!(object::delete(&mut proto, &mut heap, "y").expect("non-final property deletion"));
    assert!(
        object::is_dictionary(proto, &heap),
        "actual dictionary holder"
    );
    assert_eq!(
        object::get_own(proto, &heap, "x"),
        Some(Value::boolean(true))
    );
    assert_eq!(slot.probe_load(receiver, &heap), None);
}

#[test]
fn direct_prototype_store_transition_rejects_dictionary_prototype() {
    let mut heap = fresh_heap();
    // SAFETY: this scope belongs to the stationary heap and ends before
    // it; every old fixture cell stays traced through collecting metadata.
    let fixture_roots = unsafe { otter_gc::HandleScope::from_ptr(heap.handle_stack_ptr()) };
    let mut proto = object::alloc_object_old_for_fixture(&mut heap).unwrap();
    let _proto_root = fixture_roots.local(proto);
    shaped_data_fixture(proto, &mut heap, "x", Value::boolean(true));
    shaped_data_fixture(proto, &mut heap, "y", Value::null());
    shaped_data_fixture(proto, &mut heap, "z", Value::null());
    let mut first = object::alloc_object_old_for_fixture(&mut heap).unwrap();
    let _first_root = fixture_roots.local(first);
    assert!(
        object::set_prototype(&mut first, &mut heap, Some(proto))
            .expect("fixture prototype transition")
    );
    let from_shape = object::keyed_shape(first, &heap);
    let transition = object::capture_store_property_transition(
        first,
        &mut heap,
        key("x"),
        &Value::boolean(false),
    )
    .expect("store transition");
    let slot = PropertyIcSlot::new(PropertyIcKind::Store);
    slot.install(IcHandler::store_transition(from_shape, &transition).expect("transition"));
    let mut second = object::alloc_object_old_for_fixture(&mut heap).unwrap();
    let _second_root = fixture_roots.local(second);
    assert!(
        object::set_prototype(&mut second, &mut heap, Some(proto))
            .expect("fixture prototype transition")
    );
    assert_eq!(object::keyed_shape(second, &heap), from_shape);

    assert!(object::delete(&mut proto, &mut heap, "y").expect("non-final property deletion"));
    assert!(
        object::is_dictionary(proto, &heap),
        "actual dictionary holder"
    );
    assert!(
        !slot
            .probe_store(second, &mut heap, key("x"), &Value::null())
            .expect("store allocation")
    );
    assert_eq!(object::get_own(second, &heap, "x"), None);
}

#[test]
fn deep_prototype_proof_is_shared_and_retires_on_shadowing() {
    let mut heap = fresh_heap();
    // SAFETY: this scope belongs to the stationary heap and ends before
    // it; every old fixture cell stays traced through collecting metadata.
    let fixture_roots = unsafe { otter_gc::HandleScope::from_ptr(heap.handle_stack_ptr()) };
    let holder = object::alloc_object_old_for_fixture(&mut heap).unwrap();
    let _holder_root = fixture_roots.local(holder);
    shaped_data_fixture(holder, &mut heap, "x", Value::boolean(true));
    let mut first = holder;
    let mut middle = holder;
    for depth in 0..12 {
        let mut next = object::alloc_object_old_for_fixture(&mut heap).unwrap();
        let _next_root = fixture_roots.local(next);
        assert!(
            object::set_prototype(&mut next, &mut heap, Some(first))
                .expect("fixture prototype transition")
        );
        first = next;
        if depth == 5 {
            middle = next;
        }
    }
    let mut receiver = object::alloc_object_old_for_fixture(&mut heap).unwrap();
    let _receiver_root = fixture_roots.local(receiver);
    let mut sibling = object::alloc_object_old_for_fixture(&mut heap).unwrap();
    let _sibling_root = fixture_roots.local(sibling);
    assert!(
        object::set_prototype(&mut receiver, &mut heap, Some(first))
            .expect("fixture prototype transition")
    );
    assert!(
        object::set_prototype(&mut sibling, &mut heap, Some(first))
            .expect("fixture prototype transition")
    );
    let resolved = crate::cache_ir::resolve_atom_data_slot(receiver, &heap, key("x")).unwrap();
    let shared = crate::cache_ir::resolve_atom_data_slot(sibling, &heap, key("x")).unwrap();
    let old = resolved.validity.clone().unwrap();
    assert!(std::sync::Arc::ptr_eq(
        &old,
        shared.validity.as_ref().unwrap()
    ));
    let slot = PropertyIcSlot::new(PropertyIcKind::Load);
    slot.install(
        IcHandler::load_resolved(object::keyed_shape(receiver, &heap), &resolved).unwrap(),
    );
    assert_eq!(slot.probe_load(receiver, &heap), Some(Value::boolean(true)));
    assert!(
        object::define_own_property_in_place(
            &mut middle,
            &mut heap,
            "x",
            PropertyDescriptor::data(Value::boolean(false), true, true, true)
        )
        .expect("fixture property allocation")
    );
    assert!(!old.is_valid());
    assert_eq!(slot.probe_load(receiver, &heap), None);
    let rebuilt = object::prototype_validity::chain_validity(first, &heap).unwrap();
    assert!(rebuilt.is_valid());
    assert_ne!(old.address(), rebuilt.address());
    assert_eq!(
        object::get(receiver, &heap, "x"),
        Some(Value::boolean(false))
    );
}

#[test]
fn existing_own_store_candidate_rejects_non_writable_data() {
    let mut heap = fresh_heap();
    // SAFETY: this scope belongs to the stationary heap and ends before
    // it; every old fixture cell stays traced through collecting metadata.
    let fixture_roots = unsafe { otter_gc::HandleScope::from_ptr(heap.handle_stack_ptr()) };
    let obj = object::alloc_object_old_for_fixture(&mut heap).unwrap();
    let _obj_root = fixture_roots.local(obj);
    assert!(
        object::define_own_property(
            obj,
            &mut heap,
            "x",
            PropertyDescriptor::data(Value::boolean(true), false, true, true),
        )
        .expect("descriptor fixture allocation")
    );
    assert!(IcHandler::store_existing(obj, &heap, key("x")).is_none());
}

#[test]
fn opaque_lookup_state_rejects_ordinary_slot_attachment_and_replay() {
    let mut heap = fresh_heap();
    // SAFETY: this scope belongs to the stationary heap and ends before
    // it; every old fixture cell stays traced through collecting metadata.
    let fixture_roots = unsafe { otter_gc::HandleScope::from_ptr(heap.handle_stack_ptr()) };
    let mut obj = object::alloc_object_old_for_fixture(&mut heap).unwrap();
    let _obj_root = fixture_roots.local(obj);
    shaped_data_fixture(obj, &mut heap, "x", Value::boolean(true));
    let load = PropertyIcSlot::new(PropertyIcKind::Load);
    load.install(own_load_handler(obj, &heap, "x"));
    let store = PropertyIcSlot::new(PropertyIcKind::Store);
    store.install(IcHandler::store_existing(obj, &heap, key("x")).unwrap());

    // The mapped-arguments producer changes immutable shape state; the
    // retained ordinary handlers cannot authorize that new lookup model.
    let before = object::keyed_shape(obj, &heap);
    install_mapped_lookup(&mut obj, &mut heap);
    assert_ne!(object::keyed_shape(obj, &heap), before);

    assert!(crate::cache_ir::resolve_atom_data_slot(obj, &heap, key("x")).is_none());
    assert!(IcHandler::store_existing(obj, &heap, key("x")).is_none());
    assert_eq!(load.probe_load(obj, &heap), None);
    assert!(
        !store
            .probe_store(obj, &mut heap, key("x"), &Value::boolean(false))
            .expect("store probe")
    );
    assert_eq!(object::get_own(obj, &heap, "x"), Some(Value::boolean(true)));
}

#[test]
fn store_handlers_skip_prototype_role_receivers() {
    let mut heap = fresh_heap();
    // SAFETY: the scope belongs to the stationary heap and ends before it.
    let roots = unsafe { otter_gc::HandleScope::from_ptr(heap.handle_stack_ptr()) };
    let proto = object::alloc_object_old_for_fixture(&mut heap).unwrap();
    let _proto_root = roots.local(proto);
    shaped_data_fixture(proto, &mut heap, "x", Value::boolean(true));
    let mut receiver = object::alloc_object_old_for_fixture(&mut heap).unwrap();
    let _receiver_root = roots.local(receiver);
    assert!(object::set_prototype(&mut receiver, &mut heap, Some(proto)).unwrap());
    assert!(object::state(proto, &heap).is_prototype());
    assert!(IcHandler::store_existing(proto, &heap, key("x")).is_none());
}

fn proof_for_test(
    cell: &std::sync::Arc<object::prototype_validity::PrototypeValidity>,
) -> Option<crate::jit::JitPrototypeValidity> {
    cell.is_valid().then_some(crate::jit::JitPrototypeValidity {
        address: cell.address(),
        identity: cell.identity(),
    })
}

#[test]
fn jit_snapshot_lowers_own_load_and_store_handlers() {
    let mut interpreter = crate::Interpreter::new().expect("fixture interpreter bootstrap");
    let mut obj = object::alloc_object_old_for_fixture(interpreter.gc_heap_mut()).unwrap();
    interpreter
        .create_data_property(&mut obj, "x", Value::boolean(true))
        .unwrap();
    let heap = interpreter.gc_heap();
    let handle = object::keyed_shape(obj, heap);
    assert!(!handle.is_null());
    let atom = heap.read_payload(handle, object::ShapeBody::transition_atom);
    let property = AtomizedPropertyKey::new(PropertyAtom::new(atom), "x");
    let shape = handle.offset();
    let resolved = crate::cache_ir::resolve_atom_data_slot(obj, heap, property).unwrap();
    let load = PropertyIcSlot::new(PropertyIcKind::Load);
    load.install(IcHandler::load_resolved(handle, &resolved).unwrap());
    let programs = load
        .jit_programs(atom.raw(), |s| Some(s.offset()), proof_for_test)
        .expect("own-load snapshot");
    assert_eq!(
        programs[0].ops.as_ref(),
        &[
            JitCacheIrOp::GuardShape { object: 0, shape },
            JitCacheIrOp::GuardAtomSlot {
                object: 0,
                atom: atom.raw(),
                field: object::FieldLocation::inline(0),
                writable: false,
            },
            JitCacheIrOp::LoadField {
                object: 0,
                field: object::FieldLocation::inline(0),
            },
        ]
    );

    let store = PropertyIcSlot::new(PropertyIcKind::Store);
    store.install(IcHandler::store_existing(obj, heap, property).unwrap());
    let programs = store
        .jit_programs(atom.raw(), |s| Some(s.offset()), proof_for_test)
        .expect("own-store snapshot");
    assert_eq!(
        programs[0].ops.as_ref(),
        &[
            JitCacheIrOp::GuardShape { object: 0, shape },
            JitCacheIrOp::GuardAtomSlot {
                object: 0,
                atom: atom.raw(),
                field: object::FieldLocation::inline(0),
                writable: true,
            },
            JitCacheIrOp::StoreField {
                object: 0,
                field: object::FieldLocation::inline(0),
            },
        ]
    );
}

#[test]
fn jit_snapshot_lowers_prototype_handler_through_its_holder_root() {
    let mut interpreter = crate::Interpreter::new().expect("fixture interpreter bootstrap");
    let mut proto = object::alloc_object_old_for_fixture(interpreter.gc_heap_mut()).unwrap();
    interpreter
        .create_data_property(&mut proto, "x", Value::boolean(true))
        .unwrap();
    let mut receiver = object::alloc_object_old_for_fixture(interpreter.gc_heap_mut()).unwrap();
    assert!(
        object::set_prototype(&mut receiver, interpreter.gc_heap_mut(), Some(proto))
            .expect("fixture prototype transition")
    );
    let heap = interpreter.gc_heap();
    let receiver_handle = object::keyed_shape(receiver, heap);
    let holder_root = object::cached_instance_root(proto, heap).unwrap();
    let atom = heap.read_payload(
        object::keyed_shape(proto, heap),
        object::ShapeBody::transition_atom,
    );
    let property = AtomizedPropertyKey::new(PropertyAtom::new(atom), "x");
    let resolved = crate::cache_ir::resolve_atom_data_slot(receiver, heap, property).unwrap();
    let slot = PropertyIcSlot::new(PropertyIcKind::Load);
    slot.install(IcHandler::load_resolved(receiver_handle, &resolved).unwrap());
    assert_eq!(holder_root, receiver_handle, "the empty instance root");
    assert!(
        slot.jit_programs(atom.raw(), |_| None, proof_for_test)
            .is_none(),
        "an unnameable receiver drops the entry"
    );
    let program = slot
        .jit_programs(atom.raw(), |s| Some(s.offset()), proof_for_test)
        .expect("prototype-load snapshot");
    assert_eq!(
        program[0].ops.as_ref(),
        &[
            JitCacheIrOp::GuardShape {
                object: 0,
                shape: receiver_handle.offset(),
            },
            JitCacheIrOp::GuardPrototypeValidity {
                validity: proof_for_test(resolved.validity.as_ref().unwrap()).unwrap()
            },
            JitCacheIrOp::LoadPrototypeHolder {
                root: holder_root.offset(),
                result: 1
            },
            JitCacheIrOp::LoadField {
                object: 1,
                field: object::FieldLocation::inline(0),
            },
        ]
    );
}

#[test]
fn jit_snapshot_preserves_transition_guards_and_publication() {
    let mut interpreter = crate::Interpreter::new().expect("fixture interpreter bootstrap");
    let mut obj = object::alloc_object_old_for_fixture(interpreter.gc_heap_mut()).unwrap();
    assert!(object::set_prototype(&mut obj, interpreter.gc_heap_mut(), None).unwrap());
    interpreter
        .create_data_property(&mut obj, "prefix", Value::boolean(false))
        .unwrap();
    let from_handle = object::keyed_shape(obj, interpreter.gc_heap());
    let mut value = Value::boolean(true);
    let to_handle = interpreter
        .shape_child_rooting_object_value(from_handle, "x", &mut obj, &mut value)
        .unwrap();
    let atom = interpreter
        .gc_heap()
        .read_payload(to_handle, object::ShapeBody::transition_atom);
    let property = AtomizedPropertyKey::new(PropertyAtom::new(atom), "x");
    let transition = object::capture_store_property_transition_with_shape(
        obj,
        interpreter.gc_heap_mut(),
        property,
        &value,
        to_handle,
    )
    .expect("resident own-add transition");
    assert!(matches!(
        transition.kind,
        object::StorePropertyTransitionKind::OwnAdd
    ));
    let slot = PropertyIcSlot::new(PropertyIcKind::Store);
    slot.install(IcHandler::store_transition(from_handle, &transition).unwrap());
    let program = slot
        .jit_programs(atom.raw(), |s| Some(s.offset()), proof_for_test)
        .expect("complete add-transition snapshot");
    assert_eq!(
        program[0].ops.as_ref(),
        &[
            JitCacheIrOp::GuardShape {
                object: 0,
                shape: from_handle.offset(),
            },
            JitCacheIrOp::GuardPrototypeNull { object: 0 },
            JitCacheIrOp::GuardExtensible {
                object: 0,
                field: object::FieldLocation::inline(1),
            },
            JitCacheIrOp::StoreField {
                object: 0,
                field: object::FieldLocation::inline(1),
            },
            JitCacheIrOp::PublishShape {
                object: 0,
                shape: to_handle.offset(),
            },
        ]
    );
}

#[test]
fn full_collection_forgets_entries_naming_dead_shapes() {
    let mut heap = fresh_heap();
    let slot = PropertyIcSlot::new(PropertyIcKind::Load);
    {
        // SAFETY: the scope belongs to the stationary heap and ends here.
        let roots = unsafe { otter_gc::HandleScope::from_ptr(heap.handle_stack_ptr()) };
        let obj = object::alloc_object_old_for_fixture(&mut heap).unwrap();
        let _root = roots.local(obj);
        shaped_data_fixture(obj, &mut heap, "z", Value::null());
        shaped_data_fixture(obj, &mut heap, "own", Value::null());
        let resolved =
            crate::cache_ir::resolve_atom_data_slot(obj, &heap, key("own")).expect("own data");
        slot.install(IcHandler::load_resolved(object::keyed_shape(obj, &heap), &resolved).unwrap());
    }
    assert_eq!(slot.entry_count(), 1);
    let mut no_roots = |_: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {};
    heap.mark_phase(&mut no_roots).expect("mark phase");
    slot.sweep_dead(&heap);
    heap.sweep_phase();
    assert_eq!(slot.entry_count(), 0, "unreached receiver shape pruned");
    assert_eq!(
        slot.inline_shape.load(std::sync::atomic::Ordering::Relaxed),
        super::EMPTY_SHAPE
    );
}
