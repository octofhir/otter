//! One managed insertion-ordered symbol descriptor table for objects and arrays.
//!
//! # Contents
//! - The existing SymbolPropsBody records, trace and function-liveness owner.
//! - Rooted table growth with actual cap admission and noncollecting copying.
//! - Descriptor reads/publication, deletion, ordering and integrity attributes.
//!
//! # Invariants
//! - Object and Array sidecars use this one physical table/type/tag/layout.
//! - Keys and every descriptor reference are traced in the actual managed slots.
//! - Capacity growth roots the old table until its current records are copied.
//! - No payload borrow encloses allocation; writes require prepared capacity.
//! - Existing entries update without allocation or cap reservation.
//!
//! # See also
//! - `super::descriptor_install` owns object descriptor publication.
//! - `crate::array::symbol_properties` owns array descriptor validation.

use super::{PropertyDescriptor, PropertyFlags, SlotData, SlotKind};
use crate::symbol::JsSymbol;
use otter_gc::{
    GcHeap,
    heap::RootSlotVisitor,
    raw::{RawGc, SlotVisitor},
};

/// Reserved [`otter_gc::Traceable::TYPE_TAG`] for [`SymbolPropsBody`].
pub(crate) const SYMBOL_PROPS_BODY_TYPE_TAG: u8 = 0x3b;

/// Handle to the shared object/array symbol descriptor table.
pub(crate) type SymbolPropsHandle = otter_gc::Gc<SymbolPropsBody>;

/// One symbol-keyed own property.
pub(super) type SymbolProp = (crate::symbol::JsSymbol, SlotData);

/// Header for insertion-ordered symbol descriptors on objects and arrays.
/// Keys and descriptor references occupy the same managed trailing records;
/// tracing rewrites the key, its description and every descriptor value.
#[repr(C, align(8))]
pub(crate) struct SymbolPropsBody {
    /// Records the trailing array can hold.
    capacity: u32,
    /// Records written, and therefore traced.
    len: u32,
}

impl SymbolPropsBody {
    /// Trailing bytes a table of `capacity` records needs.
    #[must_use]
    pub(super) fn trailing_bytes(capacity: usize) -> usize {
        capacity * std::mem::size_of::<SymbolProp>()
    }

    pub(super) fn new(capacity: usize) -> Self {
        Self {
            capacity: u32::try_from(capacity).expect("symbol prop capacity exceeds u32"),
            len: 0,
        }
    }

    /// Number of records the actual managed allocation can hold.
    pub(crate) fn capacity(&self) -> usize {
        self.capacity as usize
    }

    pub(super) fn len(&self) -> usize {
        self.len as usize
    }

    fn entries_ptr(&self) -> *mut SymbolProp {
        // SAFETY: the allocation reserved `trailing_bytes(capacity)`
        // immediately after this header.
        unsafe {
            (self as *const Self as *mut u8)
                .add(std::mem::size_of::<Self>())
                .cast()
        }
    }

    /// The written records.
    pub(super) fn entries(&self) -> &[SymbolProp] {
        // SAFETY: the first `len` records were written before the table
        // became reachable.
        unsafe { std::slice::from_raw_parts(self.entries_ptr().cast_const(), self.len()) }
    }

    /// The written records, mutably.
    pub(super) fn entries_mut(&mut self) -> &mut [SymbolProp] {
        // SAFETY: as in `entries`.
        unsafe { std::slice::from_raw_parts_mut(self.entries_ptr(), self.len()) }
    }

    /// Append a record. The caller must have reserved capacity: growth
    /// allocates, and a payload borrow has no heap to allocate from.
    pub(super) fn push(&mut self, entry: SymbolProp) {
        let index = self.len();
        debug_assert!(
            index < self.capacity(),
            "symbol prop push without a reservation"
        );
        // SAFETY: `index < capacity`, so the slot is inside the table.
        unsafe { self.entries_ptr().add(index).write(entry) };
        self.len += 1;
    }

    /// Remove the record at `index`, sliding later records down.
    pub(super) fn remove(&mut self, index: usize) {
        let len = self.len();
        debug_assert!(index < len);
        for i in index..len - 1 {
            // SAFETY: both slots are inside the written prefix.
            unsafe {
                let next = self.entries_ptr().add(i + 1).read();
                self.entries_ptr().add(i).write(next);
            }
        }
        self.len -= 1;
    }
}

const _: () = assert!(
    std::mem::size_of::<SymbolPropsBody>().is_multiple_of(std::mem::align_of::<SymbolProp>())
);

impl otter_gc::SafeTraceable for SymbolPropsBody {
    const TYPE_TAG: u8 = SYMBOL_PROPS_BODY_TYPE_TAG;

    fn trace_slots_safe(&mut self, v: &mut SlotVisitor<'_>) {
        for (sym, slot) in self.entries_mut() {
            // The key's symbol handle (and its description string) must
            // relocate with the table: symbol property lookup is handle
            // identity, so an unvisited key would compare unequal to the
            // relocated well-known symbol after a snapshot restore.
            sym.trace_value_slots(v);
            match &mut slot.kind {
                SlotKind::Data => slot.value.trace_value_slot_mut(v),
                SlotKind::Accessor(pair) => {
                    if let Some(g) = &mut pair.getter {
                        g.trace_value_slot_mut(v);
                    }
                    if let Some(s) = &mut pair.setter {
                        s.trace_value_slot_mut(v);
                    }
                }
            }
        }
    }

