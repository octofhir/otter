//! ECMA-262 `Number::ToString(x)` (radix 10) wrapper.
//!
//! Routes through, in order:
//! 1. Cold paths: `NaN`, `±Infinity`, `±0`.
//! 2. Integer fast path: any finite, integral `|x| < 2^53` →
//!    [`super::integer_fast`].
//! 3. Schubfach core for arbitrary finite non-integer values.
//!
//! Output format follows §6.1.6.1.13 step 5 exactly: chooses between
//! a positional emit (with optional trailing zeros), a positional
//! emit with a `'.'`, a leading-zero positional emit, or computerized
//! scientific notation `m.dddde±EE`.
//!
//! # Contents
//! - [`f64_to_ecma_string_buf`] — `(f64, &mut [u8; 32]) → usize`.
//! - [`number_to_string`] — `(f64, &StringHeap) → JsString`.
//! - [`ECMA_BUF_LEN`] — worst-case stack buffer length.
//!
//! # Invariants
//! - Output is ASCII; every byte is `< 0x80`.
//! - Hot path performs no heap allocation. The single allocation
//!   that builds the resulting [`JsString`] happens at the end of
//!   [`number_to_string`].
//! - `±0` renders as `"0"` (sign of zero is dropped per
//!   <https://tc39.es/ecma262/#sec-numeric-types-number-tostring>).
//!
//! # ECMA-262
//! - <https://tc39.es/ecma262/#sec-numeric-types-number-tostring>
//! - <https://tc39.es/ecma262/#sec-tostring> (Number arm).

use super::digit_pair::DIGIT_PAIRS;
use super::integer_fast;
use super::schubfach::schubfach_finite;
use crate::string::JsString;

/// Stack-buffer length sufficient for any `Number::ToString` output.
///
/// Worst case is `-1.7976931348623157e+308` at 24 bytes; 32 leaves
/// headroom and aligns the buffer in cache lines for callers.
pub const ECMA_BUF_LEN: usize = 32;

/// Format `x` per ECMA-262 §6.1.6.1.13 into the front of `out` and
/// return the number of bytes written.
#[inline]
pub fn f64_to_ecma_string_buf(x: f64, out: &mut [u8; ECMA_BUF_LEN]) -> usize {
    if x.is_nan() {
        return cold_nan(out);
    }
    if x.is_infinite() {
        return cold_infinity(x, out);
    }
    if x == 0.0 {
        out[0] = b'0';
        return 1;
    }
    finite_nonzero(x, out)
}

/// Convert `x` to a [`JsString`] per ECMA-262 §6.1.6.1.13.
///
/// # Errors
/// Returns [`StringError::OutOfMemory`] if `heap` cannot accommodate
/// the resulting string.
pub fn number_to_string(
    x: f64,
    heap: &mut otter_gc::GcHeap,
) -> Result<JsString, otter_gc::OutOfMemory> {
    let mut buf = [0u8; ECMA_BUF_LEN];
    let len = f64_to_ecma_string_buf(x, &mut buf);
    // Output is ASCII; route directly into the Latin-1 `Thin`
    // variant of `JsString`. Skips the `&str → Vec<u16> → Arc<[u16]>`
    // widening that `JsString::from_str` does — single allocation,
    // no per-byte copy.
    JsString::from_latin1(&buf[..len], heap)
}

