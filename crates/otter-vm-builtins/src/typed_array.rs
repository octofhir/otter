//! TypedArray built-in
//!
//! Provides TypedArray constructors and methods for all 11 types:
//! Int8Array, Uint8Array, Uint8ClampedArray, Int16Array, Uint16Array,
//! Int32Array, Uint32Array, Float32Array, Float64Array, BigInt64Array, BigUint64Array

use otter_vm_core::memory;
use otter_vm_core::string::JsString;
use otter_vm_core::typed_array::{JsTypedArray, TypedArrayKind};
use otter_vm_core::value::{BigInt, HeapRef, Value as VmValue};
use otter_vm_runtime::{op_native_with_mm as op_native, Op};
use std::sync::Arc;

/// Get TypedArray ops for extension registration
pub fn ops() -> Vec<Op> {
    vec![
        // Constructors
        op_native("__TypedArray_create", native_typed_array_create),
        op_native(
            "__TypedArray_createFromLength",
            native_typed_array_create_from_length,
        ),
        op_native(
            "__TypedArray_createFromArray",
            native_typed_array_create_from_array,
        ),
        // Prototype getters
        op_native("__TypedArray_buffer", native_typed_array_buffer),
        op_native("__TypedArray_byteLength", native_typed_array_byte_length),
        op_native("__TypedArray_byteOffset", native_typed_array_byte_offset),
        op_native("__TypedArray_length", native_typed_array_length),
        // Element access
        op_native("__TypedArray_get", native_typed_array_get),
        op_native("__TypedArray_set", native_typed_array_set),
        // Prototype methods
        op_native("__TypedArray_subarray", native_typed_array_subarray),
        op_native("__TypedArray_slice", native_typed_array_slice),
        op_native("__TypedArray_fill", native_typed_array_fill),
        op_native("__TypedArray_copyWithin", native_typed_array_copy_within),
        op_native("__TypedArray_reverse", native_typed_array_reverse),
        op_native("__TypedArray_set_array", native_typed_array_set_array),
        // Type check
        op_native(
            "__TypedArray_isTypedArray",
            native_typed_array_is_typed_array,
        ),
        op_native("__TypedArray_kind", native_typed_array_kind),
    ]
}

/// Helper to extract BigInt value from a Value
fn get_bigint_value(value: &VmValue) -> Option<&Arc<BigInt>> {
    match value.heap_ref() {
        Some(HeapRef::BigInt(bi)) => Some(bi),
        _ => None,
    }
}

/// Parse TypedArray kind from string
fn parse_kind(kind_str: &str) -> Option<TypedArrayKind> {
    match kind_str {
        "Int8Array" => Some(TypedArrayKind::Int8),
        "Uint8Array" => Some(TypedArrayKind::Uint8),
        "Uint8ClampedArray" => Some(TypedArrayKind::Uint8Clamped),
        "Int16Array" => Some(TypedArrayKind::Int16),
        "Uint16Array" => Some(TypedArrayKind::Uint16),
        "Int32Array" => Some(TypedArrayKind::Int32),
        "Uint32Array" => Some(TypedArrayKind::Uint32),
        "Float32Array" => Some(TypedArrayKind::Float32),
        "Float64Array" => Some(TypedArrayKind::Float64),
        "BigInt64Array" => Some(TypedArrayKind::BigInt64),
        "BigUint64Array" => Some(TypedArrayKind::BigUint64),
        _ => None,
    }
}

// ============================================================================
// Constructors
// ============================================================================

