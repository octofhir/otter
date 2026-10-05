//! Dictionary descriptor identity, integrity and publication regressions.
//!
//! # Contents
//! - Bulk and single-slot structural/layout retirement and repeated no-ops.
//! - Accessor, symbol, Private Name and saturated-epoch semantics.
//! - Proof retirement before descriptor publication and prototype invalidation.
//!
//! # Invariants
//! - Fixtures use old owners and immediate values; no moving value crosses an
//!   allocation without its object storage owner.
//! - Assertions distinguish whole structural identity from watched-slot epochs.
//! - Capacity and slot banks remain those of the original immutable shape.
//!
//! # See also
//! - `super::DescriptorChanges` is the production retirement owner.
//! - `crate::object::persistent_fields_tests` proves real child relocation.

use super::DescriptorChanges;
use crate::Value;
use crate::object::*;
use otter_gc::GcHeap;

fn dictionary_fixture(capacity: usize) -> (GcHeap, JsObject, usize) {
    let mut heap = GcHeap::new().expect("heap");
    let head = fixture_root_shape(&mut heap).expect("fixture root");
    let root = shape_body::alloc_root_shape_body_with_roots(
        &mut heap,
        shape_body::ShapePrototype::Null,
        capacity,
        head,
        ShapeState::ORDINARY,
        &mut |_| {},
    )
    .expect("capacity root");
    shape_body::set_null_root(&mut heap, root);
    let mut object = alloc_object_body_old(&mut heap, empty_object_body(root)).expect("old owner");
    let count = capacity + 2;
    for index in 0..count {
        assert!(
            define_own_property_in_place(
                &mut object,
                &mut heap,
                &format!("p{index}"),
                PropertyDescriptor::data(Value::number_i32(index as i32), true, true, true)
            )
            .expect("fixture property allocation")
        );
    }
    assert!(is_dictionary(object, &heap));
    (heap, object, count)
}

fn identities(heap: &GcHeap, object: JsObject) -> (ShapeId, u32) {
    heap.read_payload(object, |body| {
        (body.dictionary_shape_id(), body.dictionary_layout())
    })
}

fn watch(heap: &mut GcHeap, object: JsObject, index: usize) {
    assert!(watch_dictionary_slot(object, heap, index as u16));
}

fn watched(heap: &GcHeap, object: JsObject, index: usize) -> bool {
    heap.read_payload(object, |body| body.slots()[index].watched)
}

#[test]
fn bulk_freeze_retires_each_domain_once_across_both_field_banks() {
    for capacity in [0, 4, 64] {
        let (mut heap, mut object, count) = dictionary_fixture(capacity);
        watch(&mut heap, object, 0);
        watch(&mut heap, object, count - 1);
        let before = identities(&heap, object);
        let fields: Vec<_> = (0..count)
            .map(|index| field_location_at(object, &heap, index as u32))
            .collect();

        freeze(&mut object, &mut heap).expect("freeze fixture");

        let after = identities(&heap, object);
        assert_ne!(after.0, before.0, "whole descriptor identity retired");
        assert_eq!(after.1, before.1 + 1, "one retirement for multiple watches");
        assert!(!watched(&heap, object, 0));
        assert!(!watched(&heap, object, count - 1));
        assert!(is_frozen(object, &heap));
        for (index, field) in fields.into_iter().enumerate() {
            assert_eq!(field_location_at(object, &heap, index as u32), field);
            assert_eq!(
                get_own(object, &heap, &format!("p{index}")),
                Some(Value::number_i32(index as i32))
            );
            assert!(matches!(
                resolve_set(object, &heap, &format!("p{index}")),
                SetOutcome::Reject {
                    reason: SetRejectReason::NonWritable
                }
            ));
        }
        heap.read_payload(object, |body| {
            body.debug_verify_field_layout();
            assert_eq!(body.inline_capacity(), capacity);
        });

        watch(&mut heap, object, 0);
        freeze(&mut object, &mut heap).expect("freeze fixture");
        seal(&mut object, &mut heap).expect("seal fixture");
        assert_eq!(
            identities(&heap, object),
            after,
            "equal integrity is a no-op"
        );
        assert!(
            watched(&heap, object, 0),
            "unchanged watched descriptor survives"
        );
    }
}

