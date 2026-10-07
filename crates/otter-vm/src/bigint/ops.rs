//! BigInt operators over GC handles.
//!
//! Each operator reads its operands' signs and lengths, sizes the result,
//! allocates it once (operands rooted across the allocation), and runs a
//! [`super::digits`] kernel straight into it — V8's `MutableBigInt`
//! discipline. An operator whose result equals an operand returns that
//! operand: published BigInts are immutable.
//!
//! # Contents
//! - Arithmetic: [`add`], [`sub`], [`mul`], [`div`], [`rem`], [`pow`],
//!   [`neg`], [`step`].
//! - Bitwise: [`bitwise_and`], [`bitwise_or`], [`bitwise_xor`],
//!   [`bitwise_not`], [`shl`], [`shr`], [`as_int_n`], [`as_uint_n`].
//! - Comparison with a Number: [`compare_to_f64`], [`equals_f64`].
//! - [`OpError`] — failure modes the dispatcher converts to `VmError`.
//! - [`Operator`] — the binary operators by number, for compiled code.
//!
//! # Invariants
//! - Results never exceed [`MAX_BITS`]; a larger one is
//!   [`OpError::TooBig`] (V8 `kMaxLengthBits`).
//! - Two's-complement bitwise semantics map onto magnitude kernels through
//!   `-x == !(x - 1)` for negative operands, as in V8 `bitwise.cc`.
//!
//! # Spec references
//! - ECMA-262 §6.1.6.2 — BigInt operations.

use std::cmp::Ordering;

use otter_gc::GcHeap;

use super::BigIntValue;
use super::digits::{self, Scratch};
use super::gc_body::{BigIntBody, alloc_digits, body_ptr, no_roots};
use otter_gc::heap::RootSlotVisitor;

/// The largest BigInt, in bits.
pub const MAX_BITS: u64 = 1 << 30;
const MAX_DIGITS: usize = (MAX_BITS / 64) as usize;

/// Failure modes for BigInt operations.
#[derive(Debug, Clone, thiserror::Error)]
#[non_exhaustive]
pub enum OpError {
    /// Divide / remainder by zero. Spec: `RangeError`.
    #[error("Division by zero")]
    DivisionByZero,
    /// Negative exponent on `**`. Spec: `RangeError`.
    #[error("Exponent must be non-negative")]
    NegativeExponent,
    /// The result exceeds [`MAX_BITS`]. `RangeError`.
    #[error("Maximum BigInt size exceeded")]
    TooBig,
    /// The result body could not be allocated.
    #[error(transparent)]
    OutOfMemory(#[from] otter_gc::OutOfMemory),
}

/// Signature of every BigInt binary operator. The visitor publishes the
/// caller's other live values to a collection the result allocation
/// triggers.
pub type Binary = fn(
    &mut GcHeap,
    BigIntValue,
    BigIntValue,
    &mut RootSlotVisitor<'_>,
) -> Result<BigIntValue, OpError>;

/// A binary operator compiled code names by number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Operator {
    /// `+`.
    Add,
    /// `-`.
    Sub,
    /// `*`.
    Mul,
    /// `/`.
    Div,
    /// `%`.
    Rem,
    /// `&`.
    BitwiseAnd,
    /// `|`.
    BitwiseOr,
    /// `^`.
    BitwiseXor,
    /// `<<`.
    Shl,
    /// `>>`.
    Shr,
}

impl Operator {
    const ALL: [Self; 10] = [
        Self::Add,
        Self::Sub,
        Self::Mul,
        Self::Div,
        Self::Rem,
        Self::BitwiseAnd,
        Self::BitwiseOr,
        Self::BitwiseXor,
        Self::Shl,
        Self::Shr,
    ];

    /// The operator a compiled call passes as `code`.
    #[must_use]
    pub fn decode(code: i32) -> Option<Self> {
        usize::try_from(code)
            .ok()
            .and_then(|index| Self::ALL.get(index).copied())
    }

    /// The operator's function.
    #[must_use]
    pub const fn function(self) -> Binary {
        match self {
            Self::Add => add,
            Self::Sub => sub,
            Self::Mul => mul,
            Self::Div => div,
            Self::Rem => rem,
            Self::BitwiseAnd => bitwise_and,
            Self::BitwiseOr => bitwise_or,
            Self::BitwiseXor => bitwise_xor,
            Self::Shl => shl,
            Self::Shr => shr,
        }
    }
}

