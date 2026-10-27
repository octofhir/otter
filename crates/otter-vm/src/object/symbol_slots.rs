//! Symbol-keyed own properties held in hidden-class slots.
//!
//! A shaped object keys its symbol properties the way it keys its string
//! ones: the hidden class names each slot by the symbol's property atom, so a
//! symbol read is a shape lookup and objects built alike share one class.
//! The side table ([`super::symbol_table`]) keeps the symbol properties of
//! dictionary objects and every symbol added once the hidden class could not
//! take one.
//!
//! # Contents
//! - [`shaped_slot`] / [`shaped_symbols`] — lookup and slot-order enumeration.
//! - [`has_shaped_well_known`] — allocation-free well-known key probe.
//! - [`own_symbol_entries`] — every own symbol property in insertion order.
//! - [`define_shaped`] — appends and same-attribute writes through the class.
//! - [`spill_entries`] — the slots a dictionary conversion moves to the table.
//!
//! # Invariants
//! - Hidden-class symbols precede side-table symbols in insertion order: an
//!   append takes a slot only while the side table is empty.
//! - A dictionary object's slots are all string-keyed: every conversion to
//!   dictionary storage moves the class's symbol slots into the side table
//!   ahead of its existing entries.
//! - Attribute changes leave the hidden class first; the side table owns every
//!   descriptor change other than a value write.
//! - Private Names live in the same slots and stay out of every enumeration
//!   and integrity level.
//!
//! # See also
//! - `super::descriptor_install` routes symbol descriptors here first.
//! - `super::shape_interner::ShapeInterner::symbol_child_with_roots` mints the
//!   transitions.

use super::*;
use crate::rooting::RootScopeExt;

/// The slot a shaped object's hidden class gives `key`.
#[must_use]
pub(super) fn shaped_slot(heap: &GcHeap, body: &ObjectBody, key: JsSymbol) -> Option<usize> {
    if body.is_dictionary() || !shape_body::has_symbol_keys(body.shape) {
        return None;
    }
    let atom = key.minted_atom(heap)?;
    shape_body::shape_slot_of_atom(heap, body.shape, atom).map(|slot| slot.offset as usize)
}

/// A shaped object's symbol-keyed slots, in insertion order.
#[must_use]
pub(super) fn shaped_symbols(heap: &GcHeap, body: &ObjectBody) -> Vec<(JsSymbol, usize)> {
    if body.is_dictionary() || !shape_body::has_symbol_keys(body.shape) {
        return Vec::new();
    }
    shape_body::shape_keys_ordered(heap, body.shape)
        .into_iter()
        .filter_map(|(key, offset)| {
            Some((
                JsSymbol::from_handle(heap, key.as_symbol()?),
                offset as usize,
            ))
        })
        .collect()
}

/// Whether a shaped object's hidden class keys a slot by the well-known
/// symbol `tag`, walking the transition chain without collecting it.
#[must_use]
pub(super) fn has_shaped_well_known(
    heap: &GcHeap,
    body: &ObjectBody,
    tag: crate::symbol::WellKnown,
) -> bool {
    if body.is_dictionary() || !shape_body::has_symbol_keys(body.shape) {
        return false;
    }
    let mut shape = body.shape;
    while !shape.is_null() {
        let (parent, key) =
            heap.read_payload(shape, |shape| (shape.parent(), shape.transition_key()));
        if key
            .as_symbol()
            .is_some_and(|symbol| JsSymbol::from_handle(heap, symbol).well_known_tag() == Some(tag))
        {
            return true;
        }
        shape = parent;
    }
    false
}

/// Whether slot `index` of a shaped object holds a Private Name.
#[must_use]
pub(super) fn is_private_slot(heap: &GcHeap, body: &ObjectBody, index: usize) -> bool {
    !body.is_dictionary()
        && shape_body::has_symbol_keys(body.shape)
        && shape_body::shape_key_at_offset(heap, body.shape, index as u32)
            .and_then(shape_body::ShapeKey::as_symbol)
            .is_some_and(|key| JsSymbol::from_handle(heap, key).is_private_name())
}

/// [`super::lookup_own_atom_with`] for a symbol key: the hidden class by
/// atom, then the side table, which a cache never names by slot.
pub(super) fn lookup_atom(
    heap: &GcHeap,
    body: &ObjectBody,
    key: AtomizedPropertyKey<'_>,
    symbol: crate::symbol::SymbolHandle,
    slot_of: impl FnOnce(ShapeHandle, crate::property_atom::AtomId) -> Option<shape_body::ShapeSlot>,
) -> AtomPropertyLookup {
    if !body.is_dictionary()
        && shape_body::has_symbol_keys(body.shape)
        && let Some(slot) = slot_of(body.shape, key.atom().id())
    {
        return lookup_resolved_slot(
            heap,
            body,
            key,
            Some((slot.offset, slot.flags, slot.is_accessor)),
        );
    }
    let lookup = body
        .symbol_props()
        .iter()
        .find(|(entry, _)| entry.handle() == symbol)
        .map_or(PropertyLookup::Absent, |(_, slot)| slot.to_lookup());
    AtomPropertyLookup { hit: None, lookup }
}