#[test]
fn sealed_writable_slots_keep_legal_writes_and_freeze_retires_their_proof() {
    let (mut heap, mut object, _) = dictionary_fixture(4);
    watch(&mut heap, object, 0);
    let initial = identities(&heap, object);
    seal(&mut object, &mut heap).expect("seal fixture");
    let sealed = identities(&heap, object);
    assert_ne!(sealed.0, initial.0);
    assert_eq!(sealed.1, initial.1 + 1);
    let descriptor = get_own_descriptor(object, &heap, "p0").expect("sealed slot");
    assert!(descriptor.writable());
    assert!(!descriptor.configurable());
    assert!(!delete(&mut object, &mut heap, "p0").expect("delete fixture"));

    watch(&mut heap, object, 0);
    seal(&mut object, &mut heap).expect("seal fixture");
    assert!(
        ordinary_set_data_property(&mut object, &mut heap, "p0", Value::number_i32(719))
            .expect("fixture assignment allocation")
    );
    assert_eq!(identities(&heap, object), sealed);
    assert!(watched(&heap, object, 0));
    assert_eq!(get_own(object, &heap, "p0"), Some(Value::number_i32(719)));

    freeze(&mut object, &mut heap).expect("freeze fixture");
    let frozen = identities(&heap, object);
    assert_ne!(frozen.0, sealed.0);
    assert_eq!(frozen.1, sealed.1 + 1);
    assert!(
        !ordinary_set_data_property(&mut object, &mut heap, "p0", Value::number_i32(999))
            .expect("fixture assignment allocation")
    );
    assert_eq!(get_own(object, &heap, "p0"), Some(Value::number_i32(719)));
}

#[test]
fn unwatched_descriptor_change_preserves_an_unchanged_slot_epoch() {
    let (mut heap, mut object, _) = dictionary_fixture(4);
    assert!(
        define_own_property(
            object,
            &mut heap,
            "p0",
            PropertyDescriptor::data(Value::number_i32(0), false, true, false),
        )
        .expect("descriptor fixture allocation")
    );
    watch(&mut heap, object, 0);
    let before = identities(&heap, object);
    freeze(&mut object, &mut heap).expect("freeze fixture");
    let after = identities(&heap, object);
    assert_ne!(after.0, before.0, "unwatched descriptors changed");
    assert_eq!(after.1, before.1, "watched descriptor was already frozen");
    assert!(watched(&heap, object, 0));
    assert!(is_frozen(object, &heap));
}

#[test]
fn single_slot_value_and_kind_updates_share_the_retirement_owner() {
    let (mut heap, object, _) = dictionary_fixture(4);
    watch(&mut heap, object, 0);
    let before = identities(&heap, object);
    assert!(
        define_own_property(
            object,
            &mut heap,
            "p0",
            PropertyDescriptor::data(Value::number_i32(21), true, true, true),
        )
        .expect("descriptor fixture allocation")
    );
    assert_eq!(identities(&heap, object), before);
    assert!(watched(&heap, object, 0));
    assert_eq!(get_own(object, &heap, "p0"), Some(Value::number_i32(21)));

    assert!(
        define_own_property(
            object,
            &mut heap,
            "p0",
            PropertyDescriptor::accessor(None, None, true, true),
        )
        .expect("descriptor fixture allocation")
    );
    let accessor = identities(&heap, object);
    assert_ne!(accessor.0, before.0);
    assert_eq!(accessor.1, before.1 + 1);
    assert!(!watched(&heap, object, 0));
    assert!(!watch_dictionary_slot(object, &mut heap, 0));
    assert!(matches!(
        get_own_descriptor(object, &heap, "p0")
            .expect("accessor")
            .kind,
        DescriptorKind::Accessor {
            getter: None,
            setter: None
        }
    ));

    assert!(
        define_own_property(
            object,
            &mut heap,
            "p0",
            PropertyDescriptor::data(Value::number_i32(31), true, true, true),
        )
        .expect("descriptor fixture allocation")
    );
    let data = identities(&heap, object);
    assert_ne!(data.0, accessor.0);
    assert_eq!(data.1, accessor.1, "no direct accessor slot proof existed");
}

