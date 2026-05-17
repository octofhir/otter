//! Iterative `JSON.stringify` walker with explicit cycle + depth
//! guards.
//!
//! The walker maintains a single output `String` plus a [`Frame`]
//! stack — one frame per `[` / `{` we have descended into. Each
//! frame remembers whether we still need a leading comma plus the
//! children remaining to emit. This keeps the cost per node at
//! O(1) bookkeeping and avoids the per-recursion Drop overhead a
//! recursive serializer pays.
//!
//! # Contents
//! - [`stringify`] — convenience entry returning `Option<String>`.
//! - [`stringify_with_options`] — the same with a programmable
//!   `space` indent.
//! - [`StringifyOptions`] — `space` indent (≤10 spaces or ≤10-char
//!   string).
//!
//! # Invariants
//! - Frames live on a `Vec` and pop in strict LIFO order.
//! - Cycle detection compares object/array handles by their `Rc`
//!   data-pointer (`identity_addr`), so we never round-trip
//!   through string keys.
//! - Number formatting mirrors `Number.prototype.toString` (which
//!   we already ship), so the output is byte-identical to V8 for
//!   the foundation subset.

use crate::Value;
use crate::array::JsArray;
use crate::number::NumberValue;
use crate::object::JsObject;

use super::{JsonError, MAX_NESTING_DEPTH};

/// `JSON.stringify` options.
#[derive(Debug, Clone, Default)]
pub struct StringifyOptions {
    /// Indent per level (0–10 ASCII bytes). Empty means compact.
    indent: Vec<u8>,
}

impl StringifyOptions {
    /// Build a [`StringifyOptions`] from the JS `space` argument.
    pub fn from_space(space: &Value) -> Result<Self, JsonError> {
        match space {
            Value::Undefined | Value::Null => Ok(Self::default()),
            Value::Number(n) => {
                let f = n.as_f64();
                let count = if f.is_nan() || f <= 0.0 {
                    0
                } else if f >= 10.0 {
                    10
                } else {
                    f.trunc() as usize
                };
                Ok(Self {
                    indent: vec![b' '; count],
                })
            }
            Value::String(s) => {
                let lossy = s.to_lossy_string();
                let bytes = lossy.as_bytes();
                let take = bytes.len().min(10);
                Ok(Self {
                    indent: bytes[..take].to_vec(),
                })
            }
            _ => Err(JsonError::BadArgument {
                name: "stringify",
                index: 2,
                reason: "must be a number or string",
            }),
        }
    }

    /// `true` when the output should be pretty-printed.
    fn pretty(&self) -> bool {
        !self.indent.is_empty()
    }
}

/// Convenience entry: serialises with no indent.
pub fn stringify(value: &Value, gc_heap: &otter_gc::GcHeap) -> Result<Option<String>, JsonError> {
    stringify_with_options(value, &StringifyOptions::default(), gc_heap)
}

/// Full entry. Returns `Some(text)` for a serialisable root value,
/// `None` for `undefined`/functions/etc. (matches spec).
pub fn stringify_with_options(
    value: &Value,
    options: &StringifyOptions,
    gc_heap: &otter_gc::GcHeap,
) -> Result<Option<String>, JsonError> {
    if !is_serialisable(value) {
        return Ok(None);
    }
    let mut out = String::new();
    let mut visit = VisitSet::default();
    let mut stack: Vec<Frame> = Vec::with_capacity(8);
    emit_value(value, &mut out, &mut stack, &mut visit, options, gc_heap)?;
    drive(&mut out, &mut stack, &mut visit, options, gc_heap)?;
    Ok(Some(out))
}

#[derive(Default)]
struct VisitSet {
    objects: Vec<*const ()>,
    arrays: Vec<*const ()>,
}

impl VisitSet {
    fn enter_object(&mut self, obj: &JsObject) -> Result<(), JsonError> {
        let key = obj.as_header_ptr() as *const ();
        if self.objects.contains(&key) {
            return Err(JsonError::Cyclic);
        }
        self.objects.push(key);
        Ok(())
    }
    fn leave_object(&mut self) {
        self.objects.pop();
    }
    fn enter_array(&mut self, arr: &JsArray) -> Result<(), JsonError> {
        let key = crate::array::identity_addr(*arr);
        if self.arrays.contains(&key) {
            return Err(JsonError::Cyclic);
        }
        self.arrays.push(key);
        Ok(())
    }
    fn leave_array(&mut self) {
        self.arrays.pop();
    }
}

