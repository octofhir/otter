//! Vector classification and checking of one block of document bytes.
//!
//! # Contents
//! - [`Masks`] — the three bit sets one 64-byte block yields.
//! - [`Classifier`] — what a block of bytes holds; [`Validator`] — whether
//!   its UTF-8 is legal; [`UnitClassifier`] — what a block of UTF-16 units
//!   holds.
//! - [`Kernels`] — the set an instruction set provides, picked together.
//! - [`LUT_LO`] / [`LUT_HI`] — the nibble tables the kernels share with the
//!   portable classifier, so the two cannot drift apart.
//!
//! # Invariants
//! - A kernel answers for exactly 64 bytes; callers pad a short tail with a
//!   byte that classifies as nothing (a space).
//! - Bit `n` of every mask describes byte `n` of the block.
//! - A machine runs one instruction set's kernels or another's, never one of
//!   each: [`Kernels`] is what the dispatcher hands out. The unit kernel is
//!   the exception the x86 side states in place: 16-bit lanes pack across a
//!   256-bit vector's halves, so both x86 sets share one.
//! - Every kernel returns what [`classify_scalar`] / [`validate_scalar`] /
//!   [`classify_units_scalar`] return. That is a test, not a hope: `tests/index_agreement.rs` runs them
//!   over random input.
//! - The tables classify by nibble pair: `LUT_LO[low] & LUT_HI[high]` is
//!   non-zero only for bytes that are structural or forbidden, which is what
//!   lets one shuffle pair replace a ladder of comparisons.
//!
//! # See also
//! - [`utf8`] — the UTF-8 tables and what they decide.
//! - [`crate::index`] — the only caller.

#[cfg(target_arch = "aarch64")]
mod aarch64;
pub mod utf16;
pub mod utf8;
#[cfg(target_arch = "x86_64")]
mod x86;

pub use utf8::{PRECEDING, Validator, validate_scalar};
pub use utf16::{UNIT_BLOCK, UnitClassifier, UnitMasks, classify_units_scalar};

/// How many bytes one classification covers.
pub const BLOCK: usize = 64;

/// Bits set for a byte the index records: `\r`, `&`, `<`, `>`.
pub const STRUCTURAL_BITS: u8 = 0b0001_1100;
/// Bits set for a byte no document may contain: a C0 control that is not tab,
/// newline or carriage return.
pub const FORBIDDEN_BITS: u8 = 0b0000_0011;

/// Table indexed by a byte's low nibble.
///
/// Bit 0 marks the bytes of `0x00..=0x0F` that are forbidden, bit 1 the same
/// for `0x10..=0x1F`, bit 2 picks `&`, bit 3 picks `<` and `>`, and bit 4
/// picks `\r`. Pairing these with [`LUT_HI`] leaves exactly the interesting
/// bytes non-zero.
pub const LUT_LO: [u8; 16] = [
    0b0000_0011, // 0x_0
    0b0000_0011, // 0x_1
    0b0000_0011, // 0x_2
    0b0000_0011, // 0x_3
    0b0000_0011, // 0x_4
    0b0000_0011, // 0x_5
    0b0000_0111, // 0x_6 — `&` is 0x26
    0b0000_0011, // 0x_7
    0b0000_0011, // 0x_8
    0b0000_0010, // 0x_9 — tab is allowed, 0x19 is not
    0b0000_0010, // 0x_A — newline is allowed, 0x1A is not
    0b0000_0011, // 0x_B
    0b0000_1011, // 0x_C — `<` is 0x3C
    0b0001_0010, // 0x_D — carriage return is structural, 0x1D forbidden
    0b0000_1011, // 0x_E — `>` is 0x3E
    0b0000_0011, // 0x_F
];

/// Table indexed by a byte's high nibble; see [`LUT_LO`].
pub const LUT_HI: [u8; 16] = [
    0b0001_0001, // 0x0_ — controls, and the carriage return among them
    0b0000_0010, // 0x1_ — all forbidden
    0b0000_0100, // 0x2_ — holds `&`
    0b0000_1000, // 0x3_ — holds `<` and `>`
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
];

/// What one block of bytes turned out to hold.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Masks {
    /// Bytes the index records.
    pub structural: u64,
    /// Bytes no document may contain.
    pub forbidden: u64,
    /// Bytes that are not ASCII, and so need the encoding's decoder.
    pub nonascii: u64,
}

/// A kernel that classifies one block.
pub type Classifier = fn(&[u8; BLOCK]) -> Masks;

/// The kernels one instruction set provides.
#[derive(Debug, Clone, Copy)]
pub struct Kernels {
    /// What a block of bytes holds.
    pub classify: Classifier,
    /// Whether a block's UTF-8 is legal, given the bytes before it.
    pub validate: Validator,
    /// What a block of UTF-16 code units holds.
    pub classify_units: UnitClassifier,
}

