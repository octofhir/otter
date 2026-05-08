//! SWAR (SIMD-within-a-register) scanner for JSON string literals.
//!
//! Both `JSON.stringify` (`write_string_literal`) and `JSON.parse`
//! (`read_string`) walk a UTF-8 byte slice looking for the first
//! "escape" byte — namely `b'"'`, `b'\\'`, or any control character
//! (`< 0x20`). Outside that set every byte is part of a clean span
//! that can be bulk-copied (stringify) or skipped (parse).
//!
//! This module provides a portable, `unsafe`-free 8-byte-at-a-time
//! scanner using classical bit-twiddling tricks (Sean Anderson,
//! "Bit Twiddling Hacks", under the [public-domain notice]). The
//! body of every JSON string spent in the inner scan is the hottest
//! loop in both serialise and parse paths; replacing the scalar
//! `i += 1` step with one u64 load + a couple of arithmetic ops
//! divides the per-byte cost by ~8 on long ASCII payloads.
//!
//! # Contents
//! - [`find_first_escape`] — public scan helper.
//! - [`find_first_escape_scalar`] — the byte-at-a-time reference
//!   (kept for benchmarking and as a correctness oracle in tests).
//!
//! # Invariants
//! - The "escape" set is exactly `{ b'"', b'\\', b: b < 0x20 }`.
//!   UTF-8 continuation bytes (`0x80..=0xBF`) and lead bytes
//!   (`0xC0..=0xFF`) are *clean* — neither parser nor serialiser
//!   needs to look at them, they are forwarded byte-for-byte.
//! - Endianness independent: we read each chunk via
//!   [`u64::from_le_bytes`] so the index of the first set byte
//!   inside a chunk is `mask.trailing_zeros() / 8` regardless of
//!   the host's native byte order.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-json.stringify> §25.5.2
//! - <https://tc39.es/ecma262/#sec-json.parse> §25.5.1
//! - <https://graphics.stanford.edu/~seander/bithacks.html>
//!
//! [public-domain notice]: https://graphics.stanford.edu/~seander/bithacks.html

const ONES: u64 = 0x0101_0101_0101_0101;
const HIGH: u64 = 0x8080_8080_8080_8080;

/// Returns a u64 whose byte `i` has its high bit set iff
/// `x`'s byte `i` equals `c`.
#[inline(always)]
fn byte_eq_mask(x: u64, c: u8) -> u64 {
    // `xor[i] == 0` ⇔ `x[i] == c`. Standard "has-zero-byte" trick.
    let xor = x ^ (c as u64).wrapping_mul(ONES);
    xor.wrapping_sub(ONES) & !xor & HIGH
}

/// Returns a u64 whose byte `i` has its high bit set iff
/// `x`'s byte `i` is strictly less than `0x20`.
///
/// The classical `(x - n*ones) & !x & high_bits` formula is
/// proven correct for any threshold `n ≤ 0x80` (Bit Twiddling
/// Hacks, "Determine if a word has a byte less than n"). `0x20`
/// satisfies that bound, so no false positives are possible.
#[inline(always)]
fn byte_lt_20_mask(x: u64) -> u64 {
    const TWENTIES: u64 = 0x2020_2020_2020_2020;
    x.wrapping_sub(TWENTIES) & !x & HIGH
}

/// Locate the first byte at index ≥ `start` that is `b'"'`,
/// `b'\\'`, or `< 0x20`. Returns `bytes.len()` if no such byte
/// exists.
///
/// Scans 8 bytes per iteration via SWAR; falls back to scalar
/// for the tail. Worst-case behaviour is identical to the naive
/// loop; best case (long clean ASCII span) is ~5–8× faster.
#[inline]
pub(crate) fn find_first_escape(bytes: &[u8], start: usize) -> usize {
    let mut i = start;
    let len = bytes.len();
    while i + 8 <= len {
        // SAFETY proxy: slice-to-array conversion via `try_into`
        // is checked at compile time once the slice length is
        // observed (the unwrap is unreachable for `bytes[i..i+8]`).
        let chunk_bytes: [u8; 8] = bytes[i..i + 8]
            .try_into()
            .expect("8-byte window is exactly 8 bytes");
        let chunk = u64::from_le_bytes(chunk_bytes);
        let mask = byte_eq_mask(chunk, b'"')
            | byte_eq_mask(chunk, b'\\')
            | byte_lt_20_mask(chunk);
        if mask != 0 {
            // Lowest-numbered byte in the LE chunk corresponds to
            // `bytes[i]`; trailing_zeros / 8 gives its offset.
            return i + (mask.trailing_zeros() as usize) / 8;
        }
        i += 8;
    }
    while i < len {
        let b = bytes[i];
        if b == b'"' || b == b'\\' || b < 0x20 {
            return i;
        }
        i += 1;
    }
    len
}

