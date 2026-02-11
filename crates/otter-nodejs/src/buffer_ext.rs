//! Native `node:buffer` extension.
//!
//! Buffer extends Uint8Array with Node.js-specific encoding/decoding methods.
//! Prototype chain: Buffer.prototype → Uint8Array.prototype → TypedArray.prototype → Object.prototype
//!
//! All methods use `#[js_class]` / `#[js_static]` / `#[js_method]` macros.

use std::sync::Arc;

use otter_macros::{js_class, js_method, js_static};
use otter_vm_core::builtin_builder::BuiltInBuilder;
use otter_vm_core::context::NativeContext;
use otter_vm_core::error::VmError;
use otter_vm_core::gc::GcRef;
use otter_vm_core::object::{JsObject, PropertyAttributes, PropertyKey};
use otter_vm_core::string::JsString;
use otter_vm_core::typed_array::{JsTypedArray, TypedArrayKind};
use otter_vm_core::value::Value;
use otter_vm_runtime::extension_v2::{OtterExtension, Profile};
use otter_vm_runtime::registration::RegistrationContext;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const MAX_BUFFER_SIZE: usize = 2 * 1024 * 1024 * 1024; // 2 GB

// ---------------------------------------------------------------------------
// Encoding helpers (reused from buffer.rs via inline impl)
// ---------------------------------------------------------------------------

fn decode_utf8(s: &str) -> Vec<u8> {
    s.as_bytes().to_vec()
}

fn decode_hex(hex: &str) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(hex.len() / 2);
    let mut i = 0;
    let chars: Vec<char> = hex.chars().collect();
    while i + 1 < chars.len() {
        if let Ok(b) = u8::from_str_radix(&hex[i..i + 2], 16) {
            bytes.push(b);
            i += 2;
        } else {
            break;
        }
    }
    bytes
}

fn decode_base64(s: &str) -> Vec<u8> {
    base64::Engine::decode(&base64::engine::general_purpose::STANDARD, s).unwrap_or_default()
}

fn decode_latin1(s: &str) -> Vec<u8> {
    s.bytes().collect()
}

fn decode_ascii(s: &str) -> Vec<u8> {
    s.bytes().map(|b| b & 0x7f).collect()
}

fn encode_utf8(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).to_string()
}

fn encode_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn encode_base64(bytes: &[u8]) -> String {
    base64::Engine::encode(&base64::engine::general_purpose::STANDARD, bytes)
}

fn encode_latin1(bytes: &[u8]) -> String {
    bytes.iter().map(|&b| b as char).collect()
}

fn parse_encoding(val: Option<&Value>) -> String {
    val.and_then(|v| v.as_string())
        .map(|s| s.as_str().to_string())
        .unwrap_or_else(|| "utf8".to_string())
}

fn decode_string(s: &str, encoding: &str) -> Vec<u8> {
    match encoding {
        "utf8" | "utf-8" => decode_utf8(s),
        "hex" => decode_hex(s),
        "base64" => decode_base64(s),
        "latin1" | "binary" => decode_latin1(s),
        "ascii" => decode_ascii(s),
        _ => decode_utf8(s),
    }
}

fn encode_bytes(bytes: &[u8], encoding: &str) -> String {
    match encoding {
        "utf8" | "utf-8" => encode_utf8(bytes),
        "hex" => encode_hex(bytes),
        "base64" => encode_base64(bytes),
        "latin1" | "binary" => encode_latin1(bytes),
        "ascii" => encode_latin1(bytes),
        _ => encode_utf8(bytes),
    }
}

/// Extract raw bytes from a TypedArray value.
fn get_buffer_bytes(val: &Value) -> Option<Vec<u8>> {
    let ta = val.as_typed_array()?;
    let len = ta.length();
    let mut bytes = Vec::with_capacity(len);
    for i in 0..len {
        bytes.push(ta.get(i)? as u8);
    }
    Some(bytes)
}

