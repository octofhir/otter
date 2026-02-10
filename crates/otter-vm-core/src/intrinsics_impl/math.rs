//! Math namespace initialization
//!
//! Creates the Math global namespace object with:
//! - 8 Math constants (E, LN10, LN2, LOG10E, LOG2E, PI, SQRT1_2, SQRT2)
//! - 36+ Math static methods (ES2026)
//!
//! All methods are `#[dive]` functions with automatic type conversion.

use crate::context::NativeContext;
use crate::error::VmError;
use crate::gc::GcRef;
use crate::memory::MemoryManager;
use crate::object::{JsObject, PropertyAttributes, PropertyDescriptor, PropertyKey};
use crate::string::JsString;
use crate::value::Value;
use otter_macros::dive;
use std::sync::Arc;

// ============================================================================
// Helper
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

/// ES2023 §7.1.6 ToInt32
fn to_int32(n: f64) -> i32 {
    if n.is_nan() || n.is_infinite() || n == 0.0 {
        return 0;
    }
    let i = n.trunc() as i64;
    (i as u32) as i32
}

/// ES2023 §7.1.7 ToUint32
fn to_uint32(n: f64) -> u32 {
    if n.is_nan() || n.is_infinite() || n == 0.0 {
        return 0;
    }
    let i = n.trunc() as i64;
    i as u32
}

// ============================================================================
// Single-argument f64 → f64 methods
// ============================================================================

#[dive(name = "abs")]
fn math_abs(x: f64) -> f64 { x.abs() }

#[dive(name = "ceil")]
fn math_ceil(x: f64) -> f64 { x.ceil() }

#[dive(name = "floor")]
fn math_floor(x: f64) -> f64 { x.floor() }

#[dive(name = "round", length = 1)]
fn math_round(args: &[Value], _ncx: &mut NativeContext) -> Result<Value, VmError> {
    let x = to_number(args.get(0).unwrap_or(&Value::undefined()));
    if x.is_nan() || x.is_infinite() || x == 0.0 {
        return Ok(Value::number(x));
    }
    // If x is in [-0.5, 0), result is -0
    if x >= -0.5 && x < 0.0 {
        return Ok(Value::number(-0.0));
    }
    // Use floor + comparison to avoid precision loss from adding 0.5
    let f = x.floor();
    let result = if x - f >= 0.5 { f + 1.0 } else { f };
    Ok(Value::number(result))
}

#[dive(name = "trunc")]
fn math_trunc(x: f64) -> f64 { x.trunc() }

#[dive(name = "sqrt")]
fn math_sqrt(x: f64) -> f64 { x.sqrt() }

#[dive(name = "cbrt")]
fn math_cbrt(x: f64) -> f64 { x.cbrt() }

#[dive(name = "exp")]
fn math_exp(x: f64) -> f64 { x.exp() }

#[dive(name = "expm1")]
fn math_expm1(x: f64) -> f64 { x.exp_m1() }

#[dive(name = "log")]
fn math_log(x: f64) -> f64 { x.ln() }

#[dive(name = "log1p")]
fn math_log1p(x: f64) -> f64 { x.ln_1p() }

#[dive(name = "log2")]
fn math_log2(x: f64) -> f64 { x.log2() }

#[dive(name = "log10")]
fn math_log10(x: f64) -> f64 { x.log10() }

#[dive(name = "sin")]
fn math_sin(x: f64) -> f64 { x.sin() }

#[dive(name = "cos")]
fn math_cos(x: f64) -> f64 { x.cos() }

#[dive(name = "tan")]
fn math_tan(x: f64) -> f64 { x.tan() }

#[dive(name = "asin")]
fn math_asin(x: f64) -> f64 { x.asin() }

#[dive(name = "acos")]
fn math_acos(x: f64) -> f64 { x.acos() }

#[dive(name = "atan")]
fn math_atan(x: f64) -> f64 { x.atan() }

#[dive(name = "sinh")]
fn math_sinh(x: f64) -> f64 { x.sinh() }

#[dive(name = "cosh")]
fn math_cosh(x: f64) -> f64 { x.cosh() }

