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

/// Wire all String.prototype methods to the prototype object
pub fn init_string_prototype(
    string_proto: GcRef<JsObject>,
    fn_proto: GcRef<JsObject>,
    mm: &Arc<MemoryManager>,
) {
        string_proto.define_property(
            PropertyKey::string("toString"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |this_val, _args, _mm| {
                    if let Some(s) = this_val.as_string() {
                        Ok(Value::string(s))
                    } else {
                        Ok(Value::string(JsString::intern(&format!("{:?}", this_val))))
                    }
                },
                mm.clone(),
                fn_proto,
            )),
        );
        string_proto.define_property(
            PropertyKey::string("valueOf"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |this_val, _args, _mm| Ok(this_val.clone()),
                mm.clone(),
                fn_proto,
            )),
        );

        // String.prototype.length (getter)
        string_proto.define_property(
            PropertyKey::string("length"),
            PropertyDescriptor::getter(Value::native_function_with_proto(
                |this_val, _args, _mm| {
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
                |this_val, args, _mm| {
                    let s = this_val
                        .as_string()
                        .ok_or_else(|| "String.prototype.charAt: not a string".to_string())?;
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
                |this_val, args, _mm| {
                    let s = this_val
                        .as_string()
                        .ok_or_else(|| "String.prototype.charCodeAt: not a string".to_string())?;
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
                |this_val, args, _mm| {
                    let s = this_val
                        .as_string()
                        .ok_or_else(|| "String.prototype.slice: not a string".to_string())?;
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
                |this_val, args, _mm| {
                    let s = this_val
                        .as_string()
                        .ok_or_else(|| "String.prototype.substring: not a string".to_string())?;
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
                |this_val, _args, _mm| {
                    let s = this_val
                        .as_string()
                        .ok_or_else(|| "String.prototype.toLowerCase: not a string".to_string())?;
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
                |this_val, _args, _mm| {
                    let s = this_val
                        .as_string()
                        .ok_or_else(|| "String.prototype.toUpperCase: not a string".to_string())?;
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
                |this_val, _args, _mm| {
                    let s = this_val
                        .as_string()
                        .ok_or_else(|| "String.prototype.trim: not a string".to_string())?;
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
                |this_val, _args, _mm| {
                    let s = this_val
                        .as_string()
                        .ok_or_else(|| "String.prototype.trimStart: not a string".to_string())?;
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
                |this_val, _args, _mm| {
                    let s = this_val
                        .as_string()
                        .ok_or_else(|| "String.prototype.trimEnd: not a string".to_string())?;
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
                |this_val, args, _mm| {
                    let s = this_val
                        .as_string()
                        .ok_or_else(|| "String.prototype.startsWith: not a string".to_string())?;
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
                |this_val, args, _mm| {
                    let s = this_val
                        .as_string()
                        .ok_or_else(|| "String.prototype.endsWith: not a string".to_string())?;
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
                |this_val, args, _mm| {
                    let s = this_val
                        .as_string()
                        .ok_or_else(|| "String.prototype.includes: not a string".to_string())?;
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
                |this_val, args, _mm| {
                    let s = this_val
                        .as_string()
                        .ok_or_else(|| "String.prototype.repeat: not a string".to_string())?;
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
                |this_val, args, _mm| {
                    let s = this_val
                        .as_string()
                        .ok_or_else(|| "String.prototype.padStart: not a string".to_string())?;
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
                |this_val, args, _mm| {
                    let s = this_val
                        .as_string()
                        .ok_or_else(|| "String.prototype.padEnd: not a string".to_string())?;
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
                |this_val, args, _mm| {
                    let s = this_val
                        .as_string()
                        .ok_or_else(|| "String.prototype.at: not a string".to_string())?;
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
                |this_val, args, _mm| {
                    let s = this_val
                        .as_string()
                        .ok_or_else(|| "String.prototype.indexOf: not a string".to_string())?;
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
                |this_val, args, _mm| {
                    let s = this_val
                        .as_string()
                        .ok_or_else(|| "String.prototype.lastIndexOf: not a string".to_string())?;
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
                |this_val, args, _mm| {
                    let s = this_val
                        .as_string()
                        .ok_or_else(|| "String.prototype.concat: not a string".to_string())?;
                    let mut result = s.as_str().to_string();
                    for arg in args {
                        if let Some(s) = arg.as_string() {
                            result.push_str(s.as_str());
                        } else if let Some(n) = arg.as_number() {
                            result.push_str(&n.to_string());
                        } else if let Some(b) = arg.as_boolean() {
                            result.push_str(if b { "true" } else { "false" });
                        } else if arg.is_null() {
                            result.push_str("null");
                        } else if arg.is_undefined() {
                            result.push_str("undefined");
                        }
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
                |this_val, args, mm_inner| {
                    let s = this_val
                        .as_string()
                        .ok_or_else(|| "String.prototype.split: not a string".to_string())?;

                    // If separator is a RegExp, delegate to Symbol.split
                    if let Some(sep) = args.first() {
                        if let Some(regex) = sep.as_regex() {
                            let method = regex
                                .object
                                .get(&PropertyKey::Symbol(crate::intrinsics::well_known::SPLIT))
                                .unwrap_or_else(Value::undefined);
                            if let Some(func) = method.as_native_function() {
                                let mut sym_args = vec![Value::string(s.clone())];
                                if let Some(limit) = args.get(1) {
                                    sym_args.push(limit.clone());
                                }
                                return func(sep, &sym_args, mm_inner);
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
                    let result = GcRef::new(JsObject::array(result_len, mm_inner));
                    for (i, part) in parts.iter().take(result_len).enumerate() {
                        result.set(
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
                |this_val, args, mm_inner| {
                    let s = this_val
                        .as_string()
                        .ok_or_else(|| "String.prototype.replace: not a string".to_string())?;

                    // If searchValue is a RegExp, delegate to Symbol.replace
                    if let Some(search_val) = args.first() {
                        if let Some(regex) = search_val.as_regex() {
                            let method = regex
                                .object
                                .get(&PropertyKey::Symbol(crate::intrinsics::well_known::REPLACE))
                                .unwrap_or_else(Value::undefined);
                            if let Some(func) = method.as_native_function() {
                                let mut sym_args = vec![Value::string(s.clone())];
                                if let Some(replacement) = args.get(1) {
                                    sym_args.push(replacement.clone());
                                }
                                return func(search_val, &sym_args, mm_inner);
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
                |this_val, args, mm_inner| {
                    let s = this_val
                        .as_string()
                        .ok_or_else(|| "String.prototype.replaceAll: not a string".to_string())?;

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
                                .get(&PropertyKey::Symbol(crate::intrinsics::well_known::REPLACE))
                                .unwrap_or_else(Value::undefined);
                            if let Some(func) = method.as_native_function() {
                                let mut sym_args = vec![Value::string(s.clone())];
                                if let Some(replacement) = args.get(1) {
                                    sym_args.push(replacement.clone());
                                }
                                return func(search_val, &sym_args, mm_inner);
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
                |this_val, args, mm_inner| {
                    let s = this_val
                        .as_string()
                        .ok_or_else(|| "String.prototype.search: not a string".to_string())?;

                    // If regexp is a RegExp, delegate to Symbol.search
                    if let Some(search_val) = args.first() {
                        if let Some(regex) = search_val.as_regex() {
                            let method = regex
                                .object
                                .get(&PropertyKey::Symbol(crate::intrinsics::well_known::SEARCH))
                                .unwrap_or_else(Value::undefined);
                            if let Some(func) = method.as_native_function() {
                                let sym_args = vec![Value::string(s.clone())];
                                return func(search_val, &sym_args, mm_inner);
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
                |this_val, args, mm_inner| {
                    let s = this_val
                        .as_string()
                        .ok_or_else(|| "String.prototype.match: not a string".to_string())?;

                    // If regexp is a RegExp, delegate to Symbol.match
                    if let Some(search_val) = args.first() {
                        if let Some(regex) = search_val.as_regex() {
                            let method = regex
                                .object
                                .get(&PropertyKey::Symbol(crate::intrinsics::well_known::MATCH))
                                .unwrap_or_else(Value::undefined);
                            if let Some(func) = method.as_native_function() {
                                let sym_args = vec![Value::string(s.clone())];
                                return func(search_val, &sym_args, mm_inner);
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
                            let arr = GcRef::new(JsObject::array(1, mm_inner));
                            arr.set(PropertyKey::Index(0), Value::string(JsString::intern(&search)));
                            arr.set(PropertyKey::string("index"), Value::number(pos as f64));
                            arr.set(PropertyKey::string("input"), Value::string(s.clone()));
                            arr.set(PropertyKey::string("groups"), Value::undefined());
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
                |this_val, args, mm_inner| {
                    let s = this_val
                        .as_string()
                        .ok_or_else(|| "String.prototype.matchAll: not a string".to_string())?;

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
                                .get(&PropertyKey::Symbol(crate::intrinsics::well_known::MATCH_ALL))
                                .unwrap_or_else(Value::undefined);
                            if let Some(func) = method.as_native_function() {
                                let sym_args = vec![Value::string(s.clone())];
                                return func(search_val, &sym_args, mm_inner);
                            }
                        }
                    }

                    // String-based matchAll: find all occurrences of string
                    let str_val = s.as_str();
                    let search = args
                        .first()
                        .and_then(|v| v.as_string())
                        .map(|s| s.as_str().to_string())
                        .unwrap_or_default();
                    let mut results = Vec::new();
                    let mut start = 0;
                    while let Some(pos) = str_val[start..].find(&search) {
                        let abs_pos = start + pos;
                        let match_arr = GcRef::new(JsObject::array(1, mm_inner.clone()));
                        match_arr.set(PropertyKey::Index(0), Value::string(JsString::intern(&search)));
                        match_arr.set(PropertyKey::string("index"), Value::number(abs_pos as f64));
                        match_arr.set(PropertyKey::string("input"), Value::string(s.clone()));
                        match_arr.set(PropertyKey::string("groups"), Value::undefined());
                        results.push(Value::array(match_arr));
                        start = abs_pos + search.len().max(1);
                    }
                    let arr = GcRef::new(JsObject::array(results.len(), mm_inner));
                    for (i, val) in results.into_iter().enumerate() {
                        arr.set(PropertyKey::Index(i as u32), val);
                    }
                    Ok(Value::array(arr))
                },
                mm.clone(),
                fn_proto,
            )),
        );
}