/// Create a Buffer (Uint8Array) value from raw bytes, using ncx to get prototypes.
fn create_buffer_from_bytes(bytes: &[u8], ncx: &NativeContext) -> Value {
    let mm = ncx.memory_manager().clone();

    // Get Buffer.prototype from globalThis.Buffer.prototype
    // This gives us our custom methods (toString, write, etc.) AND
    // inherits from Uint8Array.prototype for TypedArray methods.
    let buffer_proto = ncx
        .global()
        .get(&PropertyKey::string("Buffer"))
        .and_then(|v| v.as_object())
        .and_then(|ctor| ctor.get(&PropertyKey::string("prototype")))
        .and_then(|v| v.as_object());

    let ta = JsTypedArray::with_length(TypedArrayKind::Uint8, bytes.len(), buffer_proto, mm);

    for (i, &b) in bytes.iter().enumerate() {
        ta.set(i, b as f64);
    }

    // Mark as Buffer for isBuffer() detection
    let _ = ta
        .object
        .set(PropertyKey::string("__is_buffer"), Value::boolean(true));

    Value::typed_array(GcRef::new(ta))
}

// ---------------------------------------------------------------------------
// Buffer class methods via #[js_class]
// ---------------------------------------------------------------------------

#[js_class(name = "Buffer")]
pub struct Buffer;

#[js_class]
impl Buffer {
    // --- Static methods ---

    #[js_static(name = "alloc", length = 1)]
    pub fn alloc(_this: &Value, args: &[Value], ncx: &mut NativeContext) -> Result<Value, VmError> {
        let size = args
            .first()
            .and_then(|v| v.as_number())
            .map(|n| n as usize)
            .unwrap_or(0);

        if size > MAX_BUFFER_SIZE {
            return Err(VmError::range_error("Buffer size exceeds maximum"));
        }

        let fill_val = args.get(1).and_then(|v| v.as_number()).unwrap_or(0.0) as u8;
        let bytes = vec![fill_val; size];
        Ok(create_buffer_from_bytes(&bytes, ncx))
    }

    #[js_static(name = "allocUnsafe", length = 1)]
    pub fn alloc_unsafe(
        _this: &Value,
        args: &[Value],
        ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        // Same as alloc but uninitialized (we zero-fill for safety)
        let size = args
            .first()
            .and_then(|v| v.as_number())
            .map(|n| n as usize)
            .unwrap_or(0);

        if size > MAX_BUFFER_SIZE {
            return Err(VmError::range_error("Buffer size exceeds maximum"));
        }

        let bytes = vec![0u8; size];
        Ok(create_buffer_from_bytes(&bytes, ncx))
    }

    #[js_static(name = "from", length = 1)]
    pub fn from(_this: &Value, args: &[Value], ncx: &mut NativeContext) -> Result<Value, VmError> {
        let source = args.first().cloned().unwrap_or(Value::undefined());

        // Buffer.from(string, encoding?)
        if let Some(s) = source.as_string() {
            let encoding = parse_encoding(args.get(1));
            let bytes = decode_string(s.as_str(), &encoding);
            return Ok(create_buffer_from_bytes(&bytes, ncx));
        }

        // Buffer.from(typedArray) — copy bytes
        if let Some(bytes) = get_buffer_bytes(&source) {
            return Ok(create_buffer_from_bytes(&bytes, ncx));
        }

        // Buffer.from(array) — array of numbers
        if let Some(obj) = source.as_object() {
            if obj.is_array() {
                let len = obj.array_length();
                let mut bytes = Vec::with_capacity(len);
                for i in 0..len {
                    let val = obj
                        .get(&PropertyKey::Index(i as u32))
                        .unwrap_or(Value::number(0.0));
                    let b = val.as_number().unwrap_or(0.0) as u8;
                    bytes.push(b);
                }
                return Ok(create_buffer_from_bytes(&bytes, ncx));
            }
        }

        // Buffer.from(arrayBuffer, byteOffset?, length?)
        if let Some(ab) = source.as_array_buffer() {
            let offset = args.get(1).and_then(|v| v.as_number()).unwrap_or(0.0) as usize;
            let ab_len = ab.byte_length();
            let length = args
                .get(2)
                .and_then(|v| v.as_number())
                .map(|n| n as usize)
                .unwrap_or(ab_len.saturating_sub(offset));

            let mut bytes = vec![0u8; length];
            ab.read_bytes(offset, &mut bytes);
            return Ok(create_buffer_from_bytes(&bytes, ncx));
        }

        Err(VmError::type_error(
            "The first argument must be a string, Buffer, ArrayBuffer, Array, or array-like object",
        ))
    }

