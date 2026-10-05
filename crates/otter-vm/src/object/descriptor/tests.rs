//! Fully specified descriptor conversion and accessor replacement regressions.
//!
//! # Contents
//! - All getter/setter presence combinations and descriptor attribute roundtrips.
//! - Replacement of existing accessor slots through the ordinary property owner.
//!
//! # Invariants
//! - Stored `None` accessor slots mean `undefined`, while partial `None` means
//!   omitted; converting a full descriptor must preserve this distinction.
//! - Function IDs are immediate values; fixtures never retain a moving child
//!   across an allocation or invoke their synthetic callable identities.
//!
//! # See also
//! - `super::PartialPropertyDescriptor::from_full` owns the conversion.
//! - `crate::object::descriptor_mutation::tests` covers symbol integrity changes.

use super::{DescriptorKind, PartialPropertyDescriptor, PropertyDescriptor};
use crate::Value;
use crate::object::{
    alloc_object_old_for_fixture, define_own_property_partial, get_own_descriptor,
};
use otter_gc::GcHeap;

fn assert_accessor(descriptor: &PropertyDescriptor, getter: Option<Value>, setter: Option<Value>) {
    match descriptor.kind {
        DescriptorKind::Accessor {
            getter: actual_getter,
            setter: actual_setter,
        } => {
            assert_eq!(actual_getter, getter);
            assert_eq!(actual_setter, setter);
        }
        DescriptorKind::Data { .. } => panic!("accessor conversion changed descriptor kind"),
    }
    assert!(!descriptor.writable());
}

#[test]
fn full_accessor_roundtrip_preserves_all_four_slot_presence_combinations() {
    for getter in [None, Some(Value::function(17))] {
        for setter in [None, Some(Value::function(29))] {
            for enumerable in [false, true] {
                for configurable in [false, true] {
                    let full =
                        PropertyDescriptor::accessor(getter, setter, enumerable, configurable);
                    let partial = PartialPropertyDescriptor::from_full(&full);
                    assert!(partial.is_accessor());
                    assert!(!partial.is_generic() && !partial.is_data());
                    assert_eq!(partial.get, Some(getter.unwrap_or(Value::undefined())));
                    assert_eq!(partial.set, Some(setter.unwrap_or(Value::undefined())));
                    assert_eq!(partial.value, None);
                    assert_eq!(partial.writable, None);
                    assert_eq!(partial.enumerable, Some(enumerable));
                    assert_eq!(partial.configurable, Some(configurable));
                    let completed = partial.complete_for_new_property();
                    assert_accessor(&completed, getter, setter);
                    assert_eq!(completed.flags, full.flags);
                }
            }
        }
    }
}

#[test]
fn full_data_roundtrip_preserves_value_and_all_attribute_combinations() {
    for value in [Value::undefined(), Value::number_i32(31)] {
        for writable in [false, true] {
            for enumerable in [false, true] {
                for configurable in [false, true] {
                    let full = PropertyDescriptor::data(value, writable, enumerable, configurable);
                    let partial = PartialPropertyDescriptor::from_full(&full);
                    assert!(partial.is_data());
                    assert!(!partial.is_generic() && !partial.is_accessor());
                    assert_eq!(partial.value, Some(value));
                    assert_eq!(partial.writable, Some(writable));
                    assert_eq!(partial.get, None);
                    assert_eq!(partial.set, None);
                    let completed = partial.complete_for_new_property();
                    assert!(
                        matches!(completed.kind, DescriptorKind::Data { value: actual } if actual == value)
                    );
                    assert_eq!(completed.flags, full.flags);
                }
            }
        }
    }
}

#[test]
fn full_accessor_update_clears_both_or_either_existing_slot() {
    let mut heap = GcHeap::new().expect("heap");
    let mut object = alloc_object_old_for_fixture(&mut heap).expect("old owner");
    let old_getter = Value::function(41);
    let old_setter = Value::function(43);
    for getter in [None, Some(Value::function(47))] {
        for setter in [None, Some(Value::function(53))] {
            let initial =
                PropertyDescriptor::accessor(Some(old_getter), Some(old_setter), true, true);
            assert!(
                define_own_property_partial(
                    &mut object,
                    &mut heap,
                    "accessor",
                    PartialPropertyDescriptor::from_full(&initial),
                )
                .expect("descriptor fixture allocation")
            );
            let before = get_own_descriptor(object, &heap, "accessor").expect("initial accessor");
            assert_accessor(&before, Some(old_getter), Some(old_setter));

            let replacement = PropertyDescriptor::accessor(getter, setter, false, true);
            assert!(
                define_own_property_partial(
                    &mut object,
                    &mut heap,
                    "accessor",
                    PartialPropertyDescriptor::from_full(&replacement),
                )
                .expect("descriptor fixture allocation")
            );
            let after = get_own_descriptor(object, &heap, "accessor").expect("updated accessor");
            assert_accessor(&after, getter, setter);
            assert_eq!(after.flags, replacement.flags);
        }
    }
}