    /// The trailing array lives in the heap cell, not in this body, so a
    /// pending copy on the stack has nothing to trace: everything
    /// `trace_slots_safe` walks is storage that does not exist yet.
    fn trace_pending_slots_safe(&mut self, _visitor: &mut SlotVisitor<'_>) {}
}

impl SymbolPropsBody {
    pub(crate) fn visit_function_ids(&self, visitor: &mut dyn FnMut(u32)) {
        for (_, slot) in self.entries() {
            match &slot.kind {
                SlotKind::Data => crate::code_liveness::visit_value(&slot.value, visitor),
                SlotKind::Accessor(pair) => {
                    if let Some(value) = &pair.getter {
                        crate::code_liveness::visit_value(value, visitor);
                    }
                    if let Some(value) = &pair.setter {
                        crate::code_liveness::visit_value(value, visitor);
                    }
                }
            }
        }
    }
}

/// The table payload behind `handle`, or `None` for a null handle.
#[must_use]
pub(crate) fn body_of(handle: SymbolPropsHandle) -> Option<*mut SymbolPropsBody> {
    if handle.is_null() {
        return None;
    }
    let header = handle.as_header_ptr();
    // SAFETY: a non-null handle names a live cell whose payload is a
    // `SymbolPropsBody` one header past the start.
    Some(unsafe {
        header
            .cast::<u8>()
            .add(std::mem::size_of::<otter_gc::GcHeader>())
            .cast::<SymbolPropsBody>()
    })
}

impl SymbolPropsBody {
    /// Current descriptors in their original creation order.
    pub(crate) fn descriptors(&self) -> impl Iterator<Item = (JsSymbol, PropertyDescriptor)> + '_ {
        self.entries()
            .iter()
            .map(|(key, slot)| (*key, slot.to_descriptor()))
    }

    pub(crate) fn descriptor(&self, key: JsSymbol) -> Option<PropertyDescriptor> {
        self.entries()
            .iter()
            .find(|(symbol, _)| symbol.ptr_eq(key))
            .map(|(_, slot)| slot.to_descriptor())
    }

    /// Commit a validated descriptor after the caller prepared capacity.
    pub(crate) fn put_descriptor(&mut self, key: JsSymbol, descriptor: PropertyDescriptor) {
        let slot = SlotData::from_descriptor(descriptor);
        if let Some((_, stored)) = self
            .entries_mut()
            .iter_mut()
            .find(|(symbol, _)| symbol.ptr_eq(key))
        {
            *stored = slot;
        } else {
            self.push((key, slot));
        }
    }

    pub(crate) fn remove_key(&mut self, key: JsSymbol) -> bool {
        let Some(index) = self
            .entries()
            .iter()
            .position(|(symbol, _)| symbol.ptr_eq(key))
        else {
            return false;
        };
        self.remove(index);
        true
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub(crate) fn set_integrity_level(&mut self, frozen: bool) {
        for (key, slot) in self.entries_mut() {
            if !key.is_private_name() {
                slot.flags = PropertyFlags::new(
                    !frozen && slot.flags.writable(),
                    slot.flags.enumerable(),
                    false,
                );
            }
        }
    }

    pub(crate) fn test_integrity_level(&self, frozen: bool) -> bool {
        self.entries()
            .iter()
            .filter(|(key, _)| !key.is_private_name())
            .all(|(_, slot)| !slot.flags.configurable() && (!frozen || !slot.flags.writable()))
    }
}

pub(crate) fn len_of(handle: SymbolPropsHandle) -> usize {
    body_of(handle).map_or(0, |body| {
        // SAFETY: this typed handle names the live table owned by a sidecar.
        unsafe { (*body).len() }
    })
}

/// Prepare one append slot, preserving the old table on actual allocator failure.
/// The caller roots its receiver and pending key/descriptor through external_visit
/// or the heap's normal root providers. This owner roots the actual table slot.
pub(crate) fn reserve_table(
    handle: &mut SymbolPropsHandle,
    heap: &mut GcHeap,
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<(), otter_gc::OutOfMemory> {
    let (len, capacity) = body_of(*handle).map_or((0, 0), |body| {
        // SAFETY: the caller's live typed handle owns this immutable lookup.
        unsafe { ((*body).len(), (*body).capacity()) }
    });
    if len < capacity {
        return Ok(());
    }
    let grown = (capacity * 2).max(2);
    let old = *handle;
    let handle_slot = std::ptr::from_mut(handle).cast::<RawGc>();
    let mut visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
        external_visit(visitor);
        visitor(handle_slot);
    };
    let replacement = heap.alloc_variable_with_roots(
        SymbolPropsBody::new(grown),
        SymbolPropsBody::trailing_bytes(grown),
        &mut visit,
    )?;
    if let Some(old_body) = body_of(old) {
        let replacement_body = body_of(replacement).expect("fresh managed table");
        // SAFETY: tables are old-space and nonmoving; the old one stayed rooted
        // through allocation. Current records were rewritten by any collection.
        // Copying and barriers below cannot collect before publication.
        unsafe {
            for entry in (*old_body).entries() {
                (*replacement_body).push(entry.clone());
            }
        }
    }
    if let Some(replacement_body) = body_of(replacement) {
        // SAFETY: this fresh table is live and no barrier allocates/collects.
        for (key, slot) in unsafe { (*replacement_body).entries() } {
            record_entry_write(heap, replacement, key, &slot.to_descriptor());
        }
    }
    *handle = replacement;
    Ok(())
}

/// Remember both key and descriptor edges against the physical table owner.
pub(crate) fn record_entry_write(
    heap: &mut GcHeap,
    table: SymbolPropsHandle,
    key: &JsSymbol,
    descriptor: &PropertyDescriptor,
) {
    record_write(heap, table, key);
    record_write(heap, table, descriptor);
}

/// Record an edge against the actual managed descriptor storage owner.
pub(crate) fn record_write<V>(heap: &mut GcHeap, table: SymbolPropsHandle, value: &V)
where
    V: otter_gc::GcStore + ?Sized,
{
    heap.record_write(table, value);
}
