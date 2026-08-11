//! Vector classification of one block of UTF-16 code units.
//!
//! A UTF-16 document was indexed one unit at a time, with a decode and a
//! range ladder per unit, while a byte document had a kernel. The work is the
//! same shape in both: a handful of comparisons whose answers are bit sets.
//! The only part that is not comparison is surrogate pairing, and that falls
//! out of the bit sets themselves — every leading surrogate must be followed
//! by a trailing one, which is one shift and one comparison of two masks for
//! a whole block.
//!
//! # Contents
//! - [`UnitMasks`] — the four bit sets one block of units yields.
//! - [`UnitClassifier`] — the kernel picked for this machine.
//! - [`classify_units_scalar`] — the portable one, and the definition the
//!   vector kernels answer to.
//!
//! # Invariants
//! - A kernel answers for exactly [`UNIT_BLOCK`] units; callers pad a short
//!   tail with a unit that classifies as nothing (a space).
//! - Bit `n` of every mask describes unit `n` of the block.
//! - `forbidden` holds both halves of what the `Char` production bars in the
//!   BMP: the C0 controls other than tab, newline and carriage return, and
//!   U+FFFE / U+FFFF. Everything else a unit can be is either a surrogate,
//!   reported in its own mask, or legal.
//! - A supplementary code point is legal whenever its pair is well formed, so
//!   a matched pair needs no further check.
//! - Every kernel returns what [`classify_units_scalar`] returns. That is a
//!   test, not a hope: `tests/index_agreement.rs` runs them over random
//!   input.
//!
//! # See also
//! - [`crate::index`] — the only caller.

/// How many code units one classification covers.
pub const UNIT_BLOCK: usize = 64;

/// The unit a short tail is padded with: legal, unremarkable, and not a
/// surrogate of either kind.
pub const PAD: u16 = b' ' as u16;

/// What one block of code units turned out to hold.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UnitMasks {
    /// Units the index records: `<`, `>`, `&`, `\r`.
    pub structural: u64,
    /// Units no document may contain, whatever precedes or follows them.
    pub forbidden: u64,
    /// Leading surrogates, each of which owes a trailing one after it.
    pub high: u64,
    /// Trailing surrogates, each of which owes a leading one before it.
    pub low: u64,
}

/// A kernel that classifies one block of units.
pub type UnitClassifier = fn(&[u16; UNIT_BLOCK]) -> UnitMasks;

/// The highest unit that is a C0 control.
pub const LAST_CONTROL: u16 = 0x1F;
/// The lowest unit the `Char` production bars at the top of the BMP.
pub const FIRST_NONCHARACTER: u16 = 0xFFFE;
/// The bits that tell a surrogate from anything else.
pub const SURROGATE_MASK: u16 = 0xFC00;
/// What those bits are for a leading surrogate.
pub const HIGH_SURROGATE: u16 = 0xD800;
/// What they are for a trailing one.
pub const LOW_SURROGATE: u16 = 0xDC00;

/// Whether a unit is one no document may contain on its own.
#[inline]
#[must_use]
pub const fn unit_is_forbidden(unit: u16) -> bool {
    (unit <= LAST_CONTROL && !matches!(unit, 0x09 | 0x0A | 0x0D)) || unit >= FIRST_NONCHARACTER
}

/// Whether a unit is one the index records.
#[inline]
#[must_use]
pub const fn unit_is_structural(unit: u16) -> bool {
    matches!(unit, 0x3C | 0x3E | 0x26 | 0x0D)
}

/// The portable classifier, and the definition every kernel answers to.
#[must_use]
pub fn classify_units_scalar(block: &[u16; UNIT_BLOCK]) -> UnitMasks {
    let mut masks = UnitMasks::default();
    for (at, &unit) in block.iter().enumerate() {
        let bit = 1u64 << at;
        if unit_is_structural(unit) {
            masks.structural |= bit;
        }
        if unit_is_forbidden(unit) {
            masks.forbidden |= bit;
        }
        match unit & SURROGATE_MASK {
            HIGH_SURROGATE => masks.high |= bit,
            LOW_SURROGATE => masks.low |= bit,
            _ => {}
        }
    }
    masks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_masks_say_exactly_what_the_grammar_says() {
        for unit in 0..=0xFFFFu32 {
            let unit = unit as u16;
            let mut block = [PAD; UNIT_BLOCK];
            block[0] = unit;
            let masks = classify_units_scalar(&block);
            let structural = matches!(unit, 0x3C | 0x3E | 0x26 | 0x0D);
            let forbidden = (unit < 0x20 && !matches!(unit, 0x09 | 0x0A | 0x0D))
                || matches!(unit, 0xFFFE | 0xFFFF);
            assert_eq!(masks.structural & 1 != 0, structural, "{unit:#06x}");
            assert_eq!(masks.forbidden & 1 != 0, forbidden, "{unit:#06x}");
            assert_eq!(
                masks.high & 1 != 0,
                (0xD800..0xDC00).contains(&unit),
                "{unit:#06x}"
            );
            assert_eq!(
                masks.low & 1 != 0,
                (0xDC00..0xE000).contains(&unit),
                "{unit:#06x}"
            );
        }
    }

    #[test]
    fn a_unit_is_never_both_structural_and_forbidden() {
        for unit in 0..=0xFFFFu32 {
            let mut block = [PAD; UNIT_BLOCK];
            block[0] = unit as u16;
            let masks = classify_units_scalar(&block);
            assert_eq!(masks.structural & masks.forbidden, 0, "{unit:#06x}");
            assert_eq!(masks.high & masks.low, 0, "{unit:#06x}");
        }
    }

    #[test]
    fn every_bit_answers_for_its_own_unit() {
        let mut block = [u16::from(b'x'); UNIT_BLOCK];
        block[0] = u16::from(b'<');
        block[31] = 0x0001;
        block[62] = 0xD83D;
        block[63] = 0xDE00;
        let masks = classify_units_scalar(&block);
        assert_eq!(masks.structural, 1 << 0);
        assert_eq!(masks.forbidden, 1 << 31);
        assert_eq!(masks.high, 1 << 62);
        assert_eq!(masks.low, 1 << 63);
    }
}
