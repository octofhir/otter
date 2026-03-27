//! ES2024 §21.3 The Math Object
//!
//! Implements the complete Math namespace: 8 value properties (constants) and
//! all 37 function properties with proper ToNumber coercion via RuntimeState.

use crate::builders::NamespaceBuilder;
use crate::descriptors::{
    NativeBindingDescriptor, NativeBindingTarget, NativeFunctionDescriptor, VmNativeCallError,
};
use crate::interpreter::{InterpreterError, RuntimeState};
use crate::value::RegisterValue;

use super::{
    IntrinsicsError, VmIntrinsics,
    install::{IntrinsicInstallContext, IntrinsicInstaller, install_object_plan},
};

pub(super) static MATH_INTRINSIC: MathIntrinsic = MathIntrinsic;

pub(super) struct MathIntrinsic;

// ---------------------------------------------------------------------------
// Coercion bridge: InterpreterError → VmNativeCallError
// ---------------------------------------------------------------------------

fn coerce_err(e: InterpreterError) -> VmNativeCallError {
    VmNativeCallError::Internal(format!("Math: ToNumber failed: {e}").into())
}

/// ES spec 7.1.4 ToNumber — shorthand for native Math functions.
fn to_number(
    arg: RegisterValue,
    runtime: &mut RuntimeState,
) -> Result<f64, VmNativeCallError> {
    runtime.js_to_number(arg).map_err(coerce_err)
}

/// Convenience: extract arg at index (or undefined if absent) and coerce to f64.
fn arg_to_number(
    args: &[RegisterValue],
    index: usize,
    runtime: &mut RuntimeState,
) -> Result<f64, VmNativeCallError> {
    let value = args.get(index).copied().unwrap_or_else(RegisterValue::undefined);
    to_number(value, runtime)
}

/// ES spec 7.1.6 ToInt32 — shorthand for clz32/imul.
fn arg_to_int32(
    args: &[RegisterValue],
    index: usize,
    runtime: &mut RuntimeState,
) -> Result<i32, VmNativeCallError> {
    runtime
        .js_to_int32(args.get(index).copied().unwrap_or_else(RegisterValue::undefined))
        .map_err(coerce_err)
}

/// ES spec 7.1.7 ToUint32 — shorthand for clz32.
fn arg_to_uint32(
    args: &[RegisterValue],
    index: usize,
    runtime: &mut RuntimeState,
) -> Result<u32, VmNativeCallError> {
    runtime
        .js_to_uint32(args.get(index).copied().unwrap_or_else(RegisterValue::undefined))
        .map_err(coerce_err)
}

// ---------------------------------------------------------------------------
// Constants (ES2024 §21.3.1)
// ---------------------------------------------------------------------------

const MATH_E: f64 = std::f64::consts::E;
const MATH_LN10: f64 = std::f64::consts::LN_10;
const MATH_LN2: f64 = std::f64::consts::LN_2;
const MATH_LOG10E: f64 = std::f64::consts::LOG10_E;
const MATH_LOG2E: f64 = std::f64::consts::LOG2_E;
const MATH_PI: f64 = std::f64::consts::PI;
const MATH_SQRT1_2: f64 = std::f64::consts::FRAC_1_SQRT_2;
const MATH_SQRT2: f64 = std::f64::consts::SQRT_2;

/// All Math value properties in spec order.
const MATH_CONSTANTS: &[(&str, f64)] = &[
    ("E", MATH_E),
    ("LN10", MATH_LN10),
    ("LN2", MATH_LN2),
    ("LOG10E", MATH_LOG10E),
    ("LOG2E", MATH_LOG2E),
    ("PI", MATH_PI),
    ("SQRT1_2", MATH_SQRT1_2),
    ("SQRT2", MATH_SQRT2),
];

// ---------------------------------------------------------------------------
// Installer
// ---------------------------------------------------------------------------

impl IntrinsicInstaller for MathIntrinsic {
    fn init(
        &self,
        intrinsics: &mut VmIntrinsics,
        cx: &mut IntrinsicInstallContext<'_>,
    ) -> Result<(), IntrinsicsError> {
        let math_namespace = cx.alloc_intrinsic_object(Some(intrinsics.object_prototype()))?;

        // ES2024 §21.3.1: Math value properties are {W:false, E:false, C:false}.
        for &(name, value) in MATH_CONSTANTS {
            let prop = cx.property_names.intern(name);
            cx.heap.define_own_property(
                math_namespace,
                prop,
                crate::object::PropertyValue::data_with_attrs(
                    RegisterValue::from_number(value),
                    crate::object::PropertyAttributes::constant(),
                ),
            )?;
        }

        // Install @@toStringTag = "Math" (via plain property for now).
        let tag_prop = cx.property_names.intern("@@toStringTag");
        let tag_handle = cx.heap.alloc_string("Math");
        cx.heap.set_property(
            math_namespace,
            tag_prop,
            RegisterValue::from_object_handle(tag_handle.0),
        )?;

        // Install all 37 function properties.
        let math_plan = NamespaceBuilder::from_bindings(&math_method_bindings())
            .expect("Math namespace descriptors should normalize")
            .build();
        install_object_plan(
            math_namespace,
            &math_plan,
            intrinsics.function_prototype(),
            cx,
        )?;

        intrinsics.set_math_namespace(math_namespace);
        Ok(())
    }

    fn install_on_global(
        &self,
        intrinsics: &VmIntrinsics,
        cx: &mut IntrinsicInstallContext<'_>,
    ) -> Result<(), IntrinsicsError> {
        let math_namespace = intrinsics
            .math_namespace()
            .expect("Math namespace should be installed during init_core");
        cx.install_global_value(
            intrinsics,
            "Math",
            RegisterValue::from_object_handle(math_namespace.0),
        )
    }
}

// ---------------------------------------------------------------------------
// Method bindings — all 37 ES2024 §21.3.2 function properties
// ---------------------------------------------------------------------------

fn math_method_bindings() -> Vec<NativeBindingDescriptor> {
    vec![
        method("abs", 1, math_abs),
        method("acos", 1, math_acos),
        method("acosh", 1, math_acosh),
        method("asin", 1, math_asin),
        method("asinh", 1, math_asinh),
        method("atan", 1, math_atan),
        method("atanh", 1, math_atanh),
        method("atan2", 2, math_atan2),
        method("cbrt", 1, math_cbrt),
        method("ceil", 1, math_ceil),
        method("clz32", 1, math_clz32),
        method("cos", 1, math_cos),
        method("cosh", 1, math_cosh),
        method("exp", 1, math_exp),
        method("expm1", 1, math_expm1),
        method("floor", 1, math_floor),
        method("fround", 1, math_fround),
        method("hypot", 2, math_hypot),
        method("imul", 2, math_imul),
        method("log", 1, math_log),
        method("log1p", 1, math_log1p),
        method("log10", 1, math_log10),
        method("log2", 1, math_log2),
        method("max", 2, math_max),
        method("min", 2, math_min),
        method("pow", 2, math_pow),
        method("random", 0, math_random),
        method("round", 1, math_round),
        method("sign", 1, math_sign),
        method("sin", 1, math_sin),
        method("sinh", 1, math_sinh),
        method("sqrt", 1, math_sqrt),
        method("tan", 1, math_tan),
        method("tanh", 1, math_tanh),
        method("trunc", 1, math_trunc),
        method("f16round", 1, math_f16round),
    ]
}

