//! Field-presence-aware ordinary descriptor validation.
//!
//! This private submodule keeps the production descriptor algorithms out of the
//! already-large `object.rs` surface while staying inside the object module's
//! storage boundary.
//!
//! # Contents
//! - [`validate_and_apply_partial`] — field-presence-aware slot validation.
//! - [`validate_descriptor_partial`] — the same validation for array symbols.
//!
//! # Invariants
//! - Validation preserves every absent incoming field.
//! - Rejection performs no mutation and never allocates.
//! - Publication belongs solely to `super::descriptor_install`.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-ordinarysetwithowndescriptor>
//! - <https://tc39.es/ecma262/#sec-validateandapplypropertydescriptor>

use crate::Value;

use super::{
    DescriptorKind, PartialPropertyDescriptor, PropertyDescriptor, PropertyFlags, SlotData,
    SlotKind,
};

/// Implements §10.1.6.3 ValidateAndApplyPropertyDescriptor for an existing
/// slot. Field-presence aware: a missing field in `incoming` means
/// "preserve existing", not "default to false".
pub(super) fn validate_and_apply_partial(
    existing: &SlotData,
    incoming: &PartialPropertyDescriptor,
    heap: &otter_gc::GcHeap,
) -> Option<SlotData> {
    let existing_is_data = existing.kind.is_data();
    let incoming_is_accessor = incoming.is_accessor();
    let incoming_is_data = incoming.is_data();

    if !existing.flags.configurable() {
        // Step 4.a: configurable cannot transition to true.
        if matches!(incoming.configurable, Some(true)) {
            return None;
        }
        // Step 4.b: enumerable cannot change.
        if let Some(en) = incoming.enumerable
            && en != existing.flags.enumerable()
        {
            return None;
        }
        // Step 4.c: kind cannot flip when present.
        if incoming_is_accessor && existing_is_data {
            return None;
        }
        if incoming_is_data && !existing_is_data {
            return None;
        }
        // Step 4.d/e: data, non-writable — restrict writable→true and
        // value changes.
        if existing_is_data && incoming_is_data && !existing.flags.writable() {
            if matches!(incoming.writable, Some(true)) {
                return None;
            }
            if let Some(in_v) = &incoming.value
                && !same_value(&existing.value, in_v, heap)
            {
                return None;
            }
        }
        // Step 4.f: accessor — get/set cannot change.
        if !existing_is_data
            && incoming_is_accessor
            && let SlotKind::Accessor(pair) = &existing.kind
        {
            if incoming.get.is_some() && !optional_value_eq(&pair.getter, &incoming.get, heap) {
                return None;
            }
            if incoming.set.is_some() && !optional_value_eq(&pair.setter, &incoming.set, heap) {
                return None;
            }
        }
    }

    // Build merged slot — start from existing, apply present fields.
    let mut configurable = existing.flags.configurable();
    let mut enumerable = existing.flags.enumerable();
    let mut writable = existing.flags.writable();
    if let Some(c) = incoming.configurable {
        configurable = c;
    }
    if let Some(e) = incoming.enumerable {
        enumerable = e;
    }
    let kind = if incoming_is_accessor || (!incoming_is_data && !existing_is_data) {
        // Accessor descriptors never carry the data-only writable bit.
        // This applies to data-to-accessor and partial accessor updates alike.
        writable = false;
        let (mut getter, mut setter) = match &existing.kind {
            SlotKind::Accessor(pair) => (pair.getter, pair.setter),
            SlotKind::Data => (None, None),
        };
        if let Some(g) = &incoming.get {
            getter = if g.is_undefined() { None } else { Some(*g) };
        }
        if let Some(s) = &incoming.set {
            setter = if s.is_undefined() { None } else { Some(*s) };
        }
        DescriptorKind::Accessor { getter, setter }
    } else {
        // Data descriptor.
        let mut value = if existing.kind.is_data() {
            existing.value
        } else {
            Value::undefined()
        };
        if let Some(v) = &incoming.value {
            value = *v;
        }
        if let Some(w) = incoming.writable {
            writable = w;
        } else if !existing_is_data {
            // Transitioning accessor → data: writable defaults to false
            // per §10.1.6.3 step 5.b.
            writable = false;
        }
        DescriptorKind::Data { value }
    };
    Some(SlotData::from_descriptor(PropertyDescriptor {
        kind,
        flags: PropertyFlags::new(writable, enumerable, configurable),
    }))
}