#[inline]
fn finite_nonzero(x: f64, out: &mut [u8; ECMA_BUF_LEN]) -> usize {
    let mut pos = 0;
    let abs = if x.is_sign_negative() {
        out[pos] = b'-';
        pos += 1;
        -x
    } else {
        x
    };

    // Integer fast path. Sub-buffer is large enough for any output
    // the fast path can produce (16 digits + sign).
    let mut int_buf = [0u8; integer_fast::I64_BUF_LEN];
    if let Some(n) = integer_fast::format_f64_if_integer(abs, &mut int_buf) {
        out[pos..pos + n].copy_from_slice(&int_buf[..n]);
        return pos + n;
    }

    // Schubfach core gives `value = sig · 10^exp`. Two passes of
    // shortening follow:
    // 1. Strip trailing zeros (cheap; covers the common case where
    //    Schubfach left full-precision digits ending in zeros).
    // 2. Iteratively round one digit off via half-to-even and check
    //    round-trip — handles subnormal corners where Schubfach's
    //    one-shot `s≥100` trim can't reach the ECMA-mandated
    //    minimum digit count.
    let dec = schubfach_finite(abs);
    let (sig, exp) = strip_trailing_zeros(dec.significand, dec.exponent);
    let (sig, exp) = shorten_to_minimum(sig, exp, abs);

    let mut digit_buf = [0u8; integer_fast::I64_BUF_LEN];
    let k = format_unsigned(sig, &mut digit_buf);
    let digits = &digit_buf[..k];
    let n = exp + k as i32;

    pos + emit_step5(digits, k as i32, n, &mut out[pos..])
}

/// Apply ECMA-262 §6.1.6.1.13 step 5 dispatch and emit into `out`,
/// returning the number of bytes written.
fn emit_step5(digits: &[u8], k: i32, n: i32, out: &mut [u8]) -> usize {
    let mut pos = 0usize;
    if k <= n && n <= 21 {
        // Step 5(a): all digits, then `(n-k)` trailing zeros.
        out[pos..pos + k as usize].copy_from_slice(digits);
        pos += k as usize;
        for _ in 0..(n - k) {
            out[pos] = b'0';
            pos += 1;
        }
    } else if 0 < n && n <= 21 {
        // Step 5(b): first `n` digits, `'.'`, remaining `(k-n)`.
        out[pos..pos + n as usize].copy_from_slice(&digits[..n as usize]);
        pos += n as usize;
        out[pos] = b'.';
        pos += 1;
        out[pos..pos + (k - n) as usize].copy_from_slice(&digits[n as usize..]);
        pos += (k - n) as usize;
    } else if -6 < n && n <= 0 {
        // Step 5(c): `"0."`, `(-n)` zeros, all `k` digits.
        out[pos] = b'0';
        out[pos + 1] = b'.';
        pos += 2;
        for _ in 0..(-n) {
            out[pos] = b'0';
            pos += 1;
        }
        out[pos..pos + k as usize].copy_from_slice(digits);
        pos += k as usize;
    } else {
        // Step 5(d, e): scientific.
        out[pos] = digits[0];
        pos += 1;
        if k > 1 {
            out[pos] = b'.';
            pos += 1;
            out[pos..pos + (k - 1) as usize].copy_from_slice(&digits[1..]);
            pos += (k - 1) as usize;
        }
        out[pos] = b'e';
        pos += 1;
        let exp = n - 1;
        let abs_exp = if exp < 0 {
            out[pos] = b'-';
            pos += 1;
            (-exp) as u64
        } else {
            out[pos] = b'+';
            pos += 1;
            exp as u64
        };
        pos += format_unsigned(abs_exp, &mut out[pos..]);
    }
    pos
}

/// Strip trailing decimal zeros from `(sig, exp)`. Increments `exp`
/// by one for each `0` removed from the low end of `sig`.
#[inline]
fn strip_trailing_zeros(mut sig: u64, mut exp: i32) -> (u64, i32) {
    while sig != 0 && sig.is_multiple_of(10) {
        sig /= 10;
        exp += 1;
    }
    (sig, exp)
}

