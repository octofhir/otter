//! `JSON` namespace object (ES2024 §25.5)
//!
//! The `JSON` object provides `parse` and `stringify` methods for working with JSON.
//! It is not a constructor and has no `[[Call]]` or `[[Construct]]`.
//!
//! Spec: <https://tc39.es/ecma262/#sec-json-object>
//! MDN: <https://developer.mozilla.org/en-US/docs/Web/JavaScript/Reference/Global_Objects/JSON>

use crate::builtin_builder::{IntrinsicContext, IntrinsicObject, NamespaceBuilder};
use crate::context::NativeContext;
use crate::error::VmError;
use crate::gc::GcRef;
use crate::memory::MemoryManager;
use crate::object::{JsObject, PropertyAttributes, PropertyDescriptor, PropertyKey};
use crate::string::JsString;
use crate::value::Value;
use otter_macros::dive;
use rustc_hash::{FxHashMap, FxHashSet};
use smallvec::SmallVec;
use std::borrow::Cow;
use std::sync::Arc;

// Index by byte value. 0 = safe (no escape needed).
// For named escapes (\n, \t, etc.), stores the second character of the escape.
// For generic \uXXXX, stores 1.
static ESCAPE_TABLE: [u8; 256] = {
    let mut t = [0u8; 256];
    // Control characters 0x00-0x1F need escaping
    let mut i = 0;
    while i < 0x20 {
        t[i] = 1; // generic \uXXXX
        i += 1;
    }
    // Named escapes override generic
    t[0x08] = b'b'; // \b
    t[0x09] = b't'; // \t
    t[0x0A] = b'n'; // \n
    t[0x0C] = b'f'; // \f
    t[0x0D] = b'r'; // \r
    t[0x22] = b'"'; // \"
    t[0x5C] = b'\\'; // \\
    t
};

/// SWAR: check if any byte in an 8-byte word needs JSON escaping.
/// Returns true if any byte is < 0x20, == 0x22 ("), or == 0x5C (\).
#[inline(always)]
fn has_escape_char_in_word(word: u64) -> bool {
    // Detect bytes < 0x20 (control chars)
    // A byte < 0x20 means bit pattern 000xxxxx. We check by subtracting 0x20
    // from each byte and looking for underflow (high bit set in result but not in original).
    const SUB: u64 = 0x2020_2020_2020_2020;
    const HI: u64 = 0x8080_8080_8080_8080;
    const LO: u64 = 0x0101_0101_0101_0101;

    let ctrl = (word.wrapping_sub(SUB)) & (!word) & HI;

    // Detect bytes == 0x22 (") via zero-byte detection on XOR
    let xor_quote = word ^ 0x2222_2222_2222_2222;
    let quote = (xor_quote.wrapping_sub(LO)) & (!xor_quote) & HI;

    // Detect bytes == 0x5C (\) via zero-byte detection on XOR
    let xor_bslash = word ^ 0x5C5C_5C5C_5C5C_5C5C;
    let bslash = (xor_bslash.wrapping_sub(LO)) & (!xor_bslash) & HI;

    (ctrl | quote | bslash) != 0
}

/// Native JSON hot-loop interrupt check cadence (power-of-two for bitmask checks).
const JSON_INTERRUPT_CHECK_INTERVAL: usize = 1024;

#[inline]
fn maybe_check_interrupt(ncx: &mut NativeContext<'_>, index: usize) -> Result<(), VmError> {
    if (index & (JSON_INTERRUPT_CHECK_INTERVAL - 1)) == 0 {
        ncx.check_for_interrupt()?;
    }
    Ok(())
}

/// Tracks visited objects during JSON serialization to detect circular references.
/// Uses a simple depth + pointer set for the fast path, and only builds
/// path strings lazily on actual circular reference errors.
struct CircularTracker {
    /// Maps object pointer to depth index for cycle detection
    visited: FxHashMap<usize, usize>,
    /// Depth counter (avoids storing full path for non-error case)
    depth: usize,
    /// Path info stored only as (ptr, is_array) — key string allocated lazily on error
    path_ptrs: Vec<(usize, bool)>,
}

impl CircularTracker {
    fn new() -> Self {
        Self {
            visited: FxHashMap::default(),
            depth: 0,
            path_ptrs: Vec::new(),
        }
    }

    /// Try to enter an object. Returns Err with formatted message if circular.
    fn enter(&mut self, key: &str, ptr: usize, is_array: bool) -> Result<(), String> {
        if let Some(&_cycle_start) = self.visited.get(&ptr) {
            // Only build error path string on actual circular reference
            return Err(format!(
                "Converting circular structure to JSON\n    --> starting at object at depth {}",
                self.depth
            ));
        }
        let idx = self.depth;
        self.visited.insert(ptr, idx);
        self.path_ptrs.push((ptr, is_array));
        self.depth += 1;
        // Suppress unused variable warning for key
        let _ = key;
        Ok(())
    }

    /// Exit an object (after serialization)
    fn exit(&mut self, ptr: usize) {
        self.visited.remove(&ptr);
        self.path_ptrs.pop();
        self.depth -= 1;
    }
}

// ─── V8-style direct JSON parser ────────────────────────────────────────────
// No serde_json. Direct byte scanning → Value creation.
// Property buffering with shape template reuse for arrays of same-shaped objects.

/// Result of scanning a JSON string: either a zero-copy borrow from input,
/// or an owned String when escape sequences were present.
enum JsonStr<'a> {
    Borrowed(&'a str),
    Owned(String),
}

impl<'a> JsonStr<'a> {
    #[inline]
    fn as_str(&self) -> &str {
        match self {
            JsonStr::Borrowed(s) => s,
            JsonStr::Owned(s) => s.as_str(),
        }
    }
}

/// Cached shape template for arrays of same-shaped objects.
struct ShapeTemplate {
    keys: Vec<GcRef<JsString>>,
    shape: Arc<crate::shape::Shape>,
}

/// V8-style JSON parser. Scans input bytes directly, creates Values inline.
struct JsonParser<'a, 'ctx> {
    input: &'a [u8],
    pos: usize,
    object_proto: Value,
    array_proto: Value,
    ncx: &'a mut NativeContext<'ctx>,
    key_cache: FxHashMap<&'a str, GcRef<JsString>>,
    val_cache: FxHashMap<&'a str, GcRef<JsString>>,
    node_count: usize,
    /// Shape template for current array scope
    array_shape_template: Option<ShapeTemplate>,
    /// Global shape cache: maps key fingerprint to (keys, shape) for reuse
    /// across ALL objects with the same property set (not just array siblings).
    shape_cache: FxHashMap<u64, (Vec<GcRef<JsString>>, Arc<crate::shape::Shape>)>,
    /// Temp buffer for building escaped strings (reused to avoid alloc)
    str_buf: String,
}

impl<'a, 'ctx> JsonParser<'a, 'ctx> {
    fn new(
        input: &'a str,
        object_proto: Value,
        array_proto: Value,
        ncx: &'a mut NativeContext<'ctx>,
    ) -> Self {
        Self {
            input: input.as_bytes(),
            pos: 0,
            object_proto,
            array_proto,
            ncx,
            key_cache: FxHashMap::default(),
            val_cache: FxHashMap::default(),
            node_count: 0,
            array_shape_template: None,
            shape_cache: FxHashMap::default(),
            str_buf: String::with_capacity(128),
        }
    }

    #[inline(always)]
    fn peek(&self) -> Option<u8> {
        self.input.get(self.pos).copied()
    }

    #[inline(always)]
    fn advance(&mut self) {
        self.pos += 1;
    }

    #[inline(always)]
    fn skip_whitespace(&mut self) {
        while self.pos < self.input.len() {
            match self.input[self.pos] {
                b' ' | b'\t' | b'\n' | b'\r' => self.pos += 1,
                _ => break,
            }
        }
    }

    #[inline]
    fn check_interrupt(&mut self) -> Result<(), VmError> {
        self.node_count += 1;
        if (self.node_count & (JSON_INTERRUPT_CHECK_INTERVAL - 1)) == 0 {
            self.ncx.check_for_interrupt()?;
        }
        Ok(())
    }

    fn error(&self, msg: &str) -> VmError {
        VmError::syntax_error(format!("JSON.parse: {} at position {}", msg, self.pos))
    }

    fn expect(&mut self, b: u8) -> Result<(), VmError> {
        if self.peek() == Some(b) {
            self.advance();
            Ok(())
        } else {
            Err(self.error(&format!("expected '{}'", b as char)))
        }
    }

    /// Main entry: parse one JSON value.
    fn parse_value(&mut self) -> Result<Value, VmError> {
        self.check_interrupt()?;
        self.skip_whitespace();
        match self.peek() {
            Some(b'"') => self.parse_string_value(),
            Some(b'{') => self.parse_object(),
            Some(b'[') => self.parse_array(),
            Some(b't') => self.parse_true(),
            Some(b'f') => self.parse_false(),
            Some(b'n') => self.parse_null(),
            Some(b'-') | Some(b'0'..=b'9') => self.parse_number(),
            Some(c) => Err(self.error(&format!("unexpected character '{}'", c as char))),
            None => Err(self.error("unexpected end of input")),
        }
    }

    fn parse_true(&mut self) -> Result<Value, VmError> {
        if self.input[self.pos..].starts_with(b"true") {
            self.pos += 4;
            Ok(Value::boolean(true))
        } else {
            Err(self.error("expected 'true'"))
        }
    }

    fn parse_false(&mut self) -> Result<Value, VmError> {
        if self.input[self.pos..].starts_with(b"false") {
            self.pos += 5;
            Ok(Value::boolean(false))
        } else {
            Err(self.error("expected 'false'"))
        }
    }

    fn parse_null(&mut self) -> Result<Value, VmError> {
        if self.input[self.pos..].starts_with(b"null") {
            self.pos += 4;
            Ok(Value::null())
        } else {
            Err(self.error("expected 'null'"))
        }
    }