/// The portable set, which every machine can run.
pub const PORTABLE: Kernels = Kernels {
    classify: classify_scalar,
    validate: validate_scalar,
    classify_units: classify_units_scalar,
};

/// Every kernel pair this machine can actually run, named, portable one last.
///
/// The dispatcher only ever hands out the first of these, but a machine that
/// can run more than one should be shown to agree on all of them, so the
/// agreement test walks this list rather than [`current`] alone.
#[must_use]
pub fn kernels() -> Vec<(&'static str, Kernels)> {
    let mut all = vector_kernels();
    all.push(("scalar", PORTABLE));
    all
}

/// The vector kernels this machine can run, best first.
#[cfg(target_arch = "aarch64")]
fn vector_kernels() -> Vec<(&'static str, Kernels)> {
    vec![(
        "neon",
        Kernels {
            classify: aarch64::classify_neon,
            validate: aarch64::validate_neon,
            classify_units: aarch64::classify_units_neon,
        },
    )]
}

/// The vector kernels this machine can run, best first.
#[cfg(target_arch = "x86_64")]
fn vector_kernels() -> Vec<(&'static str, Kernels)> {
    let mut all: Vec<(&'static str, Kernels)> = Vec::new();
    if is_x86_feature_detected!("avx2") {
        all.push((
            "avx2",
            Kernels {
                classify: x86::classify_avx2,
                validate: x86::validate_avx2,
                classify_units: x86::classify_units_sse,
            },
        ));
    }
    if is_x86_feature_detected!("ssse3") {
        all.push((
            "ssse3",
            Kernels {
                classify: x86::classify_ssse3,
                validate: x86::validate_ssse3,
                classify_units: x86::classify_units_sse,
            },
        ));
    }
    all
}

/// The vector kernels this machine can run: none, on a target with no kernel.
#[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
fn vector_kernels() -> Vec<(&'static str, Kernels)> {
    Vec::new()
}

/// The best kernels this machine can run.
///
/// Advanced SIMD is part of the base aarch64 ABI, so there is nothing to
/// detect there; elsewhere the choice is made at run time, and a target with
/// no kernel of its own gets the portable pair.
#[must_use]
pub fn current() -> Kernels {
    vector_kernels()
        .first()
        .map_or(PORTABLE, |(_, kernels)| *kernels)
}

/// The portable classifier, and the definition every kernel answers to.
#[must_use]
pub fn classify_scalar(block: &[u8; BLOCK]) -> Masks {
    let mut masks = Masks::default();
    for (at, &byte) in block.iter().enumerate() {
        let bit = 1u64 << at;
        if byte >= 0x80 {
            masks.nonascii |= bit;
            continue;
        }
        let class = LUT_LO[(byte & 0x0F) as usize] & LUT_HI[(byte >> 4) as usize];
        if class & STRUCTURAL_BITS != 0 {
            masks.structural |= bit;
        }
        if class & FORBIDDEN_BITS != 0 {
            masks.forbidden |= bit;
        }
    }
    masks
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tables are only worth having if they agree with the grammar they
    /// stand for, byte for byte.
    #[test]
    fn the_tables_classify_exactly_what_the_grammar_says() {
        for byte in 0..=0xFFu8 {
            let mut block = [b' '; BLOCK];
            block[0] = byte;
            let masks = classify_scalar(&block);
            let structural = matches!(byte, b'<' | b'>' | b'&' | b'\r');
            let forbidden = byte < 0x20 && !matches!(byte, 0x09 | 0x0A | 0x0D);
            let nonascii = byte >= 0x80;
            assert_eq!(masks.structural & 1 != 0, structural, "{byte:#04x}");
            assert_eq!(masks.forbidden & 1 != 0, forbidden, "{byte:#04x}");
            assert_eq!(masks.nonascii & 1 != 0, nonascii, "{byte:#04x}");
        }
    }

    #[test]
    fn a_byte_is_never_both_structural_and_forbidden() {
        for byte in 0..=0xFFu8 {
            let mut block = [b' '; BLOCK];
            block[0] = byte;
            let masks = classify_scalar(&block);
            assert_eq!(masks.structural & masks.forbidden, 0, "{byte:#04x}");
        }
    }

    #[test]
    fn every_bit_answers_for_its_own_byte() {
        let mut block = [b'x'; BLOCK];
        block[0] = b'<';
        block[31] = b'&';
        block[63] = 0x01;
        block[7] = 0xC3;
        let masks = (current().classify)(&block);
        assert_eq!(masks.structural, (1 << 0) | (1 << 31));
        assert_eq!(masks.forbidden, 1 << 63);
        assert_eq!(masks.nonascii, 1 << 7);
        assert_eq!(masks, classify_scalar(&block));
    }
}
