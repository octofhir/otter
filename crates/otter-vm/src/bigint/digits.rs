//! Magnitude kernels over little-endian `u64` digit spans.
//!
//! The V8 `src/bigint` model: an operation sizes its result, allocates it
//! once, and a kernel writes the digits straight into it. Kernels read
//! borrowed spans and write a caller-provided output span, so they never
//! allocate a result of their own.
//!
//! # Contents
//! - [`significant_len`], [`compare`] over normalized spans.
//! - [`add`], [`sub`]: additive kernels.
//! - [`mul`]: schoolbook below [`KARATSUBA_THRESHOLD`], `num_bigint`'s
//!   Karatsuba / Toom-3 above it.
//! - [`div_rem`]: one-digit divisors directly, longer ones by Knuth's
//!   algorithm D.
//! - [`shl`], [`shr`]: bit shifts.
//! - [`and`], [`or`], [`xor`], [`and_not`]: bitwise kernels over magnitudes;
//!   [`super::ops`] maps two's-complement semantics onto them.
//! - [`from_f64_integral`]: an integral double's exact magnitude.
//!
//! # Invariants
//! - Input spans are normalized (no leading zero digit) unless a kernel
//!   says otherwise.
//! - A kernel writes every digit of the output capacity it documents, so the
//!   output may hold anything on entry; digits past that capacity are
//!   untouched.
//!
//! # See also
//! - [`super::ops`] sizes and allocates results and applies signs.

use std::cmp::Ordering;

use num_bigint::BigUint;
use smallvec::SmallVec;

/// Digit count at which both factors take `num_bigint`'s sub-quadratic
/// multiplication instead of the schoolbook loop (V8 `kKaratsubaThreshold`).
pub(crate) const KARATSUBA_THRESHOLD: usize = 34;

/// Scratch digits a kernel keeps on the stack before spilling to the heap.
pub(crate) type Scratch = SmallVec<[u64; 8]>;

/// Digits of `d` up to and including its most significant non-zero digit.
#[inline]
#[must_use]
pub(crate) fn significant_len(d: &[u64]) -> usize {
    d.iter()
        .rposition(|&digit| digit != 0)
        .map_or(0, |top| top + 1)
}

/// Magnitude comparison of two normalized spans.
#[inline]
#[must_use]
pub(crate) fn compare(a: &[u64], b: &[u64]) -> Ordering {
    a.len()
        .cmp(&b.len())
        .then_with(|| a.iter().rev().cmp(b.iter().rev()))
}

/// `out[..=max(a.len(), b.len())] = a + b`.
pub(crate) fn add(a: &[u64], b: &[u64], out: &mut [u64]) {
    let (a, b) = if a.len() >= b.len() { (a, b) } else { (b, a) };
    let (low, high) = a.split_at(b.len());
    let mut carry = false;
    for ((slot, &x), &y) in out.iter_mut().zip(low).zip(b) {
        (*slot, carry) = x.carrying_add(y, carry);
    }
    for (slot, &x) in out[b.len()..].iter_mut().zip(high) {
        (*slot, carry) = x.overflowing_add(u64::from(carry));
    }
    out[a.len()] = u64::from(carry);
}

/// `out[..a.len()] = a - b` for `a >= b`.
pub(crate) fn sub(a: &[u64], b: &[u64], out: &mut [u64]) {
    let (low, high) = a.split_at(b.len());
    let mut borrow = false;
    for ((slot, &x), &y) in out.iter_mut().zip(low).zip(b) {
        (*slot, borrow) = x.borrowing_sub(y, borrow);
    }
    for (slot, &x) in out[b.len()..].iter_mut().zip(high) {
        (*slot, borrow) = x.overflowing_sub(u64::from(borrow));
    }
    debug_assert!(!borrow, "magnitude subtraction underflowed");
}

/// `out[..a.len() + b.len()] = a * b` for non-empty factors.
pub(crate) fn mul(a: &[u64], b: &[u64], out: &mut [u64]) {
    let out = &mut out[..a.len() + b.len()];
    out.fill(0);
    if a.len().min(b.len()) >= KARATSUBA_THRESHOLD {
        let product = to_biguint(a) * to_biguint(b);
        for (slot, digit) in out.iter_mut().zip(product.iter_u64_digits()) {
            *slot = digit;
        }
        return;
    }
    for (i, &x) in a.iter().enumerate() {
        let row = &mut out[i..=i + b.len()];
        let mut carry = 0u64;
        for (slot, &y) in row.iter_mut().zip(b) {
            let t = u128::from(x) * u128::from(y) + u128::from(*slot) + u128::from(carry);
            *slot = t as u64;
            carry = (t >> 64) as u64;
        }
        row[b.len()] = carry;
    }
}