#[test]
fn integrity_preserves_accessors_private_names_and_unchanged_string_watches() {
    let mut heap = GcHeap::new().expect("heap");
    // The owner's string descriptor is already frozen, so only symbol flags
    // change when the integrity level is published.
    let mut other = alloc_object_old_for_fixture(&mut heap).expect("old symbol owner");
    assert!(
        define_own_property(
            other,
            &mut heap,
            "stable",
            PropertyDescriptor::data(Value::number_i32(7), false, true, false),
        )
        .expect("descriptor fixture allocation")
    );
    let data = JsSymbol::new(&mut heap, None).expect("symbol data key");
    let accessor = JsSymbol::new(&mut heap, None).expect("symbol accessor key");
    let private = JsSymbol::new_private(&mut heap, None).expect("private key");
    assert!(
        define_own_symbol_property_partial(
            &mut other,
            &mut heap,
            data,
            PartialPropertyDescriptor::from_full(&PropertyDescriptor::data(
                Value::number_i32(8),
                true,
                true,
                true
            )),
        )
        .expect("descriptor fixture allocation")
    );
    assert!(
        define_own_symbol_property_partial(
            &mut other,
            &mut heap,
            accessor,
            PartialPropertyDescriptor::from_full(&PropertyDescriptor::accessor(
                None, None, true, true,
            )),
        )
        .expect("descriptor fixture allocation")
    );
    assert!(
        define_own_symbol_property_partial(
            &mut other,
            &mut heap,
            private,
            PartialPropertyDescriptor::from_full(&PropertyDescriptor::data(
                Value::number_i32(9),
                true,
                true,
                true
            )),
        )
        .expect("descriptor fixture allocation")
    );
    let initial_accessor =
        get_own_symbol_descriptor(other, &heap, accessor).expect("initial symbol accessor");
    assert!(matches!(
        initial_accessor.kind,
        DescriptorKind::Accessor {
            getter: None,
            setter: None
        }
    ));
    assert!(initial_accessor.enumerable() && initial_accessor.configurable());
    assert!(!initial_accessor.writable());
    watch(&mut heap, other, 0);
    let before = identities(&heap, other);
    freeze(&mut other, &mut heap).expect("freeze fixture");
    let after = identities(&heap, other);
    assert_ne!(
        after.0, before.0,
        "symbol integrity changes retire structural identity"
    );
    assert_eq!(
        after.1, before.1,
        "symbol changes do not redefine the watched string"
    );
    assert!(watched(&heap, other, 0));
    let symbol = get_own_symbol_descriptor(other, &heap, data).expect("symbol data");
    assert!(!symbol.writable() && !symbol.configurable() && symbol.enumerable());
    let symbol = get_own_symbol_descriptor(other, &heap, accessor).expect("symbol accessor");
    assert!(matches!(
        symbol.kind,
        DescriptorKind::Accessor {
            getter: None,
            setter: None
        }
    ));
    assert!(!symbol.writable() && !symbol.configurable() && symbol.enumerable());
    let symbol = get_own_symbol_descriptor(other, &heap, private).expect("private data");
    assert_eq!(symbol.flags, PropertyFlags::data_default());
    assert!(matches!(symbol.kind, DescriptorKind::Data { value } if value == Value::number_i32(9)));
    assert!(
        is_frozen(other, &heap),
        "private flags do not affect integrity"
    );
    freeze(&mut other, &mut heap).expect("freeze fixture");
    assert_eq!(identities(&heap, other), after);
}

