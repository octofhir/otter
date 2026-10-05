//! Rooted ordinary descriptor installation with distinct rejection and OOM.
//!
//! # Contents
//! - Full descriptors enter the same partial validation algorithm.
//! - String installs prepare accessor cells, banks and dictionary metadata.
//! - Symbol installs keep the key and incoming values rooted across table growth.
//!
//! # Invariants
//! - `Ok(false)` means an ECMAScript descriptor rejection, never allocation failure.
//! - Actual receiver and incoming fields are rewritten through every allocation.
//! - Rejecting and identical descriptors do not allocate or retire watched proofs.
//! - Prepared metadata stays rooted until a non-allocating publication.
//! - No object payload borrow encloses allocation or collection.
//!
//! # See also
//! - `super::descriptor_core` owns ValidateAndApplyPropertyDescriptor.
//! - `super::descriptor_mutation` retires watched dictionary proofs before writes.

use super::*;
use crate::rooting::RootScopeExt;

fn identical(existing: &SlotData, updated: &SlotData, heap: &GcHeap) -> bool {
    if existing.flags != updated.flags {
        return false;
    }
    let same = |a: Value, b: Value| crate::abstract_ops::same_value(&a, &b, heap);
    match (&existing.kind, &updated.kind) {
        (SlotKind::Data, SlotKind::Data) => same(existing.value, updated.value),
        (SlotKind::Accessor(a), SlotKind::Accessor(b)) => {
            same(
                a.getter.unwrap_or_else(Value::undefined),
                b.getter.unwrap_or_else(Value::undefined),
            ) && same(
                a.setter.unwrap_or_else(Value::undefined),
                b.setter.unwrap_or_else(Value::undefined),
            )
        }
        _ => false,
    }
}

fn resolve_string(
    object: JsObject,
    heap: &GcHeap,
    key: &str,
    descriptor: &PartialPropertyDescriptor,
) -> Result<(Option<u32>, SlotData), bool> {
    let offset = heap.read_payload(object, |body| body_offset_of(heap, body, key));
    let slot = if let Some(offset) = offset {
        let current = heap.read_payload(object, |body| body.slot_data(heap, offset as usize));
        let Some(updated) = descriptor_core::validate_and_apply_partial(&current, descriptor, heap)
        else {
            return Err(false);
        };
        if identical(&current, &updated, heap)
            && heap
                .read_payload(object, |body| mapped_argument_cell(body, key))
                .is_none()
        {
            return Err(true);
        }
        updated
    } else {
        if !is_extensible(object, heap) {
            return Err(false);
        }
        SlotData::from_descriptor(descriptor.complete_for_new_property())
    };
    Ok((offset, slot))
}

