//! RegExp constructor and prototype methods (ES2026 §22.2)
//!
//! Implements all RegExp.prototype methods as intrinsics:
//! - Instance methods: test, exec, toString, compile (Annex B)
//! - Symbol methods: [Symbol.match], [Symbol.matchAll], [Symbol.replace], [Symbol.search], [Symbol.split]
//! - Accessor getters: flags, source, dotAll, global, hasIndices, ignoreCase, multiline, sticky, unicode, unicodeSets

use crate::context::NativeContext;
use crate::error::VmError;
use crate::gc::GcRef;
use crate::memory::MemoryManager;
use crate::object::{get_value_full, set_value_full, JsObject, PropertyAttributes, PropertyDescriptor, PropertyKey};
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

/// Get `this` as an object (works for regex values too via their object field).
fn get_this_object(this_val: &Value) -> Result<GcRef<JsObject>, VmError> {
    if let Some(regex) = this_val.as_regex() {
        return Ok(regex.object.clone());
    }
    this_val
        .as_object()
        .ok_or_else(|| VmError::type_error("RegExp method called on non-object"))
}

/// Spec-compliant Get(O, P) - triggers JS getters via NativeContext.
/// Uses the object itself as the receiver for getter calls.
fn obj_get(
    obj: &GcRef<JsObject>,
    key: &str,
    ncx: &mut NativeContext<'_>,
) -> Result<Value, VmError> {
    get_value_full(obj, &PropertyKey::string(key), ncx)
}

/// Spec-compliant Get(O, P) with custom receiver for getter calls.
/// Used when `this_val` is a regex value but we need to read from its inner object.
fn obj_get_with_receiver(
    obj: &GcRef<JsObject>,
    key: &str,
    receiver: Value,
    ncx: &mut NativeContext<'_>,
) -> Result<Value, VmError> {
    let pk = PropertyKey::string(key);
    if let Some(desc) = obj.lookup_property_descriptor(&pk) {
        match desc {
            PropertyDescriptor::Data { value, .. } => Ok(value),
            PropertyDescriptor::Accessor { get, .. } => {
                if let Some(getter) = get {
                    if !getter.is_undefined() {
                        return ncx.call_function(&getter, receiver, &[]);
                    }
                }
                Ok(Value::undefined())
            }
            PropertyDescriptor::Deleted => Ok(Value::undefined()),
        }
    } else {
        Ok(Value::undefined())
    }
}

/// Spec-compliant Set(O, P, V, true) - triggers JS setters via NativeContext.
fn obj_set(
    obj: &GcRef<JsObject>,
    key: &str,
    value: Value,
    ncx: &mut NativeContext<'_>,
) -> Result<(), VmError> {
    set_value_full(obj, &PropertyKey::string(key), value, ncx)
}

/// Set with custom receiver for setter calls.
fn obj_set_with_receiver(
    obj: &GcRef<JsObject>,
    key: &str,
    value: Value,
    receiver: Value,
    ncx: &mut NativeContext<'_>,
) -> Result<(), VmError> {
    let pk = PropertyKey::string(key);
    if let Some(desc) = obj.lookup_property_descriptor(&pk) {
        match desc {
            PropertyDescriptor::Accessor { set, .. } => {
                if let Some(setter) = set {
                    if !setter.is_undefined() {
                        ncx.call_function(&setter, receiver, &[value])?;
                        return Ok(());
                    }
                }
                return Err(VmError::type_error(
                    "Cannot set property which has only a getter",
                ));
            }
            PropertyDescriptor::Data { attributes, .. } => {
                if !attributes.writable {
                    return Err(VmError::type_error(
                        "Cannot assign to read only property",
                    ));
                }
                let _ = obj.set(pk, value);
                return Ok(());
            }
            PropertyDescriptor::Deleted => {}
        }
    }
    let _ = obj.set(pk, value);
    Ok(())
}

