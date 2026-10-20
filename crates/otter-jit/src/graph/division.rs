//! Signed int32 division by a constant as shifts or a multiply-high.
//!
//! A hardware divide sits on the critical path of a loop that takes a
//! remainder or quotient by a fixed number; a multiply-high, a shift and an
//! add give the same truncated quotient for every int32 dividend, and the
//! remainder follows as `dividend - quotient * divisor`.
//!
//! # Contents
//! - [`Quotient`] / [`quotient_by`] — how a divisor's truncated quotient is
//!   formed: shifts for a power of two, otherwise the magic number of its
//!   magnitude (Hacker's Delight, chapter 10), negated for a negative divisor.
//!
//! # Invariants
//! - The divisor is neither `0`, `1` nor `-1`.
//! - [`Quotient::Shift`] of `k`: `t = k > 1 ? n >> 31 : n`;
//!   `q = (n + (t >>> (32 - k))) >> k`.
//! - [`Quotient::Magic`]: `q = mulhi(n, multiplier)`, plus `n` when `add`;
//!   `q = (q >> shift) + (n >>> 31)`.
//! - Either is negated when the divisor is negative; the result equals
//!   `n / divisor` truncated toward zero for every int32 `n`.
//!
//! # See also
//! - `super::arm64` — `Int32Div` / `Int32Mod` by a constant.

/// How the truncated quotient by one constant magnitude is formed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Quotient {
    /// The magnitude is `2^k`.
    Shift(u32),
    /// Multiply-high by `multiplier`, add the dividend when `add`, then
    /// shift right by `shift`.
    Magic {
        multiplier: i32,
        add: bool,
        shift: u32,
    },
}

/// The quotient form of `divisor`'s magnitude, and whether to negate it.
///
/// # Panics
/// When `divisor` is `0`, `1` or `-1`.
pub(crate) fn quotient_by(divisor: i32) -> (Quotient, bool) {
    assert!(
        !matches!(divisor, -1..=1),
        "no quotient form divides by {divisor}"
    );
    let magnitude = divisor.unsigned_abs();
    let form = if magnitude.is_power_of_two() {
        Quotient::Shift(magnitude.trailing_zeros())
    } else {
        magic(magnitude)
    };
    (form, divisor < 0)
}

/// The magic number of a positive divisor that is not a power of two.
fn magic(d: u32) -> Quotient {
    let min = 1u32 << 31;
    let anc = min - 1 - (min % d);
    let mut p = 31u32;
    let mut q1 = min / anc;
    let mut r1 = min - q1 * anc;
    let mut q2 = min / d;
    let mut r2 = min - q2 * d;
    loop {
        p += 1;
        q1 = q1.wrapping_mul(2);
        r1 = r1.wrapping_mul(2);
        if r1 >= anc {
            q1 = q1.wrapping_add(1);
            r1 = r1.wrapping_sub(anc);
        }
        q2 = q2.wrapping_mul(2);
        r2 = r2.wrapping_mul(2);
        if r2 >= d {
            q2 = q2.wrapping_add(1);
            r2 = r2.wrapping_sub(d);
        }
        let delta = d.wrapping_sub(r2);
        if !(q1 < delta || (q1 == delta && r1 == 0)) {
            break;
        }
    }
    let multiplier = q2.wrapping_add(1) as i32;
    Quotient::Magic {
        multiplier,
        add: multiplier < 0,
        shift: p - 32,
    }
}

#[cfg(test)]
mod tests {
    use super::{Quotient, quotient_by};

    /// `dividend / divisor` truncated, as the generated sequence computes it.
    fn divide(dividend: i32, divisor: i32) -> i32 {
        let (form, negate) = quotient_by(divisor);
        let q = match form {
            Quotient::Shift(k) => {
                let t = if k > 1 { dividend >> 31 } else { dividend };
                dividend.wrapping_add(((t as u32) >> (32 - k)) as i32) >> k
            }
            Quotient::Magic {
                multiplier,
                add,
                shift,
            } => {
                let mut q = ((i64::from(dividend) * i64::from(multiplier)) >> 32) as i32;
                if add {
                    q = q.wrapping_add(dividend);
                }
                (q >> shift).wrapping_add(((dividend as u32) >> 31) as i32)
            }
        };
        if negate { q.wrapping_neg() } else { q }
    }

    fn dividends() -> impl Iterator<Item = i32> {
        let edges = [
            0,
            1,
            -1,
            2,
            -2,
            7,
            -7,
            1000,
            -1000,
            65_535,
            65_536,
            -65_536,
            i32::MAX,
            i32::MAX - 1,
            i32::MIN,
            i32::MIN + 1,
        ];
        let stride = (0..200_000i64).map(|k| (k * 21_475 - 2_147_483_648 + k) as i32);
        edges.into_iter().chain(stride)
    }

    #[test]
    fn the_formed_quotient_is_the_truncated_quotient_for_every_dividend() {
        let divisors = [
            2,
            -2,
            3,
            -3,
            4,
            -4,
            5,
            6,
            7,
            -7,
            10,
            16,
            -16,
            25,
            100,
            641,
            1000,
            -1000,
            65_537,
            0x3fff_ffff,
            i32::MAX,
            -i32::MAX,
            i32::MIN,
            1 << 30,
            -(1 << 30),
        ];
        for divisor in divisors {
            for dividend in dividends() {
                assert_eq!(
                    divide(dividend, divisor),
                    dividend.wrapping_div(divisor),
                    "{dividend} / {divisor} with {:?}",
                    quotient_by(divisor)
                );
            }
        }
    }

    #[test]
    fn magic_numbers_match_the_published_table() {
        // Hacker's Delight, table 10-1.
        assert_eq!(
            quotient_by(3).0,
            Quotient::Magic {
                multiplier: 0x5555_5556,
                add: false,
                shift: 0
            }
        );
        assert_eq!(
            quotient_by(7).0,
            Quotient::Magic {
                multiplier: 0x9249_2493_u32 as i32,
                add: true,
                shift: 2
            }
        );
        assert_eq!(quotient_by(-8), (Quotient::Shift(3), true));
    }
}
