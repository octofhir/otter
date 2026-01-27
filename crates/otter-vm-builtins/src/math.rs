//! Math built-in
//!
//! Provides all ES2025 Math methods and constants.

use otter_vm_core::memory;
use otter_vm_core::value::Value;
use otter_vm_runtime::{Op, op_native_with_mm as op_native};
use std::sync::Arc;

/// Get Math ops for extension registration
pub fn ops() -> Vec<Op> {
    vec![
        // === Basic ===
        op_native("__Math_abs", math_abs),
        op_native("__Math_ceil", math_ceil),
        op_native("__Math_floor", math_floor),
        op_native("__Math_round", math_round),
        op_native("__Math_trunc", math_trunc),
        op_native("__Math_sign", math_sign),
        // === Roots and Powers ===
        op_native("__Math_sqrt", math_sqrt),
        op_native("__Math_cbrt", math_cbrt),
        op_native("__Math_pow", math_pow),
        op_native("__Math_hypot", math_hypot),
        // === Exponentials and Logarithms ===
        op_native("__Math_exp", math_exp),
        op_native("__Math_expm1", math_expm1),
        op_native("__Math_log", math_log),
        op_native("__Math_log1p", math_log1p),
        op_native("__Math_log2", math_log2),
        op_native("__Math_log10", math_log10),
        // === Trigonometry ===
        op_native("__Math_sin", math_sin),
        op_native("__Math_cos", math_cos),
        op_native("__Math_tan", math_tan),
        op_native("__Math_asin", math_asin),
        op_native("__Math_acos", math_acos),
        op_native("__Math_atan", math_atan),
        op_native("__Math_atan2", math_atan2),
        // === Hyperbolic ===
        op_native("__Math_sinh", math_sinh),
        op_native("__Math_cosh", math_cosh),
        op_native("__Math_tanh", math_tanh),
        op_native("__Math_asinh", math_asinh),
        op_native("__Math_acosh", math_acosh),
        op_native("__Math_atanh", math_atanh),
        // === Min/Max/Random ===
        op_native("__Math_min", math_min),
        op_native("__Math_max", math_max),
        op_native("__Math_random", math_random),
        // === Special ===
        op_native("__Math_clz32", math_clz32),
        op_native("__Math_imul", math_imul),
        op_native("__Math_fround", math_fround),
        op_native("__Math_f16round", math_f16round),
    ]
}

// ============================================================================
// Helper: convert Value to f64
// ============================================================================

fn to_number(val: &Value) -> f64 {
    if let Some(n) = val.as_number() {
        n
    } else if let Some(n) = val.as_int32() {
        n as f64
    } else if val.is_undefined() || val.is_null() {
        f64::NAN
    } else if let Some(b) = val.as_boolean() {
        if b { 1.0 } else { 0.0 }
    } else {
        f64::NAN
    }
}

fn get_arg(args: &[Value], idx: usize) -> f64 {
    args.get(idx).map(to_number).unwrap_or(f64::NAN)
}

// ============================================================================
// Basic Methods
// ============================================================================

