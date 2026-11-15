//! Integer indices for optional heap verification.
//!
//! # Contents
//! - Full-width slot-address coverage and sparse incoming cage-offset owners.
//! - A fixed integer mixer for these engine-generated keys.
//!
//! # Invariants
//! - Keys are slot addresses and compressed cell offsets, never user strings.
//! - Slot coverage compares every address bit. Only aligned cage offsets can
//!   name walked headers; unaligned incoming offsets never cover a header.
//! - Incoming owners retain the last referencing header in traversal order.
//! - Sparse rows allocate only for referenced address ranges and die per pass.
//! - Verification walks determine diagnostic order; neither index is iterated.
//! - These indices are created only under the opt-in GC verification gate.
//!
//! # See also
//! - [`super::GcHeap::verify_remembered_complete`] owns the exhaustive proof.

use std::collections::HashSet;
use std::hash::{BuildHasherDefault, Hasher};

pub(super) type SlotCoverage = HashSet<usize, BuildHasherDefault<VerificationHasher>>;
/// One row covers 8 KiB of cage addresses with one word per aligned header.
/// The directory is bounded by the 32-bit cage and does not reserve rows for
/// untouched address ranges. Zero is not a walkable header offset.
const OWNER_ROW_LEN: usize = 1024;
const OWNER_ROW_SPAN: usize = OWNER_ROW_LEN * crate::OBJECT_ALIGNMENT;

#[derive(Default)]
pub(super) struct ReferenceOwners {
    rows: Vec<Option<Box<[u32; OWNER_ROW_LEN]>>>,
}

impl ReferenceOwners {
    pub(super) fn insert(&mut self, child: u32, owner: u32) {
        // No walked body can have an unaligned header. Ignoring such an
        // incoming offset preserves exact lookup of every legal header;
        // rounding it down would incorrectly make an unrelated body live.
        if !(child as usize).is_multiple_of(crate::OBJECT_ALIGNMENT) {
            return;
        }
        let row = child as usize / OWNER_ROW_SPAN;
        let column = child as usize / crate::OBJECT_ALIGNMENT % OWNER_ROW_LEN;
        if row >= self.rows.len() {
            self.rows.resize_with(row + 1, || None);
        }
        let entries = self.rows[row].get_or_insert_with(|| Box::new([0; OWNER_ROW_LEN]));
        entries[column] = owner;
    }

    pub(super) fn get(&self, child: u32) -> Option<u32> {
        if !(child as usize).is_multiple_of(crate::OBJECT_ALIGNMENT) {
            return None;
        }
        let row = child as usize / OWNER_ROW_SPAN;
        let column = child as usize / crate::OBJECT_ALIGNMENT % OWNER_ROW_LEN;
        let owner = self.rows.get(row)?.as_ref()?[column];
        (owner != 0).then_some(owner)
    }
}

/// A non-cryptographic mixer for engine-owned integer addresses. General
/// runtime maps retain their existing hash policy for user-derived keys.
#[derive(Default)]
pub(super) struct VerificationHasher(u64);

impl Hasher for VerificationHasher {
    fn finish(&self) -> u64 {
        let mut word = self.0;
        word = (word ^ (word >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        word = (word ^ (word >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        word ^ (word >> 31)
    }

    fn write_u32(&mut self, word: u32) {
        self.0 = u64::from(word);
    }

    fn write_usize(&mut self, word: usize) {
        self.0 = word as u64;
    }

    fn write(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 = (self.0 ^ u64::from(*byte)).wrapping_mul(0x100_0000_01b3);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sparse_owners_preserve_exact_headers_and_last_writer() {
        let mut owners = ReferenceOwners::default();
        let keys = [8, 16, 8184, 8192, 8200, 1 << 20, u32::MAX - 7];
        for (i, &key) in keys.iter().enumerate() {
            assert_eq!(owners.get(key), None);
            owners.insert(key, (i as u32 + 1) * 8);
        }
        for (i, &key) in keys.iter().enumerate() {
            assert_eq!(owners.get(key), Some((i as u32 + 1) * 8));
            for low in 1..8 {
                owners.insert(key + low, 80);
                assert_eq!(owners.get(key + low), None);
                assert_eq!(owners.get(key), Some((i as u32 + 1) * 8));
            }
        }
        owners.insert(8192, 88);
        assert_eq!(owners.get(8192), Some(88));
        assert_eq!(owners.get(24), None);
        assert_eq!(owners.rows.iter().filter(|row| row.is_some()).count(), 4);
    }

    #[test]
    fn slot_coverage_keeps_high_address_bits() {
        let mut covered = SlotCoverage::default();
        covered.insert(0x1234_5678);
        assert!(!covered.contains(&0x1_1234_5678));
        covered.insert(0x1_1234_5678);
        assert_eq!(covered.len(), 2);
    }
}
