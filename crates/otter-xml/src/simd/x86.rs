//! SSE and AVX classification and UTF-8 checking of one block.
//!
//! # Contents
//! - [`classify_avx2`] / [`validate_avx2`] — 32 bytes per step.
//! - [`classify_ssse3`] / [`validate_ssse3`] — 16 bytes per step, for
//!   machines without AVX2.
//!
//! # Invariants
//! - Every kernel is reached only through [`super::kernels`], which checks the
//!   feature first; none is called on a machine that lacks it.
//! - Loads read a fixed 64-byte array, so no read can leave it.
//! - The results equal [`super::classify_scalar`] and
//!   [`super::validate_scalar`] for every input.
//! - The shuffle index is masked to a nibble, so no lane is zeroed by the
//!   shuffle's own high-bit rule instead of by the tables.
//!
//! # See also
//! - [`super`] — the tables and the definitions these answer to.

use core::arch::x86_64::{
    __m128i, __m256i, _mm_alignr_epi8, _mm_and_si128, _mm_cmpeq_epi8, _mm_loadu_si128,
    _mm_movemask_epi8, _mm_or_si128, _mm_set1_epi8, _mm_setzero_si128, _mm_shuffle_epi8,
    _mm_srli_epi16, _mm_subs_epu8, _mm_xor_si128, _mm256_alignr_epi8, _mm256_and_si256,
    _mm256_broadcastsi128_si256, _mm256_cmpeq_epi8, _mm256_loadu_si256, _mm256_movemask_epi8,
    _mm256_or_si256, _mm256_permute2x128_si256, _mm256_set1_epi8, _mm256_setzero_si256,
    _mm256_shuffle_epi8, _mm256_srli_epi16, _mm256_subs_epu8, _mm256_xor_si256,
};

use super::utf8::{
    FOURTH_BYTE_BIAS, LEAD_HIGH, LEAD_LOW, NONCHAR_LEAD, NONCHAR_SECOND, NONCHAR_THIRD,
    NONCHAR_THIRD_MASK, PRECEDING, SECOND_HIGH, THIRD_BYTE_BIAS,
};
use super::{BLOCK, FORBIDDEN_BITS, LUT_HI, LUT_LO, Masks, STRUCTURAL_BITS};

/// Classify one block with AVX2.
#[must_use]
pub fn classify_avx2(block: &[u8; BLOCK]) -> Masks {
    // SAFETY: the caller reached this through `classifier`, which detected
    // AVX2; the loads stay inside `block`.
    unsafe { avx2(block) }
}

/// Classify one block with SSSE3.
#[must_use]
pub fn classify_ssse3(block: &[u8; BLOCK]) -> Masks {
    // SAFETY: the caller reached this through `classifier`, which detected
    // SSSE3; the loads stay inside `block`.
    unsafe { ssse3(block) }
}

#[target_feature(enable = "avx2")]
unsafe fn avx2(block: &[u8; BLOCK]) -> Masks {
    // SAFETY: every load below stays inside `block` or inside a table that is
    // one 128-bit vector wide.
    unsafe {
        let lut_lo = _mm256_broadcastsi128_si256(_mm_loadu_si128(LUT_LO.as_ptr().cast()));
        let lut_hi = _mm256_broadcastsi128_si256(_mm_loadu_si128(LUT_HI.as_ptr().cast()));
        let nibble = _mm256_set1_epi8(0x0F);
        let structural_bits = _mm256_set1_epi8(STRUCTURAL_BITS as i8);
        let forbidden_bits = _mm256_set1_epi8(FORBIDDEN_BITS as i8);
        let zero = _mm256_setzero_si256();

        let mut masks = Masks::default();
        for lane in 0..BLOCK / 32 {
            let bytes = _mm256_loadu_si256(block.as_ptr().add(lane * 32).cast());
            let low = _mm256_shuffle_epi8(lut_lo, _mm256_and_si256(bytes, nibble));
            // There is no byte-wise shift; shifting 16-bit lanes and masking
            // off what bled in from the neighbour gives the high nibble.
            let high_index = _mm256_and_si256(_mm256_srli_epi16::<4>(bytes), nibble);
            let high = _mm256_shuffle_epi8(lut_hi, high_index);
            let class = _mm256_and_si256(low, high);

            // `cmpeq` against zero marks the bytes *without* the bits, so the
            // move-mask is inverted to get the bytes with them.
            let absent_structural =
                _mm256_cmpeq_epi8(_mm256_and_si256(class, structural_bits), zero);
            let absent_forbidden = _mm256_cmpeq_epi8(_mm256_and_si256(class, forbidden_bits), zero);

            let shift = lane * 32;
            masks.structural |=
                u64::from(!(_mm256_movemask_epi8(absent_structural) as u32)) << shift;
            masks.forbidden |= u64::from(!(_mm256_movemask_epi8(absent_forbidden) as u32)) << shift;
            // A byte is non-ASCII exactly when its sign bit is set.
            masks.nonascii |= u64::from(_mm256_movemask_epi8(bytes) as u32) << shift;
        }
        masks
    }
}

