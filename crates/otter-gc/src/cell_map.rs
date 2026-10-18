//! Cell map word: the first `u32` of a value cell's body.
//!
//! Every cell a JavaScript program can hold as a value carries, at the fixed
//! byte offset [`MAP_WORD_OFFSET`] from the cell start, one word that
//! identifies its layout. For an ordinary object the word is its hidden class
//! (the compressed offset of its shape cell); for every other value cell it is
//! a constant derived from the type tag. A shape check on a value of unknown
//! type is therefore one load and one compare: no other cell's map word can
//! equal a shape.
//!
//! # Contents
//!
//! - [`MAP_WORD_OFFSET`] — byte offset of the map word from the cell start.
//! - [`CellMap`] — the constant map word of a value cell without a shape.
//! - [`TypeMap`] — the first field of such a body, correct by construction.
//! - [`MapWord`] — what a body type promises about its first word.
//!
//! # Invariants
//!
//! - A shape word is a nonzero cage offset of a live cell, so it is a
//!   multiple of the cell alignment (8). A type map word has its low three
//!   bits set, so it never equals a shape word or zero.
//! - A body declaring [`MapWord::Type`] or [`MapWord::Shape`] keeps that word
//!   at body offset 0 for its whole life; allocation checks it in debug
//!   builds.
//! - Type map words fit a 12-bit compare immediate.
//! - The scavenger writes a forwarding offset over the map word of an
//!   evacuated from-space cell. The copy keeps the original word, and no
//!   mutator reads a from-space cell.
//!
//! # See also
//!
//! - [`crate::header`] — the 8-byte header the map word follows.
//! - [`crate::trace::Traceable::MAP_WORD`] — the per-type declaration.

use crate::header::HEADER_SIZE;

/// Byte offset of the map word from the start of a cell (header included).
pub const MAP_WORD_OFFSET: usize = HEADER_SIZE;

/// Low bits every type map word carries; no cell offset has them.
pub const TYPE_MAP_LOW_BITS: u32 = 0b111;

/// The map word of a value cell whose type alone fixes its layout.
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CellMap(u32);

impl CellMap {
    /// The map word of every cell with type tag `tag`.
    #[inline]
    #[must_use]
    pub const fn of_type(tag: u8) -> Self {
        Self(((tag as u32) << 3) | TYPE_MAP_LOW_BITS)
    }

    /// The raw word as stored at [`MAP_WORD_OFFSET`].
    #[inline]
    #[must_use]
    pub const fn word(self) -> u32 {
        self.0
    }
}

const _: () = assert!(CellMap::of_type(u8::MAX).word() < 1 << 12);

/// The `map` field of a value cell body with type tag `TAG`: the body's
/// first field, holding [`CellMap::of_type`]`(TAG)` by construction.
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct TypeMap<const TAG: u8>(u32);

impl<const TAG: u8> TypeMap<TAG> {
    /// The one value of this field.
    pub const NEW: Self = Self(CellMap::of_type(TAG).word());
}

impl<const TAG: u8> Default for TypeMap<TAG> {
    #[inline]
    fn default() -> Self {
        Self::NEW
    }
}

impl<const TAG: u8> std::fmt::Debug for TypeMap<TAG> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "TypeMap({:#x})", self.0)
    }
}

/// What a body type stores in its first word.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MapWord {
    /// An internal cell no program value refers to; no promise.
    Internal,
    /// A value cell whose first word is the offset of its shape cell.
    Shape,
    /// A value cell whose first word is this constant.
    Type(CellMap),
}

impl MapWord {
    /// The declaration for a value cell of type tag `tag` without a shape.
    #[inline]
    #[must_use]
    pub const fn of_type(tag: u8) -> Self {
        Self::Type(CellMap::of_type(tag))
    }

    /// Whether `word`, read at body offset 0, keeps this declaration.
    #[inline]
    #[must_use]
    pub const fn holds(self, word: u32) -> bool {
        match self {
            Self::Internal => true,
            Self::Shape => word != 0 && word & TYPE_MAP_LOW_BITS == 0,
            Self::Type(map) => word == map.word(),
        }
    }
}

/// Debug check that a freshly initialized body keeps its type's
/// [`crate::trace::Traceable::MAP_WORD`] declaration.
///
/// # Safety
/// `payload` must address a fully initialized `T` inside a heap cell.
#[inline]
pub(crate) unsafe fn debug_check_map_word<T: crate::trace::Traceable>(payload: *const T) {
    if cfg!(debug_assertions) && !matches!(T::MAP_WORD, MapWord::Internal) {
        // SAFETY: caller contract; a declaring body is at least one word.
        let word = unsafe { payload.cast::<u32>().read() };
        assert!(
            T::MAP_WORD.holds(word),
            "type tag {:#x} allocated with map word {word:#x}, declared {:?}",
            T::TYPE_TAG,
            T::MAP_WORD
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type_maps_never_look_like_cell_offsets() {
        for tag in 0..=u8::MAX {
            let word = CellMap::of_type(tag).word();
            assert_ne!(word % 8, 0);
            assert!(!MapWord::Shape.holds(word));
            assert!(MapWord::of_type(tag).holds(word));
        }
        assert!(MapWord::Shape.holds(0x4_0078));
        assert!(!MapWord::Shape.holds(0));
    }
}