#[dive(name = "tanh")]
fn math_tanh(x: f64) -> f64 { x.tanh() }

#[dive(name = "asinh")]
fn math_asinh(x: f64) -> f64 { x.asinh() }

#[dive(name = "acosh")]
fn math_acosh(x: f64) -> f64 { x.acosh() }

#[dive(name = "atanh")]
fn math_atanh(x: f64) -> f64 { x.atanh() }

// ============================================================================
// Two-argument methods
// ============================================================================

#[dive(name = "pow", length = 2)]
fn math_pow(args: &[Value], _ncx: &mut NativeContext) -> Result<Value, VmError> {
    let base = to_number(args.get(0).unwrap_or(&Value::undefined()));
    let exp = to_number(args.get(1).unwrap_or(&Value::undefined()));
    let result = js_pow(base, exp);
    Ok(Value::number(result))
}

/// Spec-compliant Math.pow (ES2023 §6.1.6.1.3)
fn js_pow(base: f64, exp: f64) -> f64 {
    // 1. If exponent is NaN, return NaN
    if exp.is_nan() {
        return f64::NAN;
    }
    // 2. If exponent is +0 or -0, return 1
    if exp == 0.0 {
        return 1.0;
    }
    // 3. If base is NaN, return NaN
    if base.is_nan() {
        return f64::NAN;
    }
    // 4. If base is +Infinity
    if base == f64::INFINITY {
        return if exp > 0.0 { f64::INFINITY } else { 0.0 };
    }
    // 5. If base is -Infinity
    if base == f64::NEG_INFINITY {
        if exp > 0.0 {
            return if is_odd_integer(exp) { f64::NEG_INFINITY } else { f64::INFINITY };
        }
        return if is_odd_integer(exp) { -0.0 } else { 0.0 };
    }
    // 6. If base is +0
    if base == 0.0 && base.is_sign_positive() {
        return if exp > 0.0 { 0.0 } else { f64::INFINITY };
    }
    // 7. If base is -0
    if base == 0.0 && base.is_sign_negative() {
        if exp > 0.0 {
            return if is_odd_integer(exp) { -0.0 } else { 0.0 };
        }
        return if is_odd_integer(exp) { f64::NEG_INFINITY } else { f64::INFINITY };
    }
    // 8. If base < 0 and base is finite, and exponent is finite and not integer
    if base < 0.0 && base.is_finite() && exp.is_finite() && exp.fract() != 0.0 {
        return f64::NAN;
    }
    // 9. If abs(base) == 1 and exponent is infinite
    if base.abs() == 1.0 && exp.is_infinite() {
        return f64::NAN;
    }
    // 10. If abs(base) > 1
    if base.abs() > 1.0 {
        return if exp == f64::INFINITY { f64::INFINITY }
        else if exp == f64::NEG_INFINITY { 0.0 }
        else { base.powf(exp) };
    }
    // 11. If abs(base) < 1
    if base.abs() < 1.0 {
        return if exp == f64::INFINITY { 0.0 }
        else if exp == f64::NEG_INFINITY { f64::INFINITY }
        else { base.powf(exp) };
    }
    base.powf(exp)
}

fn is_odd_integer(n: f64) -> bool {
    if !n.is_finite() || n.fract() != 0.0 {
        return false;
    }
    // For |n| >= 2^53, all representable f64 integers are even
    let abs_n = n.abs();
    if abs_n >= 9007199254740992.0 {
        return false;
    }
    (n as i64) % 2 != 0
}

#[dive(name = "atan2")]
fn math_atan2(y: f64, x: f64) -> f64 { y.atan2(x) }

#[dive(name = "imul", length = 2)]
fn math_imul(args: &[Value], _ncx: &mut NativeContext) -> Result<Value, VmError> {
    let a = to_int32(to_number(args.get(0).unwrap_or(&Value::undefined())));
    let b = to_int32(to_number(args.get(1).unwrap_or(&Value::undefined())));
    Ok(Value::number(a.wrapping_mul(b) as f64))
}

// ============================================================================
// Special single-argument methods (non-trivial conversion)
// ============================================================================

