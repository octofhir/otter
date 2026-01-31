//! Number.prototype methods implementation
//!
//! All Number object methods for ES2026 standard.

use crate::error::VmError;
use crate::gc::GcRef;
use crate::object::{JsObject, PropertyDescriptor, PropertyKey};
use crate::string::JsString;
use crate::value::Value;
use crate::memory::MemoryManager;
use std::sync::Arc;

/// Wire all Number.prototype methods to the prototype object
pub fn init_number_prototype(
    number_proto: GcRef<JsObject>,
    fn_proto: GcRef<JsObject>,
    mm: &Arc<MemoryManager>,
) {
        number_proto.define_property(
            PropertyKey::string("valueOf"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |this_val, _args, _mm| Ok(this_val.clone()),
                mm.clone(),
                fn_proto,
            )),
        );

        // Number.prototype.toString([radix])
        number_proto.define_property(
            PropertyKey::string("toString"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |this_val, args, _mm| {
                    let n = if let Some(num) = this_val.as_number() {
                        num
                    } else if let Some(i) = this_val.as_int32() {
                        i as f64
                    } else {
                        return Err(VmError::type_error("Number.prototype.toString requires a number"));
                    };

                    let radix = args.first().and_then(|v| v.as_int32()).unwrap_or(10);
                    if radix < 2 || radix > 36 {
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
                        // Manual radix conversion
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
                        // Fractional numbers: just return decimal for now
                        n.to_string()
                    };
                    Ok(Value::string(JsString::intern(&result)))
                },
                mm.clone(),
                fn_proto,
            )),
        );

        // Number.prototype.toFixed(digits)
        number_proto.define_property(
            PropertyKey::string("toFixed"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |this_val, args, _mm| {
                    let n = if let Some(num) = this_val.as_number() {
                        num
                    } else if let Some(i) = this_val.as_int32() {
                        i as f64
                    } else {
                        return Err(VmError::type_error("Number.prototype.toFixed requires a number"));
                    };

                    let digits = args
                        .first()
                        .and_then(|v| v.as_int32())
                        .unwrap_or(0)
                        .max(0)
                        .min(100) as usize;

                    if n.is_nan() {
                        return Ok(Value::string(JsString::intern("NaN")));
                    }
                    if n.is_infinite() {
                        return Ok(Value::string(JsString::intern(
                            if n.is_sign_positive() {
                                "Infinity"
                            } else {
                                "-Infinity"
                            },
                        )));
                    }

                    let result = format!("{:.prec$}", n, prec = digits);
                    Ok(Value::string(JsString::intern(&result)))
                },
                mm.clone(),
                fn_proto,
            )),
        );

        // Number.prototype.toExponential(fractionDigits)
        number_proto.define_property(
            PropertyKey::string("toExponential"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |this_val, args, _mm| {
                    let n = if let Some(num) = this_val.as_number() {
                        num
                    } else if let Some(i) = this_val.as_int32() {
                        i as f64
                    } else {
                        return Err(
                            VmError::type_error("Number.prototype.toExponential requires a number")
                        );
                    };

                    if n.is_nan() {
                        return Ok(Value::string(JsString::intern("NaN")));
                    }
                    if n.is_infinite() {
                        return Ok(Value::string(JsString::intern(
                            if n.is_sign_positive() {
                                "Infinity"
                            } else {
                                "-Infinity"
                            },
                        )));
                    }

                    let digits = args
                        .first()
                        .and_then(|v| v.as_int32())
                        .unwrap_or(0)
                        .max(0)
                        .min(100) as usize;

                    let result = format!("{:.prec$e}", n, prec = digits);
                    Ok(Value::string(JsString::intern(&result)))
                },
                mm.clone(),
                fn_proto,
            )),
        );

        // Number.prototype.toPrecision(precision)
        number_proto.define_property(
            PropertyKey::string("toPrecision"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |this_val, args, _mm| {
                    let n = if let Some(num) = this_val.as_number() {
                        num
                    } else if let Some(i) = this_val.as_int32() {
                        i as f64
                    } else {
                        return Err(
                            VmError::type_error("Number.prototype.toPrecision requires a number")
                        );
                    };

                    if args.is_empty() {
                        return Ok(Value::string(JsString::intern(&n.to_string())));
                    }

                    if n.is_nan() {
                        return Ok(Value::string(JsString::intern("NaN")));
                    }
                    if n.is_infinite() {
                        return Ok(Value::string(JsString::intern(
                            if n.is_sign_positive() {
                                "Infinity"
                            } else {
                                "-Infinity"
                            },
                        )));
                    }

                    let precision = args
                        .first()
                        .and_then(|v| v.as_int32())
                        .unwrap_or(1)
                        .max(1)
                        .min(100) as usize;

                    let result = format!("{:.prec$}", n, prec = precision - 1);
                    Ok(Value::string(JsString::intern(&result)))
                },
                mm.clone(),
                fn_proto,
            )),
        );

        // Number.prototype.toLocaleString()
        number_proto.define_property(
            PropertyKey::string("toLocaleString"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |this_val, _args, _mm| {
                    let n = if let Some(num) = this_val.as_number() {
                        num
                    } else if let Some(i) = this_val.as_int32() {
                        i as f64
                    } else {
                        return Err(
                            VmError::type_error("Number.prototype.toLocaleString requires a number")
                        );
                    };
                    // Simplified: just use toString for now
                    Ok(Value::string(JsString::intern(&n.to_string())))
                },
                mm.clone(),
                fn_proto,
            )),
        );
}
