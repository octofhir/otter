//! Managed named-property storage and canonical Array key classification.
//!
//! # Contents
//! - Pure length/index/expando classification and current bag projection.
//! - Sorted present own indices shared by descriptors, keys and integrity.
//!
//! # Invariants
//! - The bag is storage only; accessors and Set retain the original Array receiver.
//! - Canonical indices never enter the bag, and holes never count as own keys.
//! - Readers do not allocate VM values or reenter JavaScript.
//!
//! # See also
//! - [`crate::handles`] owns scoped bag birth and attachment.
//! - [`crate::object_internal_ops`] owns descriptor and completion semantics.

use super::JsArray;
use crate::{VmPropertyKey, object::JsObject};
use otter_gc::GcHeap;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ArrayPropertyKey {
    Length,
    Index(u32),
    Expando,
}

pub(crate) fn classify_property_key(key: &VmPropertyKey<'_>) -> ArrayPropertyKey {
    match key.string_name() {
        Some("length") => ArrayPropertyKey::Length,
        Some(name) => crate::object::array_index_property_name(name)
            .map_or(ArrayPropertyKey::Expando, ArrayPropertyKey::Index),
        None => ArrayPropertyKey::Expando,
    }
}

pub(crate) fn expando_object(array: JsArray, heap: &GcHeap) -> Option<JsObject> {
    heap.read_payload(array, |body| {
        (!body.expando.is_null()).then_some(body.expando)
    })
}

pub(crate) fn own_indices(array: JsArray, heap: &GcHeap) -> Vec<u32> {
    heap.read_payload(array, |body| {
        let mut indices = Vec::new();
        for index in 0..body.dense_len() {
            if body
                .dense_value(index)
                .is_some_and(|value| !value.is_hole())
            {
                indices.push(index as u32);
            }
        }
        if let Some(sparse) = body.sparse_elements() {
            indices.extend(sparse.keys().map(|&index| index as u32));
        }
        if let Some(accessors) = body.accessors() {
            indices.extend(accessors.keys().copied());
        }
        indices.sort_unstable();
        indices.dedup();
        indices
    })
}

#[cfg(test)]
mod tests;
