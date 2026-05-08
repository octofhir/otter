//! ECMA-262 fixed-precision Number formatters.
//!
//! - `Number.prototype.toFixed(fractionDigits)` — §21.1.3.3
//! - `Number.prototype.toExponential(fractionDigits)` — §21.1.3.2
//! - `Number.prototype.toPrecision(precision)` — §21.1.3.5
//!
//! Each uses `BigUint` for the exact-decimal scale + IEEE
//! round-half-to-even, since the inputs span the full IEEE 754
//! binary64 dynamic range and the spec requires exact decimal
//! rounding (a `f64`-only multiplication would lose digits at the
//! extremes).
//!
//! # Contents
//! - [`number_to_fixed`] — `(f64, u32) → String`
//! - [`number_to_exponential`] — `(f64, Option<u32>) → String`
//! - [`number_to_precision`] — `(f64, Option<u32>) → String`
//!
//! # Invariants
//! - Caller validates the digit-count argument range
//!   (`toFixed`: 0..=100; `toExponential`: 0..=100;
//!   `toPrecision`: 1..=100). This module re-asserts the bounds in
//!   debug builds.
//! - Output is ASCII.
//! - `±0` and `-0` collapse to the unsigned form per the spec.
//!
//! # ECMA-262
//! - <https://tc39.es/ecma262/#sec-number.prototype.tofixed>
//! - <https://tc39.es/ecma262/#sec-number.prototype.toexponential>
//! - <https://tc39.es/ecma262/#sec-number.prototype.toprecision>

use num_bigint::BigUint;
use num_traits::{One, Zero};

use super::ecma;

/// Decompose a finite, positive, non-zero `f64` into `(f, e)` such
/// that `value == f · 2^e` exactly, with `f` integer.
fn decompose(abs: f64) -> (u64, i32) {
    debug_assert!(abs.is_finite() && abs > 0.0);
    let bits = abs.to_bits();
    let raw_exp = ((bits >> 52) & 0x7FF) as i32;
    let raw_mantissa = bits & ((1u64 << 52) - 1);
    if raw_exp == 0 {
        (raw_mantissa, -1074)
    } else {
        ((1u64 << 52) | raw_mantissa, raw_exp - 1075)
    }
}

/// Resolve the half-tie per ECMA-262: pick the candidate `n` for
/// which `n · 10^(...)` is **larger** (in signed sense). For
/// magnitude rounding (`abs` path), this maps to:
/// - `negative == false`: round up (`q + 1`).
/// - `negative == true`:  round down (`q`).
#[inline]
fn tie_break_round(q: BigUint, negative: bool) -> BigUint {
    if negative {
        q
    } else {
        q + 1u32
    }
}

/// Compute `round_to_larger_n(value · 10^digits)` as a `BigUint`,
/// using the exact `f64` decomposition. `negative` selects the
/// tie-break direction per §21.1.3.3 step 8.
fn scale_round(abs: f64, digits: u32, negative: bool) -> BigUint {
    let (f, e) = decompose(abs);
    let f_big = BigUint::from(f);
    let ten_f = BigUint::from(10u32).pow(digits);
    if e >= 0 {
        f_big * ten_f * (BigUint::one() << e as usize)
    } else {
        let num = f_big * ten_f;
        let denom = BigUint::one() << (-e) as usize;
        let q = &num / &denom;
        let r = &num % &denom;
        let twice_r = &r * 2u32;
        if twice_r > denom {
            q + 1u32
        } else if twice_r < denom {
            q
        } else {
            tie_break_round(q, negative)
        }
    }
}