    /// Parse a JSON number. Uses integer fast path for i32, else f64.
    fn parse_number(&mut self) -> Result<Value, VmError> {
        let start = self.pos;
        let negative = self.peek() == Some(b'-');
        if negative {
            self.advance();
        }

        // Leading digit(s)
        if self.peek() == Some(b'0') {
            self.advance();
        } else if matches!(self.peek(), Some(b'1'..=b'9')) {
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.advance();
            }
        } else {
            return Err(self.error("invalid number"));
        }

        let mut is_float = false;
        // Fraction
        if self.peek() == Some(b'.') {
            is_float = true;
            self.advance();
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(self.error("invalid number: no digits after decimal point"));
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.advance();
            }
        }

        // Exponent
        if matches!(self.peek(), Some(b'e') | Some(b'E')) {
            is_float = true;
            self.advance();
            if matches!(self.peek(), Some(b'+') | Some(b'-')) {
                self.advance();
            }
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(self.error("invalid number: no digits in exponent"));
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.advance();
            }
        }

        // SAFETY: JSON numbers are always valid ASCII
        let num_str = unsafe { std::str::from_utf8_unchecked(&self.input[start..self.pos]) };

        // Integer fast path
        if !is_float {
            if let Ok(i) = num_str.parse::<i32>() {
                return Ok(Value::int32(i));
            }
            if let Ok(i) = num_str.parse::<i64>() {
                return Ok(Value::number(i as f64));
            }
        }

        // Float path (Eisel-Lemire via Rust stdlib)
        match num_str.parse::<f64>() {
            Ok(n) => Ok(Value::number(n)),
            Err(_) => Err(self.error("invalid number")),
        }
    }

    /// Scan a JSON string. Returns a borrowed &str slice from input when no escapes
    /// (zero-copy), or an owned String for escape sequences.
    fn scan_string_raw(&mut self) -> Result<JsonStr<'a>, VmError> {
        // Skip opening quote
        self.advance();
        let start = self.pos;

        // Fast scan: find closing quote, detect escapes
        let mut has_escape = false;
        loop {
            if self.pos >= self.input.len() {
                return Err(self.error("unterminated string"));
            }
            let b = self.input[self.pos];
            if b == b'"' {
                if !has_escape {
                    // Zero-copy: return slice from input
                    let s = unsafe { std::str::from_utf8_unchecked(&self.input[start..self.pos]) };
                    self.pos += 1; // skip closing quote
                    return Ok(JsonStr::Borrowed(s));
                }
                break;
            }
            if b == b'\\' {
                has_escape = true;
                self.pos += 1; // skip backslash
                if self.pos >= self.input.len() {
                    return Err(self.error("unterminated string escape"));
                }
                // Skip the escaped character
                if self.input[self.pos] == b'u' {
                    self.pos += 4; // skip 4 hex digits
                }
                self.pos += 1;
                continue;
            }
            if b < 0x20 {
                return Err(self.error("invalid control character in string"));
            }
            self.pos += 1;
        }

        // Has escapes — re-scan from start and build the unescaped string
        self.pos = start;
        self.str_buf.clear();
        loop {
            if self.pos >= self.input.len() {
                return Err(self.error("unterminated string"));
            }
            let b = self.input[self.pos];
            if b == b'"' {
                self.pos += 1;
                return Ok(JsonStr::Owned(std::mem::take(&mut self.str_buf)));
            }
            if b == b'\\' {
                self.pos += 1;
                let esc = self
                    .input
                    .get(self.pos)
                    .copied()
                    .ok_or_else(|| self.error("unterminated escape"))?;
                match esc {
                    b'"' => self.str_buf.push('"'),
                    b'\\' => self.str_buf.push('\\'),
                    b'/' => self.str_buf.push('/'),
                    b'b' => self.str_buf.push('\u{0008}'),
                    b'f' => self.str_buf.push('\u{000C}'),
                    b'n' => self.str_buf.push('\n'),
                    b'r' => self.str_buf.push('\r'),
                    b't' => self.str_buf.push('\t'),
                    b'u' => {
                        self.pos += 1;
                        let cp = self.parse_hex4()?;
                        // Handle surrogate pairs
                        if (0xD800..=0xDBFF).contains(&cp) {
                            if self.input.get(self.pos) == Some(&b'\\')
                                && self.input.get(self.pos + 1) == Some(&b'u')
                            {
                                self.pos += 2;
                                let low = self.parse_hex4()?;
                                if (0xDC00..=0xDFFF).contains(&low) {
                                    let full = 0x10000
                                        + ((cp as u32 - 0xD800) << 10)
                                        + (low as u32 - 0xDC00);
                                    if let Some(ch) = char::from_u32(full) {
                                        self.str_buf.push(ch);
                                    }
                                } else {
                                    // Lone high surrogate + non-surrogate
                                    if let Some(ch) = char::from_u32(cp as u32) {
                                        self.str_buf.push(ch);
                                    }
                                    if let Some(ch) = char::from_u32(low as u32) {
                                        self.str_buf.push(ch);
                                    }
                                }
                            } else {
                                // Lone high surrogate — use replacement char
                                self.str_buf.push('\u{FFFD}');
                            }
                        } else if (0xDC00..=0xDFFF).contains(&cp) {
                            // Lone low surrogate
                            self.str_buf.push('\u{FFFD}');
                        } else if let Some(ch) = char::from_u32(cp as u32) {
                            self.str_buf.push(ch);
                        }
                        continue; // don't advance again
                    }
                    _ => return Err(self.error("invalid escape character")),
                }
                self.pos += 1;
                continue;
            }
            // Regular UTF-8 byte — copy as-is
            // Multi-byte UTF-8 sequences: count continuation bytes
            let char_len = if b < 0x80 {
                1
            } else if b < 0xE0 {
                2
            } else if b < 0xF0 {
                3
            } else {
                4
            };
            let end = (self.pos + char_len).min(self.input.len());
            let slice = unsafe { std::str::from_utf8_unchecked(&self.input[self.pos..end]) };
            self.str_buf.push_str(slice);
            self.pos = end;
        }
    }

    /// Reclaim the str_buf capacity after an Owned result has been taken.
    /// Call this after consuming an Owned JsonStr to reuse the buffer.
    fn reclaim_buf(&mut self, mut s: String) {
        s.clear();
        if s.capacity() >= self.str_buf.capacity() {
            self.str_buf = s;
        }
    }

    fn parse_hex4(&mut self) -> Result<u16, VmError> {
        if self.pos + 4 > self.input.len() {
            return Err(self.error("invalid unicode escape"));
        }
        let hex = &self.input[self.pos..self.pos + 4];
        let mut val: u16 = 0;
        for &b in hex {
            val <<= 4;
            val |= match b {
                b'0'..=b'9' => (b - b'0') as u16,
                b'a'..=b'f' => (b - b'a' + 10) as u16,
                b'A'..=b'F' => (b - b'A' + 10) as u16,
                _ => return Err(self.error("invalid unicode escape")),
            };
        }
        self.pos += 4;
        Ok(val)
    }

    /// Parse a string value (for JSON values, not keys)
    fn parse_string_value(&mut self) -> Result<Value, VmError> {
        let scanned = self.scan_string_raw()?;
        match scanned {
            JsonStr::Borrowed(s) => {
                // Zero-copy path: s points into input, safe to cache
                if let Some(&cached) = self.val_cache.get(s) {
                    return Ok(Value::string(cached));
                }
                let js = JsString::new_gc(s);
                if s.len() <= 64 {
                    self.val_cache.insert(s, js);
                }
                Ok(Value::string(js))
            }
            JsonStr::Owned(s) => {
                // Escaped string: can't cache by &str (owned, not in input)
                let js = JsString::new_gc(&s);
                self.reclaim_buf(s);
                Ok(Value::string(js))
            }
        }
    }

    /// Intern a key string (for object property keys).
    fn intern_key(&mut self, s: &'a str) -> GcRef<JsString> {
        if let Some(&cached) = self.key_cache.get(s) {
            return cached;
        }
        let js = JsString::intern(s);
        self.key_cache.insert(s, js);
        js
    }

    /// Intern a key from a JsonStr scan result.
    fn intern_key_json(&mut self, scanned: JsonStr<'a>) -> GcRef<JsString> {
        match scanned {
            JsonStr::Borrowed(s) => self.intern_key(s),
            JsonStr::Owned(s) => {
                // Can't cache by &str since s is owned and will be dropped
                let js = JsString::intern(&s);
                self.reclaim_buf(s);
                js
            }
        }
    }

    /// Parse a JSON object with property buffering (V8 style).
    fn parse_object(&mut self) -> Result<Value, VmError> {
        self.advance(); // skip '{'
        self.skip_whitespace();

        if self.peek() == Some(b'}') {
            self.advance();
            let obj = GcRef::new(JsObject::new(self.object_proto));
            return Ok(Value::object(obj));
        }

        // Try shape template matching (hot path for arrays of same-shaped objects)
        let template = self.array_shape_template.take();
        if let Some(tmpl) = template {
            let result = self.parse_object_with_template(tmpl);
            return result;
        }

        // Cold path: buffer all properties, then build object
        self.parse_object_cold()
    }

    /// Hot path: match object properties against a cached shape template.
    fn parse_object_with_template(&mut self, tmpl: ShapeTemplate) -> Result<Value, VmError> {
        let expected_len = tmpl.keys.len();
        let mut values: SmallVec<[Value; 8]> = SmallVec::with_capacity(expected_len);
        let mut matched = true;
        let mut key_idx = 0;

        loop {
            self.skip_whitespace();
            if self.peek() != Some(b'"') {
                matched = false;
                break;
            }

            let key_scanned = self.scan_string_raw()?;
            let key_str = key_scanned.as_str();
            if key_idx >= expected_len || tmpl.keys[key_idx].as_str() != key_str {
                matched = false;
                // Still need to parse the value to advance position
                self.skip_whitespace();
                self.expect(b':')?;
                let _ = self.parse_value()?;
                // TODO: handle remaining keys in slow path
                break;
            }

            self.skip_whitespace();
            self.expect(b':')?;
            // Save/restore template so nested objects don't clobber it
            let saved = self.array_shape_template.take();
            let value = self.parse_value()?;
            self.array_shape_template = saved;
            values.push(value);
            key_idx += 1;

            self.skip_whitespace();
            match self.peek() {
                Some(b',') => self.advance(),
                Some(b'}') => {
                    self.advance();
                    break;
                }
                _ => return Err(self.error("expected ',' or '}'")),
            }
        }

        if matched && key_idx == expected_len {
            // Perfect match — reuse cached shape
            let shape = Arc::clone(&tmpl.shape);
            self.array_shape_template = Some(tmpl);
            let obj = GcRef::new(JsObject::with_shape_and_values_no_barrier(
                self.object_proto,
                shape,
                &values,
            ));
            return Ok(Value::object(obj));
        }

        // Mismatch — build object from prefix + remaining
        let obj = GcRef::new(JsObject::new(self.object_proto));
        for (i, val) in values.into_iter().enumerate() {
            if i < tmpl.keys.len() {
                obj.define_data_property_for_construction(tmpl.keys[i], val);
            }
        }
        // Parse remaining properties
        loop {
            self.skip_whitespace();
            match self.peek() {
                Some(b'}') => {
                    self.advance();
                    break;
                }
                Some(b',') => self.advance(),
                _ => {}
            }
            self.skip_whitespace();
            if self.peek() == Some(b'}') {
                self.advance();
                break;
            }
            if self.peek() != Some(b'"') {
                return Err(self.error("expected string key"));
            }
            let key_scanned = self.scan_string_raw()?;
            let key = self.intern_key_json(key_scanned);
            self.skip_whitespace();
            self.expect(b':')?;
            let value = self.parse_value()?;
            obj.define_data_property_for_construction(key, value);
        }
        // Invalidate template
        self.array_shape_template = None;
        Ok(Value::object(obj))
    }

    /// Compute a fast fingerprint for a key sequence (for shape cache lookup).
    #[inline]
    fn key_fingerprint(keys: &[GcRef<JsString>]) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = rustc_hash::FxHasher::default();
        keys.len().hash(&mut h);
        for k in keys {
            k.as_ptr().hash(&mut h);
        }
        h.finish()
    }

    /// Cold path: no template. Buffer all properties, build shape, cache template.
    fn parse_object_cold(&mut self) -> Result<Value, VmError> {
        let mut keys: SmallVec<[GcRef<JsString>; 8]> = SmallVec::new();
        let mut values: SmallVec<[Value; 8]> = SmallVec::new();

        loop {
            self.skip_whitespace();
            if self.peek() == Some(b'}') {
                self.advance();
                break;
            }
            if !keys.is_empty() {
                self.expect(b',')?;
                self.skip_whitespace();
            }

            // Key
            if self.peek() != Some(b'"') {
                return Err(self.error("expected string key"));
            }
            let key_scanned = self.scan_string_raw()?;
            let key = self.intern_key_json(key_scanned);

            self.skip_whitespace();
            self.expect(b':')?;

            // Value — save/restore template so nested objects don't clobber
            // the parent array's template (critical for shape reuse)
            let saved = self.array_shape_template.take();
            let value = self.parse_value()?;
            self.array_shape_template = saved;

            keys.push(key);
            values.push(value);
        }

        // Build object with shape
        if keys.len() <= crate::object::DICTIONARY_THRESHOLD {
            // Check global shape cache first — reuses shapes for ALL objects
            // with the same key set (metadata, preferences, etc.), not just array siblings.
            let fp = Self::key_fingerprint(&keys);
            let shape = if let Some((cached_keys, cached_shape)) = self.shape_cache.get(&fp) {
                // Verify keys match (collision check)
                if cached_keys.len() == keys.len()
                    && cached_keys
                        .iter()
                        .zip(keys.iter())
                        .all(|(a, b)| a.as_ptr() == b.as_ptr())
                {
                    Arc::clone(cached_shape)
                } else {
                    let root = crate::shape::Shape::root();
                    let prop_keys: SmallVec<[PropertyKey; 8]> =
                        keys.iter().map(|k| PropertyKey::String(*k)).collect();
                    crate::shape::Shape::from_keys(&root, &prop_keys)
                }
            } else {
                let root = crate::shape::Shape::root();
                let prop_keys: SmallVec<[PropertyKey; 8]> =
                    keys.iter().map(|k| PropertyKey::String(*k)).collect();
                let shape = crate::shape::Shape::from_keys(&root, &prop_keys);
                self.shape_cache
                    .insert(fp, (keys.to_vec(), Arc::clone(&shape)));
                shape
            };

            let obj = GcRef::new(JsObject::with_shape_and_values_no_barrier(
                self.object_proto,
                Arc::clone(&shape),
                &values,
            ));

            // Cache shape template for sibling objects in the same array
            self.array_shape_template = Some(ShapeTemplate {
                keys: keys.to_vec(),
                shape,
            });

            return Ok(Value::object(obj));
        }

        // Dictionary fallback for very large objects
        let obj = GcRef::new(JsObject::new(self.object_proto));
        for (key, value) in keys.into_iter().zip(values.into_iter()) {
            obj.define_data_property_for_construction(key, value);
        }
        Ok(Value::object(obj))
    }

    /// Parse a JSON array.
    /// Collects all elements first, then creates the array with pre-sized elements.
    fn parse_array(&mut self) -> Result<Value, VmError> {
        self.advance(); // skip '['
        self.skip_whitespace();

        if self.peek() == Some(b']') {
            self.advance();
            let arr = GcRef::new(JsObject::array(0));
            arr.set_prototype(self.array_proto);
            return Ok(Value::array(arr));
        }

        // Save parent array's shape template
        let saved_template = self.array_shape_template.take();

        // Collect all elements first
        let mut elements: Vec<Value> = Vec::with_capacity(16);
        loop {
            let value = self.parse_value()?;
            elements.push(value);

            self.skip_whitespace();
            match self.peek() {
                Some(b',') => {
                    self.advance();
                    self.skip_whitespace();
                }
                Some(b']') => {
                    self.advance();
                    break;
                }
                _ => return Err(self.error("expected ',' or ']'")),
            }
        }

        // Create array with pre-sized elements (single borrow, no per-element barriers)
        let arr = GcRef::new(JsObject::array(elements.len()));
        arr.set_prototype(self.array_proto);
        {
            let mut elems = arr.elements.borrow_mut();
            *elems = crate::object::ElementsKind::Object(elements);
        }
        arr.flags.borrow_mut().dense_array_length_hint = arr.elements.borrow().len() as u32;

        // Restore parent scope's template
        self.array_shape_template = saved_template;
        Ok(Value::array(arr))
    }

    /// Top-level parse entry.
    fn parse(&mut self) -> Result<Value, VmError> {
        self.skip_whitespace();
        let value = self.parse_value()?;
        self.skip_whitespace();
        if self.pos < self.input.len() {
            return Err(self.error("unexpected trailing content"));
        }
        Ok(value)
    }
}

