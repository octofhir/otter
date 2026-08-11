//! Advanced SIMD classification and UTF-8 checking of one block.
//!
//! # Contents
//! - [`classify_neon`] — the kernel [`super::classifier`] hands out on
//!   aarch64.
//! - [`validate_neon`] — the UTF-8 checker that goes with it.
//!
//! # Invariants
//! - Advanced SIMD is part of the base aarch64 ABI, so the kernels need no
//!   runtime check and are always the ones selected.
//! - Loads read a fixed 64-byte array four vectors at a time, so no read can
//!   leave it.
//! - The results equal [`super::classify_scalar`] and
//!   [`super::validate_scalar`] for every input.
//!
//! # See also
//! - [`super`] — the tables and the definitions these answer to.

use core::arch::aarch64::{
    uint8x16_t, vaddv_u8, vandq_u8, vceqq_u8, vdupq_n_u8, veorq_u8, vextq_u8, vget_high_u8,
    vget_low_u8, vld1q_u8, vorrq_u8, vqsubq_u8, vqtbl1q_u8, vshrq_n_u8, vtstq_u8,
};

use super::utf8::{
    FOURTH_BYTE_BIAS, LEAD_HIGH, LEAD_LOW, NONCHAR_LEAD, NONCHAR_SECOND, NONCHAR_THIRD,
    NONCHAR_THIRD_MASK, PRECEDING, SECOND_HIGH, THIRD_BYTE_BIAS,
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

/// Check one block's UTF-8 with Advanced SIMD.
#[must_use]
pub fn validate_neon(prev: &[u8; PRECEDING], block: &[u8; BLOCK]) -> u64 {
    // SAFETY: Advanced SIMD is guaranteed by the aarch64 ABI, and `validate`
    // reads only the two 64-byte arrays it was given.
    unsafe { validate(prev, block) }
}

#[target_feature(enable = "neon")]
unsafe fn validate(prev: &[u8; PRECEDING], block: &[u8; BLOCK]) -> u64 {
    // SAFETY: every load below stays inside `prev`, `block` or a table that
    // is one vector wide.
    unsafe {
        let lead_high = vld1q_u8(LEAD_HIGH.as_ptr());
        let lead_low = vld1q_u8(LEAD_LOW.as_ptr());
        let second_high = vld1q_u8(SECOND_HIGH.as_ptr());
        let nibble = vdupq_n_u8(0x0F);
        let high_bit = vdupq_n_u8(0x80);
        let third_bias = vdupq_n_u8(THIRD_BYTE_BIAS);
        let fourth_bias = vdupq_n_u8(FOURTH_BYTE_BIAS);
        let noncharacter_lead = vdupq_n_u8(NONCHAR_LEAD);
        let noncharacter_second = vdupq_n_u8(NONCHAR_SECOND);
        let noncharacter_third = vdupq_n_u8(NONCHAR_THIRD);
        let noncharacter_mask = vdupq_n_u8(NONCHAR_THIRD_MASK);

        let mut error = 0u64;
        for lane in 0..BLOCK / 16 {
            let bytes = vld1q_u8(block.as_ptr().add(lane * 16));
            // The three bytes before this vector come from the vector before
            // it — the tail of `prev` for the first one.
            let before = if lane == 0 {
                vld1q_u8(prev.as_ptr().add(PRECEDING - 16))
            } else {
                vld1q_u8(block.as_ptr().add((lane - 1) * 16))
            };
            let prev1 = vextq_u8::<15>(before, bytes);
            let prev2 = vextq_u8::<14>(before, bytes);
            let prev3 = vextq_u8::<13>(before, bytes);

            let special = vandq_u8(
                vandq_u8(
                    vqtbl1q_u8(lead_high, vshrq_n_u8::<4>(prev1)),
                    vqtbl1q_u8(lead_low, vandq_u8(prev1, nibble)),
                ),
                vqtbl1q_u8(second_high, vshrq_n_u8::<4>(bytes)),
            );
            // Saturating subtraction leaves the high bit set exactly for the
            // leads that demand a byte here.
            let must_continue = vandq_u8(
                vorrq_u8(vqsubq_u8(prev2, third_bias), vqsubq_u8(prev3, fourth_bias)),
                high_bit,
            );
            let malformed = veorq_u8(must_continue, special);

            // U+FFFE / U+FFFF: well-formed UTF-8 the `Char` production bars.
            let noncharacter = vandq_u8(
                vandq_u8(
                    vceqq_u8(prev2, noncharacter_lead),
                    vceqq_u8(prev1, noncharacter_second),
                ),
                vceqq_u8(vandq_u8(bytes, noncharacter_mask), noncharacter_third),
            );

            let wrong = vorrq_u8(malformed, noncharacter);
            error |= u64::from(movemask(vtstq_u8(wrong, wrong))) << (lane * 16);
        }
        error
    }
}