#[target_feature(enable = "ssse3")]
unsafe fn ssse3(block: &[u8; BLOCK]) -> Masks {
    // SAFETY: every load below stays inside `block` or inside a table that is
    // one vector wide.
    unsafe {
        let lut_lo: __m128i = _mm_loadu_si128(LUT_LO.as_ptr().cast());
        let lut_hi: __m128i = _mm_loadu_si128(LUT_HI.as_ptr().cast());
        let nibble = _mm_set1_epi8(0x0F);
        let structural_bits = _mm_set1_epi8(STRUCTURAL_BITS as i8);
        let forbidden_bits = _mm_set1_epi8(FORBIDDEN_BITS as i8);
        let zero = _mm_setzero_si128();

        let mut masks = Masks::default();
        for lane in 0..BLOCK / 16 {
            let bytes = _mm_loadu_si128(block.as_ptr().add(lane * 16).cast());
            let low = _mm_shuffle_epi8(lut_lo, _mm_and_si128(bytes, nibble));
            let high_index = _mm_and_si128(_mm_srli_epi16::<4>(bytes), nibble);
            let high = _mm_shuffle_epi8(lut_hi, high_index);
            let class = _mm_and_si128(low, high);

            let absent_structural = _mm_cmpeq_epi8(_mm_and_si128(class, structural_bits), zero);
            let absent_forbidden = _mm_cmpeq_epi8(_mm_and_si128(class, forbidden_bits), zero);

            let shift = lane * 16;
            masks.structural |= u64::from(!(_mm_movemask_epi8(absent_structural) as u16)) << shift;
            masks.forbidden |= u64::from(!(_mm_movemask_epi8(absent_forbidden) as u16)) << shift;
            masks.nonascii |= u64::from(_mm_movemask_epi8(bytes) as u16) << shift;
        }
        masks
    }
}

/// Check one block's UTF-8 with AVX2.
#[must_use]
pub fn validate_avx2(prev: &[u8; PRECEDING], block: &[u8; BLOCK]) -> u64 {
    // SAFETY: the caller reached this through `kernels`, which detected AVX2;
    // the loads stay inside the two arrays given.
    unsafe { validate_with_avx2(prev, block) }
}

/// Check one block's UTF-8 with SSSE3.
#[must_use]
pub fn validate_ssse3(prev: &[u8; PRECEDING], block: &[u8; BLOCK]) -> u64 {
    // SAFETY: the caller reached this through `kernels`, which detected
    // SSSE3; the loads stay inside the two arrays given.
    unsafe { validate_with_ssse3(prev, block) }
}

#[target_feature(enable = "avx2")]
unsafe fn validate_with_avx2(prev: &[u8; PRECEDING], block: &[u8; BLOCK]) -> u64 {
    // SAFETY: every load below stays inside `prev`, `block` or a table that
    // is one 128-bit vector wide.
    unsafe {
        let lead_high = _mm256_broadcastsi128_si256(_mm_loadu_si128(LEAD_HIGH.as_ptr().cast()));
        let lead_low = _mm256_broadcastsi128_si256(_mm_loadu_si128(LEAD_LOW.as_ptr().cast()));
        let second_high = _mm256_broadcastsi128_si256(_mm_loadu_si128(SECOND_HIGH.as_ptr().cast()));
        let nibble = _mm256_set1_epi8(0x0F);
        let high_bit = _mm256_set1_epi8(0x80u8 as i8);
        let third_bias = _mm256_set1_epi8(THIRD_BYTE_BIAS as i8);
        let fourth_bias = _mm256_set1_epi8(FOURTH_BYTE_BIAS as i8);
        let noncharacter_lead = _mm256_set1_epi8(NONCHAR_LEAD as i8);
        let noncharacter_second = _mm256_set1_epi8(NONCHAR_SECOND as i8);
        let noncharacter_third = _mm256_set1_epi8(NONCHAR_THIRD as i8);
        let noncharacter_mask = _mm256_set1_epi8(NONCHAR_THIRD_MASK as i8);
        let zero = _mm256_setzero_si256();

        let mut error = 0u64;
        for lane in 0..BLOCK / 32 {
            let bytes: __m256i = _mm256_loadu_si256(block.as_ptr().add(lane * 32).cast());
            // The bytes before this vector come from the vector before it —
            // the tail of `prev` for the first one.
            let before: __m256i = if lane == 0 {
                _mm256_loadu_si256(prev.as_ptr().add(PRECEDING - 32).cast())
            } else {
                _mm256_loadu_si256(block.as_ptr().add((lane - 1) * 32).cast())
            };
            // `alignr` works within each 128-bit half, so the halves are
            // re-paired first: low half gets `before`'s high half, high half
            // gets `bytes`'s low half.
            let straddle = _mm256_permute2x128_si256(before, bytes, 0x21);
            let prev1 = _mm256_alignr_epi8(bytes, straddle, 15);
            let prev2 = _mm256_alignr_epi8(bytes, straddle, 14);
            let prev3 = _mm256_alignr_epi8(bytes, straddle, 13);

            let special = _mm256_and_si256(
                _mm256_and_si256(
                    _mm256_shuffle_epi8(
                        lead_high,
                        _mm256_and_si256(_mm256_srli_epi16::<4>(prev1), nibble),
                    ),
                    _mm256_shuffle_epi8(lead_low, _mm256_and_si256(prev1, nibble)),
                ),
                _mm256_shuffle_epi8(
                    second_high,
                    _mm256_and_si256(_mm256_srli_epi16::<4>(bytes), nibble),
                ),
            );
            let must_continue = _mm256_and_si256(
                _mm256_or_si256(
                    _mm256_subs_epu8(prev2, third_bias),
                    _mm256_subs_epu8(prev3, fourth_bias),
                ),
                high_bit,
            );
            let malformed = _mm256_xor_si256(must_continue, special);

            let noncharacter = _mm256_and_si256(
                _mm256_and_si256(
                    _mm256_cmpeq_epi8(prev2, noncharacter_lead),
                    _mm256_cmpeq_epi8(prev1, noncharacter_second),
                ),
                _mm256_cmpeq_epi8(
                    _mm256_and_si256(bytes, noncharacter_mask),
                    noncharacter_third,
                ),
            );

            let wrong = _mm256_or_si256(malformed, noncharacter);
            // `cmpeq` against zero marks the bytes that are *right*, so the
            // move-mask is inverted.
            let right = _mm256_cmpeq_epi8(wrong, zero);
            error |= u64::from(!(_mm256_movemask_epi8(right) as u32)) << (lane * 32);
        }
        error
    }
}