fn sign_len(heap: &GcHeap, value: BigIntValue) -> (bool, usize) {
    value.read(heap, |negative, magnitude| (negative, magnitude.len()))
}

/// Allocate `capacity` result digits and fill them from both operands;
/// `fill` returns the result's sign.
fn binary_into(
    heap: &mut GcHeap,
    a: BigIntValue,
    b: BigIntValue,
    capacity: usize,
    roots: &mut RootSlotVisitor<'_>,
    fill: impl FnOnce(&BigIntBody, &BigIntBody, &mut [u64]) -> bool,
) -> Result<BigIntValue, OpError> {
    if capacity > MAX_DIGITS {
        return Err(OpError::TooBig);
    }
    let mut handles = [a.handle(), b.handle()];
    let out = alloc_digits(heap, capacity, &mut handles, roots)?;
    // SAFETY: no allocation until `publish`; the operands are live bodies
    // the allocation rooted and rewrote, and the result is a fresh cell.
    let negative = unsafe {
        fill(
            &*body_ptr(handles[0]),
            &*body_ptr(handles[1]),
            out.digits_mut(),
        )
    };
    Ok(BigIntValue::from_handle(out.publish(negative)))
}

/// [`binary_into`] for one operand.
fn unary_into(
    heap: &mut GcHeap,
    a: BigIntValue,
    capacity: usize,
    roots: &mut RootSlotVisitor<'_>,
    fill: impl FnOnce(&BigIntBody, &mut [u64]) -> bool,
) -> Result<BigIntValue, OpError> {
    binary_into(heap, a, a, capacity, roots, |a, _, out| fill(a, out))
}

/// `lhs + rhs`.
pub fn add(
    heap: &mut GcHeap,
    lhs: BigIntValue,
    rhs: BigIntValue,
    roots: &mut RootSlotVisitor<'_>,
) -> Result<BigIntValue, OpError> {
    add_signed(heap, lhs, rhs, false, roots)
}

/// `lhs - rhs`.
pub fn sub(
    heap: &mut GcHeap,
    lhs: BigIntValue,
    rhs: BigIntValue,
    roots: &mut RootSlotVisitor<'_>,
) -> Result<BigIntValue, OpError> {
    add_signed(heap, lhs, rhs, true, roots)
}

fn add_signed(
    heap: &mut GcHeap,
    lhs: BigIntValue,
    rhs: BigIntValue,
    negate_rhs: bool,
    roots: &mut RootSlotVisitor<'_>,
) -> Result<BigIntValue, OpError> {
    let capacity = sign_len(heap, lhs).1.max(sign_len(heap, rhs).1) + 1;
    binary_into(heap, lhs, rhs, capacity, roots, |a, b, out| {
        let (x, y) = (a.digits(), b.digits());
        let b_negative = b.is_negative() != negate_rhs;
        if a.is_negative() == b_negative {
            digits::add(x, y, out);
            a.is_negative()
        } else if digits::compare(x, y).is_ge() {
            digits::sub(x, y, out);
            a.is_negative()
        } else {
            digits::sub(y, x, out);
            b_negative
        }
    })
}

/// `value + 1` (`up`) or `value - 1`: `++` / `--`.
pub fn step(heap: &mut GcHeap, value: BigIntValue, up: bool) -> Result<BigIntValue, OpError> {
    let capacity = sign_len(heap, value).1.max(1) + 1;
    unary_into(heap, value, capacity, &mut no_roots, |a, out| {
        let x = a.digits();
        // Moving away from zero grows the magnitude; toward it shrinks it.
        if a.is_negative() != up || x.is_empty() {
            digits::add(x, &[1], out);
            !up
        } else {
            digits::sub(x, &[1], out);
            a.is_negative()
        }
    })
}

/// `lhs * rhs`.
pub fn mul(
    heap: &mut GcHeap,
    lhs: BigIntValue,
    rhs: BigIntValue,
    roots: &mut RootSlotVisitor<'_>,
) -> Result<BigIntValue, OpError> {
    let (a_len, b_len) = (sign_len(heap, lhs).1, sign_len(heap, rhs).1);
    if a_len == 0 {
        return Ok(lhs);
    }
    if b_len == 0 {
        return Ok(rhs);
    }
    binary_into(heap, lhs, rhs, a_len + b_len, roots, |a, b, out| {
        digits::mul(a.digits(), b.digits(), out);
        a.is_negative() != b.is_negative()
    })
}