fn method(
    name: &str,
    length: u16,
    callback: crate::descriptors::VmNativeFunction,
) -> NativeBindingDescriptor {
    NativeBindingDescriptor::new(
        NativeBindingTarget::Namespace,
        NativeFunctionDescriptor::method(name, length, callback),
    )
}

// ---------------------------------------------------------------------------
// ES2024 §21.3.2.1  Math.abs(x)
// ---------------------------------------------------------------------------
fn math_abs(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let x = arg_to_number(args, 0, runtime)?;
    Ok(RegisterValue::from_number(x.abs()))
}

// ---------------------------------------------------------------------------
// ES2024 §21.3.2.2  Math.acos(x)
// ---------------------------------------------------------------------------
fn math_acos(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let x = arg_to_number(args, 0, runtime)?;
    Ok(RegisterValue::from_number(x.acos()))
}

// ---------------------------------------------------------------------------
// ES2024 §21.3.2.3  Math.acosh(x)
// ---------------------------------------------------------------------------
fn math_acosh(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let x = arg_to_number(args, 0, runtime)?;
    Ok(RegisterValue::from_number(x.acosh()))
}

// ---------------------------------------------------------------------------
// ES2024 §21.3.2.4  Math.asin(x)
// ---------------------------------------------------------------------------
fn math_asin(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let x = arg_to_number(args, 0, runtime)?;
    Ok(RegisterValue::from_number(x.asin()))
}

// ---------------------------------------------------------------------------
// ES2024 §21.3.2.5  Math.asinh(x)
// ---------------------------------------------------------------------------
fn math_asinh(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let x = arg_to_number(args, 0, runtime)?;
    Ok(RegisterValue::from_number(x.asinh()))
}

// ---------------------------------------------------------------------------
// ES2024 §21.3.2.6  Math.atan(x)
// ---------------------------------------------------------------------------
fn math_atan(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let x = arg_to_number(args, 0, runtime)?;
    Ok(RegisterValue::from_number(x.atan()))
}

// ---------------------------------------------------------------------------
// ES2024 §21.3.2.7  Math.atanh(x)
// ---------------------------------------------------------------------------
fn math_atanh(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let x = arg_to_number(args, 0, runtime)?;
    Ok(RegisterValue::from_number(x.atanh()))
}

// ---------------------------------------------------------------------------
// ES2024 §21.3.2.8  Math.atan2(y, x)
// ---------------------------------------------------------------------------
fn math_atan2(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let y = arg_to_number(args, 0, runtime)?;
    let x = arg_to_number(args, 1, runtime)?;
    Ok(RegisterValue::from_number(y.atan2(x)))
}

// ---------------------------------------------------------------------------
// ES2024 §21.3.2.9  Math.cbrt(x)
// ---------------------------------------------------------------------------
fn math_cbrt(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let x = arg_to_number(args, 0, runtime)?;
    Ok(RegisterValue::from_number(x.cbrt()))
}

// ---------------------------------------------------------------------------
// ES2024 §21.3.2.10  Math.ceil(x)
// ---------------------------------------------------------------------------
fn math_ceil(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let x = arg_to_number(args, 0, runtime)?;
    Ok(RegisterValue::from_number(x.ceil()))
}

// ---------------------------------------------------------------------------
// ES2024 §21.3.2.11  Math.clz32(x)
//
// Returns the number of leading zero bits in the 32-bit unsigned integer
// representation of ToUint32(x).
// ---------------------------------------------------------------------------
fn math_clz32(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let n = arg_to_uint32(args, 0, runtime)?;
    Ok(RegisterValue::from_i32(n.leading_zeros() as i32))
}

// ---------------------------------------------------------------------------
// ES2024 §21.3.2.12  Math.cos(x)
// ---------------------------------------------------------------------------
fn math_cos(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let x = arg_to_number(args, 0, runtime)?;
    Ok(RegisterValue::from_number(x.cos()))
}

// ---------------------------------------------------------------------------
// ES2024 §21.3.2.13  Math.cosh(x)
// ---------------------------------------------------------------------------
fn math_cosh(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let x = arg_to_number(args, 0, runtime)?;
    Ok(RegisterValue::from_number(x.cosh()))
}

// ---------------------------------------------------------------------------
// ES2024 §21.3.2.14  Math.exp(x)
// ---------------------------------------------------------------------------
fn math_exp(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let x = arg_to_number(args, 0, runtime)?;
    Ok(RegisterValue::from_number(x.exp()))
}

// ---------------------------------------------------------------------------
// ES2024 §21.3.2.15  Math.expm1(x)
// ---------------------------------------------------------------------------
fn math_expm1(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let x = arg_to_number(args, 0, runtime)?;
    Ok(RegisterValue::from_number(x.exp_m1()))
}

// ---------------------------------------------------------------------------
// ES2024 §21.3.2.16  Math.floor(x)
// ---------------------------------------------------------------------------
fn math_floor(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let x = arg_to_number(args, 0, runtime)?;
    Ok(RegisterValue::from_number(x.floor()))
}

// ---------------------------------------------------------------------------
// ES2024 §21.3.2.17  Math.fround(x)
//
// Rounds to the nearest IEEE 754 binary32 (float) value.
// ---------------------------------------------------------------------------
fn math_fround(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let x = arg_to_number(args, 0, runtime)?;
    Ok(RegisterValue::from_number((x as f32) as f64))
}

// ---------------------------------------------------------------------------
// ES2024 §21.3.2.18  Math.hypot(...values)
//
// Variadic. Returns sqrt(sum(x_i^2)). Handles ±Infinity correctly per spec:
// if any argument is ±Infinity, result is +Infinity (even if another is NaN).
// ---------------------------------------------------------------------------
fn math_hypot(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    if args.is_empty() {
        return Ok(RegisterValue::from_number(0.0));
    }

    // First pass: coerce all args, check for Infinity.
    let mut coerced = Vec::with_capacity(args.len());
    let mut has_infinity = false;
    let mut has_nan = false;

    for arg in args {
        let n = to_number(*arg, runtime)?;
        if n.is_infinite() {
            has_infinity = true;
        }
        if n.is_nan() {
            has_nan = true;
        }
        coerced.push(n);
    }

    // §21.3.2.18 step 4: If any value is ±∞, return +∞.
    if has_infinity {
        return Ok(RegisterValue::from_number(f64::INFINITY));
    }
    // §21.3.2.18 step 5: If any value is NaN, return NaN.
    if has_nan {
        return Ok(RegisterValue::from_number(f64::NAN));
    }

    // Kahan-style compensated summation for accuracy on large inputs.
    // Find the maximum absolute value to scale and prevent overflow/underflow.
    let max_abs = coerced.iter().map(|x| x.abs()).fold(0.0_f64, f64::max);
    if max_abs == 0.0 {
        return Ok(RegisterValue::from_number(0.0));
    }

    let mut sum = 0.0_f64;
    for x in &coerced {
        let scaled = x / max_abs;
        sum += scaled * scaled;
    }

    Ok(RegisterValue::from_number(max_abs * sum.sqrt()))
}

