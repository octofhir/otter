//! Short-lived memoization of a completed primitive concatenation.
//!
//! # Contents
//! - `ConcatCache` owns a fixed table of raw operand/result words.
//! - The layout constants define the one hash and entry geometry used by
//!   the native encoders and collecting stub.
//!
//! # Invariants
//! - An exact ordered pair stands for an immutable String/Number operation
//!   with at least one String. A match returns the finished string.
//! - Entries own no GC handles. The heap invalidates the whole table before
//!   moving or reclaiming cells, including split collections without an
//!   interpreter root registration. Publication is disabled during marking.
//! - The mutator alone reads and writes these slots, between safepoints.
//!
//! # See also
//! - `crate::isolate_names` installs the cache in the heap's isolate data.
//! - `crate::runtime_stubs::string` publishes after collecting allocations.

use std::cell::Cell;

use crate::Value;

const ENTRIES: usize = 64;
/// Log2 of the table entry count.
pub const CACHE_INDEX_BITS: u32 = ENTRIES.trailing_zeros();
/// Rotation of the right operand before folding into the index.
pub const CACHE_RIGHT_ROTATION: u32 = 11;
/// First bit of the mixed word used for the index.
pub const CACHE_INDEX_START: u32 = 3;
/// Log2 of an entry's size.
pub const CACHE_ENTRY_SHIFT: u32 = 5;
/// Byte offset of the left and right operands, adjacent in that order.
pub const CACHE_KEYS_BYTE: u32 = std::mem::offset_of!(Entry, left) as u32;
/// Byte offset of the result. Zero is an empty entry.
pub const CACHE_RESULT_BYTE: u32 = std::mem::offset_of!(Entry, result) as u32;

#[repr(C)]
#[derive(Clone, Copy)]
struct Entry {
    left: u64,
    right: u64,
    result: u64,
    padding: u64,
}

const _: () = assert!(std::mem::size_of::<Entry>() == 1 << CACHE_ENTRY_SHIFT);
const _: () = assert!(std::mem::offset_of!(Entry, right) == 8);

const EMPTY: Entry = Entry {
    left: 0,
    right: 0,
    result: 0,
    padding: 0,
};

pub(crate) struct ConcatCache {
    entries: [Cell<Entry>; ENTRIES],
}

impl ConcatCache {
    pub(crate) fn empty() -> Self {
        Self {
            entries: [const { Cell::new(EMPTY) }; ENTRIES],
        }
    }

    pub(crate) fn addr(&self) -> u64 {
        self.entries.as_ptr() as u64
    }

    pub(crate) fn clear(&self) {
        for slot in &self.entries {
            slot.set(EMPTY);
        }
    }

    /// Store exact current words after all possibly collecting operations.
    pub(crate) fn insert(&self, left: Value, right: Value, result: Value) {
        let left = left.to_bits();
        let right = right.to_bits();
        let hash = left ^ right.rotate_right(CACHE_RIGHT_ROTATION);
        let index = (hash >> CACHE_INDEX_START) as usize & (ENTRIES - 1);
        self.entries[index].set(Entry {
            left,
            right,
            result: result.to_bits(),
            padding: 0,
        });
    }
}
