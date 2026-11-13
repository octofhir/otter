//! Integer indices for optional heap verification.
//!
//! # Contents
//! - Full-width slot-address coverage and incoming cage-offset owners.
//! - A fixed integer mixer for these engine-generated keys.
//!
//! # Invariants
//! - Keys are slot addresses and compressed cell offsets, never user strings.
//! - Key equality still compares every bit; hashing cannot merge references.
//! - Verification walks determine diagnostic order; neither index is iterated.
//! - These indices are created only under the opt-in GC verification gate.
//!
//! # See also
//! - [`super::GcHeap::verify_remembered_complete`] owns the exhaustive proof.

use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasherDefault, Hasher};

pub(super) type SlotCoverage = HashSet<usize, BuildHasherDefault<VerificationHasher>>;
pub(super) type ReferenceOwners = HashMap<u32, usize, BuildHasherDefault<VerificationHasher>>;

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