#[dive(name = "sign", length = 1)]
fn math_sign(args: &[Value], _ncx: &mut NativeContext) -> Result<Value, VmError> {
    let x = to_number(args.get(0).unwrap_or(&Value::undefined()));
    let result = if x.is_nan() {
        f64::NAN
    } else if x == 0.0 || x == -0.0 {
        x // preserve sign of zero
    } else if x > 0.0 {
        1.0
    } else {
        -1.0
    };
    Ok(Value::number(result))
}

#[dive(name = "clz32", length = 1)]
fn math_clz32(args: &[Value], _ncx: &mut NativeContext) -> Result<Value, VmError> {
    let x = to_number(args.get(0).unwrap_or(&Value::undefined()));
    let val = to_uint32(x);
    Ok(Value::number(val.leading_zeros() as f64))
}

#[dive(name = "fround", length = 1)]
fn math_fround(args: &[Value], _ncx: &mut NativeContext) -> Result<Value, VmError> {
    let x = to_number(args.get(0).unwrap_or(&Value::undefined()));
    Ok(Value::number((x as f32) as f64))
}

#[dive(name = "f16round", length = 1)]
fn math_f16round(args: &[Value], _ncx: &mut NativeContext) -> Result<Value, VmError> {
    let x = to_number(args.get(0).unwrap_or(&Value::undefined()));
    let f16_val = half::f16::from_f64(x);
    Ok(Value::number(f16_val.to_f64()))
}

// ============================================================================
// Varargs methods
// ============================================================================

#[dive(name = "max", length = 2)]
fn math_max(args: &[Value], ncx: &mut NativeContext) -> Result<Value, VmError> {
    if args.is_empty() {
        return Ok(Value::number(f64::NEG_INFINITY));
    }
    // Step 2: coerce ALL args to numbers first (ToNumber can throw)
    let mut coerced = Vec::with_capacity(args.len());
    for arg in args {
        coerced.push(ncx.to_number_value(arg)?);
    }
    let mut max = f64::NEG_INFINITY;
    for n in coerced {
        if n.is_nan() {
            return Ok(Value::number(f64::NAN));
        }
        // +0 > -0 per spec
        if n > max || (n == 0.0 && max == 0.0 && max.is_sign_negative() && n.is_sign_positive()) {
            max = n;
        }
    }
    Ok(Value::number(max))
}

#[dive(name = "min", length = 2)]
fn math_min(args: &[Value], ncx: &mut NativeContext) -> Result<Value, VmError> {
    if args.is_empty() {
        return Ok(Value::number(f64::INFINITY));
    }
    // Step 2: coerce ALL args to numbers first (ToNumber can throw)
    let mut coerced = Vec::with_capacity(args.len());
    for arg in args {
        coerced.push(ncx.to_number_value(arg)?);
    }
    let mut min = f64::INFINITY;
    for n in coerced {
        if n.is_nan() {
            return Ok(Value::number(f64::NAN));
        }
        // -0 < +0 per spec
        if n < min || (n == 0.0 && min == 0.0 && n.is_sign_negative() && min.is_sign_positive()) {
            min = n;
        }
    }
    Ok(Value::number(min))
}

#[dive(name = "hypot", length = 2)]
fn math_hypot(args: &[Value], ncx: &mut NativeContext) -> Result<Value, VmError> {
    if args.is_empty() {
        return Ok(Value::number(0.0));
    }
    // Step 2: coerce ALL args to numbers first (ToNumber can throw)
    let mut coerced = Vec::with_capacity(args.len());
    for arg in args {
        coerced.push(ncx.to_number_value(arg)?);
    }
    let mut has_inf = false;
    let mut has_nan = false;
    let mut sum = 0.0;
    for n in coerced {
        if n.is_infinite() {
            has_inf = true;
        } else if n.is_nan() {
            has_nan = true;
        } else {
            sum += n * n;
        }
    }
    // Per spec: if any value is ±Infinity, return +Infinity (even if NaN present)
    if has_inf {
        return Ok(Value::number(f64::INFINITY));
    }
    if has_nan {
        return Ok(Value::number(f64::NAN));
    }
    Ok(Value::number(sum.sqrt()))
}