/// `(hi:lo) / d` and its remainder, for `hi < d` with `d`'s top bit set:
/// Hacker's Delight `divlu` (V8 `digit_div`), two native divisions.
#[inline]
fn div_wide(hi: u64, lo: u64, d: u64) -> (u64, u64) {
    debug_assert!(hi < d && d >> 63 == 1);
    const HALF: u64 = 1 << 32;
    let (d1, d0) = (d >> 32, d & (HALF - 1));
    let (l1, l0) = (lo >> 32, lo & (HALF - 1));
    let half_digit = |numerator: u64, next: u64| {
        let (mut q, mut r) = (numerator / d1, numerator % d1);
        while q >= HALF || q * d0 > ((r << 32) | next) {
            q -= 1;
            r += d1;
            if r >= HALF {
                break;
            }
        }
        q
    };
    let q1 = half_digit(hi, l1);
    let middle = ((hi << 32) | l1).wrapping_sub(q1.wrapping_mul(d));
    let q0 = half_digit(middle, l0);
    let remainder = ((middle << 32) | l0).wrapping_sub(q0.wrapping_mul(d));
    ((q1 << 32) | q0, remainder)
}

/// Quotient and remainder of `a / b` for a non-empty normalized divisor and
/// `a.len() >= b.len()`. `q` takes `a.len() - b.len() + 1` digits and `r`
/// takes `b.len()` digits.
pub(crate) fn div_rem(a: &[u64], b: &[u64], q: Option<&mut [u64]>, r: Option<&mut [u64]>) {
    debug_assert!(!b.is_empty() && a.len() >= b.len());
    if let [divisor] = *b {
        let remainder = div_digit(a, divisor, q);
        if let Some(r) = r {
            r[0] = remainder;
        }
        return;
    }
    div_knuth(a, b, q, r);
}

/// `q[..a.len()] = a / d`, returning `a % d`. The divisor is scaled so its
/// top bit is set and the dividend is shifted on the fly.
fn div_digit(a: &[u64], d: u64, mut q: Option<&mut [u64]>) -> u64 {
    let shift = d.leading_zeros();
    let d = d << shift;
    let spill = |digit: u64| if shift == 0 { 0 } else { digit >> (64 - shift) };
    let mut remainder = spill(a[a.len() - 1]);
    for i in (0..a.len()).rev() {
        let lo = (a[i] << shift) | i.checked_sub(1).map_or(0, |below| spill(a[below]));
        let (digit, rest) = div_wide(remainder, lo, d);
        if let Some(q) = q.as_deref_mut() {
            q[i] = digit;
        }
        remainder = rest;
    }
    remainder >> shift
}

/// Knuth, TAOCP vol. 2 §4.3.1, algorithm D over 64-bit digits.
fn div_knuth(a: &[u64], b: &[u64], mut q: Option<&mut [u64]>, r: Option<&mut [u64]>) {
    let n = b.len();
    // D1: scale so the divisor's top digit has its high bit set.
    let shift = b[n - 1].leading_zeros();
    let mut v = Scratch::from_elem(0, n);
    shl_bits(b, shift, &mut v);
    let mut u = Scratch::from_elem(0, a.len() + 1);
    shl_bits(a, shift, &mut u);
    let (v, u) = (&v[..], &mut u[..]);
    let (v_top, v_next) = (v[n - 1], v[n - 2]);
    for j in (0..=a.len() - n).rev() {
        // D3: estimate the quotient digit from the top dividend digits.
        let (u_top, u_mid, u_low) = (u[j + n], u[j + n - 1], u[j + n - 2]);
        let (mut q_hat, mut r_hat) = if u_top == v_top {
            (u64::MAX, u_mid.checked_add(v_top))
        } else {
            let (q_hat, r_hat) = div_wide(u_top, u_mid, v_top);
            (q_hat, Some(r_hat))
        };
        while let Some(rest) = r_hat
            && u128::from(q_hat) * u128::from(v_next)
                > ((u128::from(rest) << 64) | u128::from(u_low))
        {
            q_hat -= 1;
            r_hat = rest.checked_add(v_top);
        }
        // D4: multiply and subtract.
        let window = &mut u[j..=j + n];
        let (mut carry, mut borrow) = (0u64, false);
        for (slot, &digit) in window.iter_mut().zip(v) {
            let product = u128::from(q_hat) * u128::from(digit) + u128::from(carry);
            carry = (product >> 64) as u64;
            (*slot, borrow) = slot.borrowing_sub(product as u64, borrow);
        }
        let (top, under) = window[n].borrowing_sub(carry, borrow);
        window[n] = top;
        // D6: add back when the estimate was one too large.
        if under {
            q_hat -= 1;
            let mut carry = false;
            for (slot, &digit) in window.iter_mut().zip(v) {
                (*slot, carry) = slot.carrying_add(digit, carry);
            }
            window[n] = window[n].wrapping_add(u64::from(carry));
        }
        if let Some(q) = q.as_deref_mut() {
            q[j] = q_hat;
        }
    }
    // D8: unscale the remainder.
    if let Some(r) = r {
        for (i, slot) in r[..n].iter_mut().enumerate() {
            *slot = if shift == 0 {
                u[i]
            } else {
                (u[i] >> shift) | (u[i + 1] << (64 - shift))
            };
        }
    }
}