/// Magnitude ordering of the two operands, or `DivisionByZero`.
fn divisor_order(heap: &GcHeap, lhs: BigIntValue, rhs: BigIntValue) -> Result<Ordering, OpError> {
    lhs.read(heap, |_, x| {
        rhs.read(heap, |_, y| {
            if y.is_empty() {
                Err(OpError::DivisionByZero)
            } else {
                Ok(digits::compare(x, y))
            }
        })
    })
}

/// `lhs / rhs`, truncated toward zero.
pub fn div(
    heap: &mut GcHeap,
    lhs: BigIntValue,
    rhs: BigIntValue,
    roots: &mut RootSlotVisitor<'_>,
) -> Result<BigIntValue, OpError> {
    if divisor_order(heap, lhs, rhs)?.is_lt() {
        return Ok(BigIntValue::from_handle(
            alloc_digits(heap, 0, &mut [], roots)?.publish(false),
        ));
    }
    let capacity = sign_len(heap, lhs).1 - sign_len(heap, rhs).1 + 1;
    binary_into(heap, lhs, rhs, capacity, roots, |a, b, out| {
        digits::div_rem(a.digits(), b.digits(), Some(out), None);
        a.is_negative() != b.is_negative()
    })
}

/// `lhs % rhs`; the sign follows the dividend.
pub fn rem(
    heap: &mut GcHeap,
    lhs: BigIntValue,
    rhs: BigIntValue,
    roots: &mut RootSlotVisitor<'_>,
) -> Result<BigIntValue, OpError> {
    if divisor_order(heap, lhs, rhs)?.is_lt() {
        return Ok(lhs);
    }
    let capacity = sign_len(heap, rhs).1;
    binary_into(heap, lhs, rhs, capacity, roots, |a, b, out| {
        digits::div_rem(a.digits(), b.digits(), None, Some(out));
        a.is_negative()
    })
}

/// `base ** exponent`.
pub fn pow(
    heap: &mut GcHeap,
    base: BigIntValue,
    exponent: BigIntValue,
    roots: &mut RootSlotVisitor<'_>,
) -> Result<BigIntValue, OpError> {
    let (negative, exponent) = exponent.read(heap, |negative, d| {
        (
            negative,
            match *d {
                [] => 0,
                [e] => e,
                _ => u64::MAX,
            },
        )
    });
    if negative {
        return Err(OpError::NegativeExponent);
    }
    let (base_negative, base_bits, unit) =
        base.read(heap, |negative, d| (negative, bit_length(d), d == [1]));
    if exponent == 0 {
        return Ok(BigIntValue::from_magnitude_with_roots(
            heap,
            false,
            &[1],
            roots,
        )?);
    }
    if base_bits == 0 {
        return Ok(base);
    }
    if unit {
        let odd = exponent % 2 == 1;
        return Ok(BigIntValue::from_magnitude_with_roots(
            heap,
            base_negative && odd,
            &[1],
            roots,
        )?);
    }
    if exponent > MAX_BITS || (base_bits - 1).saturating_mul(exponent) > MAX_BITS {
        return Err(OpError::TooBig);
    }
    let result = base.to_num(heap).pow(exponent as u32);
    let magnitude: Scratch = result.iter_u64_digits().collect();
    let negative = result.sign() == num_bigint::Sign::Minus;
    Ok(BigIntValue::from_magnitude_with_roots(
        heap, negative, &magnitude, roots,
    )?)
}

/// Unary `-`.
pub fn neg(heap: &mut GcHeap, value: BigIntValue) -> Result<BigIntValue, OpError> {
    let len = sign_len(heap, value).1;
    if len == 0 {
        return Ok(value);
    }
    unary_into(heap, value, len, &mut no_roots, |a, out| {
        out.copy_from_slice(a.digits());
        !a.is_negative()
    })
}

/// `~value`, which is `-value - 1`.
pub fn bitwise_not(heap: &mut GcHeap, value: BigIntValue) -> Result<BigIntValue, OpError> {
    let capacity = sign_len(heap, value).1.max(1) + 1;
    unary_into(heap, value, capacity, &mut no_roots, |a, out| {
        if a.is_negative() {
            digits::sub(a.digits(), &[1], out);
            false
        } else {
            digits::add(a.digits(), &[1], out);
            true
        }
    })
}