/// Iteratively shorten `(sig, exp)` until no further digit can be
/// dropped while still round-tripping to `target`. Each step rounds
/// the last digit half-to-even into its neighbour and verifies via
/// stdlib `f64::from_str`. Up to ~17 iterations for any `f64` input.
///
/// Schubfach's published algorithm performs at most one digit drop
/// (`s ≥ 100`). For ECMA-262 minimum-digit conformance we need an
/// unbounded shortening loop; subnormal-extreme inputs (e.g. the
/// bit-1 value `2^-1074`) require multi-digit reduction the paper's
/// one-shot trim cannot reach.
#[inline]
fn shorten_to_minimum(mut sig: u64, mut exp: i32, target: f64) -> (u64, i32) {
    let target_bits = target.to_bits();
    let mut scratch = [0u8; 32];
    while sig >= 10 {
        let dropped = sig % 10;
        let head = sig / 10;
        // Round half-to-even on the discarded digit.
        let rounded = if dropped < 5 || (dropped == 5 && head & 1 == 0) {
            head
        } else {
            head + 1
        };
        if !parses_to(rounded, exp + 1, target_bits, &mut scratch) {
            break;
        }
        sig = rounded;
        exp += 1;
    }
    (sig, exp)
}

/// Heap-free check: does `<sig>e<exp>` parse to the `f64` whose bit
/// pattern is `target_bits`?
fn parses_to(sig: u64, exp: i32, target_bits: u64, scratch: &mut [u8; 32]) -> bool {
    let mut pos = format_unsigned(sig, &mut scratch[..]);
    scratch[pos] = b'e';
    pos += 1;
    let abs_exp = if exp < 0 {
        scratch[pos] = b'-';
        pos += 1;
        (-exp) as u64
    } else {
        exp as u64
    };
    pos += format_unsigned(abs_exp, &mut scratch[pos..]);
    let s = core::str::from_utf8(&scratch[..pos]).expect("ASCII");
    match s.parse::<f64>() {
        Ok(v) => v.to_bits() == target_bits,
        Err(_) => false,
    }
}

