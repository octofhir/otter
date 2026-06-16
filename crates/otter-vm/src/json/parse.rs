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
/// Install `object_proto` as the `[[Prototype]]` of every ordinary
/// object reachable from a freshly parsed value.
///
/// The hot parser allocates objects with a null prototype because it
/// runs without a realm handle; `JSON.parse` must instead expose
/// results whose prototype is the realm `%Object.prototype%` (so
/// inherited methods like `hasOwnProperty` resolve, and an own
/// `"__proto__"` key stays an ordinary data property — §25.5.1).
/// Parsed arrays already carry `%Array.prototype%`. The parse result
/// is an acyclic tree, so an explicit work-stack suffices.
pub(crate) fn install_object_prototype(
    root: Value,
    object_proto: Value,
    gc_heap: &mut otter_gc::GcHeap,
) {
    let mut stack = vec![root];
    while let Some(value) = stack.pop() {
        if let Some(obj) = value.as_object() {
            crate::object::set_prototype_value(obj, gc_heap, Some(object_proto));
            let children: Vec<Value> = crate::object::with_properties(obj, gc_heap, |p| {
                p.enumerable_data_iter().map(|(_, v)| v).collect()
            });
            stack.extend(
                children
                    .into_iter()
                    .filter(|v| v.is_object() || v.as_array().is_some()),
            );
        } else if let Some(arr) = value.as_array() {
            let len = crate::array::len(arr, gc_heap);
            for idx in 0..len {
                let elem = crate::array::get(arr, gc_heap, idx);
                if elem.is_object() || elem.as_array().is_some() {
                    stack.push(elem);
                }
            }
        }
    }
}

/// Source-span mirror of a parsed JSON value, used to feed the
/// reviver `context.source` argument (ECMA-262 §25.5.1, the
/// `json-parse-with-source` proposal). Only primitive leaves carry a
/// source string; containers exist purely for navigation.
#[derive(Debug, Clone)]
pub(crate) enum SourceNode {
    /// Verbatim, whitespace-trimmed source text of a primitive token.
    Primitive(String),
    Array(Vec<SourceNode>),
    Object(Vec<(String, SourceNode)>),
}

impl SourceNode {
    /// The `source` text for this node, present only for primitives.
    pub(crate) fn source(&self) -> Option<&str> {
        match self {
            SourceNode::Primitive(s) => Some(s),
            _ => None,
        }
    }

    /// Child node for array index `idx`, if this is an array node.
    pub(crate) fn array_child(&self, idx: usize) -> Option<&SourceNode> {
        match self {
            SourceNode::Array(items) => items.get(idx),
            _ => None,
        }
    }

    /// Child node for object key `key` (last occurrence wins, mirroring
    /// `JSON.parse`'s last-key-wins materialisation).
    pub(crate) fn object_child(&self, key: &str) -> Option<&SourceNode> {
        match self {
            SourceNode::Object(pairs) => pairs.iter().rev().find(|(k, _)| k == key).map(|(_, n)| n),
            _ => None,
        }
    }
}

/// Build the [`SourceNode`] tree for already-validated JSON `text`.
/// Returns `None` only if the text is unexpectedly malformed or
/// nesting exceeds [`MAX_NESTING_DEPTH`]; callers fall back to a
/// source-less reviver walk in that case.
pub(crate) fn parse_source_tree(text: &str) -> Option<SourceNode> {
    let mut sc = SourceScanner {
        bytes: text.as_bytes(),
        pos: 0,
    };
    sc.skip_ws();
    let node = sc.scan_value(0)?;
    sc.skip_ws();
    if sc.pos == sc.bytes.len() {
        Some(node)
    } else {
        None
    }
}

