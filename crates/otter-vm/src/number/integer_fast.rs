//! Integer-valued `f64 → ASCII digits` fast path.
//!
//! Bypasses the shortest-decimal core for the common case where a
//! JavaScript `Number` happens to be an exact integer. Drives the
//! formatter from the 2-digit pair table in [`super::digit_pair`],
//! consuming two decimal digits per loop iteration with no
//! `core::fmt` / heap allocation traffic.
//!
//! Used by the ECMA wrapper (`super::ecma`) as the very first branch
//! of `Number::ToString` (`§6.1.6.1.13`). When the input is finite,
//! integral, and round-trip-representable as `i64`, we never reach
//! the Schubfach core.
//!
//! # ECMA-262
//! - <https://tc39.es/ecma262/#sec-numeric-types-number-tostring>
//!
//! # Invariants
//! - Output is ASCII (every byte `< 0x80`), so the result also forms
//!   a valid Latin-1 byte sequence.
//! - The `f64` fast path activates only when `x.is_finite()`,
//!   `x.fract() == 0.0`, and `|x| < 2^53` (the f64 → integer
//!   round-trip envelope).
//! - `-0.0` → `"0"` (sign of zero is dropped per
//!   <https://tc39.es/ecma262/#sec-numeric-types-number-tostring>).
//! - All writes go into caller-supplied buffers; this module never
//!   allocates.

use super::digit_pair::DIGIT_PAIRS;

/// Largest exact integer round-trip-representable as `f64`.
const F64_INT_LIMIT: f64 = (1u64 << 53) as f64;

/// Worst-case ASCII length for `i32::MIN` (`-2147483648`) is 11.
/// One byte of headroom for callers that prefer round buffers.
pub const I32_BUF_LEN: usize = 12;

/// Worst-case ASCII length for `i64::MIN`
/// (`-9223372036854775808`) is 20. 24 chosen to match the f64
/// integer envelope with extra headroom.
pub const I64_BUF_LEN: usize = 24;

/// Format `n` into the front of `out`. Returns the number of bytes
/// written; the caller reads `&out[..returned]`.
#[inline]
pub fn format_i32(n: i32, out: &mut [u8; I32_BUF_LEN]) -> usize {
    if n < 0 {
        out[0] = b'-';
        let len = format_u64_into(u64::from(n.unsigned_abs()), &mut out[1..]);
        1 + len
    } else {
        format_u64_into(n as u64, out)
    }
}

/// Format `n` into the front of `out`. Returns bytes written.
#[inline]
pub fn format_i64(n: i64, out: &mut [u8; I64_BUF_LEN]) -> usize {
    if n < 0 {
        out[0] = b'-';
        let len = format_u64_into(n.unsigned_abs(), &mut out[1..]);
        1 + len
    } else {
        format_u64_into(n as u64, out)
    }
}

/// Probe + format in one call. Returns `Some(len)` iff `x` is finite,
/// integral, and within the `f64 → i64` round-trip envelope.
/// Returns `None` for non-integer, non-finite, or out-of-range
/// inputs — those caller routes to the Schubfach core.
///
/// Treats `-0.0` as integer `0`, matching the ECMA-262 rule that
/// `ToString(-0)` is `"0"`.
#[inline]
pub fn format_f64_if_integer(x: f64, out: &mut [u8; I64_BUF_LEN]) -> Option<usize> {
    if !x.is_finite() {
        return None;
    }
    let abs = x.abs();
    if abs >= F64_INT_LIMIT {
        return None;
    }
    // `f64::fract` returns `+0.0` for both `+0.0` and `-0.0`, and `0`
    // for any exact integer. Non-integer `x` returns a non-zero
    // fraction; that path falls back to Schubfach.
    if x.fract() != 0.0 {
        return None;
    }
    // Safe cast: |x| < 2^53 < i64::MAX, and integral.
    Some(format_i64(x as i64, out))
}

