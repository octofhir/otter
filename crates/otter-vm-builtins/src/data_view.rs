//! DataView built-in
//!
//! Provides DataView constructor and methods for arbitrary byte-order access
//! to ArrayBuffer data.

use otter_vm_core::error::VmError;
use otter_vm_core::data_view::JsDataView;
use otter_vm_core::gc::GcRef;
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
fn get_bigint_value(value: &VmValue) -> Option<otter_vm_core::gc::GcRef<BigInt>> {
    match value.heap_ref() {
        Some(HeapRef::BigInt(bi)) => Some(*bi),
        _ => None,
    }
}

/// Create DataView from ArrayBuffer
/// Args: [buffer, byteOffset?, byteLength?]
fn native_data_view_create(
    args: &[VmValue],
    _mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, VmError> {
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

    Ok(VmValue::data_view(GcRef::new(dv)))
}

/// Get the underlying buffer
/// Args: [dataView]
fn native_data_view_get_buffer(
    args: &[VmValue],
    _mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, VmError> {
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
) -> Result<VmValue, VmError> {
    let dv = args
        .first()
        .and_then(|v| v.as_data_view())
        .ok_or("TypeError: not a DataView")?;

    if dv.is_detached() {
        return Err(VmError::type_error("ArrayBuffer is detached"));
    }

    Ok(VmValue::number(dv.byte_offset() as f64))
}

/// Get the byte length
/// Args: [dataView]
fn native_data_view_get_byte_length(
    args: &[VmValue],
    _mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, VmError> {
    let dv = args
        .first()
        .and_then(|v| v.as_data_view())
        .ok_or("TypeError: not a DataView")?;

    if dv.is_detached() {
        return Err(VmError::type_error("ArrayBuffer is detached"));
    }

    Ok(VmValue::number(dv.byte_length() as f64))
}

// ===== Get methods =====

/// getInt8(byteOffset)
fn native_data_view_get_int8(
    args: &[VmValue],
    _mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, VmError> {
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
) -> Result<VmValue, VmError> {
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
) -> Result<VmValue, VmError> {
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
) -> Result<VmValue, VmError> {
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
) -> Result<VmValue, VmError> {
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
) -> Result<VmValue, VmError> {
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
) -> Result<VmValue, VmError> {
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
) -> Result<VmValue, VmError> {
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
) -> Result<VmValue, VmError> {
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
) -> Result<VmValue, VmError> {
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
) -> Result<VmValue, VmError> {
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
) -> Result<VmValue, VmError> {
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
) -> Result<VmValue, VmError> {
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
) -> Result<VmValue, VmError> {
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
) -> Result<VmValue, VmError> {
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
) -> Result<VmValue, VmError> {
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
) -> Result<VmValue, VmError> {
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
) -> Result<VmValue, VmError> {
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
) -> Result<VmValue, VmError> {
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
) -> Result<VmValue, VmError> {
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
) -> Result<VmValue, VmError> {
    let is_dv = args.first().map(|v| v.is_data_view()).unwrap_or(false);
    Ok(VmValue::boolean(is_dv))
}

// TODO: Tests need to be updated to use NativeContext instead of Arc<MemoryManager>