/// `|x| - 1` for a non-zero magnitude.
fn minus_one(x: &[u64]) -> Scratch {
    let mut out = Scratch::from_elem(0, x.len());
    digits::sub(x, &[1], &mut out);
    out
}

/// `out += 1` in place; the capacity holds the carry.
fn increment(out: &mut [u64]) {
    for digit in out {
        let (sum, carry) = digit.overflowing_add(1);
        *digit = sum;
        if !carry {
            return;
        }
    }
}

/// `lhs & rhs` in two's complement.
pub fn bitwise_and(
    heap: &mut GcHeap,
    lhs: BigIntValue,
    rhs: BigIntValue,
    roots: &mut RootSlotVisitor<'_>,
) -> Result<BigIntValue, OpError> {
    let ((a_neg, a_len), (b_neg, b_len)) = (sign_len(heap, lhs), sign_len(heap, rhs));
    let capacity = match (a_neg, b_neg) {
        (false, false) => a_len.min(b_len),
        (true, true) => a_len.max(b_len) + 1,
        (false, true) => a_len,
        (true, false) => b_len,
    };
    binary_into(heap, lhs, rhs, capacity, roots, |a, b, out| {
        let (x, y) = (a.digits(), b.digits());
        match (a.is_negative(), b.is_negative()) {
            (false, false) => {
                digits::and(x, y, out);
                false
            }
            // -(((|x|-1) | (|y|-1)) + 1)
            (true, true) => {
                digits::or(&minus_one(x), &minus_one(y), out);
                increment(out);
                true
            }
            // x & !(|y|-1)
            (false, true) => {
                digits::and_not(x, &minus_one(y), out);
                false
            }
            (true, false) => {
                digits::and_not(y, &minus_one(x), out);
                false
            }
        }
    })
}

/// `lhs | rhs` in two's complement.
pub fn bitwise_or(
    heap: &mut GcHeap,
    lhs: BigIntValue,
    rhs: BigIntValue,
    roots: &mut RootSlotVisitor<'_>,
) -> Result<BigIntValue, OpError> {
    let ((a_neg, a_len), (b_neg, b_len)) = (sign_len(heap, lhs), sign_len(heap, rhs));
    let capacity = match (a_neg, b_neg) {
        (false, false) => a_len.max(b_len),
        (true, true) => a_len.min(b_len) + 1,
        (false, true) => b_len + 1,
        (true, false) => a_len + 1,
    };
    binary_into(heap, lhs, rhs, capacity, roots, |a, b, out| {
        let (x, y) = (a.digits(), b.digits());
        match (a.is_negative(), b.is_negative()) {
            (false, false) => {
                digits::or(x, y, out);
                false
            }
            // -(((|x|-1) & (|y|-1)) + 1)
            (true, true) => {
                digits::and(&minus_one(x), &minus_one(y), out);
                increment(out);
                true
            }
            // -(((|y|-1) & !x) + 1)
            (false, true) => {
                digits::and_not(&minus_one(y), x, out);
                increment(out);
                true
            }
            (true, false) => {
                digits::and_not(&minus_one(x), y, out);
                increment(out);
                true
            }
        }
    })
}

/// `lhs ^ rhs` in two's complement.
pub fn bitwise_xor(
    heap: &mut GcHeap,
    lhs: BigIntValue,
    rhs: BigIntValue,
    roots: &mut RootSlotVisitor<'_>,
) -> Result<BigIntValue, OpError> {
    let ((a_neg, a_len), (b_neg, b_len)) = (sign_len(heap, lhs), sign_len(heap, rhs));
    let capacity = a_len.max(b_len) + usize::from(a_neg != b_neg);
    binary_into(heap, lhs, rhs, capacity, roots, |a, b, out| {
        let (x, y) = (a.digits(), b.digits());
        match (a.is_negative(), b.is_negative()) {
            (false, false) => {
                digits::xor(x, y, out);
                false
            }
            // (|x|-1) ^ (|y|-1)
            (true, true) => {
                digits::xor(&minus_one(x), &minus_one(y), out);
                false
            }
            // -((x ^ (|y|-1)) + 1)
            (false, true) => {
                digits::xor(x, &minus_one(y), out);
                increment(out);
                true
            }
            (true, false) => {
                digits::xor(&minus_one(x), y, out);
                increment(out);
                true
            }
        }
    })
}