/// One frame on the iterative-walk stack.
enum Frame {
    Array {
        arr: JsArray,
        idx: usize,
        had_member: bool,
    },
    /// Snapshot the (key, value) pairs up front so insertion order
    /// is fixed even if a reviver mutates the receiver mid-walk.
    Object {
        entries: Vec<(String, Value)>,
        idx: usize,
        had_member: bool,
        // Anchors the `Rc` for the lifetime of the frame so the
        // `identity_addr` stored in `VisitSet` remains valid.
        _root: JsObject,
    },
}

/// Iterative driver: pulls work off the frame stack until empty.
fn drive(
    out: &mut String,
    stack: &mut Vec<Frame>,
    visit: &mut VisitSet,
    options: &StringifyOptions,
    gc_heap: &otter_gc::GcHeap,
) -> Result<(), JsonError> {
    loop {
        let depth = stack.len();
        let Some(top) = stack.last_mut() else {
            return Ok(());
        };
        match top {
            Frame::Array {
                arr,
                idx,
                had_member,
            } => {
                if *idx >= crate::array::len(*arr, gc_heap) {
                    if options.pretty() && *had_member {
                        write_indent(out, &options.indent, depth - 1);
                    }
                    out.push(']');
                    stack.pop();
                    visit.leave_array();
                    continue;
                }
                let elem = crate::array::get(*arr, gc_heap, *idx);
                *idx += 1;
                if *had_member {
                    out.push(',');
                } else {
                    *had_member = true;
                }
                if options.pretty() {
                    write_indent(out, &options.indent, depth);
                }
                // Spec: undefined / function / symbol in array → null.
                if !is_serialisable(&elem) {
                    out.push_str("null");
                    continue;
                }
                emit_value(&elem, out, stack, visit, options, gc_heap)?;
            }
            Frame::Object {
                entries,
                idx,
                had_member,
                ..
            } => {
                // Skip non-serialisable values for object members
                // (spec: omit them entirely).
                let mut next_pair: Option<(String, Value)> = None;
                while *idx < entries.len() {
                    let (k, v) = &entries[*idx];
                    *idx += 1;
                    if is_serialisable(v) {
                        next_pair = Some((k.clone(), v.clone()));
                        break;
                    }
                }
                let Some((key, value)) = next_pair else {
                    if options.pretty() && *had_member {
                        write_indent(out, &options.indent, depth - 1);
                    }
                    out.push('}');
                    stack.pop();
                    visit.leave_object();
                    continue;
                };
                if *had_member {
                    out.push(',');
                } else {
                    *had_member = true;
                }
                if options.pretty() {
                    write_indent(out, &options.indent, depth);
                }
                write_string_literal(out, &key);
                out.push(':');
                if options.pretty() {
                    out.push(' ');
                }
                emit_value(&value, out, stack, visit, options, gc_heap)?;
            }
        }
    }
}