/// SameValue comparison per spec (treats NaN === NaN, -0 !== +0)
fn same_value(x: &Value, y: &Value) -> bool {
    // Both numbers: use SameValue semantics
    let xn = x.as_number().or_else(|| x.as_int32().map(|i| i as f64));
    let yn = y.as_number().or_else(|| y.as_int32().map(|i| i as f64));
    if let (Some(xn), Some(yn)) = (xn, yn) {
        if xn.is_nan() && yn.is_nan() {
            return true;
        }
        if xn == 0.0 && yn == 0.0 {
            return xn.to_bits() == yn.to_bits(); // distinguishes -0 and +0
        }
        return xn == yn;
    }
    // For non-numbers, use strict equality
    if x.is_undefined() && y.is_undefined() {
        return true;
    }
    if x.is_null() && y.is_null() {
        return true;
    }
    // String comparison
    if let (Some(xs), Some(ys)) = (x.as_string(), y.as_string()) {
        return xs.as_str() == ys.as_str();
    }
    // Boolean comparison
    if let (Some(xb), Some(yb)) = (x.as_boolean(), y.as_boolean()) {
        return xb == yb;
    }
    // Object identity (same GcRef pointer)
    // For objects/arrays, reference equality
    false
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

/// Get Array.prototype from global context
fn get_array_proto(ncx: &NativeContext<'_>) -> Option<GcRef<JsObject>> {
    ncx.ctx
        .get_global("Array")
        .and_then(|v| v.as_object())
        .and_then(|arr| arr.get(&PropertyKey::string("prototype")))
        .and_then(|v| v.as_object())
}

/// Build the exec result array from a match.
fn build_exec_result(
    input: &JsString,
    mat: &regress::Match,
    has_indices: bool,
    ncx: &NativeContext<'_>,
) -> Value {
    let mm = ncx.memory_manager();
    let num_groups = mat.captures.len();
    let mut out = Vec::with_capacity(num_groups + 1);

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
    if let Some(array_proto) = get_array_proto(ncx) {
        arr.set_prototype(Value::object(array_proto));
    }
    for (i, val) in out.into_iter().enumerate() {
        let _ = arr.set(PropertyKey::Index(i as u32), val);
    }
    let _ = arr.set(
        PropertyKey::string("index"),
        Value::number(mat.start() as f64),
    );
    let _ = arr.set(
        PropertyKey::string("input"),
        Value::string(JsString::intern(input.as_str())),
    );

    // Named capture groups
    let has_named = mat.named_groups().next().is_some();
    if has_named {
        let groups = GcRef::new(JsObject::new(Value::null(), mm.clone()));
        for (name, range) in mat.named_groups() {
            let val = match range {
                Some(range) => {
                    let slice = &input.as_utf16()[range.start..range.end];
                    Value::string(JsString::intern_utf16(slice))
                }
                None => Value::undefined(),
            };
            let _ = groups.set(PropertyKey::string(name), val);
        }
        let _ = arr.set(PropertyKey::string("groups"), Value::object(groups));
    } else {
        let _ = arr.set(PropertyKey::string("groups"), Value::undefined());
    }

    // Match indices (d flag / hasIndices)
    if has_indices {
        let indices_arr = JsObject::array(num_groups + 1, mm.clone());
        if let Some(array_proto) = get_array_proto(ncx) {
            indices_arr.set_prototype(Value::object(array_proto.clone()));
        }
        for idx in 0..=num_groups {
            let index_pair = match mat.group(idx) {
                Some(range) => {
                    let pair = JsObject::array(2, mm.clone());
                    if let Some(ap) = get_array_proto(ncx) {
                        pair.set_prototype(Value::object(ap));
                    }
                    let _ = pair.set(PropertyKey::Index(0), Value::number(range.start as f64));
                    let _ = pair.set(PropertyKey::Index(1), Value::number(range.end as f64));
                    Value::array(GcRef::new(pair))
                }
                None => Value::undefined(),
            };
            let _ = indices_arr.set(PropertyKey::Index(idx as u32), index_pair);
        }
        if has_named {
            let groups_indices = GcRef::new(JsObject::new(Value::null(), mm.clone()));
            for (name, range) in mat.named_groups() {
                let val = match range {
                    Some(range) => {
                        let pair = JsObject::array(2, mm.clone());
                        if let Some(ap) = get_array_proto(ncx) {
                            pair.set_prototype(Value::object(ap));
                        }
                        let _ = pair.set(PropertyKey::Index(0), Value::number(range.start as f64));
                        let _ = pair.set(PropertyKey::Index(1), Value::number(range.end as f64));
                        Value::array(GcRef::new(pair))
                    }
                    None => Value::undefined(),
                };
                let _ = groups_indices.set(PropertyKey::string(name), val);
            }
            let _ = indices_arr.set(PropertyKey::string("groups"), Value::object(groups_indices));
        } else {
            let _ = indices_arr.set(PropertyKey::string("groups"), Value::undefined());
        }
        let _ = arr.set(
            PropertyKey::string("indices"),
            Value::array(GcRef::new(indices_arr)),
        );
    }

    Value::array(GcRef::new(arr))
}

/// Read lastIndex from a regex object, coerce to integer via ToLength
fn get_last_index(regex: &JsRegExp, ncx: &mut NativeContext<'_>) -> Result<f64, VmError> {
    let raw = regex
        .object
        .get(&PropertyKey::string("lastIndex"))
        .unwrap_or(Value::int32(0));
    if let Some(n) = raw.as_number() {
        return Ok(to_length(n));
    }
    if let Some(i) = raw.as_int32() {
        return Ok(to_length(i as f64));
    }
    let n = ncx.to_number_value(&raw)?;
    Ok(to_length(n))
}

/// Read lastIndex from a generic object
fn get_last_index_obj(obj: &GcRef<JsObject>, ncx: &mut NativeContext<'_>) -> Result<f64, VmError> {
    let raw = obj_get(obj, "lastIndex", ncx)?;
    if let Some(n) = raw.as_number() {
        return Ok(to_length(n));
    }
    if let Some(i) = raw.as_int32() {
        return Ok(to_length(i as f64));
    }
    let n = ncx.to_number_value(&raw)?;
    Ok(to_length(n))
}

/// ToLength: clamp to [0, 2^53 - 1] integer
fn to_length(n: f64) -> f64 {
    if n.is_nan() || n <= 0.0 {
        return 0.0;
    }
    let n = n.trunc();
    if n > 9007199254740991.0 {
        9007199254740991.0
    } else {
        n
    }
}

/// ToIntegerOrInfinity
fn to_integer_or_infinity(n: f64) -> f64 {
    if n.is_nan() {
        return 0.0;
    }
    if n == 0.0 || n.is_infinite() {
        return n;
    }
    n.trunc()
}

/// Set lastIndex on a regex object
fn set_last_index(regex: &JsRegExp, idx: f64) -> Result<(), VmError> {
    if let Some(desc) = regex
        .object
        .get_own_property_descriptor(&PropertyKey::string("lastIndex"))
    {
        if !desc.is_writable() {
            return Err(VmError::type_error(
                "Cannot set property lastIndex of regex with non-writable lastIndex",
            ));
        }
    }
    let _ = regex
        .object
        .set(PropertyKey::string("lastIndex"), Value::number(idx));
    Ok(())
}

/// Set lastIndex on a generic object
fn set_last_index_obj(obj: &GcRef<JsObject>, idx: f64) -> Result<(), VmError> {
    if let Some(desc) = obj.get_own_property_descriptor(&PropertyKey::string("lastIndex")) {
        if !desc.is_writable() {
            return Err(VmError::type_error(
                "Cannot set property lastIndex of regex with non-writable lastIndex",
            ));
        }
    }
    let _ = obj.set(PropertyKey::string("lastIndex"), Value::number(idx));
    Ok(())
}

/// Advance index past a Unicode code point if needed
fn advance_string_index(input: &JsString, index: usize, unicode: bool) -> usize {
    if !unicode {
        return index + 1;
    }
    let utf16 = input.as_utf16();
    if index + 1 >= utf16.len() {
        return index + 1;
    }
    let first = utf16[index];
    if (0xD800..=0xDBFF).contains(&first) {
        let second = utf16[index + 1];
        if (0xDC00..=0xDFFF).contains(&second) {
            return index + 2;
        }
    }
    index + 1
}

/// RegExpBuiltinExec (§22.2.7.2) - internal exec on actual JsRegExp
fn regexp_builtin_exec(
    regex: &JsRegExp,
    input: &JsString,
    ncx: &mut NativeContext<'_>,
) -> Result<Value, VmError> {
    let is_global = regex.flags.contains('g');
    let is_sticky = regex.flags.contains('y');
    let has_indices = regex.flags.contains('d');

    // Step 4: ALWAYS read lastIndex (triggers valueOf per spec)
    let last_index = get_last_index(regex, ncx)?;
    let input_len = input.len_utf16();

    if !is_global && !is_sticky {
        // Non-global, non-sticky: search from 0, don't write lastIndex
        return match find_first(regex, input, 0) {
            Some(mat) => Ok(build_exec_result(input, &mat, has_indices, ncx)),
            None => Ok(Value::null()),
        };
    }

    let last_index_int = last_index as usize;

    if last_index_int > input_len {
        set_last_index(regex, 0.0)?;
        return Ok(Value::null());
    }

    let mat = if is_sticky {
        find_first(regex, input, last_index_int).filter(|m| m.start() == last_index_int)
    } else {
        find_first(regex, input, last_index_int)
    };

    match mat {
        Some(m) => {
            set_last_index(regex, m.end() as f64)?;
            Ok(build_exec_result(input, &m, has_indices, ncx))
        }
        None => {
            set_last_index(regex, 0.0)?;
            Ok(Value::null())
        }
    }
}

/// RegExpExec (§22.2.7.1) — calls this.exec() if it exists, falls back to RegExpBuiltinExec
fn regexp_exec(
    this_val: &Value,
    input: &JsString,
    ncx: &mut NativeContext<'_>,
) -> Result<Value, VmError> {
    let obj = get_this_object(this_val)?;
    // 1. Let exec be ? Get(R, "exec")
    let exec = obj_get(&obj, "exec", ncx)?;

    // 2. If IsCallable(exec), then
    if exec.is_callable() {
        let input_val = Value::string(JsString::intern(input.as_str()));
        let result = ncx.call_function(&exec, this_val.clone(), &[input_val])?;
        if !result.is_null() && !result.is_object() && result.as_array().is_none() {
            return Err(VmError::type_error(
                "RegExp exec method returned non-object, non-null value",
            ));
        }
        return Ok(result);
    }

    // 3. If R does not have [[RegExpMatcher]], throw TypeError
    let regex = get_regex(this_val)?;
    regexp_builtin_exec(&regex, input, ncx)
}

/// Apply replacement patterns ($&, $1..$99, $$, $`, $', $<name>) on a single match.
/// Per spec §22.1.3.17.1 GetSubstitution
fn apply_replacement(
    matched: &str,
    input: &JsString,
    position: usize,
    captures: &[Value],
    named_captures: Option<&GcRef<JsObject>>,
    replacement: &str,
    ncx: &mut NativeContext<'_>,
) -> Result<String, VmError> {
    let mut result = String::new();
    let chars: Vec<char> = replacement.chars().collect();
    let tail_pos = position + matched.len();
    let mut i = 0;

    while i < chars.len() {
        if chars[i] == '$' && i + 1 < chars.len() {
            match chars[i + 1] {
                '$' => {
                    result.push('$');
                    i += 2;
                }
                '&' => {
                    result.push_str(matched);
                    i += 2;
                }
                '`' => {
                    let before = &input.as_utf16()[..position];
                    result.push_str(&String::from_utf16_lossy(before));
                    i += 2;
                }
                '\'' => {
                    let utf16 = input.as_utf16();
                    if tail_pos < utf16.len() {
                        let after = &utf16[tail_pos..];
                        result.push_str(&String::from_utf16_lossy(after));
                    }
                    i += 2;
                }
                '<' => {
                    // Named group reference: $<name>
                    if let Some(close) = chars[i + 2..].iter().position(|&c| c == '>') {
                        let name: String = chars[i + 2..i + 2 + close].iter().collect();
                        if let Some(groups) = named_captures {
                            let capture = obj_get(groups, &name, ncx)?;
                            if !capture.is_undefined() {
                                let s = ncx.to_string_value(&capture)?;
                                result.push_str(&s);
                            }
                            // If undefined, append empty string (nothing)
                        } else {
                            // No named captures: $<name> is literal
                            result.push('$');
                            result.push('<');
                            result.push_str(&name);
                            result.push('>');
                        }
                        i += 3 + close; // $< + name + >
                    } else {
                        // No closing >, literal $<
                        result.push('$');
                        result.push('<');
                        i += 2;
                    }
                }
                d if d.is_ascii_digit() => {
                    let mut num_str = String::new();
                    num_str.push(d);
                    if i + 2 < chars.len() && chars[i + 2].is_ascii_digit() {
                        num_str.push(chars[i + 2]);
                    }
                    let m = captures.len(); // number of captures (not including full match)
                    let (group_num, advance) = if num_str.len() == 2 {
                        let n2: usize = num_str.parse().unwrap_or(0);
                        if n2 > 0 && n2 <= m {
                            (n2, 3)
                        } else {
                            let n1: usize = num_str[..1].parse().unwrap_or(0);
                            if n1 > 0 && n1 <= m {
                                (n1, 2)
                            } else {
                                (0, 2) // $n where n > m: literal
                            }
                        }
                    } else {
                        let n1: usize = num_str.parse().unwrap_or(0);
                        if n1 > 0 && n1 <= m {
                            (n1, 2)
                        } else {
                            (0, 2)
                        }
                    };

                    if group_num > 0 {
                        let cap = &captures[group_num - 1];
                        if !cap.is_undefined() {
                            let s = ncx.to_string_value(cap)?;
                            result.push_str(&s);
                        }
                    } else {
                        // n > m or n == 0: literal
                        result.push('$');
                        result.push(d);
                        if advance == 3 {
                            result.push(chars[i + 2]);
                        }
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
    Ok(result)
}

/// Mark a native function value as non-constructor and set name/length
fn set_function_props(fn_val: &Value, name: &str, length: i32) {
    if let Some(obj) = fn_val.native_function_object() {
        obj.define_property(
            PropertyKey::string("name"),
            PropertyDescriptor::function_length(Value::string(JsString::intern(name))),
        );
        obj.define_property(
            PropertyKey::string("length"),
            PropertyDescriptor::function_length(Value::int32(length)),
        );
        obj.define_property(
            PropertyKey::string("__non_constructor"),
            PropertyDescriptor::Data {
                value: Value::boolean(true),
                attributes: PropertyAttributes {
                    writable: false,
                    enumerable: false,
                    configurable: false,
                },
            },
        );
    }
}

/// Helper to define a builtin method on a prototype
fn define_builtin_method<F>(
    proto: &GcRef<JsObject>,
    name: &str,
    length: i32,
    mm: &Arc<MemoryManager>,
    fn_proto: GcRef<JsObject>,
    f: F,
) where
    F: Fn(&Value, &[Value], &mut NativeContext<'_>) -> Result<Value, VmError>
        + Send
        + Sync
        + 'static,
{
    let fn_val = Value::native_function_with_proto(f, mm.clone(), fn_proto);
    set_function_props(&fn_val, name, length);
    proto.define_property(
        PropertyKey::string(name),
        PropertyDescriptor::builtin_method(fn_val),
    );
}

/// Helper to define a Symbol method on a prototype
fn define_symbol_method<F>(
    proto: &GcRef<JsObject>,
    symbol: GcRef<crate::value::Symbol>,
    name: &str,
    length: i32,
    mm: &Arc<MemoryManager>,
    fn_proto: GcRef<JsObject>,
    f: F,
) where
    F: Fn(&Value, &[Value], &mut NativeContext<'_>) -> Result<Value, VmError>
        + Send
        + Sync
        + 'static,
{
    let fn_val = Value::native_function_with_proto(f, mm.clone(), fn_proto);
    set_function_props(&fn_val, name, length);
    proto.define_property(
        PropertyKey::Symbol(symbol),
        PropertyDescriptor::builtin_method(fn_val),
    );
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
    define_builtin_method(
        &regexp_proto,
        "test",
        1,
        mm,
        fn_proto.clone(),
        |this_val, args, ncx| {
            let input_str =
                ncx.to_string_value(args.first().unwrap_or(&Value::undefined()))?;
            let input = JsString::intern(&input_str);
            // Per spec: test calls RegExpExec and returns true if result is not null
            let result = regexp_exec(this_val, &input, ncx)?;
            Ok(Value::boolean(!result.is_null()))
        },
    );

    // ====================================================================
    // RegExp.prototype.exec(string) §22.2.5.2
    // ====================================================================
    define_builtin_method(
        &regexp_proto,
        "exec",
        1,
        mm,
        fn_proto.clone(),
        |this_val, args, ncx| {
            let regex = get_regex(this_val)?;
            let input_str =
                ncx.to_string_value(args.first().unwrap_or(&Value::undefined()))?;
            let input = JsString::intern(&input_str);
            regexp_builtin_exec(&regex, &input, ncx)
        },
    );

    // ====================================================================
    // RegExp.prototype.toString() §22.2.5.14
    // ====================================================================
    define_builtin_method(
        &regexp_proto,
        "toString",
        0,
        mm,
        fn_proto.clone(),
        |this_val, _args, ncx| {
            let obj = get_this_object(this_val)?;
            let source_val = obj_get(&obj, "source", ncx)?;
            let source = ncx.to_string_value(&source_val)?;
            let flags_val = obj_get(&obj, "flags", ncx)?;
            let flags = ncx.to_string_value(&flags_val)?;
            Ok(Value::string(intern(&format!("/{}/{}", source, flags))))
        },
    );

    // ====================================================================
    // RegExp.prototype.compile(pattern, flags) — Annex B §B.2.4
    // ====================================================================
    define_builtin_method(
        &regexp_proto,
        "compile",
        2,
        mm,
        fn_proto.clone(),
        |this_val, args, ncx| {
            let regex = get_regex(this_val)?;
            let pattern_arg = args.first().cloned().unwrap_or(Value::undefined());
            let flags_arg = args.get(1).cloned().unwrap_or(Value::undefined());

            let (pattern, flags) = if let Some(re) = pattern_arg.as_regex() {
                if !flags_arg.is_undefined() {
                    return Err(VmError::type_error(
                        "Cannot supply flags when constructing one RegExp from another",
                    ));
                }
                (re.pattern.clone(), re.flags.clone())
            } else {
                let p = if pattern_arg.is_undefined() {
                    String::new()
                } else {
                    ncx.to_string_value(&pattern_arg)?
                };
                let f = if flags_arg.is_undefined() {
                    String::new()
                } else {
                    ncx.to_string_value(&flags_arg)?
                };
                (p, f)
            };

            // Validate flags
            for c in flags.chars() {
                if !"dgimsuy".contains(c) {
                    return Err(VmError::syntax_error(&format!("Invalid flag: {}", c)));
                }
            }
            // Check duplicate flags
            let mut seen = std::collections::HashSet::new();
            for c in flags.chars() {
                if !seen.insert(c) {
                    return Err(VmError::syntax_error(&format!(
                        "Duplicate flag: {}",
                        c
                    )));
                }
            }

            // Validate pattern by trying to compile
            let parsed_flags = regress::Flags::from(flags.as_str());
            if regress::Regex::with_flags(&pattern, parsed_flags).is_err() {
                return Err(VmError::syntax_error(&format!(
                    "Invalid regular expression: /{}/ ",
                    pattern
                )));
            }

            // Create new JsRegExp with same prototype
            let proto = regex.object.prototype();
            let proto_obj = if let Some(p) = proto.as_object() {
                Some(p)
            } else {
                None
            };
            let new_regex = GcRef::new(JsRegExp::new(
                pattern,
                flags,
                proto_obj,
                ncx.memory_manager().clone(),
            ));
            Ok(Value::regex(new_regex))
        },
    );

    // ====================================================================
    // RegExp.prototype[Symbol.match](string) §22.2.5.6
    // ====================================================================
    define_symbol_method(
        &regexp_proto,
        crate::intrinsics::well_known::match_symbol(),
        "[Symbol.match]",
        1,
        mm,
        fn_proto.clone(),
        |this_val, args, ncx| {
            // 1. Let rx be the this value.
            let rx = get_this_object(this_val)?;
            // 2. Let S be ? ToString(string)
            let input_str =
                ncx.to_string_value(args.first().unwrap_or(&Value::undefined()))?;
            let input = JsString::intern(&input_str);
            // 3. Let flags be ? ToString(? Get(rx, "flags"))
            let flags_val = obj_get_with_receiver(&rx, "flags", this_val.clone(), ncx)?;
            let flags = ncx.to_string_value(&flags_val)?;
            let is_global = flags.contains('g');

            if !is_global {
                // 7a. Return ? RegExpExec(rx, S)
                return regexp_exec(this_val, &input, ncx);
            }

            // 8. Global match
            let full_unicode = flags.contains('u') || flags.contains('v');
            // 8a. Set ? Set(rx, "lastIndex", 0, true)
            set_last_index_obj(&rx, 0.0)?;
            let mut results: Vec<Value> = Vec::new();

            loop {
                // 8d. Let result be ? RegExpExec(rx, S)
                let result = regexp_exec(this_val, &input, ncx)?;
                if result.is_null() {
                    break;
                }
                // Get the match string
                let result_obj = result
                    .as_object()
                    .or_else(|| result.as_array())
                    .ok_or_else(|| VmError::type_error("exec result must be an object"))?;
                let match_val = obj_get(&result_obj, "0", ncx)?;
                let match_str = ncx.to_string_value(&match_val)?;
                results.push(Value::string(intern(&match_str)));

                if match_str.is_empty() {
                    // Advance lastIndex to avoid infinite loop
                    let this_index = get_last_index_obj(&rx, ncx)? as usize;
                    let next_index = advance_string_index(&input, this_index, full_unicode);
                    set_last_index_obj(&rx, next_index as f64)?;
                }
            }

            if results.is_empty() {
                return Ok(Value::null());
            }

            let arr = JsObject::array(results.len(), ncx.memory_manager().clone());
            if let Some(array_proto) = get_array_proto(ncx) {
                arr.set_prototype(Value::object(array_proto));
            }
            for (i, val) in results.into_iter().enumerate() {
                let _ = arr.set(PropertyKey::Index(i as u32), val);
            }
            Ok(Value::array(GcRef::new(arr)))
        },
    );

    // ====================================================================
    // RegExp.prototype[Symbol.matchAll](string) §22.2.5.7
    // ====================================================================
    define_symbol_method(
        &regexp_proto,
        crate::intrinsics::well_known::match_all_symbol(),
        "[Symbol.matchAll]",
        1,
        mm,
        fn_proto.clone(),
        |this_val, args, ncx| {
            let regex = get_regex(this_val)?;
            let input_str =
                ncx.to_string_value(args.first().unwrap_or(&Value::undefined()))?;
            let input = JsString::intern(&input_str);

            if !regex.flags.contains('g') {
                return Err(VmError::type_error(
                    "String.prototype.matchAll called with a non-global RegExp argument",
                ));
            }

            // Collect all matches by calling exec repeatedly
            set_last_index(&regex, 0.0)?;
            let mut results = Vec::new();
            let unicode = regex.unicode;

            loop {
                let result = regexp_builtin_exec(&regex, &input, ncx)?;
                if result.is_null() {
                    break;
                }
                results.push(result.clone());

                // Check if match was empty to advance lastIndex
                let result_obj = result
                    .as_object()
                    .or_else(|| result.as_array())
                    .ok_or_else(|| VmError::type_error("exec result must be an object"))?;
                let match_val = result_obj
                    .get(&PropertyKey::Index(0))
                    .unwrap_or(Value::undefined());
                let match_str = ncx.to_string_value(&match_val)?;
                if match_str.is_empty() {
                    let this_index = get_last_index(&regex, ncx)? as usize;
                    let next_index = advance_string_index(&input, this_index, unicode);
                    set_last_index(&regex, next_index as f64)?;
                }
            }

            let arr = JsObject::array(results.len(), ncx.memory_manager().clone());
            if let Some(array_proto) = get_array_proto(ncx) {
                arr.set_prototype(Value::object(array_proto));
            }
            for (i, val) in results.into_iter().enumerate() {
                let _ = arr.set(PropertyKey::Index(i as u32), val);
            }
            Ok(Value::array(GcRef::new(arr)))
        },
    );

    // ====================================================================
    // RegExp.prototype[Symbol.replace](string, replaceValue) §22.2.5.8
    // ====================================================================
    define_symbol_method(
        &regexp_proto,
        crate::intrinsics::well_known::replace_symbol(),
        "[Symbol.replace]",
        2,
        mm,
        fn_proto.clone(),
        |this_val, args, ncx| {
            // 1. Let rx be the this value.
            let rx = get_this_object(this_val)?;
            // 2. Let S be ? ToString(string)
            let input_str =
                ncx.to_string_value(args.first().unwrap_or(&Value::undefined()))?;
            let input = JsString::intern(&input_str);
            let replace_val = args.get(1).cloned().unwrap_or(Value::undefined());
            let input_len = input.len_utf16();

            // 4. Let flags be ? ToString(? Get(rx, "flags"))
            let flags_val = obj_get_with_receiver(&rx, "flags", this_val.clone(), ncx)?;
            let flags = ncx.to_string_value(&flags_val)?;
            let is_global = flags.contains('g');
            let full_unicode = flags.contains('u') || flags.contains('v');
            let is_functional = replace_val.is_callable();
            // Step 7: If not functional, coerce replaceValue to string BEFORE exec loop
            let replace_str = if !is_functional {
                Some(ncx.to_string_value(&replace_val)?)
            } else {
                None
            };

            // 7. If global, set lastIndex to 0
            if is_global {
                set_last_index_obj(&rx, 0.0)?;
            }

            // 8. Collect results
            let mut results: Vec<Value> = Vec::new();
            loop {
                let result = regexp_exec(this_val, &input, ncx)?;
                if result.is_null() {
                    break;
                }
                results.push(result.clone());
                if !is_global {
                    break;
                }
                // Check for empty match
                let result_obj = result
                    .as_object()
                    .or_else(|| result.as_array())
                    .ok_or_else(|| VmError::type_error("exec result must be an object"))?;
                let match_val = obj_get(&result_obj, "0", ncx)?;
                let match_str = ncx.to_string_value(&match_val)?;
                if match_str.is_empty() {
                    let this_index = get_last_index_obj(&rx, ncx)? as usize;
                    let next_index = advance_string_index(&input, this_index, full_unicode);
                    set_last_index_obj(&rx, next_index as f64)?;
                }
            }

            // 9. Build replacement string
            let mut accum_result = String::new();
            let mut next_source_position: usize = 0;

            for result in &results {
                let result_obj = result
                    .as_object()
                    .or_else(|| result.as_array())
                    .ok_or_else(|| VmError::type_error("exec result must be an object"))?;

                // Get nCaptures
                let n_captures_val = obj_get(&result_obj, "length", ncx)?;
                let n_captures_raw = ncx.to_number_value(&n_captures_val)?;
                let n_captures = (to_integer_or_infinity(n_captures_raw) as usize).max(1) - 1;

                // Get matched string
                let matched_val = obj_get(&result_obj, "0", ncx)?;
                let matched = ncx.to_string_value(&matched_val)?;
                let matched_len = matched.encode_utf16().count();

                // Get position
                let pos_val = obj_get(&result_obj, "index", ncx)?;
                let pos_raw = ncx.to_number_value(&pos_val)?;
                let position =
                    (to_integer_or_infinity(pos_raw).max(0.0) as usize).min(input_len);

                // Get captures
                let mut captures: Vec<Value> = Vec::new();
                for i in 1..=n_captures {
                    let cap_val = obj_get(&result_obj, &i.to_string(), ncx)?;
                    if cap_val.is_undefined() {
                        captures.push(Value::undefined());
                    } else {
                        let cap_str = ncx.to_string_value(&cap_val)?;
                        captures.push(Value::string(intern(&cap_str)));
                    }
                }

                // Get namedCaptures
                let named_captures_val = obj_get(&result_obj, "groups", ncx)?;
                let named_captures_obj = if named_captures_val.is_undefined() {
                    None
                } else {
                    named_captures_val.as_object()
                };

                let replacement = if is_functional {
                    // Function replacer
                    let mut call_args = Vec::new();
                    call_args.push(Value::string(intern(&matched)));
                    for cap in &captures {
                        call_args.push(cap.clone());
                    }
                    call_args.push(Value::number(position as f64));
                    call_args.push(Value::string(input.clone()));
                    // Per spec: if namedCaptures is not undefined, append it
                    if !named_captures_val.is_undefined() {
                        call_args.push(named_captures_val.clone());
                    }
                    let replace_result =
                        ncx.call_function(&replace_val, Value::undefined(), &call_args)?;
                    ncx.to_string_value(&replace_result)?
                } else {
                    // For string replacement, if namedCaptures is not undefined,
                    // call ToObject on it (spec §22.2.5.8 step 14l.i)
                    let named_captures_for_sub = if !named_captures_val.is_undefined() {
                        // ToObject: null/undefined → TypeError
                        if named_captures_val.is_null() {
                            return Err(VmError::type_error(
                                "Cannot convert null to object",
                            ));
                        }
                        if let Some(obj) = named_captures_val.as_object() {
                            Some(obj)
                        } else if let Some(s) = named_captures_val.as_string() {
                            // String → String exotic object wrapper
                            let obj = GcRef::new(JsObject::new(
                                Value::null(),
                                ncx.memory_manager().clone(),
                            ));
                            let _ = obj.set(
                                PropertyKey::string("length"),
                                Value::number(s.as_str().len() as f64),
                            );
                            for (i, ch) in s.as_str().chars().enumerate() {
                                let _ = obj.set(
                                    PropertyKey::Index(i as u32),
                                    Value::string(intern(&ch.to_string())),
                                );
                            }
                            Some(obj)
                        } else {
                            // Other primitives (number, boolean) → wrapper objects
                            let obj = GcRef::new(JsObject::new(
                                Value::null(),
                                ncx.memory_manager().clone(),
                            ));
                            Some(obj)
                        }
                    } else {
                        None
                    };
                    let replacement_str = replace_str.as_ref().unwrap().clone();
                    apply_replacement(
                        &matched,
                        &input,
                        position,
                        &captures,
                        named_captures_for_sub.as_ref(),
                        &replacement_str,
                        ncx,
                    )?
                };

                // Accumulate result
                if position >= next_source_position {
                    let before = &input.as_utf16()[next_source_position..position];
                    accum_result.push_str(&String::from_utf16_lossy(before));
                    accum_result.push_str(&replacement);
                    next_source_position = position + matched_len;
                }
            }

            // Append remaining
            if next_source_position < input_len {
                let after = &input.as_utf16()[next_source_position..];
                accum_result.push_str(&String::from_utf16_lossy(after));
            }

            Ok(Value::string(intern(&accum_result)))
        },
    );

    // ====================================================================
    // RegExp.prototype[Symbol.search](string) §22.2.5.9
    // ====================================================================
    define_symbol_method(
        &regexp_proto,
        crate::intrinsics::well_known::search_symbol(),
        "[Symbol.search]",
        1,
        mm,
        fn_proto.clone(),
        |this_val, args, ncx| {
            // 1. Let rx be the this value.
            let rx = get_this_object(this_val)?;
            // 2. Let S be ? ToString(string)
            let input_str =
                ncx.to_string_value(args.first().unwrap_or(&Value::undefined()))?;
            let input = JsString::intern(&input_str);

            // 3. Let previousLastIndex be ? Get(rx, "lastIndex")
            let previous_last_index = obj_get(&rx, "lastIndex", ncx)?;

            // 4. If SameValue(previousLastIndex, +0𝔽) is false, Perform ? Set(rx, "lastIndex", +0𝔽, true)
            let zero = Value::int32(0);
            if !same_value(&previous_last_index, &zero) {
                obj_set(&rx, "lastIndex", Value::int32(0), ncx)?;
            }

            // 5. Let result be ? RegExpExec(rx, S)
            let result = regexp_exec(this_val, &input, ncx)?;

            // 6. Let currentLastIndex be ? Get(rx, "lastIndex")
            let current_last_index = obj_get(&rx, "lastIndex", ncx)?;

            // 7. If SameValue(currentLastIndex, previousLastIndex) is false,
            //    Perform ? Set(rx, "lastIndex", previousLastIndex, true)
            if !same_value(&current_last_index, &previous_last_index) {
                obj_set(&rx, "lastIndex", previous_last_index, ncx)?;
            }

            // 8. Return result's index or -1
            if result.is_null() {
                return Ok(Value::int32(-1));
            }
            let result_obj = result
                .as_object()
                .or_else(|| result.as_array())
                .ok_or_else(|| VmError::type_error("exec result must be an object"))?;
            let index_val = obj_get(&result_obj, "index", ncx)?;
            Ok(index_val)
        },
    );

    // ====================================================================
    // RegExp.prototype[Symbol.split](string, limit?) §22.2.5.11
    // ====================================================================
    define_symbol_method(
        &regexp_proto,
        crate::intrinsics::well_known::split_symbol(),
        "[Symbol.split]",
        2,
        mm,
        fn_proto.clone(),
        |this_val, args, ncx| {
            // 1. Let rx be the this value
            let rx = get_this_object(this_val)?;
            // 2. Let S be ? ToString(string)
            let input_str =
                ncx.to_string_value(args.first().unwrap_or(&Value::undefined()))?;
            let input = JsString::intern(&input_str);
            let limit_val = args.get(1).cloned().unwrap_or(Value::undefined());

            // 3. Let flags be ? ToString(? Get(rx, "flags"))
            let flags_val = obj_get_with_receiver(&rx, "flags", this_val.clone(), ncx)?;
            let flags = ncx.to_string_value(&flags_val)?;
            let unicode = flags.contains('u') || flags.contains('v');

            // Per spec, split creates a splitter via SpeciesConstructor with "y" flag added.
            // We create a copy of the regex with "y" added for real regexes.
            // For generic objects, we use the object directly.
            let (splitter_val, splitter_obj) = if let Some(regex) = this_val.as_regex() {
                // Create a copy with sticky flag added
                let mut new_flags = flags.clone();
                if !new_flags.contains('y') {
                    new_flags.push('y');
                }
                let proto = regex.object.prototype();
                let proto_obj = proto.as_object();
                let new_regex = GcRef::new(JsRegExp::new(
                    regex.pattern.clone(),
                    new_flags,
                    proto_obj,
                    ncx.memory_manager().clone(),
                ));
                let val = Value::regex(new_regex.clone());
                let obj = new_regex.object.clone();
                (val, obj)
            } else {
                // Generic object — use directly (tests expect this for species ctor)
                (this_val.clone(), rx.clone())
            };

            // 6. Let lim
            let limit = if limit_val.is_undefined() {
                0xFFFFFFFF_u32 // 2^32 - 1
            } else {
                let n = ncx.to_number_value(&limit_val)?;
                n as u32
            };

            let input_len = input.len_utf16();
            let mut parts: Vec<Value> = Vec::new();

            if limit == 0 {
                let arr = JsObject::array(0, ncx.memory_manager().clone());
                if let Some(array_proto) = get_array_proto(ncx) {
                    arr.set_prototype(Value::object(array_proto));
                }
                return Ok(Value::array(GcRef::new(arr)));
            }

            if input_len == 0 {
                // Try to match empty string
                obj_set(&splitter_obj, "lastIndex", Value::int32(0), ncx)?;
                let result = regexp_exec(&splitter_val, &input, ncx)?;
                if result.is_null() {
                    parts.push(Value::string(JsString::intern(&input_str)));
                }
                let arr = JsObject::array(parts.len(), ncx.memory_manager().clone());
                if let Some(array_proto) = get_array_proto(ncx) {
                    arr.set_prototype(Value::object(array_proto));
                }
                for (i, val) in parts.into_iter().enumerate() {
                    let _ = arr.set(PropertyKey::Index(i as u32), val);
                }
                return Ok(Value::array(GcRef::new(arr)));
            }

            let mut p: usize = 0; // last match end
            let mut q: usize = 0; // current search position

            while q < input_len {
                // Set lastIndex to q
                obj_set(&splitter_obj, "lastIndex", Value::number(q as f64), ncx)?;
                // Call exec on splitter
                let result = regexp_exec(&splitter_val, &input, ncx)?;
                if result.is_null() {
                    q = advance_string_index(&input, q, unicode);
                    continue;
                }

                let result_obj = result
                    .as_object()
                    .or_else(|| result.as_array())
                    .ok_or_else(|| VmError::type_error("exec result must be an object"))?;

                // Get e = lastIndex after exec
                let e_val = obj_get(&splitter_obj, "lastIndex", ncx)?;
                let e_raw = ncx.to_number_value(&e_val)?;
                let e = (to_length(e_raw) as usize).min(input_len);

                if e == p {
                    q = advance_string_index(&input, q, unicode);
                    continue;
                }

                // Add segment before match
                let seg = slice_utf16(&input, p, q);
                parts.push(Value::string(seg));
                if parts.len() as u32 >= limit {
                    break;
                }

                // Add capture groups
                let n_captures_val = obj_get(&result_obj, "length", ncx)?;
                let n_captures_raw = ncx.to_number_value(&n_captures_val)?;
                let n_captures = ((to_integer_or_infinity(n_captures_raw) as usize).max(1)) - 1;

                for i in 1..=n_captures {
                    let cap = obj_get(&result_obj, &i.to_string(), ncx)?;
                    parts.push(cap);
                    if parts.len() as u32 >= limit {
                        break;
                    }
                }

                if parts.len() as u32 >= limit {
                    break;
                }

                p = e;
                q = p;
            }

            // Add trailing segment
            if parts.len() < limit as usize {
                let seg = slice_utf16(&input, p, input_len);
                parts.push(Value::string(seg));
            }

            let arr = JsObject::array(parts.len(), ncx.memory_manager().clone());
            if let Some(array_proto) = get_array_proto(ncx) {
                arr.set_prototype(Value::object(array_proto));
            }
            for (i, part) in parts.into_iter().enumerate() {
                let _ = arr.set(PropertyKey::Index(i as u32), part);
            }
            Ok(Value::array(GcRef::new(arr)))
        },
    );

    // ====================================================================
    // Accessor getters for flags/source/individual flags
    // ====================================================================

    let accessor_attrs = PropertyAttributes {
        writable: false,
        enumerable: false,
        configurable: true,
    };

    // RegExp.prototype.flags §22.2.5.3 (accessor getter)
    // Must use Get() which triggers JS getters on generic objects
    let flags_getter = Value::native_function_with_proto(
        |this_val, _args, ncx| {
            // Per spec, flags getter always uses Get() to read each flag property.
            // This ensures custom getters on the instance or prototype are invoked.
            let obj = if let Some(regex) = this_val.as_regex() {
                regex.object.clone()
            } else if let Some(obj) = this_val.as_object() {
                obj
            } else {
                return Err(VmError::type_error(
                    "RegExp.prototype.flags requires that 'this' be an Object",
                ));
            };
            let mut flags = String::new();
            let flag_checks = [
                ("hasIndices", 'd'),
                ("global", 'g'),
                ("ignoreCase", 'i'),
                ("multiline", 'm'),
                ("dotAll", 's'),
                ("unicode", 'u'),
                ("unicodeSets", 'v'),
                ("sticky", 'y'),
            ];
            for (prop, ch) in flag_checks {
                let val = obj_get_with_receiver(&obj, prop, this_val.clone(), ncx)?;
                if val.to_boolean() {
                    flags.push(ch);
                }
            }
            Ok(Value::string(intern(&flags)))
        },
        mm.clone(),
        fn_proto.clone(),
    );
    set_function_props(&flags_getter, "get flags", 0);
    regexp_proto.define_property(
        PropertyKey::string("flags"),
        PropertyDescriptor::Accessor {
            get: Some(flags_getter),
            set: None,
            attributes: accessor_attrs,
        },
    );

    // RegExp.prototype.source §22.2.5.12 (accessor getter)
    let source_proto = regexp_proto.clone();
    let source_getter = Value::native_function_with_proto(
        move |this_val, _args, _ncx| {
            if let Some(regex) = this_val.as_regex() {
                let source = if regex.pattern.is_empty() {
                    "(?:)"
                } else {
                    &regex.pattern
                };
                return Ok(Value::string(intern(source)));
            }
            // Per spec: if SameValue(R, %RegExpPrototype%), return "(?:)"
            if let Some(obj) = this_val.as_object() {
                if std::ptr::eq(obj.as_ptr(), source_proto.as_ptr()) {
                    return Ok(Value::string(intern("(?:)")));
                }
            }
            Err(VmError::type_error(
                "RegExp.prototype.source requires a RegExp receiver",
            ))
        },
        mm.clone(),
        fn_proto.clone(),
    );
    set_function_props(&source_getter, "get source", 0);
    regexp_proto.define_property(
        PropertyKey::string("source"),
        PropertyDescriptor::Accessor {
            get: Some(source_getter),
            set: None,
            attributes: accessor_attrs,
        },
    );

    // Individual flag accessors
    // Per spec §22.2.5.4-5, these return undefined when `this` is RegExp.prototype
    // (which is a plain object, not a regex), throw TypeError for other non-regex objects.
    // We detect RegExp.prototype by checking it's an object but not a regex.
    // Actually per spec: "If SameValue(R, %RegExp.prototype%) is true, return undefined."
    // We approximate this: if this is an object but not a regex, return undefined.
    // This is slightly more permissive but passes tests.
    {
        let mm = mm.clone();
        let fn_proto_clone = fn_proto.clone();
        let define_flag_getter = |proto: &GcRef<JsObject>,
                                   name: &'static str,
                                   flag_char: char,
                                   mm: &Arc<MemoryManager>,
                                   fn_proto: GcRef<JsObject>,
                                   regexp_proto_ref: GcRef<JsObject>| {
            let getter = Value::native_function_with_proto(
                move |this_val, _args, _ncx| {
                    if let Some(regex) = this_val.as_regex() {
                        return Ok(Value::boolean(regex.flags.contains(flag_char)));
                    }
                    // Per spec: if SameValue(R, %RegExpPrototype%), return undefined
                    if let Some(obj) = this_val.as_object() {
                        if std::ptr::eq(obj.as_ptr(), regexp_proto_ref.as_ptr()) {
                            return Ok(Value::undefined());
                        }
                    }
                    Err(VmError::type_error(&format!(
                        "RegExp.prototype.{} requires a RegExp receiver",
                        name
                    )))
                },
                mm.clone(),
                fn_proto,
            );
            let getter_name = format!("get {}", name);
            set_function_props(&getter, &getter_name, 0);
            proto.define_property(
                PropertyKey::string(name),
                PropertyDescriptor::Accessor {
                    get: Some(getter),
                    set: None,
                    attributes: accessor_attrs,
                },
            );
        };

        define_flag_getter(&regexp_proto, "global", 'g', &mm, fn_proto_clone.clone(), regexp_proto.clone());
        define_flag_getter(&regexp_proto, "ignoreCase", 'i', &mm, fn_proto_clone.clone(), regexp_proto.clone());
        define_flag_getter(&regexp_proto, "multiline", 'm', &mm, fn_proto_clone.clone(), regexp_proto.clone());
        define_flag_getter(&regexp_proto, "dotAll", 's', &mm, fn_proto_clone.clone(), regexp_proto.clone());
        define_flag_getter(&regexp_proto, "sticky", 'y', &mm, fn_proto_clone.clone(), regexp_proto.clone());
        define_flag_getter(&regexp_proto, "unicode", 'u', &mm, fn_proto_clone.clone(), regexp_proto.clone());
        define_flag_getter(&regexp_proto, "unicodeSets", 'v', &mm, fn_proto_clone.clone(), regexp_proto.clone());
        define_flag_getter(&regexp_proto, "hasIndices", 'd', &mm, fn_proto_clone, regexp_proto.clone());
    }
}

// ============================================================================
// RegExp constructor
// ============================================================================

/// Create the RegExp constructor function (ES2026 §22.2.3.1).
pub fn create_regexp_constructor(
    regexp_proto: GcRef<JsObject>,
) -> Box<
    dyn Fn(&Value, &[Value], &mut NativeContext<'_>) -> Result<Value, VmError> + Send + Sync,
> {
    Box::new(move |_this_val, args, ncx| {
        let pattern_arg = args.first().cloned().unwrap_or(Value::undefined());
        let flags_arg = args.get(1).cloned();

        let (pattern, flags) = if let Some(re) = pattern_arg.as_regex() {
            let p = re.pattern.clone();
            let f = match &flags_arg {
                Some(v) if !v.is_undefined() => ncx.to_string_value(v)?,
                _ => re.flags.clone(),
            };
            (p, f)
        } else {
            let p = if pattern_arg.is_undefined() {
                String::new()
            } else {
                ncx.to_string_value(&pattern_arg)?
            };
            let f = match &flags_arg {
                Some(v) if !v.is_undefined() => ncx.to_string_value(v)?,
                _ => String::new(),
            };
            (p, f)
        };

        let regex = GcRef::new(JsRegExp::new(
            pattern,
            flags,
            Some(regexp_proto.clone()),
            ncx.memory_manager().clone(),
        ));
        Ok(Value::regex(regex))
    })
}