/// Backwards-compatible wrapper that takes a full
/// [`PropertyDescriptor`]. Treats every field as present.
pub(super) fn validate_and_apply(
    existing: &SlotData,
    incoming: &PropertyDescriptor,
    heap: &otter_gc::GcHeap,
) -> Option<SlotData> {
    let existing_kind_is_data = existing.kind.is_data();
    let incoming_kind_is_data = matches!(incoming.kind, DescriptorKind::Data { .. });

    // 4.a: every field of `incoming` is identical to `existing` →
    // no-op success. Skipped for simplicity — we always apply.

    if !existing.flags.configurable() {
        // 4.b: configurable cannot transition to true.
        if incoming.flags.configurable() && !existing.flags.configurable() {
            return None;
        }
        // 4.c: enumerable cannot change.
        if incoming.flags.enumerable() != existing.flags.enumerable() {
            return None;
        }
        // 4.d: kind cannot change (data ↔ accessor).
        if existing_kind_is_data != incoming_kind_is_data {
            return None;
        }
        // 4.e: data with non-writable rejects writable→true / value change.
        if existing_kind_is_data {
            if !existing.flags.writable() {
                if incoming.flags.writable() {
                    return None;
                }
                if let DescriptorKind::Data { value: incoming_v } = &incoming.kind
                    && !same_value(&existing.value, incoming_v, heap)
                {
                    return None;
                }
            }
        } else {
            // 4.f: accessor — get / set cannot change.
            if let DescriptorKind::Accessor {
                getter: in_get,
                setter: in_set,
            } = &incoming.kind
                && let SlotKind::Accessor(pair) = &existing.kind
                && (!optional_value_eq(&pair.getter, in_get, heap)
                    || !optional_value_eq(&pair.setter, in_set, heap))
            {
                return None;
            }
        }
    }

    // Build merged slot.
    Some(SlotData::from_descriptor(PropertyDescriptor {
        flags: incoming.flags,
        kind: incoming.kind.clone(),
    }))
}

pub(super) fn validate_descriptor_update(
    existing: &PropertyDescriptor,
    incoming: &PropertyDescriptor,
    heap: &otter_gc::GcHeap,
) -> Option<PropertyDescriptor> {
    let existing = SlotData::from_descriptor(existing.clone());
    validate_and_apply(&existing, incoming, heap).map(|slot| slot.to_descriptor())
}

/// Validate a present partial descriptor against one stored descriptor.
/// Array symbol properties share this exact ordinary validation authority.
pub(crate) fn validate_descriptor_partial(
    existing: &PropertyDescriptor,
    incoming: &PartialPropertyDescriptor,
    heap: &otter_gc::GcHeap,
) -> Option<PropertyDescriptor> {
    let existing = SlotData::from_descriptor(existing.clone());
    validate_and_apply_partial(&existing, incoming, heap).map(|slot| slot.to_descriptor())
}

fn optional_value_eq(a: &Option<Value>, b: &Option<Value>, heap: &otter_gc::GcHeap) -> bool {
    // §10.1.6.3 — a missing accessor side (`[[Get]]` / `[[Set]]`) is
    // spec-defined to be `undefined`, so a stored `None` slot
    // compares SameValue-equal to an incoming explicit
    // `Value::Undefined`. Anything else falls back to SameValue.
    match (a, b) {
        (None, None) => true,
        (None, Some(v)) | (Some(v), None) => v.is_undefined(),
        (Some(x), Some(y)) => same_value(x, y, heap),
    }
}

fn same_value(a: &Value, b: &Value, heap: &otter_gc::GcHeap) -> bool {
    crate::abstract_ops::same_value(a, b, heap)
}

#[cfg(test)]
mod tests;
