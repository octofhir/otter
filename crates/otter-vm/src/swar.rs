//! SIMD-within-a-register (SWAR) byte / code-unit scanners.
//!
//! Reusable helpers that scan a byte slice 8 bytes at a time
//! using portable u64 bit-twiddling. No `unsafe`, no external
//! SIMD intrinsics, no dependencies — auto-vectorisable on any
//! platform Rust supports.
//!
//! # Contents
//!
//! - [`find_byte`] — first occurrence of a single byte.
//! - [`find_u16`] — first occurrence of a single 16-bit code
//!   unit (4 units / iteration).
//!
//! # Invariants
//!
//! - All scanners are endian-portable: chunks are reassembled via
//!   `from_le_bytes`, so the byte index of the first match is
//!   `mask.trailing_zeros() / 8` regardless of host byte order.
//! - Worst-case behaviour matches the naïve scalar loop; best
//!   case (long clean span) is ~5–8× faster.
//!
//! # See also
//!
//! - <https://graphics.stanford.edu/~seander/bithacks.html>
//!   ("Determine if a word has a zero byte" / "byte less than n").

const ONES_U8: u64 = 0x0101_0101_0101_0101;
const HIGH_U8: u64 = 0x8080_8080_8080_8080;
const ONES_U16: u64 = 0x0001_0001_0001_0001;
const HIGH_U16: u64 = 0x8000_8000_8000_8000;

/// Returns a u64 whose byte `i` has its high bit set iff
/// `x`'s byte `i` equals `c`.
#[inline(always)]
fn byte_eq_mask(x: u64, c: u8) -> u64 {
    let xor = x ^ (c as u64).wrapping_mul(ONES_U8);
    xor.wrapping_sub(ONES_U8) & !xor & HIGH_U8
}

/// Returns a u64 whose 16-bit lane `i` has its high bit set iff
/// the lane equals `c`.
#[inline(always)]
fn u16_eq_mask(x: u64, c: u16) -> u64 {
    let xor = x ^ (c as u64).wrapping_mul(ONES_U16);
    xor.wrapping_sub(ONES_U16) & !xor & HIGH_U16
}