fn parse_json_to_value_direct(
    text: &str,
    object_proto: &Value,
    array_proto: &Value,
    ncx: &mut NativeContext<'_>,
) -> Result<Value, VmError> {
    let mut parser = JsonParser::new(text, *object_proto, *array_proto, ncx);
    parser.parse()
}

/// JSON string escaping with SWAR fast scan + DoNotEscape table.
/// Processes 8 bytes at a time when safe, falls back to per-byte table lookup.
fn escape_json_string(s: &str, out: &mut String) {
    let bytes = s.as_bytes();
    let len = bytes.len();
    let mut last_flush = 0;
    let mut i = 0;

    // SWAR fast scan: skip 8 bytes at a time when all are safe
    while i + 8 <= len {
        // SAFETY: i + 8 <= len guarantees we're in bounds
        let word = unsafe { (bytes.as_ptr().add(i) as *const u64).read_unaligned() };
        if !has_escape_char_in_word(word) {
            i += 8;
            continue;
        }
        // Some byte in this 8-byte chunk needs escaping — process one by one
        let chunk_end = i + 8;
        while i < chunk_end {
            let esc = ESCAPE_TABLE[bytes[i] as usize];
            if esc != 0 {
                // Flush safe prefix
                if last_flush < i {
                    out.push_str(unsafe { std::str::from_utf8_unchecked(&bytes[last_flush..i]) });
                }
                if esc == 1 {
                    // Generic \uXXXX for control chars
                    let hex = hex4(bytes[i]);
                    out.push_str(unsafe { std::str::from_utf8_unchecked(&hex) });
                } else {
                    // Named escape: \n, \t, \", \\, etc.
                    out.push('\\');
                    out.push(esc as char);
                }
                last_flush = i + 1;
            }
            i += 1;
        }
    }

    // Handle remaining bytes (< 8)
    while i < len {
        let esc = ESCAPE_TABLE[bytes[i] as usize];
        if esc != 0 {
            if last_flush < i {
                out.push_str(unsafe { std::str::from_utf8_unchecked(&bytes[last_flush..i]) });
            }
            if esc == 1 {
                let hex = hex4(bytes[i]);
                out.push_str(unsafe { std::str::from_utf8_unchecked(&hex) });
            } else {
                out.push('\\');
                out.push(esc as char);
            }
            last_flush = i + 1;
        }
        i += 1;
    }

    // Flush remainder
    if last_flush < len {
        out.push_str(unsafe { std::str::from_utf8_unchecked(&bytes[last_flush..]) });
    }
}

/// Format a byte as `\uXXXX` — returns 6 ASCII bytes.
#[inline(always)]
fn hex4(b: u8) -> [u8; 6] {
    const HEX: [u8; 16] = *b"0123456789abcdef";
    [
        b'\\',
        b'u',
        b'0',
        b'0',
        HEX[(b >> 4) as usize],
        HEX[(b & 0xF) as usize],
    ]
}

