//! Math namespace initialization
//!
//! Creates the Math global namespace object with:
//! - 8 Math constants (E, LN10, LN2, LOG10E, LOG2E, PI, SQRT1_2, SQRT2)
//! - 36 Math static methods (all ES2026 methods except sumPrecise)
//!
//! All Math methods are already implemented in `otter-vm-builtins/src/math.rs`
//! and registered as global `__Math_*` operations. This module creates the
//! Math namespace object and wires those operations as properties.
//!
//! ## ES2026 Compliance
//!
//! **Constants**: All constants have property attributes:
//! - `writable: false` (immutable mathematical constants)
//! - `enumerable: false` (don't pollute Object.keys)
//! - `configurable: false` (cannot be deleted or redefined)
//!
//! **Methods**: All methods have property attributes:
//! - `writable: true` (allow polyfills/testing overrides)
//! - `enumerable: false` (keep namespace clean)
//! - `configurable: true` (allow runtime modifications)
//!
//! ## Missing from ES2026
//!
//! - **Math.sumPrecise**: Not yet implemented in otter-vm-builtins.
//!   This method uses enhanced floating-point precision (Kahan summation)
//!   to sum an iterable. Will be added in a future update.

use crate::gc::GcRef;
use crate::memory::MemoryManager;
use crate::object::{JsObject, PropertyAttributes, PropertyDescriptor, PropertyKey};
use crate::value::Value;
use std::sync::Arc;

/// Helper to convert Value to f64
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