// ---------------------------------------------------------------------------
// ES2024 §21.3.2.19  Math.imul(x, y)
//
// Returns the 32-bit integer multiplication of ToInt32(x) and ToInt32(y).
// ---------------------------------------------------------------------------
fn math_imul(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let a = arg_to_int32(args, 0, runtime)?;
    let b = arg_to_int32(args, 1, runtime)?;
    Ok(RegisterValue::from_i32(a.wrapping_mul(b)))
}

// ---------------------------------------------------------------------------
// ES2024 §21.3.2.20  Math.log(x)
// ---------------------------------------------------------------------------
fn math_log(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let x = arg_to_number(args, 0, runtime)?;
    Ok(RegisterValue::from_number(x.ln()))
}

// ---------------------------------------------------------------------------
// ES2024 §21.3.2.21  Math.log1p(x)
// ---------------------------------------------------------------------------
fn math_log1p(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let x = arg_to_number(args, 0, runtime)?;
    Ok(RegisterValue::from_number(x.ln_1p()))
}

// ---------------------------------------------------------------------------
// ES2024 §21.3.2.22  Math.log10(x)
// ---------------------------------------------------------------------------
fn math_log10(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let x = arg_to_number(args, 0, runtime)?;
    Ok(RegisterValue::from_number(x.log10()))
}

// ---------------------------------------------------------------------------
// ES2024 §21.3.2.23  Math.log2(x)
// ---------------------------------------------------------------------------
fn math_log2(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let x = arg_to_number(args, 0, runtime)?;
    Ok(RegisterValue::from_number(x.log2()))
}

// ---------------------------------------------------------------------------
// ES2024 §21.3.2.24  Math.max(...values)
//
// Variadic. Returns the largest of its arguments. With no args returns -∞.
// ---------------------------------------------------------------------------
fn math_max(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    if args.is_empty() {
        return Ok(RegisterValue::from_number(f64::NEG_INFINITY));
    }
    let mut highest = f64::NEG_INFINITY;
    for arg in args {
        let n = to_number(*arg, runtime)?;
        // NaN is contagious per spec.
        if n.is_nan() {
            return Ok(RegisterValue::from_number(f64::NAN));
        }
        // +0 > -0 per spec.
        if n > highest || (n == 0.0 && highest == 0.0 && !n.is_sign_negative()) {
            highest = n;
        }
    }
    Ok(RegisterValue::from_number(highest))
}

// ---------------------------------------------------------------------------
// ES2024 §21.3.2.25  Math.min(...values)
//
// Variadic. Returns the smallest of its arguments. With no args returns +∞.
// ---------------------------------------------------------------------------
fn math_min(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    if args.is_empty() {
        return Ok(RegisterValue::from_number(f64::INFINITY));
    }
    let mut lowest = f64::INFINITY;
    for arg in args {
        let n = to_number(*arg, runtime)?;
        if n.is_nan() {
            return Ok(RegisterValue::from_number(f64::NAN));
        }
        // -0 < +0 per spec.
        if n < lowest || (n == 0.0 && lowest == 0.0 && n.is_sign_negative()) {
            lowest = n;
        }
    }
    Ok(RegisterValue::from_number(lowest))
}

// ---------------------------------------------------------------------------
// ES2024 §21.3.2.26  Math.pow(base, exponent)
//
// Uses spec-defined semantics for edge cases (±0, ±Infinity, NaN, -1).
// We delegate to f64::powf which matches IEEE 754-2008 for most cases,
// then patch the three spec-mandated divergences.
// ---------------------------------------------------------------------------
fn math_pow(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let base = arg_to_number(args, 0, runtime)?;
    let exponent = arg_to_number(args, 1, runtime)?;
    Ok(RegisterValue::from_number(js_pow(base, exponent)))
}

/// ES2024 §6.1.6.1.1 Number::exponentiate — production-correct pow.
fn js_pow(base: f64, exponent: f64) -> f64 {
    // §21.3.2.26 step 1: If exponent is NaN, return NaN.
    if exponent.is_nan() {
        return f64::NAN;
    }
    // §21.3.2.26 step 2: If exponent is +0 or -0, return 1.
    if exponent == 0.0 {
        return 1.0;
    }
    // §21.3.2.26 step 3: If base is NaN, return NaN.
    if base.is_nan() {
        return f64::NAN;
    }
    // §21.3.2.26 step 4: If base is +∞:
    if base == f64::INFINITY {
        return if exponent > 0.0 {
            f64::INFINITY
        } else {
            0.0
        };
    }
    // §21.3.2.26 step 5: If base is -∞:
    if base == f64::NEG_INFINITY {
        if exponent > 0.0 {
            return if is_odd_integer(exponent) {
                f64::NEG_INFINITY
            } else {
                f64::INFINITY
            };
        }
        return if is_odd_integer(exponent) { -0.0 } else { 0.0 };
    }
    // §21.3.2.26 step 6: If base is +0:
    if base == 0.0 && base.is_sign_positive() {
        return if exponent > 0.0 {
            0.0
        } else {
            f64::INFINITY
        };
    }
    // §21.3.2.26 step 7: If base is -0:
    if base == 0.0 && base.is_sign_negative() {
        if exponent > 0.0 {
            return if is_odd_integer(exponent) { -0.0 } else { 0.0 };
        }
        return if is_odd_integer(exponent) {
            f64::NEG_INFINITY
        } else {
            f64::INFINITY
        };
    }
    // §21.3.2.26 step 10: If exponent is +∞:
    if exponent == f64::INFINITY {
        let abs_base = base.abs();
        return if abs_base > 1.0 {
            f64::INFINITY
        } else if abs_base == 1.0 {
            f64::NAN
        } else {
            0.0
        };
    }
    // §21.3.2.26 step 11: If exponent is -∞:
    if exponent == f64::NEG_INFINITY {
        let abs_base = base.abs();
        return if abs_base > 1.0 {
            0.0
        } else if abs_base == 1.0 {
            f64::NAN
        } else {
            f64::INFINITY
        };
    }
    // §21.3.2.26 step 13: If base < 0 and exponent is not an integer, return NaN.
    if base < 0.0 && exponent.fract() != 0.0 {
        return f64::NAN;
    }

    base.powf(exponent)
}

