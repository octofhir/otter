//! String.prototype methods implementation
//!
//! All String object methods for ES2026 standard.

use crate::error::VmError;
use crate::gc::GcRef;
use crate::object::{JsObject, PropertyDescriptor, PropertyKey};
use crate::string::JsString;
use crate::value::Value;
use crate::memory::MemoryManager;
use std::sync::Arc;

/// thisStringValue(value) per ES2023 §22.1.3
///
/// Extracts the string value from `this`:
/// - If `this` is a string primitive, returns it directly.
/// - If `this` is a String wrapper object (`new String("...")`), reads its
///   `[[PrimitiveValue]]` internal slot (stored as `toString()` result or via
///   a `__primitiveValue__` property).
/// - If `this` is any other object, calls `toString()` on it as a fallback.
/// - Otherwise, returns an error (null/undefined).
fn this_string_value(this_val: &Value) -> Result<GcRef<JsString>, String> {
    // Fast path: string primitive
    if let Some(s) = this_val.as_string() {
        return Ok(s);
    }
    // Object path: String wrapper or generic object
    if let Some(obj) = this_val.as_object() {
        // Check for __primitiveValue__ (internal slot for String wrapper objects)
        if let Some(prim) = obj.get(&PropertyKey::string("__primitiveValue__")) {
            if let Some(s) = prim.as_string() {
                return Ok(s);
            }
        }
        // Fallback: try valueOf then toString
        if let Some(val) = obj.get(&PropertyKey::string("valueOf")) {
            if let Some(s) = val.as_string() {
                return Ok(s);
            }
        }
        if let Some(val) = obj.get(&PropertyKey::string("toString")) {
            if let Some(s) = val.as_string() {
                return Ok(s);
            }
        }
        // Last resort: coerce via number-like or use "[object Object]"
        return Ok(JsString::intern("[object Object]"));
    }
    // Number/boolean coercion
    if let Some(n) = this_val.as_number() {
        let s = if n.fract() == 0.0 && n.abs() < 1e15 {
            format!("{}", n as i64)
        } else {
            format!("{}", n)
        };
        return Ok(JsString::intern(&s));
    }
    if let Some(b) = this_val.as_boolean() {
        return Ok(JsString::intern(if b { "true" } else { "false" }));
    }
    Err("String.prototype method called on null or undefined".to_string())
}

// ============================================================================
// String Iterator
// ============================================================================

/// Helper to check if a UTF-16 code unit is a high surrogate
fn is_high_surrogate(unit: u16) -> bool {
    unit >= 0xD800 && unit <= 0xDBFF
}

/// Helper to check if a UTF-16 code unit is a low surrogate
fn is_low_surrogate(unit: u16) -> bool {
    unit >= 0xDC00 && unit <= 0xDFFF
}

