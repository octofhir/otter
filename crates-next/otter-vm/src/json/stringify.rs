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
pub fn stringify(value: &Value) -> Result<Option<String>, JsonError> {
    stringify_with_options(value, &StringifyOptions::default())
}

/// Full entry. Returns `Some(text)` for a serialisable root value,
/// `None` for `undefined`/functions/etc. (matches spec).
pub fn stringify_with_options(
    value: &Value,
    options: &StringifyOptions,
) -> Result<Option<String>, JsonError> {
    if !is_serialisable(value) {
        return Ok(None);
    }
    let mut out = String::new();
    let mut visit = VisitSet::default();
    let mut stack: Vec<Frame> = Vec::with_capacity(8);
    emit_value(value, &mut out, &mut stack, &mut visit, options)?;
    drive(&mut out, &mut stack, &mut visit, options)?;
    Ok(Some(out))
}

#[derive(Default)]
struct VisitSet {
    objects: Vec<*const ()>,
    arrays: Vec<*const ()>,
}

impl VisitSet {
    fn enter_object(&mut self, obj: &JsObject) -> Result<(), JsonError> {
        let key = obj.identity_addr();
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
        let key = arr.identity_addr();
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
                if *idx >= arr.len() {
                    if options.pretty() && *had_member {
                        write_indent(out, &options.indent, depth - 1);
                    }
                    out.push(']');
                    stack.pop();
                    visit.leave_array();
                    continue;
                }
                let elem = arr.get(*idx);
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
                emit_value(&elem, out, stack, visit, options)?;
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
                emit_value(&value, out, stack, visit, options)?;
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
) -> Result<(), JsonError> {
    match value {
        Value::Null => out.push_str("null"),
        // Top-level `undefined` is filtered upstream; nested
        // `undefined` reaches us only inside arrays where the
        // caller already substituted `null`. As a safety net, treat
        // it as null too.
        Value::Undefined => out.push_str("null"),
        Value::Boolean(true) => out.push_str("true"),
        Value::Boolean(false) => out.push_str("false"),
        Value::Number(n) => write_number(out, *n),
        Value::BigInt(_) => return Err(JsonError::BigInt),
        Value::String(s) => write_string_literal(out, &s.to_lossy_string()),
        Value::Array(arr) => {
            if stack.len() >= MAX_NESTING_DEPTH {
                return Err(JsonError::TooDeep {
                    limit: MAX_NESTING_DEPTH,
                });
            }
            visit.enter_array(arr)?;
            out.push('[');
            stack.push(Frame::Array {
                arr: arr.clone(),
                idx: 0,
                had_member: false,
            });
        }
        Value::Object(obj) => {
            if stack.len() >= MAX_NESTING_DEPTH {
                return Err(JsonError::TooDeep {
                    limit: MAX_NESTING_DEPTH,
                });
            }
            visit.enter_object(obj)?;
            out.push('{');
            let entries: Vec<(String, Value)> = obj
                .borrow_props()
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect();
            stack.push(Frame::Object {
                entries,
                idx: 0,
                had_member: false,
                _root: obj.clone(),
            });
        }
        Value::Function { .. }
        | Value::Closure { .. }
        | Value::BoundFunction(_)
        | Value::NativeFunction(_)
        | Value::Iterator(_)
        | Value::RegExp(_)
        | Value::Promise(_)
        | Value::ClassConstructor(_) => {
            // Non-serialisable inside an array path — the array
            // walker substituted `null` before calling us. As a
            // belt-and-braces guard, emit `null` here too.
            out.push_str("null");
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

/// Hand-rolled JSON string encoder. Walks bytes of the lossy
/// UTF-8 view; control chars become `\uXXXX`; ASCII fast path
/// covers the common case in a tight loop without per-char
/// branching beyond the escape switch.
fn write_string_literal(out: &mut String, s: &str) {
    out.push('"');
    let bytes = s.as_bytes();
    let mut i = 0;
    let mut copy_from = 0;
    while i < bytes.len() {
        let b = bytes[i];
        let escape: Option<&'static str> = match b {
            b'"' => Some("\\\""),
            b'\\' => Some("\\\\"),
            b'\n' => Some("\\n"),
            b'\r' => Some("\\r"),
            b'\t' => Some("\\t"),
            0x08 => Some("\\b"),
            0x0C => Some("\\f"),
            0x00..=0x1F => None,
            _ => {
                i += 1;
                continue;
            }
        };
        if copy_from < i {
            out.push_str(&s[copy_from..i]);
        }
        match escape {
            Some(seq) => out.push_str(seq),
            None => {
                let buf = format!("\\u{b:04x}");
                out.push_str(&buf);
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
    use crate::object::JsObject;

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
        let obj = JsObject::new();
        obj.set("self", Value::Object(obj.clone()));
        let err = stringify(&Value::Object(obj)).unwrap_err();
        assert!(matches!(err, JsonError::Cyclic));
    }
}