#[dive(name = "random", length = 0)]
fn math_random(_args: &[Value], _ncx: &mut NativeContext) -> Result<Value, VmError> {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u64(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64,
    );
    let hash = hasher.finish();
    let rand = (hash as f64) / (u64::MAX as f64);
    Ok(Value::number(rand))
}

#[dive(name = "sumPrecise", length = 1)]
fn math_sum_precise(args: &[Value], ncx: &mut NativeContext) -> Result<Value, VmError> {
    let undefined = Value::undefined();
    let iterable = args.get(0).unwrap_or(&undefined);

    // Per spec: TypeError if not iterable
    let iter_sym = crate::intrinsics::well_known::iterator_symbol();
    let iter_key = PropertyKey::Symbol(iter_sym);
    let iter_obj = iterable.as_object().or_else(|| iterable.as_array());
    let iter_fn = if let Some(obj) = &iter_obj {
        obj.get(&iter_key).unwrap_or(Value::undefined())
    } else {
        Value::undefined()
    };
    if !iter_fn.is_callable() {
        return Err(VmError::type_error("object is not iterable"));
    }

    // Call @@iterator to get iterator
    let iterator = ncx.call_function(&iter_fn, iterable.clone(), &[])?;
    let iterator_obj = iterator
        .as_object()
        .ok_or_else(|| VmError::type_error("Iterator result is not an object"))?;
    let next_fn = iterator_obj
        .get(&PropertyKey::string("next"))
        .unwrap_or(Value::undefined());

    // Collect all values (per spec, iterate fully before computing)
    let mut values: Vec<f64> = Vec::new();
    loop {
        let result = ncx.call_function(&next_fn, iterator.clone(), &[])?;
        let result_obj = result
            .as_object()
            .ok_or_else(|| VmError::type_error("Iterator result is not an object"))?;
        let done = result_obj
            .get(&PropertyKey::string("done"))
            .unwrap_or(Value::undefined());
        if done.to_boolean() {
            break;
        }
        let value = result_obj
            .get(&PropertyKey::string("value"))
            .unwrap_or(Value::undefined());

        // Per spec: TypeError if value is not a Number (with IteratorClose)
        let n = if let Some(n) = value.as_number() {
            n
        } else if let Some(n) = value.as_int32() {
            n as f64
        } else {
            // IteratorClose: call iterator.return() if it exists
            if let Some(return_fn) = iterator_obj.get(&PropertyKey::string("return")) {
                if return_fn.is_callable() || return_fn.is_native_function() {
                    let _ = ncx.call_function(&return_fn, iterator.clone(), &[]);
                }
            }
            return Err(VmError::type_error(
                "Math.sumPrecise: values must be numbers",
            ));
        };
        values.push(n);
    }

    if values.is_empty() {
        return Ok(Value::number(-0.0));
    }

    // Check for Infinity/NaN
    let mut has_pos_inf = false;
    let mut has_neg_inf = false;
    for &n in &values {
        if n.is_nan() {
            return Ok(Value::number(f64::NAN));
        }
        if n == f64::INFINITY {
            has_pos_inf = true;
        } else if n == f64::NEG_INFINITY {
            has_neg_inf = true;
        }
    }
    if has_pos_inf && has_neg_inf {
        return Ok(Value::number(f64::NAN));
    }
    if has_pos_inf {
        return Ok(Value::number(f64::INFINITY));
    }
    if has_neg_inf {
        return Ok(Value::number(f64::NEG_INFINITY));
    }

    // Track -0 for the empty/all-zeros case
    let mut all_negative_zero = true;
    for &value in &values {
        if value != 0.0 || value.is_sign_positive() {
            all_negative_zero = false;
            break;
        }
    }
    if all_negative_zero {
        return Ok(Value::number(-0.0));
    }

    // Shewchuk's exact summation algorithm (like Python's math.fsum)
    let sum = shewchuk_sum(&values);
    Ok(Value::number(sum))
}

