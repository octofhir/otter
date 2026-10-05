//! One VM-owned payload and allocation geometry for primitive string cells.
//!
//! # Contents
//! - `JitStringLayout` describes reads and fully initialized native flat/rope cells.
//! - `Default` obtains the current layout from the actual VM representation owner.
//!
//! # Invariants
//! Compiled code receives offsets, representation tags, capacities and an opaque
//! young header from the VM. It never duplicates enum layout or header flags.
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
    /// Representation discriminant and union payload.
    pub string_repr_byte: u32,
    /// Byte immediately after the discriminant's alignment padding.
    pub string_repr_payload_byte: u32,
    /// First trailing unit/byte for sequential representations.
    pub string_body_size: u32,
    /// Exact allocated cell size for inline and Cons representations.
    pub cell_bytes: u32,
    /// Opaque fully sized young GC header word.
    pub header_word: u64,
    /// Interner identity word; newly concatenated cells initialize it to zero.
    pub id_byte: u32,
    /// Compressed optional UTF-16 cache; initialized to null.
    pub cache_byte: u32,
    /// Compressed Cons children, measured by the representation owner.
    pub cons_left_byte: u32,
    /// Right child, adjacent to the left compressed handle.
    pub cons_right_byte: u32,
    /// Bounded rope depth byte.
    pub cons_depth_byte: u32,
    /// Native short-result capacities and maximum rope depth.
    pub inline_flat_cap: u8,
    /// Latin-1 inline byte capacity.
    pub inline_latin1_cap: u8,
    /// Maximum legal Cons depth, checked before candidate writes.
    pub max_rope_depth: u8,
    /// Tags of the six physical representations.
    pub inline_flat_tag: u8,
    /// Sequential UTF-16 tag.
    pub seq_flat_tag: u8,
    /// Inline Latin-1 tag.
    pub inline_latin1_tag: u8,
    /// Sequential Latin-1 tag.
    pub seq_latin1_tag: u8,
    /// Compressed-children rope tag.
    pub cons_tag: u8,
    /// Collapsed slice view tag.
    pub sliced_tag: u8,
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
        assert_eq!(p.header_word as u8, p.string_type_tag);
        assert_eq!((p.header_word >> 32) as u32, p.cell_bytes);
        assert_eq!(p.cell_bytes % otter_gc::page::CELL_SIZE as u32, 0);
        let mut ranges = [
            (p.id_byte, 4),
            (p.string_len_byte, 4),
            (p.string_repr_byte, 1),
            (p.cons_left_byte, 4),
            (p.cons_right_byte, 4),
            (p.cons_depth_byte, 1),
            (p.cache_byte, 4),
        ];
        ranges.sort();
        for pair in ranges.windows(2) {
            assert!(pair[0].0 + pair[0].1 <= pair[1].0);
        }
        for (start, len) in ranges {
            assert!(start >= otter_gc::header::HEADER_SIZE as u32);
            assert!(start + len <= p.cell_bytes);
        }
        assert_eq!(p.cons_right_byte, p.cons_left_byte + 4);
        assert!(p.string_repr_payload_byte + 2 * u32::from(p.inline_flat_cap) <= p.cell_bytes);
        assert!(p.string_repr_payload_byte + u32::from(p.inline_latin1_cap) <= p.cell_bytes);
    }
}