struct SourceScanner<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl SourceScanner<'_> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\n' | b'\r' | b'\t')) {
            self.pos += 1;
        }
    }

    fn scan_value(&mut self, depth: usize) -> Option<SourceNode> {
        if depth >= MAX_NESTING_DEPTH {
            return None;
        }
        self.skip_ws();
        match self.peek()? {
            b'{' => self.scan_object(depth),
            b'[' => self.scan_array(depth),
            b'"' => {
                let start = self.pos;
                self.skip_string()?;
                let span = std::str::from_utf8(&self.bytes[start..self.pos]).ok()?;
                Some(SourceNode::Primitive(span.to_string()))
            }
            _ => {
                let start = self.pos;
                // Number / true / false / null — read until a
                // structural / whitespace delimiter.
                while let Some(b) = self.peek() {
                    if matches!(b, b',' | b']' | b'}' | b' ' | b'\n' | b'\r' | b'\t') {
                        break;
                    }
                    self.pos += 1;
                }
                if self.pos == start {
                    return None;
                }
                let span = std::str::from_utf8(&self.bytes[start..self.pos]).ok()?;
                Some(SourceNode::Primitive(span.to_string()))
            }
        }
    }

    fn scan_array(&mut self, depth: usize) -> Option<SourceNode> {
        self.pos += 1; // consume '['
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Some(SourceNode::Array(items));
        }
        loop {
            let node = self.scan_value(depth + 1)?;
            items.push(node);
            self.skip_ws();
            match self.peek()? {
                b',' => {
                    self.pos += 1;
                    self.skip_ws();
                }
                b']' => {
                    self.pos += 1;
                    return Some(SourceNode::Array(items));
                }
                _ => return None,
            }
        }
    }

    fn scan_object(&mut self, depth: usize) -> Option<SourceNode> {
        self.pos += 1; // consume '{'
        let mut pairs = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Some(SourceNode::Object(pairs));
        }
        loop {
            self.skip_ws();
            if self.peek()? != b'"' {
                return None;
            }
            let key = self.scan_key()?;
            self.skip_ws();
            if self.peek()? != b':' {
                return None;
            }
            self.pos += 1; // consume ':'
            let node = self.scan_value(depth + 1)?;
            pairs.push((key, node));
            self.skip_ws();
            match self.peek()? {
                b',' => self.pos += 1,
                b'}' => {
                    self.pos += 1;
                    return Some(SourceNode::Object(pairs));
                }
                _ => return None,
            }
        }
    }

    /// Read and decode an object key string (positioned on the opening
    /// quote) for use as a property-name lookup key.
    fn scan_key(&mut self) -> Option<String> {
        let start = self.pos;
        self.skip_string()?;
        let raw = std::str::from_utf8(&self.bytes[start + 1..self.pos - 1]).ok()?;
        Some(decode_json_string(raw))
    }

    /// Advance past a string literal (positioned on the opening quote),
    /// honouring backslash escapes. Leaves `pos` just after the
    /// closing quote.
    fn skip_string(&mut self) -> Option<()> {
        self.pos += 1; // opening quote
        while let Some(b) = self.peek() {
            self.pos += 1;
            match b {
                b'"' => return Some(()),
                b'\\' => {
                    // Skip the escaped unit; `\uXXXX` advances 4 more.
                    match self.peek()? {
                        b'u' => self.pos += 5,
                        _ => self.pos += 1,
                    }
                }
                _ => {}
            }
        }
        None
    }
}

/// Minimal JSON string unescape for object-key matching. Covers the
/// standard short escapes plus BMP `\uXXXX`; surrogate pairs combine.
fn decode_json_string(raw: &str) -> String {
    if !raw.contains('\\') {
        return raw.to_string();
    }
    let units: Vec<u16> = raw.encode_utf16().collect();
    let mut out: Vec<u16> = Vec::with_capacity(units.len());
    let mut i = 0;
    while i < units.len() {
        let c = units[i];
        if c != b'\\' as u16 {
            out.push(c);
            i += 1;
            continue;
        }
        i += 1;
        let Some(&esc) = units.get(i) else { break };
        match esc {
            0x75 => {
                // \uXXXX
                let hex: String = units[i + 1..(i + 5).min(units.len())]
                    .iter()
                    .filter_map(|&u| char::from_u32(u as u32))
                    .collect();
                if let Ok(cp) = u16::from_str_radix(&hex, 16) {
                    out.push(cp);
                    i += 5;
                } else {
                    out.push(esc);
                    i += 1;
                }
            }
            0x62 => {
                out.push(0x08);
                i += 1;
            } // \b
            0x66 => {
                out.push(0x0C);
                i += 1;
            } // \f
            0x6E => {
                out.push(0x0A);
                i += 1;
            } // \n
            0x72 => {
                out.push(0x0D);
                i += 1;
            } // \r
            0x74 => {
                out.push(0x09);
                i += 1;
            } // \t
            _ => {
                out.push(esc);
                i += 1;
            }
        }
    }
    String::from_utf16_lossy(&out)
}

