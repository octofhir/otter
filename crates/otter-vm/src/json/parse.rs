//! Strict, hand-rolled `JSON.parse`.
//!
//! No `serde_json` — measured to be 3-5× slower on the JS shapes
//! we serialize. We walk a byte cursor over `&[u8]` and only
//! decode UTF-8 when we lift a JSON string into a [`JsString`].
//! Strings with `\uXXXX` escapes go through a small WTF-16 buffer
//! so surrogate pairs round-trip losslessly.
//!
//! # Contents
//! - [`parse`] — public entry, byte cursor → [`Value`].
//! - [`ParseError`] — strict-mode failure with byte offset.
//!
//! # Invariants
//! - Recursion-free. Object / array bodies live on a Rust stack of
//!   builder frames capped at [`super::MAX_NESTING_DEPTH`] levels.
//! - Strict spec: trailing commas, comments, single quotes, leading
//!   `+`, leading zeros, NaN / Infinity / undefined / hex literals
//!   — all rejected.
//! - Numeric overflow falls back to `f64::INFINITY` per IEEE-754
//!   round-toward-infinity (matches V8's strtod path for huge
//!   inputs).

use std::sync::Arc;

use otter_gc::heap::RootSlotVisitor;

use crate::Value;
use crate::number::NumberValue;
use crate::string::JsString;

use super::MAX_NESTING_DEPTH;

/// Strict-mode parse failure.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{message} at byte {position}")]
pub struct ParseError {
    /// Diagnostic body.
    pub message: String,
    /// 0-based byte offset.
    pub position: usize,
}

impl ParseError {
    fn at(pos: usize, message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            position: pos,
        }
    }
}

/// Builder frame for the iterative parser. Each frame represents
/// a `[` or `{` we've descended into and is responsible for
/// stitching its children together.
enum Builder {
    Array {
        elements: Vec<Value>,
        // `true` once we've emitted at least one element so the
        // separator state machine knows whether to demand a `,`.
        has_member: bool,
        // Byte offset of the opening `[` in the input. Lets
        // `finish_builder` capture the verbatim slice for the
        // stringify memcpy fast-path.
        array_start: usize,
    },
    Object {
        entries: Vec<(String, Value)>,
        // The most recently parsed key, awaiting its value. `None`
        // before the next key starts.
        pending_key: Option<String>,
        has_member: bool,
    },
}

/// Strict `JSON.parse`. `gc_heap` allocates both `JsString` values
/// and `JsObject`s for parsed JSON objects.
pub fn parse(text: &str, gc_heap: &mut otter_gc::GcHeap) -> Result<Value, ParseError> {
    let mut external_visit = |_visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {};
    parse_with_roots(text, gc_heap, &mut external_visit)
}

/// Strict `JSON.parse` with an explicit GC root visitor for caller-owned
/// runtime/native/frame roots.
pub(crate) fn parse_with_roots(
    text: &str,
    gc_heap: &mut otter_gc::GcHeap,
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<Value, ParseError> {
    let bytes = text.as_bytes();
    let mut cursor = Cursor { bytes, pos: 0 };
    cursor.skip_ws();
    let value = read_value(&mut cursor, gc_heap, external_visit)?;
    cursor.skip_ws();
    if cursor.pos != bytes.len() {
        return Err(ParseError::at(cursor.pos, "unexpected trailing content"));
    }
    Ok(value)
}

struct Cursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl Cursor<'_> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn expect(&mut self, byte: u8, expected: &'static str) -> Result<(), ParseError> {
        match self.peek() {
            Some(b) if b == byte => {
                self.pos += 1;
                Ok(())
            }
            Some(_) => Err(ParseError::at(self.pos, format!("expected {expected}"))),
            None => Err(ParseError::at(
                self.pos,
                format!("unexpected end of input, expected {expected}"),
            )),
        }
    }

    fn skip_ws(&mut self) {
        while let Some(b) = self.peek() {
            if matches!(b, b' ' | b'\n' | b'\r' | b'\t') {
                self.pos += 1;
            } else {
                break;
            }
        }
    }
}

