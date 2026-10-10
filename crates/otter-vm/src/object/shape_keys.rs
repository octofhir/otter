//! The key table a shape lineage shares.
//!
//! A hidden class's keys are its transition chain: slot `i` is the key the
//! `i`-th transition added. Walking that chain costs one dependent heap read
//! per link, and a string-keyed lookup compares string contents per link.
//! Once a lineage grows past [`SHAPE_KEYS_MIN`] keys its shapes name one flat
//! table instead: entry `i` is slot `i`'s key, atom, spelling hash and
//! attributes, and an atom index finds an entry in one probe. A shape with
//! `n` keys reads entries `0..n` only.
//!
//! Along a transition chain, a child shares its parent's table when the
//! parent's keys are exactly the table's: it appends its key in place, and
//! the parent, which reads a shorter prefix, does not see it. A second child
//! of the same parent, or a full table, copies the prefix into a new, larger
//! table.
//!
//! # Contents
//! - [`ShapeKeysBody`] — the table: length and capacity, then the entries and
//!   the atom index in trailing storage.
//! - [`ShapeKeysHandle`] — the handle a shape stores.
//! - [`keys_for_child`] — the table a new child shape names.
//! - [`ShapeKeysBody::slot_of_atom`] / [`ShapeKeysBody::slot_of_str`] /
//!   [`ShapeKeysBody::entry`] — reads bounded by a shape's key count.
//!
//! # Invariants
//! - Entry `i` is the key of logical slot `i`; a table holds each atom at
//!   most once, since one chain adds a key once and a branch copies.
//! - Entries `0..len` are initialized and traced (the table keeps their key
//!   strings alive); entries past `len` and index buckets of no entry are
//!   never read.
//! - A shape reads entries below its own key count only; an entry another
//!   child appended is invisible to it.
//! - A table only grows in place by one entry at `len`, when `len` equals the
//!   appending parent's key count and the capacity has room; the key store
//!   is write-barriered.
//! - Old space: a table lives as long as the shapes naming it.
//!
//! # See also
//! - [`super::shape_body`] — the chain the table flattens.

use otter_gc::GcHeap;
use otter_gc::heap::RootSlotVisitor;
use otter_gc::raw::{RawGc, SlotVisitor};

use crate::property_atom::AtomId;
use crate::string::{JsStringHandle, eq_str};

use super::descriptor::PropertyFlags;

/// Reserved [`otter_gc::Traceable::TYPE_TAG`] for [`ShapeKeysBody`].
pub const SHAPE_KEYS_BODY_TYPE_TAG: u8 = 0x41;

/// Handle to a lineage's key table.
pub type ShapeKeysHandle = otter_gc::Gc<ShapeKeysBody>;

/// A shape with fewer keys walks its chain: a few links cost less than a
/// table.
pub(crate) const SHAPE_KEYS_MIN: u32 = 8;

/// One slot's key.
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub(crate) struct KeyEntry {
    pub(crate) key: JsStringHandle,
    pub(crate) atom: AtomId,
    /// Low bits of the spelling's interner hash
    /// ([`crate::property_atom::spelling_hash`]).
    pub(crate) hash: u32,
    pub(crate) flags: PropertyFlags,
    pub(crate) is_accessor: bool,
}

/// Index bucket of no entry.
const EMPTY: u16 = 0;

/// Length and capacity of a lineage's key table. `capacity` entries follow
/// in the same cell, then `2 * capacity` atom index buckets, each the
/// entry's index plus one.
#[repr(C, align(8))]
pub struct ShapeKeysBody {
    len: u32,
    capacity: u32,
}

impl ShapeKeysBody {
    fn new(capacity: u32) -> Self {
        debug_assert!(capacity.is_power_of_two());
        Self { len: 0, capacity }
    }

    fn trailing_bytes(capacity: u32) -> usize {
        let capacity = capacity as usize;
        capacity * std::mem::size_of::<KeyEntry>() + 2 * capacity * std::mem::size_of::<u16>()
    }

    fn entries_ptr(&self) -> *mut KeyEntry {
        // SAFETY: the allocation reserved `trailing_bytes(capacity)` right
        // after this header.
        unsafe { (self as *const Self as *mut Self).add(1).cast() }
    }

    fn index_ptr(&self) -> *mut u16 {
        // SAFETY: the index follows the `capacity` entries in the same tail.
        unsafe { self.entries_ptr().add(self.capacity as usize).cast() }
    }

