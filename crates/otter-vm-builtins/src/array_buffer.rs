//! ArrayBuffer built-in
//!
//! Provides ArrayBuffer constructor and methods:
//! - `new ArrayBuffer(byteLength, options?)`
//! - `.byteLength`, `.maxByteLength`, `.resizable`, `.detached` (getters)
//! - `.slice(begin, end)`
//! - `.transfer()`, `.transferToFixedLength(newLength)`, `.resize(newLength)` (ES2024)
//! - `ArrayBuffer.isView(arg)` (static)

use otter_vm_core::error::VmError;
use otter_vm_core::array_buffer::JsArrayBuffer;
use otter_vm_core::memory;
use otter_vm_core::value::Value as VmValue;
use otter_vm_runtime::{op_native_with_mm as op_native, Op};
use std::sync::Arc;

/// Get ArrayBuffer ops for extension registration
pub fn ops() -> Vec<Op> {
    vec![
        // Constructor
        op_native("__ArrayBuffer_create", native_array_buffer_create),
        // Prototype getters
        op_native("__ArrayBuffer_byteLength", native_array_buffer_byte_length),
        op_native(
            "__ArrayBuffer_maxByteLength",
            native_array_buffer_max_byte_length,
        ),
        op_native("__ArrayBuffer_resizable", native_array_buffer_resizable),
        op_native("__ArrayBuffer_detached", native_array_buffer_detached),
        // Prototype methods
        op_native("__ArrayBuffer_slice", native_array_buffer_slice),
        op_native("__ArrayBuffer_transfer", native_array_buffer_transfer),
        op_native(
            "__ArrayBuffer_transferToFixedLength",
            native_array_buffer_transfer_to_fixed_length,
        ),
        op_native("__ArrayBuffer_resize", native_array_buffer_resize),
        // Static methods
        op_native("__ArrayBuffer_isView", native_array_buffer_is_view),
    ]
}

// ============================================================================
// Constructor
// ============================================================================

