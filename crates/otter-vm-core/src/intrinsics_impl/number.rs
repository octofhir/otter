//! Number constructor statics and prototype methods (ES2026)
//!
//! ## Constructor statics:
//! - Constants: EPSILON, MAX_VALUE, MIN_VALUE, MAX_SAFE_INTEGER, MIN_SAFE_INTEGER,
//!   POSITIVE_INFINITY, NEGATIVE_INFINITY, NaN
//! - `Number.isFinite()`, `Number.isInteger()`, `Number.isNaN()`, `Number.isSafeInteger()`
//! - `Number.parseFloat()`, `Number.parseInt()`
//!
//! ## Prototype methods:
//! - `valueOf()`, `toString()`, `toFixed()`, `toPrecision()`, `toExponential()`, `toLocaleString()`

use crate::context::NativeContext;
use crate::error::VmError;
use crate::gc::GcRef;
use crate::memory::MemoryManager;
use crate::object::{JsObject, PropertyAttributes, PropertyDescriptor, PropertyKey};
use crate::string::JsString;
use crate::value::Value;
use otter_macros::dive;
use std::sync::Arc;

fn number_this_value(this_val: &Value, method: &str) -> Result<f64, VmError> {
    if let Some(num) = this_val.as_number() {
        Ok(num)
    } else if let Some(i) = this_val.as_int32() {
        Ok(i as f64)
    } else {
        Err(VmError::type_error(format!(
            "Number.prototype.{method} requires a number"
        )))
    }
}

#[dive(name = "valueOf", length = 0)]
fn number_value_of(
    this_val: &Value,
    _args: &[Value],
    _ncx: &mut NativeContext<'_>,
) -> Result<Value, VmError> {
    if let Some(num) = this_val.as_number() {
        return Ok(Value::number(num));
    }
    if let Some(i) = this_val.as_int32() {
        return Ok(Value::number(i as f64));
    }
    if let Some(obj) = this_val.as_object() {
        if let Some(val) = obj.get(&PropertyKey::string("__value__")) {
            return Ok(val);
        }
    }
    Err(VmError::type_error(
        "Number.prototype.valueOf requires a number",
    ))
}

#[dive(name = "toString", length = 1)]
fn number_to_string(
    this_val: &Value,
    args: &[Value],
    _ncx: &mut NativeContext<'_>,
) -> Result<Value, VmError> {
    let n = number_this_value(this_val, "toString")?;
    let radix = args.first().and_then(|v| v.as_int32()).unwrap_or(10);
    if !(2..=36).contains(&radix) {
        return Err(VmError::type_error("radix must be between 2 and 36"));
    }

    let result = if n.is_nan() {
        "NaN".to_string()
    } else if n.is_infinite() {
        if n.is_sign_positive() {
            "Infinity".to_string()
        } else {
            "-Infinity".to_string()
        }
    } else if radix == 10 {
        n.to_string()
    } else if n.fract() == 0.0 && n.is_finite() {
        let i = n as i64;
        let is_negative = i < 0;
        let mut num = i.abs() as u64;
        let mut digits = Vec::new();
        let radix_u = radix as u64;

        if num == 0 {
            "0".to_string()
        } else {
            while num > 0 {
                let digit = (num % radix_u) as u8;
                let ch = if digit < 10 {
                    (b'0' + digit) as char
                } else {
                    (b'a' + (digit - 10)) as char
                };
                digits.push(ch);
                num /= radix_u;
            }
            digits.reverse();
            let mut result = String::new();
            if is_negative {
                result.push('-');
            }
            result.extend(digits);
            result
        }
    } else {
        n.to_string()
    };

    Ok(Value::string(JsString::intern(&result)))
}

#[dive(name = "toFixed", length = 1)]
fn number_to_fixed(
    this_val: &Value,
    args: &[Value],
    _ncx: &mut NativeContext<'_>,
) -> Result<Value, VmError> {
    let n = number_this_value(this_val, "toFixed")?;
    let digits = args
        .first()
        .and_then(|v| v.as_int32())
        .unwrap_or(0)
        .clamp(0, 100) as usize;

    if n.is_nan() {
        return Ok(Value::string(JsString::intern("NaN")));
    }
    if n.is_infinite() {
        return Ok(Value::string(JsString::intern(if n.is_sign_positive() {
            "Infinity"
        } else {
            "-Infinity"
        })));
    }

    let result = format!("{:.prec$}", n, prec = digits);
    Ok(Value::string(JsString::intern(&result)))
}