/// Create TypedArray from ArrayBuffer
/// Args: [arrayBuffer, kindString, byteOffset?, length?]
fn native_typed_array_create(
    args: &[VmValue],
    _mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, String> {
    let ab = args
        .first()
        .and_then(|v| v.as_array_buffer())
        .ok_or("TypeError: first argument must be an ArrayBuffer")?
        .clone();

    let kind_str = args
        .get(1)
        .and_then(|v| v.as_string())
        .ok_or("TypeError: kind must be a string")?;

    let kind = parse_kind(kind_str.as_str()).ok_or("TypeError: invalid TypedArray kind")?;

    if ab.is_detached() {
        return Err("TypeError: ArrayBuffer is detached".to_string());
    }

    let byte_offset = args
        .get(2)
        .and_then(|v| {
            if v.is_undefined() {
                None
            } else {
                v.as_number()
            }
        })
        .unwrap_or(0.0) as usize;

    let elem_size = kind.element_size();

    // Validate byte offset alignment
    if byte_offset % elem_size != 0 {
        return Err(format!(
            "RangeError: byte offset must be a multiple of {}",
            elem_size
        ));
    }

    let length = args.get(3).and_then(|v| {
        if v.is_undefined() {
            None
        } else {
            v.as_number()
        }
    });

    let actual_length = match length {
        Some(len) => len as usize,
        None => {
            let remaining = ab.byte_length().saturating_sub(byte_offset);
            if remaining % elem_size != 0 {
                return Err(format!(
                    "RangeError: buffer length minus offset must be a multiple of {}",
                    elem_size
                ));
            }
            remaining / elem_size
        }
    };

    let ta = JsTypedArray::new(ab, kind, byte_offset, actual_length)
        .map_err(|e| format!("RangeError: {}", e))?;

    Ok(VmValue::typed_array(Arc::new(ta)))
}

/// Create TypedArray from length (creates new buffer)
/// Args: [length, kindString]
fn native_typed_array_create_from_length(
    args: &[VmValue],
    _mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, String> {
    let length = args.first().and_then(|v| v.as_number()).unwrap_or(0.0) as usize;

    let kind_str = args
        .get(1)
        .and_then(|v| v.as_string())
        .ok_or("TypeError: kind must be a string")?;

    let kind = parse_kind(kind_str.as_str()).ok_or("TypeError: invalid TypedArray kind")?;

    let ta = JsTypedArray::with_length(kind, length, None, _mm);
    Ok(VmValue::typed_array(Arc::new(ta)))
}

/// Create TypedArray from array-like (creates new buffer with values)
/// Args: [valuesArray, kindString]
fn native_typed_array_create_from_array(
    args: &[VmValue],
    _mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, String> {
    let values = args.first().ok_or("TypeError: values required")?;

    let kind_str = args
        .get(1)
        .and_then(|v| v.as_string())
        .ok_or("TypeError: kind must be a string")?;

    let kind = parse_kind(kind_str.as_str()).ok_or("TypeError: invalid TypedArray kind")?;

    // Get length from array
    let arr_obj = values
        .as_object()
        .ok_or("TypeError: values must be array-like")?;
    let length_val = arr_obj
        .get(&otter_vm_core::object::PropertyKey::string("length"))
        .ok_or("TypeError: values must have length")?;
    let length = length_val.as_number().unwrap_or(0.0) as usize;

    let ta = JsTypedArray::with_length(kind, length, None, _mm);

    // Copy values
    for i in 0..length {
        if let Some(val) = arr_obj.get(&otter_vm_core::object::PropertyKey::Index(i as u32)) {
            if kind.is_bigint() {
                // For BigInt arrays, expect BigInt values
                if let Some(bigint) = get_bigint_value(&val) {
                    let int_val: i64 = bigint.value.parse().unwrap_or(0);
                    ta.set_bigint(i, int_val);
                }
            } else if let Some(n) = val.as_number() {
                ta.set(i, n);
            }
        }
    }

    Ok(VmValue::typed_array(Arc::new(ta)))
}

// ============================================================================
// Prototype Getters
// ============================================================================

/// Get the underlying ArrayBuffer
fn native_typed_array_buffer(
    args: &[VmValue],
    _mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, String> {
    let ta = args
        .first()
        .and_then(|v| v.as_typed_array())
        .ok_or("TypeError: not a TypedArray")?;

    Ok(VmValue::array_buffer(ta.buffer().clone()))
}

/// Get the byte length
fn native_typed_array_byte_length(
    args: &[VmValue],
    _mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, String> {
    let ta = args
        .first()
        .and_then(|v| v.as_typed_array())
        .ok_or("TypeError: not a TypedArray")?;

    Ok(VmValue::number(ta.byte_length() as f64))
}

/// Get the byte offset
fn native_typed_array_byte_offset(
    args: &[VmValue],
    _mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, String> {
    let ta = args
        .first()
        .and_then(|v| v.as_typed_array())
        .ok_or("TypeError: not a TypedArray")?;

    if ta.is_detached() {
        Ok(VmValue::number(0.0))
    } else {
        Ok(VmValue::number(ta.byte_offset() as f64))
    }
}

/// Get the element count
fn native_typed_array_length(
    args: &[VmValue],
    _mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, String> {
    let ta = args
        .first()
        .and_then(|v| v.as_typed_array())
        .ok_or("TypeError: not a TypedArray")?;

    Ok(VmValue::number(ta.length() as f64))
}

// ============================================================================
// Element Access
// ============================================================================

/// Get element at index
/// Args: [typedArray, index]
fn native_typed_array_get(
    args: &[VmValue],
    _mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, String> {
    let ta = args
        .first()
        .and_then(|v| v.as_typed_array())
        .ok_or("TypeError: not a TypedArray")?;

    let index = args.get(1).and_then(|v| v.as_number()).unwrap_or(0.0) as usize;

    if ta.is_detached() {
        return Err("TypeError: ArrayBuffer is detached".to_string());
    }

    if ta.kind().is_bigint() {
        match ta.get_bigint(index) {
            Some(v) => Ok(VmValue::bigint(v.to_string())),
            None => Ok(VmValue::undefined()),
        }
    } else {
        match ta.get(index) {
            Some(v) => Ok(VmValue::number(v)),
            None => Ok(VmValue::undefined()),
        }
    }
}

/// Set element at index
/// Args: [typedArray, index, value]
fn native_typed_array_set(
    args: &[VmValue],
    _mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, String> {
    let ta = args
        .first()
        .and_then(|v| v.as_typed_array())
        .ok_or("TypeError: not a TypedArray")?;

    let index = args.get(1).and_then(|v| v.as_number()).unwrap_or(0.0) as usize;

    if ta.is_detached() {
        return Err("TypeError: ArrayBuffer is detached".to_string());
    }

    if ta.kind().is_bigint() {
        let arg = args
            .get(2)
            .ok_or("TypeError: BigInt expected for BigInt typed array")?;
        let bigint =
            get_bigint_value(arg).ok_or("TypeError: BigInt expected for BigInt typed array")?;
        let int_val: i64 = bigint.value.parse().unwrap_or(0);
        ta.set_bigint(index, int_val);
    } else {
        let value = args.get(2).and_then(|v| v.as_number()).unwrap_or(0.0);
        ta.set(index, value);
    }

    Ok(VmValue::undefined())
}

// ============================================================================
// Prototype Methods
// ============================================================================

/// Create subarray view
/// Args: [typedArray, begin?, end?]
fn native_typed_array_subarray(
    args: &[VmValue],
    _mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, String> {
    let ta = args
        .first()
        .and_then(|v| v.as_typed_array())
        .ok_or("TypeError: not a TypedArray")?;

    let begin = args.get(1).and_then(|v| v.as_number()).unwrap_or(0.0) as i64;
    let end = args
        .get(2)
        .and_then(|v| {
            if v.is_undefined() {
                None
            } else {
                v.as_number()
            }
        })
        .map(|n| n as i64);

    let new_ta = ta
        .subarray(begin, end)
        .map_err(|e| format!("TypeError: {}", e))?;
    Ok(VmValue::typed_array(Arc::new(new_ta)))
}

/// Create slice (copy)
/// Args: [typedArray, begin?, end?]
fn native_typed_array_slice(
    args: &[VmValue],
    _mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, String> {
    let ta = args
        .first()
        .and_then(|v| v.as_typed_array())
        .ok_or("TypeError: not a TypedArray")?;

    let begin = args.get(1).and_then(|v| v.as_number()).unwrap_or(0.0) as i64;
    let end = args
        .get(2)
        .and_then(|v| {
            if v.is_undefined() {
                None
            } else {
                v.as_number()
            }
        })
        .map(|n| n as i64);

    let new_ta = ta
        .slice(begin, end)
        .map_err(|e| format!("TypeError: {}", e))?;
    Ok(VmValue::typed_array(Arc::new(new_ta)))
}

/// Fill with value
/// Args: [typedArray, value, start?, end?]
fn native_typed_array_fill(
    args: &[VmValue],
    _mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, String> {
    let ta = args
        .first()
        .and_then(|v| v.as_typed_array())
        .ok_or("TypeError: not a TypedArray")?;

    if ta.is_detached() {
        return Err("TypeError: ArrayBuffer is detached".to_string());
    }

    let value = args.get(1).and_then(|v| v.as_number()).unwrap_or(0.0);
    let start = args
        .get(2)
        .and_then(|v| {
            if v.is_undefined() {
                None
            } else {
                v.as_number()
            }
        })
        .map(|n| n as i64);
    let end = args
        .get(3)
        .and_then(|v| {
            if v.is_undefined() {
                None
            } else {
                v.as_number()
            }
        })
        .map(|n| n as i64);

    ta.fill(value, start, end);

    // Return the typed array value (for chaining)
    Ok(args.first().cloned().unwrap_or(VmValue::undefined()))
}

/// Copy within array
/// Args: [typedArray, target, start, end?]
fn native_typed_array_copy_within(
    args: &[VmValue],
    _mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, String> {
    let ta = args
        .first()
        .and_then(|v| v.as_typed_array())
        .ok_or("TypeError: not a TypedArray")?;

    if ta.is_detached() {
        return Err("TypeError: ArrayBuffer is detached".to_string());
    }

    let target = args.get(1).and_then(|v| v.as_number()).unwrap_or(0.0) as i64;
    let start = args.get(2).and_then(|v| v.as_number()).unwrap_or(0.0) as i64;
    let end = args
        .get(3)
        .and_then(|v| {
            if v.is_undefined() {
                None
            } else {
                v.as_number()
            }
        })
        .map(|n| n as i64);

    ta.copy_within(target, start, end);

    Ok(args.first().cloned().unwrap_or(VmValue::undefined()))
}

/// Reverse in place
/// Args: [typedArray]
fn native_typed_array_reverse(
    args: &[VmValue],
    _mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, String> {
    let ta = args
        .first()
        .and_then(|v| v.as_typed_array())
        .ok_or("TypeError: not a TypedArray")?;

    if ta.is_detached() {
        return Err("TypeError: ArrayBuffer is detached".to_string());
    }

    ta.reverse();

    Ok(args.first().cloned().unwrap_or(VmValue::undefined()))
}

/// Set from array
/// Args: [typedArray, sourceArray, offset?]
fn native_typed_array_set_array(
    args: &[VmValue],
    _mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, String> {
    let ta = args
        .first()
        .and_then(|v| v.as_typed_array())
        .ok_or("TypeError: not a TypedArray")?;

    if ta.is_detached() {
        return Err("TypeError: ArrayBuffer is detached".to_string());
    }

    let source = args.get(1).ok_or("TypeError: source required")?;
    let offset = args.get(2).and_then(|v| v.as_number()).unwrap_or(0.0) as usize;

    // Get source length
    let src_obj = source
        .as_object()
        .ok_or("TypeError: source must be array-like")?;
    let length_val = src_obj
        .get(&otter_vm_core::object::PropertyKey::string("length"))
        .ok_or("TypeError: source must have length")?;
    let src_len = length_val.as_number().unwrap_or(0.0) as usize;

    if offset + src_len > ta.length() {
        return Err("RangeError: source is too large".to_string());
    }

    // Copy values
    for i in 0..src_len {
        if let Some(val) = src_obj.get(&otter_vm_core::object::PropertyKey::Index(i as u32)) {
            if ta.kind().is_bigint() {
                if let Some(bigint) = get_bigint_value(&val) {
                    let int_val: i64 = bigint.value.parse().unwrap_or(0);
                    ta.set_bigint(offset + i, int_val);
                }
            } else if let Some(n) = val.as_number() {
                ta.set(offset + i, n);
            }
        }
    }

    Ok(VmValue::undefined())
}

// ============================================================================
// Type Checks
// ============================================================================

/// Check if value is a TypedArray
fn native_typed_array_is_typed_array(
    args: &[VmValue],
    _mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, String> {
    let is_ta = args.first().map(|v| v.is_typed_array()).unwrap_or(false);
    Ok(VmValue::boolean(is_ta))
}

/// Get TypedArray kind name
fn native_typed_array_kind(
    args: &[VmValue],
    _mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, String> {
    let ta = args
        .first()
        .and_then(|v| v.as_typed_array())
        .ok_or("TypeError: not a TypedArray")?;

    Ok(VmValue::string(JsString::intern(ta.kind().name())))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mm() -> Arc<memory::MemoryManager> {
        Arc::new(memory::MemoryManager::test())
    }

    #[test]
    fn test_create_from_length() {
        let result = native_typed_array_create_from_length(
            &[
                VmValue::number(10.0),
                VmValue::string(JsString::intern("Int32Array")),
            ],
            mm(),
        )
        .unwrap();

        assert!(result.is_typed_array());
        let ta = result.as_typed_array().unwrap();
        assert_eq!(ta.length(), 10);
        assert_eq!(ta.byte_length(), 40);
    }

    #[test]
    fn test_get_set() {
        let ta_val = native_typed_array_create_from_length(
            &[
                VmValue::number(4.0),
                VmValue::string(JsString::intern("Int32Array")),
            ],
            mm(),
        )
        .unwrap();

        // Set value
        native_typed_array_set(
            &[ta_val.clone(), VmValue::number(0.0), VmValue::number(42.0)],
            mm(),
        )
        .unwrap();

        // Get value
        let result = native_typed_array_get(&[ta_val, VmValue::number(0.0)], mm()).unwrap();
        assert_eq!(result.as_number(), Some(42.0));
    }

    #[test]
    fn test_subarray() {
        let ta_val = native_typed_array_create_from_length(
            &[
                VmValue::number(10.0),
                VmValue::string(JsString::intern("Int32Array")),
            ],
            mm(),
        )
        .unwrap();

        // Set some values
        for i in 0..10 {
            native_typed_array_set(
                &[
                    ta_val.clone(),
                    VmValue::number(i as f64),
                    VmValue::number(i as f64),
                ],
                mm(),
            )
            .unwrap();
        }

        // Create subarray
        let sub_val = native_typed_array_subarray(
            &[ta_val.clone(), VmValue::number(2.0), VmValue::number(5.0)],
            mm(),
        )
        .unwrap();

        let sub = sub_val.as_typed_array().unwrap();
        assert_eq!(sub.length(), 3);
        assert_eq!(sub.get(0), Some(2.0));
        assert_eq!(sub.get(1), Some(3.0));
        assert_eq!(sub.get(2), Some(4.0));
    }

    #[test]
    fn test_slice() {
        let ta_val = native_typed_array_create_from_length(
            &[
                VmValue::number(10.0),
                VmValue::string(JsString::intern("Int32Array")),
            ],
            mm(),
        )
        .unwrap();

        for i in 0..10 {
            native_typed_array_set(
                &[
                    ta_val.clone(),
                    VmValue::number(i as f64),
                    VmValue::number(i as f64),
                ],
                mm(),
            )
            .unwrap();
        }

        let slice_val = native_typed_array_slice(
            &[ta_val.clone(), VmValue::number(2.0), VmValue::number(5.0)],
            mm(),
        )
        .unwrap();

        let slice = slice_val.as_typed_array().unwrap();
        assert_eq!(slice.length(), 3);

        // Modify slice - should not affect original
        native_typed_array_set(
            &[slice_val, VmValue::number(0.0), VmValue::number(100.0)],
            mm(),
        )
        .unwrap();

        let orig = ta_val.as_typed_array().unwrap();
        assert_eq!(orig.get(2), Some(2.0)); // Original unchanged
    }

    #[test]
    fn test_fill() {
        let ta_val = native_typed_array_create_from_length(
            &[
                VmValue::number(5.0),
                VmValue::string(JsString::intern("Int32Array")),
            ],
            mm(),
        )
        .unwrap();

        native_typed_array_fill(&[ta_val.clone(), VmValue::number(42.0)], mm()).unwrap();

        let ta = ta_val.as_typed_array().unwrap();
        for i in 0..5 {
            assert_eq!(ta.get(i), Some(42.0));
        }
    }

    #[test]
    fn test_reverse() {
        let ta_val = native_typed_array_create_from_length(
            &[
                VmValue::number(5.0),
                VmValue::string(JsString::intern("Int32Array")),
            ],
            mm(),
        )
        .unwrap();

        for i in 0..5 {
            native_typed_array_set(
                &[
                    ta_val.clone(),
                    VmValue::number(i as f64),
                    VmValue::number(i as f64),
                ],
                mm(),
            )
            .unwrap();
        }

        native_typed_array_reverse(&[ta_val.clone()], mm()).unwrap();

        let ta = ta_val.as_typed_array().unwrap();
        assert_eq!(ta.get(0), Some(4.0));
        assert_eq!(ta.get(1), Some(3.0));
        assert_eq!(ta.get(2), Some(2.0));
        assert_eq!(ta.get(3), Some(1.0));
        assert_eq!(ta.get(4), Some(0.0));
    }
}