    #[js_static(name = "isBuffer", length = 1)]
    pub fn is_buffer(
        _this: &Value,
        args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        let val = args.first().cloned().unwrap_or(Value::undefined());
        // A Buffer is a Uint8Array with our marker
        let is_buf = val.as_typed_array().is_some_and(|ta| {
            ta.kind() == TypedArrayKind::Uint8
                && ta.object.get(&PropertyKey::string("__is_buffer")).is_some()
        });
        Ok(Value::boolean(is_buf))
    }

    #[js_static(name = "byteLength", length = 1)]
    pub fn byte_length_static(
        _this: &Value,
        args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        let val = args.first().cloned().unwrap_or(Value::undefined());
        let encoding = parse_encoding(args.get(1));

        if let Some(s) = val.as_string() {
            let bytes = decode_string(s.as_str(), &encoding);
            return Ok(Value::number(bytes.len() as f64));
        }

        if let Some(ta) = val.as_typed_array() {
            return Ok(Value::number(ta.byte_length() as f64));
        }

        if let Some(ab) = val.as_array_buffer() {
            return Ok(Value::number(ab.byte_length() as f64));
        }

        Ok(Value::number(0.0))
    }

    #[js_static(name = "concat", length = 1)]
    pub fn concat(
        _this: &Value,
        args: &[Value],
        ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        let list = args.first().cloned().unwrap_or(Value::undefined());
        let total_length = args.get(1).and_then(|v| v.as_number());

        let arr = list
            .as_object()
            .ok_or_else(|| VmError::type_error("Buffer.concat: list must be an array"))?;

        let len = arr.array_length();
        let mut all_bytes = Vec::new();

        for i in 0..len {
            if let Some(item) = arr.get(&PropertyKey::Index(i as u32)) {
                if let Some(bytes) = get_buffer_bytes(&item) {
                    all_bytes.extend_from_slice(&bytes);
                }
            }
        }

        if let Some(max_len) = total_length {
            let max = max_len as usize;
            if all_bytes.len() > max {
                all_bytes.truncate(max);
            } else {
                all_bytes.resize(max, 0);
            }
        }

        Ok(create_buffer_from_bytes(&all_bytes, ncx))
    }

    #[js_static(name = "isEncoding", length = 1)]
    pub fn is_encoding(
        _this: &Value,
        args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        let val = args.first().and_then(|v| v.as_string());
        let is_valid = val.is_some_and(|s| {
            matches!(
                s.as_str(),
                "utf8"
                    | "utf-8"
                    | "hex"
                    | "base64"
                    | "ascii"
                    | "latin1"
                    | "binary"
                    | "ucs2"
                    | "ucs-2"
                    | "utf16le"
                    | "utf-16le"
            )
        });
        Ok(Value::boolean(is_valid))
    }

    #[js_static(name = "compare", length = 2)]
    pub fn compare_static(
        _this: &Value,
        args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        let a = args.first().cloned().unwrap_or(Value::undefined());
        let b = args.get(1).cloned().unwrap_or(Value::undefined());
        let a_bytes = get_buffer_bytes(&a).ok_or_else(|| {
            VmError::type_error("Buffer.compare: first argument must be a Buffer")
        })?;
        let b_bytes = get_buffer_bytes(&b).ok_or_else(|| {
            VmError::type_error("Buffer.compare: second argument must be a Buffer")
        })?;

        let result = a_bytes.cmp(&b_bytes);
        Ok(Value::number(match result {
            std::cmp::Ordering::Less => -1.0,
            std::cmp::Ordering::Equal => 0.0,
            std::cmp::Ordering::Greater => 1.0,
        }))
    }

