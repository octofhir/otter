//! Array symbol descriptors over one insertion-ordered sidecar table.
//!
//! # Contents
//! - Field-presence-aware definition and data-half assignment.
//! - Exact descriptor reads, ordinary deletion and public-key enumeration.
//!
//! # Invariants
//! - A symbol owns one descriptor; changing kind preserves creation order.
//! - Ordinary descriptor validation has the same owner as object descriptors.
//! - `Ok(false)` means rejection; allocation failures retain their cause.
//! - Object and Array share one managed insertion-ordered symbol table.
//! - Preparation roots the actual array, key and merged descriptor before any
//!   collection; descriptor publication and write barriers cannot collect.
//! - New entries reserve managed table capacity before publication; actual VM
//!   cap failures publish no new descriptor. Existing updates never allocate.
//!
//! # See also
//! - [`super::ArrayExoticSlots`] owns tracing, integrity and cold storage.
//! - [`crate::object::validate_descriptor_partial`] owns descriptor validation.

use super::{GcHeap, JsArray, PropertyFlags};
use crate::Value;
use crate::object::{DescriptorKind, PartialPropertyDescriptor, PropertyDescriptor, symbol_table};
use crate::rooting::RootScopeExt;
use crate::symbol::JsSymbol;

/// Read an exact own symbol descriptor, including its attributes.
#[must_use]
pub fn get_symbol_descriptor(
    array: JsArray,
    heap: &GcHeap,
    key: JsSymbol,
) -> Option<PropertyDescriptor> {
    heap.read_payload(array, |body| {
        body.symbol_properties()
            .and_then(|table| table.descriptor(key))
    })
}

fn publish(array: JsArray, heap: &mut GcHeap, key: JsSymbol, descriptor: PropertyDescriptor) {
    let barrier = descriptor.clone();
    let table = heap.read_payload(array, |body| {
        body.exotic()
            .expect("symbol sidecar prepared")
            .symbol_properties
    });
    heap.with_payload(table, |body| body.put_descriptor(key, descriptor));
    symbol_table::record_entry_write(heap, table, &key, &barrier);
    heap.with_payload(array, |body| body.mark_dirty());
}

/// Apply an array's ordinary symbol [[DefineOwnProperty]] completion.
///
/// # Errors
///
/// Returns the actual sidecar/table allocation refusal before descriptor publication.
pub fn define_symbol_property_partial(
    array: &mut JsArray,
    heap: &mut GcHeap,
    key: JsSymbol,
    incoming: PartialPropertyDescriptor,
) -> Result<bool, otter_gc::OutOfMemory> {
    let mut descriptor = if let Some(existing) = get_symbol_descriptor(*array, heap, key) {
        let Some(updated) = crate::object::validate_descriptor_partial(&existing, &incoming, heap)
        else {
            return Ok(false);
        };
        // A present descriptor already owns a sidecar; publication cannot
        // enter an allocator, even when the descriptor changes its kind.
        publish(*array, heap, key, updated);
        return Ok(true);
    } else {
        if !super::is_extensible(*array, heap) {
            return Ok(false);
        }
        incoming.complete_for_new_property()
    };
    let mut key = Value::symbol(key);
    let mut roots = otter_gc::RootScope::new(heap);
    // SAFETY: caller array and these key/descriptor slots remain stationary
    // through sidecar preparation and final noncollecting publication.
    unsafe {
        roots.add_raw_slot(std::ptr::from_mut(array).cast());
        roots.add_value(&mut key);
        roots.add_pelt(&mut descriptor);
    }
    super::ensure_exotic(array, heap, &mut |_| {})?;
    let mut table = heap.read_payload(*array, |body| {
        body.exotic()
            .expect("symbol sidecar prepared")
            .symbol_properties
    });
    let previous = table;
    symbol_table::reserve_table(&mut table, heap, &mut |_| {})?;
    if table != previous {
        let sidecar = heap.read_payload(*array, |body| body.exotic);
        heap.with_payload(sidecar, |body| body.symbol_properties = table);
        heap.record_write(sidecar, &table);
    }
    let key = key.as_symbol(heap).expect("rooted symbol remains a symbol");
    publish(*array, heap, key, descriptor);
    Ok(true)
}

/// Assign the data half after the full receiver/prototype resolver.
///
/// # Errors
///
/// Preserves descriptor preparation OutOfMemory; `Ok(false)` is a genuine rejection.
pub fn ordinary_set_symbol_data_property(
    array: &mut JsArray,
    heap: &mut GcHeap,
    key: JsSymbol,
    value: Value,
) -> Result<bool, otter_gc::OutOfMemory> {
    let descriptor = match get_symbol_descriptor(*array, heap, key) {
        Some(existing) if existing.is_data() && existing.writable() => PartialPropertyDescriptor {
            value: Some(value),
            ..PartialPropertyDescriptor::default()
        },
        Some(_) => return Ok(false),
        None => PartialPropertyDescriptor::from_full(&PropertyDescriptor {
            kind: DescriptorKind::Data { value },
            flags: PropertyFlags::data_default(),
        }),
    };
    define_symbol_property_partial(array, heap, key, descriptor)
}

/// Read the accessor pair of a present accessor descriptor.
#[must_use]
pub fn get_symbol_accessor(
    array: JsArray,
    heap: &GcHeap,
    key: JsSymbol,
) -> Option<(Option<Value>, Option<Value>)> {
    match get_symbol_descriptor(array, heap, key)?.kind {
        DescriptorKind::Accessor { getter, setter } => Some((getter, setter)),
        DescriptorKind::Data { .. } => None,
    }
}

/// Read a present own data value.
#[must_use]
pub fn get_symbol_property(array: JsArray, heap: &GcHeap, key: JsSymbol) -> Option<Value> {
    match get_symbol_descriptor(array, heap, key)?.kind {
        DescriptorKind::Data { value } => Some(value),
        DescriptorKind::Accessor { .. } => None,
    }
}

/// Delete a configurable symbol descriptor; missing keys succeed.
pub fn delete_symbol_property(array: JsArray, heap: &mut GcHeap, key: JsSymbol) -> bool {
    let Some(descriptor) = get_symbol_descriptor(array, heap, key) else {
        return true;
    };
    if !descriptor.configurable() {
        return false;
    }
    let table = heap.read_payload(array, |body| {
        body.exotic()
            .expect("present symbol owns its sidecar")
            .symbol_properties
    });
    let empty = heap.with_payload(table, |body| {
        assert!(
            body.remove_key(key),
            "present symbol remains without JS or allocation"
        );
        body.is_empty()
    });
    heap.with_payload(array, |body| {
        if empty {
            body.exotic_mut().symbol_properties = super::SymbolPropsHandle::null();
        }
        body.mark_dirty();
    });
    true
}

/// Enumerate public symbol keys in creation order, independent of kind.
#[must_use]
pub fn own_symbol_keys(array: JsArray, heap: &GcHeap) -> Vec<JsSymbol> {
    heap.read_payload(array, |body| {
        body.symbol_properties().map_or_else(Vec::new, |table| {
            table
                .descriptors()
                .map(|(key, _)| key)
                .filter(|key| !key.is_private_name())
                .collect()
        })
    })
}

#[cfg(test)]
mod tests;