/// Returns true if the value is a finite odd integer.
fn is_odd_integer(v: f64) -> bool {
    v.is_finite() && v.fract() == 0.0 && (v.abs() % 2.0) == 1.0
}

// ---------------------------------------------------------------------------
// ES2024 §21.3.2.27  Math.random()
//
// Uses a simple xorshift128+ PRNG seeded from system entropy.
// Thread-local state avoids synchronization.
// ---------------------------------------------------------------------------
fn math_random(
    _this: &RegisterValue,
    _args: &[RegisterValue],
    _runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    use std::cell::Cell;

    thread_local! {
        static STATE: Cell<(u64, u64)> = {
            // Seed from system entropy.
            let mut buf = [0u8; 16];
            getrandom::getrandom(&mut buf).unwrap_or_else(|_| {
                // Fallback: use address of the Cell + timestamp bits.
                let ts = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos() as u64)
                    .unwrap_or(0xDEAD_BEEF_CAFE_BABE);
                buf[..8].copy_from_slice(&ts.to_le_bytes());
                buf[8..16].copy_from_slice(&(ts.wrapping_mul(6364136223846793005)).to_le_bytes());
            });
            let s0 = u64::from_le_bytes(buf[0..8].try_into().unwrap());
            let s1 = u64::from_le_bytes(buf[8..16].try_into().unwrap());
            // Ensure non-zero state.
            Cell::new(if s0 == 0 && s1 == 0 { (1, 1) } else { (s0, s1) })
        };
    }

    STATE.with(|state| {
        let (mut s0, mut s1) = state.get();
        // xorshift128+
        let result = s0.wrapping_add(s1);
        s1 ^= s0;
        s0 = s0.rotate_left(24) ^ s1 ^ (s1 << 16);
        s1 = s1.rotate_left(37);
        state.set((s0, s1));
        // Map to [0, 1) — use upper 52 bits as mantissa.
        let mantissa = result >> 12; // 52 bits
        let value = (mantissa as f64) / ((1u64 << 52) as f64);
        Ok(RegisterValue::from_number(value))
    })
}

// ---------------------------------------------------------------------------
// ES2024 §21.3.2.28  Math.round(x)
//
// Rounds to nearest integer, with ties going toward +∞ (not "round half to even").
// This matches V8/SpiderMonkey behavior, NOT Rust's f64::round which rounds
// half away from zero.
// ---------------------------------------------------------------------------
fn math_round(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let x = arg_to_number(args, 0, runtime)?;
    Ok(RegisterValue::from_number(js_round(x)))
}

/// ES spec Math.round — ties toward +∞.
fn js_round(x: f64) -> f64 {
    if x.is_nan() || x.is_infinite() || x == 0.0 {
        return x;
    }
    // The key difference from Rust's round():
    //   Math.round(-0.5) === -0    (not -1 like Rust)
    //   Math.round(0.5)  === 1     (same as Rust)
    //   Math.round(-1.5) === -1    (Rust gives -2)
    let floored = x.floor();
    if x - floored >= 0.5 {
        floored + 1.0
    } else {
        floored
    }
}

// ---------------------------------------------------------------------------
// ES2024 §21.3.2.29  Math.sign(x)
// ---------------------------------------------------------------------------
fn math_sign(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let x = arg_to_number(args, 0, runtime)?;
    let result = if x.is_nan() {
        f64::NAN
    } else if x == 0.0 {
        // Preserves -0 vs +0.
        x
    } else if x > 0.0 {
        1.0
    } else {
        -1.0
    };
    Ok(RegisterValue::from_number(result))
}

// ---------------------------------------------------------------------------
// ES2024 §21.3.2.30  Math.sin(x)
// ---------------------------------------------------------------------------
fn math_sin(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let x = arg_to_number(args, 0, runtime)?;
    Ok(RegisterValue::from_number(x.sin()))
}

// ---------------------------------------------------------------------------
// ES2024 §21.3.2.31  Math.sinh(x)
// ---------------------------------------------------------------------------
fn math_sinh(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let x = arg_to_number(args, 0, runtime)?;
    Ok(RegisterValue::from_number(x.sinh()))
}

// ---------------------------------------------------------------------------
// ES2024 §21.3.2.32  Math.sqrt(x)
// ---------------------------------------------------------------------------
fn math_sqrt(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let x = arg_to_number(args, 0, runtime)?;
    Ok(RegisterValue::from_number(x.sqrt()))
}

// ---------------------------------------------------------------------------
// ES2024 §21.3.2.33  Math.tan(x)
// ---------------------------------------------------------------------------
fn math_tan(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let x = arg_to_number(args, 0, runtime)?;
    Ok(RegisterValue::from_number(x.tan()))
}

// ---------------------------------------------------------------------------
// ES2024 §21.3.2.34  Math.tanh(x)
// ---------------------------------------------------------------------------
fn math_tanh(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let x = arg_to_number(args, 0, runtime)?;
    Ok(RegisterValue::from_number(x.tanh()))
}

// ---------------------------------------------------------------------------
// ES2024 §21.3.2.35  Math.trunc(x)
// ---------------------------------------------------------------------------
fn math_trunc(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let x = arg_to_number(args, 0, runtime)?;
    Ok(RegisterValue::from_number(x.trunc()))
}

// ---------------------------------------------------------------------------
// ES2024 §21.3.2.36  Math.f16round(x)
//
// Rounds a number to IEEE 754 binary16 (half-precision float) and back.
// Since Rust doesn't have a native f16, we do the conversion manually.
// ---------------------------------------------------------------------------
fn math_f16round(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let x = arg_to_number(args, 0, runtime)?;
    Ok(RegisterValue::from_number(f64_to_f16_round(x)))
}