/// A shift count: its direction and size, `None` past every possible
/// result width.
fn shift_count(heap: &GcHeap, count: BigIntValue) -> (bool, Option<usize>) {
    count.read(heap, |negative, d| match *d {
        [] => (negative, Some(0)),
        [n] if n <= MAX_BITS => (negative, Some(n as usize)),
        _ => (negative, None),
    })
}

/// `lhs << rhs`; a negative count shifts right.
pub fn shl(
    heap: &mut GcHeap,
    lhs: BigIntValue,
    rhs: BigIntValue,
    roots: &mut RootSlotVisitor<'_>,
) -> Result<BigIntValue, OpError> {
    let (right, count) = shift_count(heap, rhs);
    shift(heap, lhs, right, count, roots)
}

/// `lhs >> rhs`, rounding toward negative infinity; a negative count shifts
/// left. There is no BigInt `>>>`: the dispatcher rejects it.
pub fn shr(
    heap: &mut GcHeap,
    lhs: BigIntValue,
    rhs: BigIntValue,
    roots: &mut RootSlotVisitor<'_>,
) -> Result<BigIntValue, OpError> {
    let (left, count) = shift_count(heap, rhs);
    shift(heap, lhs, !left, count, roots)
}

fn shift(
    heap: &mut GcHeap,
    value: BigIntValue,
    right: bool,
    count: Option<usize>,
    roots: &mut RootSlotVisitor<'_>,
) -> Result<BigIntValue, OpError> {
    let (negative, len) = sign_len(heap, value);
    if len == 0 || count == Some(0) {
        return Ok(value);
    }
    if !right {
        let count = count.ok_or(OpError::TooBig)?;
        return unary_into(heap, value, len + count / 64 + 1, roots, |a, out| {
            digits::shl(a.digits(), count, out);
            a.is_negative()
        });
    }
    let digit_shift = count.map_or(usize::MAX, |count| count / 64);
    if digit_shift >= len {
        let magnitude: &[u64] = if negative { &[1] } else { &[] };
        return Ok(BigIntValue::from_magnitude_with_roots(
            heap, negative, magnitude, roots,
        )?);
    }
    let count = count.expect("an in-range digit shift has a count");
    unary_into(heap, value, len - digit_shift + 1, roots, |a, out| {
        let lost = digits::shr(a.digits(), count, out);
        // Flooring: a negative value that lost bits moves one further down.
        if a.is_negative() && lost {
            increment(out);
        }
        a.is_negative()
    })
}

fn bit_length(magnitude: &[u64]) -> u64 {
    magnitude.last().map_or(0, |top| {
        (magnitude.len() as u64 - 1) * 64 + u64::from(64 - top.leading_zeros())
    })
}

/// Keep the low `bits` of `out`, which is exactly `bits.div_ceil(64)` long.
fn mask_top(out: &mut [u64], bits: u64) {
    if bits % 64 != 0
        && let Some(top) = out.last_mut()
    {
        *top &= (1u64 << (bits % 64)) - 1;
    }
}

/// `out = 2^bits - out` modulo `2^bits`.
fn negate_modulo(out: &mut [u64], bits: u64) {
    for digit in out.iter_mut() {
        *digit = !*digit;
    }
    increment(out);
    mask_top(out, bits);
}

/// §21.2.2.2 BigInt.asUintN: `value` modulo `2^bits`.
pub fn as_uint_n(heap: &mut GcHeap, bits: u64, value: BigIntValue) -> Result<BigIntValue, OpError> {
    let (negative, len, bit_len) = value.read(heap, |n, d| (n, d.len(), bit_length(d)));
    if len == 0 || (!negative && bit_len <= bits) {
        return Ok(value);
    }
    if bits > MAX_BITS {
        return Err(OpError::TooBig);
    }
    unary_into(
        heap,
        value,
        bits.div_ceil(64) as usize,
        &mut no_roots,
        |a, out| {
            truncate(a.digits(), out, bits);
            if a.is_negative() {
                negate_modulo(out, bits);
            }
            false
        },
    )
}