    // --- Instance methods ---

    #[js_method(name = "toString", length = 0)]
    pub fn to_string_method(
        this: &Value,
        args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        let bytes = get_buffer_bytes(this)
            .ok_or_else(|| VmError::type_error("Buffer.prototype.toString: not a Buffer"))?;

        let encoding = parse_encoding(args.first());
        let start = args.get(1).and_then(|v| v.as_number()).unwrap_or(0.0) as usize;
        let end = args
            .get(2)
            .and_then(|v| v.as_number())
            .map(|n| n as usize)
            .unwrap_or(bytes.len());

        let start = start.min(bytes.len());
        let end = end.min(bytes.len());
        let slice = &bytes[start..end];

        let result = encode_bytes(slice, &encoding);
        Ok(Value::string(JsString::new_gc(&result)))
    }

    #[js_method(name = "write", length = 1)]
    pub fn write(this: &Value, args: &[Value], _ncx: &mut NativeContext) -> Result<Value, VmError> {
        let ta = this
            .as_typed_array()
            .ok_or_else(|| VmError::type_error("Buffer.prototype.write: not a Buffer"))?;

        let string = args
            .first()
            .and_then(|v| v.as_string())
            .ok_or_else(|| VmError::type_error("Buffer.prototype.write: string required"))?;

        let offset = args.get(1).and_then(|v| v.as_number()).unwrap_or(0.0) as usize;
        let encoding = parse_encoding(args.get(3));
        let src_bytes = decode_string(string.as_str(), &encoding);

        let buf_len = ta.length();
        let max_write = args
            .get(2)
            .and_then(|v| v.as_number())
            .map(|n| n as usize)
            .unwrap_or(buf_len.saturating_sub(offset));

        let write_len = src_bytes
            .len()
            .min(max_write)
            .min(buf_len.saturating_sub(offset));

        for i in 0..write_len {
            ta.set(offset + i, src_bytes[i] as f64);
        }

        Ok(Value::number(write_len as f64))
    }

    #[js_method(name = "copy", length = 1)]
    pub fn copy(this: &Value, args: &[Value], _ncx: &mut NativeContext) -> Result<Value, VmError> {
        let src = this
            .as_typed_array()
            .ok_or_else(|| VmError::type_error("Buffer.prototype.copy: not a Buffer"))?;
        let target = args
            .first()
            .and_then(|v| v.as_typed_array())
            .ok_or_else(|| VmError::type_error("Buffer.prototype.copy: target must be a Buffer"))?;

        let target_start = args.get(1).and_then(|v| v.as_number()).unwrap_or(0.0) as usize;
        let source_start = args.get(2).and_then(|v| v.as_number()).unwrap_or(0.0) as usize;
        let source_end = args
            .get(3)
            .and_then(|v| v.as_number())
            .map(|n| n as usize)
            .unwrap_or(src.length());

        let source_end = source_end.min(src.length());
        let source_start = source_start.min(source_end);
        let copy_len = source_end - source_start;
        let actual_copy = copy_len.min(target.length().saturating_sub(target_start));

        for i in 0..actual_copy {
            if let Some(b) = src.get(source_start + i) {
                target.set(target_start + i, b);
            }
        }

        Ok(Value::number(actual_copy as f64))
    }