/// Locate the first index `i` ≥ `from` such that `bytes[i] == c`.
/// Returns `None` if no such index exists.
///
/// Scans 8 bytes per iteration; falls back to scalar for the
/// trailing tail (< 8 bytes).
#[inline]
pub fn find_byte(bytes: &[u8], c: u8, from: usize) -> Option<usize> {
    let mut i = from;
    let len = bytes.len();
    while i + 8 <= len {
        let chunk_bytes: [u8; 8] = bytes[i..i + 8]
            .try_into()
            .expect("8-byte window is exactly 8 bytes");
        let chunk = u64::from_le_bytes(chunk_bytes);
        let mask = byte_eq_mask(chunk, c);
        if mask != 0 {
            return Some(i + (mask.trailing_zeros() as usize) / 8);
        }
        i += 8;
    }
    while i < len {
        if bytes[i] == c {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Locate the **last** index `i` such that `bytes[i] == c`.
/// Returns `None` if no such index exists.
///
/// Scans 8 bytes per iteration in reverse; falls back to scalar
/// for the leading head (< 8 bytes).
///
/// # SWAR caveat — false positives in [`byte_eq_mask`]
///
/// The classical `(v - ones) & !v & high` zero-byte formula sets
/// the high bit at the **first** zero lane *and* may also set it
/// for a higher lane whose unxored byte differs from `c` by 1
/// (the borrow chain crossing a real zero contaminates the next
/// byte if its xor is `0x01`). [`find_byte`] sidesteps this
/// because [`u64::trailing_zeros`] returns the lowest set bit —
/// always a true match — but `rfind` looks at the **highest**
/// set bit and would otherwise return a false positive. We
/// therefore enumerate the set lanes top-down and verify each
/// against the original chunk before reporting.
#[inline]
pub fn rfind_byte(bytes: &[u8], c: u8) -> Option<usize> {
    let mut end = bytes.len();
    while end >= 8 {
        let chunk_bytes: [u8; 8] = bytes[end - 8..end]
            .try_into()
            .expect("8-byte window is exactly 8 bytes");
        let chunk = u64::from_le_bytes(chunk_bytes);
        let mut mask = byte_eq_mask(chunk, c);
        while mask != 0 {
            let lane = 7 - (mask.leading_zeros() as usize) / 8;
            let byte = (chunk >> (lane * 8)) as u8;
            if byte == c {
                return Some(end - 8 + lane);
            }
            // Clear this set lane and continue scanning the
            // chunk top-down for the next candidate.
            mask &= !(0x80u64 << (lane * 8));
        }
        end -= 8;
    }
    while end > 0 {
        end -= 1;
        if bytes[end] == c {
            return Some(end);
        }
    }
    None
}

/// Locate the first index `i` ≥ `from` such that `units[i] == c`.
/// Returns `None` if no such index exists.
///
/// Scans 4 code units per iteration; falls back to scalar for
/// the trailing tail (< 4 units).
#[inline]
pub fn find_u16(units: &[u16], c: u16, from: usize) -> Option<usize> {
    let mut i = from;
    let len = units.len();
    while i + 4 <= len {
        let chunk_bytes: [u8; 8] = bytemuck_u16_to_u64_le(&units[i..i + 4]);
        let chunk = u64::from_le_bytes(chunk_bytes);
        let mask = u16_eq_mask(chunk, c);
        if mask != 0 {
            return Some(i + (mask.trailing_zeros() as usize) / 16);
        }
        i += 4;
    }
    while i < len {
        if units[i] == c {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Locate the **last** index `i` such that `units[i] == c`.
/// Returns `None` if no such index exists.
///
/// Scans 4 code units per iteration in reverse; falls back to
/// scalar for the leading head (< 4 units). Same iterate-and-
/// verify protocol as [`rfind_byte`] — see its docstring for
/// why the borrow chain across [`u16_eq_mask`] can stain a
/// higher lane.
#[inline]
pub fn rfind_u16(units: &[u16], c: u16) -> Option<usize> {
    let mut end = units.len();
    while end >= 4 {
        let chunk_bytes: [u8; 8] = bytemuck_u16_to_u64_le(&units[end - 4..end]);
        let chunk = u64::from_le_bytes(chunk_bytes);
        let mut mask = u16_eq_mask(chunk, c);
        while mask != 0 {
            let lane = 3 - (mask.leading_zeros() as usize) / 16;
            let unit = (chunk >> (lane * 16)) as u16;
            if unit == c {
                return Some(end - 4 + lane);
            }
            mask &= !(0x8000u64 << (lane * 16));
        }
        end -= 4;
    }
    while end > 0 {
        end -= 1;
        if units[end] == c {
            return Some(end);
        }
    }
    None
}

/// Pack four `u16` values into the byte representation of one
/// little-endian u64 without going through `bytemuck` or `unsafe`
/// pointer casts.
#[inline(always)]
fn bytemuck_u16_to_u64_le(units: &[u16]) -> [u8; 8] {
    debug_assert_eq!(units.len(), 4);
    let mut out = [0u8; 8];
    let [a, b, c, d] = [units[0], units[1], units[2], units[3]];
    out[0..2].copy_from_slice(&a.to_le_bytes());
    out[2..4].copy_from_slice(&b.to_le_bytes());
    out[4..6].copy_from_slice(&c.to_le_bytes());
    out[6..8].copy_from_slice(&d.to_le_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_byte_empty() {
        assert_eq!(find_byte(b"", b'a', 0), None);
    }

    #[test]
    fn find_byte_present_at_chunk_boundary() {
        let bytes = b"01234567a";
        assert_eq!(find_byte(bytes, b'a', 0), Some(8));
    }

    #[test]
    fn find_byte_inside_chunk() {
        let bytes = b"abcXdefghijklmno";
        assert_eq!(find_byte(bytes, b'X', 0), Some(3));
    }

    #[test]
    fn find_byte_in_tail() {
        let bytes = b"0123456789abc!";
        assert_eq!(find_byte(bytes, b'!', 0), Some(13));
    }

    #[test]
    fn find_byte_respects_from() {
        let bytes = b"abcabcabc";
        assert_eq!(find_byte(bytes, b'a', 0), Some(0));
        assert_eq!(find_byte(bytes, b'a', 1), Some(3));
        assert_eq!(find_byte(bytes, b'a', 4), Some(6));
        assert_eq!(find_byte(bytes, b'a', 7), None);
    }

    #[test]
    fn find_byte_absent() {
        let bytes = b"the quick brown fox";
        assert_eq!(find_byte(bytes, b'z', 0), None);
    }

    #[test]
    fn find_byte_high_bit_byte_does_not_collide() {
        // 0xFF must NOT register as a hit when the needle is 0x80.
        let bytes = &[0xFFu8, 0xFE, 0xFD, 0xFC, 0xFB, 0xFA, 0xF9, 0xF8];
        assert_eq!(find_byte(bytes, 0x80, 0), None);
        // And the needle 0xFF should still be found.
        assert_eq!(find_byte(bytes, 0xFF, 0), Some(0));
    }

    #[test]
    fn find_byte_agrees_with_scalar_on_random() {
        let mut state: u32 = 0xC0FFEE_u32;
        for _ in 0..200 {
            state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            let len = (state as usize) % 257;
            let mut buf = Vec::with_capacity(len);
            for _ in 0..len {
                state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                buf.push((state >> 16) as u8);
            }
            let needle = (state >> 24) as u8;
            for from in 0..=buf.len() {
                let swar = find_byte(&buf, needle, from);
                let scalar = buf[from..].iter().position(|&b| b == needle).map(|p| p + from);
                assert_eq!(swar, scalar, "needle={needle:#x} from={from}");
            }
        }
    }

    #[test]
    fn find_u16_present() {
        let units: Vec<u16> = (0..16).collect();
        assert_eq!(find_u16(&units, 5, 0), Some(5));
        assert_eq!(find_u16(&units, 12, 0), Some(12));
    }

    #[test]
    fn find_u16_absent() {
        let units: Vec<u16> = (0..16).collect();
        assert_eq!(find_u16(&units, 99, 0), None);
    }

    #[test]
    fn find_u16_respects_from() {
        let units = [1u16, 2, 3, 1, 2, 3, 1];
        assert_eq!(find_u16(&units, 1, 0), Some(0));
        assert_eq!(find_u16(&units, 1, 1), Some(3));
        assert_eq!(find_u16(&units, 1, 4), Some(6));
        assert_eq!(find_u16(&units, 1, 7), None);
    }

    #[test]
    fn find_u16_supplementary_planes() {
        // Surrogate-pair lead and trail must round-trip cleanly.
        let units = [0x0041u16, 0xD83D, 0xDE00, 0x0041];
        assert_eq!(find_u16(&units, 0xD83D, 0), Some(1));
        assert_eq!(find_u16(&units, 0xDE00, 0), Some(2));
    }

    #[test]
    fn rfind_byte_present_at_chunk_end() {
        // `Z` lands inside the SWAR-scanned window (last 8 bytes).
        let bytes = b"abcdefghZjklmnop";
        assert_eq!(rfind_byte(bytes, b'Z'), Some(8));
    }

    #[test]
    fn rfind_byte_present_in_leading_tail() {
        // Length 5 → entire slice handled by the scalar tail of
        // the reverse scan.
        let bytes = b"hZllo";
        assert_eq!(rfind_byte(bytes, b'Z'), Some(1));
    }

    #[test]
    fn rfind_byte_picks_last_of_multiple_matches() {
        let bytes = b"aXbXcXdXeXfX";
        assert_eq!(rfind_byte(bytes, b'X'), Some(11));
    }

    #[test]
    fn rfind_byte_absent() {
        let bytes = b"abcdefghijklmnop";
        assert_eq!(rfind_byte(bytes, b'Z'), None);
    }

    #[test]
    fn rfind_byte_empty() {
        assert_eq!(rfind_byte(b"", b'a'), None);
    }

    #[test]
    fn rfind_byte_high_bit_byte_does_not_collide() {
        let bytes = &[0xFFu8, 0xFE, 0xFD, 0xFC, 0xFB, 0xFA, 0xF9, 0xF8];
        assert_eq!(rfind_byte(bytes, 0x80), None);
        assert_eq!(rfind_byte(bytes, 0xFF), Some(0));
        assert_eq!(rfind_byte(bytes, 0xF8), Some(7));
    }

    #[test]
    fn rfind_byte_agrees_with_scalar_on_random() {
        let mut state: u32 = 0xDEC0DE_u32;
        for _ in 0..200 {
            state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            let len = (state as usize) % 257;
            let mut buf = Vec::with_capacity(len);
            for _ in 0..len {
                state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                buf.push((state >> 16) as u8);
            }
            let needle = (state >> 24) as u8;
            let swar = rfind_byte(&buf, needle);
            let scalar = buf.iter().rposition(|&b| b == needle);
            assert_eq!(swar, scalar, "needle={needle:#x}");
        }
    }

    #[test]
    fn rfind_u16_picks_last_match() {
        let units: Vec<u16> = vec![1, 2, 3, 1, 2, 3, 1, 4];
        assert_eq!(rfind_u16(&units, 1), Some(6));
        assert_eq!(rfind_u16(&units, 2), Some(4));
        assert_eq!(rfind_u16(&units, 4), Some(7));
    }

    #[test]
    fn rfind_u16_absent() {
        let units = [1u16, 2, 3, 4];
        assert_eq!(rfind_u16(&units, 99), None);
    }

    #[test]
    fn rfind_u16_agrees_with_scalar_on_random() {
        let mut state: u32 = 0xC0DE_u32;
        for _ in 0..200 {
            state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            let len = (state as usize) % 65;
            let mut buf = Vec::with_capacity(len);
            for _ in 0..len {
                state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                buf.push((state >> 8) as u16);
            }
            let needle = (state >> 16) as u16;
            let swar = rfind_u16(&buf, needle);
            let scalar = buf.iter().rposition(|&u| u == needle);
            assert_eq!(swar, scalar, "needle={needle:#x}");
        }
    }

    #[test]
    fn find_u16_agrees_with_scalar_on_random() {
        let mut state: u32 = 0xBADF00D_u32;
        for _ in 0..200 {
            state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            let len = (state as usize) % 65;
            let mut buf = Vec::with_capacity(len);
            for _ in 0..len {
                state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                buf.push((state >> 8) as u16);
            }
            let needle = (state >> 16) as u16;
            for from in 0..=buf.len() {
                let swar = find_u16(&buf, needle, from);
                let scalar = buf[from..].iter().position(|&u| u == needle).map(|p| p + from);
                assert_eq!(swar, scalar, "needle={needle:#x} from={from}");
            }
        }
    }
}