/// Create a String iterator object that handles UTF-16 surrogate pairs correctly.
fn make_string_iterator(
    this_val: &Value,
    mm: Arc<MemoryManager>,
    fn_proto: GcRef<JsObject>,
    iter_proto: GcRef<JsObject>,
) -> Result<Value, VmError> {
    // Extract string value (handles both primitives and String objects)
    let string = this_string_value(this_val)
        .map_err(|e| VmError::type_error(&e))?;

    // Create iterator object with %IteratorPrototype% as prototype
    let iter = GcRef::new(JsObject::new(Value::object(iter_proto), mm.clone()));

    // Store the string reference and current index
    let _ = iter.set(PropertyKey::string("__string_ref__"), Value::string(string));
    let _ = iter.set(PropertyKey::string("__string_index__"), Value::number(0.0));

    // Define next() method
    let fn_proto_for_next = fn_proto;
    iter.define_property(
        PropertyKey::string("next"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, _args, ncx| {
                let iter_obj = this_val
                    .as_object()
                    .ok_or_else(|| "not an iterator object".to_string())?;
                let string = iter_obj
                    .get(&PropertyKey::string("__string_ref__"))
                    .and_then(|v| v.as_string())
                    .ok_or_else(|| "iterator: missing string ref".to_string())?;
                let idx = iter_obj
                    .get(&PropertyKey::string("__string_index__"))
                    .and_then(|v| v.as_number())
                    .unwrap_or(0.0) as usize;

                // Get UTF-16 code units
                let units = string.as_utf16();
                let len = units.len();

                if idx >= len {
                    // Done
                    let result = GcRef::new(JsObject::new(Value::null(), ncx.memory_manager().clone()));
                    let _ = result.set(PropertyKey::string("value"), Value::undefined());
                    let _ = result.set(PropertyKey::string("done"), Value::boolean(true));
                    return Ok(Value::object(result));
                }

                // Read code point(s), handling surrogate pairs
                let first = units[idx];
                let (char_string, next_idx) = if is_high_surrogate(first)
                    && idx + 1 < len
                    && is_low_surrogate(units[idx + 1]) {
                    // Surrogate pair: combine into single code point
                    let pair = vec![first, units[idx + 1]];
                    let char_str = String::from_utf16_lossy(&pair);
                    (JsString::intern(&char_str), idx + 2)
                } else {
                    // Single code unit (either BMP character or unpaired surrogate)
                    let single = vec![first];
                    let char_str = String::from_utf16_lossy(&single);
                    (JsString::intern(&char_str), idx + 1)
                };

                // Advance index
                let _ = iter_obj.set(
                    PropertyKey::string("__string_index__"),
                    Value::number(next_idx as f64),
                );

                let result = GcRef::new(JsObject::new(Value::null(), ncx.memory_manager().clone()));
                let _ = result.set(PropertyKey::string("value"), Value::string(char_string));
                let _ = result.set(PropertyKey::string("done"), Value::boolean(false));
                Ok(Value::object(result))
            },
            mm,
            fn_proto_for_next,
        )),
    );
    Ok(Value::object(iter))
}

