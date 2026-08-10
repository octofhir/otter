//! SSE and AVX classification of one block.
//!
//! # Contents
//! - [`classify_avx2`] — 32 bytes per step.
//! - [`classify_ssse3`] — 16 bytes per step, for machines without AVX2.
//!
//! # Invariants
//! - Both kernels are reached only through [`super::classifier`], which checks
//!   the feature first; neither is called on a machine that lacks it.
//! - Loads read a fixed 64-byte array, so no read can leave it.
//! - Both results equal [`super::classify_scalar`] for every input.
//! - The shuffle index is masked to a nibble, so no lane is zeroed by the
//!   shuffle's own high-bit rule instead of by the tables.
//!
//! # See also
//! - [`super`] — the tables and the definition these answer to.

use core::arch::x86_64::{
    __m128i, _mm_and_si128, _mm_cmpeq_epi8, _mm_loadu_si128, _mm_movemask_epi8, _mm_set1_epi8,
    _mm_setzero_si128, _mm_shuffle_epi8, _mm_srli_epi16, _mm256_and_si256,
    _mm256_broadcastsi128_si256, _mm256_cmpeq_epi8, _mm256_loadu_si256, _mm256_movemask_epi8,
    _mm256_set1_epi8, _mm256_setzero_si256, _mm256_shuffle_epi8, _mm256_srli_epi16,
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