#[target_feature(enable = "ssse3")]
unsafe fn validate_with_ssse3(prev: &[u8; PRECEDING], block: &[u8; BLOCK]) -> u64 {
    // SAFETY: every load below stays inside `prev`, `block` or a table that
    // is one vector wide.
    unsafe {
        let lead_high: __m128i = _mm_loadu_si128(LEAD_HIGH.as_ptr().cast());
        let lead_low: __m128i = _mm_loadu_si128(LEAD_LOW.as_ptr().cast());
        let second_high: __m128i = _mm_loadu_si128(SECOND_HIGH.as_ptr().cast());
        let nibble = _mm_set1_epi8(0x0F);
        let high_bit = _mm_set1_epi8(0x80u8 as i8);
        let third_bias = _mm_set1_epi8(THIRD_BYTE_BIAS as i8);
        let fourth_bias = _mm_set1_epi8(FOURTH_BYTE_BIAS as i8);
        let noncharacter_lead = _mm_set1_epi8(NONCHAR_LEAD as i8);
        let noncharacter_second = _mm_set1_epi8(NONCHAR_SECOND as i8);
        let noncharacter_third = _mm_set1_epi8(NONCHAR_THIRD as i8);
        let noncharacter_mask = _mm_set1_epi8(NONCHAR_THIRD_MASK as i8);
        let zero = _mm_setzero_si128();

        let mut error = 0u64;
        for lane in 0..BLOCK / 16 {
            let bytes: __m128i = _mm_loadu_si128(block.as_ptr().add(lane * 16).cast());
            let before: __m128i = if lane == 0 {
                _mm_loadu_si128(prev.as_ptr().add(PRECEDING - 16).cast())
            } else {
                _mm_loadu_si128(block.as_ptr().add((lane - 1) * 16).cast())
            };
            let prev1 = _mm_alignr_epi8(bytes, before, 15);
            let prev2 = _mm_alignr_epi8(bytes, before, 14);
            let prev3 = _mm_alignr_epi8(bytes, before, 13);

            let special = _mm_and_si128(
                _mm_and_si128(
                    _mm_shuffle_epi8(lead_high, _mm_and_si128(_mm_srli_epi16::<4>(prev1), nibble)),
                    _mm_shuffle_epi8(lead_low, _mm_and_si128(prev1, nibble)),
                ),
                _mm_shuffle_epi8(
                    second_high,
                    _mm_and_si128(_mm_srli_epi16::<4>(bytes), nibble),
                ),
            );
            let must_continue = _mm_and_si128(
                _mm_or_si128(
                    _mm_subs_epu8(prev2, third_bias),
                    _mm_subs_epu8(prev3, fourth_bias),
                ),
                high_bit,
            );
            let malformed = _mm_xor_si128(must_continue, special);

            let noncharacter = _mm_and_si128(
                _mm_and_si128(
                    _mm_cmpeq_epi8(prev2, noncharacter_lead),
                    _mm_cmpeq_epi8(prev1, noncharacter_second),
                ),
                _mm_cmpeq_epi8(_mm_and_si128(bytes, noncharacter_mask), noncharacter_third),
            );

            let wrong = _mm_or_si128(malformed, noncharacter);
            let right = _mm_cmpeq_epi8(wrong, zero);
            error |= u64::from(!(_mm_movemask_epi8(right) as u16)) << (lane * 16);
        }
        error
    }
}