/// §21.2.2.1 BigInt.asIntN: `value` modulo `2^bits` as a signed `bits`-wide
/// integer.
pub fn as_int_n(heap: &mut GcHeap, bits: u64, value: BigIntValue) -> Result<BigIntValue, OpError> {
    let (len, bit_len) = value.read(heap, |_, d| (d.len(), bit_length(d)));
    if bits == 0 {
        return Ok(BigIntValue::from_i32(heap, 0)?);
    }
    if len == 0 || bit_len < bits {
        return Ok(value);
    }
    unary_into(
        heap,
        value,
        bits.div_ceil(64) as usize,
        &mut no_roots,
        |a, out| {
            truncate(a.digits(), out, bits);
            if a.is_negative() {
                negate_modulo(out, bits);
            }
            let top = bits - 1;
            let sign_bit = (out[(top / 64) as usize] >> (top % 64)) & 1 == 1;
            if sign_bit {
                negate_modulo(out, bits);
            }
            sign_bit
        },
    )
}

/// `out = x mod 2^bits` for `out.len() == bits.div_ceil(64)`.
fn truncate(x: &[u64], out: &mut [u64], bits: u64) {
    for (i, slot) in out.iter_mut().enumerate() {
        *slot = x.get(i).copied().unwrap_or(0);
    }
    mask_top(out, bits);
}

/// Compare a BigInt with a Number (§6.1.6.2.12 / §7.2.13): `None` for NaN.
#[must_use]
pub fn compare_to_f64(heap: &GcHeap, lhs: BigIntValue, rhs: f64) -> Option<Ordering> {
    if rhs.is_nan() {
        return None;
    }
    if rhs.is_infinite() {
        return Some(if rhs > 0.0 {
            Ordering::Less
        } else {
            Ordering::Greater
        });
    }
    let truncated = rhs.trunc();
    let magnitude = digits::from_f64_integral(truncated.abs());
    let rhs_negative = truncated < 0.0;
    let order = lhs.read(heap, |negative, x| match (negative, rhs_negative) {
        (false, true) => Ordering::Greater,
        (true, false) => Ordering::Less,
        (false, false) => digits::compare(x, &magnitude),
        (true, true) => digits::compare(&magnitude, x),
    });
    Some(match order {
        // Equal integer parts: the Number's fraction decides.
        Ordering::Equal => truncated.partial_cmp(&rhs).unwrap_or(Ordering::Equal),
        other => other,
    })
}

/// Equality with a Number (§7.2.13): only an integral Number of the same
/// value is equal.
#[must_use]
pub fn equals_f64(heap: &GcHeap, lhs: BigIntValue, rhs: f64) -> bool {
    compare_to_f64(heap, lhs, rhs) == Some(Ordering::Equal)
}