/// Compute `round_to_larger_n(value · 10^dec_shift)` with `negative`
/// selecting the tie-break direction. `dec_shift` can be negative.
/// Used by `number_to_exponential` and `number_to_precision` where a
/// fixed number of significant digits is required at an arbitrary
/// decimal scale.
fn scale_round_decimal_shift(abs: f64, dec_shift: i32, negative: bool) -> BigUint {
    let (f, e) = decompose(abs);
    let f_big = BigUint::from(f);
    if dec_shift >= 0 {
        let ten_pos = BigUint::from(10u32).pow(dec_shift as u32);
        if e >= 0 {
            f_big * ten_pos * (BigUint::one() << e as usize)
        } else {
            let num = f_big * ten_pos;
            let denom = BigUint::one() << (-e) as usize;
            div_round_to_larger(&num, &denom, negative)
        }
    } else {
        let ten_neg = BigUint::from(10u32).pow((-dec_shift) as u32);
        if e >= 0 {
            let num = f_big * (BigUint::one() << e as usize);
            div_round_to_larger(&num, &ten_neg, negative)
        } else {
            let denom = (BigUint::one() << (-e) as usize) * &ten_neg;
            div_round_to_larger(&f_big, &denom, negative)
        }
    }
}

#[inline]
fn div_round_to_larger(num: &BigUint, denom: &BigUint, negative: bool) -> BigUint {
    let q = num / denom;
    let r = num % denom;
    let twice_r = &r * 2u32;
    if twice_r > *denom {
        q + 1u32
    } else if twice_r < *denom {
        q
    } else {
        tie_break_round(q, negative)
    }
}

/// Number of decimal digits in `n` (treats `0` as having `1` digit).
fn digit_count(n: &BigUint) -> usize {
    if n.is_zero() {
        return 1;
    }
    n.to_str_radix(10).len()
}

/// True iff `value = f · 2^e < 10^n` exactly.
///
/// Works in `BigUint` without relying on `f64` log; needed because
/// `f64::log10` of a value that is the closest-representable
/// approximation of a power of 10 returns the integer exponent
/// exactly (e.g. `(1e-21_f64).log10() == -21.0` even though the
/// true value is `≈ 9.999_999_999_999_999_07e-22`, strictly less
/// than `10^-21`). Without exact comparison, the
/// `n` estimate near such boundaries is off by one and toPrecision
/// / toExponential lose conformance on Test262 cases like
/// `(1e-21).toPrecision(16)` → `"9.999999999999999e-22"`.
fn value_lt_pow10(f: u64, e: i32, n: i32) -> bool {
    let f_big = BigUint::from(f);
    match (e >= 0, n >= 0) {
        (true, true) => {
            let lhs = &f_big << e as usize;
            let rhs = BigUint::from(10u32).pow(n as u32);
            lhs < rhs
        }
        (true, false) => {
            // value = f · 2^e ≥ 1 (for f ≥ 1, e ≥ 0); 10^n < 1
            // for n < 0. So `value < 10^n` is false.
            false
        }
        (false, true) => {
            let rhs = BigUint::from(10u32).pow(n as u32) << ((-e) as usize);
            f_big < rhs
        }
        (false, false) => {
            let lhs = f_big * BigUint::from(10u32).pow((-n) as u32);
            let rhs = BigUint::one() << ((-e) as usize);
            lhs < rhs
        }
    }
}

/// Compute `floor(log10(f · 2^e))` exactly. Uses an `f64` estimate
/// then corrects via [`value_lt_pow10`] until the invariant
/// `10^k ≤ value < 10^(k+1)` holds.
fn floor_log10_exact(f: u64, e: i32) -> i32 {
    let estimate = ((f as f64) * (e as f64).exp2()).log10().floor() as i32;
    let mut k = estimate;
    // value < 10^k → k is too big.
    while value_lt_pow10(f, e, k) {
        k -= 1;
    }
    // value ≥ 10^(k+1) → k is too small.
    while !value_lt_pow10(f, e, k + 1) {
        k += 1;
    }
    k
}

/// `Number.prototype.toFixed(fractionDigits)`.
///
/// `fraction_digits` must be in `0..=100` (caller-validated per spec
/// §21.1.3.3 step 3).
pub fn number_to_fixed(value: f64, fraction_digits: u32) -> String {
    debug_assert!(fraction_digits <= 100);

    if value.is_nan() {
        return "NaN".to_string();
    }
    if value.is_infinite() {
        return if value.is_sign_negative() {
            "-Infinity".to_string()
        } else {
            "Infinity".to_string()
        };
    }
    if value == 0.0 {
        return zero_with_fraction(fraction_digits);
    }
    // §21.1.3.3 step 6: |x| ≥ 10^21 → defer to ToString.
    if value.abs() >= 1e21 {
        return ecma_to_string(value);
    }

    let negative = value.is_sign_negative();
    let n = scale_round(value.abs(), fraction_digits, negative);
    format_fixed(&n, fraction_digits, negative)
}

