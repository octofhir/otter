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
        Self::from_space_with_heap(space, None)
    }

    /// Same as [`from_space`] but with the heap available so Number /
    /// String wrapper objects can be unwrapped per §25.5.2.4 step 5.
    pub fn from_space_with_heap(
        space: &Value,
        gc_heap: Option<&otter_gc::GcHeap>,
    ) -> Result<Self, JsonError> {
        // §25.5.2.4 step 5 — unwrap Number / String wrapper objects.
        let unwrapped: Value;
        let space = if let Some(obj) = space.as_object()
            && let Some(heap) = gc_heap
        {
            if let Some(n) = crate::object::number_data(obj, heap) {
                unwrapped = Value::number(n);
                &unwrapped
            } else if let Some(s) = crate::object::string_data(obj, heap) {
                unwrapped = Value::string(s);
                &unwrapped
            } else {
                space
            }
        } else {
            space
        };
        if space.is_undefined() || space.is_null() {
            return Ok(Self::default());
        }
        if let Some(n) = space.as_number() {
            let f = n.as_f64();
            let count = if f.is_nan() || f <= 0.0 {
                0
            } else if f >= 10.0 {
                10
            } else {
                f.trunc() as usize
            };
            return Ok(Self {
                indent: vec![b' '; count],
            });
        }
        if let Some(s) = gc_heap.and_then(|h| space.as_string(h).map(|s| (s, h))) {
            let bytes = s.0.to_lossy_string(s.1).into_bytes();
            let take = bytes.len().min(10);
            return Ok(Self {
                indent: bytes[..take].to_vec(),
            });
        }
        // §25.5.2.4 step 8 — empty gap fallback.
        Ok(Self::default())
    }

    /// `true` when the output should be pretty-printed.
    fn pretty(&self) -> bool {
        !self.indent.is_empty()
    }
}

/// Convenience entry: serialises with no indent.
pub fn stringify(
    value: &Value,
    gc_heap: &mut otter_gc::GcHeap,
) -> Result<Option<String>, JsonError> {
    stringify_with_options(value, &StringifyOptions::default(), gc_heap)
}

/// Full entry. Returns `Some(text)` for a serialisable root value,
/// `None` for `undefined`/functions/etc. (matches spec).
pub fn stringify_with_options(
    value: &Value,
    options: &StringifyOptions,
    gc_heap: &mut otter_gc::GcHeap,
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
    gc_heap: &mut otter_gc::GcHeap,
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
                        next_pair = Some((k.clone(), *v));
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
    gc_heap: &mut otter_gc::GcHeap,
) -> Result<(), JsonError> {
    if value.is_null() {
        out.push_str("null");
    } else if value.is_undefined() || value.is_hole() {
        // Top-level `undefined` filtered upstream; nested also routes
        // to `null` upstream — belt-and-braces.
        out.push_str("null");
    } else if let Some(b) = value.as_boolean() {
        out.push_str(if b { "true" } else { "false" });
    } else if let Some(n) = value.as_number() {
        write_number(out, n);
    } else if value.is_big_int() {
        return Err(JsonError::BigInt);
    } else if let Some(s) = value.as_string(gc_heap) {
        write_string_literal(out, &s.to_lossy_string(gc_heap));
    } else if let Some(arr) = value.as_array() {
        // Lazy stringify memcpy fast-path: capture text from JSON.parse.
        if !options.pretty()
            && let Some(source) = crate::array::clean_source_bytes(arr, gc_heap)
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
        visit.enter_array(&arr)?;
        out.push('[');
        stack.push(Frame::Array {
            arr,
            idx: 0,
            had_member: false,
        });
    } else if let Some(obj) = value.as_object() {
        // §25.5.2 Date instances expose `[[DateValue]]`.
        if let Some(time) = crate::object::date_data(obj, gc_heap) {
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
        visit.enter_object(&obj)?;
        out.push('{');
        // §25.5.2.4 SerializeJSONObject step 4 — enumerable own string keys.
        let entries: Vec<(String, Value)> = crate::object::with_properties(obj, gc_heap, |p| {
            p.enumerable_data_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect()
        });
        stack.push(Frame::Object {
            entries,
            idx: 0,
            had_member: false,
            _root: obj,
        });
    } else if let Some(ta) = value.as_typed_array(gc_heap) {
        // §25.5.2 — TypedArrays serialise like array-likes.
        if stack.len() >= MAX_NESTING_DEPTH {
            return Err(JsonError::TooDeep {
                limit: MAX_NESTING_DEPTH,
            });
        }
        out.push('[');
        let len = ta.length(gc_heap);
        for i in 0..len {
            if i > 0 {
                out.push(',');
            }
            let value = ta.get(gc_heap, i)?;
            emit_value(&value, out, stack, visit, options, gc_heap)?;
        }
        out.push(']');
    } else {
        // Symbols, functions, Map / Set / Weak collections, etc.
        // — render as `null` per §25.5.2 + foundation fallback.
        out.push_str("null");
    }
    let _ = options;
    Ok(())
}

fn is_serialisable(value: &Value) -> bool {
    !(value.is_undefined()
        || value.is_function()
        || value.is_closure()
        || value.is_bound_function()
        || value.is_native_function()
        || value.is_iterator()
        || value.is_regexp()
        || value.is_class_constructor())
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
        let opts = StringifyOptions::from_space(&Value::number(NumberValue::from_i32(20))).unwrap();
        assert_eq!(opts.indent.len(), 10);
        let opts = StringifyOptions::from_space(&Value::number(NumberValue::from_i32(-5))).unwrap();
        assert_eq!(opts.indent.len(), 0);
    }

    #[test]
    fn options_string_truncated_to_ten_bytes() {
        let mut heap = otter_gc::GcHeap::new().expect("heap");
        let s = crate::string::JsString::from_str("xxxxxxxxxxYYYY", &mut heap).unwrap();
        let opts = StringifyOptions::from_space_with_heap(&Value::string(s), Some(&heap)).unwrap();
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
        let mut obj = crate::object::alloc_object_old_for_fixture(&mut heap).unwrap();
        let self_ref = Value::object(obj);
        crate::object::set(&mut obj, &mut heap, "self", self_ref);
        let err = stringify(&Value::object(obj), &mut heap).unwrap_err();
        assert!(matches!(err, JsonError::Cyclic));
    }
}