/// Create a new ArrayBuffer
/// Args: [byteLength, maxByteLength?]
/// Returns the ArrayBuffer value
fn native_array_buffer_create(
    args: &[VmValue],
    _mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, VmError> {
    let byte_length = args
        .first()
        .and_then(|v| v.as_number())
        .map(|n| n as usize)
        .unwrap_or(0);

    let max_byte_length = args.get(1).and_then(|v| {
        if v.is_undefined() {
            None
        } else {
            v.as_number().map(|n| n as usize)
        }
    });

    let ab = if let Some(max) = max_byte_length {
        if byte_length > max {
            return Err(VmError::range_error("byteLength exceeds maxByteLength"));
        }
        JsArrayBuffer::new_resizable(byte_length, max, None, _mm)
    } else {
        JsArrayBuffer::new(byte_length, None, _mm)
    };

    Ok(VmValue::array_buffer(Arc::new(ab)))
}

// ============================================================================
// Prototype Getters
// ============================================================================

/// Get the byteLength of an ArrayBuffer
/// Args: [arrayBuffer]
fn native_array_buffer_byte_length(
    args: &[VmValue],
    _mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, VmError> {
    let ab = args
        .first()
        .and_then(|v| v.as_array_buffer())
        .ok_or("TypeError: not an ArrayBuffer")?;

    Ok(VmValue::number(ab.byte_length() as f64))
}

/// Get the maxByteLength of a resizable ArrayBuffer
/// Args: [arrayBuffer]
fn native_array_buffer_max_byte_length(
    args: &[VmValue],
    _mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, VmError> {
    let ab = args
        .first()
        .and_then(|v| v.as_array_buffer())
        .ok_or("TypeError: not an ArrayBuffer")?;

    match ab.max_byte_length() {
        Some(max) => Ok(VmValue::number(max as f64)),
        None => Ok(VmValue::number(ab.byte_length() as f64)),
    }
}

/// Check if the ArrayBuffer is resizable
/// Args: [arrayBuffer]
fn native_array_buffer_resizable(
    args: &[VmValue],
    _mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, VmError> {
    let ab = args
        .first()
        .and_then(|v| v.as_array_buffer())
        .ok_or("TypeError: not an ArrayBuffer")?;

    Ok(VmValue::boolean(ab.is_resizable()))
}

/// Check if the ArrayBuffer is detached
/// Args: [arrayBuffer]
fn native_array_buffer_detached(
    args: &[VmValue],
    _mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, VmError> {
    let ab = args
        .first()
        .and_then(|v| v.as_array_buffer())
        .ok_or("TypeError: not an ArrayBuffer")?;

    Ok(VmValue::boolean(ab.is_detached()))
}

// ============================================================================
// Prototype Methods
// ============================================================================

/// Slice the ArrayBuffer
/// Args: [arrayBuffer, begin?, end?]
fn native_array_buffer_slice(
    args: &[VmValue],
    _mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, VmError> {
    let ab = args
        .first()
        .and_then(|v| v.as_array_buffer())
        .ok_or("TypeError: not an ArrayBuffer")?;

    if ab.is_detached() {
        return Err(VmError::type_error("ArrayBuffer is detached"));
    }

    let len = ab.byte_length() as i64;

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
        .map(|n| n as i64)
        .unwrap_or(len);

    // Handle negative indices
    let start = if begin < 0 {
        (len + begin).max(0) as usize
    } else {
        (begin as usize).min(len as usize)
    };

    let end_pos = if end < 0 {
        (len + end).max(0) as usize
    } else {
        (end as usize).min(len as usize)
    };

    let new_ab = ab
        .slice(start, end_pos)
        .ok_or("TypeError: ArrayBuffer is detached")?;

    Ok(VmValue::array_buffer(Arc::new(new_ab)))
}

/// Transfer the ArrayBuffer (detaches the original)
/// Args: [arrayBuffer]
fn native_array_buffer_transfer(
    args: &[VmValue],
    _mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, VmError> {
    let ab = args
        .first()
        .and_then(|v| v.as_array_buffer())
        .ok_or("TypeError: not an ArrayBuffer")?;

    if ab.is_detached() {
        return Err(VmError::type_error("ArrayBuffer is already detached"));
    }

    let new_ab = ab.transfer().ok_or("TypeError: ArrayBuffer is detached")?;

    Ok(VmValue::array_buffer(Arc::new(new_ab)))
}

/// Transfer the ArrayBuffer to a fixed-length buffer
/// Args: [arrayBuffer, newLength?]
fn native_array_buffer_transfer_to_fixed_length(
    args: &[VmValue],
    _mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, VmError> {
    let ab = args
        .first()
        .and_then(|v| v.as_array_buffer())
        .ok_or("TypeError: not an ArrayBuffer")?;

    if ab.is_detached() {
        return Err(VmError::type_error("ArrayBuffer is already detached"));
    }

    let new_length = args
        .get(1)
        .and_then(|v| {
            if v.is_undefined() {
                None
            } else {
                v.as_number()
            }
        })
        .map(|n| n as usize)
        .unwrap_or_else(|| ab.byte_length());

    let new_ab = ab
        .transfer_to_fixed_length(new_length)
        .ok_or("TypeError: ArrayBuffer is detached")?;

    Ok(VmValue::array_buffer(Arc::new(new_ab)))
}

/// Resize a resizable ArrayBuffer
/// Args: [arrayBuffer, newLength]
fn native_array_buffer_resize(
    args: &[VmValue],
    _mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, VmError> {
    let ab = args
        .first()
        .and_then(|v| v.as_array_buffer())
        .ok_or("TypeError: not an ArrayBuffer")?;

    let new_length = args
        .get(1)
        .and_then(|v| v.as_number())
        .map(|n| n as usize)
        .ok_or("TypeError: newLength is required")?;

    ab.resize(new_length)
        .map_err(|e| format!("TypeError: {}", e))?;

    Ok(VmValue::undefined())
}

// ============================================================================
// Static Methods
// ============================================================================

/// ArrayBuffer.isView(arg)
/// Returns true if arg is a TypedArray view or DataView
/// Args: [arg]
fn native_array_buffer_is_view(
    args: &[VmValue],
    _mm: Arc<memory::MemoryManager>,
) -> Result<VmValue, VmError> {
    // Check for TypedArray or DataView
    let is_view = args
        .first()
        .map(|v| v.is_typed_array() || v.is_data_view())
        .unwrap_or(false);

    Ok(VmValue::boolean(is_view))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mm() -> Arc<memory::MemoryManager> {
        Arc::new(memory::MemoryManager::test())
    }

    #[test]
    fn test_create_array_buffer() {
        let result = native_array_buffer_create(&[VmValue::number(16.0)], mm()).unwrap();
        assert!(result.is_array_buffer());
        let ab = result.as_array_buffer().unwrap();
        assert_eq!(ab.byte_length(), 16);
        assert!(!ab.is_detached());
    }

    #[test]
    fn test_create_resizable_array_buffer() {
        let result =
            native_array_buffer_create(&[VmValue::number(8.0), VmValue::number(16.0)], mm())
                .unwrap();
        let ab = result.as_array_buffer().unwrap();
        assert_eq!(ab.byte_length(), 8);
        assert_eq!(ab.max_byte_length(), Some(16));
        assert!(ab.is_resizable());
    }

    #[test]
    fn test_byte_length() {
        let ab = native_array_buffer_create(&[VmValue::number(16.0)], mm()).unwrap();
        let len = native_array_buffer_byte_length(&[ab], mm()).unwrap();
        assert_eq!(len.as_number(), Some(16.0));
    }

    #[test]
    fn test_slice() {
        let ab = native_array_buffer_create(&[VmValue::number(16.0)], mm()).unwrap();
        let slice =
            native_array_buffer_slice(&[ab, VmValue::number(4.0), VmValue::number(12.0)], mm())
                .unwrap();
        let slice_ab = slice.as_array_buffer().unwrap();
        assert_eq!(slice_ab.byte_length(), 8);
    }

    #[test]
    fn test_transfer() {
        let ab = native_array_buffer_create(&[VmValue::number(16.0)], mm()).unwrap();
        let new_ab = native_array_buffer_transfer(&[ab.clone()], mm()).unwrap();

        // Original should be detached
        let detached = native_array_buffer_detached(&[ab], mm()).unwrap();
        assert_eq!(detached.as_boolean(), Some(true));

        // New should not be detached
        let new_detached = native_array_buffer_detached(&[new_ab.clone()], mm()).unwrap();
        assert_eq!(new_detached.as_boolean(), Some(false));

        // New should have same length
        let new_len = native_array_buffer_byte_length(&[new_ab], mm()).unwrap();
        assert_eq!(new_len.as_number(), Some(16.0));
    }

    #[test]
    fn test_resize() {
        let ab = native_array_buffer_create(&[VmValue::number(8.0), VmValue::number(16.0)], mm())
            .unwrap();

        // Resize to larger
        native_array_buffer_resize(&[ab.clone(), VmValue::number(12.0)], mm()).unwrap();
        let len = native_array_buffer_byte_length(&[ab.clone()], mm()).unwrap();
        assert_eq!(len.as_number(), Some(12.0));

        // Resize to smaller
        native_array_buffer_resize(&[ab.clone(), VmValue::number(4.0)], mm()).unwrap();
        let len = native_array_buffer_byte_length(&[ab], mm()).unwrap();
        assert_eq!(len.as_number(), Some(4.0));
    }

    #[test]
    fn test_resize_exceeds_max() {
        let ab = native_array_buffer_create(&[VmValue::number(8.0), VmValue::number(16.0)], mm())
            .unwrap();

        let result = native_array_buffer_resize(&[ab, VmValue::number(20.0)], mm());
        assert!(result.is_err());
    }
}