/// Drive the iterative parser. The input is a single value
/// (primitive or compound); compound builders live on `stack`.
fn read_value(
    cursor: &mut Cursor<'_>,
    gc_heap: &mut otter_gc::GcHeap,
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<Value, ParseError> {
    let mut stack: Vec<Builder> = Vec::with_capacity(8);
    let result = read_step(cursor, &mut stack, gc_heap, external_visit)?;
    let mut current = result;
    while stack.last().is_some() {
        cursor.skip_ws();
        current = continue_container(cursor, &mut stack, current, gc_heap, external_visit)?;
    }
    Ok(current)
}

/// Read one value. If it's a primitive, return immediately. If
/// it's a container, push a builder frame and return a sentinel
/// `Undefined` — the driver picks up from `continue_container`.
fn read_step(
    cursor: &mut Cursor<'_>,
    stack: &mut Vec<Builder>,
    gc_heap: &mut otter_gc::GcHeap,
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<Value, ParseError> {
    cursor.skip_ws();
    let b = cursor
        .peek()
        .ok_or_else(|| ParseError::at(cursor.pos, "unexpected end of input"))?;
    match b {
        b'{' => {
            cursor.pos += 1;
            if stack.len() >= MAX_NESTING_DEPTH {
                return Err(ParseError::at(cursor.pos, "JSON nesting too deep"));
            }
            stack.push(Builder::Object {
                entries: Vec::new(),
                pending_key: None,
                has_member: false,
            });
            // Object body might be empty: handle `{}` directly.
            cursor.skip_ws();
            if cursor.peek() == Some(b'}') {
                cursor.pos += 1;
                let frame = stack.pop().expect("just pushed");
                let bytes = cursor.bytes;
                return finish_builder(frame, stack, bytes, gc_heap, cursor.pos, external_visit);
            }
            // Otherwise read the first key.
            let key = read_object_key(cursor, gc_heap)?;
            if let Some(Builder::Object { pending_key, .. }) = stack.last_mut() {
                *pending_key = Some(key);
            }
            cursor.skip_ws();
            cursor.expect(b':', "':' after object key")?;
            cursor.skip_ws();
            // Recurse into the value.
            read_step(cursor, stack, gc_heap, external_visit)
        }
        b'[' => {
            let array_start = cursor.pos;
            cursor.pos += 1;
            if stack.len() >= MAX_NESTING_DEPTH {
                return Err(ParseError::at(cursor.pos, "JSON nesting too deep"));
            }
            stack.push(Builder::Array {
                elements: Vec::new(),
                has_member: false,
                array_start,
            });
            cursor.skip_ws();
            if cursor.peek() == Some(b']') {
                cursor.pos += 1;
                let frame = stack.pop().expect("just pushed");
                let bytes = cursor.bytes;
                return finish_builder(frame, stack, bytes, gc_heap, cursor.pos, external_visit);
            }
            read_step(cursor, stack, gc_heap, external_visit)
        }
        b'"' => Ok(Value::String(read_string(cursor, gc_heap)?)),
        b't' => {
            consume_keyword(cursor, b"true")?;
            Ok(Value::Boolean(true))
        }
        b'f' => {
            consume_keyword(cursor, b"false")?;
            Ok(Value::Boolean(false))
        }
        b'n' => {
            consume_keyword(cursor, b"null")?;
            Ok(Value::Null)
        }
        b'-' | b'0'..=b'9' => Ok(Value::Number(read_number(cursor)?)),
        other => Err(ParseError::at(
            cursor.pos,
            format!("unexpected byte 0x{other:02x}"),
        )),
    }
}

/// After completing one value inside a compound, decide whether
/// we close the container or read the next member.
fn continue_container(
    cursor: &mut Cursor<'_>,
    stack: &mut Vec<Builder>,
    just_read: Value,
    gc_heap: &mut otter_gc::GcHeap,
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<Value, ParseError> {
    let frame = stack.last_mut().expect("non-empty stack");
    match frame {
        Builder::Array {
            elements,
            has_member,
            ..
        } => {
            elements.push(just_read);
            *has_member = true;
            cursor.skip_ws();
            match cursor.peek() {
                Some(b',') => {
                    cursor.pos += 1;
                    cursor.skip_ws();
                    if matches!(cursor.peek(), Some(b']')) {
                        return Err(ParseError::at(cursor.pos, "trailing comma"));
                    }
                    read_step(cursor, stack, gc_heap, external_visit)
                }
                Some(b']') => {
                    cursor.pos += 1;
                    let frame = stack.pop().expect("just had it");
                    let bytes = cursor.bytes;
                    finish_builder(frame, stack, bytes, gc_heap, cursor.pos, external_visit)
                }
                Some(other) => Err(ParseError::at(
                    cursor.pos,
                    format!("expected ',' or ']', found 0x{other:02x}"),
                )),
                None => Err(ParseError::at(cursor.pos, "unterminated array")),
            }
        }
        Builder::Object {
            entries,
            pending_key,
            has_member,
        } => {
            let key = pending_key
                .take()
                .expect("pending key set before reading value");
            entries.push((key, just_read));
            *has_member = true;
            cursor.skip_ws();
            match cursor.peek() {
                Some(b',') => {
                    cursor.pos += 1;
                    cursor.skip_ws();
                    if matches!(cursor.peek(), Some(b'}')) {
                        return Err(ParseError::at(cursor.pos, "trailing comma"));
                    }
                    let key = read_object_key(cursor, gc_heap)?;
                    if let Some(Builder::Object { pending_key, .. }) = stack.last_mut() {
                        *pending_key = Some(key);
                    }
                    cursor.skip_ws();
                    cursor.expect(b':', "':' after object key")?;
                    cursor.skip_ws();
                    read_step(cursor, stack, gc_heap, external_visit)
                }
                Some(b'}') => {
                    cursor.pos += 1;
                    let frame = stack.pop().expect("just had it");
                    let bytes = cursor.bytes;
                    finish_builder(frame, stack, bytes, gc_heap, cursor.pos, external_visit)
                }
                Some(other) => Err(ParseError::at(
                    cursor.pos,
                    format!("expected ',' or '}}', found 0x{other:02x}"),
                )),
                None => Err(ParseError::at(cursor.pos, "unterminated object")),
            }
        }
    }
}

fn finish_builder(
    builder: Builder,
    stack: &[Builder],
    bytes: &[u8],
    gc_heap: &mut otter_gc::GcHeap,
    pos: usize,
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<Value, ParseError> {
    match builder {
        Builder::Array {
            elements,
            array_start,
            ..
        } => {
            // Capture the verbatim source slice spanning `[…]` so a
            // subsequent `JSON.stringify` of an unmodified parsed
            // array can re-emit it via memcpy. The slice is
            // freshly-cloned (not aliased into the input) so the
            // input buffer is free to drop independently.
            let source: Arc<[u8]> = Arc::from(&bytes[array_start..pos]);
            let mut roots = |visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
                external_visit(visitor);
                trace_builder_stack(stack, visitor);
                for value in &elements {
                    value.trace_value_slots(visitor);
                }
            };
            Ok(Value::Array(
                crate::array::from_elements_with_source_and_roots(
                    gc_heap,
                    elements.iter().cloned(),
                    source,
                    &mut roots,
                )
                .map_err(|_| ParseError::at(pos, "JSON.parse: out of memory"))?,
            ))
        }
        Builder::Object { entries, .. } => {
            let mut roots = |visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
                external_visit(visitor);
                trace_builder_stack(stack, visitor);
                for (_, value) in &entries {
                    value.trace_value_slots(visitor);
                }
            };
            let obj = crate::object::alloc_object_with_roots(gc_heap, &mut roots)
                .map_err(|_| ParseError::at(pos, "JSON.parse: out of memory"))?;
            for (k, v) in entries {
                crate::object::set(obj, gc_heap, &k, v);
            }
            Ok(Value::Object(obj))
        }
    }
}

fn trace_builder_stack(stack: &[Builder], visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)) {
    for builder in stack {
        match builder {
            Builder::Array { elements, .. } => {
                for value in elements {
                    value.trace_value_slots(visitor);
                }
            }
            Builder::Object { entries, .. } => {
                for (_, value) in entries {
                    value.trace_value_slots(visitor);
                }
            }
        }
    }
}

fn consume_keyword(cursor: &mut Cursor<'_>, keyword: &[u8]) -> Result<(), ParseError> {
    let start = cursor.pos;
    let end = start + keyword.len();
    if end > cursor.bytes.len() || &cursor.bytes[start..end] != keyword {
        return Err(ParseError::at(start, "invalid literal"));
    }
    cursor.pos = end;
    Ok(())
}

fn read_object_key(
    cursor: &mut Cursor<'_>,
    heap: &mut otter_gc::GcHeap,
) -> Result<String, ParseError> {
    if cursor.peek() != Some(b'"') {
        return Err(ParseError::at(
            cursor.pos,
            "expected '\"' starting object key",
        ));
    }
    let s = read_string(cursor, heap)?;
    Ok(s.to_lossy_string(heap))
}

/// Strict number parser. Accepts the JSON subset of JS numeric
/// syntax: optional `-`, integer part (no leading zero unless the
/// integer is a single zero), optional fraction, optional
/// exponent. NaN / Infinity / hex / bin / leading `+` are not
/// accepted. Overflow → `f64::INFINITY` (matches `parseFloat`).
fn read_number(cursor: &mut Cursor<'_>) -> Result<NumberValue, ParseError> {
    let start = cursor.pos;
    if cursor.peek() == Some(b'-') {
        cursor.pos += 1;
    }
    let int_start = cursor.pos;
    match cursor.peek() {
        Some(b'0') => {
            cursor.pos += 1;
            // Zero must not be followed by another digit.
            if let Some(b'0'..=b'9') = cursor.peek() {
                return Err(ParseError::at(cursor.pos, "leading zero is not allowed"));
            }
        }
        Some(b'1'..=b'9') => {
            cursor.pos += 1;
            while let Some(b'0'..=b'9') = cursor.peek() {
                cursor.pos += 1;
            }
        }
        _ => return Err(ParseError::at(cursor.pos, "expected digit")),
    }
    let _ = int_start;

    let mut has_fraction = false;
    if cursor.peek() == Some(b'.') {
        cursor.pos += 1;
        has_fraction = true;
        if !matches!(cursor.peek(), Some(b'0'..=b'9')) {
            return Err(ParseError::at(
                cursor.pos,
                "expected digit after decimal point",
            ));
        }
        while let Some(b'0'..=b'9') = cursor.peek() {
            cursor.pos += 1;
        }
    }
    let mut has_exponent = false;
    if matches!(cursor.peek(), Some(b'e' | b'E')) {
        cursor.pos += 1;
        has_exponent = true;
        if matches!(cursor.peek(), Some(b'+' | b'-')) {
            cursor.pos += 1;
        }
        if !matches!(cursor.peek(), Some(b'0'..=b'9')) {
            return Err(ParseError::at(cursor.pos, "expected digit after exponent"));
        }
        while let Some(b'0'..=b'9') = cursor.peek() {
            cursor.pos += 1;
        }
    }

    let text = std::str::from_utf8(&cursor.bytes[start..cursor.pos])
        .map_err(|_| ParseError::at(start, "invalid utf-8 in number"))?;

    // Smi fast path: integer-only literal that fits i32.
    if !has_fraction
        && !has_exponent
        && let Ok(n) = text.parse::<i32>()
    {
        return Ok(NumberValue::from_i32(n));
    }
    let f: f64 = text
        .parse()
        .map_err(|_| ParseError::at(start, "invalid number"))?;
    Ok(NumberValue::from_f64(f))
}

/// Read a JSON string. Hot path for ASCII-only payloads bulk-skips
/// clean spans via [`super::scan::find_first_escape`] (8 bytes per
/// iteration) and only inspects bytes that are `"`, `\\`, or
/// control characters. Unicode escapes (`\uXXXX`) and surrogate
/// pairs fall back to a WTF-16 builder.
///
/// Spec: <https://tc39.es/ecma262/#sec-json.parse> §25.5.1
fn read_string(
    cursor: &mut Cursor<'_>,
    heap: &mut otter_gc::GcHeap,
) -> Result<JsString, ParseError> {
    debug_assert_eq!(cursor.peek(), Some(b'"'));
    cursor.pos += 1;
    let start = cursor.pos;
    // SWAR scan over the input until we hit a special byte or end.
    let next = super::scan::find_first_escape_pub(cursor.bytes, cursor.pos);
    cursor.pos = next;
    if cursor.pos >= cursor.bytes.len() {
        return Err(ParseError::at(cursor.pos, "unterminated string"));
    }
    let b = cursor.bytes[cursor.pos];
    if b == b'"' {
        let slice = &cursor.bytes[start..cursor.pos];
        cursor.pos += 1;
        let text = std::str::from_utf8(slice)
            .map_err(|_| ParseError::at(start, "invalid utf-8 in string"))?;
        return JsString::from_str(text, heap)
            .map_err(|_| ParseError::at(start, "out of memory while interning string"));
    }
    if b < 0x20 {
        return Err(ParseError::at(cursor.pos, "control character in string"));
    }
    debug_assert_eq!(b, b'\\');
    // Slow path: fall back to a WTF-16 builder seeded with the
    // already-consumed prefix.
    read_string_with_escapes(cursor, start, heap)
}

fn read_string_with_escapes(
    cursor: &mut Cursor<'_>,
    plain_start: usize,
    heap: &mut otter_gc::GcHeap,
) -> Result<JsString, ParseError> {
    // Lift the plain prefix into a UTF-16 buffer. Bytes between
    // `plain_start` and `cursor.pos` are guaranteed ASCII (we'd
    // have errored on control chars earlier), so the cast is
    // lossless.
    let mut buf: Vec<u16> = cursor.bytes[plain_start..cursor.pos]
        .iter()
        .map(|&b| u16::from(b))
        .collect();

    while let Some(b) = cursor.peek() {
        match b {
            b'"' => {
                cursor.pos += 1;
                return JsString::from_utf16_units(&buf, heap).map_err(|_| {
                    ParseError::at(plain_start, "out of memory while interning string")
                });
            }
            b'\\' => {
                cursor.pos += 1;
                let esc = cursor
                    .peek()
                    .ok_or_else(|| ParseError::at(cursor.pos, "unterminated escape"))?;
                cursor.pos += 1;
                match esc {
                    b'"' => buf.push(b'"' as u16),
                    b'\\' => buf.push(b'\\' as u16),
                    b'/' => buf.push(b'/' as u16),
                    b'b' => buf.push(0x08),
                    b'f' => buf.push(0x0C),
                    b'n' => buf.push(b'\n' as u16),
                    b'r' => buf.push(b'\r' as u16),
                    b't' => buf.push(b'\t' as u16),
                    b'u' => {
                        let unit = read_hex_escape(cursor)?;
                        buf.push(unit);
                    }
                    other => {
                        return Err(ParseError::at(
                            cursor.pos - 1,
                            format!("invalid escape '\\{}'", other as char),
                        ));
                    }
                }
            }
            0x00..=0x1F => {
                return Err(ParseError::at(cursor.pos, "control character in string"));
            }
            _ => {
                let ch_start = cursor.pos;
                // Decode one UTF-8 codepoint, push as 1–2 UTF-16 units.
                let bytes = &cursor.bytes[ch_start..];
                let len = utf8_char_len(bytes[0]);
                if len == 0 || bytes.len() < len {
                    return Err(ParseError::at(ch_start, "invalid utf-8 sequence"));
                }
                let slice = &bytes[..len];
                let s = std::str::from_utf8(slice)
                    .map_err(|_| ParseError::at(ch_start, "invalid utf-8 sequence"))?;
                for unit in s.encode_utf16() {
                    buf.push(unit);
                }
                cursor.pos += len;
            }
        }
    }
    Err(ParseError::at(cursor.pos, "unterminated string"))
}

fn read_hex_escape(cursor: &mut Cursor<'_>) -> Result<u16, ParseError> {
    let start = cursor.pos;
    if cursor.bytes.len() < start + 4 {
        return Err(ParseError::at(start, "incomplete \\u escape"));
    }
    let mut value: u16 = 0;
    for i in 0..4 {
        let b = cursor.bytes[start + i];
        let nibble = match b {
            b'0'..=b'9' => b - b'0',
            b'a'..=b'f' => b - b'a' + 10,
            b'A'..=b'F' => b - b'A' + 10,
            _ => {
                return Err(ParseError::at(start + i, "invalid hex digit in \\u escape"));
            }
        };
        value = (value << 4) | u16::from(nibble);
    }
    cursor.pos += 4;
    Ok(value)
}

fn utf8_char_len(first: u8) -> usize {
    if first < 0x80 {
        1
    } else if first < 0xC0 {
        0 // continuation byte → invalid lead
    } else if first < 0xE0 {
        2
    } else if first < 0xF0 {
        3
    } else if first < 0xF8 {
        4
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_str(s: &str) -> Result<Value, ParseError> {
        let mut gc_heap = otter_gc::GcHeap::new().expect("gc heap");
        parse(s, &mut gc_heap)
    }

    fn parse_str_with_heap(s: &str, gc_heap: &mut otter_gc::GcHeap) -> Result<Value, ParseError> {
        parse(s, gc_heap)
    }

    #[test]
    fn parses_primitives() {
        let mut gc_heap = otter_gc::GcHeap::new().expect("gc heap");
        assert!(matches!(
            parse_str_with_heap("null", &mut gc_heap).unwrap(),
            Value::Null
        ));
        assert!(matches!(
            parse_str_with_heap("true", &mut gc_heap).unwrap(),
            Value::Boolean(true)
        ));
        assert!(matches!(
            parse_str_with_heap("false", &mut gc_heap).unwrap(),
            Value::Boolean(false)
        ));
        assert_eq!(
            parse_str_with_heap("42", &mut gc_heap)
                .unwrap()
                .display_string(&gc_heap),
            "42"
        );
        assert_eq!(
            parse_str_with_heap("-3.14", &mut gc_heap)
                .unwrap()
                .display_string(&gc_heap),
            "-3.14"
        );
        assert_eq!(
            parse_str_with_heap("1e2", &mut gc_heap)
                .unwrap()
                .display_string(&gc_heap),
            "100"
        );
    }

    #[test]
    fn parses_strings_with_escapes() {
        let mut gc_heap = otter_gc::GcHeap::new().expect("gc heap");
        let v = parse_str_with_heap("\"a\\nb\\\\c\\\"d\"", &mut gc_heap).unwrap();
        assert_eq!(v.display_string(&gc_heap), "a\nb\\c\"d");
    }

    #[test]
    fn parses_unicode_escape_and_surrogate_pair() {
        let mut gc_heap = otter_gc::GcHeap::new().expect("gc heap");
        // U+0041 'A' encoded as A.
        let v = parse_str_with_heap("\"\\u0041\"", &mut gc_heap).unwrap();
        assert_eq!(v.display_string(&gc_heap), "A");
        // Surrogate pair → U+10000 '𐀀'.
        let v = parse_str_with_heap("\"\\uD800\\uDC00\"", &mut gc_heap).unwrap();
        match v {
            Value::String(s) => assert_eq!(s.to_utf16_vec(&gc_heap), vec![0xD800, 0xDC00]),
            _ => panic!(),
        }
    }

    #[test]
    fn parses_array_and_object() {
        let mut gc_heap = otter_gc::GcHeap::new().expect("gc heap");
        let v = parse_str_with_heap("{\"x\":[1,2,3]}", &mut gc_heap).unwrap();
        let Value::Object(obj) = v else { panic!() };
        let Some(Value::Array(arr)) = crate::object::get(obj, &gc_heap, "x") else {
            panic!()
        };
        assert_eq!(crate::array::len(arr, &gc_heap), 3);
        assert_eq!(
            crate::array::get(arr, &gc_heap, 2).display_string(&gc_heap),
            "3"
        );
    }

    #[test]
    fn rejects_trailing_comma() {
        assert!(parse_str("[1,2,]").is_err());
        assert!(parse_str("{\"a\":1,}").is_err());
    }

    #[test]
    fn rejects_leading_zero_and_plus() {
        assert!(parse_str("01").is_err());
        assert!(parse_str("+1").is_err());
    }

    #[test]
    fn rejects_nan_and_undefined() {
        assert!(parse_str("NaN").is_err());
        assert!(parse_str("undefined").is_err());
        assert!(parse_str("Infinity").is_err());
    }

    #[test]
    fn rejects_unterminated() {
        assert!(parse_str("[1, 2").is_err());
        assert!(parse_str("\"abc").is_err());
    }

    #[test]
    fn nested_object_round_trip() {
        let mut gc_heap = otter_gc::GcHeap::new().expect("gc heap");
        let v = parse_str_with_heap("{\"a\":{\"b\":{\"c\":42}}}", &mut gc_heap).unwrap();
        let Value::Object(o) = v else { panic!() };
        let Some(Value::Object(o2)) = crate::object::get(o, &gc_heap, "a") else {
            panic!()
        };
        let Some(Value::Object(o3)) = crate::object::get(o2, &gc_heap, "b") else {
            panic!()
        };
        assert_eq!(
            crate::object::get(o3, &gc_heap, "c")
                .unwrap()
                .display_string(&gc_heap),
            "42"
        );
    }
}
