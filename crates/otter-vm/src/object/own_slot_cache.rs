//! Isolate cache of shaped objects' own-slot lookups (V8's
//! `DescriptorLookupCache`).
//!
//! # Contents
//! - [`OwnSlotCache`] — a direct-mapped table from a shape id and an
//!   interned key to the slot and attributes the shape gives that key, or to
//!   its absence.
//!
//! # Invariants
//! - A shape's slots never change and shape ids are never reused, so an
//!   entry never goes stale and nothing invalidates the table.
//! - Only interned keys of shaped objects are cached; a dictionary object's
//!   own keys change in place and are looked up by name.
//!
//! # See also
//! - [`super::shape_body::shape_slot_of_atom`] — the chain walk an entry
//!   stands for.

use std::cell::Cell;

use super::ShapeId;
use super::descriptor::PropertyFlags;
use super::shape_body::ShapeSlot;
use crate::property_atom::AtomId;

/// Entries of the table; a power of two.
const WAYS: usize = 1024;
/// The entry records a slot, rather than the key's absence.
const PRESENT: u64 = 1 << 63;
/// The recorded slot holds an accessor.
const ACCESSOR: u64 = 1 << 62;
const OFFSET_SHIFT: u32 = 32;
const FLAGS_SHIFT: u32 = 48;

/// See the [module documentation](self).
pub(crate) struct OwnSlotCache {
    /// `(shape id, packed atom, slot and attributes)`; an empty entry holds
    /// the unassigned shape id.
    entries: Box<[Cell<(u64, u64)>]>,
}

impl OwnSlotCache {
    pub(crate) fn new() -> Self {
        Self {
            entries: (0..WAYS).map(|_| Cell::new((0, 0))).collect(),
        }
    }

    /// The slot `shape` gives `atom`, answered by `walk` and remembered when
    /// this shape and key were not seen before.
    pub(crate) fn slot(
        &self,
        shape: ShapeId,
        atom: AtomId,
        walk: impl FnOnce() -> Option<ShapeSlot>,
    ) -> Option<ShapeSlot> {
        let entry = &self.entries[index(shape, atom)];
        let (key, value) = entry.get();
        if key == shape.raw() && value as u32 == atom.raw() {
            return unpack(value);
        }
        let slot = walk();
        if let Some(value) = pack(atom, slot) {
            entry.set((shape.raw(), value));
        }
        slot
    }
}

fn index(shape: ShapeId, atom: AtomId) -> usize {
    let mixed =
        (shape.raw() ^ u64::from(atom.raw()).rotate_left(29)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    (mixed >> (64 - WAYS.trailing_zeros())) as usize
}

/// The entry value for `slot`, or `None` for an offset the packing cannot
/// hold, which stays uncached.
fn pack(atom: AtomId, slot: Option<ShapeSlot>) -> Option<u64> {
    let Some(slot) = slot else {
        return Some(u64::from(atom.raw()));
    };
    let offset = u16::try_from(slot.offset).ok()?;
    let flags = u64::from(slot.flags.writable())
        | u64::from(slot.flags.enumerable()) << 1
        | u64::from(slot.flags.configurable()) << 2;
    Some(
        u64::from(atom.raw())
            | u64::from(offset) << OFFSET_SHIFT
            | flags << FLAGS_SHIFT
            | if slot.is_accessor { ACCESSOR } else { 0 }
            | PRESENT,
    )
}

fn unpack(value: u64) -> Option<ShapeSlot> {
    if value & PRESENT == 0 {
        return None;
    }
    let flags = value >> FLAGS_SHIFT;
    Some(ShapeSlot {
        offset: u32::from((value >> OFFSET_SHIFT) as u16),
        flags: PropertyFlags::new(flags & 1 != 0, flags & 2 != 0, flags & 4 != 0),
        is_accessor: value & ACCESSOR != 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slots_and_absence_round_trip() {
        let atom = AtomId::from_global(7);
        let slot = ShapeSlot {
            offset: 513,
            flags: PropertyFlags::new(true, false, true),
            is_accessor: true,
        };
        assert_eq!(unpack(pack(atom, Some(slot)).unwrap()), Some(slot));
        assert_eq!(unpack(pack(atom, None).unwrap()), None);
        let wide = ShapeSlot {
            offset: 1 << 16,
            ..slot
        };
        assert_eq!(pack(atom, Some(wide)), None);
    }
}
