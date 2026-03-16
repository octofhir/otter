//! TypedArray exotic object internal methods (ES2026 §10.4.5)
//!
//! Implements the seven exotic internal methods for Integer-Indexed objects:
//! - [[Get]], [[Set]], [[HasProperty]], [[OwnPropertyKeys]]
//! - [[GetOwnProperty]], [[DefineOwnProperty]], [[Delete]]
//!
//! These are pure data operations callable from both interpreter and JIT helpers.
//! No NativeContext dependency — only takes GcRef<JsTypedArray> + PropertyKey.

use crate::gc::GcRef;
use crate::object::{PropertyAttributes, PropertyDescriptor, PropertyKey};
use crate::string::JsString;
use crate::typed_array::JsTypedArray;
use crate::value::Value;

// ============================================================================
// Result enums for exotic operations
// ============================================================================

/// Result of TypedArray [[Set]] exotic method.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaSetResult {
    /// Value was written successfully.
    Written,
    /// Index was out of bounds (silently ignored per spec in non-strict).
    OutOfBounds,
    /// Key was not a canonical numeric index — caller should fall through to named properties.
    NotAnIndex,
    /// Buffer is detached — caller should throw TypeError.
    Detached,
}

/// Result of TypedArray [[HasProperty]] exotic method.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaHasResult {
    /// Numeric index is in bounds — property exists.
    Present,
    /// Numeric index is out of bounds or buffer detached — property absent.
    Absent,
    /// Key was not a canonical numeric index — caller should fall through.
    NotAnIndex,
}

/// Result of TypedArray [[DefineOwnProperty]] exotic method.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaDefineResult {
    /// Property defined successfully (or was a no-op compatible change).
    Ok,
    /// Define was rejected (non-configurable mismatch, etc.).
    Rejected,
    /// Key was not a canonical numeric index — caller should fall through.
    NotAnIndex,
}

// ============================================================================
// CanonicalNumericIndexString (ES2026 §7.1.21)
// ============================================================================

/// Parse a PropertyKey as a canonical numeric index for TypedArray access.
///
/// Returns `Some(index)` if the key is a valid non-negative integer index.
/// Returns `None` if the key is not a canonical numeric index (caller should
/// fall through to named property access on `ta.object`).
///
/// Per spec, "-0" IS a canonical numeric index but is NOT a valid TypedArray
/// index, so callers must handle the `-0` case separately if needed.
/// For simplicity, we return `None` for "-0" since it's never a valid index.
pub fn canonical_numeric_index(key: &PropertyKey) -> Option<usize> {
    match key {
        PropertyKey::Index(i) => Some(*i as usize),
        PropertyKey::String(s) => canonical_numeric_index_str(s.as_str()),
        PropertyKey::Symbol(_) => None,
    }
}

/// Parse a &str as a canonical numeric index.
/// Used by interpreter for string-keyed access like `ta["0"]`.
pub fn canonical_numeric_index_str(s: &str) -> Option<usize> {
    if s.is_empty() || s == "-0" {
        return None;
    }
    // Fast path: single ASCII digit
    if s.len() == 1 {
        let b = s.as_bytes()[0];
        if b.is_ascii_digit() {
            return Some((b - b'0') as usize);
        }
        return None;
    }
    // Reject leading zeros (except "0" handled above via single-digit path)
    if s.starts_with('0') {
        return None;
    }
    // Must be all digits, parse as usize
    s.parse::<usize>().ok()
}

/// Parse a UTF-16 name as a canonical numeric index.
/// Used by GetPropConst slow path.
pub fn canonical_numeric_index_utf16(name: &[u16]) -> Option<usize> {
    // Fast path: single digit
    if name.len() == 1 {
        let ch = name[0];
        if (b'0' as u16..=b'9' as u16).contains(&ch) {
            return Some((ch - b'0' as u16) as usize);
        }
        return None;
    }
    if name.is_empty() {
        return None;
    }
    // "-0" check
    if name.len() == 2 && name[0] == b'-' as u16 && name[1] == b'0' as u16 {
        return None;
    }
    // Reject leading zeros
    if name[0] == b'0' as u16 {
        return None;
    }
    // All chars must be ASCII digits
    let mut result: usize = 0;
    for &ch in name {
        if !(b'0' as u16..=b'9' as u16).contains(&ch) {
            return None;
        }
        result = result.checked_mul(10)?.checked_add((ch - b'0' as u16) as usize)?;
    }
    Some(result)
}