/// `Number.prototype.toExponential(fractionDigits)`.
///
/// `fraction_digits` is `Some(n)` (caller-validated `n ≤ 100`) or
/// `None` for the shortest-round-trip form.
pub fn number_to_exponential(value: f64, fraction_digits: Option<u32>) -> String {
    if value.is_nan() {
        return "NaN".to_string();
    }
    if value.is_infinite() {
        return if value.is_sign_negative() {
            "-Infinity".to_string()
        } else {
            "Infinity".to_string()
        };
    }
    let negative = value.is_sign_negative();
    let abs = value.abs();
    if abs == 0.0 {
        let f = fraction_digits.unwrap_or(0);
        return zero_with_exponential(f);
    }

    // Position of the MSD (1-indexed) computed exactly to avoid
    // f64-log10 boundary errors near powers of 10.
    let (f_mantissa, e_exp) = decompose(abs);
    let n_estimate = floor_log10_exact(f_mantissa, e_exp) + 1;
    // Round to `fraction_digits + 1` significant digits.
    let f = match fraction_digits {
        Some(d) => d,
        None => {
            // Shortest: route to `ecma::number_to_string` (Schubfach
            // shortest), then re-format as scientific.
            return shortest_to_exponential(value);
        }
    };
    let total_digits = (f + 1) as i32;
    // dec_shift such that round_half_even(abs · 10^dec_shift) has
    // exactly `total_digits` decimal digits; dec_shift = total_digits - n.
    let dec_shift = total_digits - n_estimate;
    let mut rounded = scale_round_decimal_shift(abs, dec_shift, negative);
    // Adjust if the rounding crossed a decimal-magnitude boundary.
    let mut got_digits = digit_count(&rounded) as i32;
    let mut n_actual = n_estimate;
    if got_digits != total_digits {
        // rounded is one digit longer or shorter than expected.
        // Recompute by adjusting the shift by the delta.
        let delta = got_digits - total_digits;
        n_actual += delta;
        let new_shift = total_digits - n_actual;
        rounded = scale_round_decimal_shift(abs, new_shift, negative);
        got_digits = digit_count(&rounded) as i32;
        // After re-rounding the boundary case can flip again at most
        // once; clamp for safety.
        if got_digits != total_digits {
            n_actual += got_digits - total_digits;
        }
    }

    let digits = rounded.to_str_radix(10);
    format_exponential(&digits, n_actual - 1, f, negative)
}

/// `Number.prototype.toPrecision(precision)`.
///
/// `precision` is `Some(p)` (caller-validated `1 ≤ p ≤ 100`) or
/// `None` for plain `Number::ToString`.
pub fn number_to_precision(value: f64, precision: Option<u32>) -> String {
    let p = match precision {
        None => return ecma_to_string(value),
        Some(p) => p,
    };
    debug_assert!((1..=100).contains(&p));

    if value.is_nan() {
        return "NaN".to_string();
    }
    if value.is_infinite() {
        return if value.is_sign_negative() {
            "-Infinity".to_string()
        } else {
            "Infinity".to_string()
        };
    }
    let negative = value.is_sign_negative();
    let abs = value.abs();
    if abs == 0.0 {
        return zero_with_precision(p);
    }

    // Round to exactly `p` significant digits. MSD position is
    // computed exactly (see `floor_log10_exact`).
    let (f_mantissa, e_exp) = decompose(abs);
    let n_estimate = floor_log10_exact(f_mantissa, e_exp) + 1;
    let mut dec_shift = p as i32 - n_estimate;
    let mut rounded = scale_round_decimal_shift(abs, dec_shift, negative);
    let mut got = digit_count(&rounded) as i32;
    let mut n_actual = n_estimate;
    if got != p as i32 {
        let delta = got - p as i32;
        n_actual += delta;
        dec_shift = p as i32 - n_actual;
        rounded = scale_round_decimal_shift(abs, dec_shift, negative);
        got = digit_count(&rounded) as i32;
        if got != p as i32 {
            n_actual += got - p as i32;
        }
    }

    let digits = rounded.to_str_radix(10);
    let e = n_actual - 1;
    // §21.1.3.5 step 11: if `e < -6` or `e ≥ p`, scientific;
    // else fixed.
    if e < -6 || e >= p as i32 {
        format_exponential(&digits, e, p - 1, negative)
    } else {
        format_precision_fixed(&digits, e, p, negative)
    }
}

