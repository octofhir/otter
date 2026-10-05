//! Accessor flag normalization through the actual descriptor installer.
//!
//! # Contents
//! - Data-to-accessor conversion clears the data-only writable bit.
//! - Partial accessor updates preserve omitted getter/setter and attributes.
//!
//! # Invariants
//! - Assertions read the published descriptor, not a validator-only result.
//! - Empty accessor pairs remain accessors before and after integrity changes.
//!
//! # See also
//! - `super::validate_and_apply_partial` owns the field-presence semantics.

use crate::Value;
use crate::object::{self, DescriptorKind, PartialPropertyDescriptor, PropertyDescriptor};

#[test]
fn writable_data_to_empty_accessor_publishes_only_accessor_attributes() {
    let mut heap = object::fixture_heap();
    let mut object = object::alloc_object_old_for_fixture(&mut heap).expect("old owner");
    assert!(
        object::define_own_property_in_place(
            &mut object,
            &mut heap,
            "field",
            PropertyDescriptor::data(Value::number_i32(11), true, true, true),
        )
        .expect("data descriptor")
    );
    assert!(
        object::define_own_property_partial(
            &mut object,
            &mut heap,
            "field",
            PartialPropertyDescriptor {
                get: Some(Value::undefined()),
                set: Some(Value::undefined()),
                ..PartialPropertyDescriptor::default()
            },
        )
        .expect("partial accessor conversion")
    );
    let descriptor = object::get_own_descriptor(object, &heap, "field").expect("published field");
    assert!(matches!(
        descriptor.kind,
        DescriptorKind::Accessor {
            getter: None,
            setter: None
        }
    ));
    assert!(!descriptor.writable());
    assert!(descriptor.enumerable());
    assert!(descriptor.configurable());
    object::freeze(&mut object, &mut heap).expect("freeze accessor");
    let descriptor = object::get_own_descriptor(object, &heap, "field").unwrap();
    assert!(matches!(
        descriptor.kind,
        DescriptorKind::Accessor {
            getter: None,
            setter: None
        }
    ));
    assert!(!descriptor.writable());
    assert!(descriptor.enumerable());
    assert!(!descriptor.configurable());
}

#[test]
fn partial_accessor_updates_preserve_pair_presence_and_clear_data_writability() {
    let mut heap = object::fixture_heap();
    let mut object = object::alloc_object_old_for_fixture(&mut heap).expect("old owner");
    let getter = Value::function(17);
    let setter = Value::function(18);
    assert!(
        object::define_own_property_in_place(
            &mut object,
            &mut heap,
            "field",
            PropertyDescriptor::accessor(Some(getter), Some(setter), true, true,),
        )
        .expect("accessor pair")
    );
    assert!(
        object::define_own_property_partial(
            &mut object,
            &mut heap,
            "field",
            PartialPropertyDescriptor {
                enumerable: Some(false),
                ..PartialPropertyDescriptor::default()
            },
        )
        .expect("generic partial attribute update")
    );
    let descriptor = object::get_own_descriptor(object, &heap, "field").unwrap();
    assert!(
        matches!(descriptor.kind, DescriptorKind::Accessor { getter: Some(g), setter: Some(s) }
        if g == getter && s == setter)
    );
    assert!(!descriptor.writable());
    assert!(!descriptor.enumerable());
    assert!(descriptor.configurable());
    assert!(
        object::define_own_property_partial(
            &mut object,
            &mut heap,
            "field",
            PartialPropertyDescriptor {
                set: Some(Value::undefined()),
                ..PartialPropertyDescriptor::default()
            },
        )
        .expect("explicit undefined clears only setter")
    );
    let descriptor = object::get_own_descriptor(object, &heap, "field").unwrap();
    assert!(
        matches!(descriptor.kind, DescriptorKind::Accessor { getter: Some(g), setter: None } if g == getter)
    );
    assert!(!descriptor.writable());
    assert!(!descriptor.enumerable());
    assert!(descriptor.configurable());
}
