//! Ordinary assignment gates over the sole descriptor publication owner.
//!
//! # Contents
//! - String-keyed writes with optional prepared append shapes.
//! - Symbol-keyed writes with the same descriptor rejection contract.
//!
//! # Invariants
//! - `Ok(false)` means an ECMAScript assignment rejection, never actual OOM.
//! - Existing writable data preserves attributes; accessors reject this data half.
//! - Missing keys use ordinary default data attributes only on extensible objects.
//! - Descriptor installation roots all pending receiver/key/value/shape slots.
//! - Construction uses CreateDataProperty through descriptor installation directly.
//!
//! # See also
//! - `super::descriptor_install` owns preparation and noncollecting publication.
//! - `super::lookup` describes the preceding receiver/prototype resolver.

use super::*;

fn descriptor(lookup: PropertyLookup, value: Value) -> Option<PartialPropertyDescriptor> {
    match lookup {
        PropertyLookup::Absent => Some(PartialPropertyDescriptor::from_full(
            &PropertyDescriptor::data(value, true, true, true),
        )),
        PropertyLookup::Data { flags, .. } if flags.writable() => Some(PartialPropertyDescriptor {
            value: Some(value),
            ..PartialPropertyDescriptor::default()
        }),
        _ => None,
    }
}

pub(super) fn string(
    object: &mut JsObject,
    heap: &mut GcHeap,
    key: &str,
    value: Value,
    next_shape: Option<ShapeHandle>,
) -> Result<bool, otter_gc::OutOfMemory> {
    let Some(descriptor) = descriptor(lookup_own(*object, heap, key), value) else {
        return Ok(false);
    };
    if let Some(shape) = next_shape {
        descriptor_install::define_string_with_shape(object, heap, key, descriptor, shape)
    } else {
        descriptor_install::define_string(object, heap, key, descriptor)
    }
}

pub(super) fn symbol(
    object: &mut JsObject,
    heap: &mut GcHeap,
    key: JsSymbol,
    value: Value,
) -> Result<bool, otter_gc::OutOfMemory> {
    let Some(descriptor) = descriptor(lookup_own_symbol(*object, heap, key), value) else {
        return Ok(false);
    };
    descriptor_install::define_symbol(object, heap, key, descriptor)
}

#[cfg(test)]
mod tests;
