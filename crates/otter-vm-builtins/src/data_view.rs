//! DataView built-in
//!
//! Provides DataView constructor and methods for arbitrary byte-order access
//! to ArrayBuffer data.

use otter_vm_core::data_view::JsDataView;
use otter_vm_core::memory;
use otter_vm_core::value::{BigInt, HeapRef, Value as VmValue};
use otter_vm_runtime::{Op, op_native_with_mm as op_native};
use std::sync::Arc;

/// Get DataView ops for extension registration
pub fn ops() -> Vec<Op> {
    vec![
        // Constructor
        op_native("__DataView_create", native_data_view_create),
        // Getters for properties
        op_native("__DataView_getBuffer", native_data_view_get_buffer),
        op_native("__DataView_getByteOffset", native_data_view_get_byte_offset),
        op_native("__DataView_getByteLength", native_data_view_get_byte_length),
        // Get methods
        op_native("__DataView_getInt8", native_data_view_get_int8),
        op_native("__DataView_getUint8", native_data_view_get_uint8),
        op_native("__DataView_getInt16", native_data_view_get_int16),
        op_native("__DataView_getUint16", native_data_view_get_uint16),
        op_native("__DataView_getInt32", native_data_view_get_int32),
        op_native("__DataView_getUint32", native_data_view_get_uint32),
        op_native("__DataView_getFloat32", native_data_view_get_float32),
        op_native("__DataView_getFloat64", native_data_view_get_float64),
        op_native("__DataView_getBigInt64", native_data_view_get_big_int64),
        op_native("__DataView_getBigUint64", native_data_view_get_big_uint64),
        // Set methods
        op_native("__DataView_setInt8", native_data_view_set_int8),
        op_native("__DataView_setUint8", native_data_view_set_uint8),
        op_native("__DataView_setInt16", native_data_view_set_int16),
        op_native("__DataView_setUint16", native_data_view_set_uint16),
        op_native("__DataView_setInt32", native_data_view_set_int32),
        op_native("__DataView_setUint32", native_data_view_set_uint32),
        op_native("__DataView_setFloat32", native_data_view_set_float32),
        op_native("__DataView_setFloat64", native_data_view_set_float64),
        op_native("__DataView_setBigInt64", native_data_view_set_big_int64),
        op_native("__DataView_setBigUint64", native_data_view_set_big_uint64),
        // Type check
        op_native("__DataView_isDataView", native_data_view_is_data_view),
    ]
}

/// Helper to extract BigInt value from a Value
fn get_bigint_value(value: &VmValue) -> Option<&Arc<BigInt>> {
    match value.heap_ref() {
        Some(HeapRef::BigInt(bi)) => Some(bi),
        _ => None,
    }
}