/// Exact floating-point summation using Shewchuk's algorithm.
/// Handles the near-MAX_VALUE overflow boundary correctly.
fn shewchuk_sum(values: &[f64]) -> f64 {
    // Phase 1: Accumulate non-overlapping partials via two-sum.
    let mut partials: Vec<f64> = Vec::new();

    for &x in values {
        let mut hi = x;
        let mut new_partials: Vec<f64> = Vec::new();

        for &p in &partials {
            let (mut a, mut b) = (hi, p);
            if a.abs() < b.abs() {
                std::mem::swap(&mut a, &mut b);
            }
            let sum = a + b;
            if sum.is_infinite() && a.is_finite() && b.is_finite() {
                // Overflow: keep separate for potential cancellation
                new_partials.push(b);
                hi = a;
            } else {
                let lo = b - (sum - a);
                if lo != 0.0 {
                    new_partials.push(lo);
                }
                hi = sum;
            }
        }

        new_partials.push(hi);
        partials = new_partials;
    }

    if partials.is_empty() {
        return 0.0;
    }

    let n = partials.len();
    if n == 1 {
        return partials[0];
    }

    // Phase 2: Final summation with overflow handling.
    //
    // Standard CPython fsum final loop (top-to-bottom with correction),
    // but with special overflow handling: when the loop produces Infinity
    // from finite partials, we determine the correct rounding by examining
    // the partial that caused the overflow.
    let mut idx = n;
    let mut hi = partials[idx - 1];
    idx -= 1;
    let mut lo = 0.0f64;
    while idx > 0 {
        let x = hi;
        let y = partials[idx - 1];
        idx -= 1;
        let new_hi = x + y;

        // Overflow check: if x + y overflows but both are finite,
        // the exact sum might still round to MAX_VALUE.
        if new_hi.is_infinite() && x.is_finite() && y.is_finite() {
            // Overflow in final summation. The exact sum might still round
            // to MAX_VALUE. Use quarter-scale Shewchuk to avoid overflow.
            let quarter_values: Vec<f64> = partials.iter().map(|&p| p * 0.25).collect();
            let mut qp: Vec<f64> = Vec::new();
            for &qx in &quarter_values {
                let mut qhi = qx;
                let mut new_qp: Vec<f64> = Vec::new();
                for &qpp in &qp {
                    let (mut qa, mut qb) = (qhi, qpp);
                    if qa.abs() < qb.abs() {
                        std::mem::swap(&mut qa, &mut qb);
                    }
                    let qsum = qa + qb;
                    let qlo = qb - (qsum - qa);
                    if qlo != 0.0 {
                        new_qp.push(qlo);
                    }
                    qhi = qsum;
                }
                new_qp.push(qhi);
                qp = new_qp;
            }
            // Sum quarter-scale partials with CPython correction
            let qn = qp.len();
            let mut qhi = qp[qn - 1];
            let mut qidx = qn - 1;
            let mut qlo = 0.0f64;
            while qidx > 0 {
                let qx2 = qhi;
                let qy2 = qp[qidx - 1];
                qidx -= 1;
                qhi = qx2 + qy2;
                let qyr = qhi - qx2;
                qlo = qy2 - qyr;
                if qlo != 0.0 {
                    break;
                }
            }
            if qidx > 0 && ((qlo < 0.0 && qp[qidx - 1] < 0.0) ||
                              (qlo > 0.0 && qp[qidx - 1] > 0.0)) {
                let qy3 = qlo * 2.0;
                let qx3 = qhi + qy3;
                let qyr3 = qx3 - qhi;
                if qy3 == qyr3 {
                    qhi = qx3;
                }
            }
            // Scale back: multiply by 4. This may overflow at the
            // boundary, which is correct IEEE 754 rounding.
            let doubled = qhi * 2.0;
            return doubled * 2.0;
        }

        hi = new_hi;
        let yr = hi - x;
        lo = y - yr;
        if lo != 0.0 {
            break;
        }
    }
    if idx > 0 && ((lo < 0.0 && partials[idx - 1] < 0.0) ||
                    (lo > 0.0 && partials[idx - 1] > 0.0)) {
        let y = lo * 2.0;
        let x = hi + y;
        let yr = x - hi;
        if y == yr {
            hi = x;
        }
    }
    hi
}

// ============================================================================
// Install
// ============================================================================

