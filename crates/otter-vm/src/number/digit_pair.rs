//! 2-digit pair lookup table for fast `u64 → ASCII digits`.
//!
//! Standard "two digits at a time" trick (Alexandrescu, "Three
//! Optimization Tips for C++"). Each entry of [`DIGIT_PAIRS`] is the
//! 2-byte ASCII representation of the integer formed by its index:
//! index `42` → `b"42"`. Allows the integer-format loop to consume
//! two decimal digits per iteration without going through generic
//! formatting.
//!
//! # Contents
//! - [`DIGIT_PAIRS`] — the 200-byte lookup table.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-numeric-types-number-tostring>
//!   (consumer of fast integer formatting).

/// Compile-time-built ASCII pair table. `DIGIT_PAIRS[2*k..2*k+2]` is
/// the two ASCII bytes that spell `k` (`00`, `01`, ..., `99`).
pub static DIGIT_PAIRS: [u8; 200] = {
    let mut out = [0u8; 200];
    let mut i = 0usize;
    while i < 100 {
        out[i * 2] = b'0' + (i / 10) as u8;
        out[i * 2 + 1] = b'0' + (i % 10) as u8;
        i += 1;
    }
    out
};

#[cfg(test)]
mod tests {
    use super::DIGIT_PAIRS;

    #[test]
    fn pair_42_is_42() {
        assert_eq!(&DIGIT_PAIRS[42 * 2..42 * 2 + 2], b"42");
    }

    #[test]
    fn pair_00_and_99() {
        assert_eq!(&DIGIT_PAIRS[0..2], b"00");
        assert_eq!(&DIGIT_PAIRS[99 * 2..99 * 2 + 2], b"99");
    }

    #[test]
    fn every_pair_is_ascii_digit() {
        for byte in DIGIT_PAIRS.iter() {
            assert!(byte.is_ascii_digit(), "{byte:#x}");
        }
    }
}