/// Wire all String.prototype methods to the prototype object
pub fn init_string_prototype(
    string_proto: GcRef<JsObject>,
    fn_proto: GcRef<JsObject>,
    mm: &Arc<MemoryManager>,
    iterator_proto: GcRef<JsObject>,
    symbol_iterator: crate::gc::GcRef<crate::value::Symbol>,
) {
        string_proto.define_property(
            PropertyKey::string("toString"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |this_val, _args, _ncx| {
                    // String primitive
                    if let Some(s) = this_val.as_string() {
                        return Ok(Value::string(s));
                    }
                    // String wrapper object
                    if let Some(obj) = this_val.as_object() {
                        if let Some(prim) = obj.get(&PropertyKey::string("__primitiveValue__")) {
                            if let Some(s) = prim.as_string() {
                                return Ok(Value::string(s));
                            }
                        }
                    }
                    Err(VmError::type_error(
                        "String.prototype.toString requires that 'this' be a String",
                    ))
                },
                mm.clone(),
                fn_proto,
            )),
        );
        string_proto.define_property(
            PropertyKey::string("valueOf"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |this_val, _args, _ncx| {
                    // String primitive
                    if let Some(s) = this_val.as_string() {
                        return Ok(Value::string(s));
                    }
                    // String wrapper object
                    if let Some(obj) = this_val.as_object() {
                        if let Some(prim) = obj.get(&PropertyKey::string("__primitiveValue__")) {
                            if let Some(s) = prim.as_string() {
                                return Ok(Value::string(s));
                            }
                        }
                    }
                    Err(VmError::type_error(
                        "String.prototype.valueOf requires that 'this' be a String",
                    ))
                },
                mm.clone(),
                fn_proto,
            )),
        );

        // String.prototype.length (getter)
        string_proto.define_property(
            PropertyKey::string("length"),
            PropertyDescriptor::getter(Value::native_function_with_proto(
                |this_val, _args, _ncx| {
                    if let Some(s) = this_val.as_string() {
                        Ok(Value::number(s.as_str().len() as f64))
                    } else {
                        Ok(Value::number(0.0))
                    }
                },
                mm.clone(),
                fn_proto,
            )),
        );

        // String.prototype.charAt
        string_proto.define_property(
            PropertyKey::string("charAt"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |this_val, args, _ncx| {
                    let s = this_string_value(this_val)?;
                    let pos = args
                        .first()
                        .and_then(|v| v.as_number())
                        .unwrap_or(0.0) as usize;
                    let chars: Vec<char> = s.as_str().chars().collect();
                    if pos < chars.len() {
                        Ok(Value::string(JsString::intern(&chars[pos].to_string())))
                    } else {
                        Ok(Value::string(JsString::intern("")))
                    }
                },
                mm.clone(),
                fn_proto,
            )),
        );

        // String.prototype.charCodeAt
        string_proto.define_property(
            PropertyKey::string("charCodeAt"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |this_val, args, _ncx| {
                    let s = this_string_value(this_val)?;
                    let pos = args
                        .first()
                        .and_then(|v| v.as_number())
                        .unwrap_or(0.0) as usize;
                    let chars: Vec<char> = s.as_str().chars().collect();
                    if pos < chars.len() {
                        Ok(Value::number(chars[pos] as u32 as f64))
                    } else {
                        Ok(Value::number(f64::NAN))
                    }
                },
                mm.clone(),
                fn_proto,
            )),
        );

        // String.prototype.slice
        string_proto.define_property(
            PropertyKey::string("slice"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |this_val, args, _ncx| {
                    let s = this_string_value(this_val)?;
                    let str_val = s.as_str();
                    let len = str_val.len() as i64;
                    let start = args
                        .first()
                        .and_then(|v| v.as_number())
                        .unwrap_or(0.0) as i64;
                    let end = args
                        .get(1)
                        .and_then(|v| {
                            if v.is_undefined() {
                                None
                            } else {
                                v.as_number()
                            }
                        })
                        .unwrap_or(len as f64) as i64;
                    let from = if start < 0 {
                        (len + start).max(0)
                    } else {
                        start.min(len)
                    } as usize;
                    let to = if end < 0 {
                        (len + end).max(0)
                    } else {
                        end.min(len)
                    } as usize;
                    if to > from {
                        Ok(Value::string(JsString::intern(&str_val[from..to])))
                    } else {
                        Ok(Value::string(JsString::intern("")))
                    }
                },
                mm.clone(),
                fn_proto,
            )),
        );

        // String.prototype.substring
        string_proto.define_property(
            PropertyKey::string("substring"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |this_val, args, _ncx| {
                    let s = this_string_value(this_val)?;
                    let str_val = s.as_str();
                    let len = str_val.len();
                    let start = args
                        .first()
                        .and_then(|v| v.as_number())
                        .unwrap_or(0.0)
                        .max(0.0) as usize;
                    let end = args
                        .get(1)
                        .and_then(|v| {
                            if v.is_undefined() {
                                None
                            } else {
                                v.as_number()
                            }
                        })
                        .unwrap_or(len as f64)
                        .max(0.0) as usize;
                    let from = start.min(end).min(len);
                    let to = start.max(end).min(len);
                    Ok(Value::string(JsString::intern(&str_val[from..to])))
                },
                mm.clone(),
                fn_proto,
            )),
        );

        // String.prototype.toLowerCase
        string_proto.define_property(
            PropertyKey::string("toLowerCase"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |this_val, _args, _ncx| {
                    let s = this_string_value(this_val)?;
                    Ok(Value::string(JsString::intern(&s.as_str().to_lowercase())))
                },
                mm.clone(),
                fn_proto,
            )),
        );

        // String.prototype.toUpperCase
        string_proto.define_property(
            PropertyKey::string("toUpperCase"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |this_val, _args, _ncx| {
                    let s = this_string_value(this_val)?;
                    Ok(Value::string(JsString::intern(&s.as_str().to_uppercase())))
                },
                mm.clone(),
                fn_proto,
            )),
        );

        // String.prototype.trim
        string_proto.define_property(
            PropertyKey::string("trim"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |this_val, _args, _ncx| {
                    let s = this_string_value(this_val)?;
                    Ok(Value::string(JsString::intern(s.as_str().trim())))
                },
                mm.clone(),
                fn_proto,
            )),
        );

        // String.prototype.trimStart (ES2019)
        string_proto.define_property(
            PropertyKey::string("trimStart"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |this_val, _args, _ncx| {
                    let s = this_string_value(this_val)?;
                    Ok(Value::string(JsString::intern(s.as_str().trim_start())))
                },
                mm.clone(),
                fn_proto,
            )),
        );

        // String.prototype.trimEnd (ES2019)
        string_proto.define_property(
            PropertyKey::string("trimEnd"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |this_val, _args, _ncx| {
                    let s = this_string_value(this_val)?;
                    Ok(Value::string(JsString::intern(s.as_str().trim_end())))
                },
                mm.clone(),
                fn_proto,
            )),
        );

        // String.prototype.startsWith (ES2015)
        string_proto.define_property(
            PropertyKey::string("startsWith"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |this_val, args, _ncx| {
                    let s = this_string_value(this_val)?;
                    let search = args
                        .first()
                        .and_then(|v| v.as_string())
                        .ok_or_else(|| "startsWith requires a search string".to_string())?;
                    let pos = args
                        .get(1)
                        .and_then(|v| v.as_number())
                        .unwrap_or(0.0)
                        .max(0.0) as usize;
                    let str_val = s.as_str();
                    if pos > str_val.len() {
                        return Ok(Value::boolean(false));
                    }
                    Ok(Value::boolean(str_val[pos..].starts_with(search.as_str())))
                },
                mm.clone(),
                fn_proto,
            )),
        );

        // String.prototype.endsWith (ES2015)
        string_proto.define_property(
            PropertyKey::string("endsWith"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |this_val, args, _ncx| {
                    let s = this_string_value(this_val)?;
                    let search = args
                        .first()
                        .and_then(|v| v.as_string())
                        .ok_or_else(|| "endsWith requires a search string".to_string())?;
                    let str_val = s.as_str();
                    let len = str_val.len();
                    let end_pos = args
                        .get(1)
                        .and_then(|v| {
                            if v.is_undefined() {
                                None
                            } else {
                                v.as_number()
                            }
                        })
                        .unwrap_or(len as f64) as usize;
                    let pos = end_pos.min(len);
                    Ok(Value::boolean(str_val[..pos].ends_with(search.as_str())))
                },
                mm.clone(),
                fn_proto,
            )),
        );

        // String.prototype.includes (ES2015)
        string_proto.define_property(
            PropertyKey::string("includes"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |this_val, args, _ncx| {
                    let s = this_string_value(this_val)?;
                    let search = args
                        .first()
                        .and_then(|v| v.as_string())
                        .ok_or_else(|| "includes requires a search string".to_string())?;
                    let pos = args
                        .get(1)
                        .and_then(|v| v.as_number())
                        .unwrap_or(0.0)
                        .max(0.0) as usize;
                    let str_val = s.as_str();
                    if pos > str_val.len() {
                        return Ok(Value::boolean(false));
                    }
                    Ok(Value::boolean(str_val[pos..].contains(search.as_str())))
                },
                mm.clone(),
                fn_proto,
            )),
        );

        // String.prototype.repeat (ES2015)
        string_proto.define_property(
            PropertyKey::string("repeat"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |this_val, args, _ncx| {
                    let s = this_string_value(this_val)?;
                    let count = args
                        .first()
                        .and_then(|v| v.as_number())
                        .unwrap_or(0.0);
                    if count < 0.0 || count.is_infinite() {
                        return Err(VmError::type_error("RangeError: Invalid count"));
                    }
                    let n = count as usize;
                    Ok(Value::string(JsString::intern(&s.as_str().repeat(n))))
                },
                mm.clone(),
                fn_proto,
            )),
        );

        // String.prototype.padStart (ES2017)
        string_proto.define_property(
            PropertyKey::string("padStart"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |this_val, args, _ncx| {
                    let s = this_string_value(this_val)?;
                    let target_len = args
                        .first()
                        .and_then(|v| v.as_number())
                        .unwrap_or(0.0) as usize;
                    let str_val = s.as_str();
                    if target_len <= str_val.len() {
                        return Ok(Value::string(s));
                    }
                    let fill_str = args
                        .get(1)
                        .and_then(|v| {
                            if v.is_undefined() {
                                None
                            } else {
                                v.as_string()
                            }
                        })
                        .map(|s| s.as_str().to_string())
                        .unwrap_or_else(|| " ".to_string());
                    if fill_str.is_empty() {
                        return Ok(Value::string(s));
                    }
                    let pad_len = target_len - str_val.len();
                    let pad = fill_str.repeat((pad_len / fill_str.len()) + 1);
                    let result = format!("{}{}", &pad[..pad_len], str_val);
                    Ok(Value::string(JsString::intern(&result)))
                },
                mm.clone(),
                fn_proto,
            )),
        );

        // String.prototype.padEnd (ES2017)
        string_proto.define_property(
            PropertyKey::string("padEnd"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |this_val, args, _ncx| {
                    let s = this_string_value(this_val)?;
                    let target_len = args
                        .first()
                        .and_then(|v| v.as_number())
                        .unwrap_or(0.0) as usize;
                    let str_val = s.as_str();
                    if target_len <= str_val.len() {
                        return Ok(Value::string(s));
                    }
                    let fill_str = args
                        .get(1)
                        .and_then(|v| {
                            if v.is_undefined() {
                                None
                            } else {
                                v.as_string()
                            }
                        })
                        .map(|s| s.as_str().to_string())
                        .unwrap_or_else(|| " ".to_string());
                    if fill_str.is_empty() {
                        return Ok(Value::string(s));
                    }
                    let pad_len = target_len - str_val.len();
                    let pad = fill_str.repeat((pad_len / fill_str.len()) + 1);
                    let result = format!("{}{}", str_val, &pad[..pad_len]);
                    Ok(Value::string(JsString::intern(&result)))
                },
                mm.clone(),
                fn_proto,
            )),
        );

        // String.prototype.at (ES2022)
        string_proto.define_property(
            PropertyKey::string("at"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |this_val, args, _ncx| {
                    let s = this_string_value(this_val)?;
                    let chars: Vec<char> = s.as_str().chars().collect();
                    let len = chars.len() as i64;
                    let idx = args
                        .first()
                        .and_then(|v| v.as_number())
                        .unwrap_or(0.0) as i64;
                    let actual = if idx < 0 { len + idx } else { idx };
                    if actual < 0 || actual >= len {
                        return Ok(Value::undefined());
                    }
                    Ok(Value::string(JsString::intern(&chars[actual as usize].to_string())))
                },
                mm.clone(),
                fn_proto,
            )),
        );

        // String.prototype.indexOf
        string_proto.define_property(
            PropertyKey::string("indexOf"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |this_val, args, _ncx| {
                    let s = this_string_value(this_val)?;
                    let search = args
                        .first()
                        .and_then(|v| v.as_string())
                        .ok_or_else(|| "indexOf requires a search string".to_string())?;
                    let from_index = args
                        .get(1)
                        .and_then(|v| v.as_number())
                        .unwrap_or(0.0)
                        .max(0.0) as usize;
                    let str_val = s.as_str();
                    if from_index >= str_val.len() {
                        return Ok(Value::number(-1.0));
                    }
                    match str_val[from_index..].find(search.as_str()) {
                        Some(pos) => Ok(Value::number((from_index + pos) as f64)),
                        None => Ok(Value::number(-1.0)),
                    }
                },
                mm.clone(),
                fn_proto,
            )),
        );

        // String.prototype.lastIndexOf
        string_proto.define_property(
            PropertyKey::string("lastIndexOf"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |this_val, args, _ncx| {
                    let s = this_string_value(this_val)?;
                    let search = args
                        .first()
                        .and_then(|v| v.as_string())
                        .ok_or_else(|| "lastIndexOf requires a search string".to_string())?;
                    let str_val = s.as_str();
                    match str_val.rfind(search.as_str()) {
                        Some(pos) => Ok(Value::number(pos as f64)),
                        None => Ok(Value::number(-1.0)),
                    }
                },
                mm.clone(),
                fn_proto,
            )),
        );

        // String.prototype.concat
        string_proto.define_property(
            PropertyKey::string("concat"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |this_val, args, ncx| {
                    let s = this_string_value(this_val)?;
                    let mut result = s.as_str().to_string();
                    for arg in args {
                        let arg_str = ncx.to_string_value(arg)?;
                        result.push_str(&arg_str);
                    }
                    Ok(Value::string(JsString::intern(&result)))
                },
                mm.clone(),
                fn_proto,
            )),
        );

        // String.prototype.split (ES2026 §22.1.3.22)
        string_proto.define_property(
            PropertyKey::string("split"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |this_val, args, ncx| {
                    let s = this_string_value(this_val)?;

                    // If separator is a RegExp, delegate to Symbol.split
                    if let Some(sep) = args.first() {
                        if let Some(regex) = sep.as_regex() {
                            let method = regex
                                .object
                                .get(&PropertyKey::Symbol(crate::intrinsics::well_known::split_symbol()))
                                .unwrap_or_else(Value::undefined);
                            if let Some(func) = method.as_native_function() {
                                let mut sym_args = vec![Value::string(s.clone())];
                                if let Some(limit) = args.get(1) {
                                    sym_args.push(limit.clone());
                                }
                                return func(sep, &sym_args, ncx);
                            }
                        }
                    }

                    let str_val = s.as_str();
                    let separator = args.first();
                    let limit = args
                        .get(1)
                        .and_then(|v| {
                            if v.is_undefined() {
                                None
                            } else {
                                v.as_number()
                            }
                        })
                        .map(|n| n as usize);

                    let parts: Vec<&str> = if let Some(sep) = separator {
                        if sep.is_undefined() {
                            vec![str_val]
                        } else if let Some(sep_str) = sep.as_string() {
                            if sep_str.as_str().is_empty() {
                                str_val.chars().map(|_| "").collect()
                            } else {
                                str_val.split(sep_str.as_str()).collect()
                            }
                        } else {
                            vec![str_val]
                        }
                    } else {
                        vec![str_val]
                    };

                    let result_len = limit.unwrap_or(parts.len()).min(parts.len());
                    let result = GcRef::new(JsObject::array(result_len, ncx.memory_manager().clone()));
                    for (i, part) in parts.iter().take(result_len).enumerate() {
                        let _ = result.set(
                            PropertyKey::Index(i as u32),
                            Value::string(JsString::intern(part)),
                        );
                    }
                    Ok(Value::array(result))
                },
                mm.clone(),
                fn_proto,
            )),
        );

        // String.prototype.replace (ES2026 §22.1.3.19)
        string_proto.define_property(
            PropertyKey::string("replace"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |this_val, args, ncx| {
                    let s = this_string_value(this_val)?;

                    // If searchValue is a RegExp, delegate to Symbol.replace
                    if let Some(search_val) = args.first() {
                        if let Some(regex) = search_val.as_regex() {
                            let method = regex
                                .object
                                .get(&PropertyKey::Symbol(crate::intrinsics::well_known::replace_symbol()))
                                .unwrap_or_else(Value::undefined);
                            if let Some(func) = method.as_native_function() {
                                let mut sym_args = vec![Value::string(s.clone())];
                                if let Some(replacement) = args.get(1) {
                                    sym_args.push(replacement.clone());
                                }
                                return func(search_val, &sym_args, ncx);
                            }
                        }
                    }

                    // String-based replace (first occurrence only)
                    let str_val = s.as_str();
                    let search = args
                        .first()
                        .and_then(|v| v.as_string())
                        .map(|s| s.as_str().to_string())
                        .unwrap_or_default();
                    let replacement = args
                        .get(1)
                        .and_then(|v| v.as_string())
                        .map(|s| s.as_str().to_string())
                        .unwrap_or_default();

                    if let Some(pos) = str_val.find(&search) {
                        let result = format!(
                            "{}{}{}",
                            &str_val[..pos],
                            replacement,
                            &str_val[pos + search.len()..]
                        );
                        Ok(Value::string(JsString::intern(&result)))
                    } else {
                        Ok(Value::string(s))
                    }
                },
                mm.clone(),
                fn_proto,
            )),
        );

        // String.prototype.replaceAll (ES2021 §22.1.3.20)
        string_proto.define_property(
            PropertyKey::string("replaceAll"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |this_val, args, ncx| {
                    let s = this_string_value(this_val)?;

                    // If searchValue is a RegExp, it must have global flag
                    if let Some(search_val) = args.first() {
                        if let Some(regex) = search_val.as_regex() {
                            if !regex.flags.contains('g') {
                                return Err(VmError::type_error(
                                    "String.prototype.replaceAll called with a non-global RegExp argument",
                                ));
                            }
                            let method = regex
                                .object
                                .get(&PropertyKey::Symbol(crate::intrinsics::well_known::replace_symbol()))
                                .unwrap_or_else(Value::undefined);
                            if let Some(func) = method.as_native_function() {
                                let mut sym_args = vec![Value::string(s.clone())];
                                if let Some(replacement) = args.get(1) {
                                    sym_args.push(replacement.clone());
                                }
                                return func(search_val, &sym_args, ncx);
                            }
                        }
                    }

                    // String-based replaceAll
                    let str_val = s.as_str();
                    let search = args
                        .first()
                        .and_then(|v| v.as_string())
                        .map(|s| s.as_str().to_string())
                        .unwrap_or_default();
                    let replacement = args
                        .get(1)
                        .and_then(|v| v.as_string())
                        .map(|s| s.as_str().to_string())
                        .unwrap_or_default();

                    let result = str_val.replace(&search, &replacement);
                    Ok(Value::string(JsString::intern(&result)))
                },
                mm.clone(),
                fn_proto,
            )),
        );

        // String.prototype.search (ES2026 §22.1.3.21)
        string_proto.define_property(
            PropertyKey::string("search"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |this_val, args, ncx| {
                    let s = this_string_value(this_val)?;

                    // If regexp is a RegExp, delegate to Symbol.search
                    if let Some(search_val) = args.first() {
                        if let Some(regex) = search_val.as_regex() {
                            let method = regex
                                .object
                                .get(&PropertyKey::Symbol(crate::intrinsics::well_known::search_symbol()))
                                .unwrap_or_else(Value::undefined);
                            if let Some(func) = method.as_native_function() {
                                let sym_args = vec![Value::string(s.clone())];
                                return func(search_val, &sym_args, ncx);
                            }
                        }
                    }

                    // String-based search (indexOf behavior)
                    let str_val = s.as_str();
                    let search = args
                        .first()
                        .and_then(|v| v.as_string())
                        .map(|s| s.as_str().to_string())
                        .unwrap_or_default();
                    match str_val.find(&search) {
                        Some(pos) => Ok(Value::int32(pos as i32)),
                        None => Ok(Value::int32(-1)),
                    }
                },
                mm.clone(),
                fn_proto,
            )),
        );

        // String.prototype.match (ES2026 §22.1.3.12)
        string_proto.define_property(
            PropertyKey::string("match"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |this_val, args, ncx| {
                    let s = this_string_value(this_val)?;

                    // If regexp is a RegExp, delegate to Symbol.match
                    if let Some(search_val) = args.first() {
                        if let Some(regex) = search_val.as_regex() {
                            let method = regex
                                .object
                                .get(&PropertyKey::Symbol(crate::intrinsics::well_known::match_symbol()))
                                .unwrap_or_else(Value::undefined);
                            if let Some(func) = method.as_native_function() {
                                let sym_args = vec![Value::string(s.clone())];
                                return func(search_val, &sym_args, ncx);
                            }
                        }
                    }

                    // String-based match: create a non-global RegExp and delegate
                    // For now, simple indexOf-based fallback
                    let str_val = s.as_str();
                    let search = args
                        .first()
                        .and_then(|v| v.as_string())
                        .map(|s| s.as_str().to_string())
                        .unwrap_or_default();
                    match str_val.find(&search) {
                        Some(pos) => {
                            let arr = GcRef::new(JsObject::array(1, ncx.memory_manager().clone()));
                            let _ = arr.set(PropertyKey::Index(0), Value::string(JsString::intern(&search)));
                            let _ = arr.set(PropertyKey::string("index"), Value::number(pos as f64));
                            let _ = arr.set(PropertyKey::string("input"), Value::string(s.clone()));
                            let _ = arr.set(PropertyKey::string("groups"), Value::undefined());
                            Ok(Value::array(arr))
                        }
                        None => Ok(Value::null()),
                    }
                },
                mm.clone(),
                fn_proto,
            )),
        );

        // String.prototype.matchAll (ES2020 §22.1.3.13)
        string_proto.define_property(
            PropertyKey::string("matchAll"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |this_val, args, ncx| {
                    let s = this_string_value(this_val)?;

                    // If regexp is a RegExp, delegate to Symbol.matchAll
                    if let Some(search_val) = args.first() {
                        if let Some(regex) = search_val.as_regex() {
                            if !regex.flags.contains('g') {
                                return Err(VmError::type_error(
                                    "String.prototype.matchAll called with a non-global RegExp argument",
                                ));
                            }
                            let method = regex
                                .object
                                .get(&PropertyKey::Symbol(crate::intrinsics::well_known::match_all_symbol()))
                                .unwrap_or_else(Value::undefined);
                            if let Some(func) = method.as_native_function() {
                                let sym_args = vec![Value::string(s.clone())];
                                return func(search_val, &sym_args, ncx);
                            }
                        }
                    }

                    // Per spec §22.1.3.13: if argument is not a RegExp, create
                    // a new RegExp from ToString(regexp) with "g" flag and
                    // call its [@@matchAll]
                    let search_val = args.first().cloned().unwrap_or(Value::undefined());
                    let search_str = ncx.to_string_value(&search_val)?;
                    // Create a new global regex from the string
                    let regexp_ctor = ncx.ctx.get_global("RegExp");
                    if let Some(ctor) = regexp_ctor {
                        let ctor_args = [
                            Value::string(JsString::intern(&search_str)),
                            Value::string(JsString::intern("g")),
                        ];
                        let regex_val = ncx.call_function_construct(&ctor, Value::undefined(), &ctor_args)?;
                        // Call [@@matchAll] on the new regex
                        let rx_obj = regex_val.as_regex().map(|r| r.object.clone())
                            .or_else(|| regex_val.as_object());
                        if let Some(obj) = rx_obj {
                            let method = obj
                                .get(&PropertyKey::Symbol(crate::intrinsics::well_known::match_all_symbol()))
                                .unwrap_or_else(Value::undefined);
                            if method.is_callable() {
                                return ncx.call_function(&method, regex_val, &[Value::string(s.clone())]);
                            }
                        }
                    }
                    // Fallback: return empty iterator
                    let arr = GcRef::new(JsObject::array(0, ncx.memory_manager().clone()));
                    Ok(Value::array(arr))
                },
                mm.clone(),
                fn_proto,
            )),
        );

    // String.prototype[Symbol.iterator]
    let iter_proto_for_symbol = iterator_proto;
    let mm_for_symbol = mm.clone();
    let fn_proto_for_symbol = fn_proto;
    string_proto.define_property(
        PropertyKey::Symbol(symbol_iterator),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            move |this_val, _args, ncx| {
                make_string_iterator(this_val, ncx.memory_manager().clone(), fn_proto_for_symbol, iter_proto_for_symbol)
            },
            mm_for_symbol,
            fn_proto,
        )),
    );
}