/// Strict `JSON.parse` over `text`, with no caller-owned GC roots.
/// Objects come back with a null prototype; the native entry point
/// applies [`install_object_prototype`] afterwards.
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
        b'"' => Ok(Value::string(read_string(cursor, gc_heap)?)),
        b't' => {
            consume_keyword(cursor, b"true")?;
            Ok(Value::boolean(true))
        }
        b'f' => {
            consume_keyword(cursor, b"false")?;
            Ok(Value::boolean(false))
        }
        b'n' => {
            consume_keyword(cursor, b"null")?;
            Ok(Value::null())
        }
        b'-' | b'0'..=b'9' => Ok(Value::number(read_number(cursor)?)),
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
            Ok(Value::array(
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
            Ok(Value::object(obj))
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
    // Fast path — an escape-free key is the overwhelming common case
    // (`"id"`, `"name"`, …). Build the Rust `String` straight from the
    // input slice, skipping the GC `JsString` allocation that the general
    // `read_string` path would mint only to immediately stringify and drop.
    let key_start = cursor.pos + 1;
    let next = super::scan::find_first_escape_pub(cursor.bytes, key_start);
    if next < cursor.bytes.len() && cursor.bytes[next] == b'"' {
        let slice = &cursor.bytes[key_start..next];
        let text = std::str::from_utf8(slice)
            .map_err(|_| ParseError::at(key_start, "invalid utf-8 in object key"))?;
        cursor.pos = next + 1;
        return Ok(text.to_string());
    }
    // Escape / control / unterminated: defer to the full string reader,
    // which decodes escapes (and reports the precise error) identically.
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

    // Smi fast path: integer-only literal that fits i32. `-0` is
    // excluded so it round-trips as IEEE negative zero rather than
    // collapsing to the `+0` Smi.
    if !has_fraction
        && !has_exponent
        && text != "-0"
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
        assert!(parse_str_with_heap("null", &mut gc_heap).unwrap().is_null());
        assert_eq!(
            parse_str_with_heap("true", &mut gc_heap)
                .unwrap()
                .as_boolean(),
            Some(true)
        );
        assert_eq!(
            parse_str_with_heap("false", &mut gc_heap)
                .unwrap()
                .as_boolean(),
            Some(false)
        );
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
        let s = v.as_string(&gc_heap).expect("string");
        assert_eq!(s.to_utf16_vec(&gc_heap), vec![0xD800, 0xDC00]);
    }

    #[test]
    fn parses_array_and_object() {
        let mut gc_heap = otter_gc::GcHeap::new().expect("gc heap");
        let v = parse_str_with_heap("{\"x\":[1,2,3]}", &mut gc_heap).unwrap();
        let obj = v.as_object().expect("object");
        let arr = crate::object::get(obj, &gc_heap, "x")
            .and_then(|v| v.as_array())
            .expect("array");
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
        let o = v.as_object().expect("object");
        let o2 = crate::object::get(o, &gc_heap, "a")
            .and_then(|v| v.as_object())
            .expect("a");
        let o3 = crate::object::get(o2, &gc_heap, "b")
            .and_then(|v| v.as_object())
            .expect("b");
        assert_eq!(
            crate::object::get(o3, &gc_heap, "c")
                .unwrap()
                .display_string(&gc_heap),
            "42"
        );
    }
}
