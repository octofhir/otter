//! RegExp constructor and prototype methods (ES2026 §22.2)
//!
//! Implements all RegExp.prototype methods as intrinsics:
//! - Instance methods: test, exec, toString
//! - Symbol methods: [Symbol.match], [Symbol.matchAll], [Symbol.replace], [Symbol.search], [Symbol.split]
//! - Accessor getters: flags, source, dotAll, global, hasIndices, ignoreCase, multiline, sticky, unicode, unicodeSets
//! - Static: RegExp.escape (ES2026)
//!
//! All methods use inline implementations operating directly on `JsRegExp` values.

use crate::error::VmError;
use crate::gc::GcRef;
use crate::memory::MemoryManager;
use crate::object::{JsObject, PropertyAttributes, PropertyDescriptor, PropertyKey};
use crate::regexp::JsRegExp;
use crate::string::JsString;
use crate::value::Value;
use std::sync::Arc;

// ============================================================================
// Helpers
// ============================================================================

/// Extract the JsRegExp from a this value, returning a TypeError if not a regex.
fn get_regex(this_val: &Value) -> Result<GcRef<JsRegExp>, VmError> {
    this_val
        .as_regex()
        .ok_or_else(|| VmError::type_error("Method called on incompatible receiver"))
}

/// Convert a Value to GcRef<JsString> (for regex input arguments).
fn val_to_js_string(val: &Value) -> GcRef<JsString> {
    if let Some(s) = val.as_string() {
        return s;
    }
    if val.is_undefined() {
        return JsString::intern("undefined");
    }
    if val.is_null() {
        return JsString::intern("null");
    }
    if let Some(b) = val.as_boolean() {
        return JsString::intern(if b { "true" } else { "false" });
    }
    if let Some(n) = val.as_number() {
        if n.is_nan() {
            return JsString::intern("NaN");
        }
        if n.is_infinite() {
            return JsString::intern(if n > 0.0 { "Infinity" } else { "-Infinity" });
        }
        let s = if n == (n as i64) as f64 && n.abs() < 1e15 {
            format!("{}", n as i64)
        } else {
            format!("{}", n)
        };
        return JsString::intern(&s);
    }
    if let Some(i) = val.as_int32() {
        return JsString::intern(&i.to_string());
    }
    JsString::intern("")
}

/// Intern a JsString (get GcRef).
fn intern(s: &str) -> GcRef<JsString> {
    JsString::intern(s)
}

/// Slice a JsString by UTF-16 range and return a new interned JsString.
fn slice_utf16(input: &JsString, start: usize, end: usize) -> GcRef<JsString> {
    input.substring_utf16(start, end)
}

/// Find first match starting at `start` position.
fn find_first(regex: &JsRegExp, input: &JsString, start: usize) -> Option<regress::Match> {
    let re = regex.native_regex.as_ref()?;
    if regex.unicode {
        re.find_from_utf16(input.as_utf16(), start).next()
    } else {
        re.find_from_ucs2(input.as_utf16(), start).next()
    }
}

/// Find all matches in input.
fn find_all(regex: &JsRegExp, input: &JsString) -> Vec<regress::Match> {
    let mut matches = Vec::new();
    let mut start = 0;
    let len = input.len_utf16();

    while start <= len {
        let next = find_first(&*regex, input, start);
        let Some(mat) = next else { break };
        let end = mat.end();
        let begin = mat.start();
        matches.push(mat);
        if end == begin {
            start = end.saturating_add(1);
        } else {
            start = end;
        }
    }
    matches
}

