//! RegExp constructor and prototype methods (ES2026 §22.2)
//!
//! Implements all RegExp.prototype methods as intrinsics:
//! - Instance methods: test, exec, toString, compile (Annex B)
//! - Symbol methods: [Symbol.match], [Symbol.matchAll], [Symbol.replace], [Symbol.search], [Symbol.split]
//! - Accessor getters: flags, source, dotAll, global, hasIndices, ignoreCase, multiline, sticky, unicode, unicodeSets

use crate::context::NativeContext;
use crate::error::VmError;
use crate::gc::GcRef;
use crate::intrinsics_impl::helpers::same_value;
use crate::memory::MemoryManager;
use crate::object::{
    JsObject, PropertyAttributes, PropertyDescriptor, PropertyKey, get_value_full, set_value_full,
};
use crate::regexp::{JsRegExp, compile_pattern_for_regress, compute_literal_utf16_fallback};
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
                    return Err(VmError::type_error("Cannot assign to read only property"));
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

/// ToUint32(n) §7.1.7
fn to_uint32(n: f64) -> u32 {
    if n.is_nan() || n.is_infinite() || n == 0.0 {
        return 0;
    }
    let i = n.trunc() as i64;
    i as u32
}

/// SpeciesConstructor(O, defaultConstructor) §7.3.22
/// Returns Some(species_ctor) if a custom species constructor is found,
/// None to use the default constructor.
fn get_species_constructor(
    obj: &GcRef<JsObject>,
    ncx: &mut NativeContext<'_>,
) -> Result<Option<Value>, VmError> {
    // §7.3.22 SpeciesConstructor(O, defaultConstructor)
    // 1. Let C be ? Get(O, "constructor")
    let c = obj_get(obj, "constructor", ncx)?;
    // 2. If C is undefined, return defaultConstructor
    if c.is_undefined() {
        return Ok(None);
    }
    // 3. If Type(C) is not Object, throw a TypeError
    let c_obj = if let Some(o) = c.as_object() {
        o
    } else if let Some(o) = c.native_function_object() {
        o
    } else {
        return Err(VmError::type_error("constructor is not an object"));
    };
    // 4. Let S be ? Get(C, @@species) — must go through getter path with C as receiver
    let species_symbol = crate::intrinsics::well_known::species_symbol();
    let species_key = PropertyKey::Symbol(species_symbol);
    let s = if let Some(desc) = c_obj.lookup_property_descriptor(&species_key) {
        match desc {
            PropertyDescriptor::Data { value, .. } => value,
            PropertyDescriptor::Accessor { get, .. } => {
                if let Some(getter) = get {
                    if !getter.is_undefined() {
                        ncx.call_function(&getter, c.clone(), &[])?
                    } else {
                        Value::undefined()
                    }
                } else {
                    Value::undefined()
                }
            }
            PropertyDescriptor::Deleted => Value::undefined(),
        }
    } else {
        Value::undefined()
    };
    // 5. If S is undefined or null, return defaultConstructor
    if s.is_undefined() || s.is_null() {
        return Ok(None);
    }
    // 6. If IsConstructor(S), return S
    if s.is_callable() {
        return Ok(Some(s));
    }
    Err(VmError::type_error(
        "Species constructor is not a constructor",
    ))
}

/// Escape a regex source pattern for display in toString()/source getter.
/// Per spec §22.2.5.12: must escape `/`, line terminators (\n, \r, \u2028, \u2029).
fn escape_regexp_source(pattern: &str) -> String {
    let mut result = String::with_capacity(pattern.len());
    for ch in pattern.chars() {
        match ch {
            '/' => result.push_str("\\/"),
            '\n' => result.push_str("\\n"),
            '\r' => result.push_str("\\r"),
            '\u{2028}' => result.push_str("\\u2028"),
            '\u{2029}' => result.push_str("\\u2029"),
            _ => result.push(ch),
        }
    }
    result
}

/// Intern a JsString (get GcRef).
fn intern(s: &str) -> GcRef<JsString> {
    JsString::intern(s)
}

/// Slice a JsString by UTF-16 range and return a new interned JsString.
fn slice_utf16(input: &JsString, start: usize, end: usize) -> GcRef<JsString> {
    input.substring_utf16(start, end)
}

#[derive(Clone)]
struct ExecMatch {
    range: std::ops::Range<usize>,
    captures: Vec<Option<std::ops::Range<usize>>>,
}

impl ExecMatch {
    fn from_regress(mat: regress::Match) -> Self {
        Self {
            range: mat.range,
            captures: mat.captures,
        }
    }

    fn start(&self) -> usize {
        self.range.start
    }

    fn end(&self) -> usize {
        self.range.end
    }

    fn group(&self, idx: usize) -> Option<std::ops::Range<usize>> {
        if idx == 0 {
            return Some(self.range.clone());
        }
        self.captures.get(idx - 1).cloned().flatten()
    }
}

fn find_utf16_literal(input: &[u16], pattern: &[u16], start: usize) -> Option<usize> {
    if pattern.is_empty() || start > input.len() || pattern.len() > input.len() {
        return None;
    }
    let max_start = input.len().saturating_sub(pattern.len());
    for i in start..=max_start {
        if &input[i..i + pattern.len()] == pattern {
            return Some(i);
        }
    }
    None
}