    #[js_method(name = "fill", length = 1)]
    pub fn fill_method(
        this: &Value,
        args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        let ta = this
            .as_typed_array()
            .ok_or_else(|| VmError::type_error("Buffer.prototype.fill: not a Buffer"))?;

        let fill_val = args.first().cloned().unwrap_or(Value::number(0.0));
        let offset = args.get(1).and_then(|v| v.as_number());
        let end = args.get(2).and_then(|v| v.as_number());

        if let Some(n) = fill_val.as_number() {
            let byte_val = n as u8 as f64;
            ta.fill(byte_val, offset.map(|n| n as i64), end.map(|n| n as i64));
        } else if let Some(s) = fill_val.as_string() {
            let encoding = parse_encoding(args.get(3));
            let fill_bytes = decode_string(s.as_str(), &encoding);
            if !fill_bytes.is_empty() {
                let len = ta.length();
                let start = offset.unwrap_or(0.0) as usize;
                let end_idx = end.map(|n| n as usize).unwrap_or(len);
                let end_idx = end_idx.min(len);
                let mut j = 0;
                for i in start..end_idx {
                    ta.set(i, fill_bytes[j % fill_bytes.len()] as f64);
                    j += 1;
                }
            }
        }

        Ok(this.clone())
    }

    #[js_method(name = "slice", length = 0)]
    pub fn slice(this: &Value, args: &[Value], ncx: &mut NativeContext) -> Result<Value, VmError> {
        let bytes = get_buffer_bytes(this)
            .ok_or_else(|| VmError::type_error("Buffer.prototype.slice: not a Buffer"))?;

        let len = bytes.len() as i64;
        let start = args.first().and_then(|v| v.as_number()).unwrap_or(0.0) as i64;
        let end = args
            .get(1)
            .and_then(|v| v.as_number())
            .map(|n| n as i64)
            .unwrap_or(len);

        let start = if start < 0 {
            (len + start).max(0)
        } else {
            start.min(len)
        } as usize;
        let end = if end < 0 {
            (len + end).max(0)
        } else {
            end.min(len)
        } as usize;
        let end = end.max(start);

        let slice_bytes = &bytes[start..end];
        Ok(create_buffer_from_bytes(slice_bytes, ncx))
    }

    #[js_method(name = "indexOf", length = 1)]
    pub fn index_of(
        this: &Value,
        args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        let bytes = get_buffer_bytes(this)
            .ok_or_else(|| VmError::type_error("Buffer.prototype.indexOf: not a Buffer"))?;

        let search = args.first().cloned().unwrap_or(Value::undefined());
        let byte_offset = args.get(1).and_then(|v| v.as_number()).unwrap_or(0.0) as usize;
        let byte_offset = byte_offset.min(bytes.len());

        if let Some(n) = search.as_number() {
            let needle = n as u8;
            for (i, &b) in bytes[byte_offset..].iter().enumerate() {
                if b == needle {
                    return Ok(Value::number((byte_offset + i) as f64));
                }
            }
        } else if let Some(s) = search.as_string() {
            let encoding = parse_encoding(args.get(2));
            let needle = decode_string(s.as_str(), &encoding);
            if !needle.is_empty() && needle.len() <= bytes.len() - byte_offset {
                for i in byte_offset..=bytes.len() - needle.len() {
                    if bytes[i..i + needle.len()] == needle[..] {
                        return Ok(Value::number(i as f64));
                    }
                }
            }
        } else if let Some(needle_bytes) = get_buffer_bytes(&search) {
            if !needle_bytes.is_empty() && needle_bytes.len() <= bytes.len() - byte_offset {
                for i in byte_offset..=bytes.len() - needle_bytes.len() {
                    if bytes[i..i + needle_bytes.len()] == needle_bytes[..] {
                        return Ok(Value::number(i as f64));
                    }
                }
            }
        }

        Ok(Value::number(-1.0))
    }