/// Build the exec result array from a match.
fn build_exec_result(
    input: &JsString,
    mat: &regress::Match,
    mm: &Arc<MemoryManager>,
) -> Value {
    let num_groups = mat.captures.len();
    let mut out = Vec::with_capacity(num_groups + 1);

    // Group 0 = full match, groups 1..N = captures
    for idx in 0..=num_groups {
        let val = mat
            .group(idx)
            .map(|range| {
                let slice = &input.as_utf16()[range.start..range.end];
                Value::string(JsString::intern_utf16(slice))
            })
            .unwrap_or(Value::undefined());
        out.push(val);
    }

    let arr = JsObject::array(out.len(), mm.clone());
    for (i, val) in out.into_iter().enumerate() {
        arr.set(PropertyKey::Index(i as u32), val);
    }
    arr.set(
        PropertyKey::string("index"),
        Value::number(mat.start() as f64),
    );
    arr.set(PropertyKey::string("input"), Value::string(JsString::intern(input.as_str())));
    arr.set(PropertyKey::string("groups"), Value::undefined());

    Value::array(GcRef::new(arr))
}

/// Apply replacement patterns ($&, $1..$9, $$, $`, $') on a single match.
fn apply_replacement(
    input: &JsString,
    mat: &regress::Match,
    replacement: &str,
) -> String {
    let mut result = String::new();
    let chars: Vec<char> = replacement.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        if chars[i] == '$' && i + 1 < chars.len() {
            match chars[i + 1] {
                '$' => {
                    result.push('$');
                    i += 2;
                }
                '&' => {
                    // Full match
                    if let Some(range) = mat.group(0) {
                        let slice = &input.as_utf16()[range.start..range.end];
                        result.push_str(&String::from_utf16_lossy(slice));
                    }
                    i += 2;
                }
                '`' => {
                    // Before match
                    let before = &input.as_utf16()[..mat.start()];
                    result.push_str(&String::from_utf16_lossy(before));
                    i += 2;
                }
                '\'' => {
                    // After match
                    let after = &input.as_utf16()[mat.end()..];
                    result.push_str(&String::from_utf16_lossy(after));
                    i += 2;
                }
                d if d.is_ascii_digit() => {
                    // $1..$99
                    let mut num_str = String::new();
                    num_str.push(d);
                    if i + 2 < chars.len() && chars[i + 2].is_ascii_digit() {
                        num_str.push(chars[i + 2]);
                    }
                    // Try two-digit first, then one-digit
                    let (group_num, advance) = if num_str.len() == 2 {
                        let n2: usize = num_str.parse().unwrap_or(0);
                        if n2 > 0 && n2 <= mat.captures.len() {
                            (n2, 3)
                        } else {
                            let n1: usize = num_str[..1].parse().unwrap_or(0);
                            (n1, 2)
                        }
                    } else {
                        let n1: usize = num_str.parse().unwrap_or(0);
                        (n1, 2)
                    };

                    if group_num > 0 {
                        if let Some(range) = mat.group(group_num) {
                            let slice = &input.as_utf16()[range.start..range.end];
                            result.push_str(&String::from_utf16_lossy(slice));
                        }
                    } else {
                        result.push('$');
                        result.push(d);
                    }
                    i += advance;
                }
                other => {
                    result.push('$');
                    result.push(other);
                    i += 2;
                }
            }
        } else {
            result.push(chars[i]);
            i += 1;
        }
    }
    result
}

// ============================================================================
// RegExp.prototype initialization
// ============================================================================