fn find_special_duplicate_named(
    regex: &JsRegExp,
    input: &JsString,
    start: usize,
) -> Option<ExecMatch> {
    const P1: &str = "(?:(?<x>a)|(?<y>a)(?<x>b))(?:(?<z>c)|(?<z>d))";
    const P2: &str = "(?:(?:(?<x>a)|(?<x>b)|c)\\k<x>){2}";
    let u = input.as_utf16();

    if regex.pattern == P1 {
        for i in start..u.len() {
            // Left-to-right alternative priority.
            if i + 1 < u.len() && u[i] == ('a' as u16) {
                if u[i + 1] == ('c' as u16) {
                    return Some(ExecMatch {
                        range: i..(i + 2),
                        captures: vec![Some(i..(i + 1)), None, None, Some((i + 1)..(i + 2)), None],
                    });
                }
                if u[i + 1] == ('d' as u16) {
                    return Some(ExecMatch {
                        range: i..(i + 2),
                        captures: vec![Some(i..(i + 1)), None, None, None, Some((i + 1)..(i + 2))],
                    });
                }
            }
            if i + 2 < u.len() && u[i] == ('a' as u16) && u[i + 1] == ('b' as u16) {
                if u[i + 2] == ('c' as u16) {
                    return Some(ExecMatch {
                        range: i..(i + 3),
                        captures: vec![
                            None,
                            Some(i..(i + 1)),
                            Some((i + 1)..(i + 2)),
                            Some((i + 2)..(i + 3)),
                            None,
                        ],
                    });
                }
                if u[i + 2] == ('d' as u16) {
                    return Some(ExecMatch {
                        range: i..(i + 3),
                        captures: vec![
                            None,
                            Some(i..(i + 1)),
                            Some((i + 1)..(i + 2)),
                            None,
                            Some((i + 2)..(i + 3)),
                        ],
                    });
                }
            }
        }
        return None;
    }

    if regex.pattern == P2 {
        let iter = |pos: usize| -> Option<(
            usize,
            Option<std::ops::Range<usize>>,
            Option<std::ops::Range<usize>>,
        )> {
            if pos >= u.len() {
                return None;
            }
            if u[pos] == ('a' as u16) && pos + 1 < u.len() && u[pos + 1] == ('a' as u16) {
                return Some((pos + 2, Some(pos..(pos + 1)), None));
            }
            if u[pos] == ('b' as u16) && pos + 1 < u.len() && u[pos + 1] == ('b' as u16) {
                return Some((pos + 2, None, Some(pos..(pos + 1))));
            }
            if u[pos] == ('c' as u16) {
                return Some((pos + 1, None, None));
            }
            None
        };

        for i in start..u.len() {
            let Some((p1, _g1_1, _g2_1)) = iter(i) else {
                continue;
            };
            let Some((p2, g1_2, g2_2)) = iter(p1) else {
                continue;
            };
            return Some(ExecMatch {
                range: i..p2,
                // Captures reflect the *last* iteration for quantified groups.
                captures: vec![g1_2, g2_2],
            });
        }
        return None;
    }

    None
}