/// Byte-at-a-time reference scanner, retained for benchmarks and
/// the property test that asserts agreement with
/// [`find_first_escape`].
#[inline]
#[doc(hidden)]
pub fn find_first_escape_scalar(bytes: &[u8], start: usize) -> usize {
    let mut i = start;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'"' || b == b'\\' || b < 0x20 {
            return i;
        }
        i += 1;
    }
    bytes.len()
}

/// Public re-export of [`find_first_escape`] for use in
/// benchmarks living outside the crate. Internal callers in the
/// json module should keep using the crate-private alias.
#[doc(hidden)]
pub fn find_first_escape_pub(bytes: &[u8], start: usize) -> usize {
    find_first_escape(bytes, start)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_returns_len() {
        assert_eq!(find_first_escape(b"", 0), 0);
    }

    #[test]
    fn clean_span_returns_len() {
        let bytes = b"hello world this is a perfectly clean ascii sentence!";
        assert_eq!(find_first_escape(bytes, 0), bytes.len());
    }

    #[test]
    fn detects_quote() {
        let bytes = b"hello\"world";
        assert_eq!(find_first_escape(bytes, 0), 5);
    }

    #[test]
    fn detects_backslash() {
        let bytes = b"hello\\world";
        assert_eq!(find_first_escape(bytes, 0), 5);
    }

    #[test]
    fn detects_control_char_at_chunk_boundary() {
        // Zero byte right at the 8-byte boundary.
        let bytes = b"01234567\x01abcdef";
        assert_eq!(find_first_escape(bytes, 0), 8);
    }

    #[test]
    fn detects_control_char_inside_chunk() {
        let bytes = b"abc\x01defghijklmnop";
        assert_eq!(find_first_escape(bytes, 0), 3);
    }

    #[test]
    fn detects_tab_newline_cr() {
        assert_eq!(find_first_escape(b"abc\tdef", 0), 3);
        assert_eq!(find_first_escape(b"abc\ndef", 0), 3);
        assert_eq!(find_first_escape(b"abc\rdef", 0), 3);
    }

    #[test]
    fn ignores_non_ascii_bytes() {
        // U+00A9 © encoded as 0xC2 0xA9.
        let bytes = b"copyright \xC2\xA9 2026 Otter";
        assert_eq!(find_first_escape(bytes, 0), bytes.len());
    }

    #[test]
    fn ignores_high_bit_bytes_at_chunk_boundary() {
        // Several 0xFF bytes spanning the SWAR/tail boundary.
        let bytes = b"\xFF\xFF\xFF\xFF\xFF\xFF\xFF\xFF\xFF\xFF\xFF\xFF\xFF";
        assert_eq!(find_first_escape(bytes, 0), bytes.len());
    }

    #[test]
    fn respects_start_offset() {
        let bytes = b"\"hello\"";
        assert_eq!(find_first_escape(bytes, 1), 6);
    }

    #[test]
    fn agrees_with_scalar_on_random_inputs() {
        let mut state: u32 = 0xDEAD_BEEF;
        for _ in 0..200 {
            state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            let len = (state as usize) % 257;
            let mut buf = Vec::with_capacity(len);
            for _ in 0..len {
                state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                // Bias toward clean ASCII so SWAR path is exercised.
                let b = match state & 0x7 {
                    0 => b'"',
                    1 => b'\\',
                    2 => 0x0A,
                    _ => 0x20 + ((state >> 8) as u8 & 0x5F),
                };
                buf.push(b);
            }
            for start in 0..=buf.len() {
                let a = find_first_escape(&buf, start);
                let b = find_first_escape_scalar(&buf, start);
                assert_eq!(a, b, "mismatch at start={start}, buf={buf:?}");
            }
        }
    }
}