#[dive(name = "toExponential", length = 1)]
fn number_to_exponential(
    this_val: &Value,
    args: &[Value],
    _ncx: &mut NativeContext<'_>,
) -> Result<Value, VmError> {
    let n = number_this_value(this_val, "toExponential")?;
    if n.is_nan() {
        return Ok(Value::string(JsString::intern("NaN")));
    }
    if n.is_infinite() {
        return Ok(Value::string(JsString::intern(if n.is_sign_positive() {
            "Infinity"
        } else {
            "-Infinity"
        })));
    }

    let digits = args
        .first()
        .and_then(|v| v.as_int32())
        .unwrap_or(0)
        .clamp(0, 100) as usize;
    let result = format!("{:.prec$e}", n, prec = digits);
    Ok(Value::string(JsString::intern(&result)))
}

#[dive(name = "toPrecision", length = 1)]
fn number_to_precision(
    this_val: &Value,
    args: &[Value],
    _ncx: &mut NativeContext<'_>,
) -> Result<Value, VmError> {
    let n = number_this_value(this_val, "toPrecision")?;
    if args.is_empty() {
        return Ok(Value::string(JsString::intern(&n.to_string())));
    }

    if n.is_nan() {
        return Ok(Value::string(JsString::intern("NaN")));
    }
    if n.is_infinite() {
        return Ok(Value::string(JsString::intern(if n.is_sign_positive() {
            "Infinity"
        } else {
            "-Infinity"
        })));
    }

    let precision = args
        .first()
        .and_then(|v| v.as_int32())
        .unwrap_or(1)
        .clamp(1, 100) as usize;
    let result = format!("{:.prec$}", n, prec = precision - 1);
    Ok(Value::string(JsString::intern(&result)))
}

#[dive(name = "toLocaleString", length = 0)]
fn number_to_locale_string(
    this_val: &Value,
    _args: &[Value],
    _ncx: &mut NativeContext<'_>,
) -> Result<Value, VmError> {
    let n = number_this_value(this_val, "toLocaleString")?;
    Ok(Value::string(JsString::intern(&n.to_string())))
}

#[dive(name = "isFinite", length = 1)]
fn number_is_finite(
    _this: &Value,
    args: &[Value],
    _ncx: &mut NativeContext<'_>,
) -> Result<Value, VmError> {
    let val = args.first();
    match val {
        Some(v) if v.is_number() => Ok(Value::boolean(v.as_number().unwrap().is_finite())),
        Some(v) if v.is_int32() => Ok(Value::boolean(true)),
        _ => Ok(Value::boolean(false)),
    }
}

#[dive(name = "isInteger", length = 1)]
fn number_is_integer(
    _this: &Value,
    args: &[Value],
    _ncx: &mut NativeContext<'_>,
) -> Result<Value, VmError> {
    let val = args.first();
    match val {
        Some(v) if v.is_int32() => Ok(Value::boolean(true)),
        Some(v) if v.is_number() => {
            let n = v.as_number().unwrap();
            Ok(Value::boolean(n.is_finite() && n.fract() == 0.0))
        }
        _ => Ok(Value::boolean(false)),
    }
}

#[dive(name = "isNaN", length = 1)]
fn number_is_nan(
    _this: &Value,
    args: &[Value],
    _ncx: &mut NativeContext<'_>,
) -> Result<Value, VmError> {
    let val = args.first();
    match val {
        Some(v) if v.is_number() => Ok(Value::boolean(v.as_number().unwrap().is_nan())),
        _ => Ok(Value::boolean(false)),
    }
}

#[dive(name = "isSafeInteger", length = 1)]
fn number_is_safe_integer(
    _this: &Value,
    args: &[Value],
    _ncx: &mut NativeContext<'_>,
) -> Result<Value, VmError> {
    let val = args.first();
    match val {
        Some(v) if v.is_int32() => Ok(Value::boolean(true)),
        Some(v) if v.is_number() => {
            let n = v.as_number().unwrap();
            let max_safe = 9007199254740991.0;
            Ok(Value::boolean(
                n.is_finite() && n.fract() == 0.0 && n.abs() <= max_safe,
            ))
        }
        _ => Ok(Value::boolean(false)),
    }
}