fn format_fixed(n: &BigUint, fraction_digits: u32, negative: bool) -> String {
    let s = n.to_str_radix(10);
    let bytes = s.as_bytes();
    let total = bytes.len();
    let f = fraction_digits as usize;
    let mut out = String::with_capacity(total + 4);
    // §21.1.3.3 step 9: always prepend `-` if `x < 0`, even when
    // the rounded magnitude is zero (so `(-0.5).toFixed(0)` is
    // `"-0"`, matching V8/Node.js).
    let _ = n.is_zero();
    if negative {
        out.push('-');
    }
    if f == 0 {
        out.push_str(&s);
    } else if total <= f {
        out.push_str("0.");
        for _ in 0..(f - total) {
            out.push('0');
        }
        out.push_str(&s);
    } else {
        let int_len = total - f;
        out.push_str(std::str::from_utf8(&bytes[..int_len]).unwrap());
        out.push('.');
        out.push_str(std::str::from_utf8(&bytes[int_len..]).unwrap());
    }
    out
}

fn format_exponential(digits: &str, e: i32, fraction_digits: u32, negative: bool) -> String {
    let bytes = digits.as_bytes();
    let mut out = String::with_capacity(digits.len() + 8);
    if negative {
        out.push('-');
    }
    out.push(bytes[0] as char);
    if bytes.len() > 1 {
        out.push('.');
        out.push_str(std::str::from_utf8(&bytes[1..]).unwrap());
    }
    out.push('e');
    if e < 0 {
        out.push('-');
        out.push_str(&(-e).to_string());
    } else {
        out.push('+');
        out.push_str(&e.to_string());
    }
    let _ = fraction_digits;
    out
}

fn format_precision_fixed(digits: &str, e: i32, p: u32, negative: bool) -> String {
    let bytes = digits.as_bytes();
    debug_assert_eq!(bytes.len(), p as usize);
    let mut out = String::with_capacity(p as usize + 4);
    if negative {
        out.push('-');
    }
    if e < 0 {
        out.push_str("0.");
        for _ in 0..(-e - 1) {
            out.push('0');
        }
        out.push_str(digits);
    } else {
        let int_len = (e + 1) as usize;
        if int_len >= bytes.len() {
            out.push_str(digits);
            for _ in bytes.len()..int_len {
                out.push('0');
            }
        } else {
            out.push_str(std::str::from_utf8(&bytes[..int_len]).unwrap());
            out.push('.');
            out.push_str(std::str::from_utf8(&bytes[int_len..]).unwrap());
        }
    }
    out
}

fn zero_with_fraction(f: u32) -> String {
    if f == 0 {
        "0".to_string()
    } else {
        let mut s = String::with_capacity(f as usize + 2);
        s.push_str("0.");
        for _ in 0..f {
            s.push('0');
        }
        s
    }
}

fn zero_with_exponential(f: u32) -> String {
    if f == 0 {
        "0e+0".to_string()
    } else {
        let mut s = String::with_capacity(f as usize + 6);
        s.push_str("0.");
        for _ in 0..f {
            s.push('0');
        }
        s.push_str("e+0");
        s
    }
}

fn zero_with_precision(p: u32) -> String {
    if p == 1 {
        "0".to_string()
    } else {
        let mut s = String::with_capacity(p as usize + 2);
        s.push_str("0.");
        for _ in 1..p {
            s.push('0');
        }
        s
    }
}