    fn mask(&self) -> u32 {
        2 * self.capacity - 1
    }

    fn bucket_of(&self, atom: AtomId) -> u32 {
        atom.raw().wrapping_mul(0x9e37_79b1).rotate_right(16) & self.mask()
    }

    /// Entry `slot`, which must be below the reading shape's key count.
    #[must_use]
    pub(crate) fn entry(&self, slot: u32) -> KeyEntry {
        assert!(slot < self.len, "key table read past its length");
        // SAFETY: entries `0..len` are initialized.
        unsafe { *self.entries_ptr().add(slot as usize) }
    }

    /// The slot of `atom` among the first `count` entries.
    #[must_use]
    pub(crate) fn slot_of_atom(&self, atom: AtomId, count: u32) -> Option<u32> {
        let mut bucket = self.bucket_of(atom);
        loop {
            // SAFETY: `bucket <= mask`, within the `2 * capacity` buckets.
            let found = unsafe { *self.index_ptr().add(bucket as usize) };
            if found == EMPTY {
                return None;
            }
            let slot = u32::from(found) - 1;
            if self.entry(slot).atom == atom {
                return (slot < count).then_some(slot);
            }
            bucket = (bucket + 1) & self.mask();
        }
    }

    /// The slot spelled `key` (whose interner hash is `hash`) among the first
    /// `count` entries.
    #[must_use]
    pub(crate) fn slot_of_str(
        &self,
        heap: &GcHeap,
        key: &str,
        hash: u32,
        count: u32,
    ) -> Option<u32> {
        (0..count).find(|&slot| {
            let entry = self.entry(slot);
            entry.hash == hash && eq_str(heap, entry.key, key)
        })
    }

    /// Append `entry` at `len`, indexing its atom. The caller reserved the
    /// room and writes the key's barrier.
    fn push(&mut self, entry: KeyEntry) {
        assert!(self.len < self.capacity, "key table push past its capacity");
        let slot = self.len;
        // SAFETY: `slot < capacity`.
        unsafe { *self.entries_ptr().add(slot as usize) = entry };
        self.len += 1;
        let mut bucket = self.bucket_of(entry.atom);
        // SAFETY: buckets `0..=mask` exist; at most `capacity` of the
        // `2 * capacity` are occupied, so the probe ends.
        unsafe {
            while *self.index_ptr().add(bucket as usize) != EMPTY {
                bucket = (bucket + 1) & self.mask();
            }
            *self.index_ptr().add(bucket as usize) =
                u16::try_from(slot + 1).expect("key table index exceeds u16");
        }
    }

    fn len(&self) -> u32 {
        self.len
    }

    fn capacity(&self) -> u32 {
        self.capacity
    }
}

impl otter_gc::SafeTraceable for ShapeKeysBody {
    const TYPE_TAG: u8 = SHAPE_KEYS_BODY_TYPE_TAG;

    fn trace_slots_safe(&mut self, visitor: &mut SlotVisitor<'_>) {
        for slot in 0..self.len as usize {
            // SAFETY: entries `0..len` are initialized and live with the body.
            let key = unsafe { &mut (*self.entries_ptr().add(slot)).key };
            if !key.is_null() {
                visitor((key as *mut JsStringHandle).cast::<RawGc>());
            }
        }
    }