/// Emit one value: leaves write directly into `out`; containers
/// open the bracket and push a frame on `stack`.
fn emit_value(
    value: &Value,
    out: &mut String,
    stack: &mut Vec<Frame>,
    visit: &mut VisitSet,
    options: &StringifyOptions,
    gc_heap: &otter_gc::GcHeap,
) -> Result<(), JsonError> {
    match value {
        Value::Null => out.push_str("null"),
        // Top-level `undefined` is filtered upstream; nested
        // `undefined` reaches us only inside arrays where the
        // caller already substituted `null`. As a safety net, treat
        // it as null too.
        Value::Undefined | Value::Hole => out.push_str("null"),
        Value::Boolean(true) => out.push_str("true"),
        Value::Boolean(false) => out.push_str("false"),
        Value::Number(n) => write_number(out, *n),
        Value::BigInt(_) => return Err(JsonError::BigInt),
        Value::String(s) => write_string_literal(out, &s.to_lossy_string()),
        Value::Array(arr) => {
            // Lazy stringify memcpy fast-path: an array that came
            // from `JSON.parse` and has not been mutated since
            // captures the original textual `[…]` slice on its body.
            // When every element is a render-stable primitive
            // (numbers / strings / booleans / null) the captured
            // bytes are still authoritative, so we re-emit them
            // verbatim without descending. Pretty-printing changes
            // layout, so it disables the fast path.
            //
            // Spec: <https://tc39.es/ecma262/#sec-json.stringify> §25.5.2
            if !options.pretty()
                && let Some(source) = crate::array::clean_source_bytes(*arr, gc_heap)
                && let Ok(text) = std::str::from_utf8(&source)
            {
                out.push_str(text);
                return Ok(());
            }
            if stack.len() >= MAX_NESTING_DEPTH {
                return Err(JsonError::TooDeep {
                    limit: MAX_NESTING_DEPTH,
                });
            }
            visit.enter_array(arr)?;
            out.push('[');
            stack.push(Frame::Array {
                arr: *arr,
                idx: 0,
                had_member: false,
            });
        }
        Value::Object(obj) => {
            // §25.5.2 Date instances are ordinary objects with a
            // `[[DateValue]]` internal slot — emit the ISO 8601
            // form before falling into the generic object branch.
            // Mirrors `Date.prototype.toJSON` (§21.4.4.41).
            // <https://tc39.es/ecma262/#sec-date.prototype.tojson>
            if let Some(time) = crate::object::date_data(*obj, gc_heap) {
                match crate::date::to_iso_string(time) {
                    Some(s) => write_string_literal(out, &s),
                    None => out.push_str("null"),
                }
                return Ok(());
            }
            if stack.len() >= MAX_NESTING_DEPTH {
                return Err(JsonError::TooDeep {
                    limit: MAX_NESTING_DEPTH,
                });
            }
            visit.enter_object(obj)?;
            out.push('{');
            // Per ECMA-262 §25.5.2.4 SerializeJSONObject step 4 we
            // walk only the enumerable own string keys. Accessor
            // slots are skipped here for the slice — invoking
            // getters during serialisation requires interpreter
            // access and is filed as a follow-up.
            // <https://tc39.es/ecma262/#sec-serializejsonobject>
            let entries: Vec<(String, Value)> =
                crate::object::with_properties(*obj, gc_heap, |p| {
                    p.enumerable_data_iter()
                        .map(|(k, v)| (k.to_string(), v))
                        .collect()
                });
            stack.push(Frame::Object {
                entries,
                idx: 0,
                had_member: false,
                _root: *obj,
            });
        }
        // Symbols are silently dropped by `JSON.stringify` per
        // §25.5.2. Inside an array context the upstream walker has
        // already substituted `null`; for top-level symbols and
        // belt-and-braces guards we also emit `null` here. Map /
        // Set / Weak collections do not have a JSON representation
        // either — their default serialisation is `{}`. For the
        // foundation we render them as `null` to match the
        // existing wildcard behaviour.
        Value::Symbol(_)
        | Value::Function { .. }
        | Value::Closure { .. }
        | Value::BoundFunction(_)
        | Value::NativeFunction(_)
        | Value::Iterator(_)
        | Value::RegExp(_)
        | Value::Promise(_)
        | Value::ClassConstructor(_)
        | Value::Map(_)
        | Value::Set(_)
        | Value::WeakMap(_)
        | Value::WeakSet(_)
        | Value::WeakRef(_)
        | Value::FinalizationRegistry(_)
        | Value::Temporal(_)
        | Value::Intl(_)
        | Value::ArrayBuffer(_)
        | Value::DataView(_)
        | Value::Generator(_)
        | Value::Proxy(_) => {
            out.push_str("null");
        }
        // §25.5.2 — TypedArrays serialise like ordinary array-likes:
        // their indexed elements emit as a JSON array. Spec §25.5.2.4
        // SerializeJSONArray treats them through the array branch.
        Value::TypedArray(ta) => {
            if stack.len() >= MAX_NESTING_DEPTH {
                return Err(JsonError::TooDeep {
                    limit: MAX_NESTING_DEPTH,
                });
            }
            out.push('[');
            for i in 0..ta.length() {
                if i > 0 {
                    out.push(',');
                }
                let value = ta.get(i);
                emit_value(&value, out, stack, visit, options, gc_heap)?;
            }
            out.push(']');
        }
    }
    let _ = options;
    Ok(())
}