    #[js_method(name = "lastIndexOf", length = 1)]
    pub fn last_index_of(
        this: &Value,
        args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        let bytes = get_buffer_bytes(this)
            .ok_or_else(|| VmError::type_error("Buffer.prototype.lastIndexOf: not a Buffer"))?;
        if bytes.is_empty() {
            return Ok(Value::number(-1.0));
        }

        let search = args.first().cloned().unwrap_or(Value::undefined());
        let offset = args
            .get(1)
            .and_then(|v| v.as_number())
            .map(|n| n as i64)
            .unwrap_or((bytes.len() - 1) as i64);
        let encoding = parse_encoding(args.get(2));

        let mut start = if offset < 0 {
            (bytes.len() as i64 + offset).max(0) as usize
        } else {
            (offset as usize).min(bytes.len() - 1)
        };

        let needle = if let Some(n) = search.as_number() {
            vec![n as u8]
        } else if let Some(s) = search.as_string() {
            decode_string(s.as_str(), &encoding)
        } else if let Some(buf) = get_buffer_bytes(&search) {
            buf
        } else {
            Vec::new()
        };

        if needle.is_empty() || needle.len() > bytes.len() {
            return Ok(Value::number(-1.0));
        }

        if start + 1 < needle.len() {
            start = needle.len() - 1;
        }

        let max_start = bytes.len() - needle.len();
        let start = start.min(max_start);
        for i in (0..=start).rev() {
            if bytes[i..i + needle.len()] == needle[..] {
                return Ok(Value::number(i as f64));
            }
        }

        Ok(Value::number(-1.0))
    }

    #[js_method(name = "includes", length = 1)]
    pub fn includes(
        this: &Value,
        args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        let bytes = get_buffer_bytes(this)
            .ok_or_else(|| VmError::type_error("Buffer.prototype.includes: not a Buffer"))?;
        let search = args.first().cloned().unwrap_or(Value::undefined());
        let byte_offset = args.get(1).and_then(|v| v.as_number()).unwrap_or(0.0) as usize;
        let byte_offset = byte_offset.min(bytes.len());
        let encoding = parse_encoding(args.get(2));

        let needle = if let Some(n) = search.as_number() {
            vec![n as u8]
        } else if let Some(s) = search.as_string() {
            decode_string(s.as_str(), &encoding)
        } else if let Some(buf) = get_buffer_bytes(&search) {
            buf
        } else {
            Vec::new()
        };

        if needle.is_empty() || needle.len() > bytes.len().saturating_sub(byte_offset) {
            return Ok(Value::boolean(false));
        }

        for i in byte_offset..=bytes.len() - needle.len() {
            if bytes[i..i + needle.len()] == needle[..] {
                return Ok(Value::boolean(true));
            }
        }

        Ok(Value::boolean(false))
    }

    #[js_method(name = "compare", length = 1)]
    pub fn compare(
        this: &Value,
        args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        let a = get_buffer_bytes(this)
            .ok_or_else(|| VmError::type_error("Buffer.prototype.compare: not a Buffer"))?;
        let b_val = args.first().cloned().unwrap_or(Value::undefined());
        let b = get_buffer_bytes(&b_val).ok_or_else(|| {
            VmError::type_error("Buffer.prototype.compare: argument must be a Buffer")
        })?;

        let result = a.cmp(&b);
        Ok(Value::number(match result {
            std::cmp::Ordering::Less => -1.0,
            std::cmp::Ordering::Equal => 0.0,
            std::cmp::Ordering::Greater => 1.0,
        }))
    }

    #[js_method(name = "equals", length = 1)]
    pub fn equals(
        this: &Value,
        args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        let a = get_buffer_bytes(this)
            .ok_or_else(|| VmError::type_error("Buffer.prototype.equals: not a Buffer"))?;
        let b_val = args.first().cloned().unwrap_or(Value::undefined());
        let b = get_buffer_bytes(&b_val).ok_or_else(|| {
            VmError::type_error("Buffer.prototype.equals: argument must be a Buffer")
        })?;

        Ok(Value::boolean(a == b))
    }