/// `out[..=a.len()]` (or `out[..a.len()]` when `out` is that short) =
/// `a << shift` for `shift < 64`.
fn shl_bits(a: &[u64], shift: u32, out: &mut [u64]) {
    let mut carry = 0u64;
    for (i, &x) in a.iter().enumerate() {
        out[i] = if shift == 0 { x } else { (x << shift) | carry };
        carry = if shift == 0 { 0 } else { x >> (64 - shift) };
    }
    if let Some(top) = out.get_mut(a.len()) {
        *top = carry;
    } else {
        debug_assert_eq!(carry, 0, "scaled divisor keeps its length");
    }
}

/// `out[..a.len() + shift / 64 + 1] = a << shift`.
pub(crate) fn shl(a: &[u64], shift: usize, out: &mut [u64]) {
    let digits = shift / 64;
    out[..digits].fill(0);
    shl_bits(a, (shift % 64) as u32, &mut out[digits..=digits + a.len()]);
}

/// `out[..a.len() - shift / 64] = a >> shift` for `shift / 64 < a.len()`;
/// returns whether any shifted-out bit was set.
pub(crate) fn shr(a: &[u64], shift: usize, out: &mut [u64]) -> bool {
    let digits = shift / 64;
    let bits = (shift % 64) as u32;
    let mut lost = a[..digits].iter().any(|&digit| digit != 0);
    if bits != 0 {
        lost |= a[digits] << (64 - bits) != 0;
    }
    let source = &a[digits..];
    for i in 0..source.len() {
        out[i] = if bits == 0 {
            source[i]
        } else {
            (source[i] >> bits) | source.get(i + 1).map_or(0, |&next| next << (64 - bits))
        };
    }
    lost
}

/// `out[..min(a.len(), b.len())] = a & b`.
pub(crate) fn and(a: &[u64], b: &[u64], out: &mut [u64]) {
    for ((slot, &x), &y) in out.iter_mut().zip(a).zip(b) {
        *slot = x & y;
    }
}

/// `out[..a.len()] = a & !b`.
pub(crate) fn and_not(a: &[u64], b: &[u64], out: &mut [u64]) {
    let common = a.len().min(b.len());
    for ((slot, &x), &y) in out.iter_mut().zip(a).zip(b) {
        *slot = x & !y;
    }
    out[common..a.len()].copy_from_slice(&a[common..]);
}

/// `out[..max(a.len(), b.len())] = a | b`.
pub(crate) fn or(a: &[u64], b: &[u64], out: &mut [u64]) {
    merge(a, b, out, |x, y| x | y);
}

/// `out[..max(a.len(), b.len())] = a ^ b`.
pub(crate) fn xor(a: &[u64], b: &[u64], out: &mut [u64]) {
    merge(a, b, out, |x, y| x ^ y);
}

/// A digit-wise operation that keeps the longer operand's excess digits.
#[inline]
fn merge(a: &[u64], b: &[u64], out: &mut [u64], op: impl Fn(u64, u64) -> u64) {
    let (long, short) = if a.len() >= b.len() { (a, b) } else { (b, a) };
    for ((slot, &x), &y) in out.iter_mut().zip(long).zip(short) {
        *slot = op(x, y);
    }
    out[short.len()..long.len()].copy_from_slice(&long[short.len()..]);
}