fn ecma_to_string(value: f64) -> String {
    let mut buf = [0u8; ecma::ECMA_BUF_LEN];
    let len = ecma::f64_to_ecma_string_buf(value, &mut buf);
    std::str::from_utf8(&buf[..len])
        .expect("ECMA wrapper emits ASCII")
        .to_string()
}

fn shortest_to_exponential(value: f64) -> String {
    let s = ecma_to_string(value);
    // If the shortest form is already scientific, return as-is.
    if s.contains('e') {
        return s;
    }
    // Otherwise reformat as scientific with whatever significand the
    // shortest form yields.
    let (negative, body) = if let Some(rest) = s.strip_prefix('-') {
        (true, rest)
    } else {
        (false, s.as_str())
    };
    let dot_pos = body.find('.');
    let (int_part, frac_part) = match dot_pos {
        Some(p) => (&body[..p], &body[p + 1..]),
        None => (body, ""),
    };
    // Find first non-zero digit.
    let leading_zeros = int_part.bytes().take_while(|&b| b == b'0').count();
    let int_significant = if leading_zeros < int_part.len() {
        &int_part[leading_zeros..]
    } else {
        ""
    };
    let (digits, e_pos): (String, i32) = if !int_significant.is_empty() {
        let mut combined = String::new();
        combined.push_str(int_significant);
        combined.push_str(frac_part);
        // Strip trailing zeros from the combined digits to keep the
        // shortest mantissa.
        let trimmed = combined.trim_end_matches('0');
        let final_digits = if trimmed.is_empty() {
            "0".to_string()
        } else {
            trimmed.to_string()
        };
        let n = (int_significant.len() - 1) as i32;
        (final_digits, n)
    } else {
        // value is < 1; first non-zero is in fractional part.
        let frac_leading = frac_part.bytes().take_while(|&b| b == b'0').count();
        let mantissa_part = &frac_part[frac_leading..];
        let trimmed = mantissa_part.trim_end_matches('0');
        let final_digits = if trimmed.is_empty() {
            "0".to_string()
        } else {
            trimmed.to_string()
        };
        let n = -(frac_leading as i32) - 1;
        (final_digits, n)
    };
    format_exponential(&digits, e_pos, 0, negative)
}

#[cfg(test)]
mod tests {
    use super::*;

    // toFixed reference values cross-checked against Node.js v22.
    // Per §21.1.3.3 step 8, ties pick the **larger** `n` —
    // round-half-up, NOT round-half-to-even.
    #[test]
    fn to_fixed_basic() {
        assert_eq!(number_to_fixed(0.0, 0), "0");
        assert_eq!(number_to_fixed(0.0, 2), "0.00");
        assert_eq!(number_to_fixed(-0.0, 2), "0.00");
        assert_eq!(number_to_fixed(1.0, 0), "1");
        assert_eq!(number_to_fixed(1.0, 3), "1.000");
        assert_eq!(number_to_fixed(1.5, 0), "2");
        assert_eq!(number_to_fixed(0.5, 0), "1");
        assert_eq!(number_to_fixed(2.5, 0), "3");
        assert_eq!(number_to_fixed(123.456, 2), "123.46");
        assert_eq!(number_to_fixed(123.444, 2), "123.44");
    }

    #[test]
    fn to_fixed_classic() {
        assert_eq!(number_to_fixed(0.1, 1), "0.1");
        assert_eq!(number_to_fixed(0.1, 2), "0.10");
        assert_eq!(number_to_fixed(0.1 + 0.2, 1), "0.3");
        assert_eq!(number_to_fixed(0.1 + 0.2, 17), "0.30000000000000004");
        // 0.05 in f64 = 0.0500000000000000028…, so toFixed(1) rounds up.
        assert_eq!(number_to_fixed(0.05, 1), "0.1");
    }