fn math_abs(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, String> {
    let x = get_arg(args, 0);
    Ok(Value::number(x.abs()))
}

fn math_ceil(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, String> {
    let x = get_arg(args, 0);
    Ok(Value::number(x.ceil()))
}

fn math_floor(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, String> {
    let x = get_arg(args, 0);
    Ok(Value::number(x.floor()))
}

fn math_round(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, String> {
    let x = get_arg(args, 0);
    // JavaScript round: round half towards +infinity
    // e.g., round(-0.5) = -0, round(0.5) = 1
    let rounded = if x.fract() == -0.5 {
        x.ceil()
    } else {
        x.round()
    };
    Ok(Value::number(rounded))
}

fn math_trunc(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, String> {
    let x = get_arg(args, 0);
    Ok(Value::number(x.trunc()))
}

fn math_sign(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, String> {
    let x = get_arg(args, 0);
    let result = if x.is_nan() {
        f64::NAN
    } else if x > 0.0 {
        1.0
    } else if x < 0.0 {
        -1.0
    } else {
        x // Preserve +0/-0
    };
    Ok(Value::number(result))
}

// ============================================================================
// Roots and Powers
// ============================================================================

fn math_sqrt(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, String> {
    let x = get_arg(args, 0);
    Ok(Value::number(x.sqrt()))
}

fn math_cbrt(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, String> {
    let x = get_arg(args, 0);
    Ok(Value::number(x.cbrt()))
}

fn math_pow(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, String> {
    let base = get_arg(args, 0);
    let exp = get_arg(args, 1);
    Ok(Value::number(base.powf(exp)))
}

fn math_hypot(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, String> {
    // hypot(...values) - returns sqrt(sum(x^2))
    if args.is_empty() {
        return Ok(Value::number(0.0));
    }

    let mut has_infinity = false;
    let mut sum_sq = 0.0;

    for arg in args {
        let x = to_number(arg);
        if x.is_infinite() {
            has_infinity = true;
        }
        if x.is_nan() {
            return Ok(Value::number(f64::NAN));
        }
        sum_sq += x * x;
    }

    if has_infinity {
        return Ok(Value::number(f64::INFINITY));
    }

    Ok(Value::number(sum_sq.sqrt()))
}

// ============================================================================
// Exponentials and Logarithms
// ============================================================================

fn math_exp(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, String> {
    let x = get_arg(args, 0);
    Ok(Value::number(x.exp()))
}

fn math_expm1(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, String> {
    let x = get_arg(args, 0);
    Ok(Value::number(x.exp_m1()))
}

fn math_log(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, String> {
    let x = get_arg(args, 0);
    Ok(Value::number(x.ln()))
}

fn math_log1p(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, String> {
    let x = get_arg(args, 0);
    Ok(Value::number(x.ln_1p()))
}

fn math_log2(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, String> {
    let x = get_arg(args, 0);
    Ok(Value::number(x.log2()))
}

fn math_log10(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, String> {
    let x = get_arg(args, 0);
    Ok(Value::number(x.log10()))
}

// ============================================================================
// Trigonometry
// ============================================================================

fn math_sin(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, String> {
    let x = get_arg(args, 0);
    Ok(Value::number(x.sin()))
}

fn math_cos(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, String> {
    let x = get_arg(args, 0);
    Ok(Value::number(x.cos()))
}

fn math_tan(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, String> {
    let x = get_arg(args, 0);
    Ok(Value::number(x.tan()))
}

fn math_asin(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, String> {
    let x = get_arg(args, 0);
    Ok(Value::number(x.asin()))
}

fn math_acos(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, String> {
    let x = get_arg(args, 0);
    Ok(Value::number(x.acos()))
}

fn math_atan(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, String> {
    let x = get_arg(args, 0);
    Ok(Value::number(x.atan()))
}

fn math_atan2(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, String> {
    let y = get_arg(args, 0);
    let x = get_arg(args, 1);
    Ok(Value::number(y.atan2(x)))
}

// ============================================================================
// Hyperbolic
// ============================================================================

fn math_sinh(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, String> {
    let x = get_arg(args, 0);
    Ok(Value::number(x.sinh()))
}

fn math_cosh(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, String> {
    let x = get_arg(args, 0);
    Ok(Value::number(x.cosh()))
}

fn math_tanh(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, String> {
    let x = get_arg(args, 0);
    Ok(Value::number(x.tanh()))
}

fn math_asinh(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, String> {
    let x = get_arg(args, 0);
    Ok(Value::number(x.asinh()))
}

fn math_acosh(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, String> {
    let x = get_arg(args, 0);
    Ok(Value::number(x.acosh()))
}

fn math_atanh(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, String> {
    let x = get_arg(args, 0);
    Ok(Value::number(x.atanh()))
}

// ============================================================================
// Min/Max/Random
// ============================================================================

fn math_min(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, String> {
    if args.is_empty() {
        return Ok(Value::number(f64::INFINITY));
    }

    let mut result = f64::INFINITY;
    for arg in args {
        let x = to_number(arg);
        if x.is_nan() {
            return Ok(Value::number(f64::NAN));
        }
        // Handle -0 vs +0: -0 is less than +0
        if x < result || (x == 0.0 && result == 0.0 && x.is_sign_negative()) {
            result = x;
        }
    }
    Ok(Value::number(result))
}

fn math_max(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, String> {
    if args.is_empty() {
        return Ok(Value::number(f64::NEG_INFINITY));
    }

    let mut result = f64::NEG_INFINITY;
    for arg in args {
        let x = to_number(arg);
        if x.is_nan() {
            return Ok(Value::number(f64::NAN));
        }
        // Handle -0 vs +0: +0 is greater than -0
        if x > result || (x == 0.0 && result == 0.0 && !x.is_sign_negative()) {
            result = x;
        }
    }
    Ok(Value::number(result))
}

fn math_random(_args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, String> {
    // Simple xorshift64 PRNG (for demonstration; real impl should use thread_rng)
    use std::cell::Cell;
    use std::time::{SystemTime, UNIX_EPOCH};

    thread_local! {
        static STATE: Cell<u64> = const { Cell::new(0) };
    }

    STATE.with(|state| {
        let mut s = state.get();
        if s == 0 {
            // Initialize with current time
            s = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0x853c49e6748fea9b);
        }

        // xorshift64
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        state.set(s);

        // Convert to [0, 1) range
        let random = (s >> 11) as f64 / ((1u64 << 53) as f64);
        Ok(Value::number(random))
    })
}

// ============================================================================
// Special Methods
// ============================================================================

fn math_clz32(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, String> {
    let x = get_arg(args, 0);
    // ToUint32
    let n = if x.is_nan() || x.is_infinite() {
        0u32
    } else {
        x.trunc() as i64 as u32
    };
    Ok(Value::number(n.leading_zeros() as f64))
}

fn math_imul(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, String> {
    let a = get_arg(args, 0);
    let b = get_arg(args, 1);

    // ToInt32
    let a_i32 = if a.is_nan() || a.is_infinite() {
        0i32
    } else {
        a.trunc() as i64 as i32
    };
    let b_i32 = if b.is_nan() || b.is_infinite() {
        0i32
    } else {
        b.trunc() as i64 as i32
    };

    // 32-bit integer multiplication
    let result = a_i32.wrapping_mul(b_i32);
    Ok(Value::number(result as f64))
}

fn math_fround(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, String> {
    let x = get_arg(args, 0);
    // Round to 32-bit float and back
    let f32_val = x as f32;
    Ok(Value::number(f32_val as f64))
}

fn math_f16round(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, String> {
    let x = get_arg(args, 0);

    // Handle special cases
    if x.is_nan() {
        return Ok(Value::number(f64::NAN));
    }
    if x.is_infinite() {
        return Ok(Value::number(x));
    }
    if x == 0.0 {
        return Ok(Value::number(x)); // Preserve sign of zero
    }

    // Convert f64 to f16 (IEEE 754 half-precision)
    let f16_bits = f64_to_f16(x);
    let result = f16_to_f64(f16_bits);

    Ok(Value::number(result))
}

/// Convert f64 to f16 bits (IEEE 754 half-precision)
fn f64_to_f16(x: f64) -> u16 {
    let bits = x.to_bits();
    let sign = ((bits >> 63) & 1) as u16;
    let exp = ((bits >> 52) & 0x7ff) as i32;
    let frac = bits & 0xfffffffffffff;

    // Handle special cases
    if exp == 0x7ff {
        // Infinity or NaN
        if frac == 0 {
            return (sign << 15) | 0x7c00; // Infinity
        } else {
            return (sign << 15) | 0x7e00; // NaN
        }
    }

    // Adjust exponent bias: f64 bias = 1023, f16 bias = 15
    let new_exp = exp - 1023 + 15;

    if new_exp >= 31 {
        // Overflow to infinity
        return (sign << 15) | 0x7c00;
    }

    if new_exp <= 0 {
        // Denormalized or underflow
        if new_exp < -10 {
            return sign << 15; // Underflow to zero
        }
        // Denormalized
        let mantissa = (frac | 0x10000000000000) >> (1 - new_exp + 42);
        return (sign << 15) | (mantissa as u16);
    }

    // Normal number
    let mantissa = (frac >> 42) as u16;
    (sign << 15) | ((new_exp as u16) << 10) | mantissa
}

/// Convert f16 bits to f64
fn f16_to_f64(bits: u16) -> f64 {
    let sign = (bits >> 15) & 1;
    let exp = (bits >> 10) & 0x1f;
    let frac = bits & 0x3ff;

    if exp == 0x1f {
        // Infinity or NaN
        if frac == 0 {
            return if sign == 1 {
                f64::NEG_INFINITY
            } else {
                f64::INFINITY
            };
        } else {
            return f64::NAN;
        }
    }

    if exp == 0 {
        if frac == 0 {
            // Zero
            return if sign == 1 { -0.0 } else { 0.0 };
        }
        // Denormalized
        let frac_f64 = frac as f64 / 1024.0;
        let value = frac_f64 * 2.0f64.powi(-14);
        return if sign == 1 { -value } else { value };
    }

    // Normal number
    let new_exp = (exp as i32 - 15 + 1023) as u64;
    let new_frac = (frac as u64) << 42;
    let bits64 = ((sign as u64) << 63) | (new_exp << 52) | new_frac;

    f64::from_bits(bits64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_math_abs() {
        let mm = Arc::new(memory::MemoryManager::test());
        assert_eq!(
            math_abs(&[Value::number(-5.0)], mm.clone())
                .unwrap()
                .as_number(),
            Some(5.0)
        );
        assert_eq!(
            math_abs(&[Value::number(5.0)], mm).unwrap().as_number(),
            Some(5.0)
        );
    }

    #[test]
    fn test_math_ceil() {
        let mm = Arc::new(memory::MemoryManager::test());
        assert_eq!(
            math_ceil(&[Value::number(1.1)], mm.clone())
                .unwrap()
                .as_number(),
            Some(2.0)
        );
        assert_eq!(
            math_ceil(&[Value::number(-1.1)], mm).unwrap().as_number(),
            Some(-1.0)
        );
    }

    #[test]
    fn test_math_floor() {
        let mm = Arc::new(memory::MemoryManager::test());
        assert_eq!(
            math_floor(&[Value::number(1.9)], mm.clone())
                .unwrap()
                .as_number(),
            Some(1.0)
        );
        assert_eq!(
            math_floor(&[Value::number(-1.1)], mm).unwrap().as_number(),
            Some(-2.0)
        );
    }

    #[test]
    fn test_math_round() {
        let mm = Arc::new(memory::MemoryManager::test());
        assert_eq!(
            math_round(&[Value::number(1.5)], mm.clone())
                .unwrap()
                .as_number(),
            Some(2.0)
        );
        assert_eq!(
            math_round(&[Value::number(1.4)], mm).unwrap().as_number(),
            Some(1.0)
        );
    }

    #[test]
    fn test_math_trunc() {
        let mm = Arc::new(memory::MemoryManager::test());
        assert_eq!(
            math_trunc(&[Value::number(1.9)], mm.clone())
                .unwrap()
                .as_number(),
            Some(1.0)
        );
        assert_eq!(
            math_trunc(&[Value::number(-1.9)], mm).unwrap().as_number(),
            Some(-1.0)
        );
    }

    #[test]
    fn test_math_sign() {
        let mm = Arc::new(memory::MemoryManager::test());
        assert_eq!(
            math_sign(&[Value::number(5.0)], mm.clone())
                .unwrap()
                .as_number(),
            Some(1.0)
        );
        assert_eq!(
            math_sign(&[Value::number(-5.0)], mm.clone())
                .unwrap()
                .as_number(),
            Some(-1.0)
        );
        assert_eq!(
            math_sign(&[Value::number(0.0)], mm).unwrap().as_number(),
            Some(0.0)
        );
    }

    #[test]
    fn test_math_sqrt() {
        let mm = Arc::new(memory::MemoryManager::test());
        assert_eq!(
            math_sqrt(&[Value::number(4.0)], mm.clone())
                .unwrap()
                .as_number(),
            Some(2.0)
        );
        assert_eq!(
            math_sqrt(&[Value::number(9.0)], mm).unwrap().as_number(),
            Some(3.0)
        );
    }

    #[test]
    fn test_math_cbrt() {
        let mm = Arc::new(memory::MemoryManager::test());
        assert_eq!(
            math_cbrt(&[Value::number(8.0)], mm.clone())
                .unwrap()
                .as_number(),
            Some(2.0)
        );
        assert_eq!(
            math_cbrt(&[Value::number(27.0)], mm).unwrap().as_number(),
            Some(3.0)
        );
    }

    #[test]
    fn test_math_pow() {
        let mm = Arc::new(memory::MemoryManager::test());
        assert_eq!(
            math_pow(&[Value::number(2.0), Value::number(3.0)], mm)
                .unwrap()
                .as_number(),
            Some(8.0)
        );
    }

    #[test]
    fn test_math_hypot() {
        let mm = Arc::new(memory::MemoryManager::test());
        assert_eq!(
            math_hypot(&[Value::number(3.0), Value::number(4.0)], mm)
                .unwrap()
                .as_number(),
            Some(5.0)
        );
    }

    #[test]
    fn test_math_exp() {
        let mm = Arc::new(memory::MemoryManager::test());
        let result = math_exp(&[Value::number(1.0)], mm).unwrap();
        let n = result.as_number().unwrap();
        assert!((n - std::f64::consts::E).abs() < 1e-10);
    }

    #[test]
    fn test_math_log() {
        let mm = Arc::new(memory::MemoryManager::test());
        let result = math_log(&[Value::number(std::f64::consts::E)], mm).unwrap();
        let n = result.as_number().unwrap();
        assert!((n - 1.0).abs() < 1e-10);
    }

    #[test]
    fn test_math_trig() {
        let mm = Arc::new(memory::MemoryManager::test());
        // sin(0) = 0
        assert_eq!(
            math_sin(&[Value::number(0.0)], mm.clone())
                .unwrap()
                .as_number(),
            Some(0.0)
        );
        // cos(0) = 1
        assert_eq!(
            math_cos(&[Value::number(0.0)], mm.clone())
                .unwrap()
                .as_number(),
            Some(1.0)
        );
        // tan(0) = 0
        assert_eq!(
            math_tan(&[Value::number(0.0)], mm).unwrap().as_number(),
            Some(0.0)
        );
    }

    #[test]
    fn test_math_min_max() {
        let mm = Arc::new(memory::MemoryManager::test());
        assert_eq!(
            math_min(
                &[Value::number(1.0), Value::number(2.0), Value::number(3.0)],
                mm.clone()
            )
            .unwrap()
            .as_number(),
            Some(1.0)
        );
        assert_eq!(
            math_max(
                &[Value::number(1.0), Value::number(2.0), Value::number(3.0)],
                mm
            )
            .unwrap()
            .as_number(),
            Some(3.0)
        );
    }

    #[test]
    fn test_math_random() {
        let mm = Arc::new(memory::MemoryManager::test());
        let result = math_random(&[], mm).unwrap();
        let n = result.as_number().unwrap();
        assert!((0.0..1.0).contains(&n));
    }

    #[test]
    fn test_math_clz32() {
        let mm = Arc::new(memory::MemoryManager::test());
        assert_eq!(
            math_clz32(&[Value::number(1.0)], mm.clone())
                .unwrap()
                .as_number(),
            Some(31.0)
        );
        assert_eq!(
            math_clz32(&[Value::number(0.0)], mm).unwrap().as_number(),
            Some(32.0)
        );
    }

    #[test]
    fn test_math_imul() {
        let mm = Arc::new(memory::MemoryManager::test());
        assert_eq!(
            math_imul(&[Value::number(3.0), Value::number(4.0)], mm)
                .unwrap()
                .as_number(),
            Some(12.0)
        );
    }

    #[test]
    fn test_math_fround() {
        let mm = Arc::new(memory::MemoryManager::test());
        // 1.5 can be exactly represented in f32
        assert_eq!(
            math_fround(&[Value::number(1.5)], mm).unwrap().as_number(),
            Some(1.5)
        );
    }

    #[test]
    fn test_math_f16round() {
        let mm = Arc::new(memory::MemoryManager::test());
        // Test that f16round works for common values
        assert_eq!(
            math_f16round(&[Value::number(0.0)], mm.clone())
                .unwrap()
                .as_number(),
            Some(0.0)
        );
        assert_eq!(
            math_f16round(&[Value::number(1.0)], mm.clone())
                .unwrap()
                .as_number(),
            Some(1.0)
        );
        // f16 can represent 65504 (max normal)
        assert_eq!(
            math_f16round(&[Value::number(65504.0)], mm)
                .unwrap()
                .as_number(),
            Some(65504.0)
        );
    }
}