/// Create and install Math namespace on global object.
pub fn install_math_namespace(
    global: GcRef<JsObject>,
    mm: &Arc<MemoryManager>,
    object_prototype: GcRef<JsObject>,
) {
    let math_obj = GcRef::new(JsObject::new(Value::object(object_prototype), mm.clone()));

    // Math Constants (ES2026 §21.3.1)
    let const_attrs = PropertyAttributes::permanent();
    math_obj.define_property(
        PropertyKey::string("E"),
        PropertyDescriptor::data_with_attrs(Value::number(std::f64::consts::E), const_attrs),
    );
    math_obj.define_property(
        PropertyKey::string("LN10"),
        PropertyDescriptor::data_with_attrs(Value::number(std::f64::consts::LN_10), const_attrs),
    );
    math_obj.define_property(
        PropertyKey::string("LN2"),
        PropertyDescriptor::data_with_attrs(Value::number(std::f64::consts::LN_2), const_attrs),
    );
    math_obj.define_property(
        PropertyKey::string("LOG10E"),
        PropertyDescriptor::data_with_attrs(Value::number(std::f64::consts::LOG10_E), const_attrs),
    );
    math_obj.define_property(
        PropertyKey::string("LOG2E"),
        PropertyDescriptor::data_with_attrs(Value::number(std::f64::consts::LOG2_E), const_attrs),
    );
    math_obj.define_property(
        PropertyKey::string("PI"),
        PropertyDescriptor::data_with_attrs(Value::number(std::f64::consts::PI), const_attrs),
    );
    math_obj.define_property(
        PropertyKey::string("SQRT1_2"),
        PropertyDescriptor::data_with_attrs(
            Value::number(std::f64::consts::FRAC_1_SQRT_2),
            const_attrs,
        ),
    );
    math_obj.define_property(
        PropertyKey::string("SQRT2"),
        PropertyDescriptor::data_with_attrs(Value::number(std::f64::consts::SQRT_2), const_attrs),
    );

    // Math Methods (ES2026 §21.3.2)
    // Wire all #[dive] functions via their cached NativeFn arcs
    let methods: &[(&str, crate::value::NativeFn, u32)] = &[
        math_abs_decl(),
        math_ceil_decl(),
        math_floor_decl(),
        math_round_decl(),
        math_trunc_decl(),
        math_sign_decl(),
        math_max_decl(),
        math_min_decl(),
        math_sqrt_decl(),
        math_cbrt_decl(),
        math_pow_decl(),
        math_hypot_decl(),
        math_exp_decl(),
        math_expm1_decl(),
        math_log_decl(),
        math_log1p_decl(),
        math_log2_decl(),
        math_log10_decl(),
        math_sin_decl(),
        math_cos_decl(),
        math_tan_decl(),
        math_asin_decl(),
        math_acos_decl(),
        math_atan_decl(),
        math_atan2_decl(),
        math_sinh_decl(),
        math_cosh_decl(),
        math_tanh_decl(),
        math_asinh_decl(),
        math_acosh_decl(),
        math_atanh_decl(),
        math_clz32_decl(),
        math_imul_decl(),
        math_fround_decl(),
        math_f16round_decl(),
        math_random_decl(),
        math_sum_precise_decl(),
    ];

    for (name, native_fn, length) in methods {
        let fn_val = Value::native_function_from_decl(name, native_fn.clone(), *length, mm.clone());
        math_obj.define_property(
            PropertyKey::string(name),
            PropertyDescriptor::builtin_method(fn_val),
        );
    }

    // @@toStringTag = "Math" (non-writable, non-enumerable, configurable)
    math_obj.define_property(
        PropertyKey::Symbol(crate::intrinsics::well_known::to_string_tag_symbol()),
        PropertyDescriptor::data_with_attrs(
            Value::string(JsString::intern("Math")),
            PropertyAttributes {
                writable: false,
                enumerable: false,
                configurable: true,
            },
        ),
    );

    // Math is non-enumerable, writable, configurable on global
    global.define_property(
        PropertyKey::string("Math"),
        PropertyDescriptor::data_with_attrs(
            Value::object(math_obj),
            PropertyAttributes {
                writable: true,
                enumerable: false,
                configurable: true,
            },
        ),
    );
}