pub(super) fn define_string(
    object: &mut JsObject,
    heap: &mut GcHeap,
    key: &str,
    mut descriptor: PartialPropertyDescriptor,
) -> Result<bool, otter_gc::OutOfMemory> {
    let (offset, source) = match resolve_string(*object, heap, key, &descriptor) {
        Ok(resolved) => resolved,
        Err(outcome) => return Ok(outcome),
    };
    // A data-value update with unchanged attributes needs no sidecar, shape,
    // slab or accessor cell. This is the same publication owner for Define
    // and ordinary Set, including mapped-parameter alias writes.
    if let Some(offset) = offset
        && source.kind.is_data()
        && heap.read_payload(*object, |body| body.slot_attrs(heap, offset as usize))
            == (source.flags, false)
    {
        let stored = source.value;
        heap.with_payload(*object, |body| body.set_data_value(offset as usize, stored));
        apply_mapped_arguments_partial_define(*object, heap, key, descriptor, Some(offset));
        record_slot_write(heap, *object, stored);
        return Ok(true);
    }
    let mut stored = Value::undefined();
    let mut roots = otter_gc::RootScope::new(heap);
    // SAFETY: the caller slot and local slots stay stationary until this owner
    // returns; the descriptor is cloned for the non-allocating mapped commit.
    unsafe {
        roots.add_object(object);
        roots.add_value(&mut stored);
        roots.add_pelt(&mut descriptor);
    }
    let (meta, value) = source.into_flat(heap, object)?;
    stored = value;
    ensure_exotic(object, heap)?;
    if offset.is_some() {
        materialize_slots_with_pending_values(object, heap, std::slice::from_mut(&mut stored))?;
        heap.with_payload(*object, |body| {
            body.set_slot(offset.unwrap() as usize, meta, stored, None)
        });
    } else {
        let index = heap.read_payload(*object, |body| body_property_count(heap, body));
        let keys = dictionary_keys_for_shape_transition(heap, *object, None);
        let metas = slot_metas_for_shape_transition(heap, *object, None);
        reserve_slot_capacity(object, heap, index + 1, std::slice::from_mut(&mut stored))?;
        reserve_slot_meta_capacity(object, heap, index + 1, std::slice::from_mut(&mut stored))?;
        // SAFETY: prepared metadata is kept in the same heap's handle arena
        // while the later table allocation can perform a full collection.
        let handles = unsafe { otter_gc::HandleScope::from_ptr(heap.handle_stack_ptr()) };
        let table = slot_meta_table_for_install(
            object,
            heap,
            &metas,
            index + 1,
            std::slice::from_mut(&mut stored),
        )?
        .map(|table| handles.local(table));
        let keys = dict_keys_table_for_install(
            object,
            heap,
            &keys,
            key,
            std::slice::from_mut(&mut stored),
        )?
        .map(|table| handles.local(table));
        let table = table.map(|table| table.get());
        let keys = keys.map(|table| table.get());
        heap.with_payload(*object, |body| {
            body.enter_dictionary_mode(true);
            if let Some(table) = table {
                body.exotic_mut().slots = table;
            }
            if let Some(keys) = keys {
                body.exotic_mut().dictionary_keys = keys;
            }
            dict_push_key(body, key.to_owned());
            body.push_slot(index, meta, stored);
        });
        let sidecar = heap.read_payload(*object, |body| body.exotic.get());
        if let Some(table) = table {
            heap.record_write(sidecar, &table);
        }
        if let Some(keys) = keys {
            heap.record_write(sidecar, &keys);
        }
    }
    if descriptor.value.is_some() && !meta.is_accessor {
        descriptor.value = Some(stored);
    }
    apply_mapped_arguments_partial_define(*object, heap, key, descriptor.clone(), offset);
    record_slot_write(heap, *object, stored);
    Ok(true)
}

pub(super) fn define_string_with_shape(
    object: &mut JsObject,
    heap: &mut GcHeap,
    key: &str,
    mut descriptor: PartialPropertyDescriptor,
    mut next_shape: ShapeHandle,
) -> Result<bool, otter_gc::OutOfMemory> {
    let (offset, source) = match resolve_string(*object, heap, key, &descriptor) {
        Ok(resolved) => resolved,
        Err(outcome) => return Ok(outcome),
    };
    let mut stored = Value::undefined();
    let mut roots = otter_gc::RootScope::new(heap);
    // SAFETY: all registered slots remain stationary through publication.
    unsafe {
        roots.add_object(object);
        roots.add_value(&mut stored);
        roots.add_raw_slot((&mut next_shape as *mut ShapeHandle).cast::<RawGc>());
        roots.add_pelt(&mut descriptor);
    }
    let (meta, value) = source.into_flat(heap, object)?;
    stored = value;
    let index = offset.map_or_else(
        || shape_body::shape_property_count(heap, next_shape) as usize - 1,
        |offset| offset as usize,
    );
    reserve_slot_capacity(object, heap, index + 1, std::slice::from_mut(&mut stored))?;
    heap.with_payload(*object, |body| {
        if offset.is_some() {
            body.set_slot(index, meta, stored, Some(next_shape));
        } else {
            debug_assert_object_shape_handle(next_shape, "shape-slot store");
            assert_eq!(
                body.inline_capacity(),
                shape_body::inline_capacity_of(next_shape),
                "shape transition changed object footprint"
            );
            body.invalidate_prototype_proofs();
            body.shape = next_shape;
            body.push_slot(index, meta, stored);
        }
    });
    if descriptor.value.is_some() && !meta.is_accessor {
        descriptor.value = Some(stored);
    }
    apply_mapped_arguments_partial_define(*object, heap, key, descriptor.clone(), offset);
    record_slot_write(heap, *object, stored);
    record_exotic_write(heap, *object, &next_shape);
    #[cfg(debug_assertions)]
    if offset.is_none() {
        debug_assert_appended_shape_slot(*object, heap);
    }
    Ok(true)
}