/// Initialize RegExp.prototype methods (ES2026 §22.2.5).
pub fn init_regexp_prototype(
    regexp_proto: GcRef<JsObject>,
    fn_proto: GcRef<JsObject>,
    mm: &Arc<MemoryManager>,
) {
    // ====================================================================
    // RegExp.prototype.test(string) §22.2.5.13
    // ====================================================================
    regexp_proto.define_property(
        PropertyKey::string("test"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, _ncx| {
                let regex = get_regex(this_val)?;
                let input = args
                    .first()
                    .map(val_to_js_string)
                    .unwrap_or_else(|| JsString::intern("undefined"));
                Ok(Value::boolean(find_first(&*regex, &input, 0).is_some()))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // ====================================================================
    // RegExp.prototype.exec(string) §22.2.5.2
    // ====================================================================
    regexp_proto.define_property(
        PropertyKey::string("exec"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, ncx| {
                let regex = get_regex(this_val)?;
                let input = args
                    .first()
                    .map(val_to_js_string)
                    .unwrap_or_else(|| JsString::intern("undefined"));
                match find_first(&*regex, &input, 0) {
                    Some(mat) => Ok(build_exec_result(&input, &mat, ncx.memory_manager())),
                    None => Ok(Value::null()),
                }
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // ====================================================================
    // RegExp.prototype.toString() §22.2.5.14
    // ====================================================================
    regexp_proto.define_property(
        PropertyKey::string("toString"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, _args, _ncx| {
                let regex = get_regex(this_val)?;
                let source = if regex.pattern.is_empty() {
                    "(?:)"
                } else {
                    &regex.pattern
                };
                Ok(Value::string(intern(&format!(
                    "/{}/{}",
                    source, regex.flags
                ))))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // ====================================================================
    // RegExp.prototype[Symbol.match](string) §22.2.5.6
    // ====================================================================
    regexp_proto.define_property(
        PropertyKey::Symbol(crate::intrinsics::well_known::match_symbol()),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, ncx| {
                let regex = get_regex(this_val)?;
                let input = args
                    .first()
                    .map(val_to_js_string)
                    .unwrap_or_else(|| JsString::intern("undefined"));

                let is_global = regex.flags.contains('g');

                if is_global {
                    // Global: return array of all match strings
                    let matches = find_all(&*regex, &input);
                    if matches.is_empty() {
                        return Ok(Value::null());
                    }
                    let arr = JsObject::array(matches.len(), ncx.memory_manager().clone());
                    for (i, mat) in matches.iter().enumerate() {
                        let range = mat.range();
                        let slice = &input.as_utf16()[range.start..range.end];
                        arr.set(
                            PropertyKey::Index(i as u32),
                            Value::string(JsString::intern_utf16(slice)),
                        );
                    }
                    Ok(Value::array(GcRef::new(arr)))
                } else {
                    // Non-global: same as exec
                    match find_first(&*regex, &input, 0) {
                        Some(mat) => Ok(build_exec_result(&input, &mat, ncx.memory_manager())),
                        None => Ok(Value::null()),
                    }
                }
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // ====================================================================
    // RegExp.prototype[Symbol.matchAll](string) §22.2.5.7
    // ====================================================================
    regexp_proto.define_property(
        PropertyKey::Symbol(crate::intrinsics::well_known::match_all_symbol()),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, ncx| {
                let regex = get_regex(this_val)?;
                let input = args
                    .first()
                    .map(val_to_js_string)
                    .unwrap_or_else(|| JsString::intern("undefined"));

                if !regex.flags.contains('g') {
                    return Err(VmError::type_error(
                        "String.prototype.matchAll called with a non-global RegExp argument",
                    ));
                }

                let matches = find_all(&*regex, &input);
                let arr = JsObject::array(matches.len(), ncx.memory_manager().clone());
                for (i, mat) in matches.iter().enumerate() {
                    let exec_result = build_exec_result(&input, mat, ncx.memory_manager());
                    arr.set(PropertyKey::Index(i as u32), exec_result);
                }
                Ok(Value::array(GcRef::new(arr)))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // ====================================================================
    // RegExp.prototype[Symbol.replace](string, replaceValue) §22.2.5.8
    // ====================================================================
    regexp_proto.define_property(
        PropertyKey::Symbol(crate::intrinsics::well_known::replace_symbol()),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, _ncx| {
                let regex = get_regex(this_val)?;
                let input = args
                    .first()
                    .map(val_to_js_string)
                    .unwrap_or_else(|| JsString::intern("undefined"));
                let replacement = args
                    .get(1)
                    .and_then(|v| v.as_string())
                    .map(|s| s.as_str().to_string())
                    .unwrap_or_default();

                let is_global = regex.flags.contains('g');
                let matches = if is_global {
                    find_all(&*regex, &input)
                } else {
                    find_first(&*regex, &input, 0)
                        .into_iter()
                        .collect()
                };

                if matches.is_empty() {
                    return Ok(Value::string(input.clone()));
                }

                let mut result = String::new();
                let mut last_end: usize = 0;
                let utf16 = input.as_utf16();

                for mat in &matches {
                    // Append text before match
                    let before = &utf16[last_end..mat.start()];
                    result.push_str(&String::from_utf16_lossy(before));

                    // Apply replacement
                    result.push_str(&apply_replacement(&input, mat, &replacement));

                    last_end = mat.end();
                }

                // Append remaining text
                let after = &utf16[last_end..];
                result.push_str(&String::from_utf16_lossy(after));

                Ok(Value::string(intern(&result)))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // ====================================================================
    // RegExp.prototype[Symbol.search](string) §22.2.5.9
    // ====================================================================
    regexp_proto.define_property(
        PropertyKey::Symbol(crate::intrinsics::well_known::search_symbol()),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, _ncx| {
                let regex = get_regex(this_val)?;
                let input = args
                    .first()
                    .map(val_to_js_string)
                    .unwrap_or_else(|| JsString::intern("undefined"));
                match find_first(&*regex, &input, 0) {
                    Some(mat) => Ok(Value::int32(mat.start() as i32)),
                    None => Ok(Value::int32(-1)),
                }
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // ====================================================================
    // RegExp.prototype[Symbol.split](string, limit?) §22.2.5.11
    // ====================================================================
    regexp_proto.define_property(
        PropertyKey::Symbol(crate::intrinsics::well_known::split_symbol()),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, ncx| {
                let regex = get_regex(this_val)?;
                let input = args
                    .first()
                    .map(val_to_js_string)
                    .unwrap_or_else(|| JsString::intern("undefined"));
                let limit = args
                    .get(1)
                    .and_then(|v| {
                        if v.is_undefined() {
                            None
                        } else {
                            v.as_number().map(|n| n as usize)
                                .or_else(|| v.as_int32().map(|n| n as usize))
                        }
                    });

                let input_len = input.len_utf16();
                let mut parts: Vec<Value> = Vec::new();
                let mut last_end: usize = 0;

                let matches = find_all(&*regex, &input);
                for mat in &matches {
                    if let Some(lim) = limit {
                        if parts.len() >= lim {
                            break;
                        }
                    }
                    let start = mat.start();
                    // Add segment before match
                    let seg = slice_utf16(&input, last_end, start);
                    parts.push(Value::string(seg));

                    // Add capture groups per spec
                    for cap_idx in 1..=mat.captures.len() {
                        if let Some(lim) = limit {
                            if parts.len() >= lim {
                                break;
                            }
                        }
                        let cap_val = mat
                            .group(cap_idx)
                            .map(|range| {
                                let slice = &input.as_utf16()[range.start..range.end];
                                Value::string(JsString::intern_utf16(slice))
                            })
                            .unwrap_or(Value::undefined());
                        parts.push(cap_val);
                    }

                    last_end = mat.end();
                }

                // Add trailing segment
                if limit.map(|l| parts.len() < l).unwrap_or(true) {
                    let seg = slice_utf16(&input, last_end, input_len);
                    parts.push(Value::string(seg));
                }

                let arr = JsObject::array(parts.len(), ncx.memory_manager().clone());
                for (i, part) in parts.into_iter().enumerate() {
                    arr.set(PropertyKey::Index(i as u32), part);
                }
                Ok(Value::array(GcRef::new(arr)))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // ====================================================================
    // Accessor getters for flags/source/individual flags
    // These are prototype accessors per spec. Instance own data properties
    // (set by JsRegExp::new) shadow these in normal lookup.
    // ====================================================================

    let accessor_attrs = PropertyAttributes {
        writable: false,
        enumerable: false,
        configurable: true,
    };

    // RegExp.prototype.flags §22.2.5.3 (accessor getter)
    regexp_proto.define_property(
        PropertyKey::string("flags"),
        PropertyDescriptor::Accessor {
            get: Some(Value::native_function_with_proto(
                |this_val, _args, _ncx| {
                    let regex = get_regex(this_val)?;
                    Ok(Value::string(intern(&regex.flags)))
                },
                mm.clone(),
                fn_proto,
            )),
            set: None,
            attributes: accessor_attrs,
        },
    );

    // RegExp.prototype.source §22.2.5.12 (accessor getter)
    regexp_proto.define_property(
        PropertyKey::string("source"),
        PropertyDescriptor::Accessor {
            get: Some(Value::native_function_with_proto(
                |this_val, _args, _ncx| {
                    let regex = get_regex(this_val)?;
                    let source = if regex.pattern.is_empty() {
                        "(?:)"
                    } else {
                        &regex.pattern
                    };
                    Ok(Value::string(intern(source)))
                },
                mm.clone(),
                fn_proto,
            )),
            set: None,
            attributes: accessor_attrs,
        },
    );

    // Individual flag accessors
    let define_flag_getter =
        |name: &str, flag_char: char| {
            regexp_proto.define_property(
                PropertyKey::string(name),
                PropertyDescriptor::Accessor {
                    get: Some(Value::native_function_with_proto(
                        move |this_val, _args, _ncx| {
                            let regex = get_regex(this_val)?;
                            Ok(Value::boolean(regex.flags.contains(flag_char)))
                        },
                        mm.clone(),
                        fn_proto,
                    )),
                    set: None,
                    attributes: accessor_attrs,
                },
            );
        };

    define_flag_getter("global", 'g');
    define_flag_getter("ignoreCase", 'i');
    define_flag_getter("multiline", 'm');
    define_flag_getter("dotAll", 's');
    define_flag_getter("sticky", 'y');
    define_flag_getter("unicode", 'u');
    define_flag_getter("unicodeSets", 'v');
    define_flag_getter("hasIndices", 'd');
}

// ============================================================================
// RegExp constructor
// ============================================================================

/// Create the RegExp constructor function (ES2026 §22.2.3.1).
///
/// `RegExp(pattern, flags)`:
/// - If `pattern` is a RegExp, extract its pattern/flags (use provided flags if given)
/// - Otherwise convert pattern to string, flags to string
/// - Create a new JsRegExp value
///
/// The `regexp_proto` parameter is the intrinsic `%RegExp.prototype%` object, captured
/// by the closure so that both `new RegExp(...)` and bare `RegExp(...)` calls
/// (which the compiler emits for regex literals) create instances with the correct prototype.
pub fn create_regexp_constructor(
    regexp_proto: GcRef<JsObject>,
) -> Box<
    dyn Fn(&Value, &[Value], &mut crate::context::NativeContext<'_>) -> Result<Value, VmError> + Send + Sync,
> {
    Box::new(move |_this_val, args, ncx| {
        let pattern_arg = args.first().cloned().unwrap_or(Value::undefined());
        let flags_arg = args.get(1).cloned();

        let (pattern, flags) = if let Some(re) = pattern_arg.as_regex() {
            // Pattern is already a RegExp
            let p = re.pattern.clone();
            let f = match &flags_arg {
                Some(v) if !v.is_undefined() => {
                    if let Some(s) = v.as_string() {
                        s.as_str().to_string()
                    } else {
                        return Err(VmError::type_error("Invalid flags"));
                    }
                }
                _ => re.flags.clone(),
            };
            (p, f)
        } else {
            let p = if pattern_arg.is_undefined() {
                String::new()
            } else if let Some(s) = pattern_arg.as_string() {
                s.as_str().to_string()
            } else if let Some(n) = pattern_arg.as_number() {
                if n == (n as i64) as f64 && n.abs() < 1e15 {
                    format!("{}", n as i64)
                } else {
                    format!("{}", n)
                }
            } else if let Some(b) = pattern_arg.as_boolean() {
                if b { "true" } else { "false" }.to_string()
            } else if pattern_arg.is_null() {
                "null".to_string()
            } else {
                String::new()
            };
            let f = match &flags_arg {
                Some(v) if !v.is_undefined() => {
                    if let Some(s) = v.as_string() {
                        s.as_str().to_string()
                    } else {
                        String::new()
                    }
                }
                _ => String::new(),
            };
            (p, f)
        };

        let regex = GcRef::new(JsRegExp::new(pattern, flags, Some(regexp_proto), ncx.memory_manager().clone()));
        Ok(Value::regex(regex))
    })
}
