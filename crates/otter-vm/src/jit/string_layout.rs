//! One VM-owned payload and allocation geometry for primitive string cells.
//!
//! # Contents
//! - `JitStringLayout` describes reads and fully initialized native flat/rope cells.
//! - `Default` obtains the current layout from the actual VM representation owner.
//!
//! # Invariants
//! Compiled code receives offsets, representation tags, short-result limits and
//! an opaque young header (sized per cell) from the VM. It never duplicates enum layout or header flags.
//! Length lives in the cell; the content hash starts zero and the VM computes it
//! on first use, so generated code never hashes or maintains wrapper caches.
//! Children are compressed handles. Every payload/padding byte is initialized
//! before the shared LAB top is published; no data address crosses a safepoint.
//!
//! # See also
//! - `crate::string::gc_body` owns the physical body and hash domains.

/// Actual VM payload geometry and complete native string allocation recipe.
#[derive(Debug, Clone, Copy)]
pub struct JitStringLayout {
    /// Runtime cell type guarded at byte zero.
    pub string_type_tag: u8,
    /// Exact UTF-16 length word in the cell.
    pub string_len_byte: u32,
    /// Representation tag byte.
    pub string_repr_byte: u32,
    /// First trailing payload byte: a sequential string's first code unit, or
    /// a cons's or slice's first word.
    pub string_body_size: u32,
    /// Exact allocated cell size of a cons or slice.
    pub pair_cell_bytes: u32,
    /// A young string cell's header word without its size; the size goes in
    /// at [`Self::cell_size_shift`].
    pub young_header_word: u64,
    /// Bit position of the cell size within the header word.
    pub cell_size_shift: u8,
    /// First body word of a fresh cell: the map word, then a zero atom.
    pub map_atom_word: u64,
    /// Recorded-atom word; newly concatenated cells initialize it to zero,
    /// no atom recorded.
    pub atom_byte: u32,
    /// Hash word, zero in a fresh cell; it shares an eight-byte word with
    /// the length.
    pub hash_byte: u32,
    /// Compressed Cons children, measured by the representation owner.
    pub cons_left_byte: u32,
    /// Right child, adjacent to the left compressed handle.
    pub cons_right_byte: u32,
    /// Bounded rope depth byte, beside the tag.
    pub cons_depth_byte: u32,
    /// A slice's compressed parent, a flat string.
    pub sliced_parent_byte: u32,
    /// A slice's first code unit within its parent, a `u32`.
    pub sliced_start_byte: u32,
    /// Longest wide concatenation result copied into a sequential cell.
    pub short_concat_flat: u8,
    /// Longest Latin-1 concatenation result copied into a sequential cell.
    pub short_concat_latin1: u8,
    /// Maximum legal Cons depth, checked before candidate writes.
    pub max_rope_depth: u8,
    /// Sequential UTF-16 tag.
    pub seq_flat_tag: u8,
    /// Sequential Latin-1 tag.
    pub seq_latin1_tag: u8,
    /// Compressed-children rope tag.
    pub cons_tag: u8,
    /// Collapsed slice view tag.
    pub sliced_tag: u8,
}

impl JitStringLayout {
    /// The complete young header word of a cell of `bytes` bytes.
    #[must_use]
    pub const fn young_header(&self, bytes: u32) -> u64 {
        self.young_header_word | ((bytes as u64) << self.cell_size_shift)
    }
}

impl Default for JitStringLayout {
    fn default() -> Self {
        crate::string::gc_body::jit_layout()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn measured_allocation_geometry_has_nonoverlapping_complete_payload_fields() {
        let p = JitStringLayout::default();
        assert_ne!(p.string_type_tag, 0);
        assert_eq!(p.young_header_word as u8, p.string_type_tag);
        assert_eq!(
            p.young_header(p.pair_cell_bytes) >> 32,
            u64::from(p.pair_cell_bytes)
        );
        assert_eq!(p.pair_cell_bytes % otter_gc::page::CELL_SIZE as u32, 0);
        assert_eq!(p.atom_byte, otter_gc::cell_map::MAP_WORD_OFFSET as u32 + 4);
        assert_eq!(p.hash_byte, p.string_len_byte + 4);
        assert_eq!(p.string_len_byte % 8, 0, "length and hash share one word");
        let mut ranges = [
            (otter_gc::cell_map::MAP_WORD_OFFSET as u32, 4),
            (p.atom_byte, 4),
            (p.string_len_byte, 4),
            (p.hash_byte, 4),
            (p.string_repr_byte, 1),
            (p.cons_depth_byte, 1),
            (p.cons_left_byte, 4),
            (p.cons_right_byte, 4),
        ];
        ranges.sort();
        for pair in ranges.windows(2) {
            assert!(pair[0].0 + pair[0].1 <= pair[1].0);
        }
        for (start, len) in ranges {
            assert!(start >= otter_gc::header::HEADER_SIZE as u32);
            assert!(start + len <= p.pair_cell_bytes);
        }
        assert_eq!(p.cons_right_byte, p.cons_left_byte + 4);
        assert_eq!(p.cons_left_byte, p.string_body_size);
        assert_eq!(p.sliced_parent_byte, p.string_body_size);
    }
}