/// The exact magnitude of an integral, finite double.
///
/// The significand and exponent are read from the IEEE-754 bits, so values
/// past every native integer width convert exactly.
#[must_use]
pub(crate) fn from_f64_integral(value: f64) -> Scratch {
    debug_assert!(value.is_finite() && value.fract() == 0.0);
    let bits = value.to_bits();
    let raw_exponent = ((bits >> 52) & 0x7ff) as i32;
    if raw_exponent == 0 {
        // Zero; a subnormal is never a non-zero integer.
        return Scratch::new();
    }
    let significand = (bits & ((1 << 52) - 1)) | (1 << 52);
    let exponent = raw_exponent - 1075;
    if exponent <= 0 {
        // Integral, so every dropped bit is zero.
        return Scratch::from_elem(significand >> exponent.unsigned_abs(), 1);
    }
    let exponent = exponent as usize;
    let mut out = Scratch::from_elem(0, exponent / 64 + 2);
    shl(&[significand], exponent, &mut out);
    let len = significant_len(&out);
    out.truncate(len);
    out
}

/// A `num_bigint` magnitude over the same digits.
#[must_use]
pub(crate) fn to_biguint(digits: &[u64]) -> BigUint {
    let halves: SmallVec<[u32; 16]> = digits
        .iter()
        .flat_map(|&digit| [digit as u32, (digit >> 32) as u32])
        .collect();
    BigUint::from_slice(&halves)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn big(digits: &[u64]) -> BigUint {
        to_biguint(digits)
    }

    fn digits_of(value: &BigUint) -> Vec<u64> {
        value.to_u64_digits()
    }

    #[test]
    fn knuth_division_matches_reference() {
        let a = [u64::MAX, 7, u64::MAX - 3, 1 << 63, 42];
        let b = [3, u64::MAX >> 1, 9];
        let mut q = [0u64; 3];
        let mut r = [0u64; 3];
        div_rem(&a, &b, Some(&mut q), Some(&mut r));
        let (eq, er) = (big(&a) / big(&b), big(&a) % big(&b));
        assert_eq!(q[..significant_len(&q)], digits_of(&eq)[..]);
        assert_eq!(r[..significant_len(&r)], digits_of(&er)[..]);
    }

    #[test]
    fn one_digit_division_matches_reference() {
        let a = [u64::MAX, 0x0123_4567_89ab_cdef, 1 << 63, 5];
        for d in [1, 3, 10, 1 << 40, u64::MAX, (1 << 63) + 1] {
            let mut q = [0u64; 4];
            let mut r = [0u64; 1];
            div_rem(&a, &[d], Some(&mut q), Some(&mut r));
            let divisor = BigUint::from(d);
            assert_eq!(
                q[..significant_len(&q)],
                digits_of(&(big(&a) / &divisor))[..],
                "{d}"
            );
            assert_eq!(BigUint::from(r[0]), big(&a) % &divisor, "{d}");
        }
    }

    #[test]
    fn schoolbook_and_large_multiplication_match_reference() {
        for len in [1, 5, KARATSUBA_THRESHOLD + 3] {
            let a: Vec<u64> = (0..len as u64)
                .map(|i| i.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1)
                .collect();
            let b: Vec<u64> = (0..len as u64).map(|i| !i).collect();
            let mut out = vec![0u64; a.len() + b.len()];
            mul(&a, &b, &mut out);
            let n = significant_len(&out);
            assert_eq!(out[..n], digits_of(&(big(&a) * big(&b)))[..]);
        }
    }

    #[test]
    fn shifts_round_trip_and_report_lost_bits() {
        let a = [0x8000_0000_0000_0001u64, 3];
        let mut wide = [0u64; 4];
        shl(&a, 67, &mut wide);
        let mut back = [0u64; 3];
        assert!(!shr(&wide, 67, &mut back));
        assert_eq!(back[..2], a);
        let mut narrow = [0u64; 2];
        assert!(shr(&a, 1, &mut narrow));
    }

    #[test]
    fn integral_doubles_convert_exactly() {
        assert_eq!(from_f64_integral(0.0).as_slice(), &[] as &[u64]);
        assert_eq!(from_f64_integral(2f64.powi(64)).as_slice(), &[0, 1]);
        assert_eq!(
            from_f64_integral(9_007_199_254_740_992.0).as_slice(),
            &[9_007_199_254_740_992]
        );
        assert_eq!(from_f64_integral(-0.0).as_slice(), &[] as &[u64]);
    }
}