/// How many of a shaped object's slots are symbol-keyed.
#[must_use]
pub(super) fn shaped_symbol_count(heap: &GcHeap, body: &ObjectBody) -> usize {
    if body.is_dictionary() || !shape_body::has_symbol_keys(body.shape) {
        return 0;
    }
    shape_body::shape_keys_ordered(heap, body.shape)
        .into_iter()
        .filter(|(key, _)| key.as_symbol().is_some())
        .count()
}

/// Every own symbol-keyed property, Private Names included, in insertion
/// order: the hidden-class slots, then the side table.
#[must_use]
pub(super) fn own_symbol_entries(heap: &GcHeap, body: &ObjectBody) -> Vec<(JsSymbol, SlotData)> {
    let mut entries: Vec<_> = shaped_symbols(heap, body)
        .into_iter()
        .map(|(key, index)| (key, body.slot_data(heap, index)))
        .collect();
    entries.extend(body.symbol_props().iter().cloned());
    entries
}

/// The string-keyed slot offsets and the symbol entries of a shaped object
/// about to enter dictionary storage, read under one borrow with no
/// allocation before publication.
#[must_use]
pub(super) fn spill_entries(
    heap: &GcHeap,
    body: &ObjectBody,
) -> (Vec<usize>, Vec<(JsSymbol, SlotData)>) {
    let symbols = shaped_symbols(heap, body);
    let count = body_property_count(heap, body);
    let strings = (0..count)
        .filter(|index| !symbols.iter().any(|(_, offset)| offset == index))
        .collect();
    let entries = symbols
        .into_iter()
        .map(|(key, index)| (key, body.slot_data(heap, index)))
        .collect();
    (strings, entries)
}

/// [`super::descriptor_install::define_symbol`] through the hidden class:
/// `Some` when the class owns the outcome, `None` when the side table does.
/// An attribute change on a class-held slot first moves the object to
/// dictionary storage, whose side table then owns the change.
///
/// # Errors
/// Propagates [`otter_gc::OutOfMemory`].
pub(super) fn define_shaped(
    object: &mut JsObject,
    heap: &mut GcHeap,
    key: JsSymbol,
    descriptor: &mut PartialPropertyDescriptor,
) -> Result<Option<bool>, otter_gc::OutOfMemory> {
    let (slot, table_empty, count) = heap.read_payload(*object, |body| {
        (
            shaped_slot(heap, body, key),
            body.symbol_props().is_empty(),
            body_property_count(heap, body),
        )
    });
    if let Some(index) = slot {
        let current = heap.read_payload(*object, |body| body.slot_data(heap, index));
        let Some(updated) = descriptor_core::validate_and_apply_partial(&current, descriptor, heap)
        else {
            return Ok(Some(false));
        };
        if descriptor_install::identical(&current, &updated, heap) {
            return Ok(Some(true));
        }
        if current.kind.is_data() && updated.kind.is_data() && current.flags == updated.flags {
            write_layout_slot(object, heap, index, updated.value);
            return Ok(Some(true));
        }
        let mut roots = otter_gc::RootScope::new(heap);
        // SAFETY: receiver and descriptor stay in their slots through the
        // conversion; the table path rereads both.
        unsafe {
            roots.add_object(object);
            roots.add_pelt(descriptor);
        }
        materialize_slots(object, heap)?;
        return Ok(None);
    }
    let fast = heap.read_payload(*object, |body| !body.is_dictionary() && body.extensible());
    if !fast || !table_empty || count >= MAX_FAST_PROPERTIES as usize {
        return Ok(None);
    }
    let complete = descriptor.complete_for_new_property();
    let flags = complete.flags;
    let is_accessor = matches!(complete.kind, DescriptorKind::Accessor { .. });
    let mut key_value = Value::symbol(key);
    let mut stored = Value::undefined();
    let mut next_shape = ShapeHandle::null();
    let mut roots = otter_gc::RootScope::new(heap);
    // SAFETY: every registered slot stays stationary until publication; all
    // later reads go through them.
    unsafe {
        roots.add_object(object);
        roots.add_value(&mut key_value);
        roots.add_value(&mut stored);
        roots.add_raw_slot((&mut next_shape as *mut ShapeHandle).cast::<RawGc>());
        roots.add_pelt(descriptor);
    }
    let parent = shape(*object, heap);
    let (child, _) = shape_interner::ShapeInterner::symbol_child_with_roots(
        heap,
        parent,
        key,
        flags,
        is_accessor,
        &mut |_| {},
    )?;
    next_shape = child;
    let source = SlotData::from_descriptor(descriptor.complete_for_new_property());
    let (meta, value) = source.into_flat(heap, object)?;
    stored = value;
    let index = shape_body::shape_property_count(heap, next_shape) as usize - 1;
    reserve_slot_capacity(object, heap, index + 1, std::slice::from_mut(&mut stored))?;
    let physical = stored;
    heap.with_payload(*object, |body| {
        debug_assert_object_shape_handle(next_shape, "shape-slot store");
        assert_eq!(
            body.inline_capacity(),
            shape_body::inline_capacity_of(next_shape),
            "shape transition changed object footprint"
        );
        body.invalidate_prototype_proofs();
        body.adopt_shape(next_shape);
        body.push_slot(index, meta, physical);
    });
    record_slot_write(heap, *object, stored);
    record_exotic_write(heap, *object, &next_shape);
    Ok(Some(true))
}