/// Escape JSON string preserving lone surrogates from UTF-16 data.
/// Uses ESCAPE_TABLE for ASCII range, bulk copy for safe runs.
fn escape_json_string_utf16(units: &[u16], out: &mut String) {
    let mut i = 0;
    let len = units.len();

    while i < len {
        // Scan for a run of safe characters
        let run_start = i;
        while i < len {
            let c = units[i];
            // Fast path: use ESCAPE_TABLE for ASCII range
            if c < 0x80 {
                if ESCAPE_TABLE[c as usize] != 0 {
                    break; // needs escaping
                }
                i += 1;
            } else if !(0xD800..=0xDFFF).contains(&c) {
                // Non-ASCII BMP, not a surrogate — safe
                i += 1;
            } else {
                break; // surrogate — handle specially
            }
        }

        // Bulk-copy the safe run
        if i > run_start {
            out.reserve(i - run_start);
            for &u in &units[run_start..i] {
                if u < 0x80 {
                    out.push(u as u8 as char);
                } else if let Some(ch) = char::from_u32(u as u32) {
                    out.push(ch);
                }
            }
        }

        if i >= len {
            break;
        }

        let code = units[i];
        if code < 0x80 {
            // ASCII that needs escaping — use table
            let esc = ESCAPE_TABLE[code as usize];
            if esc == 1 {
                let hex = hex4_u16(code);
                out.push_str(unsafe { std::str::from_utf8_unchecked(&hex) });
            } else {
                out.push('\\');
                out.push(esc as char);
            }
        } else if (0xD800..=0xDBFF).contains(&code) {
            // High surrogate
            if i + 1 < len && (0xDC00..=0xDFFF).contains(&units[i + 1]) {
                let high = (code as u32 - 0xD800) << 10;
                let low = units[i + 1] as u32 - 0xDC00;
                let cp = 0x10000 + high + low;
                if let Some(ch) = char::from_u32(cp) {
                    out.push(ch);
                }
                i += 1;
            } else {
                // Lone high surrogate → \uXXXX
                let hex = hex4_u16(code);
                out.push_str(unsafe { std::str::from_utf8_unchecked(&hex) });
            }
        } else if (0xDC00..=0xDFFF).contains(&code) {
            // Lone low surrogate → \uXXXX
            let hex = hex4_u16(code);
            out.push_str(unsafe { std::str::from_utf8_unchecked(&hex) });
        } else if let Some(ch) = char::from_u32(code as u32) {
            out.push(ch);
        }
        i += 1;
    }
}

/// Format a u16 code unit as `\uXXXX` — returns 6 ASCII bytes.
#[inline(always)]
fn hex4_u16(c: u16) -> [u8; 6] {
    const HEX: [u8; 16] = *b"0123456789abcdef";
    [
        b'\\',
        b'u',
        HEX[((c >> 12) & 0xF) as usize],
        HEX[((c >> 8) & 0xF) as usize],
        HEX[((c >> 4) & 0xF) as usize],
        HEX[(c & 0xF) as usize],
    ]
}

/// Format a number for JSON output (NaN and Infinity become "null")
fn format_number(n: f64, out: &mut String) {
    if n.is_nan() || n.is_infinite() {
        out.push_str("null");
    } else {
        out.push_str(&crate::globals::js_number_to_string(n));
    }
}

/// Format a number as a property key (JavaScript ToString semantics)
fn number_to_property_key(n: f64) -> String {
    crate::globals::js_number_to_string(n)
}

#[inline]
fn stringify_callback_key_value(prop_key: PropertyKey, key_text: &str) -> Value {
    match prop_key {
        PropertyKey::String(s) => Value::string(s),
        PropertyKey::Index(_) => Value::string(JsString::intern(key_text)),
        PropertyKey::Symbol(_) => Value::undefined(),
    }
}

#[inline]
fn stringify_access_key_value_for_proxy(prop_key: PropertyKey, key_text: &str) -> Value {
    match prop_key {
        PropertyKey::Index(i) if i <= i32::MAX as u32 => Value::int32(i as i32),
        PropertyKey::Index(i) => Value::number(i as f64),
        PropertyKey::String(_) => stringify_callback_key_value(prop_key, key_text),
        PropertyKey::Symbol(_) => Value::undefined(),
    }
}

/// Call toJSON method on value if it exists
/// Note: Does NOT throw for BigInt - that's handled after the replacer is called
fn call_to_json(
    value: &Value,
    key_text: &str,
    prop_key: PropertyKey,
    ncx: &mut NativeContext,
) -> Result<Value, VmError> {
    // Check if value has toJSON method
    if let Some(obj) = value.as_object().or_else(|| value.as_array()) {
        // Use get_property_value to properly invoke getter accessors
        let to_json = get_property_value(&obj, &PropertyKey::string("toJSON"), value, ncx)?;
        if to_json.is_callable() {
            let key_value = stringify_callback_key_value(prop_key, key_text);
            return ncx.call_function(&to_json, *value, &[key_value]);
        }
    }
    // For BigInt, check BigInt.prototype.toJSON (but don't throw if not present)
    if value.is_bigint() {
        // Try to get BigInt.prototype from global
        let global = ncx.ctx.global();
        if let Some(bigint_ctor) = global.get(&PropertyKey::string("BigInt"))
            && let Some(bigint_ctor_obj) = bigint_ctor.as_object()
            && let Some(bigint_proto) = bigint_ctor_obj.get(&PropertyKey::string("prototype"))
            && let Some(proto_obj) = bigint_proto.as_object()
        {
            // Use get_property_value to invoke getter accessors
            // The receiver should be the BigInt value itself for proper `this` binding
            let to_json =
                get_property_value(&proto_obj, &PropertyKey::string("toJSON"), value, ncx)?;
            if to_json.is_callable() {
                let key_value = stringify_callback_key_value(prop_key, key_text);
                return ncx.call_function(&to_json, *value, &[key_value]);
            }
        }
        // Don't throw here - let the replacer have a chance first
        // The BigInt error is thrown in stringify_with_replacer after the replacer call
    }
    Ok(*value)
}

/// Call replacer function if present
fn call_replacer(
    replacer_fn: &Option<Value>,
    holder: &Value,
    key_text: &str,
    prop_key: PropertyKey,
    value: Value,
    ncx: &mut NativeContext,
) -> Result<Value, VmError> {
    if let Some(replacer) = replacer_fn {
        let key_value = stringify_callback_key_value(prop_key, key_text);
        return ncx.call_function(replacer, *holder, &[key_value, value]);
    }
    Ok(value)
}

/// Check if value is an array (including through proxies)
/// Per ES spec, IsArray recursively unwraps proxies to check the target
fn is_array_value(value: &Value) -> Result<bool, VmError> {
    // Direct array check
    if value.as_array().is_some() {
        return Ok(true);
    }
    // Object with is_array flag
    if let Some(obj) = value.as_object()
        && obj.is_array()
    {
        return Ok(true);
    }
    // Proxy: recursively check target
    if let Some(proxy) = value.as_proxy() {
        let target = proxy.target().ok_or_else(|| {
            VmError::type_error("Cannot perform 'IsArray' on a proxy that has been revoked")
        })?;
        return is_array_value(&target);
    }
    Ok(false)
}

/// Get property value from object, properly invoking accessor getters
fn get_property_value(
    obj: &GcRef<JsObject>,
    key: &PropertyKey,
    receiver: &Value,
    ncx: &mut NativeContext,
) -> Result<Value, VmError> {
    // Common fast path: ordinary data properties (own/prototype chain) without accessors.
    // JsObject::get already handles prototype traversal for data properties.
    if let Some(value) = obj.get(key) {
        return Ok(value);
    }

    // Slow path: accessor descriptors (where JsObject::get intentionally returns None).
    if let Some(desc) = obj.lookup_property_descriptor(key) {
        match desc {
            PropertyDescriptor::Data { value, .. } => Ok(value),
            PropertyDescriptor::Accessor { get, .. } => {
                if let Some(getter) = get {
                    if getter.is_callable() {
                        ncx.call_function(&getter, *receiver, &[])
                    } else {
                        Ok(Value::undefined())
                    }
                } else {
                    Ok(Value::undefined())
                }
            }
            PropertyDescriptor::Deleted => Ok(Value::undefined()),
        }
    } else {
        Ok(Value::undefined())
    }
}

/// Convert a value to usize using ToNumber semantics (may throw)
fn value_to_usize(value: &Value, ncx: &mut NativeContext) -> Result<usize, VmError> {
    // Fast path for primitives
    if let Some(n) = value.as_int32() {
        return Ok(n.max(0) as usize);
    }
    if let Some(n) = value.as_number() {
        return Ok((n.max(0.0) as usize).min(usize::MAX));
    }
    // For objects, call valueOf to convert to number
    if let Some(obj) = value.as_object()
        && let Some(value_of) = obj.get(&PropertyKey::string("valueOf"))
        && value_of.is_callable()
    {
        let result = ncx.call_function(&value_of, Value::object(obj), &[])?;
        if let Some(n) = result.as_int32() {
            return Ok(n.max(0) as usize);
        }
        if let Some(n) = result.as_number() {
            return Ok((n.max(0.0) as usize).min(usize::MAX));
        }
    }
    // Default to 0 for other cases
    Ok(0)
}

/// Unwrap wrapper objects per ES spec - calls ToString for String wrappers, ToNumber for Number wrappers
fn unwrap_primitive_with_calls(value: &Value, ncx: &mut NativeContext) -> Result<Value, VmError> {
    if let Some(obj) = value.as_object() {
        // Check for [[StringData]] (String wrapper) - call ToString
        if obj
            .get(&PropertyKey::string("__primitiveValue__"))
            .is_some()
        {
            if let Some(to_string) = obj.get(&PropertyKey::string("toString"))
                && to_string.is_callable()
            {
                return ncx.call_function(&to_string, *value, &[]);
            }
            // Fallback to primitive value
            if let Some(prim) = obj.get(&PropertyKey::string("__primitiveValue__"))
                && prim.as_string().is_some()
            {
                return Ok(prim);
            }
        }
        // Check for [[NumberData]] (Number wrapper) - call valueOf (ToNumber)
        if let Some(prim) = obj.get(&PropertyKey::string("__value__"))
            && (prim.as_number().is_some() || prim.as_int32().is_some())
        {
            // Call valueOf to get the number
            if let Some(value_of) = obj.get(&PropertyKey::string("valueOf"))
                && value_of.is_callable()
            {
                return ncx.call_function(&value_of, *value, &[]);
            }
            // Fallback to primitive value
            return Ok(prim);
        }
        // Check for [[BooleanData]] (Boolean wrapper)
        if let Some(prim) = obj.get(&PropertyKey::string("__value__"))
            && prim.as_boolean().is_some()
        {
            return Ok(prim);
        }
    }
    Ok(*value)
}

