//! Cached decimal text of Numbers, laid out for generated code.
//!
//! # Contents
//! - [`NumberStrings`] — the decimal strings of the small non-negative
//!   integers by value, and those of other recently converted Numbers in a
//!   direct-mapped table keyed by their bits.
//! - [`NumberStringEntry`] — one table entry: the bits and their string.
//! - Layout constants generated code reads both tables through.
//!
//! # Invariants
//! - An empty small slot or entry holds `undefined`; a filled one holds the
//!   string `ToString` gives its Number, which never changes.
//! - An entry's index is [`number_string_index`] of its bits; generated code
//!   computes the same index.
//! - The tables live in one boxed cell whose address never changes while the
//!   isolate does. Every string is traced as a root, so a collection rewrites
//!   it in place.
//!
//! # See also
//! - `super::concat` fills the tables on conversion.

use crate::Value;

/// Upper bound (exclusive) of the small integers cached by value.
pub const NUMBER_STRINGS_SMALL: usize = 1024;
/// Entries of the direct-mapped table; a power of two.
pub const NUMBER_STRINGS_ENTRIES: usize = 1024;
/// Byte offset of the first entry from the start of [`NumberStrings`].
pub const NUMBER_STRINGS_ENTRIES_BYTE: usize = std::mem::offset_of!(NumberStrings, entries);
/// Byte offset of an entry's string from the start of the entry.
pub const NUMBER_STRING_ENTRY_STRING_BYTE: usize = std::mem::offset_of!(NumberStringEntry, string);
/// Log2 of an entry's size.
pub const NUMBER_STRING_ENTRY_SHIFT: u32 = 4;

/// One direct-mapped entry.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct NumberStringEntry {
    /// The Number's IEEE bits.
    pub bits: u64,
    /// Its decimal string, or `undefined` while the entry is empty.
    pub string: Value,
}

/// Both tables.
#[repr(C)]
#[derive(Debug)]
pub struct NumberStrings {
    /// The decimal string of each small integer, or `undefined`.
    pub small: [Value; NUMBER_STRINGS_SMALL],
    /// Other Numbers' strings, by [`number_string_index`] of their bits.
    pub entries: [NumberStringEntry; NUMBER_STRINGS_ENTRIES],
}

const _: () = assert!(std::mem::size_of::<NumberStringEntry>() == 1 << NUMBER_STRING_ENTRY_SHIFT);

impl NumberStrings {
    /// Empty tables.
    #[must_use]
    pub(crate) fn empty() -> Box<Self> {
        Box::new(Self {
            small: [Value::undefined(); NUMBER_STRINGS_SMALL],
            entries: [NumberStringEntry {
                bits: 0,
                string: Value::undefined(),
            }; NUMBER_STRINGS_ENTRIES],
        })
    }

    /// Every cached string.
    pub(crate) fn strings(&self) -> impl Iterator<Item = &Value> {
        self.small
            .iter()
            .chain(self.entries.iter().map(|entry| &entry.string))
            .filter(|value| !value.is_undefined())
    }
}

/// The entry index of a Number with `bits`.
#[must_use]
pub const fn number_string_index(bits: u64) -> usize {
    ((bits ^ (bits >> 32)) as usize) & (NUMBER_STRINGS_ENTRIES - 1)
}