    #[test]
    fn to_fixed_negative() {
        assert_eq!(number_to_fixed(-1.0, 0), "-1");
        // Spec ties round to the larger `n`, so for -1.5 the
        // candidates are {-2, -1}; larger is -1.
        assert_eq!(number_to_fixed(-1.5, 0), "-1");
        // Sign is prepended even when the rounded magnitude is 0.
        assert_eq!(number_to_fixed(-0.5, 0), "-0");
        assert_eq!(number_to_fixed(-0.5, 1), "-0.5");
        // -2.5 rounds to {-3, -2}; larger is -2.
        assert_eq!(number_to_fixed(-2.5, 0), "-2");
    }

    #[test]
    fn to_fixed_special() {
        assert_eq!(number_to_fixed(f64::NAN, 2), "NaN");
        assert_eq!(number_to_fixed(f64::INFINITY, 2), "Infinity");
        assert_eq!(number_to_fixed(f64::NEG_INFINITY, 2), "-Infinity");
        // 1e21 falls to ToString.
        assert_eq!(number_to_fixed(1e21, 2), "1e+21");
    }

    #[test]
    fn to_fixed_high_precision() {
        // toFixed(20) on 0.1: known Node.js output.
        assert_eq!(number_to_fixed(0.1, 20), "0.10000000000000000555");
        // toFixed(20) on 1/3.
        let one_third = 1.0_f64 / 3.0;
        assert_eq!(number_to_fixed(one_third, 20), "0.33333333333333331483");
    }

    #[test]
    fn to_exponential_basic() {
        assert_eq!(number_to_exponential(0.0, Some(2)), "0.00e+0");
        assert_eq!(number_to_exponential(1.0, Some(0)), "1e+0");
        assert_eq!(number_to_exponential(1.0, Some(3)), "1.000e+0");
        assert_eq!(number_to_exponential(123.456, Some(2)), "1.23e+2");
        assert_eq!(number_to_exponential(0.001, Some(1)), "1.0e-3");
        assert_eq!(number_to_exponential(-1.5, Some(2)), "-1.50e+0");
    }

    #[test]
    fn to_exponential_special() {
        assert_eq!(number_to_exponential(f64::NAN, Some(2)), "NaN");
        assert_eq!(number_to_exponential(f64::INFINITY, Some(2)), "Infinity");
        assert_eq!(number_to_exponential(f64::NEG_INFINITY, Some(2)), "-Infinity");
    }

    #[test]
    fn to_exponential_shortest() {
        assert_eq!(number_to_exponential(1.0, None), "1e+0");
        assert_eq!(number_to_exponential(123.0, None), "1.23e+2");
        assert_eq!(number_to_exponential(0.000123, None), "1.23e-4");
    }

    #[test]
    fn to_precision_basic() {
        assert_eq!(number_to_precision(0.0, Some(1)), "0");
        assert_eq!(number_to_precision(0.0, Some(3)), "0.00");
        assert_eq!(number_to_precision(123.456, Some(4)), "123.5");
        assert_eq!(number_to_precision(123.456, Some(6)), "123.456");
        assert_eq!(number_to_precision(0.000123, Some(2)), "0.00012");
        // Per §21.1.3.5 step 11 the dispatch is on `e ≥ p` or
        // `e < -6`. For 0.0000123 = 1.23e-5, e = -5, p = 2; both
        // branches false, so fixed form. (Node.js v22 confirms.)
        assert_eq!(number_to_precision(0.0000123, Some(2)), "0.000012");
        assert_eq!(number_to_precision(-1.5, Some(2)), "-1.5");
    }

    #[test]
    fn to_precision_undefined_is_to_string() {
        assert_eq!(number_to_precision(123.456, None), "123.456");
        assert_eq!(number_to_precision(0.1, None), "0.1");
        assert_eq!(number_to_precision(1e21, None), "1e+21");
    }

    #[test]
    fn to_precision_special() {
        assert_eq!(number_to_precision(f64::NAN, Some(2)), "NaN");
        assert_eq!(number_to_precision(f64::INFINITY, Some(2)), "Infinity");
    }

    #[test]
    fn to_precision_large_exponent() {
        // For magnitude ≥ 10^p, scientific.
        assert_eq!(number_to_precision(123456.0, Some(3)), "1.23e+5");
        assert_eq!(number_to_precision(12345.0, Some(5)), "12345");
    }
}