/// Emit the decimal digits of `n` into the front of `out`. Returns
/// the byte count. Local copy of [`integer_fast`]'s internal helper
/// to avoid widening that crate's public surface.
#[inline]
fn format_unsigned(mut n: u64, out: &mut [u8]) -> usize {
    if n < 10 {
        out[0] = b'0' + n as u8;
        return 1;
    }
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

#[cold]
#[inline(never)]
fn cold_nan(out: &mut [u8; ECMA_BUF_LEN]) -> usize {
    out[..3].copy_from_slice(b"NaN");
    3
}

#[cold]
#[inline(never)]
fn cold_infinity(x: f64, out: &mut [u8; ECMA_BUF_LEN]) -> usize {
    if x.is_sign_negative() {
        out[..9].copy_from_slice(b"-Infinity");
        9
    } else {
        out[..8].copy_from_slice(b"Infinity");
        8
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fmt(x: f64) -> String {
        let mut buf = [0u8; ECMA_BUF_LEN];
        let len = f64_to_ecma_string_buf(x, &mut buf);
        String::from_utf8(buf[..len].to_vec()).unwrap()
    }

    #[test]
    fn special_values() {
        assert_eq!(fmt(f64::NAN), "NaN");
        assert_eq!(fmt(f64::INFINITY), "Infinity");
        assert_eq!(fmt(f64::NEG_INFINITY), "-Infinity");
        assert_eq!(fmt(0.0), "0");
        assert_eq!(fmt(-0.0), "0");
    }

    #[test]
    fn small_integers() {
        assert_eq!(fmt(1.0), "1");
        assert_eq!(fmt(-1.0), "-1");
        assert_eq!(fmt(10.0), "10");
        assert_eq!(fmt(100.0), "100");
        assert_eq!(fmt(1234.0), "1234");
        assert_eq!(fmt(-1234.0), "-1234");
    }

    #[test]
    fn fractions_under_one() {
        assert_eq!(fmt(0.1), "0.1");
        assert_eq!(fmt(0.5), "0.5");
        assert_eq!(fmt(0.25), "0.25");
        assert_eq!(fmt(-0.1), "-0.1");
    }

    #[test]
    fn fractions_at_n_zero_boundary() {
        assert_eq!(fmt(0.01), "0.01");
        assert_eq!(fmt(0.001), "0.001");
        assert_eq!(fmt(0.0001), "0.0001");
        assert_eq!(fmt(0.00001), "0.00001");
        assert_eq!(fmt(0.000001), "0.000001");
        // 1e-7 crosses the `n == -6` boundary into scientific.
        assert_eq!(fmt(1e-7), "1e-7");
    }

    #[test]
    fn mixed_fixed_form() {
        assert_eq!(fmt(1.5), "1.5");
        assert_eq!(fmt(12.34), "12.34");
        assert_eq!(fmt(1234.5678), "1234.5678");
    }

    #[test]
    fn classic_floating_point_oddity() {
        assert_eq!(fmt(0.1_f64 + 0.2), "0.30000000000000004");
    }

    #[test]
    fn exponential_form_high() {
        assert_eq!(fmt(1e21), "1e+21");
        assert_eq!(fmt(1.5e21), "1.5e+21");
        assert_eq!(fmt(-1e21), "-1e+21");
    }

    #[test]
    fn exponential_form_extremes() {
        // Largest finite f64.
        assert_eq!(fmt(f64::MAX), "1.7976931348623157e+308");
        // Smallest normal.
        assert_eq!(fmt(f64::MIN_POSITIVE), "2.2250738585072014e-308");
        // Smallest subnormal.
        assert_eq!(fmt(f64::from_bits(1)), "5e-324");
    }

    #[test]
    fn scientific_form_round_trip() {
        // For shortest-round-trip correctness, the formatter's
        // output must parse back to the same `f64`.
        for &x in &[
            1e21,
            -1e21,
            1.5e21,
            1e-7,
            1e308,
            f64::MAX,
            f64::MIN_POSITIVE,
            f64::from_bits(1),
        ] {
            let s = fmt(x);
            let parsed: f64 = s.parse().unwrap();
            assert_eq!(parsed.to_bits(), x.to_bits(), "{x} -> {s} -> {parsed}");
        }
    }

    #[test]
    fn integer_envelope_round_trip() {
        for &x in &[
            0.0,
            -0.0,
            1.0,
            -1.0,
            123456789.0,
            (1u64 << 53) as f64 - 1.0,
            -((1u64 << 53) as f64 - 1.0),
        ] {
            let s = fmt(x);
            let parsed: f64 = s.parse().unwrap();
            // -0.0 collapses to 0.0 in our output, which is the
            // ECMA-mandated behaviour. Compare via value, not bits,
            // for the zero case.
            if x == 0.0 {
                assert_eq!(parsed, 0.0);
            } else {
                assert_eq!(parsed.to_bits(), x.to_bits(), "{x} -> {s}");
            }
        }
    }

    #[test]
    fn step5a_no_decimal_point() {
        // value · 10^something where the result is an integer with
        // trailing zeros above the f64 integer envelope.
        // 1e20 = 100000000000000000000 — integer, fits in f64
        // exactly. n=21, k=1 → no decimal point branch.
        assert_eq!(fmt(1e20), "100000000000000000000");
    }

    #[test]
    fn dense_random_round_trip() {
        let mut state: u64 = 0xFEEDC0DE_FACEFACE;
        let mut count = 0u32;
        while count < 5000 {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let x = f64::from_bits(state);
            if !x.is_finite() {
                continue;
            }
            let s = fmt(x);
            let parsed: f64 = s.parse().unwrap_or_else(|_| panic!("parse fail: {s}"));
            if x == 0.0 {
                assert_eq!(parsed, 0.0, "{x} -> {s}");
            } else {
                assert_eq!(
                    parsed.to_bits(),
                    x.to_bits(),
                    "round-trip fail: {x} -> {s} -> {parsed}"
                );
            }
            count += 1;
        }
    }
}