fn is_serialisable(value: &Value) -> bool {
    !matches!(
        value,
        Value::Undefined
            | Value::Function { .. }
            | Value::Closure { .. }
            | Value::BoundFunction(_)
            | Value::NativeFunction(_)
            | Value::Iterator(_)
            | Value::RegExp(_)
            | Value::ClassConstructor(_)
    )
}

fn write_number(out: &mut String, n: NumberValue) {
    let f = n.as_f64();
    if !f.is_finite() {
        out.push_str("null");
        return;
    }
    if f == 0.0 {
        out.push('0');
        return;
    }
    out.push_str(&n.to_display_string());
}

/// Hand-rolled JSON string encoder. Bulk-skips clean ASCII spans
/// via [`super::scan::find_first_escape`] (8 bytes/iteration) and
/// only enters the per-byte escape switch for `"`, `\\`, or
/// control characters. Non-ASCII (≥ 0x80) bytes are forwarded
/// verbatim — UTF-8 lead and continuation bytes are clean from
/// the JSON-string viewpoint.
///
/// Spec: <https://tc39.es/ecma262/#sec-quotejsonstring> §25.5.2.2
fn write_string_literal(out: &mut String, s: &str) {
    use std::fmt::Write as _;
    out.push('"');
    let bytes = s.as_bytes();
    let mut i = 0;
    let mut copy_from = 0;
    while i < bytes.len() {
        i = super::scan::find_first_escape_pub(bytes, i);
        if i >= bytes.len() {
            break;
        }
        if copy_from < i {
            out.push_str(&s[copy_from..i]);
        }
        let b = bytes[i];
        match b {
            b'"' => out.push_str("\\\""),
            b'\\' => out.push_str("\\\\"),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b'\t' => out.push_str("\\t"),
            0x08 => out.push_str("\\b"),
            0x0C => out.push_str("\\f"),
            _ => {
                // Remaining bytes hit by the scanner are 0x00..=0x1F
                // control characters that have no shorthand escape.
                let _ = write!(out, "\\u{b:04x}");
            }
        }
        i += 1;
        copy_from = i;
    }
    if copy_from < bytes.len() {
        out.push_str(&s[copy_from..]);
    }
    out.push('"');
}

fn write_indent(out: &mut String, indent: &[u8], depth: usize) {
    out.push('\n');
    let slice = std::str::from_utf8(indent).unwrap_or("");
    for _ in 0..depth {
        out.push_str(slice);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::number::NumberValue;

    #[test]
    fn options_space_clamped_to_ten() {
        let opts = StringifyOptions::from_space(&Value::Number(NumberValue::from_i32(20))).unwrap();
        assert_eq!(opts.indent.len(), 10);
        let opts = StringifyOptions::from_space(&Value::Number(NumberValue::from_i32(-5))).unwrap();
        assert_eq!(opts.indent.len(), 0);
    }

    #[test]
    fn options_string_truncated_to_ten_bytes() {
        let heap = crate::string::StringHeap::default();
        let s = crate::string::JsString::from_str("xxxxxxxxxxYYYY", &heap).unwrap();
        let opts = StringifyOptions::from_space(&Value::String(s)).unwrap();
        assert_eq!(opts.indent.len(), 10);
    }

    #[test]
    fn write_string_literal_handles_escapes() {
        let mut s = String::new();
        write_string_literal(&mut s, "a\nb\\c\"d");
        assert_eq!(s, "\"a\\nb\\\\c\\\"d\"");
    }

    #[test]
    fn write_string_literal_handles_control_chars() {
        let mut s = String::new();
        write_string_literal(&mut s, "\x01\x1F");
        assert_eq!(s, "\"\\u0001\\u001f\"");
    }

    #[test]
    fn cycle_detection_handles_self_reference() {
        let mut heap = otter_gc::GcHeap::new().expect("gc heap");
        let obj = crate::object::alloc_object_old_for_fixture(&mut heap).unwrap();
        crate::object::set(obj, &mut heap, "self", Value::Object(obj));
        let err = stringify(&Value::Object(obj), &heap).unwrap_err();
        assert!(matches!(err, JsonError::Cyclic));
    }
}