/// Full stringify with toJSON and replacer support
fn stringify_with_replacer(
    holder: &Value,
    key: &str,
    replacer_fn: &Option<Value>,
    indent: &Option<String>,
    property_list: &Option<Vec<String>>,
    tracker: &mut CircularTracker,
    depth: usize,
    ncx: &mut NativeContext,
    out: &mut String,
) -> Result<bool, VmError> {
    let prop_key = PropertyKey::string(key);
    stringify_with_replacer_prepared(
        holder,
        key,
        prop_key,
        replacer_fn,
        indent,
        property_list,
        tracker,
        depth,
        ncx,
        out,
    )
}

fn stringify_with_replacer_prepared(
    holder: &Value,
    key: &str,
    prop_key: PropertyKey,
    replacer_fn: &Option<Value>,
    indent: &Option<String>,
    property_list: &Option<Vec<String>>,
    tracker: &mut CircularTracker,
    depth: usize,
    ncx: &mut NativeContext,
    out: &mut String,
) -> Result<bool, VmError> {
    // Depth limit
    if depth > 100 {
        out.push_str("null");
        return Ok(true);
    }

    // Step 2: Get value from holder (properly invoking getters)
    let value = if let Some(obj) = holder.as_object().or_else(|| holder.as_array()) {
        get_property_value(&obj, &prop_key, holder, ncx)?
    } else if let Some(proxy) = holder.as_proxy() {
        // For proxies, invoke the get trap
        let access_key_value = stringify_access_key_value_for_proxy(prop_key, key);
        crate::proxy_operations::proxy_get(ncx, proxy, &prop_key, access_key_value, *holder)?
    } else {
        return Ok(false);
    };

    // Step 3: Call toJSON if present
    let value = call_to_json(&value, key, prop_key, ncx)?;

    // Step 4: Call replacer function if present
    let value = call_replacer(replacer_fn, holder, key, prop_key, value, ncx)?;

    // Step 5: Unwrap wrapper objects (calls ToString for String wrappers per spec)
    let value = unwrap_primitive_with_calls(&value, ncx)?;

    // Step 6: Serialize based on type
    // undefined, functions, symbols return false (omitted)
    if value.is_undefined() || value.is_callable() || value.is_symbol() {
        return Ok(false);
    }

    // null
    if value.is_null() {
        out.push_str("null");
        return Ok(true);
    }

    // BigInt should have been handled by toJSON or should throw
    if value.is_bigint() {
        return Err(VmError::type_error("Do not know how to serialize a BigInt"));
    }

    // Boolean
    if let Some(b) = value.as_boolean() {
        out.push_str(if b { "true" } else { "false" });
        return Ok(true);
    }

    // Number (int32 or f64)
    if let Some(n) = value.as_int32() {
        let mut buf = itoa::Buffer::new();
        out.push_str(buf.format(n));
        return Ok(true);
    }
    if let Some(n) = value.as_number() {
        format_number(n, out);
        return Ok(true);
    }

    // String - use UTF-16 escaping to preserve lone surrogates
    if let Some(s) = value.as_string() {
        out.push('"');
        escape_json_string_utf16(s.as_utf16(), out);
        out.push('"');
        return Ok(true);
    }

    // Check for array (including proxy arrays)
    if is_array_value(&value)? {
        stringify_array_with_replacer(
            &value,
            key,
            replacer_fn,
            indent,
            property_list,
            tracker,
            depth,
            ncx,
            out,
        )?;
        return Ok(true);
    }

    // Regular object or proxy
    if value.as_object().is_some() || value.as_proxy().is_some() {
        stringify_object_with_replacer(
            &value,
            key,
            replacer_fn,
            indent,
            property_list,
            tracker,
            depth,
            ncx,
            out,
        )?;
        return Ok(true);
    }

    out.push_str("null");
    Ok(true)
}

/// Stringify array with replacer support
fn stringify_array_with_replacer(
    value: &Value,
    arr_key: &str,
    replacer_fn: &Option<Value>,
    indent: &Option<String>,
    property_list: &Option<Vec<String>>,
    tracker: &mut CircularTracker,
    depth: usize,
    ncx: &mut NativeContext,
    out: &mut String,
) -> Result<(), VmError> {
    // Get pointer for circular reference checking
    // Works for arrays, objects, and proxies
    let ptr = if let Some(obj) = value.as_array().or_else(|| value.as_object()) {
        obj.as_ptr() as usize
    } else if let Some(proxy) = value.as_proxy() {
        proxy.as_ptr() as usize
    } else {
        return Err(VmError::type_error("Expected array"));
    };

    if let Err(msg) = tracker.enter(arr_key, ptr, true) {
        return Err(VmError::type_error(msg));
    }

    // Get length - use property access that works for both objects and proxies
    let length_key = PropertyKey::string("length");
    let length_val = if let Some(obj) = value.as_array().or_else(|| value.as_object()) {
        obj.get(&length_key).unwrap_or(Value::int32(0))
    } else if let Some(proxy) = value.as_proxy() {
        // For proxies, invoke the get trap
        crate::proxy_operations::proxy_get(
            ncx,
            proxy,
            &length_key,
            Value::string(JsString::intern("length")),
            *value,
        )?
    } else {
        Value::int32(0)
    };

    // Convert length to number using ToNumber semantics (may throw)
    let len = value_to_usize(&length_val, ncx)?;

    if len == 0 {
        out.push_str("[]");
        tracker.exit(ptr);
        return Ok(());
    }

    out.push('[');
    if let Some(_ind) = indent {
        out.push('\n');
    }

    let mut index_key_text = String::new();
    for i in 0..len {
        maybe_check_interrupt(ncx, i)?;
        index_key_text.clear();
        let mut ibuf = itoa::Buffer::new();
        index_key_text.push_str(ibuf.format(i));

        if i > 0 {
            out.push(',');
            if let Some(_ind) = indent {
                out.push('\n');
            }
        }
        if let Some(ind) = indent {
            for _ in 0..=depth {
                out.push_str(ind);
            }
        }

        let initial_len = out.len();
        let written = if let Ok(index) = u32::try_from(i) {
            stringify_with_replacer_prepared(
                value,
                &index_key_text,
                PropertyKey::Index(index),
                replacer_fn,
                indent,
                property_list,
                tracker,
                depth + 1,
                ncx,
                out,
            )?
        } else {
            stringify_with_replacer(
                value,
                &index_key_text,
                replacer_fn,
                indent,
                property_list,
                tracker,
                depth + 1,
                ncx,
                out,
            )?
        };

        if !written {
            out.truncate(initial_len);
            out.push_str("null");
        }
    }

    if let Some(ind) = indent {
        out.push('\n');
        for _ in 0..depth {
            out.push_str(ind);
        }
    }
    out.push(']');

    tracker.exit(ptr);
    Ok(())
}

/// Stringify object with replacer support
fn stringify_object_with_replacer(
    value: &Value,
    obj_key: &str,
    replacer_fn: &Option<Value>,
    indent: &Option<String>,
    property_list: &Option<Vec<String>>,
    tracker: &mut CircularTracker,
    depth: usize,
    ncx: &mut NativeContext,
    out: &mut String,
) -> Result<(), VmError> {
    enum ObjectStringifyKey {
        Prepared(PropertyKey),
        Text(String),
    }

    // Get pointer for circular reference checking - works for objects and proxies
    let (ptr, keys) = if let Some(obj) = value.as_object() {
        let ptr = obj.as_ptr() as usize;
        let keys: Vec<ObjectStringifyKey> = if let Some(list) = property_list {
            list.iter().cloned().map(ObjectStringifyKey::Text).collect()
        } else {
            obj.own_keys()
                .into_iter()
                .filter_map(|k| {
                    // Check if property is enumerable
                    // Note: own_keys() may return Index(i) for properties stored as String("i")
                    // so we need to check both forms
                    let desc = obj.get_own_property_descriptor(&k).or_else(|| {
                        if let PropertyKey::Index(i) = &k {
                            obj.get_own_property_descriptor(&PropertyKey::string(&i.to_string()))
                        } else {
                            None
                        }
                    });
                    if let Some(desc) = desc
                        && desc.enumerable()
                    {
                        return match k {
                            PropertyKey::String(_) | PropertyKey::Index(_) => {
                                Some(ObjectStringifyKey::Prepared(k))
                            }
                            PropertyKey::Symbol(_) => None, // Symbols are not included in JSON
                        };
                    }
                    None
                })
                .collect()
        };
        (ptr, keys)
    } else if let Some(proxy) = value.as_proxy() {
        let ptr = proxy.as_ptr() as usize;
        // For proxies, use the property list if available, otherwise use proxy ownKeys trap
        let keys: Vec<ObjectStringifyKey> = if let Some(list) = property_list {
            list.iter().cloned().map(ObjectStringifyKey::Text).collect()
        } else {
            // Get keys from proxy using ownKeys trap
            let proxy_keys = crate::proxy_operations::proxy_own_keys(ncx, proxy)?;
            proxy_keys
                .into_iter()
                .filter_map(|k| match k {
                    PropertyKey::String(s) => {
                        Some(ObjectStringifyKey::Text(s.as_str().to_string()))
                    }
                    PropertyKey::Index(i) => Some(ObjectStringifyKey::Text(i.to_string())),
                    PropertyKey::Symbol(_) => None,
                })
                .collect()
        };
        (ptr, keys)
    } else {
        return Err(VmError::type_error("Expected object"));
    };

    if let Err(msg) = tracker.enter(obj_key, ptr, false) {
        return Err(VmError::type_error(msg));
    }

    if keys.is_empty() {
        out.push_str("{}");
        tracker.exit(ptr);
        return Ok(());
    }

    out.push('{');
    let mut first = true;
    let mut wrote_property = false;

    for (i, key_entry) in keys.into_iter().enumerate() {
        maybe_check_interrupt(ncx, i)?;
        let initial_len = out.len();

        match key_entry {
            ObjectStringifyKey::Prepared(prop_key) => match prop_key {
                PropertyKey::String(s) => {
                    let key_text = s.as_str();
                    if !first {
                        out.push(',');
                    }
                    if let Some(ind) = indent {
                        out.push('\n');
                        for _ in 0..=depth {
                            out.push_str(ind);
                        }
                    }

                    out.push('"');
                    escape_json_string(key_text, out);
                    out.push('"');
                    out.push(':');
                    if indent.is_some() {
                        out.push(' ');
                    }

                    let written = stringify_with_replacer_prepared(
                        value,
                        key_text,
                        prop_key,
                        replacer_fn,
                        indent,
                        property_list,
                        tracker,
                        depth + 1,
                        ncx,
                        out,
                    )?;

                    if written {
                        first = false;
                        wrote_property = true;
                    } else {
                        out.truncate(initial_len);
                    }
                }
                PropertyKey::Index(i) => {
                    let key_text = i.to_string();
                    if !first {
                        out.push(',');
                    }
                    if let Some(ind) = indent {
                        out.push('\n');
                        for _ in 0..=depth {
                            out.push_str(ind);
                        }
                    }

                    out.push('"');
                    escape_json_string(&key_text, out);
                    out.push('"');
                    out.push(':');
                    if indent.is_some() {
                        out.push(' ');
                    }

                    let written = stringify_with_replacer_prepared(
                        value,
                        &key_text,
                        prop_key,
                        replacer_fn,
                        indent,
                        property_list,
                        tracker,
                        depth + 1,
                        ncx,
                        out,
                    )?;

                    if written {
                        first = false;
                        wrote_property = true;
                    } else {
                        out.truncate(initial_len);
                    }
                }
                PropertyKey::Symbol(_) => continue,
            },
            ObjectStringifyKey::Text(key) => {
                if !first {
                    out.push(',');
                }
                if let Some(ind) = indent {
                    out.push('\n');
                    for _ in 0..=depth {
                        out.push_str(ind);
                    }
                }

                out.push('"');
                escape_json_string(&key, out);
                out.push('"');
                out.push(':');
                if indent.is_some() {
                    out.push(' ');
                }

                let written = stringify_with_replacer(
                    value,
                    &key,
                    replacer_fn,
                    indent,
                    property_list,
                    tracker,
                    depth + 1,
                    ncx,
                    out,
                )?;

                if written {
                    first = false;
                    wrote_property = true;
                } else {
                    out.truncate(initial_len);
                }
            }
        }
    }

    if wrote_property && indent.is_some() {
        out.push('\n');
        for _ in 0..depth {
            out.push_str(indent.as_ref().unwrap());
        }
    }
    out.push('}');

    tracker.exit(ptr);
    Ok(())
}