    #[js_method(name = "toJSON", length = 0)]
    pub fn to_json(
        this: &Value,
        _args: &[Value],
        ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        let bytes = get_buffer_bytes(this)
            .ok_or_else(|| VmError::type_error("Buffer.prototype.toJSON: not a Buffer"))?;

        let mm = ncx.memory_manager().clone();
        let result = GcRef::new(JsObject::new(Value::null(), mm.clone()));
        let _ = result.set(
            PropertyKey::string("type"),
            Value::string(JsString::intern("Buffer")),
        );

        let data_arr = GcRef::new(JsObject::array(0, mm));
        for &b in &bytes {
            data_arr.array_push(Value::number(b as f64));
        }
        let _ = result.set(PropertyKey::string("data"), Value::object(data_arr));

        Ok(Value::object(result))
    }
}

pub fn atob(_this: &Value, args: &[Value], _ncx: &mut NativeContext) -> Result<Value, VmError> {
    let s = args
        .first()
        .and_then(|v| v.as_string())
        .map(|s| s.as_str().to_string())
        .unwrap_or_default();
    let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, s)
        .map_err(|_| VmError::type_error("Invalid atob input"))?;
    let result: String = bytes.iter().map(|&b| b as char).collect();
    Ok(Value::string(JsString::new_gc(&result)))
}

pub fn btoa(_this: &Value, args: &[Value], _ncx: &mut NativeContext) -> Result<Value, VmError> {
    let s = args
        .first()
        .and_then(|v| v.as_string())
        .map(|s| s.as_str().to_string())
        .unwrap_or_default();
    let bytes: Vec<u8> = s.chars().map(|c| c as u8).collect();
    let result = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, bytes);
    Ok(Value::string(JsString::new_gc(&result)))
}

// ---------------------------------------------------------------------------
// OtterExtension
// ---------------------------------------------------------------------------

pub struct NodeBufferExtension;

impl OtterExtension for NodeBufferExtension {
    fn name(&self) -> &str {
        "node_buffer"
    }

    fn profiles(&self) -> &[Profile] {
        static P: [Profile; 2] = [Profile::SafeCore, Profile::Full];
        &P
    }

    fn deps(&self) -> &[&str] {
        &[]
    }

    fn module_specifiers(&self) -> &[&str] {
        static S: [&str; 2] = ["node:buffer", "buffer"];
        &S
    }

    fn install(&self, ctx: &mut RegistrationContext) -> Result<(), VmError> {
        // Build Buffer class and set as global
        let ctor = build_buffer_class(ctx);
        ctx.global_value("Buffer", ctor);

        // Install atob/btoa as globals
        let mm = ctx.mm().clone();
        ctx.global_value("atob", Value::native_function(atob, mm.clone()));
        ctx.global_value("btoa", Value::native_function(btoa, mm));

        Ok(())
    }

    fn load_module(
        &self,
        _specifier: &str,
        ctx: &mut RegistrationContext,
    ) -> Option<GcRef<JsObject>> {
        let ctor = ctx.global().get(&PropertyKey::string("Buffer"))?;
        let atob_fn = ctx.global().get(&PropertyKey::string("atob"))?;
        let btoa_fn = ctx.global().get(&PropertyKey::string("btoa"))?;

        let ns = ctx
            .module_namespace()
            .property("default", ctor.clone())
            .property("Buffer", ctor)
            .property("atob", atob_fn)
            .property("btoa", btoa_fn)
            .build();

        Some(ns)
    }
}

pub fn node_buffer_extension() -> Box<dyn OtterExtension> {
    Box::new(NodeBufferExtension)
}

// ---------------------------------------------------------------------------
// Build the Buffer class using BuiltInBuilder
// ---------------------------------------------------------------------------