pub(super) fn define_symbol(
    object: &mut JsObject,
    heap: &mut GcHeap,
    key: JsSymbol,
    mut descriptor: PartialPropertyDescriptor,
) -> Result<bool, otter_gc::OutOfMemory> {
    let existing = heap.read_payload(*object, |body| {
        body.symbol_props()
            .iter()
            .find(|(symbol, _)| symbol.ptr_eq(key))
            .map(|(_, slot)| slot.clone())
    });
    let append = existing.is_none();
    if let Some(existing) = existing {
        let Some(updated) =
            descriptor_core::validate_and_apply_partial(&existing, &descriptor, heap)
        else {
            return Ok(false);
        };
        if identical(&existing, &updated, heap) {
            return Ok(true);
        }
        if existing.kind.is_data() && updated.kind.is_data() && existing.flags == updated.flags {
            let stored = updated.value;
            heap.with_payload(*object, |body| {
                body.invalidate_prototype_proofs();
                let table = body
                    .symbol_props_mut()
                    .expect("existing symbol owns its table");
                assert!(
                    table.descriptor(key).is_some(),
                    "validated symbol remains present without JS or allocation"
                );
                table.put_descriptor(key, updated.to_descriptor());
            });
            record_symbol_entry_write(heap, *object, &key, &stored);
            return Ok(true);
        }
    } else if !is_extensible(*object, heap) {
        return Ok(false);
    }
    let mut key = Value::symbol(key);
    let mut roots = otter_gc::RootScope::new(heap);
    // SAFETY: receiver, key and incoming descriptor stay in their actual slots
    // until reservation completes; all later accesses reload their current values.
    unsafe {
        roots.add_object(object);
        roots.add_value(&mut key);
        roots.add_pelt(&mut descriptor);
    }
    if append {
        reserve_symbol_prop_capacity(object, heap, &mut |_| {})?;
    }
    let current_key = key.as_symbol(heap).expect("rooted key stays a Symbol");
    let existing = heap.read_payload(*object, |body| {
        body.symbol_props()
            .iter()
            .position(|(symbol, _)| symbol.ptr_eq(current_key))
            .map(|index| (index, body.symbol_props()[index].1.clone()))
    });
    let (index, updated) = match existing {
        Some((index, slot)) => (
            Some(index),
            descriptor_core::validate_and_apply_partial(&slot, &descriptor, heap)
                .expect("validated without intervening JS"),
        ),
        None => (
            None,
            SlotData::from_descriptor(descriptor.complete_for_new_property()),
        ),
    };
    let previous = heap.read_payload(*object, |body| {
        index.map(|index| {
            let slot = &body.symbol_props()[index].1;
            SlotMeta {
                flags: slot.flags,
                is_accessor: !slot.kind.is_data(),
                watched: false,
            }
        })
    });
    let next = SlotMeta {
        flags: updated.flags,
        is_accessor: !updated.kind.is_data(),
        watched: false,
    };
    let barrier = updated.to_descriptor();
    heap.with_payload(*object, |body| {
        if let Some(previous) = previous {
            descriptor_mutation::DescriptorChanges::for_slot(previous, next).retire(body);
        } else if body.is_dictionary() {
            body.exotic_mut().dictionary_shape_id = next_shape_id();
        }
        body.invalidate_prototype_proofs();
        let table = body
            .symbol_props_mut()
            .expect("symbol table reserved before publication");
        table.put_descriptor(current_key, updated.to_descriptor());
    });
    record_symbol_entry_write(heap, *object, &current_key, &barrier);
    Ok(true)
}

#[cfg(test)]
mod tests;