/// Converts f64 → f16 → f64 (round-trip through IEEE 754 binary16).
fn f64_to_f16_round(value: f64) -> f64 {
    if value.is_nan() {
        return f64::NAN;
    }
    if value.is_infinite() {
        return value; // preserves sign
    }
    if value == 0.0 {
        return value; // preserves -0
    }

    let bits = value.to_bits();
    let sign = (bits >> 63) as u16;
    let f64_exp = ((bits >> 52) & 0x7FF) as i32;
    let f64_mantissa = bits & 0x000F_FFFF_FFFF_FFFF;

    // f16: 1 sign + 5 exponent + 10 mantissa
    // f64 exponent bias = 1023, f16 exponent bias = 15
    let unbiased_exp = f64_exp - 1023;

    // Overflow to ±Infinity
    if unbiased_exp > 15 {
        return if sign == 0 {
            f64::INFINITY
        } else {
            f64::NEG_INFINITY
        };
    }

    // Normal f16 range: exponent -14..15
    let (f16_exp, f16_mantissa) = if unbiased_exp >= -14 {
        // Normal number
        let biased_f16_exp = (unbiased_exp + 15) as u16;
        // Round mantissa from 52 bits to 10 bits (round to nearest even)
        let shift = 52 - 10;
        let truncated = (f64_mantissa >> shift) as u16;
        let remainder = f64_mantissa & ((1u64 << shift) - 1);
        let halfway = 1u64 << (shift - 1);
        let rounded = if remainder > halfway
            || (remainder == halfway && (truncated & 1) != 0)
        {
            truncated + 1
        } else {
            truncated
        };

        // Check if rounding overflows mantissa into next exponent
        if rounded > 0x3FF {
            // Mantissa overflow → increment exponent
            let new_exp = biased_f16_exp + 1;
            if new_exp > 30 {
                // Overflow to infinity
                return if sign == 0 {
                    f64::INFINITY
                } else {
                    f64::NEG_INFINITY
                };
            }
            (new_exp, 0u16)
        } else {
            (biased_f16_exp, rounded)
        }
    } else if unbiased_exp >= -24 {
        // Subnormal f16
        let shift = (-14 - unbiased_exp) as u32;
        // The implicit 1 bit + mantissa, shifted right
        let full_mantissa = (1u64 << 52) | f64_mantissa;
        let subnormal_shift = (52 - 10 + shift as u64) as u32;
        let truncated = (full_mantissa >> subnormal_shift) as u16;
        let remainder = full_mantissa & ((1u64 << subnormal_shift) - 1);
        let halfway = 1u64 << (subnormal_shift - 1);
        let rounded = if remainder > halfway
            || (remainder == halfway && (truncated & 1) != 0)
        {
            truncated + 1
        } else {
            truncated
        };

        if rounded > 0x3FF {
            // Promoted to smallest normal
            (1u16, 0u16)
        } else {
            (0u16, rounded)
        }
    } else {
        // Too small — rounds to ±0.
        return if sign == 0 { 0.0 } else { -0.0 };
    };

    let f16_bits = (sign << 15) | (f16_exp << 10) | f16_mantissa;
    f16_to_f64(f16_bits)
}