#[dive(name = "parseFloat", length = 1)]
fn number_parse_float(
    _this: &Value,
    args: &[Value],
    _ncx: &mut NativeContext<'_>,
) -> Result<Value, VmError> {
    let val = args.first().ok_or("parseFloat requires an argument")?;
    if let Some(s) = val.as_string() {
        let trimmed = s.as_str().trim_start();
        if let Ok(n) = trimmed.parse::<f64>() {
            Ok(Value::number(n))
        } else {
            Ok(Value::number(f64::NAN))
        }
    } else {
        Ok(Value::number(f64::NAN))
    }
}

#[dive(name = "parseInt", length = 2)]
fn number_parse_int(
    _this: &Value,
    args: &[Value],
    _ncx: &mut NativeContext<'_>,
) -> Result<Value, VmError> {
    let val = args.first().ok_or("parseInt requires an argument")?;
    let radix = args.get(1).and_then(|v| v.as_int32()).unwrap_or(10);

    if !(2..=36).contains(&radix) {
        return Ok(Value::number(f64::NAN));
    }

    if let Some(s) = val.as_string() {
        let trimmed = s.as_str().trim_start();
        if let Ok(n) = i64::from_str_radix(trimmed, radix as u32) {
            Ok(Value::number(n as f64))
        } else {
            Ok(Value::number(f64::NAN))
        }
    } else {
        Ok(Value::number(f64::NAN))
    }
}

/// Wire all Number.prototype methods to the prototype object
pub fn init_number_prototype(
    number_proto: GcRef<JsObject>,
    _fn_proto: GcRef<JsObject>,
    mm: &Arc<MemoryManager>,
) {
    let methods: &[(&str, crate::value::NativeFn, u32)] = &[
        number_value_of_decl(),
        number_to_string_decl(),
        number_to_fixed_decl(),
        number_to_exponential_decl(),
        number_to_precision_decl(),
        number_to_locale_string_decl(),
    ];
    for (name, native_fn, length) in methods {
        let fn_val = Value::native_function_from_decl(name, native_fn.clone(), *length, mm.clone());
        number_proto.define_property(
            PropertyKey::string(name),
            PropertyDescriptor::builtin_method(fn_val),
        );
    }
}

/// Install constants and static methods on the Number constructor.
pub fn install_number_statics(
    ctor: GcRef<JsObject>,
    _fn_proto: GcRef<JsObject>,
    mm: &Arc<MemoryManager>,
) {
    // ========================================================================
    // Constants — §21.1.2.1–2.8
    // ========================================================================
    let constant = |name: &str, val: f64| {
        ctor.define_property(
            PropertyKey::string(name),
            PropertyDescriptor::data_with_attrs(
                Value::number(val),
                PropertyAttributes::permanent(),
            ),
        );
    };
    constant("EPSILON", f64::EPSILON);
    constant("MAX_VALUE", f64::MAX);
    constant("MIN_VALUE", f64::MIN_POSITIVE);
    constant("MAX_SAFE_INTEGER", 9007199254740991.0); // 2^53 - 1
    constant("MIN_SAFE_INTEGER", -9007199254740991.0); // -(2^53 - 1)
    constant("POSITIVE_INFINITY", f64::INFINITY);
    constant("NEGATIVE_INFINITY", f64::NEG_INFINITY);
    constant("NaN", f64::NAN);

    let methods: &[(&str, crate::value::NativeFn, u32)] = &[
        number_is_finite_decl(),
        number_is_integer_decl(),
        number_is_nan_decl(),
        number_is_safe_integer_decl(),
        number_parse_float_decl(),
        number_parse_int_decl(),
    ];
    for (name, native_fn, length) in methods {
        let fn_val = Value::native_function_from_decl(name, native_fn.clone(), *length, mm.clone());
        ctor.define_property(
            PropertyKey::string(name),
            PropertyDescriptor::builtin_method(fn_val),
        );
    }
}