fn build_buffer_class(ctx: &RegistrationContext) -> Value {
    type DeclFn = fn() -> (
        &'static str,
        Arc<dyn Fn(&Value, &[Value], &mut NativeContext) -> Result<Value, VmError> + Send + Sync>,
        u32,
    );

    // Static methods
    let static_methods: &[DeclFn] = &[
        Buffer::alloc_decl,
        Buffer::alloc_unsafe_decl,
        Buffer::from_decl,
        Buffer::is_buffer_decl,
        Buffer::byte_length_static_decl,
        Buffer::concat_decl,
        Buffer::is_encoding_decl,
        Buffer::compare_static_decl,
    ];

    // Prototype (instance) methods
    let proto_methods: &[DeclFn] = &[
        Buffer::to_string_method_decl,
        Buffer::write_decl,
        Buffer::copy_decl,
        Buffer::fill_method_decl,
        Buffer::slice_decl,
        Buffer::index_of_decl,
        Buffer::last_index_of_decl,
        Buffer::includes_decl,
        Buffer::compare_decl,
        Buffer::equals_decl,
        Buffer::to_json_decl,
    ];

    // Buffer.prototype → Uint8Array.prototype (inherits TypedArray methods)
    let uint8_proto = ctx.intrinsics().uint8_array_prototype;
    let mm = ctx.mm().clone();
    let fn_proto = ctx.fn_proto();

    // Create Buffer.prototype with Uint8Array.prototype as its [[Prototype]]
    let buffer_proto = GcRef::new(JsObject::new(Value::object(uint8_proto), mm.clone()));
    // Create Buffer constructor with Function.prototype as its [[Prototype]]
    let buffer_ctor = GcRef::new(JsObject::new(Value::object(fn_proto), mm.clone()));

    let mut builder = BuiltInBuilder::new(mm, fn_proto, buffer_ctor, buffer_proto, "Buffer")
        .constructor_fn(
            |this, args, _ncx| {
                // Buffer(size) - deprecated but supported
                if let Some(n) = args.first().and_then(|v| v.as_number()) {
                    let size = n as usize;
                    if size > MAX_BUFFER_SIZE {
                        return Err(VmError::range_error("Buffer size exceeds maximum"));
                    }
                    if let Some(obj) = this.as_object() {
                        let _ = obj.set(PropertyKey::string("__is_buffer"), Value::boolean(true));
                    }
                }
                Ok(Value::undefined())
            },
            1,
        );

    for decl in proto_methods {
        let (name, func, length) = decl();
        builder = builder.method_native(name, func, length);
    }

    for decl in static_methods {
        let (name, func, length) = decl();
        builder = builder.static_method_native(name, func, length);
    }

    builder = builder.static_property(
        PropertyKey::string("poolSize"),
        Value::number(8192.0),
        PropertyAttributes::builtin_method(),
    );

    builder.build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decode_hex() {
        assert_eq!(decode_hex("48656c6c6f"), vec![72, 101, 108, 108, 111]);
        assert_eq!(decode_hex(""), Vec::<u8>::new());
    }

    #[test]
    fn test_encode_hex() {
        assert_eq!(encode_hex(&[72, 101, 108, 108, 111]), "48656c6c6f");
    }

    #[test]
    fn test_decode_string_utf8() {
        assert_eq!(decode_string("hello", "utf8"), b"hello");
    }

    #[test]
    fn test_encode_bytes_utf8() {
        assert_eq!(encode_bytes(b"hello", "utf8"), "hello");
    }

    #[test]
    fn test_buffer_metadata() {
        assert_eq!(Buffer::JS_CLASS_NAME, "Buffer");
    }

    #[test]
    fn test_buffer_decl_functions() {
        let (name, _func, length) = Buffer::alloc_decl();
        assert_eq!(name, "alloc");
        assert_eq!(length, 1);

        let (name, _func, length) = Buffer::from_decl();
        assert_eq!(name, "from");
        assert_eq!(length, 1);

        let (name, _func, length) = Buffer::to_string_method_decl();
        assert_eq!(name, "toString");
        assert_eq!(length, 0);

        let (name, _func, length) = Buffer::is_buffer_decl();
        assert_eq!(name, "isBuffer");
        assert_eq!(length, 1);
    }
}