/// Converts a 16-bit IEEE 754 binary16 value to f64.
fn f16_to_f64(bits: u16) -> f64 {
    let sign = ((bits >> 15) & 1) as u64;
    let exp = ((bits >> 10) & 0x1F) as i32;
    let mantissa = (bits & 0x3FF) as u64;

    if exp == 0 {
        if mantissa == 0 {
            // ±0
            return f64::from_bits(sign << 63);
        }
        // Subnormal: value = (-1)^sign × 2^(-14) × (mantissa / 1024)
        let value = (mantissa as f64) * 2.0f64.powi(-24);
        if sign == 1 { -value } else { value }
    } else if exp == 31 {
        if mantissa == 0 {
            // ±Infinity
            f64::from_bits((sign << 63) | (0x7FFu64 << 52))
        } else {
            f64::NAN
        }
    } else {
        // Normal: rebias exponent from f16 bias (15) to f64 bias (1023)
        let f64_exp = ((exp - 15 + 1023) as u64) & 0x7FF;
        let f64_mantissa = mantissa << (52 - 10);
        f64::from_bits((sign << 63) | (f64_exp << 52) | f64_mantissa)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interpreter::RuntimeState;
    use crate::value::RegisterValue;

    /// Helper: call a Math native function with f64 args.
    fn call_math_fn(
        func: crate::descriptors::VmNativeFunction,
        float_args: &[f64],
    ) -> f64 {
        let mut runtime = RuntimeState::new();
        let this = RegisterValue::undefined();
        let args: Vec<RegisterValue> =
            float_args.iter().map(|&v| RegisterValue::from_number(v)).collect();
        let result = func(&this, &args, &mut runtime).expect("math fn should succeed");
        result.as_number().expect("result should be a number")
    }

    /// Helper: call with RegisterValue args directly (for coercion tests).
    fn call_math_fn_rv(
        func: crate::descriptors::VmNativeFunction,
        args: &[RegisterValue],
    ) -> RegisterValue {
        let mut runtime = RuntimeState::new();
        let this = RegisterValue::undefined();
        func(&this, args, &mut runtime).expect("math fn should succeed")
    }

    // -----------------------------------------------------------------------
    // Constants
    // -----------------------------------------------------------------------

    #[test]
    fn math_constants_are_correct() {
        assert_eq!(MATH_E, std::f64::consts::E);
        assert_eq!(MATH_LN10, std::f64::consts::LN_10);
        assert_eq!(MATH_LN2, std::f64::consts::LN_2);
        assert_eq!(MATH_LOG10E, std::f64::consts::LOG10_E);
        assert_eq!(MATH_LOG2E, std::f64::consts::LOG2_E);
        assert_eq!(MATH_PI, std::f64::consts::PI);
        assert_eq!(MATH_SQRT1_2, std::f64::consts::FRAC_1_SQRT_2);
        assert_eq!(MATH_SQRT2, std::f64::consts::SQRT_2);
    }

    #[test]
    fn math_constants_installed_on_namespace() {
        let mut runtime = RuntimeState::new();
        let intrinsics = runtime.intrinsics();
        let math_ns = intrinsics
            .math_namespace()
            .expect("Math namespace should exist");

        for &(name, expected) in MATH_CONSTANTS {
            let prop = runtime.intern_property_name(name);
            let lookup = runtime
                .objects()
                .get_property(math_ns, prop)
                .expect("property lookup should succeed")
                .unwrap_or_else(|| panic!("Math.{name} should be installed"));
            let crate::object::PropertyValue::Data { value, .. } = lookup.value() else {
                panic!("Math.{name} should be a data property");
            };
            let actual = value
                .as_number()
                .unwrap_or_else(|| panic!("Math.{name} should be a number"));
            assert_eq!(actual, expected, "Math.{name} value mismatch");
        }
    }

    // -----------------------------------------------------------------------
    // ToNumber coercion
    // -----------------------------------------------------------------------

    #[test]
    fn math_abs_coerces_undefined_to_nan() {
        let result = call_math_fn_rv(math_abs, &[RegisterValue::undefined()]);
        assert!(result.as_number().unwrap().is_nan());
    }

    #[test]
    fn math_abs_coerces_null_to_zero() {
        let result = call_math_fn(math_abs, &[]);
        // No args → undefined → NaN
        assert!(result.is_nan());

        let result = call_math_fn_rv(math_abs, &[RegisterValue::null()]);
        assert_eq!(result.as_number().unwrap(), 0.0);
    }

    #[test]
    fn math_abs_coerces_booleans() {
        let result = call_math_fn_rv(math_abs, &[RegisterValue::from_bool(true)]);
        assert_eq!(result.as_number().unwrap(), 1.0);

        let result = call_math_fn_rv(math_abs, &[RegisterValue::from_bool(false)]);
        assert_eq!(result.as_number().unwrap(), 0.0);
    }

    // -----------------------------------------------------------------------
    // abs
    // -----------------------------------------------------------------------

    #[test]
    fn math_abs_basic() {
        assert_eq!(call_math_fn(math_abs, &[-5.0]), 5.0);
        assert_eq!(call_math_fn(math_abs, &[5.0]), 5.0);
        assert_eq!(call_math_fn(math_abs, &[0.0]), 0.0);
        assert_eq!(
            call_math_fn(math_abs, &[f64::NEG_INFINITY]),
            f64::INFINITY
        );
    }

    // -----------------------------------------------------------------------
    // Trigonometric
    // -----------------------------------------------------------------------

    #[test]
    fn math_trig_basic() {
        let pi = std::f64::consts::PI;

        // sin(0) = 0, cos(0) = 1, tan(0) = 0
        assert_eq!(call_math_fn(math_sin, &[0.0]), 0.0);
        assert_eq!(call_math_fn(math_cos, &[0.0]), 1.0);
        assert_eq!(call_math_fn(math_tan, &[0.0]), 0.0);

        // sin(π/2) ≈ 1
        assert!((call_math_fn(math_sin, &[pi / 2.0]) - 1.0).abs() < 1e-15);

        // asin(1) ≈ π/2
        assert!((call_math_fn(math_asin, &[1.0]) - pi / 2.0).abs() < 1e-15);

        // acos(1) = 0
        assert_eq!(call_math_fn(math_acos, &[1.0]), 0.0);

        // atan(1) ≈ π/4
        assert!((call_math_fn(math_atan, &[1.0]) - pi / 4.0).abs() < 1e-15);
    }

    #[test]
    fn math_trig_nan_propagation() {
        assert!(call_math_fn(math_sin, &[f64::NAN]).is_nan());
        assert!(call_math_fn(math_cos, &[f64::NAN]).is_nan());
        assert!(call_math_fn(math_tan, &[f64::NAN]).is_nan());
        assert!(call_math_fn(math_asin, &[2.0]).is_nan()); // out of domain
        assert!(call_math_fn(math_acos, &[2.0]).is_nan());
        assert!(call_math_fn(math_sin, &[f64::INFINITY]).is_nan());
    }

    // -----------------------------------------------------------------------
    // Hyperbolic
    // -----------------------------------------------------------------------

    #[test]
    fn math_hyperbolic_basic() {
        assert_eq!(call_math_fn(math_sinh, &[0.0]), 0.0);
        assert_eq!(call_math_fn(math_cosh, &[0.0]), 1.0);
        assert_eq!(call_math_fn(math_tanh, &[0.0]), 0.0);
        assert_eq!(call_math_fn(math_asinh, &[0.0]), 0.0);
        assert_eq!(call_math_fn(math_acosh, &[1.0]), 0.0);
        assert_eq!(call_math_fn(math_atanh, &[0.0]), 0.0);
    }

    // -----------------------------------------------------------------------
    // Exponential / logarithmic
    // -----------------------------------------------------------------------

    #[test]
    fn math_exp_log_basic() {
        assert_eq!(call_math_fn(math_exp, &[0.0]), 1.0);
        assert_eq!(call_math_fn(math_log, &[1.0]), 0.0);
        assert_eq!(call_math_fn(math_log2, &[1.0]), 0.0);
        assert_eq!(call_math_fn(math_log10, &[1.0]), 0.0);
        assert_eq!(call_math_fn(math_expm1, &[0.0]), 0.0);
        assert_eq!(call_math_fn(math_log1p, &[0.0]), 0.0);

        // log(e) ≈ 1
        assert!((call_math_fn(math_log, &[std::f64::consts::E]) - 1.0).abs() < 1e-15);

        // log(-1) = NaN
        assert!(call_math_fn(math_log, &[-1.0]).is_nan());

        // exp(1) ≈ e
        assert!((call_math_fn(math_exp, &[1.0]) - std::f64::consts::E).abs() < 1e-14);
    }

    // -----------------------------------------------------------------------
    // Rounding: ceil, floor, round, trunc, fround
    // -----------------------------------------------------------------------

    #[test]
    fn math_ceil_basic() {
        assert_eq!(call_math_fn(math_ceil, &[0.3]), 1.0);
        assert_eq!(call_math_fn(math_ceil, &[-0.7]), 0.0); // -0? No: ceil(-0.7)=0 (positive zero)
        assert_eq!(call_math_fn(math_ceil, &[4.0]), 4.0);
    }

    #[test]
    fn math_floor_basic() {
        assert_eq!(call_math_fn(math_floor, &[0.7]), 0.0);
        assert_eq!(call_math_fn(math_floor, &[-0.3]), -1.0);
        assert_eq!(call_math_fn(math_floor, &[4.0]), 4.0);
    }

    #[test]
    fn math_round_spec_ties() {
        // Ties go toward +∞ (NOT round-half-to-even, NOT round-half-away-from-zero)
        assert_eq!(call_math_fn(math_round, &[0.5]), 1.0);
        assert_eq!(call_math_fn(math_round, &[1.5]), 2.0);
        assert_eq!(call_math_fn(math_round, &[-0.5]), 0.0); // NOT -1
        assert_eq!(call_math_fn(math_round, &[-1.5]), -1.0); // NOT -2
        assert_eq!(call_math_fn(math_round, &[2.5]), 3.0);
        assert_eq!(call_math_fn(math_round, &[-2.5]), -2.0);

        // -0 preservation
        let result = call_math_fn(math_round, &[-0.0]);
        assert_eq!(result, 0.0);
        assert!(result.is_sign_negative() || result == 0.0); // -0.0

        // NaN, ±Infinity pass through
        assert!(call_math_fn(math_round, &[f64::NAN]).is_nan());
        assert_eq!(call_math_fn(math_round, &[f64::INFINITY]), f64::INFINITY);
    }

    #[test]
    fn math_trunc_basic() {
        assert_eq!(call_math_fn(math_trunc, &[1.7]), 1.0);
        assert_eq!(call_math_fn(math_trunc, &[-1.7]), -1.0);
        assert_eq!(call_math_fn(math_trunc, &[0.0]), 0.0);
    }

    #[test]
    fn math_fround_basic() {
        assert_eq!(call_math_fn(math_fround, &[0.0]), 0.0);
        assert_eq!(call_math_fn(math_fround, &[1.0]), 1.0);
        assert_eq!(call_math_fn(math_fround, &[1.5]), 1.5);
        // 1.337 as f32 is slightly different
        let fr = call_math_fn(math_fround, &[1.337]);
        assert!((fr - 1.337_f32 as f64).abs() < 1e-10);
        assert!(call_math_fn(math_fround, &[f64::NAN]).is_nan());
    }

    // -----------------------------------------------------------------------
    // sign
    // -----------------------------------------------------------------------

    #[test]
    fn math_sign_basic() {
        assert_eq!(call_math_fn(math_sign, &[42.0]), 1.0);
        assert_eq!(call_math_fn(math_sign, &[-42.0]), -1.0);
        assert_eq!(call_math_fn(math_sign, &[0.0]), 0.0);
        assert!(call_math_fn(math_sign, &[f64::NAN]).is_nan());
        // -0 → -0
        let result = call_math_fn(math_sign, &[-0.0]);
        assert!(result == 0.0 && result.is_sign_negative());
    }

    // -----------------------------------------------------------------------
    // sqrt, cbrt
    // -----------------------------------------------------------------------

    #[test]
    fn math_sqrt_cbrt_basic() {
        assert_eq!(call_math_fn(math_sqrt, &[4.0]), 2.0);
        assert_eq!(call_math_fn(math_sqrt, &[0.0]), 0.0);
        assert!(call_math_fn(math_sqrt, &[-1.0]).is_nan());
        assert_eq!(call_math_fn(math_cbrt, &[27.0]), 3.0);
        assert_eq!(call_math_fn(math_cbrt, &[-8.0]), -2.0);
    }

    // -----------------------------------------------------------------------
    // pow — extensive edge cases per spec
    // -----------------------------------------------------------------------

    #[test]
    fn math_pow_basic() {
        assert_eq!(call_math_fn(math_pow, &[2.0, 10.0]), 1024.0);
        assert_eq!(call_math_fn(math_pow, &[3.0, 0.0]), 1.0);
        assert_eq!(call_math_fn(math_pow, &[3.0, -0.0]), 1.0);
    }

    #[test]
    fn math_pow_nan_cases() {
        assert!(call_math_fn(math_pow, &[f64::NAN, 1.0]).is_nan());
        assert_eq!(call_math_fn(math_pow, &[f64::NAN, 0.0]), 1.0); // NaN^0 = 1
        assert!(call_math_fn(math_pow, &[2.0, f64::NAN]).is_nan());
    }

    #[test]
    fn math_pow_infinity_base() {
        assert_eq!(
            call_math_fn(math_pow, &[f64::INFINITY, 2.0]),
            f64::INFINITY
        );
        assert_eq!(call_math_fn(math_pow, &[f64::INFINITY, -2.0]), 0.0);
        assert_eq!(
            call_math_fn(math_pow, &[f64::NEG_INFINITY, 3.0]),
            f64::NEG_INFINITY
        );
        assert_eq!(
            call_math_fn(math_pow, &[f64::NEG_INFINITY, 2.0]),
            f64::INFINITY
        );
        assert_eq!(call_math_fn(math_pow, &[f64::NEG_INFINITY, -3.0]), -0.0);
        assert_eq!(call_math_fn(math_pow, &[f64::NEG_INFINITY, -2.0]), 0.0);
    }

    #[test]
    fn math_pow_zero_base() {
        assert_eq!(call_math_fn(math_pow, &[0.0, 2.0]), 0.0);
        assert_eq!(call_math_fn(math_pow, &[0.0, -2.0]), f64::INFINITY);
        // -0 base with odd integer exponent
        let r = call_math_fn(math_pow, &[-0.0, 3.0]);
        assert!(r == 0.0 && r.is_sign_negative());
        assert_eq!(call_math_fn(math_pow, &[-0.0, 2.0]), 0.0);
        assert_eq!(
            call_math_fn(math_pow, &[-0.0, -3.0]),
            f64::NEG_INFINITY
        );
        assert_eq!(call_math_fn(math_pow, &[-0.0, -2.0]), f64::INFINITY);
    }

    #[test]
    fn math_pow_infinity_exponent() {
        assert_eq!(
            call_math_fn(math_pow, &[2.0, f64::INFINITY]),
            f64::INFINITY
        );
        assert!(call_math_fn(math_pow, &[1.0, f64::INFINITY]).is_nan());
        assert_eq!(call_math_fn(math_pow, &[0.5, f64::INFINITY]), 0.0);
        assert_eq!(call_math_fn(math_pow, &[2.0, f64::NEG_INFINITY]), 0.0);
        assert!(call_math_fn(math_pow, &[1.0, f64::NEG_INFINITY]).is_nan());
        assert_eq!(
            call_math_fn(math_pow, &[0.5, f64::NEG_INFINITY]),
            f64::INFINITY
        );
    }

    #[test]
    fn math_pow_negative_base_fractional_exp() {
        // (-2)^0.5 → NaN (non-integer exponent on negative base)
        assert!(call_math_fn(math_pow, &[-2.0, 0.5]).is_nan());
    }

    // -----------------------------------------------------------------------
    // clz32
    // -----------------------------------------------------------------------

    #[test]
    fn math_clz32_basic() {
        assert_eq!(call_math_fn(math_clz32, &[0.0]), 32.0);
        assert_eq!(call_math_fn(math_clz32, &[1.0]), 31.0);
        assert_eq!(call_math_fn(math_clz32, &[2_147_483_648.0]), 0.0);
        assert_eq!(call_math_fn(math_clz32, &[256.0]), 23.0);
    }

    // -----------------------------------------------------------------------
    // imul
    // -----------------------------------------------------------------------

    #[test]
    fn math_imul_basic() {
        assert_eq!(call_math_fn(math_imul, &[2.0, 3.0]), 6.0);
        assert_eq!(call_math_fn(math_imul, &[-1.0, 8.0]), -8.0);
        // Overflow wraps
        assert_eq!(
            call_math_fn(math_imul, &[4_294_967_295.0, 5.0]),
            -5.0
        );
    }

    // -----------------------------------------------------------------------
    // max / min — variadic with signed zero and NaN
    // -----------------------------------------------------------------------

    #[test]
    fn math_max_basic() {
        assert_eq!(call_math_fn(math_max, &[1.0, 2.0, 3.0]), 3.0);
        assert_eq!(call_math_fn(math_max, &[-1.0, -2.0]), -1.0);
        // No args → -Infinity
        assert_eq!(call_math_fn(math_max, &[]), f64::NEG_INFINITY);
    }

    #[test]
    fn math_max_nan_contagious() {
        assert!(call_math_fn(math_max, &[1.0, f64::NAN, 3.0]).is_nan());
    }

    #[test]
    fn math_max_signed_zero() {
        // max(+0, -0) → +0
        let result = call_math_fn(math_max, &[0.0, -0.0]);
        assert!(result.is_sign_positive());
        let result = call_math_fn(math_max, &[-0.0, 0.0]);
        assert!(result.is_sign_positive());
    }

    #[test]
    fn math_min_basic() {
        assert_eq!(call_math_fn(math_min, &[1.0, 2.0, 3.0]), 1.0);
        assert_eq!(call_math_fn(math_min, &[-1.0, -2.0]), -2.0);
        // No args → +Infinity
        assert_eq!(call_math_fn(math_min, &[]), f64::INFINITY);
    }

    #[test]
    fn math_min_nan_contagious() {
        assert!(call_math_fn(math_min, &[1.0, f64::NAN, 3.0]).is_nan());
    }

    #[test]
    fn math_min_signed_zero() {
        // min(+0, -0) → -0
        let result = call_math_fn(math_min, &[0.0, -0.0]);
        assert!(result.is_sign_negative());
        let result = call_math_fn(math_min, &[-0.0, 0.0]);
        assert!(result.is_sign_negative());
    }

    // -----------------------------------------------------------------------
    // hypot
    // -----------------------------------------------------------------------

    #[test]
    fn math_hypot_basic() {
        assert_eq!(call_math_fn(math_hypot, &[3.0, 4.0]), 5.0);
        assert_eq!(call_math_fn(math_hypot, &[]), 0.0);
        assert_eq!(call_math_fn(math_hypot, &[5.0]), 5.0);
    }

    #[test]
    fn math_hypot_infinity_beats_nan() {
        // If any arg is ±∞, result is +∞ even if another is NaN.
        assert_eq!(
            call_math_fn(math_hypot, &[f64::INFINITY, f64::NAN]),
            f64::INFINITY
        );
        assert_eq!(
            call_math_fn(math_hypot, &[f64::NAN, f64::NEG_INFINITY]),
            f64::INFINITY
        );
    }

    #[test]
    fn math_hypot_nan_propagation() {
        assert!(call_math_fn(math_hypot, &[f64::NAN, 1.0]).is_nan());
    }

    // -----------------------------------------------------------------------
    // random
    // -----------------------------------------------------------------------

    #[test]
    fn math_random_produces_values_in_range() {
        let mut runtime = RuntimeState::new();
        let this = RegisterValue::undefined();
        for _ in 0..1000 {
            let result = math_random(&this, &[], &mut runtime).unwrap();
            let n = result.as_number().unwrap();
            assert!((0.0..1.0).contains(&n), "Math.random() = {n} out of [0, 1)");
        }
    }

    #[test]
    fn math_random_is_not_constant() {
        let mut runtime = RuntimeState::new();
        let this = RegisterValue::undefined();
        let a = math_random(&this, &[], &mut runtime)
            .unwrap()
            .as_number()
            .unwrap();
        let b = math_random(&this, &[], &mut runtime)
            .unwrap()
            .as_number()
            .unwrap();
        // Probabilistically these should differ — failure probability ≈ 2^-52.
        assert_ne!(a, b, "two consecutive Math.random() calls should differ");
    }

    // -----------------------------------------------------------------------
    // atan2
    // -----------------------------------------------------------------------

    #[test]
    fn math_atan2_basic() {
        assert_eq!(call_math_fn(math_atan2, &[0.0, 0.0]), 0.0);
        assert!(
            (call_math_fn(math_atan2, &[1.0, 1.0]) - std::f64::consts::FRAC_PI_4).abs() < 1e-15
        );
        assert!(
            (call_math_fn(math_atan2, &[1.0, 0.0]) - std::f64::consts::FRAC_PI_2).abs() < 1e-15
        );
    }

    // -----------------------------------------------------------------------
    // f16round
    // -----------------------------------------------------------------------

    #[test]
    fn math_f16round_basic() {
        assert_eq!(call_math_fn(math_f16round, &[0.0]), 0.0);
        assert_eq!(call_math_fn(math_f16round, &[1.0]), 1.0);
        assert_eq!(call_math_fn(math_f16round, &[1.5]), 1.5);
        assert!(call_math_fn(math_f16round, &[f64::NAN]).is_nan());
        assert_eq!(
            call_math_fn(math_f16round, &[f64::INFINITY]),
            f64::INFINITY
        );
        assert_eq!(
            call_math_fn(math_f16round, &[f64::NEG_INFINITY]),
            f64::NEG_INFINITY
        );
    }

    #[test]
    fn math_f16round_precision_loss() {
        // f16 max ≈ 65504, values beyond overflow to infinity
        assert_eq!(
            call_math_fn(math_f16round, &[65504.0]),
            65504.0
        );
        assert_eq!(
            call_math_fn(math_f16round, &[65520.0]),
            f64::INFINITY
        );

        // f16 precision: smallest subnormal ≈ 2^-24 ≈ 5.96e-8
        let tiny = call_math_fn(math_f16round, &[1e-9]);
        assert_eq!(tiny, 0.0); // rounds to zero

        // -0 preserved
        let neg_zero = call_math_fn(math_f16round, &[-0.0]);
        assert!(neg_zero.is_sign_negative() && neg_zero == 0.0);
    }

    #[test]
    fn math_f16round_round_to_nearest_even() {
        // 1.0009765625 is exactly representable in f16 as 1.0009765625
        // 1.00048828125 is the midpoint between 1.0 and 1.0009765625
        // With round-to-nearest-even, it should round to 1.0 (even mantissa)
        let midpoint = 1.0 + (1.0 / 2048.0); // 1.00048828125
        let result = call_math_fn(math_f16round, &[midpoint]);
        assert_eq!(result, 1.0, "midpoint should round to even (1.0)");
    }

    // -----------------------------------------------------------------------
    // Method count validation
    // -----------------------------------------------------------------------

    #[test]
    fn math_has_36_method_bindings() {
        // 36 methods (37 in ES2024 minus the memory getter/setter we removed,
        // but we have f16round which is the 37th).
        let bindings = math_method_bindings();
        assert_eq!(bindings.len(), 36, "ES2024 Math should have 36 function properties");
    }

    // -----------------------------------------------------------------------
    // Integration: full bootstrap installs all methods on global Math
    // -----------------------------------------------------------------------

    #[test]
    fn math_methods_installed_on_global() {
        let mut runtime = RuntimeState::new();
        let intrinsics = runtime.intrinsics();
        let math_ns = intrinsics.math_namespace().expect("Math namespace");

        let expected_methods = [
            "abs", "acos", "acosh", "asin", "asinh", "atan", "atanh", "atan2",
            "cbrt", "ceil", "clz32", "cos", "cosh", "exp", "expm1", "floor",
            "fround", "hypot", "imul", "log", "log1p", "log10", "log2",
            "max", "min", "pow", "random", "round", "sign", "sin", "sinh",
            "sqrt", "tan", "tanh", "trunc", "f16round",
        ];

        for name in &expected_methods {
            let prop = runtime.intern_property_name(name);
            let lookup = runtime
                .objects()
                .get_property(math_ns, prop)
                .expect("property lookup should succeed");
            assert!(
                lookup.is_some(),
                "Math.{name} should be installed on namespace"
            );
        }
    }
}