/// §21.2.1.1.1 NumberToBigInt for an integral, finite double.
///
/// # Errors
/// Surfaces [`otter_gc::OutOfMemory`].
pub fn from_f64_integral(
    heap: &mut GcHeap,
    value: f64,
) -> Result<BigIntValue, otter_gc::OutOfMemory> {
    BigIntValue::from_magnitude(heap, value < 0.0, &digits::from_f64_integral(value.abs()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use num_bigint::BigInt;

    fn heap() -> GcHeap {
        GcHeap::new().expect("heap")
    }

    fn big(heap: &mut GcHeap, text: &str) -> BigIntValue {
        BigIntValue::from_decimal(heap, text).unwrap().unwrap()
    }

    const SAMPLES: [&str; 9] = [
        "0",
        "1",
        "-1",
        "18446744073709551615",
        "-18446744073709551616",
        "123456789012345678901234567890123456789",
        "-98765432109876543210987654321",
        "340282366920938463463374607431768211455",
        "-7",
    ];

    #[test]
    fn binary_operators_match_reference() {
        let mut heap = heap();
        let cases: [(Binary, fn(&BigInt, &BigInt) -> Option<BigInt>); 8] = [
            (add, |a, b| Some(a + b)),
            (sub, |a, b| Some(a - b)),
            (mul, |a, b| Some(a * b)),
            (div, |a, b| (b != &BigInt::from(0)).then(|| a / b)),
            (rem, |a, b| (b != &BigInt::from(0)).then(|| a % b)),
            (bitwise_and, |a, b| Some(a & b)),
            (bitwise_or, |a, b| Some(a | b)),
            (bitwise_xor, |a, b| Some(a ^ b)),
        ];
        for (op, reference) in cases {
            for x in SAMPLES {
                for y in SAMPLES {
                    let (a, b) = (big(&mut heap, x), big(&mut heap, y));
                    let expected = reference(&x.parse().unwrap(), &y.parse().unwrap());
                    let got = op(&mut heap, a, b, &mut no_roots)
                        .ok()
                        .map(|v| v.to_num(&heap));
                    assert_eq!(got, expected, "{x} op {y}");
                }
            }
        }
    }

    #[test]
    fn unary_and_shift_operators_match_reference() {
        let mut heap = heap();
        for x in SAMPLES {
            let a = big(&mut heap, x);
            let r: BigInt = x.parse().unwrap();
            assert_eq!(neg(&mut heap, a).unwrap().to_num(&heap), -&r);
            assert_eq!(bitwise_not(&mut heap, a).unwrap().to_num(&heap), !&r);
            assert_eq!(step(&mut heap, a, true).unwrap().to_num(&heap), &r + 1);
            assert_eq!(step(&mut heap, a, false).unwrap().to_num(&heap), &r - 1);
            for count in [0i64, 1, 63, 64, 65, 130, -1, -64, -200] {
                let c = BigIntValue::from_i64(&mut heap, count).unwrap();
                let left = shl(&mut heap, a, c, &mut no_roots).unwrap().to_num(&heap);
                let right = shr(&mut heap, a, c, &mut no_roots).unwrap().to_num(&heap);
                let (l, rr) = if count >= 0 {
                    (&r << count as u32, &r >> count as u32)
                } else {
                    (&r >> (-count) as u32, &r << (-count) as u32)
                };
                assert_eq!((left, right), (l, rr), "{x} shift {count}");
            }
            for bits in [0u64, 1, 7, 64, 65, 128, 200] {
                let modulus = BigInt::from(1) << bits;
                let mut unsigned = &r % &modulus;
                if unsigned < BigInt::from(0) {
                    unsigned += &modulus;
                }
                let signed = if bits > 0 && unsigned >= (BigInt::from(1) << (bits - 1)) {
                    &unsigned - &modulus
                } else {
                    unsigned.clone()
                };
                assert_eq!(
                    as_uint_n(&mut heap, bits, a).unwrap().to_num(&heap),
                    unsigned
                );
                assert_eq!(
                    as_int_n(&mut heap, bits, a).unwrap().to_num(&heap),
                    signed,
                    "asIntN({bits}, {x})"
                );
            }
        }
    }

    #[test]
    fn division_by_zero_and_size_limits() {
        let mut heap = heap();
        let (one, zero) = (big(&mut heap, "1"), big(&mut heap, "0"));
        assert!(matches!(
            div(&mut heap, one, zero, &mut no_roots),
            Err(OpError::DivisionByZero)
        ));
        let huge = big(&mut heap, "1073741825");
        assert!(matches!(
            shl(&mut heap, one, huge, &mut no_roots),
            Err(OpError::TooBig)
        ));
        let two = big(&mut heap, "2");
        assert!(matches!(
            pow(&mut heap, two, huge, &mut no_roots),
            Err(OpError::TooBig)
        ));
        let minus = big(&mut heap, "-1");
        assert!(matches!(
            pow(&mut heap, two, minus, &mut no_roots),
            Err(OpError::NegativeExponent)
        ));
        let hundred = big(&mut heap, "100");
        assert_eq!(
            pow(&mut heap, two, hundred, &mut no_roots)
                .unwrap()
                .to_num(&heap),
            BigInt::from(1) << 100
        );
    }

    #[test]
    fn compare_to_f64_respects_fractional_tie_breaker() {
        let mut heap = heap();
        let two = big(&mut heap, "2");
        assert_eq!(compare_to_f64(&heap, two, 2.5), Some(Ordering::Less));
        assert_eq!(compare_to_f64(&heap, two, 1.5), Some(Ordering::Greater));
        assert_eq!(compare_to_f64(&heap, two, 2.0), Some(Ordering::Equal));
        assert_eq!(compare_to_f64(&heap, two, f64::NAN), None);
        let zero = big(&mut heap, "0");
        assert_eq!(compare_to_f64(&heap, zero, -0.5), Some(Ordering::Greater));
        assert!(equals_f64(&heap, two, 2.0) && !equals_f64(&heap, two, 2.5));
        assert_eq!(compare_to_f64(&heap, two, f64::MAX), Some(Ordering::Less));
    }
}