/// Core `u64 → ASCII` digit emitter. Writes into the front of `out`
/// (via a small backing scratch since we work right-to-left), and
/// returns the byte count.
#[inline]
fn format_u64_into(mut n: u64, out: &mut [u8]) -> usize {
    // Single-digit fast path — no scratch buffer touch.
    if n < 10 {
        out[0] = b'0' + n as u8;
        return 1;
    }

    // Right-to-left into a 20-byte scratch (max u64 = 20 digits),
    // then copy the populated suffix to `out`.
    let mut scratch = [0u8; 20];
    let mut pos = scratch.len();

    while n >= 100 {
        let pair = (n % 100) as usize * 2;
        n /= 100;
        pos -= 2;
        scratch[pos] = DIGIT_PAIRS[pair];
        scratch[pos + 1] = DIGIT_PAIRS[pair + 1];
    }

    if n >= 10 {
        let pair = (n as usize) * 2;
        pos -= 2;
        scratch[pos] = DIGIT_PAIRS[pair];
        scratch[pos + 1] = DIGIT_PAIRS[pair + 1];
    } else {
        pos -= 1;
        scratch[pos] = b'0' + n as u8;
    }

    let len = scratch.len() - pos;
    out[..len].copy_from_slice(&scratch[pos..]);
    len
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fmt_i32(n: i32) -> String {
        let mut buf = [0u8; I32_BUF_LEN];
        let len = format_i32(n, &mut buf);
        String::from_utf8(buf[..len].to_vec()).unwrap()
    }

    fn fmt_i64(n: i64) -> String {
        let mut buf = [0u8; I64_BUF_LEN];
        let len = format_i64(n, &mut buf);
        String::from_utf8(buf[..len].to_vec()).unwrap()
    }

    fn fmt_f64(x: f64) -> Option<String> {
        let mut buf = [0u8; I64_BUF_LEN];
        format_f64_if_integer(x, &mut buf)
            .map(|len| String::from_utf8(buf[..len].to_vec()).unwrap())
    }

    #[test]
    fn i32_zero_and_small() {
        assert_eq!(fmt_i32(0), "0");
        assert_eq!(fmt_i32(1), "1");
        assert_eq!(fmt_i32(9), "9");
        assert_eq!(fmt_i32(10), "10");
        assert_eq!(fmt_i32(99), "99");
        assert_eq!(fmt_i32(100), "100");
    }

    #[test]
    fn i32_negative() {
        assert_eq!(fmt_i32(-1), "-1");
        assert_eq!(fmt_i32(-42), "-42");
        assert_eq!(fmt_i32(-2147483648), "-2147483648");
    }

    #[test]
    fn i32_extremes() {
        assert_eq!(fmt_i32(i32::MAX), "2147483647");
        assert_eq!(fmt_i32(i32::MIN), "-2147483648");
    }

    #[test]
    fn i64_extremes() {
        assert_eq!(fmt_i64(i64::MAX), "9223372036854775807");
        assert_eq!(fmt_i64(i64::MIN), "-9223372036854775808");
    }

    #[test]
    fn i64_random_values() {
        for n in [
            0i64,
            1,
            -1,
            12345,
            -12345,
            999_999_999,
            1_000_000_000,
            -1_000_000_000,
            1_234_567_890_123,
            -1_234_567_890_123,
        ] {
            assert_eq!(fmt_i64(n), n.to_string(), "n = {n}");
        }
    }

    #[test]
    fn f64_integer_fast_path_hits() {
        assert_eq!(fmt_f64(0.0).as_deref(), Some("0"));
        assert_eq!(fmt_f64(-0.0).as_deref(), Some("0"));
        assert_eq!(fmt_f64(1.0).as_deref(), Some("1"));
        assert_eq!(fmt_f64(-1.0).as_deref(), Some("-1"));
        assert_eq!(fmt_f64(1234567890.0).as_deref(), Some("1234567890"));
        assert_eq!(
            fmt_f64(-9007199254740991.0).as_deref(),
            Some("-9007199254740991")
        );
    }

    #[test]
    fn f64_integer_fast_path_rejects_non_integer() {
        assert_eq!(fmt_f64(1.5), None);
        assert_eq!(fmt_f64(0.1), None);
        assert_eq!(fmt_f64(1e-7), None);
    }

    #[test]
    fn f64_integer_fast_path_rejects_out_of_range() {
        // 2^53 itself is the boundary — caller falls back so the
        // shortest-decimal path can decide. 2^53 - 1 is in range.
        let two_53 = (1u64 << 53) as f64;
        assert_eq!(fmt_f64(two_53), None);
        assert_eq!(fmt_f64(two_53 - 1.0).as_deref(), Some("9007199254740991"));
    }

    #[test]
    fn f64_integer_fast_path_rejects_non_finite() {
        assert_eq!(fmt_f64(f64::NAN), None);
        assert_eq!(fmt_f64(f64::INFINITY), None);
        assert_eq!(fmt_f64(f64::NEG_INFINITY), None);
    }

    #[test]
    fn f64_negative_zero_renders_as_zero() {
        // ECMA-262 ToString(-0) → "0".
        assert_eq!(fmt_f64(-0.0).as_deref(), Some("0"));
    }
}
