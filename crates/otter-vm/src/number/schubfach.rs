//! Schubfach radix-10 shortest-decimal core for `f64`.
//!
//! Implements the algorithm of Giulietti (2018/2020),
//! *The Schubfach way to render doubles*. Given a finite, non-zero
//! `f64` value `v`, produces the unique pair `(significand, exponent)`
//! such that:
//!
//! - `significand · 10^exponent` round-trips exactly to `v` under
//!   IEEE 754 round-half-to-even,
//! - `significand` has the **fewest** decimal digits among all such
//!   pairs (and ties on length break to the even significand).
//!
//! The 126-bit power-of-ten multipliers are read from
//! [`super::pow10_table::G_TABLE`], generated offline by
//! `crates/otter-vm-codegen`.
//!
//! # Contents
//! - [`Decimal`] — output value type.
//! - [`schubfach_finite`] — public entry, asserts finite and non-zero.
//!
//! # Invariants
//! - Caller filters `NaN`, `±Infinity`, and `±0.0` before calling.
//! - The returned `significand` is in `[1, 10^17)`. `exponent` fits
//!   in `i32` (bounded by the IEEE 754 binary64 dynamic range).
//! - All arithmetic on the hot path is integer; no `f64` operations
//!   beyond the initial bit extraction.
//!
//! # ECMA-262
//! - <https://tc39.es/ecma262/#sec-numeric-types-number-tostring>

use super::pow10_table::{G_TABLE, K_MIN};

const Q_MIN: i32 = -1074;
const C_MIN: u64 = 1u64 << 52;
const T_MASK: u64 = (1u64 << 52) - 1;
const BQ_MASK: u64 = (1u64 << 11) - 1;

const C_10: i64 = 661_971_961_083;
const Q_10: u32 = 41;
const A_10: i64 = -274_743_187_321;
const C_2: i64 = 913_124_641_741;
const Q_2: u32 = 38;

const MASK_63: u64 = (1u64 << 63) - 1;

/// Magic constant for the `s ≥ 100 → s/10` trick. Equals
/// `⌈2^59 / 10⌉` shifted up; see Schubfach §6.
const MAGIC_DIV_10_BASE: u64 = 115_292_150_460_684_698;

/// Output of the Schubfach core.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Decimal {
    /// Shortest decimal significand (no trailing zeros except for
    /// the single trailing-zero case the algorithm explicitly
    /// resolves). `1 ≤ significand < 10^17`.
    pub significand: u64,
    /// Decimal exponent. The represented value is
    /// `significand · 10^exponent`.
    pub exponent: i32,
}

#[inline]
const fn flog10_pow2(e: i32) -> i32 {
    ((e as i64 * C_10) >> Q_10) as i32
}

#[inline]
const fn flog10_three_quarters_pow2(e: i32) -> i32 {
    ((e as i64 * C_10 + A_10) >> Q_10) as i32
}

#[inline]
const fn flog2_pow10(e: i32) -> i32 {
    ((e as i64 * C_2) >> Q_2) as i32
}

/// `(a * b) >> 64` for unsigned 64-bit operands.
#[inline]
const fn mul_high(a: u64, b: u64) -> u64 {
    ((a as u128 * b as u128) >> 64) as u64
}

/// Schubfach's `rop` — top 64 bits of `(g · cp) >> 65` with a
/// stickiness fixup so that the lower bits round up correctly.
/// `g = g1 · 2^63 + g0` with `g1, g0 < 2^63`.
#[inline]
fn rop(g1: u64, g0: u64, cp: u64) -> u64 {
    let x1 = mul_high(g0, cp);
    let y0 = g1.wrapping_mul(cp);
    let y1 = mul_high(g1, cp);
    let z = (y0 >> 1).wrapping_add(x1);
    let vbp = y1.wrapping_add(z >> 63);
    let extra = ((z & MASK_63).wrapping_add(MASK_63)) >> 63;
    vbp | extra
}

/// Compute the shortest decimal representation of a finite, non-zero
/// `f64`. The sign of `value` is ignored; the caller emits `'-'`
/// before calling for negative inputs.
pub fn schubfach_finite(value: f64) -> Decimal {
    debug_assert!(value.is_finite() && value != 0.0);
    let bits = value.to_bits() & 0x7FFF_FFFF_FFFF_FFFF;
    let bq = ((bits >> 52) & BQ_MASK) as i32;
    let t = bits & T_MASK;
    if bq != 0 {
        decimal_from_q_c(bq - 1075, C_MIN | t, 0)
    } else {
        decimal_from_q_c(Q_MIN, 10 * t, -1)
    }
}

