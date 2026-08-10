//! Advanced SIMD classification of one block.
//!
//! # Contents
//! - [`classify_neon`] — the kernel [`super::classifier`] hands out on
//!   aarch64.
//!
//! # Invariants
//! - Advanced SIMD is part of the base aarch64 ABI, so the kernel needs no
//!   runtime check and is always the one selected.
//! - Loads read a fixed 64-byte array four vectors at a time, so no read can
//!   leave it.
//! - The result equals [`super::classify_scalar`] for every input.
//!
//! # See also
//! - [`super`] — the tables and the definition this answers to.

use core::arch::aarch64::{
    uint8x16_t, vaddv_u8, vandq_u8, vdupq_n_u8, vget_high_u8, vget_low_u8, vld1q_u8, vqtbl1q_u8,
    vshrq_n_u8, vtstq_u8,
};

use super::{BLOCK, FORBIDDEN_BITS, LUT_HI, LUT_LO, Masks, STRUCTURAL_BITS};

/// Classify one block with Advanced SIMD.
#[must_use]
pub fn classify_neon(block: &[u8; BLOCK]) -> Masks {
    // SAFETY: Advanced SIMD is guaranteed by the aarch64 ABI, and `neon`
    // reads only the 64 bytes it was given.
    unsafe { neon(block) }
}

/// Gather the high bit of each byte of a comparison result into one word.
///
/// Advanced SIMD has no move-mask instruction; weighting each lane by its bit
/// position and adding the halves across is the standard stand-in.
#[target_feature(enable = "neon")]
unsafe fn movemask(vector: uint8x16_t) -> u16 {
    const BITS: [u8; 16] = [1, 2, 4, 8, 16, 32, 64, 128, 1, 2, 4, 8, 16, 32, 64, 128];
    // SAFETY: `BITS` is exactly one vector wide.
    let weighted = unsafe { vandq_u8(vector, vld1q_u8(BITS.as_ptr())) };
    let low = u16::from(vaddv_u8(vget_low_u8(weighted)));
    let high = u16::from(vaddv_u8(vget_high_u8(weighted)));
    (high << 8) | low
}

#[target_feature(enable = "neon")]
unsafe fn neon(block: &[u8; BLOCK]) -> Masks {
    // SAFETY: every load below stays inside `block`, and the two tables are
    // one vector wide each.
    unsafe {
        let lut_lo = vld1q_u8(LUT_LO.as_ptr());
        let lut_hi = vld1q_u8(LUT_HI.as_ptr());
        let nibble = vdupq_n_u8(0x0F);
        let structural_bits = vdupq_n_u8(STRUCTURAL_BITS);
        let forbidden_bits = vdupq_n_u8(FORBIDDEN_BITS);
        let sign_bit = vdupq_n_u8(0x80);

        let mut masks = Masks::default();
        for lane in 0..BLOCK / 16 {
            let bytes = vld1q_u8(block.as_ptr().add(lane * 16));
            let low = vqtbl1q_u8(lut_lo, vandq_u8(bytes, nibble));
            let high = vqtbl1q_u8(lut_hi, vshrq_n_u8::<4>(bytes));
            let class = vandq_u8(low, high);

            let shift = lane * 16;
            masks.structural |= u64::from(movemask(vtstq_u8(class, structural_bits))) << shift;
            masks.forbidden |= u64::from(movemask(vtstq_u8(class, forbidden_bits))) << shift;
            masks.nonascii |= u64::from(movemask(vtstq_u8(bytes, sign_bit))) << shift;
        }
        masks
    }
}