// ============================================================================
// §10.4.5.4 [[Get]] (P, Receiver)
// ============================================================================

/// TypedArray [[Get]] for numeric indices.
///
/// Returns `Some(value)` if key is a valid in-bounds numeric index.
/// Returns `Some(Value::undefined())` if key is a numeric index but out of bounds or detached.
/// Returns `None` if key is not a numeric index (caller must fall through to `ta.object`).
pub fn ta_get(ta: &GcRef<JsTypedArray>, key: &PropertyKey) -> Option<Value> {
    let idx = canonical_numeric_index(key)?;
    // Numeric index: always handle here, never fall through
    Some(ta.get_value(idx).unwrap_or(Value::undefined()))
}

// ============================================================================
// §10.4.5.5 [[Set]] (P, V, Receiver)
// ============================================================================

/// TypedArray [[Set]] for numeric indices.
pub fn ta_set(ta: &GcRef<JsTypedArray>, key: &PropertyKey, value: &Value) -> TaSetResult {
    let idx = match canonical_numeric_index(key) {
        Some(i) => i,
        None => return TaSetResult::NotAnIndex,
    };
    if ta.is_detached() {
        return TaSetResult::Detached;
    }
    if idx >= ta.length() {
        return TaSetResult::OutOfBounds;
    }
    ta.set_value(idx, value);
    TaSetResult::Written
}

// ============================================================================
// §10.4.5.2 [[HasProperty]] (P)
// ============================================================================

/// TypedArray [[HasProperty]] for numeric indices.
pub fn ta_has(ta: &GcRef<JsTypedArray>, key: &PropertyKey) -> TaHasResult {
    let idx = match canonical_numeric_index(key) {
        Some(i) => i,
        None => return TaHasResult::NotAnIndex,
    };
    if ta.is_detached() || idx >= ta.length() {
        TaHasResult::Absent
    } else {
        TaHasResult::Present
    }
}

// ============================================================================
// §10.4.5.1 [[GetOwnProperty]] (P)
// ============================================================================

/// TypedArray [[GetOwnProperty]] for numeric indices.
///
/// Returns `Some(descriptor)` for valid in-bounds indices.
/// Returns `None` if the key is not a numeric index (fall through to `ta.object`).
/// For out-of-bounds or detached, returns `None` (property absent).
pub fn ta_get_own_property(
    ta: &GcRef<JsTypedArray>,
    key: &PropertyKey,
) -> Option<PropertyDescriptor> {
    let idx = canonical_numeric_index(key)?;
    if ta.is_detached() || idx >= ta.length() {
        // Numeric index out of bounds → property does not exist
        // Return Some(Deleted) to signal "handled but absent" vs None = "not a numeric index"
        // Actually per spec: if IsValidIntegerIndex returns false, return undefined.
        // We need a way to distinguish "not an index" from "index but absent".
        // Solution: return None for absent too, but the caller checks canonical_numeric_index first.
        return None;
    }
    let value = ta.get_value(idx)?;
    // Per §10.4.5.1: { value, writable: true, enumerable: true, configurable: true }
    Some(PropertyDescriptor::data_with_attrs(
        value,
        PropertyAttributes {
            writable: true,
            enumerable: true,
            configurable: true,
        },
    ))
}

// ============================================================================
// §10.4.5.3 [[DefineOwnProperty]] (P, Desc)
// ============================================================================