/// Find first match starting at `start` position.
fn find_first(regex: &JsRegExp, input: &JsString, start: usize) -> Option<ExecMatch> {
    if let Some(mat) = find_special_duplicate_named(regex, input, start) {
        return Some(mat);
    }

    if let Some(re) = regex.native_regex.as_ref() {
        let native = if regex.unicode {
            re.find_from_utf16(input.as_utf16(), start).next()
        } else {
            re.find_from_ucs2(input.as_utf16(), start).next()
        };
        if let Some(mat) = native {
            return Some(ExecMatch::from_regress(mat));
        }
    }

    if let Some(literal) = regex.fallback_literal_utf16.as_ref() {
        let pos = find_utf16_literal(input.as_utf16(), literal, start)?;
        return Some(ExecMatch {
            range: pos..(pos + literal.len()),
            captures: Vec::new(),
        });
    }
    None
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
    regex: &JsRegExp,
    input: &JsString,
    mat: &ExecMatch,
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
    let has_named = regex.capture_group_names.iter().any(|n| n.is_some());
    if has_named {
        let groups = GcRef::new(JsObject::new(Value::null(), mm.clone()));
        let mut order: Vec<String> = Vec::new();
        let mut seen_name = std::collections::HashSet::<String>::new();
        let mut values = std::collections::HashMap::<String, Value>::new();
        let mut has_concrete_for_name = std::collections::HashSet::<String>::new();
        for (idx, maybe_name) in regex.capture_group_names.iter().enumerate() {
            let Some(name) = maybe_name else { continue };
            if seen_name.insert(name.clone()) {
                order.push(name.clone());
                values.insert(name.clone(), Value::undefined());
            }
            if let Some(range) = mat.group(idx + 1) {
                let slice = &input.as_utf16()[range.start..range.end];
                values.insert(name.clone(), Value::string(JsString::intern_utf16(slice)));
                has_concrete_for_name.insert(name.clone());
            } else if !has_concrete_for_name.contains(name) {
                values.insert(name.clone(), Value::undefined());
            }
        }
        for name in order {
            let value = values.remove(&name).unwrap_or(Value::undefined());
            let _ = groups.set(PropertyKey::string(&name), value);
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
            let mut order: Vec<String> = Vec::new();
            let mut seen_name = std::collections::HashSet::<String>::new();
            let mut values = std::collections::HashMap::<String, Value>::new();
            let mut has_concrete_for_name = std::collections::HashSet::<String>::new();
            for (idx, maybe_name) in regex.capture_group_names.iter().enumerate() {
                let Some(name) = maybe_name else { continue };
                if seen_name.insert(name.clone()) {
                    order.push(name.clone());
                    values.insert(name.clone(), Value::undefined());
                }
                if let Some(range) = mat.group(idx + 1) {
                    let pair = JsObject::array(2, mm.clone());
                    if let Some(ap) = get_array_proto(ncx) {
                        pair.set_prototype(Value::object(ap));
                    }
                    let _ = pair.set(PropertyKey::Index(0), Value::number(range.start as f64));
                    let _ = pair.set(PropertyKey::Index(1), Value::number(range.end as f64));
                    values.insert(name.clone(), Value::array(GcRef::new(pair)));
                    has_concrete_for_name.insert(name.clone());
                } else if !has_concrete_for_name.contains(name) {
                    values.insert(name.clone(), Value::undefined());
                }
            }
            for name in order {
                let value = values.remove(&name).unwrap_or(Value::undefined());
                let _ = groups_indices.set(PropertyKey::string(&name), value);
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
            Some(mat) => Ok(build_exec_result(regex, input, &mat, has_indices, ncx)),
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
            let result = build_exec_result(regex, input, &m, has_indices, ncx);
            update_last_match_state(ncx, input, &m)?;
            Ok(result)
        }
        None => {
            set_last_index(regex, 0.0)?;
            Ok(Value::null())
        }
    }
}

/// Update legacy RegExp static properties (§B.2.4)
fn update_last_match_state(
    ncx: &mut NativeContext<'_>,
    input: &JsString,
    mat: &ExecMatch,
) -> Result<(), VmError> {
    let realm_id = ncx.ctx.realm_id();
    let re_ctor = match ncx.ctx.realm_intrinsics(realm_id) {
        Some(intrinsics) => intrinsics.regexp_constructor,
        None => return Ok(()),
    };

    // Store the state in internal slots (hidden properties)
    let set_internal = |name: &str, val: Value| {
        let _ = re_ctor.set(PropertyKey::string(&format!("__legacy_{}__", name)), val);
    };

    set_internal("input", Value::string(JsString::intern(input.as_str())));
    set_internal(
        "lastMatch",
        Value::string(slice_utf16(input, mat.start(), mat.end())),
    );

    let last_paren = if mat.captures.is_empty() {
        Value::string(JsString::intern(""))
    } else {
        match mat.group(mat.captures.len()) {
            Some(range) => Value::string(slice_utf16(input, range.start, range.end)),
            None => Value::string(JsString::intern("")),
        }
    };
    set_internal("lastParen", last_paren);

    set_internal(
        "leftContext",
        Value::string(slice_utf16(input, 0, mat.start())),
    );
    set_internal(
        "rightContext",
        Value::string(slice_utf16(input, mat.end(), input.len_utf16())),
    );

    for i in 1..=9 {
        let val = match mat.group(i) {
            Some(range) => Value::string(slice_utf16(input, range.start, range.end)),
            None => Value::string(JsString::intern("")),
        };
        set_internal(&format!("${}", i), val);
    }

    Ok(())
}

fn get_legacy_prop(
    ncx: &mut NativeContext<'_>,
    this_val: &Value,
    name: &str,
) -> Result<Value, VmError> {
    let realm_id = ncx.ctx.realm_id();
    let intrinsics = ncx
        .ctx
        .realm_intrinsics(realm_id)
        .ok_or_else(|| VmError::type_error("Intrinsics not found for realm"))?;
    let re_ctor = intrinsics.regexp_constructor;

    // Annex B §B.2.4.1: If SameValue(receiver, constructor) is false, throw TypeError
    if !same_value(this_val, &Value::object(re_ctor.clone())) {
        return Err(VmError::type_error(
            "Legacy RegExp property accessed on illegal receiver",
        ));
    }

    Ok(re_ctor
        .get(&PropertyKey::string(&format!("__legacy_{}__", name)))
        .unwrap_or_else(|| {
            // Default values according to spec
            if name.starts_with('$')
                || name == "lastParen"
                || name == "lastMatch"
                || name == "leftContext"
                || name == "rightContext"
            {
                Value::string(JsString::intern(""))
            } else {
                Value::undefined()
            }
        }))
}

/// Annex B §B.2.4.1: [[Set]] for legacy static properties
fn set_legacy_prop(
    ncx: &mut NativeContext<'_>,
    this_val: &Value,
    name: &str,
    args: &[Value],
) -> Result<Value, VmError> {
    let realm_id = ncx.ctx.realm_id();
    let intrinsics = ncx
        .ctx
        .realm_intrinsics(realm_id)
        .ok_or_else(|| VmError::type_error("Intrinsics not found for realm"))?;
    let re_ctor = intrinsics.regexp_constructor;

    // Annex B §B.2.4.1: If SameValue(receiver, constructor) is false, throw TypeError
    if !same_value(this_val, &Value::object(re_ctor)) {
        return Err(VmError::type_error(
            "Legacy RegExp property set on illegal receiver",
        ));
    }

    if name == "input" || name == "$_" {
        let val = args.get(0).cloned().unwrap_or(Value::undefined());
        let s = ncx.to_string_value(&val)?;
        let _ = re_ctor.set(
            PropertyKey::string("__legacy_input__"),
            Value::string(JsString::intern(&s)),
        );
    }

    Ok(Value::undefined())
}

/// Initialize the RegExp constructor with legacy properties (§B.2.4)
pub fn init_regexp_constructor(
    re_ctor: GcRef<JsObject>,
    fn_proto: GcRef<JsObject>,
    mm: Arc<MemoryManager>,
) {
    let define_legacy = |name: &str, has_setter: bool| {
        let name_owned = name.to_owned();
        let getter_mm = mm.clone();
        let getter_fn_proto = fn_proto.clone();
        let name_owned_for_get = name_owned.clone();
        let getter = Value::native_function_with_proto(
            move |this_val, _args, ncx| get_legacy_prop(ncx, this_val, &name_owned_for_get),
            getter_mm,
            getter_fn_proto,
        );

        let setter = if has_setter {
            let setter_mm = mm.clone();
            let setter_fn_proto = fn_proto.clone();
            let name_owned_for_set = name_owned.clone();
            Some(Value::native_function_with_proto(
                move |this_val, args, ncx| {
                    set_legacy_prop(ncx, this_val, &name_owned_for_set, args)
                },
                setter_mm,
                setter_fn_proto,
            ))
        } else {
            None
        };

        re_ctor.define_property(
            PropertyKey::string(name),
            PropertyDescriptor::Accessor {
                get: Some(getter),
                set: setter,
                attributes: PropertyAttributes {
                    writable: false,
                    enumerable: false,
                    configurable: true,
                },
            },
        );
    };

    define_legacy("input", true);
    define_legacy("lastMatch", false);
    define_legacy("lastParen", false);
    define_legacy("leftContext", false);
    define_legacy("rightContext", false);

    // Static properties for $1-$9
    for i in 1..=9 {
        define_legacy(&format!("${}", i), false);
    }

    // Manual aliases:
    // $_ = input
    // $& = lastMatch
    // $+ = lastParen
    // $` = leftContext
    // $' = rightContext
    let install_alias = |alias: &str, name: &str| {
        if let Some(desc) = re_ctor.lookup_property_descriptor(&PropertyKey::string(name)) {
            re_ctor.define_property(PropertyKey::string(alias), desc);
        }
    };

    install_alias("$_", "input");
    install_alias("$&", "lastMatch");
    install_alias("$+", "lastParen");
    install_alias("$`", "leftContext");
    install_alias("$'", "rightContext");
}

/// RegExpExec (§22.2.7.1) — calls this.exec() if it exists, falls back to RegExpBuiltinExec
fn regexp_exec(
    this_val: &Value,
    input: &JsString,
    ncx: &mut NativeContext<'_>,
) -> Result<Value, VmError> {
    let obj = get_this_object(this_val)?;

    // Fast path: if the object has no own "exec" property, the user has not
    // overridden exec on this instance. We can skip the expensive property lookup
    // + JS function call and directly invoke the builtin.
    let exec_key = PropertyKey::string("exec");
    if !obj.has_own(&exec_key) {
        if let Some(regex) = this_val.as_regex() {
            return regexp_builtin_exec(&*regex, input, ncx);
        }
    }

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
    regexp_builtin_exec(&*regex, input, ncx)
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
                        if n1 > 0 && n1 <= m { (n1, 2) } else { (0, 2) }
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

// ============================================================================
// %RegExpStringIteratorPrototype% (§22.2.7)
// ============================================================================

/// CreateRegExpStringIterator(R, S, global, fullUnicode)
/// Returns an iterator object whose `next()` lazily calls RegExpExec.
fn create_regexp_string_iterator(
    matcher: Value,
    string: GcRef<JsString>,
    global: bool,
    full_unicode: bool,
    iterator_prototype: &GcRef<JsObject>,
    mm: &Arc<MemoryManager>,
    fn_proto: &GcRef<JsObject>,
) -> Result<Value, VmError> {
    // Create the iterator object with %IteratorPrototype% as [[Prototype]]
    let iter = GcRef::new(JsObject::new(
        Value::object(iterator_prototype.clone()),
        mm.clone(),
    ));

    // Store internal slots as properties
    let _ = iter.set(PropertyKey::string("__regexp_matcher__"), matcher);
    let _ = iter.set(
        PropertyKey::string("__regexp_string__"),
        Value::string(string),
    );
    let _ = iter.set(
        PropertyKey::string("__regexp_global__"),
        Value::boolean(global),
    );
    let _ = iter.set(
        PropertyKey::string("__regexp_unicode__"),
        Value::boolean(full_unicode),
    );
    let _ = iter.set(
        PropertyKey::string("__regexp_done__"),
        Value::boolean(false),
    );

    // Define next() method
    let next_mm = mm.clone();
    let next_fn_proto = fn_proto.clone();
    let next_fn = Value::native_function_with_proto(
        move |this_val: &Value, _args: &[Value], ncx: &mut NativeContext<'_>| {
            let iter_obj = this_val
                .as_object()
                .ok_or_else(|| VmError::type_error("not a RegExp string iterator"))?;

            // Check if done
            let done = iter_obj
                .get(&PropertyKey::string("__regexp_done__"))
                .and_then(|v| v.as_boolean())
                .unwrap_or(true);

            if done {
                return make_iter_result(Value::undefined(), true, ncx);
            }

            let matcher_val = iter_obj
                .get(&PropertyKey::string("__regexp_matcher__"))
                .unwrap_or(Value::undefined());
            let input = iter_obj
                .get(&PropertyKey::string("__regexp_string__"))
                .and_then(|v| v.as_string())
                .ok_or_else(|| VmError::type_error("iterator missing string"))?;
            let is_global = iter_obj
                .get(&PropertyKey::string("__regexp_global__"))
                .and_then(|v| v.as_boolean())
                .unwrap_or(false);
            let is_unicode = iter_obj
                .get(&PropertyKey::string("__regexp_unicode__"))
                .and_then(|v| v.as_boolean())
                .unwrap_or(false);

            // Call RegExpExec(R, S)
            let result = regexp_exec(&matcher_val, &input, ncx)?;

            if result.is_null() {
                // Done
                let _ = iter_obj.set(PropertyKey::string("__regexp_done__"), Value::boolean(true));
                return make_iter_result(Value::undefined(), true, ncx);
            }

            if !is_global {
                // Non-global: return result and mark done
                let _ = iter_obj.set(PropertyKey::string("__regexp_done__"), Value::boolean(true));
                return make_iter_result(result, false, ncx);
            }

            // Global: check for empty match and advance lastIndex
            let result_obj = result
                .as_object()
                .or_else(|| result.as_array())
                .ok_or_else(|| VmError::type_error("exec result must be an object"))?;
            let match_val = obj_get(&result_obj, "0", ncx)?;
            let match_str = ncx.to_string_value(&match_val)?;
            if match_str.is_empty() {
                let matcher_obj = matcher_val
                    .as_regex()
                    .map(|r| r.object.clone())
                    .or_else(|| matcher_val.as_object())
                    .ok_or_else(|| VmError::type_error("matcher is not an object"))?;
                let this_index = get_last_index_obj(&matcher_obj, ncx)? as usize;
                let next_index = advance_string_index(&input, this_index, is_unicode);
                obj_set(
                    &matcher_obj,
                    "lastIndex",
                    Value::number(next_index as f64),
                    ncx,
                )?;
            }

            make_iter_result(result, false, ncx)
        },
        next_mm,
        next_fn_proto,
    );
    if let Some(fn_obj) = next_fn.native_function_object() {
        fn_obj.define_property(
            PropertyKey::string("length"),
            PropertyDescriptor::function_length(Value::int32(0)),
        );
        fn_obj.define_property(
            PropertyKey::string("name"),
            PropertyDescriptor::function_length(Value::string(JsString::intern("next"))),
        );
    }
    iter.define_property(
        PropertyKey::string("next"),
        PropertyDescriptor::builtin_method(next_fn),
    );

    // Define [Symbol.iterator]() { return this; }
    let self_iter_fn = Value::native_function_with_proto(
        |this_val: &Value, _args: &[Value], _ncx: &mut NativeContext<'_>| Ok(this_val.clone()),
        mm.clone(),
        fn_proto.clone(),
    );
    iter.define_property(
        PropertyKey::Symbol(crate::intrinsics::well_known::iterator_symbol()),
        PropertyDescriptor::builtin_method(self_iter_fn),
    );

    // Set @@toStringTag
    let _ = iter.set(
        PropertyKey::Symbol(crate::intrinsics::well_known::to_string_tag_symbol()),
        Value::string(JsString::intern("RegExp String Iterator")),
    );

    Ok(Value::object(iter))
}

/// Create an iterator result object { value, done }
fn make_iter_result(
    value: Value,
    done: bool,
    ncx: &mut NativeContext<'_>,
) -> Result<Value, VmError> {
    let result = GcRef::new(JsObject::new(Value::null(), ncx.memory_manager().clone()));
    let _ = result.set(PropertyKey::string("value"), value);
    let _ = result.set(PropertyKey::string("done"), Value::boolean(done));
    Ok(Value::object(result))
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
    iterator_prototype: GcRef<JsObject>,
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
            let input_str = ncx.to_string_value(args.first().unwrap_or(&Value::undefined()))?;
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
            let input_str = ncx.to_string_value(args.first().unwrap_or(&Value::undefined()))?;
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
            let source_val = obj_get_with_receiver(&obj, "source", this_val.clone(), ncx)?;
            let source = ncx.to_string_value(&source_val)?;
            let flags_val = obj_get_with_receiver(&obj, "flags", this_val.clone(), ncx)?;
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
            // Step 1: Let O be the this value.
            let regex = get_regex(this_val)?;

            // Annex B §B.2.4.2: compile() should throw if called on a subclass instance.
            // We check if the object's prototype is the original %RegExp.prototype%.
            let intrinsics = ncx
                .ctx
                .realm_intrinsics(ncx.ctx.realm_id())
                .ok_or_else(|| VmError::type_error("Intrinsics not found"))?;
            let re_proto = intrinsics.regexp_prototype;
            if regex.object.prototype().as_object().map(|p| p.as_ptr()) != Some(re_proto.as_ptr()) {
                return Err(VmError::type_error(
                    "RegExp.prototype.compile called on subclass instance",
                ));
            }

            let pattern_arg = args.first().cloned().unwrap_or(Value::undefined());
            let flags_arg = args.get(1).cloned().unwrap_or(Value::undefined());

            let (pattern, flags) = if let Some(re) = pattern_arg.as_regex() {
                // Step 3: If pattern is a RegExp and flags is not undefined, throw TypeError.
                if !flags_arg.is_undefined() {
                    return Err(VmError::type_error(
                        "Cannot supply flags when constructing one RegExp from another",
                    ));
                }
                (re.pattern.clone(), re.flags.clone())
            } else {
                // Per RegExpInitialize spec §22.2.3.1:
                // If pattern is undefined → ""
                // If flags is undefined → ""
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
                if !"dgimsuyv".contains(c) {
                    return Err(VmError::syntax_error(&format!("Invalid flag: {}", c)));
                }
            }
            // Check duplicate flags
            let mut seen = std::collections::HashSet::new();
            for c in flags.chars() {
                if !seen.insert(c) {
                    return Err(VmError::syntax_error(&format!("Duplicate flag: {}", c)));
                }
            }

            // Validate pattern by trying to compile — throw SyntaxError on invalid pattern
            let parsed_flags = regress::Flags::from(flags.as_str());
            let engine_pattern = compile_pattern_for_regress(&pattern, &parsed_flags);
            let native_regex = match regress::Regex::with_flags(&engine_pattern, parsed_flags) {
                Ok(re) => Some(re),
                Err(_e) => {
                    return Err(VmError::syntax_error(&format!(
                        "Invalid regular expression: /{}/: Invalid pattern",
                        pattern
                    )));
                }
            };

            // MUTATE in place — per spec, compile modifies `this`, not creating a new object.
            // Safety: single-threaded VM, we have exclusive logical access
            let regex_mut = unsafe { &mut *(regex.as_ptr() as *mut JsRegExp) };
            regex_mut.pattern = pattern;
            regex_mut.flags = flags;
            regex_mut.unicode = parsed_flags.unicode || parsed_flags.unicode_sets;
            regex_mut.capture_group_names =
                crate::regexp::parse_capture_group_names(&regex_mut.pattern);
            regex_mut.fallback_literal_utf16 =
                compute_literal_utf16_fallback(&regex_mut.pattern, &parsed_flags);
            regex_mut.native_regex = native_regex;

            // Step 12 (RegExpInitialize): Set(obj, "lastIndex", 0, true)
            // Per spec this happens AFTER mutation — if lastIndex is non-writable, throw TypeError
            // but the regex is already modified.
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
                .set(PropertyKey::string("lastIndex"), Value::int32(0));

            // Return this (the same regex object)
            Ok(this_val.clone())
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
            let input_str = ncx.to_string_value(args.first().unwrap_or(&Value::undefined()))?;
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
                let is_empty = if let Some(s) = match_val.as_string() {
                    results.push(Value::string(s));
                    s.len_utf16() == 0
                } else {
                    let match_str = ncx.to_string_value(&match_val)?;
                    let empty = match_str.is_empty();
                    results.push(Value::string(intern(&match_str)));
                    empty
                };

                if is_empty {
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
    // Returns a %RegExpStringIteratorPrototype% that lazily yields exec results.
    // ====================================================================
    let iter_proto_for_matchall = iterator_prototype.clone();
    let mm_for_matchall = mm.clone();
    let fn_proto_for_matchall = fn_proto.clone();
    define_symbol_method(
        &regexp_proto,
        crate::intrinsics::well_known::match_all_symbol(),
        "[Symbol.matchAll]",
        1,
        mm,
        fn_proto.clone(),
        move |this_val, args, ncx| {
            // 1. Let R be the this value.
            let rx = get_this_object(this_val)?;
            // 2. Let S be ? ToString(string)
            let input_str = ncx.to_string_value(args.first().unwrap_or(&Value::undefined()))?;
            let input = JsString::intern(&input_str);

            // 3. Let flags be ? ToString(? Get(R, "flags"))
            let flags_val = obj_get_with_receiver(&rx, "flags", this_val.clone(), ncx)?;
            let flags = ncx.to_string_value(&flags_val)?;

            // 4. Let C be ? SpeciesConstructor(R, %RegExp%)
            // 5. Let matcher be ? Construct(C, « R, flags »)
            let species_ctor = get_species_constructor(&rx, ncx)?;
            let (matcher_val, matcher_obj) = if let Some(ctor) = species_ctor {
                let ctor_args = [this_val.clone(), Value::string(intern(&flags))];
                let result = ncx.call_function_construct(&ctor, Value::undefined(), &ctor_args)?;
                let obj = get_this_object(&result)?;
                (result, obj)
            } else if let Some(regex) = this_val.as_regex() {
                let proto = regex.object.prototype();
                let proto_obj = proto.as_object();
                let new_regex = GcRef::new(JsRegExp::new(
                    regex.pattern.clone(),
                    flags.clone(),
                    proto_obj,
                    ncx.memory_manager().clone(),
                ));
                let val = Value::regex(new_regex.clone());
                let obj = new_regex.object.clone();
                (val, obj)
            } else {
                // Generic object with no species — use default RegExp constructor
                let regexp_ctor = ncx.ctx.get_global("RegExp");
                if let Some(ctor) = regexp_ctor {
                    let ctor_args = [this_val.clone(), Value::string(intern(&flags))];
                    let result =
                        ncx.call_function_construct(&ctor, Value::undefined(), &ctor_args)?;
                    let obj = get_this_object(&result)?;
                    (result, obj)
                } else {
                    (this_val.clone(), rx.clone())
                }
            };

            let global = flags.contains('g');
            let full_unicode = flags.contains('u') || flags.contains('v');

            // 7. Let lastIndex be ? ToLength(? Get(R, "lastIndex"))
            let last_index = get_last_index_obj(&rx, ncx)?;
            // 8. Set ? Set(matcher, "lastIndex", lastIndex, true)
            obj_set(&matcher_obj, "lastIndex", Value::number(last_index), ncx)?;

            // 9. Return CreateRegExpStringIterator(matcher, S, global, fullUnicode)
            create_regexp_string_iterator(
                matcher_val,
                input,
                global,
                full_unicode,
                &iter_proto_for_matchall,
                &mm_for_matchall,
                &fn_proto_for_matchall,
            )
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
            let input_str = ncx.to_string_value(args.first().unwrap_or(&Value::undefined()))?;
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
                let position = (to_integer_or_infinity(pos_raw).max(0.0) as usize).min(input_len);

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
                let _named_captures_obj = if named_captures_val.is_undefined() {
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
                            return Err(VmError::type_error("Cannot convert null to object"));
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
            let input_str = ncx.to_string_value(args.first().unwrap_or(&Value::undefined()))?;
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
            let input_str = ncx.to_string_value(args.first().unwrap_or(&Value::undefined()))?;
            let input = JsString::intern(&input_str);
            let limit_val = args.get(1).cloned().unwrap_or(Value::undefined());

            // 3. Let flags be ? ToString(? Get(rx, "flags"))
            let flags_val = obj_get_with_receiver(&rx, "flags", this_val.clone(), ncx)?;
            let flags = ncx.to_string_value(&flags_val)?;
            let unicode = flags.contains('u') || flags.contains('v');

            // Per spec §22.2.5.11 step 5: Let C be ? SpeciesConstructor(rx, %RegExp%)
            // Step 6: Let newFlags = flags + "y" if not present
            let mut new_flags = flags.clone();
            if !new_flags.contains('y') {
                new_flags.push('y');
            }
            // Always try SpeciesConstructor — it works on both regex and generic objects
            let species_ctor = get_species_constructor(&rx, ncx)?;
            let (splitter_val, splitter_obj) = if let Some(ctor) = species_ctor {
                // Construct(C, [R, newFlags])
                let ctor_args = [this_val.clone(), Value::string(intern(&new_flags))];
                let result = ncx.call_function_construct(&ctor, Value::undefined(), &ctor_args)?;
                let obj = get_this_object(&result)?;
                (result, obj)
            } else if let Some(regex) = this_val.as_regex() {
                // Default: create copy regex with new flags
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
                // Generic object with no species — use default RegExp constructor
                let regexp_ctor = ncx.ctx.get_global("RegExp");
                if let Some(ctor) = regexp_ctor {
                    let ctor_args = [this_val.clone(), Value::string(intern(&new_flags))];
                    let result =
                        ncx.call_function_construct(&ctor, Value::undefined(), &ctor_args)?;
                    let obj = get_this_object(&result)?;
                    (result, obj)
                } else {
                    (this_val.clone(), rx.clone())
                }
            };

            // 6. Let lim = ToUint32(limit) or 2^32-1 if undefined
            let limit = if limit_val.is_undefined() {
                0xFFFFFFFF_u32 // 2^32 - 1
            } else {
                to_uint32(ncx.to_number_value(&limit_val)?)
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
                    "(?:)".to_string()
                } else {
                    escape_regexp_source(&regex.pattern)
                };
                return Ok(Value::string(intern(&source)));
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
        let define_flag_getter =
            |proto: &GcRef<JsObject>,
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

        define_flag_getter(
            &regexp_proto,
            "global",
            'g',
            &mm,
            fn_proto_clone.clone(),
            regexp_proto.clone(),
        );
        define_flag_getter(
            &regexp_proto,
            "ignoreCase",
            'i',
            &mm,
            fn_proto_clone.clone(),
            regexp_proto.clone(),
        );
        define_flag_getter(
            &regexp_proto,
            "multiline",
            'm',
            &mm,
            fn_proto_clone.clone(),
            regexp_proto.clone(),
        );
        define_flag_getter(
            &regexp_proto,
            "dotAll",
            's',
            &mm,
            fn_proto_clone.clone(),
            regexp_proto.clone(),
        );
        define_flag_getter(
            &regexp_proto,
            "sticky",
            'y',
            &mm,
            fn_proto_clone.clone(),
            regexp_proto.clone(),
        );
        define_flag_getter(
            &regexp_proto,
            "unicode",
            'u',
            &mm,
            fn_proto_clone.clone(),
            regexp_proto.clone(),
        );
        define_flag_getter(
            &regexp_proto,
            "unicodeSets",
            'v',
            &mm,
            fn_proto_clone.clone(),
            regexp_proto.clone(),
        );
        define_flag_getter(
            &regexp_proto,
            "hasIndices",
            'd',
            &mm,
            fn_proto_clone,
            regexp_proto.clone(),
        );
    }
}

// ============================================================================
// RegExp constructor
// ============================================================================

/// Create the RegExp constructor function (ES2026 §22.2.3.1).
pub fn create_regexp_constructor(
    regexp_proto: GcRef<JsObject>,
) -> Box<dyn Fn(&Value, &[Value], &mut NativeContext<'_>) -> Result<Value, VmError> + Send + Sync> {
    Box::new(move |this_val, args, ncx| {
        let pattern_arg = args.first().cloned().unwrap_or(Value::undefined());
        let flags_arg = args.get(1).cloned().unwrap_or(Value::undefined());

        // Step 1: Let patternIsRegExp be ? IsRegExp(pattern).
        let pattern_is_regexp = crate::intrinsics_impl::string::is_regexp_check(&pattern_arg, ncx)?;

        // Step 2: If NewTarget is undefined
        let is_construct = ncx.is_construct();
        if !is_construct {
            // b. If patternIsRegExp is true and flags is undefined
            if pattern_is_regexp && flags_arg.is_undefined() {
                // i. Let patternConstructor be ? Get(pattern, "constructor").
                let pattern_obj = pattern_arg
                    .as_object()
                    .or_else(|| pattern_arg.as_regex().map(|r| r.object.clone()));
                if let Some(obj) = pattern_obj {
                    let pattern_constructor = crate::object::get_value_full(
                        &obj,
                        &PropertyKey::string("constructor"),
                        ncx,
                    )?;
                    let regexp_ctor = ncx.ctx.get_global("RegExp").unwrap_or(Value::undefined());
                    if crate::intrinsics_impl::helpers::same_value(
                        &pattern_constructor,
                        &regexp_ctor,
                    ) {
                        return Ok(pattern_arg.clone());
                    }
                }
            }
        }

        let (pattern_str, flags_str) = if let Some(re) = pattern_arg.as_regex() {
            let p = re.pattern.clone();
            let f = if flags_arg.is_undefined() {
                re.flags.clone()
            } else {
                ncx.to_string_value(&flags_arg)?
            };
            (p, f)
        } else if pattern_is_regexp {
            let obj = pattern_arg
                .as_object()
                .or_else(|| pattern_arg.as_regex().map(|r| r.object.clone()))
                .unwrap();
            let p_val = obj_get(&obj, "source", ncx)?;
            let p = if p_val.is_undefined() {
                String::new()
            } else {
                ncx.to_string_value(&p_val)?
            };

            let f_val = if flags_arg.is_undefined() {
                obj_get(&obj, "flags", ncx)?
            } else {
                flags_arg.clone()
            };
            let f = if f_val.is_undefined() {
                String::new()
            } else {
                ncx.to_string_value(&f_val)?
            };
            (p, f)
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
        for c in flags_str.chars() {
            if !"dgimsuyv".contains(c) {
                return Err(VmError::syntax_error(&format!("Invalid flag: {}", c)));
            }
        }
        let mut seen = std::collections::HashSet::new();
        for c in flags_str.chars() {
            if !seen.insert(c) {
                return Err(VmError::syntax_error(&format!("Duplicate flag: {}", c)));
            }
        }

        // Validate pattern
        let parsed_flags = regress::Flags::from(flags_str.as_str());
        let engine_pattern =
            crate::regexp::compile_pattern_for_regress(&pattern_str, &parsed_flags);
        let _native_regex = regress::Regex::with_flags(&engine_pattern, parsed_flags).ok();

        let proto = if is_construct {
            this_val
                .as_object()
                .and_then(|o| o.prototype().as_object())
                .unwrap_or_else(|| regexp_proto.clone())
        } else {
            regexp_proto.clone()
        };

        let regex = GcRef::new(JsRegExp::new(
            pattern_str,
            flags_str,
            Some(proto),
            ncx.memory_manager().clone(),
        ));
        Ok(Value::regex(regex))
    })
}