fn decimal_from_q_c(q: i32, c: u64, dk: i32) -> Decimal {
    let out: u64 = c & 1;
    let cb: u64 = c << 2;
    let cbr: u64 = cb + 2;
    let (cbl, k) = if c != C_MIN || q == Q_MIN {
        (cb - 2, flog10_pow2(q))
    } else {
        (cb - 1, flog10_three_quarters_pow2(q))
    };
    let h_signed = q + flog2_pow10(-k) + 2;
    debug_assert!((0..=6).contains(&h_signed), "h out of range: {h_signed}");
    let h = h_signed as u32;

    let entry = G_TABLE[(k - K_MIN) as usize];
    let g_high = entry.0;
    let g_low = entry.1;
    // Repack the (high u64, low u64) 128-bit `g` as (g1, g0) with
    // g1 = top 63 bits, g0 = bottom 63 bits, per Schubfach §9.
    let g1 = (g_high << 1) | (g_low >> 63);
    let g0 = g_low & MASK_63;

    let cb_h = cb << h;
    let cbl_h = cbl << h;
    let cbr_h = cbr << h;

    let vb = rop(g1, g0, cb_h);
    let vbl = rop(g1, g0, cbl_h);
    let vbr = rop(g1, g0, cbr_h);

    let s = vb >> 2;

    if s >= 100 {
        // Drop one trailing zero if doing so still round-trips.
        let sp10 = 10u64.wrapping_mul(mul_high(s, MAGIC_DIV_10_BASE << 4));
        let tp10 = sp10 + 10;
        let upin = vbl + out <= sp10 << 2;
        let wpin = (tp10 << 2) + out <= vbr;
        if upin != wpin {
            return Decimal {
                significand: if upin { sp10 } else { tp10 },
                exponent: k + dk,
            };
        }
    }

    let t_val = s + 1;
    let uin = vbl + out <= s << 2;
    let win = (t_val << 2) + out <= vbr;
    if uin != win {
        return Decimal {
            significand: if uin { s } else { t_val },
            exponent: k + dk,
        };
    }

    // Tie-break: prefer the candidate closer to `vb`; on equal
    // distance pick the even significand (round-half-to-even).
    let cmp = (vb as i64).wrapping_sub((s.wrapping_add(t_val) << 1) as i64);
    let chosen = if cmp < 0 || (cmp == 0 && (s & 1) == 0) {
        s
    } else {
        t_val
    };
    Decimal {
        significand: chosen,
        exponent: k + dk,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-trip: format the magnitude as `<significand>e<exponent>`,
    /// reapply the sign, parse via Rust stdlib, and assert bit
    /// equality. The Schubfach core returns a representation that
    /// is mathematically equal to the input (often with trailing
    /// zeros that the format stage strips); the round-trip property
    /// is what makes it correct.
    fn round_trip(x: f64) {
        let abs = x.abs();
        let dec = schubfach_finite(abs);
        let s = if x.is_sign_negative() {
            format!("-{}e{}", dec.significand, dec.exponent)
        } else {
            format!("{}e{}", dec.significand, dec.exponent)
        };
        let parsed: f64 = s.parse().unwrap_or_else(|_| panic!("parse fail: {s}"));
        assert_eq!(
            parsed.to_bits(),
            x.to_bits(),
            "round-trip mismatch for {x}: schubfach said {s}, parsed back to {parsed}"
        );
    }

    #[test]
    fn one_round_trips() {
        round_trip(1.0);
    }

    #[test]
    fn ten_round_trips() {
        round_trip(10.0);
    }

    #[test]
    fn point_one_round_trips() {
        round_trip(0.1);
    }

    #[test]
    fn point_one_plus_point_two_is_canonical() {
        // The classic floating-point gotcha: 0.1 + 0.2 has a
        // 17-digit shortest-round-trip representation whose canonical
        // form is well-known to be 30000000000000004e-17.
        let x = 0.1_f64 + 0.2;
        let dec = schubfach_finite(x);
        assert_eq!(
            dec,
            Decimal {
                significand: 30000000000000004,
                exponent: -17
            }
        );
        round_trip(x);
    }

    #[test]
    fn integers_round_trip() {
        for &x in &[1.0, 2.0, 5.0, 10.0, 100.0, 1234.0, 9007199254740991.0] {
            round_trip(x);
        }
    }

    #[test]
    #[allow(clippy::approx_constant)]
    fn fractions_round_trip() {
        for &x in &[0.5, 0.25, 0.1, 0.2, 0.3, 1.5, 3.14159, 2.718281828] {
            round_trip(x);
        }
    }

    #[test]
    fn small_round_trip() {
        for &x in &[1e-7, 1e-100, 5e-324, f64::MIN_POSITIVE] {
            round_trip(x);
        }
    }

    #[test]
    fn large_round_trip() {
        for &x in &[1e21, 1e100, 1e308, f64::MAX] {
            round_trip(x);
        }
    }

    #[test]
    fn negative_values_round_trip() {
        for &x in &[-1.0, -0.1, -1e21, -f64::MIN_POSITIVE, -f64::MAX] {
            round_trip(x);
            // Magnitude path returns the same Decimal regardless
            // of sign — confirm explicitly so that the wrapper can
            // safely strip the sign bit and call this fn directly.
            let pos_dec = schubfach_finite(x.abs());
            let neg_dec = schubfach_finite(-x.abs());
            assert_eq!(pos_dec, neg_dec);
        }
    }

    #[test]
    fn power_of_ten_boundaries() {
        for k in -10i32..=10 {
            round_trip(10f64.powi(k));
        }
    }

    #[test]
    fn subnormal_round_trip() {
        // Smallest positive subnormal.
        round_trip(f64::from_bits(1));
        // Largest subnormal.
        round_trip(f64::from_bits(0x000F_FFFF_FFFF_FFFF));
        // Mid-range subnormal.
        round_trip(f64::from_bits(0x0001_0000_0000_0000));
    }

    #[test]
    fn boundary_doubles_round_trip() {
        // Mantissa boundaries — values at which Schubfach's
        // irregular-spacing branch fires.
        for &x in &[
            f64::MIN_POSITIVE,                     // smallest positive normal
            f64::from_bits(0x000F_FFFF_FFFF_FFFF), // largest subnormal
            f64::from_bits(0x0010_0000_0000_0000), // smallest normal (= MIN_POSITIVE)
            2.0_f64.powi(53),
            2.0_f64.powi(53) - 1.0,
            2.0_f64.powi(-1022),
            2.0_f64,
            4.0_f64,
        ] {
            round_trip(x);
        }
    }

    /// Hard-doubles corpus: values from the literature known to
    /// stress shortest-decimal algorithms.
    ///
    /// - Paxson 1991 "A Program for Testing IEEE Decimal–Binary
    ///   Conversion": values that exposed bugs in `printf` of the
    ///   era (carries, exact halfway points, decimals just below
    ///   round boundaries).
    /// - Loitsch 2010 "Printing Floating-Point Numbers Quickly and
    ///   Accurately": Grisu3 boundary cases.
    /// - Adams 2018 "Ryū: Fast Float-to-String Conversion": inputs
    ///   chosen to exercise the rounding-window edges.
    ///
    /// Each input must round-trip; this is the strongest
    /// correctness signal short of a Test262 run.
    #[test]
    fn paxson_loitsch_adams_corpus() {
        const CORPUS: &[f64] = &[
            // Paxson 1991, Table 1 (positive values requiring full
            // 17-digit shortest forms or exposing carry/rounding
            // edges).
            5.0844542805_2_e-22,
            8.4674318044_e-46,
            8.5747326176_4_e+44,
            5.31399887517_4_e+254,
            6.2538647911_75_e+76,
            3.3870307530_43_e-77,
            1.1873256960_43_e-93,
            1.7423813547_43_e-280,
            7.7414851917_85_e-110,
            5.42101086_242_e-20,
            // Paxson Table 2 (inputs near tie boundaries).
            2.1098e-308,
            5.7184e-309,
            2.5e-323,
            // Loitsch 2010 — Grisu3 boundary cases.
            1.7976931348623157e308,  // f64::MAX
            2.2250738585072014e-308, // f64::MIN_POSITIVE
            5e-324,                  // smallest positive subnormal
            // Adams 2018 — values at the decimal-window boundaries.
            #[allow(clippy::excessive_precision)]
            9.999_999_999_999_999e-3,
            #[allow(clippy::excessive_precision)]
            9.999_999_999_999_999e22,
            1e-300,
            1e300,
            // The classic 0.1 / 0.2 / 0.3 set (each is irrational
            // in binary; expose 17-digit shortest representations).
            0.1,
            0.2,
            0.3,
            0.1 + 0.2,
            // Powers of two (single-bit mantissas) — should round
            // to the shortest decimal in their binade.
            1.0,
            2.0,
            4.0,
            8.0,
            // Power-of-ten boundaries that span mantissa precision.
            1e15,
            1e16,
            1e17,
            1e21,
            1e22,
            // Negatives — `schubfach_finite` strips sign, but the
            // round-trip helper preserves it.
            -1e-300,
            -1e300,
        ];
        for &x in CORPUS {
            round_trip(x);
        }
    }

    #[test]
    fn dense_random_round_trip() {
        // Deterministic LCG so the test is reproducible across
        // platforms. Sweeps random `f64` bit patterns; each finite
        // non-zero result must round-trip.
        let mut state: u64 = 0xCAFEBABE_DEADBEEF;
        let mut count = 0u32;
        while count < 5000 {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let x = f64::from_bits(state);
            if !x.is_finite() || x == 0.0 {
                continue;
            }
            round_trip(x);
            count += 1;
        }
    }
}