/// `JSON.parse ( text [, reviver] )`
///
/// Parses a JSON string, constructing the JavaScript value or object described by the string.
///
/// Spec: <https://tc39.es/ecma262/#sec-json.parse>
/// MDN: <https://developer.mozilla.org/en-US/docs/Web/JavaScript/Reference/Global_Objects/JSON/parse>
#[dive(name = "parse", length = 2)]
fn json_parse(
    _this_val: &Value,
    args: &[Value],
    ncx: &mut NativeContext<'_>,
) -> Result<Value, VmError> {
    let arg = args.first().cloned().unwrap_or(Value::undefined());
    let string_arg = arg.as_string();

    // Convert to string using ToString (calling toString() if needed)
    let text = if let Some(s) = string_arg.as_ref() {
        Cow::Borrowed(s.as_str())
    } else if let Some(n) = arg.as_number() {
        Cow::Owned(format!("{}", n))
    } else if let Some(n) = arg.as_int32() {
        Cow::Owned(format!("{}", n))
    } else if let Some(b) = arg.as_boolean() {
        Cow::Borrowed(if b { "true" } else { "false" })
    } else if arg.is_null() {
        Cow::Borrowed("null")
    } else if arg.is_undefined() {
        return Err(VmError::syntax_error("JSON.parse: unexpected input"));
    } else if let Some(obj) = arg.as_object() {
        // Try calling toString() on the object
        if let Some(to_string_fn) = obj.get(&PropertyKey::string("toString")) {
            if to_string_fn.is_callable() {
                let result = ncx.call_function(&to_string_fn, Value::object(obj), &[])?;
                if let Some(s) = result.as_string() {
                    Cow::Owned(s.as_str().to_owned())
                } else {
                    return Err(VmError::syntax_error(
                        "JSON.parse: toString did not return string",
                    ));
                }
            } else {
                Cow::Borrowed("[object Object]")
            }
        } else {
            Cow::Borrowed("[object Object]")
        }
    } else {
        return Err(VmError::syntax_error("JSON.parse: unexpected input"));
    };

    // Avoid cloning the full Intrinsics registry on every parse call.
    let global = ncx.ctx.global();
    let object_proto = global
        .get(&PropertyKey::string("Object"))
        .and_then(|o| o.as_object())
        .and_then(|o| o.get(&PropertyKey::string("prototype")))
        .unwrap_or_else(Value::null);
    let array_proto = global
        .get(&PropertyKey::string("Array"))
        .and_then(|o| o.as_object())
        .and_then(|o| o.get(&PropertyKey::string("prototype")))
        .unwrap_or_else(Value::null);

    let mm = ncx.memory_manager().clone();
    let result = parse_json_to_value_direct(text.as_ref(), &object_proto, &array_proto, ncx)?;

    // Apply reviver if provided
    if let Some(reviver) = args.get(1)
        && reviver.is_callable()
    {
        return apply_reviver(result, reviver, ncx, &mm);
    }

    Ok(result)
}

/// `JSON.stringify ( value [, replacer [, space]] )`
///
/// Converts a JavaScript value to a JSON string, optionally replacing values or including only specified properties.
///
/// Spec: <https://tc39.es/ecma262/#sec-json.stringify>
/// MDN: <https://developer.mozilla.org/en-US/docs/Web/JavaScript/Reference/Global_Objects/JSON/stringify>
#[dive(name = "stringify", length = 3)]
fn json_stringify(
    _this_val: &Value,
    args: &[Value],
    ncx: &mut NativeContext<'_>,
) -> Result<Value, VmError> {
    let val = args.first().cloned().unwrap_or(Value::undefined());

    // undefined at top level returns undefined
    if val.is_undefined() {
        return Ok(Value::undefined());
    }

    // Fast path: no replacer, no space → try direct stringify without wrapper object
    let has_replacer = args.get(1).is_some_and(|r| !r.is_undefined());
    let has_space = args.get(2).is_some_and(|s| !s.is_undefined());

    if !has_replacer && !has_space {
        let estimated_capacity = estimate_stringify_capacity(&val);
        let mut out = String::with_capacity(estimated_capacity);
        if stringify_value_fast(&val, &mut out, 0) {
            return Ok(Value::string(JsString::new_gc(&out)));
        }
        // Fast path couldn't handle it (proxy, toJSON, accessors, etc.) — fall through
    }

    // Parse replacer argument
    let (replacer_fn, property_list) = parse_replacer(args.get(1), ncx)?;

    // Parse space argument
    let space_str = parse_space(args.get(2), ncx)?;

    let mut tracker = CircularTracker::new();

    let estimated_capacity = estimate_stringify_capacity(&val);
    let mut out = String::with_capacity(estimated_capacity);

    // Per spec, stringify always uses a wrapper object and the SerializeJSONProperty algorithm,
    // which ensures toJSON/replacer/getters/proxies are honored.
    let global = ncx.ctx.global();
    let object_proto = global
        .get(&PropertyKey::string("Object"))
        .and_then(|o| o.as_object())
        .and_then(|o| o.get(&PropertyKey::string("prototype")))
        .unwrap_or_else(Value::null);
    let wrapper = GcRef::new(JsObject::new(object_proto));
    let _ = wrapper.set(PropertyKey::string(""), val);
    let wrapper_val = Value::object(wrapper);

    let written = stringify_with_replacer(
        &wrapper_val,
        "",
        &replacer_fn,
        &space_str,
        &property_list,
        &mut tracker,
        0,
        ncx,
        &mut out,
    )?;

    if written {
        // Stringify results are typically short-lived; avoid interning megabyte JSON strings.
        Ok(Value::string(JsString::new_gc(&out)))
    } else {
        Ok(Value::undefined())
    }
}

/// `JSON` namespace object.
///
/// Spec: <https://tc39.es/ecma262/#sec-json-object>
/// Estimate output capacity for JSON.stringify pre-allocation.
fn estimate_stringify_capacity(val: &Value) -> usize {
    if let Some(obj) = val.as_object() {
        if obj.is_array() {
            obj.array_length() * 8 + 16
        } else {
            obj.get_shape_key_count() * 32 + 16
        }
    } else {
        128
    }
    .max(128)
}

/// Fast path for JSON.stringify — V8/JSC-style direct serialization.
/// No toJSON, no replacer, no proxies, no wrapper objects, no accessors.
/// Uses circular tracking, itoa for integers, SWAR escape, and inline shape iteration.
///
/// Returns `true` if the value was successfully serialized to `out`.
/// Returns `false` if the value requires the full spec path.
fn stringify_value_fast(value: &Value, out: &mut String, depth: usize) -> bool {
    let mut stack: SmallVec<[usize; 32]> = SmallVec::new();
    stringify_value_fast_inner(value, out, depth, &mut stack)
}