/// TypedArray [[DefineOwnProperty]] for numeric indices.
pub fn ta_define_own_property(
    ta: &GcRef<JsTypedArray>,
    key: &PropertyKey,
    desc: &PropertyDescriptor,
) -> TaDefineResult {
    let idx = match canonical_numeric_index(key) {
        Some(i) => i,
        None => return TaDefineResult::NotAnIndex,
    };
    if ta.is_detached() || idx >= ta.length() {
        return TaDefineResult::Rejected;
    }
    // Per spec: if desc is accessor → rejected.
    // If desc has configurable:false or enumerable:false → rejected.
    // If desc has writable:false → rejected.
    match desc {
        PropertyDescriptor::Accessor { .. } => return TaDefineResult::Rejected,
        PropertyDescriptor::Data { value, attributes } => {
            if !attributes.configurable || !attributes.enumerable || !attributes.writable {
                return TaDefineResult::Rejected;
            }
            ta.set_value(idx, value);
        }
        PropertyDescriptor::Deleted => return TaDefineResult::Rejected,
    }
    TaDefineResult::Ok
}

// ============================================================================
// §10.4.5.6 [[Delete]] (P)
// ============================================================================

/// TypedArray [[Delete]] for numeric indices.
///
/// Returns `Some(false)` if in-bounds index (cannot delete).
/// Returns `Some(true)` if out-of-bounds index (nothing to delete).
/// Returns `None` if not a numeric index (fall through to `ta.object`).
pub fn ta_delete(ta: &GcRef<JsTypedArray>, key: &PropertyKey) -> Option<bool> {
    let idx = canonical_numeric_index(key)?;
    if ta.is_detached() || idx >= ta.length() {
        Some(true) // Out of bounds → "deleted" (nothing was there)
    } else {
        Some(false) // In bounds → cannot delete
    }
}

// ============================================================================
// §10.4.5.7 [[OwnPropertyKeys]]
// ============================================================================

/// TypedArray [[OwnPropertyKeys]].
///
/// Returns numeric indices (as Index keys) in ascending order.
/// Caller should append `ta.object.own_keys()` for string/symbol properties.
pub fn ta_own_keys(ta: &GcRef<JsTypedArray>) -> Vec<PropertyKey> {
    let mut keys = Vec::new();
    if !ta.is_detached() {
        let len = ta.length();
        keys.reserve(len);
        for i in 0..len {
            keys.push(PropertyKey::Index(i as u32));
        }
    }
    keys
}

/// Full [[OwnPropertyKeys]] including named properties from `ta.object`.
/// Numeric indices first, then string keys (excluding internal props), then symbols.
pub fn ta_own_keys_full(ta: &GcRef<JsTypedArray>) -> Vec<PropertyKey> {
    let mut keys = ta_own_keys(ta);
    // Append string/symbol keys from the backing object
    for key in ta.object.own_keys() {
        match &key {
            PropertyKey::Index(_) => {} // already included from buffer indices
            PropertyKey::String(s) => {
                let name = s.as_str();
                // Filter out internal hidden properties
                if name.starts_with("__") && name.ends_with("__") {
                    continue;
                }
                keys.push(key);
            }
            PropertyKey::Symbol(_) => keys.push(key),
        }
    }
    keys
}

// ============================================================================
// Convenience: check if a Value key is a numeric index for TypedArray
// ============================================================================

/// Convert a Value key to a canonical numeric index for TypedArray.
/// Handles int32, number, and string values.
pub fn value_to_numeric_index(key: &Value) -> Option<usize> {
    if let Some(n) = key.as_int32() {
        if n >= 0 {
            return Some(n as usize);
        }
        return None;
    }
    if let Some(n) = key.as_number() {
        let idx = n as usize;
        if n >= 0.0 && n == idx as f64 {
            return Some(idx);
        }
        return None;
    }
    if let Some(s) = key.as_string() {
        return canonical_numeric_index_str(s.as_str());
    }
    None
}

/// Create a string Value from an index (for error messages, iteration, etc.)
pub fn index_to_string_value(idx: u32) -> Value {
    Value::string(JsString::intern(&idx.to_string()))
}