#[test]
fn string_accessor_integrity_never_rewrites_its_kind_or_accessor_pair() {
    let (mut heap, mut object, _) = dictionary_fixture(4);
    assert!(
        define_own_property(
            object,
            &mut heap,
            "p0",
            PropertyDescriptor::accessor(None, None, false, true),
        )
        .expect("descriptor fixture allocation")
    );
    let before = identities(&heap, object);
    freeze(&mut object, &mut heap).expect("freeze fixture");
    let after = identities(&heap, object);
    assert_ne!(after.0, before.0);
    assert_eq!(after.1, before.1, "no slot was watched");
    let descriptor = get_own_descriptor(object, &heap, "p0").expect("accessor");
    assert!(matches!(
        descriptor.kind,
        DescriptorKind::Accessor {
            getter: None,
            setter: None
        }
    ));
    assert!(!descriptor.writable() && !descriptor.enumerable() && !descriptor.configurable());
    assert!(matches!(
        resolve_set(object, &heap, "p0"),
        SetOutcome::Reject {
            reason: SetRejectReason::AccessorWithoutSetter
        }
    ));
}

#[test]
fn saturated_layout_epochs_never_become_provable_again() {
    let (mut heap, mut object, _) = dictionary_fixture(4);
    heap.with_payload(object, |body| {
        body.exotic_mut().dictionary_layout = u32::MAX - 1
    });
    watch(&mut heap, object, 0);
    seal(&mut object, &mut heap).expect("seal fixture");
    assert_eq!(identities(&heap, object).1, u32::MAX);
    assert_eq!(dictionary_layout(object, &heap), None);
    watch(&mut heap, object, 0);
    freeze(&mut object, &mut heap).expect("freeze fixture");
    assert_eq!(identities(&heap, object).1, u32::MAX);
    assert_eq!(dictionary_layout(object, &heap), None);

    let (mut heap, object, _) = dictionary_fixture(4);
    heap.with_payload(object, |body| {
        body.exotic_mut().dictionary_layout = u32::MAX
    });
    watch(&mut heap, object, 0);
    assert!(
        define_own_property(
            object,
            &mut heap,
            "p0",
            PropertyDescriptor::accessor(None, None, true, true),
        )
        .expect("descriptor fixture allocation")
    );
    assert_eq!(identities(&heap, object).1, u32::MAX);
    assert_eq!(dictionary_layout(object, &heap), None);
}

#[test]
fn proof_retirement_precedes_descriptor_and_value_publication() {
    let (mut heap, object, _) = dictionary_fixture(4);
    watch(&mut heap, object, 0);
    let before = identities(&heap, object);
    heap.with_payload(object, |body| {
        let previous = body.slots()[0];
        let next = SlotMeta {
            flags: previous.flags.with_writable(false),
            ..previous
        };
        DescriptorChanges::for_slot(previous, next).retire(body);
        assert_ne!(body.dictionary_shape_id(), before.0);
        assert_eq!(body.dictionary_layout(), before.1 + 1);
        assert_eq!(
            body.slots()[0].flags,
            previous.flags,
            "descriptor is not published yet"
        );
        assert_eq!(
            body.slot_word(0),
            Value::number_i32(0),
            "value is not published yet"
        );
    });
}

#[test]
fn changed_integrity_invalidates_prototypes_but_repeated_noops_preserve_proofs() {
    let (mut heap, mut object, _) = dictionary_fixture(4);
    let root = shape_body::alloc_root_shape_body_with_roots(
        &mut heap,
        shape_body::ShapePrototype::Object(object),
        4,
        ShapeHandle::null(),
        ShapeState::ORDINARY,
        &mut |_| {},
    )
    .expect("instance root");
    cache_instance_root(&mut object, &mut heap, root).expect("prototype sidecar");
    let initial = prototype_validity::chain_validity(object, &heap).expect("prototype proof");
    assert!(initial.is_valid());
    freeze(&mut object, &mut heap).expect("freeze fixture");
    assert!(!initial.is_valid());
    let current = prototype_validity::chain_validity(object, &heap).expect("new proof");
    assert!(current.is_valid());
    freeze(&mut object, &mut heap).expect("freeze fixture");
    seal(&mut object, &mut heap).expect("seal fixture");
    assert!(current.is_valid(), "no semantic mutation occurred");
    assert!(!initial.is_valid(), "retired proof never becomes valid");
}