#[inline]
fn stringify_value_fast_inner(
    value: &Value,
    out: &mut String,
    depth: usize,
    stack: &mut SmallVec<[usize; 32]>,
) -> bool {
    if depth > 100 {
        return false; // bail to slow path which handles depth limit properly
    }

    // null
    if value.is_null() {
        out.push_str("null");
        return true;
    }

    // undefined, functions, symbols → not serializable at top level
    if value.is_undefined() || value.is_callable() || value.is_symbol() {
        return false;
    }

    // Boolean
    if let Some(b) = value.as_boolean() {
        out.push_str(if b { "true" } else { "false" });
        return true;
    }

    // int32 — use itoa for ~3x faster formatting than write!()
    if let Some(n) = value.as_int32() {
        let mut buf = itoa::Buffer::new();
        out.push_str(buf.format(n));
        return true;
    }

    // f64
    if let Some(n) = value.as_number() {
        format_number(n, out);
        return true;
    }

    // String
    if let Some(s) = value.as_string() {
        out.push('"');
        escape_json_string(s.as_str(), out);
        out.push('"');
        return true;
    }

    // BigInt can't be serialized — bail to slow path for proper error
    if value.is_bigint() {
        return false;
    }

    // Proxy — bail to slow path
    if value.as_proxy().is_some() {
        return false;
    }

    // Object (check is_array flag to distinguish arrays from plain objects)
    if let Some(obj) = value.as_object() {
        if obj.is_array() {
            return stringify_array_fast_inner(&obj, out, depth, stack);
        }
        return stringify_object_fast_inner(&obj, out, depth, stack);
    }

    // Also handle Value::array() tag (same underlying JsObject)
    if let Some(obj) = value.as_array() {
        return stringify_array_fast_inner(&obj, out, depth, stack);
    }

    false
}

/// Fast-path array stringify with circular reference tracking.
fn stringify_array_fast_inner(
    arr: &GcRef<JsObject>,
    out: &mut String,
    depth: usize,
    stack: &mut SmallVec<[usize; 32]>,
) -> bool {
    let ptr = arr.as_ptr() as usize;

    // V8-style circular reference check (linear scan of stack)
    if stack.contains(&ptr) {
        return false; // bail to slow path which throws proper TypeError
    }

    let len = arr.array_length();
    if len == 0 {
        out.push_str("[]");
        return true;
    }

    stack.push(ptr);
    out.push('[');
    let elements = arr.elements.borrow();
    for i in 0..len {
        if i > 0 {
            out.push(',');
        }
        if let Some(val) = elements.get(i) {
            if val.is_hole() || val.is_undefined() || val.is_callable() || val.is_symbol() {
                out.push_str("null");
            } else if !stringify_value_fast_inner(&val, out, depth + 1, stack) {
                stack.pop();
                return false;
            }
        } else {
            out.push_str("null");
        }
    }
    out.push(']');
    stack.pop();
    true
}

/// V8-style fast object stringify: shaped objects only, no toJSON/accessors/dictionary.
fn stringify_object_fast_inner(
    obj: &GcRef<JsObject>,
    out: &mut String,
    depth: usize,
    stack: &mut SmallVec<[usize; 32]>,
) -> bool {
    let flags = obj.flags.borrow();
    if flags.is_dictionary {
        return false;
    }
    drop(flags);

    // Check for toJSON: first check own shape (fast — no intern/prototype walk),
    // then fall back to full property lookup (handles Date.prototype.toJSON etc.)
    if obj
        .get(&PropertyKey::string("toJSON"))
        .is_some_and(|v| v.is_callable())
    {
        return false;
    }

    let ptr = obj.as_ptr() as usize;
    if stack.contains(&ptr) {
        return false; // circular — bail to slow path
    }

    let pairs = obj.with_shape(|s| s.own_keys_with_offsets());
    if pairs.is_empty() {
        out.push_str("{}");
        return true;
    }

    stack.push(ptr);
    out.push('{');
    let mut first = true;

    for (key, offset) in &pairs {
        let key_str = match key {
            PropertyKey::String(s) => s.as_str(),
            PropertyKey::Index(_) => {
                // Index keys in shaped objects are very rare — bail to slow path
                stack.pop();
                return false;
            }
            PropertyKey::Symbol(_) => continue,
        };

        let val = match obj.get_by_offset(*offset) {
            Some(v) => v,
            None => {
                stack.pop();
                return false; // accessor property → slow path
            }
        };

        // Skip undefined/function/symbol values
        if val.is_undefined() || val.is_callable() || val.is_symbol() {
            continue;
        }

        if !first {
            out.push(',');
        }
        out.push('"');
        escape_json_string(key_str, out);
        out.push_str("\":");

        if !stringify_value_fast_inner(&val, out, depth + 1, stack) {
            stack.pop();
            return false;
        }
        first = false;
    }

    out.push('}');
    stack.pop();
    true
}

/// MDN: <https://developer.mozilla.org/en-US/docs/Web/JavaScript/Reference/Global_Objects/JSON>
pub struct JsonNamespace;

impl IntrinsicObject for JsonNamespace {
    fn init(ctx: &IntrinsicContext) {
        let json_obj = GcRef::new(JsObject::new(Value::null()));

        NamespaceBuilder::new(ctx.mm(), ctx.fn_proto(), json_obj)
            .method_decl(json_parse_decl())
            .method_decl(json_stringify_decl())
            .string_tag("JSON")
            .install_on(&ctx.global(), "JSON");
    }
}

/// Get a property value from replacer (array or proxy), invoking accessor getters and proxy traps
fn get_replacer_element(
    replacer: &Value,
    index: u32,
    ncx: &mut NativeContext,
) -> Result<Value, VmError> {
    let key = PropertyKey::Index(index);
    // Also try string key for accessor properties defined with string "0", "1", etc.
    let str_key = PropertyKey::String(JsString::intern(&index.to_string()));

    // For arrays/objects
    if let Some(obj) = replacer.as_array().or_else(|| replacer.as_object()) {
        // First check if there's an accessor property (could be defined on string key)
        if let Some(PropertyDescriptor::Accessor { get, .. }) =
            obj.get_own_property_descriptor(&str_key)
        {
            if let Some(getter) = get
                && getter.is_callable()
            {
                return ncx.call_function(&getter, *replacer, &[]);
            }
            return Ok(Value::undefined());
        }
        // Otherwise use direct access
        return Ok(obj.get(&key).unwrap_or(Value::undefined()));
    }

    // For proxies, use proxy_get to invoke the get trap
    if let Some(proxy) = replacer.as_proxy() {
        return crate::proxy_operations::proxy_get(
            ncx,
            proxy,
            &key,
            Value::int32(index as i32),
            *replacer,
        );
    }

    Ok(Value::undefined())
}

/// Get length from replacer (array or proxy), converting to number (may throw)
fn get_replacer_length(replacer: &Value, ncx: &mut NativeContext) -> Result<usize, VmError> {
    let length_key = PropertyKey::string("length");

    // Get the length value - either from object or proxy
    let len_val = if let Some(obj) = replacer.as_array().or_else(|| replacer.as_object()) {
        // Use obj.get() which has special handling for array "length"
        obj.get(&length_key).unwrap_or(Value::int32(0))
    } else if let Some(proxy) = replacer.as_proxy() {
        crate::proxy_operations::proxy_get(
            ncx,
            proxy,
            &length_key,
            Value::string(JsString::intern("length")),
            *replacer,
        )?
    } else {
        return Ok(0);
    };

    // Convert to number using ToNumber semantics (may throw for objects with throwing valueOf)
    value_to_usize(&len_val, ncx)
}