/// Create and install Math namespace on global object
///
/// This function expects that all `__Math_*` ops have already been registered as globals.
/// It creates the Math namespace object, defines constants, wires methods, and installs
/// the namespace on the global object.
///
/// # Property Attributes
///
/// - **Constants**: Use `PropertyAttributes::permanent()` for immutability
/// - **Methods**: Use default attributes from `.set()` (writable, non-enumerable, configurable)
///
/// # Arguments
///
/// * `global` - The global object where Math will be installed
/// * `mm` - Memory manager for GC allocations
pub fn install_math_namespace(global: GcRef<JsObject>, mm: &Arc<MemoryManager>) {
    // Create Math namespace object (plain object, not a constructor)
    let math_obj = GcRef::new(JsObject::new(Value::null(), mm.clone()));

    // ====================================================================
    // Math Constants (ES2026 §21.3.1)
    // All constants are { writable: false, enumerable: false, configurable: false }
    // ====================================================================
    let const_attrs = PropertyAttributes::permanent();

    // Math.E (2.718281828459045)
    // The base of natural logarithms
    math_obj.define_property(
        PropertyKey::string("E"),
        PropertyDescriptor::data_with_attrs(Value::number(std::f64::consts::E), const_attrs),
    );

    // Math.LN10 (2.302585092994046)
    // The natural logarithm of 10
    math_obj.define_property(
        PropertyKey::string("LN10"),
        PropertyDescriptor::data_with_attrs(Value::number(std::f64::consts::LN_10), const_attrs),
    );

    // Math.LN2 (0.6931471805599453)
    // The natural logarithm of 2
    math_obj.define_property(
        PropertyKey::string("LN2"),
        PropertyDescriptor::data_with_attrs(Value::number(std::f64::consts::LN_2), const_attrs),
    );

    // Math.LOG10E (0.4342944819032518)
    // The base 10 logarithm of e
    math_obj.define_property(
        PropertyKey::string("LOG10E"),
        PropertyDescriptor::data_with_attrs(Value::number(std::f64::consts::LOG10_E), const_attrs),
    );

    // Math.LOG2E (1.4426950408889634)
    // The base 2 logarithm of e
    math_obj.define_property(
        PropertyKey::string("LOG2E"),
        PropertyDescriptor::data_with_attrs(Value::number(std::f64::consts::LOG2_E), const_attrs),
    );

    // Math.PI (3.141592653589793)
    // The ratio of a circle's circumference to its diameter
    math_obj.define_property(
        PropertyKey::string("PI"),
        PropertyDescriptor::data_with_attrs(Value::number(std::f64::consts::PI), const_attrs),
    );

    // Math.SQRT1_2 (0.7071067811865476)
    // The square root of 1/2 (equivalent to 1/√2)
    math_obj.define_property(
        PropertyKey::string("SQRT1_2"),
        PropertyDescriptor::data_with_attrs(
            Value::number(std::f64::consts::FRAC_1_SQRT_2),
            const_attrs,
        ),
    );

    // Math.SQRT2 (1.4142135623730951)
    // The square root of 2
    math_obj.define_property(
        PropertyKey::string("SQRT2"),
        PropertyDescriptor::data_with_attrs(Value::number(std::f64::consts::SQRT_2), const_attrs),
    );

    // ====================================================================
    // Math Methods (ES2026 §21.3.2)
    // All methods are { writable: true, enumerable: false, configurable: true }
    // ====================================================================

    // Helper macro to define a Math method
    macro_rules! math_method {
        ($name:literal, $body:expr) => {
            let _ = math_obj.set(
                PropertyKey::string($name),
                Value::native_function($body, mm.clone()),
            );
        };
    }

    // === Basic Arithmetic ===
    math_method!("abs", |_, args: &[Value], _ncx| {
        let x = to_number(args.get(0).unwrap_or(&Value::undefined()));
        Ok(Value::number(x.abs()))
    });

    math_method!("ceil", |_, args, _ncx| {
        let x = to_number(args.get(0).unwrap_or(&Value::undefined()));
        Ok(Value::number(x.ceil()))
    });

    math_method!("floor", |_, args, _ncx| {
        let x = to_number(args.get(0).unwrap_or(&Value::undefined()));
        Ok(Value::number(x.floor()))
    });

    math_method!("round", |_, args, _ncx| {
        let x = to_number(args.get(0).unwrap_or(&Value::undefined()));
        // JavaScript Math.round: rounds towards +infinity for .5 cases
        // -4.5 rounds to -4, 4.5 rounds to 5
        Ok(Value::number((x + 0.5).floor()))
    });

    math_method!("trunc", |_, args, _ncx| {
        let x = to_number(args.get(0).unwrap_or(&Value::undefined()));
        Ok(Value::number(x.trunc()))
    });

    math_method!("sign", |_, args, _ncx| {
        let x = to_number(args.get(0).unwrap_or(&Value::undefined()));
        let result = if x.is_nan() {
            f64::NAN
        } else if x == 0.0 || x == -0.0 {
            x
        } else if x > 0.0 {
            1.0
        } else {
            -1.0
        };
        Ok(Value::number(result))
    });

    math_method!("max", |_, args, _ncx| {
        let mut max = f64::NEG_INFINITY;
        for arg in args {
            let n = to_number(arg);
            if n.is_nan() {
                return Ok(Value::number(f64::NAN));
            }
            if n > max {
                max = n;
            }
        }
        Ok(Value::number(max))
    });

    math_method!("min", |_, args, _ncx| {
        let mut min = f64::INFINITY;
        for arg in args {
            let n = to_number(arg);
            if n.is_nan() {
                return Ok(Value::number(f64::NAN));
            }
            if n < min {
                min = n;
            }
        }
        Ok(Value::number(min))
    });

    // === Roots and Powers ===
    math_method!("sqrt", |_, args, _ncx| {
        let x = to_number(args.get(0).unwrap_or(&Value::undefined()));
        Ok(Value::number(x.sqrt()))
    });

    math_method!("cbrt", |_, args, _ncx| {
        let x = to_number(args.get(0).unwrap_or(&Value::undefined()));
        Ok(Value::number(x.cbrt()))
    });

    math_method!("pow", |_, args, _ncx| {
        let base = to_number(args.get(0).unwrap_or(&Value::undefined()));
        let exp = to_number(args.get(1).unwrap_or(&Value::undefined()));
        Ok(Value::number(base.powf(exp)))
    });

    math_method!("hypot", |_, args, _ncx| {
        let mut sum = 0.0;
        for arg in args {
            let n = to_number(arg);
            if n.is_nan() || n.is_infinite() {
                return Ok(Value::number(f64::INFINITY));
            }
            sum += n * n;
        }
        Ok(Value::number(sum.sqrt()))
    });

    // === Exponentials and Logarithms ===
    math_method!("exp", |_, args, _ncx| {
        let x = to_number(args.get(0).unwrap_or(&Value::undefined()));
        Ok(Value::number(x.exp()))
    });

    math_method!("expm1", |_, args, _ncx| {
        let x = to_number(args.get(0).unwrap_or(&Value::undefined()));
        Ok(Value::number(x.exp_m1()))
    });

    math_method!("log", |_, args, _ncx| {
        let x = to_number(args.get(0).unwrap_or(&Value::undefined()));
        Ok(Value::number(x.ln()))
    });

    math_method!("log1p", |_, args, _ncx| {
        let x = to_number(args.get(0).unwrap_or(&Value::undefined()));
        Ok(Value::number(x.ln_1p()))
    });

    math_method!("log2", |_, args, _ncx| {
        let x = to_number(args.get(0).unwrap_or(&Value::undefined()));
        Ok(Value::number(x.log2()))
    });

    math_method!("log10", |_, args, _ncx| {
        let x = to_number(args.get(0).unwrap_or(&Value::undefined()));
        Ok(Value::number(x.log10()))
    });

    // === Trigonometry ===
    math_method!("sin", |_, args, _ncx| {
        let x = to_number(args.get(0).unwrap_or(&Value::undefined()));
        Ok(Value::number(x.sin()))
    });

    math_method!("cos", |_, args, _ncx| {
        let x = to_number(args.get(0).unwrap_or(&Value::undefined()));
        Ok(Value::number(x.cos()))
    });

    math_method!("tan", |_, args, _ncx| {
        let x = to_number(args.get(0).unwrap_or(&Value::undefined()));
        Ok(Value::number(x.tan()))
    });

    math_method!("asin", |_, args, _ncx| {
        let x = to_number(args.get(0).unwrap_or(&Value::undefined()));
        Ok(Value::number(x.asin()))
    });

    math_method!("acos", |_, args, _ncx| {
        let x = to_number(args.get(0).unwrap_or(&Value::undefined()));
        Ok(Value::number(x.acos()))
    });

    math_method!("atan", |_, args, _ncx| {
        let x = to_number(args.get(0).unwrap_or(&Value::undefined()));
        Ok(Value::number(x.atan()))
    });

    math_method!("atan2", |_, args, _ncx| {
        let y = to_number(args.get(0).unwrap_or(&Value::undefined()));
        let x = to_number(args.get(1).unwrap_or(&Value::undefined()));
        Ok(Value::number(y.atan2(x)))
    });

    // === Hyperbolic Functions ===
    math_method!("sinh", |_, args, _ncx| {
        let x = to_number(args.get(0).unwrap_or(&Value::undefined()));
        Ok(Value::number(x.sinh()))
    });

    math_method!("cosh", |_, args, _ncx| {
        let x = to_number(args.get(0).unwrap_or(&Value::undefined()));
        Ok(Value::number(x.cosh()))
    });

    math_method!("tanh", |_, args, _ncx| {
        let x = to_number(args.get(0).unwrap_or(&Value::undefined()));
        Ok(Value::number(x.tanh()))
    });

    math_method!("asinh", |_, args, _ncx| {
        let x = to_number(args.get(0).unwrap_or(&Value::undefined()));
        Ok(Value::number(x.asinh()))
    });

    math_method!("acosh", |_, args, _ncx| {
        let x = to_number(args.get(0).unwrap_or(&Value::undefined()));
        Ok(Value::number(x.acosh()))
    });

    math_method!("atanh", |_, args, _ncx| {
        let x = to_number(args.get(0).unwrap_or(&Value::undefined()));
        Ok(Value::number(x.atanh()))
    });

    // === Special Functions ===
    math_method!("clz32", |_, args, _ncx| {
        let x = to_number(args.get(0).unwrap_or(&Value::undefined()));
        let val = (x as i32) as u32;
        Ok(Value::number(val.leading_zeros() as f64))
    });

    math_method!("imul", |_, args, _ncx| {
        let a = to_number(args.get(0).unwrap_or(&Value::undefined())) as i32;
        let b = to_number(args.get(1).unwrap_or(&Value::undefined())) as i32;
        Ok(Value::number(a.wrapping_mul(b) as f64))
    });

    math_method!("fround", |_, args, _ncx| {
        let x = to_number(args.get(0).unwrap_or(&Value::undefined()));
        Ok(Value::number((x as f32) as f64))
    });

    math_method!("random", |_, _args, _ncx| {
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
    });

    // Math.f16round (ES2026) - round to half-precision (16-bit) float
    // Uses IEEE 754 binary16 format via the 'half' crate
    // Range: ±65504, overflow becomes infinity, proper subnormal handling
    math_method!("f16round", |_, args, _ncx| {
        let x = to_number(args.get(0).unwrap_or(&Value::undefined()));

        // IEEE 754 binary16 (half-precision) conversion
        // 1 sign bit + 5 exponent bits + 10 mantissa bits
        let f16_val = half::f16::from_f64(x);
        let result = f16_val.to_f64();

        Ok(Value::number(result))
    });

    // Math.sumPrecise (ES2026) - sum with enhanced floating-point precision
    math_method!("sumPrecise", |_, args, _ncx| {
        // Get the iterable (first argument)
        let undefined = Value::undefined();
        let iterable = args.get(0).unwrap_or(&undefined);

        // If it's an array, sum its elements using Kahan summation
        if let Some(arr_obj) = iterable.as_object() {
            // Try to get length property
            let length_val = arr_obj
                .get(&PropertyKey::string("length"))
                .unwrap_or(Value::undefined());
            let length = if let Some(n) = length_val.as_number() {
                n as usize
            } else if let Some(n) = length_val.as_int32() {
                n as usize
            } else {
                0
            };

            // Kahan compensated summation algorithm
            // Reduces floating-point rounding errors in sum
            let mut sum = 0.0;
            let mut compensation = 0.0; // Running compensation for lost low-order bits

            for i in 0..length {
                let elem = arr_obj
                    .get(&PropertyKey::Index(i as u32))
                    .unwrap_or(Value::undefined());
                let value = to_number(&elem);

                // Skip NaN values (return NaN immediately)
                if value.is_nan() {
                    return Ok(Value::number(f64::NAN));
                }

                // Kahan summation step
                let y = value - compensation; // Subtract the compensation
                let t = sum + y; // Add to sum
                compensation = (t - sum) - y; // Calculate new compensation
                sum = t; // Update sum
            }

            Ok(Value::number(sum))
        } else {
            // Not an array/iterable, return 0
            Ok(Value::number(0.0))
        }
    });

    // ====================================================================
    // Install Math on global
    // ====================================================================
    let _ = global.set(PropertyKey::string("Math"), Value::object(math_obj));
}