    /// The entries live in the heap cell, not in this header, so a pending
    /// copy on the stack has nothing to trace.
    fn trace_pending_slots_safe(&mut self, _visitor: &mut SlotVisitor<'_>) {}
}

/// The table a new child of `parent` names once its key is `entry`:
/// `parent_keys` itself when the parent's `parent_count` keys are exactly
/// that table's and it has room (appending in place), else a new table with
/// the first `parent_count` entries of `prefix`, which yields the parent's
/// keys in slot order, plus `entry`. Null below [`SHAPE_KEYS_MIN`] keys.
///
/// `external_visit` yields every root the caller holds: a new table's
/// allocation may collect.
///
/// # Errors
/// Propagates [`otter_gc::OutOfMemory`].
pub(crate) fn keys_for_child(
    heap: &mut GcHeap,
    parent_keys: ShapeKeysHandle,
    parent_count: u32,
    prefix: impl FnOnce(&GcHeap) -> Vec<KeyEntry>,
    mut entry: KeyEntry,
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<ShapeKeysHandle, otter_gc::OutOfMemory> {
    let count = parent_count + 1;
    if count < SHAPE_KEYS_MIN {
        return Ok(ShapeKeysHandle::null());
    }
    if !parent_keys.is_null()
        && heap.read_payload(parent_keys, |table| {
            table.len() == parent_count && table.len() < table.capacity()
        })
    {
        heap.with_payload(parent_keys, |table| table.push(entry));
        heap.record_write_edge(parent_keys, entry.key.raw());
        return Ok(parent_keys);
    }
    let mut entries = prefix(heap);
    debug_assert_eq!(entries.len(), parent_count as usize);
    let capacity = (2 * count).next_power_of_two();
    let base = entries.as_mut_ptr();
    let len = entries.len();
    let entry_key = &mut entry.key as *mut JsStringHandle;
    let mut visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
        external_visit(visitor);
        // SAFETY: `entries` and `entry` outlive the allocation; their key
        // handles are rewritten in place by a collection it runs.
        unsafe {
            for slot in 0..len {
                let key = &mut (*base.add(slot)).key;
                if !key.is_null() {
                    visitor((key as *mut JsStringHandle).cast::<RawGc>());
                }
            }
            if !(*entry_key).is_null() {
                visitor(entry_key.cast::<RawGc>());
            }
        }
    };
    heap.alloc_variable_with_roots_initialized(
        ShapeKeysBody::new(capacity),
        ShapeKeysBody::trailing_bytes(capacity),
        &mut visit,
        |table| {
            for &prefix_entry in &entries {
                table.push(prefix_entry);
            }
            table.push(entry);
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(atom: u32) -> KeyEntry {
        KeyEntry {
            key: JsStringHandle::null(),
            atom: AtomId::from_global(atom),
            hash: atom,
            flags: PropertyFlags::data_default(),
            is_accessor: false,
        }
    }

    #[test]
    fn a_shape_reads_only_its_own_prefix_of_a_shared_table() {
        let mut heap = GcHeap::new().expect("heap");
        let mut roots = |_visitor: &mut dyn FnMut(*mut RawGc)| {};
        let prefix = |count: u32| move |_: &GcHeap| (0..count).map(entry).collect::<Vec<_>>();
        let table = keys_for_child(
            &mut heap,
            ShapeKeysHandle::null(),
            SHAPE_KEYS_MIN - 1,
            prefix(SHAPE_KEYS_MIN - 1),
            entry(SHAPE_KEYS_MIN - 1),
            &mut roots,
        )
        .expect("table");
        let shared = keys_for_child(
            &mut heap,
            table,
            SHAPE_KEYS_MIN,
            prefix(SHAPE_KEYS_MIN),
            entry(100),
            &mut roots,
        )
        .expect("append");
        assert_eq!(shared, table, "the only child appends in place");
        let parent_count = SHAPE_KEYS_MIN;
        heap.read_payload(table, |keys| {
            assert_eq!(
                keys.slot_of_atom(AtomId::from_global(3), parent_count),
                Some(3)
            );
            assert_eq!(
                keys.slot_of_atom(AtomId::from_global(100), parent_count),
                None
            );
            assert_eq!(
                keys.slot_of_atom(AtomId::from_global(100), parent_count + 1),
                Some(parent_count)
            );
            assert_eq!(
                keys.slot_of_atom(AtomId::from_global(7777), parent_count + 1),
                None
            );
        });
        let branch = keys_for_child(
            &mut heap,
            table,
            SHAPE_KEYS_MIN,
            prefix(SHAPE_KEYS_MIN),
            entry(200),
            &mut roots,
        )
        .expect("branch");
        assert_ne!(branch, table, "a second child copies the prefix");
        heap.read_payload(branch, |keys| {
            assert_eq!(
                keys.slot_of_atom(AtomId::from_global(200), parent_count + 1),
                Some(parent_count)
            );
            assert_eq!(
                keys.slot_of_atom(AtomId::from_global(100), parent_count + 1),
                None
            );
        });
    }

    #[test]
    fn a_short_lineage_names_no_table() {
        let mut heap = GcHeap::new().expect("heap");
        let mut roots = |_visitor: &mut dyn FnMut(*mut RawGc)| {};
        let table = keys_for_child(
            &mut heap,
            ShapeKeysHandle::null(),
            2,
            |_| Vec::new(),
            entry(2),
            &mut roots,
        )
        .expect("no table");
        assert!(table.is_null());
    }
}
