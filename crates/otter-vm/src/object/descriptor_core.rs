//! Descriptor validation and ordinary data-assignment core.
//!
//! This private submodule keeps the production descriptor algorithms out of the
//! already-large `object.rs` surface while staying inside the object module's
//! storage boundary.
//!
//! # Contents
//! - [`ordinary_set_data_property`] — the string-keyed data-write half of
//!   ordinary `[[Set]]`.
//! - [`ordinary_set_symbol_data_property`] — the symbol-keyed data-write half.
//! - [`validate_and_apply`] — `ValidateAndApplyPropertyDescriptor` for an
//!   existing ordinary string-keyed slot.
//!
//! # Invariants
//! - Runtime `[[Set]]` data writes preserve an existing data descriptor's
//!   `enumerable` / `configurable` bits and never overwrite accessors.
//! - New ordinary data properties are installed with the default
//!   writable/enumerable/configurable triple only when the receiver is
//!   extensible.
//! - Successful stores record the object write barrier after the payload update.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-ordinarysetwithowndescriptor>
//! - <https://tc39.es/ecma262/#sec-validateandapplypropertydescriptor>

use crate::Value;

use super::{
    DescriptorKind, JsObject, JsSymbol, PropertyDescriptor, PropertySlot, Shape, SlotBody,
};

pub(super) fn ordinary_set_data_property(
    obj: JsObject,
    heap: &mut otter_gc::GcHeap,
    key: &str,
    value: Value,
) -> bool {
    let barrier_value = value.clone();
    let success = heap.with_payload(obj, |body| {
        if let Some(offset) = body.shape.offset_of(key) {
            let slot = &mut body.slots[offset as usize];
            if !slot.flags.writable() {
                return false;
            }
            let SlotBody::Data { value: stored } = &mut slot.body else {
                return false;
            };
            *stored = value;
            return true;
        }

        if !body.extensible {
            return false;
        }
        let new_shape = Shape::add_property(&body.shape, key);
        body.shape = new_shape;
        body.slots.push(PropertySlot::data_default(value));
        true
    });
    if success {
        heap.record_write(obj, &barrier_value);
    }
    success
}

pub(super) fn ordinary_set_symbol_data_property(
    obj: JsObject,
    heap: &mut otter_gc::GcHeap,
    key: &JsSymbol,
    value: Value,
) -> bool {
    let barrier_value = value.clone();
    let success = heap.with_payload(obj, |body| {
        if let Some(pos) = body.symbol_props.iter().position(|(k, _)| k.ptr_eq(key)) {
            let slot = &mut body.symbol_props[pos].1;
            if !slot.flags.writable() {
                return false;
            }
            let SlotBody::Data { value: stored } = &mut slot.body else {
                return false;
            };
            *stored = value;
            return true;
        }

        if !body.extensible {
            return false;
        }
        body.symbol_props
            .push((key.clone(), PropertySlot::data_default(value)));
        true
    });
    if success {
        heap.record_write(obj, &barrier_value);
    }
    success
}

/// Implements §10.1.6.3 ValidateAndApplyPropertyDescriptor for an existing
/// slot. Returns `Some(updated)` on success, `None` to reject.
pub(super) fn validate_and_apply(
    existing: &PropertySlot,
    incoming: &PropertyDescriptor,
) -> Option<PropertySlot> {
    let existing_kind_is_data = matches!(existing.body, SlotBody::Data { .. });
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
                    && let SlotBody::Data { value: existing_v } = &existing.body
                    && !same_value(existing_v, incoming_v)
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
                && let SlotBody::Accessor {
                    getter: ex_get,
                    setter: ex_set,
                } = &existing.body
                && (!optional_value_eq(ex_get, in_get) || !optional_value_eq(ex_set, in_set))
            {
                return None;
            }
        }
    }

    // Build merged slot.
    Some(PropertySlot::from_descriptor(PropertyDescriptor {
        flags: incoming.flags,
        kind: incoming.kind.clone(),
    }))
}

pub(super) fn validate_descriptor_update(
    existing: &PropertyDescriptor,
    incoming: &PropertyDescriptor,
) -> Option<PropertyDescriptor> {
    let existing = PropertySlot::from_descriptor(existing.clone());
    validate_and_apply(&existing, incoming).map(|slot| slot.to_descriptor())
}

fn optional_value_eq(a: &Option<Value>, b: &Option<Value>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(x), Some(y)) => same_value(x, y),
        _ => false,
    }
}

fn same_value(a: &Value, b: &Value) -> bool {
    crate::abstract_ops::same_value(a, b)
}