/// Parse the replacer argument (can be function or array)
/// Per spec, for objects with [[StringData]] or [[NumberData]], call ToString(v)
fn parse_replacer(
    replacer: Option<&Value>,
    ncx: &mut NativeContext,
) -> Result<(Option<Value>, Option<Vec<String>>), VmError> {
    let Some(r) = replacer else {
        return Ok((None, None));
    };

    if r.is_callable() {
        return Ok((Some(*r), None));
    }

    // Check for array - use is_array_value to handle proxies wrapping arrays
    if is_array_value(r)? {
        let len = get_replacer_length(r, ncx)?;

        let mut list = Vec::new();
        let mut seen = FxHashSet::default();

        for i in 0..len {
            maybe_check_interrupt(ncx, i)?;
            let item = get_replacer_element(r, i as u32, ncx)?;

            let key = if let Some(s) = item.as_string() {
                Some(s.as_str().to_string())
            } else if let Some(n) = item.as_int32() {
                Some(n.to_string())
            } else if let Some(n) = item.as_number() {
                // Use JavaScript ToString semantics for property keys (preserves NaN, Infinity)
                Some(number_to_property_key(n))
            } else if let Some(obj) = item.as_object() {
                // Check if it's a String or Number wrapper object
                let is_string_wrapper = obj
                    .get(&PropertyKey::string("__primitiveValue__"))
                    .is_some();
                let is_number_wrapper = obj.get(&PropertyKey::string("__value__")).is_some();

                if is_string_wrapper || is_number_wrapper {
                    // Per spec: call ToString(v) for both String and Number wrappers
                    if let Some(to_string) = obj.get(&PropertyKey::string("toString")) {
                        if to_string.is_callable() {
                            let result = ncx.call_function(&to_string, Value::object(obj), &[])?;
                            result.as_string().map(|s| s.as_str().to_string())
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                } else {
                    None
                }
            } else {
                None
            };

            if let Some(k) = key
                && !seen.contains(&k)
            {
                seen.insert(k.clone());
                list.push(k);
            }
        }

        return Ok((None, Some(list)));
    }

    Ok((None, None))
}

/// Parse the space argument
/// Per spec:
/// - If space has [[NumberData]], set space to ToNumber(space)
/// - Else if space has [[StringData]], set space to ToString(space)
fn parse_space(space: Option<&Value>, ncx: &mut NativeContext) -> Result<Option<String>, VmError> {
    let Some(v) = space else {
        return Ok(None);
    };

    // Primitive number
    if let Some(n) = v.as_int32() {
        let n = n.clamp(0, 10) as usize;
        return Ok(if n > 0 { Some(" ".repeat(n)) } else { None });
    }
    if let Some(n) = v.as_number() {
        let n = (n.clamp(0.0, 10.0) as i32).max(0) as usize;
        return Ok(if n > 0 { Some(" ".repeat(n)) } else { None });
    }

    // Primitive string
    if let Some(s) = v.as_string() {
        let str_val = s.as_str();
        return Ok(if str_val.is_empty() {
            None
        } else {
            Some(str_val.chars().take(10).collect::<String>())
        });
    }

    // Object - check if Number or String wrapper
    if let Some(obj) = v.as_object() {
        // Check for [[NumberData]] - use ToNumber (calls valueOf)
        if obj.get(&PropertyKey::string("__value__")).is_some() {
            // It's a Number wrapper - call valueOf to get the number
            if let Some(value_of) = obj.get(&PropertyKey::string("valueOf"))
                && value_of.is_callable()
            {
                let result = ncx.call_function(&value_of, Value::object(obj), &[])?;
                if let Some(n) = result.as_number() {
                    let n = (n.clamp(0.0, 10.0) as i32).max(0) as usize;
                    return Ok(if n > 0 { Some(" ".repeat(n)) } else { None });
                }
                if let Some(n) = result.as_int32() {
                    let n = n.clamp(0, 10) as usize;
                    return Ok(if n > 0 { Some(" ".repeat(n)) } else { None });
                }
            }
            return Ok(None);
        }

        // Check for [[StringData]] - use ToString (calls toString)
        if obj
            .get(&PropertyKey::string("__primitiveValue__"))
            .is_some()
        {
            // It's a String wrapper - call toString to get the string
            if let Some(to_string) = obj.get(&PropertyKey::string("toString"))
                && to_string.is_callable()
            {
                let result = ncx.call_function(&to_string, Value::object(obj), &[])?;
                if let Some(s) = result.as_string() {
                    let str_val = s.as_str();
                    return Ok(if str_val.is_empty() {
                        None
                    } else {
                        Some(str_val.chars().take(10).collect::<String>())
                    });
                }
            }
            return Ok(None);
        }
    }

    Ok(None)
}

/// Apply reviver function to parsed JSON
fn apply_reviver(
    value: Value,
    reviver: &Value,
    ncx: &mut NativeContext,
    _mm: &Arc<MemoryManager>,
) -> Result<Value, VmError> {
    // Create root holder
    let root = GcRef::new(JsObject::new(Value::null()));
    let _ = root.set(PropertyKey::string(""), value);
    let root_val = Value::object(root);

    // Walk and transform
    walk_reviver(&root_val, "", reviver, ncx)
}

/// Recursively apply reviver to parsed value
fn walk_reviver(
    holder: &Value,
    key: &str,
    reviver: &Value,
    ncx: &mut NativeContext,
) -> Result<Value, VmError> {
    // Get value from holder
    let value = get_reviver_value(holder, key, ncx)?;

    // If value is array, recurse into array elements
    if is_array_for_reviver(&value, ncx)? {
        let len = get_length_for_reviver(&value, ncx)?;

        for i in 0..len {
            maybe_check_interrupt(ncx, i)?;
            let elem_key = i.to_string();
            let new_elem = walk_reviver(&value, &elem_key, reviver, ncx)?;
            let prop_key = PropertyKey::Index(i as u32);
            let key_val = Value::string(JsString::intern(&elem_key));
            if new_elem.is_undefined() {
                // Delete the property
                delete_reviver_property(&value, &prop_key, key_val, ncx)?;
            } else {
                // CreateDataProperty - triggers proxy defineProperty trap
                create_data_property(&value, &prop_key, key_val, new_elem, ncx)?;
            }
        }
    } else if is_object_for_reviver(&value) {
        // Get enumerable own property keys
        let keys = get_enumerable_keys(&value, ncx)?;
        for (i, key_str) in keys.into_iter().enumerate() {
            maybe_check_interrupt(ncx, i)?;
            let new_val = walk_reviver(&value, &key_str, reviver, ncx)?;
            let prop_key = PropertyKey::string(&key_str);
            let key_val = Value::string(JsString::intern(&key_str));
            if new_val.is_undefined() {
                delete_reviver_property(&value, &prop_key, key_val, ncx)?;
            } else {
                // CreateDataProperty - triggers proxy defineProperty trap
                create_data_property(&value, &prop_key, key_val, new_val, ncx)?;
            }
        }
    }

    // Call reviver
    let key_val = Value::string(JsString::intern(key));
    ncx.call_function(reviver, *holder, &[key_val, value])
}

/// Get value from holder during reviver walk
fn get_reviver_value(holder: &Value, key: &str, ncx: &mut NativeContext) -> Result<Value, VmError> {
    let prop_key = if let Ok(idx) = key.parse::<u32>() {
        PropertyKey::Index(idx)
    } else {
        PropertyKey::string(key)
    };
    let key_val = Value::string(JsString::intern(key));

    if let Some(proxy) = holder.as_proxy() {
        crate::proxy_operations::proxy_get(ncx, proxy, &prop_key, key_val, *holder)
    } else if let Some(obj) = holder.as_object().or_else(|| holder.as_array()) {
        Ok(obj.get(&prop_key).unwrap_or(Value::undefined()))
    } else {
        Ok(Value::undefined())
    }
}

/// Check if value is an array (handles proxies)
#[allow(clippy::only_used_in_recursion)]
fn is_array_for_reviver(value: &Value, ncx: &mut NativeContext) -> Result<bool, VmError> {
    if let Some(proxy) = value.as_proxy() {
        let target = proxy
            .target()
            .ok_or_else(|| VmError::type_error("Cannot check isArray on revoked proxy"))?;
        return is_array_for_reviver(&target, ncx);
    }
    if let Some(obj) = value.as_object().or_else(|| value.as_array()) {
        return Ok(obj.is_array());
    }
    Ok(false)
}

/// Check if value is an object (not array, handles proxies)
fn is_object_for_reviver(value: &Value) -> bool {
    if let Some(proxy) = value.as_proxy() {
        if let Some(target) = proxy.target() {
            return is_object_for_reviver(&target);
        }
        return false;
    }
    if value.as_array().is_some() {
        return false;
    }
    value.as_object().is_some()
}

/// Get length for array during reviver walk
fn get_length_for_reviver(value: &Value, ncx: &mut NativeContext) -> Result<usize, VmError> {
    let length_key = PropertyKey::string("length");
    let key_val = Value::string(JsString::intern("length"));

    let len_val = if let Some(proxy) = value.as_proxy() {
        crate::proxy_operations::proxy_get(ncx, proxy, &length_key, key_val, *value)?
    } else if let Some(obj) = value.as_object().or_else(|| value.as_array()) {
        obj.get(&length_key).unwrap_or(Value::int32(0))
    } else {
        return Ok(0);
    };

    value_to_usize(&len_val, ncx)
}

/// Get enumerable own property keys from object/proxy
fn get_enumerable_keys(value: &Value, ncx: &mut NativeContext) -> Result<Vec<String>, VmError> {
    // Helper to convert PropertyKey to string representation
    fn key_to_string(k: PropertyKey) -> String {
        match k {
            PropertyKey::String(s) => s.as_str().to_string(),
            PropertyKey::Index(i) => i.to_string(),
            PropertyKey::Symbol(_) => String::new(), // Symbols not included in enumerable keys
        }
    }

    if let Some(proxy) = value.as_proxy() {
        // proxy_own_keys returns Vec<PropertyKey>
        let keys = crate::proxy_operations::proxy_own_keys(ncx, proxy)?;
        return Ok(keys
            .into_iter()
            .map(key_to_string)
            .filter(|s| !s.is_empty())
            .collect());
    }
    if let Some(obj) = value.as_object().or_else(|| value.as_array()) {
        let keys = obj.own_keys();
        return Ok(keys
            .into_iter()
            .map(key_to_string)
            .filter(|s| !s.is_empty())
            .collect());
    }
    Ok(Vec::new())
}

/// Delete property during reviver walk (handles proxies)
fn delete_reviver_property(
    value: &Value,
    key: &PropertyKey,
    key_val: Value,
    ncx: &mut NativeContext,
) -> Result<(), VmError> {
    if let Some(proxy) = value.as_proxy() {
        crate::proxy_operations::proxy_delete_property(ncx, proxy, key, key_val)?;
    } else if let Some(obj) = value.as_object().or_else(|| value.as_array()) {
        obj.delete(key);
    }
    Ok(())
}

/// CreateDataProperty - creates data property, triggering proxy traps if applicable
/// Per spec, this fails silently for non-configurable properties
fn create_data_property(
    value: &Value,
    key: &PropertyKey,
    key_val: Value,
    new_value: Value,
    ncx: &mut NativeContext,
) -> Result<(), VmError> {
    if let Some(proxy) = value.as_proxy() {
        // Use defineProperty trap to create data property
        let desc = PropertyDescriptor::Data {
            value: new_value,
            attributes: PropertyAttributes {
                writable: true,
                enumerable: true,
                configurable: true,
            },
        };
        crate::proxy_operations::proxy_define_property(ncx, proxy, key, key_val, &desc)?;
    } else if let Some(obj) = value.as_object().or_else(|| value.as_array()) {
        // Check if property is non-configurable - if so, CreateDataProperty fails silently
        // We need to check both Index key and String key forms for array indices
        let existing_desc = obj.get_own_property_descriptor(key);

        if let Some(desc) = existing_desc
            && !desc.is_configurable()
        {
            // Cannot redefine non-configurable property - fail silently
            return Ok(());
        }

        let _ = obj.set(*key, new_value);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_table_covers_special_chars() {
        assert_ne!(ESCAPE_TABLE[b'"' as usize], 0);
        assert_ne!(ESCAPE_TABLE[b'\\' as usize], 0);
        assert_ne!(ESCAPE_TABLE[b'\n' as usize], 0);
        assert_ne!(ESCAPE_TABLE[b'\r' as usize], 0);
        assert_ne!(ESCAPE_TABLE[b'\t' as usize], 0);
        assert_eq!(ESCAPE_TABLE[b'a' as usize], 0); // safe
        assert_eq!(ESCAPE_TABLE[b' ' as usize], 0); // safe (0x20)
    }

    #[test]
    fn hex4_formats_correctly() {
        let h = hex4(0x0A);
        assert_eq!(&h, b"\\u000a");
        let h = hex4(0x1F);
        assert_eq!(&h, b"\\u001f");
    }
}