/// Create DataView from ArrayBuffer
/// Args: [buffer, byteOffset?, byteLength?]
fn native_data_view_create(
    args: &[VmValue],
    _mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, String> {
    let ab = args
        .first()
        .and_then(|v| v.as_array_buffer())
        .ok_or("TypeError: first argument must be an ArrayBuffer")?
        .clone();

    let byte_offset = args
        .get(1)
        .and_then(|v| v.as_number())
        .map(|n| n as usize)
        .unwrap_or(0);

    let byte_length = args.get(2).and_then(|v| {
        if v.is_undefined() {
            None
        } else {
            v.as_number().map(|n| n as usize)
        }
    });

    let dv = JsDataView::new(ab, byte_offset, byte_length)
        .map_err(|e| format!("RangeError: {}", e))?;

    Ok(VmValue::data_view(Arc::new(dv)))
}

/// Get the underlying buffer
/// Args: [dataView]
fn native_data_view_get_buffer(
    args: &[VmValue],
    _mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, String> {
    let dv = args
        .first()
        .and_then(|v| v.as_data_view())
        .ok_or("TypeError: not a DataView")?;

    Ok(VmValue::array_buffer(dv.buffer().clone()))
}

/// Get the byte offset
/// Args: [dataView]
fn native_data_view_get_byte_offset(
    args: &[VmValue],
    _mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, String> {
    let dv = args
        .first()
        .and_then(|v| v.as_data_view())
        .ok_or("TypeError: not a DataView")?;

    if dv.is_detached() {
        return Err("TypeError: ArrayBuffer is detached".to_string());
    }

    Ok(VmValue::number(dv.byte_offset() as f64))
}

/// Get the byte length
/// Args: [dataView]
fn native_data_view_get_byte_length(
    args: &[VmValue],
    _mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, String> {
    let dv = args
        .first()
        .and_then(|v| v.as_data_view())
        .ok_or("TypeError: not a DataView")?;

    if dv.is_detached() {
        return Err("TypeError: ArrayBuffer is detached".to_string());
    }

    Ok(VmValue::number(dv.byte_length() as f64))
}

// ===== Get methods =====

/// getInt8(byteOffset)
fn native_data_view_get_int8(
    args: &[VmValue],
    _mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, String> {
    let dv = args
        .first()
        .and_then(|v| v.as_data_view())
        .ok_or("TypeError: not a DataView")?;

    let byte_offset = args.get(1).and_then(|v| v.as_number()).unwrap_or(0.0) as usize;

    let value = dv.get_int8(byte_offset).map_err(|e| format!("RangeError: {}", e))?;
    Ok(VmValue::number(value as f64))
}

/// getUint8(byteOffset)
fn native_data_view_get_uint8(
    args: &[VmValue],
    _mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, String> {
    let dv = args
        .first()
        .and_then(|v| v.as_data_view())
        .ok_or("TypeError: not a DataView")?;

    let byte_offset = args.get(1).and_then(|v| v.as_number()).unwrap_or(0.0) as usize;

    let value = dv.get_uint8(byte_offset).map_err(|e| format!("RangeError: {}", e))?;
    Ok(VmValue::number(value as f64))
}

/// getInt16(byteOffset, littleEndian?)
fn native_data_view_get_int16(
    args: &[VmValue],
    _mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, String> {
    let dv = args
        .first()
        .and_then(|v| v.as_data_view())
        .ok_or("TypeError: not a DataView")?;

    let byte_offset = args.get(1).and_then(|v| v.as_number()).unwrap_or(0.0) as usize;
    let little_endian = args.get(2).map(|v| v.to_boolean()).unwrap_or(false);

    let value = dv
        .get_int16(byte_offset, little_endian)
        .map_err(|e| format!("RangeError: {}", e))?;
    Ok(VmValue::number(value as f64))
}

/// getUint16(byteOffset, littleEndian?)
fn native_data_view_get_uint16(
    args: &[VmValue],
    _mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, String> {
    let dv = args
        .first()
        .and_then(|v| v.as_data_view())
        .ok_or("TypeError: not a DataView")?;

    let byte_offset = args.get(1).and_then(|v| v.as_number()).unwrap_or(0.0) as usize;
    let little_endian = args.get(2).map(|v| v.to_boolean()).unwrap_or(false);

    let value = dv
        .get_uint16(byte_offset, little_endian)
        .map_err(|e| format!("RangeError: {}", e))?;
    Ok(VmValue::number(value as f64))
}

/// getInt32(byteOffset, littleEndian?)
fn native_data_view_get_int32(
    args: &[VmValue],
    _mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, String> {
    let dv = args
        .first()
        .and_then(|v| v.as_data_view())
        .ok_or("TypeError: not a DataView")?;

    let byte_offset = args.get(1).and_then(|v| v.as_number()).unwrap_or(0.0) as usize;
    let little_endian = args.get(2).map(|v| v.to_boolean()).unwrap_or(false);

    let value = dv
        .get_int32(byte_offset, little_endian)
        .map_err(|e| format!("RangeError: {}", e))?;
    Ok(VmValue::number(value as f64))
}

/// getUint32(byteOffset, littleEndian?)
fn native_data_view_get_uint32(
    args: &[VmValue],
    _mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, String> {
    let dv = args
        .first()
        .and_then(|v| v.as_data_view())
        .ok_or("TypeError: not a DataView")?;

    let byte_offset = args.get(1).and_then(|v| v.as_number()).unwrap_or(0.0) as usize;
    let little_endian = args.get(2).map(|v| v.to_boolean()).unwrap_or(false);

    let value = dv
        .get_uint32(byte_offset, little_endian)
        .map_err(|e| format!("RangeError: {}", e))?;
    Ok(VmValue::number(value as f64))
}

/// getFloat32(byteOffset, littleEndian?)
fn native_data_view_get_float32(
    args: &[VmValue],
    _mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, String> {
    let dv = args
        .first()
        .and_then(|v| v.as_data_view())
        .ok_or("TypeError: not a DataView")?;

    let byte_offset = args.get(1).and_then(|v| v.as_number()).unwrap_or(0.0) as usize;
    let little_endian = args.get(2).map(|v| v.to_boolean()).unwrap_or(false);

    let value = dv
        .get_float32(byte_offset, little_endian)
        .map_err(|e| format!("RangeError: {}", e))?;
    Ok(VmValue::number(value as f64))
}

/// getFloat64(byteOffset, littleEndian?)
fn native_data_view_get_float64(
    args: &[VmValue],
    _mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, String> {
    let dv = args
        .first()
        .and_then(|v| v.as_data_view())
        .ok_or("TypeError: not a DataView")?;

    let byte_offset = args.get(1).and_then(|v| v.as_number()).unwrap_or(0.0) as usize;
    let little_endian = args.get(2).map(|v| v.to_boolean()).unwrap_or(false);

    let value = dv
        .get_float64(byte_offset, little_endian)
        .map_err(|e| format!("RangeError: {}", e))?;
    Ok(VmValue::number(value))
}

/// getBigInt64(byteOffset, littleEndian?)
fn native_data_view_get_big_int64(
    args: &[VmValue],
    _mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, String> {
    let dv = args
        .first()
        .and_then(|v| v.as_data_view())
        .ok_or("TypeError: not a DataView")?;

    let byte_offset = args.get(1).and_then(|v| v.as_number()).unwrap_or(0.0) as usize;
    let little_endian = args.get(2).map(|v| v.to_boolean()).unwrap_or(false);

    let value = dv
        .get_big_int64(byte_offset, little_endian)
        .map_err(|e| format!("RangeError: {}", e))?;
    Ok(VmValue::bigint(value.to_string()))
}

/// getBigUint64(byteOffset, littleEndian?)
fn native_data_view_get_big_uint64(
    args: &[VmValue],
    _mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, String> {
    let dv = args
        .first()
        .and_then(|v| v.as_data_view())
        .ok_or("TypeError: not a DataView")?;

    let byte_offset = args.get(1).and_then(|v| v.as_number()).unwrap_or(0.0) as usize;
    let little_endian = args.get(2).map(|v| v.to_boolean()).unwrap_or(false);

    let value = dv
        .get_big_uint64(byte_offset, little_endian)
        .map_err(|e| format!("RangeError: {}", e))?;
    Ok(VmValue::bigint(value.to_string()))
}

// ===== Set methods =====

/// setInt8(byteOffset, value)
fn native_data_view_set_int8(
    args: &[VmValue],
    _mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, String> {
    let dv = args
        .first()
        .and_then(|v| v.as_data_view())
        .ok_or("TypeError: not a DataView")?;

    let byte_offset = args.get(1).and_then(|v| v.as_number()).unwrap_or(0.0) as usize;
    let value = args.get(2).and_then(|v| v.as_number()).unwrap_or(0.0) as i8;

    dv.set_int8(byte_offset, value)
        .map_err(|e| format!("RangeError: {}", e))?;
    Ok(VmValue::undefined())
}

/// setUint8(byteOffset, value)
fn native_data_view_set_uint8(
    args: &[VmValue],
    _mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, String> {
    let dv = args
        .first()
        .and_then(|v| v.as_data_view())
        .ok_or("TypeError: not a DataView")?;

    let byte_offset = args.get(1).and_then(|v| v.as_number()).unwrap_or(0.0) as usize;
    let value = args.get(2).and_then(|v| v.as_number()).unwrap_or(0.0) as u8;

    dv.set_uint8(byte_offset, value)
        .map_err(|e| format!("RangeError: {}", e))?;
    Ok(VmValue::undefined())
}

/// setInt16(byteOffset, value, littleEndian?)
fn native_data_view_set_int16(
    args: &[VmValue],
    _mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, String> {
    let dv = args
        .first()
        .and_then(|v| v.as_data_view())
        .ok_or("TypeError: not a DataView")?;

    let byte_offset = args.get(1).and_then(|v| v.as_number()).unwrap_or(0.0) as usize;
    let value = args.get(2).and_then(|v| v.as_number()).unwrap_or(0.0) as i16;
    let little_endian = args.get(3).map(|v| v.to_boolean()).unwrap_or(false);

    dv.set_int16(byte_offset, value, little_endian)
        .map_err(|e| format!("RangeError: {}", e))?;
    Ok(VmValue::undefined())
}

/// setUint16(byteOffset, value, littleEndian?)
fn native_data_view_set_uint16(
    args: &[VmValue],
    _mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, String> {
    let dv = args
        .first()
        .and_then(|v| v.as_data_view())
        .ok_or("TypeError: not a DataView")?;

    let byte_offset = args.get(1).and_then(|v| v.as_number()).unwrap_or(0.0) as usize;
    let value = args.get(2).and_then(|v| v.as_number()).unwrap_or(0.0) as u16;
    let little_endian = args.get(3).map(|v| v.to_boolean()).unwrap_or(false);

    dv.set_uint16(byte_offset, value, little_endian)
        .map_err(|e| format!("RangeError: {}", e))?;
    Ok(VmValue::undefined())
}

/// setInt32(byteOffset, value, littleEndian?)
fn native_data_view_set_int32(
    args: &[VmValue],
    _mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, String> {
    let dv = args
        .first()
        .and_then(|v| v.as_data_view())
        .ok_or("TypeError: not a DataView")?;

    let byte_offset = args.get(1).and_then(|v| v.as_number()).unwrap_or(0.0) as usize;
    let value = args.get(2).and_then(|v| v.as_number()).unwrap_or(0.0) as i32;
    let little_endian = args.get(3).map(|v| v.to_boolean()).unwrap_or(false);

    dv.set_int32(byte_offset, value, little_endian)
        .map_err(|e| format!("RangeError: {}", e))?;
    Ok(VmValue::undefined())
}

/// setUint32(byteOffset, value, littleEndian?)
fn native_data_view_set_uint32(
    args: &[VmValue],
    _mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, String> {
    let dv = args
        .first()
        .and_then(|v| v.as_data_view())
        .ok_or("TypeError: not a DataView")?;

    let byte_offset = args.get(1).and_then(|v| v.as_number()).unwrap_or(0.0) as usize;
    let value = args.get(2).and_then(|v| v.as_number()).unwrap_or(0.0) as u32;
    let little_endian = args.get(3).map(|v| v.to_boolean()).unwrap_or(false);

    dv.set_uint32(byte_offset, value, little_endian)
        .map_err(|e| format!("RangeError: {}", e))?;
    Ok(VmValue::undefined())
}

/// setFloat32(byteOffset, value, littleEndian?)
fn native_data_view_set_float32(
    args: &[VmValue],
    _mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, String> {
    let dv = args
        .first()
        .and_then(|v| v.as_data_view())
        .ok_or("TypeError: not a DataView")?;

    let byte_offset = args.get(1).and_then(|v| v.as_number()).unwrap_or(0.0) as usize;
    let value = args.get(2).and_then(|v| v.as_number()).unwrap_or(0.0) as f32;
    let little_endian = args.get(3).map(|v| v.to_boolean()).unwrap_or(false);

    dv.set_float32(byte_offset, value, little_endian)
        .map_err(|e| format!("RangeError: {}", e))?;
    Ok(VmValue::undefined())
}

/// setFloat64(byteOffset, value, littleEndian?)
fn native_data_view_set_float64(
    args: &[VmValue],
    _mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, String> {
    let dv = args
        .first()
        .and_then(|v| v.as_data_view())
        .ok_or("TypeError: not a DataView")?;

    let byte_offset = args.get(1).and_then(|v| v.as_number()).unwrap_or(0.0) as usize;
    let value = args.get(2).and_then(|v| v.as_number()).unwrap_or(0.0);
    let little_endian = args.get(3).map(|v| v.to_boolean()).unwrap_or(false);

    dv.set_float64(byte_offset, value, little_endian)
        .map_err(|e| format!("RangeError: {}", e))?;
    Ok(VmValue::undefined())
}

/// setBigInt64(byteOffset, value, littleEndian?)
fn native_data_view_set_big_int64(
    args: &[VmValue],
    _mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, String> {
    let dv = args
        .first()
        .and_then(|v| v.as_data_view())
        .ok_or("TypeError: not a DataView")?;

    let byte_offset = args.get(1).and_then(|v| v.as_number()).unwrap_or(0.0) as usize;

    let value_arg = args.get(2).ok_or("TypeError: BigInt expected")?;
    let bigint = get_bigint_value(value_arg).ok_or("TypeError: BigInt expected")?;
    let value: i64 = bigint.value.parse().unwrap_or(0);

    let little_endian = args.get(3).map(|v| v.to_boolean()).unwrap_or(false);

    dv.set_big_int64(byte_offset, value, little_endian)
        .map_err(|e| format!("RangeError: {}", e))?;
    Ok(VmValue::undefined())
}

/// setBigUint64(byteOffset, value, littleEndian?)
fn native_data_view_set_big_uint64(
    args: &[VmValue],
    _mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, String> {
    let dv = args
        .first()
        .and_then(|v| v.as_data_view())
        .ok_or("TypeError: not a DataView")?;

    let byte_offset = args.get(1).and_then(|v| v.as_number()).unwrap_or(0.0) as usize;

    let value_arg = args.get(2).ok_or("TypeError: BigInt expected")?;
    let bigint = get_bigint_value(value_arg).ok_or("TypeError: BigInt expected")?;
    let value: u64 = bigint.value.parse().unwrap_or(0);

    let little_endian = args.get(3).map(|v| v.to_boolean()).unwrap_or(false);

    dv.set_big_uint64(byte_offset, value, little_endian)
        .map_err(|e| format!("RangeError: {}", e))?;
    Ok(VmValue::undefined())
}

/// Check if value is a DataView
fn native_data_view_is_data_view(
    args: &[VmValue],
    _mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, String> {
    let is_dv = args.first().map(|v| v.is_data_view()).unwrap_or(false);
    Ok(VmValue::boolean(is_dv))
}

#[cfg(test)]
mod tests {
    use super::*;
    use otter_vm_core::array_buffer::JsArrayBuffer;

    fn mm() -> Arc<memory::MemoryManager> {
        Arc::new(memory::MemoryManager::test())
    }

    #[test]
    fn test_create_data_view() {
        let ab = Arc::new(JsArrayBuffer::new(16));
        let ab_val = VmValue::array_buffer(ab);

        let result = native_data_view_create(&[ab_val], mm()).unwrap();
        assert!(result.is_data_view());

        let dv = result.as_data_view().unwrap();
        assert_eq!(dv.byte_length(), 16);
        assert_eq!(dv.byte_offset(), 0);
    }

    #[test]
    fn test_create_with_offset() {
        let ab = Arc::new(JsArrayBuffer::new(16));
        let ab_val = VmValue::array_buffer(ab);

        let result =
            native_data_view_create(&[ab_val, VmValue::number(4.0), VmValue::number(8.0)], mm())
                .unwrap();
        let dv = result.as_data_view().unwrap();
        assert_eq!(dv.byte_length(), 8);
        assert_eq!(dv.byte_offset(), 4);
    }

    #[test]
    fn test_int32_operations() {
        let ab = Arc::new(JsArrayBuffer::new(16));
        let ab_val = VmValue::array_buffer(ab);
        let dv_val = native_data_view_create(&[ab_val], mm()).unwrap();

        // Set and get little-endian
        native_data_view_set_int32(
            &[dv_val.clone(), VmValue::number(0.0), VmValue::number(0x12345678 as f64), VmValue::boolean(true)],
            mm(),
        )
        .unwrap();

        let result = native_data_view_get_int32(
            &[dv_val.clone(), VmValue::number(0.0), VmValue::boolean(true)],
            mm(),
        )
        .unwrap();
        assert_eq!(result.as_number(), Some(0x12345678 as f64));

        // Read same bytes as big-endian (should be different)
        let result_be = native_data_view_get_int32(
            &[dv_val, VmValue::number(0.0), VmValue::boolean(false)],
            mm(),
        )
        .unwrap();
        assert_ne!(result_be.as_number(), Some(0x12345678 as f64));
    }

    #[test]
    fn test_float64_operations() {
        let ab = Arc::new(JsArrayBuffer::new(16));
        let ab_val = VmValue::array_buffer(ab);
        let dv_val = native_data_view_create(&[ab_val], mm()).unwrap();

        let pi = std::f64::consts::PI;
        native_data_view_set_float64(
            &[dv_val.clone(), VmValue::number(0.0), VmValue::number(pi), VmValue::boolean(true)],
            mm(),
        )
        .unwrap();

        let result = native_data_view_get_float64(
            &[dv_val, VmValue::number(0.0), VmValue::boolean(true)],
            mm(),
        )
        .unwrap();
        let val = result.as_number().unwrap();
        assert!((val - pi).abs() < 1e-10);
    }

    #[test]
    fn test_bounds_check() {
        let ab = Arc::new(JsArrayBuffer::new(4));
        let ab_val = VmValue::array_buffer(ab);
        let dv_val = native_data_view_create(&[ab_val], mm()).unwrap();

        // Try to read past end
        let result = native_data_view_get_int32(
            &[dv_val, VmValue::number(1.0), VmValue::boolean(true)],
            mm(),
        );
        assert!(result.is_err());
    }
}
