//! TypedArray implementation
//!
//! TypedArrays are views over ArrayBuffer, providing typed access to binary data.
//! All 11 types share common implementation via TypedArrayKind.

use crate::array_buffer::JsArrayBuffer;
use crate::gc::GcRef;
use crate::memory::MemoryManager;
use crate::object::JsObject;
use crate::value::Value;
use std::sync::Arc;

/// The kind of TypedArray - determines element size and interpretation
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypedArrayKind {
    /// Int8Array - 8-bit signed integers
    Int8,
    /// Uint8Array - 8-bit unsigned integers
    Uint8,
    /// Uint8ClampedArray - 8-bit unsigned integers (clamped)
    Uint8Clamped,
    /// Int16Array - 16-bit signed integers
    Int16,
    /// Uint16Array - 16-bit unsigned integers
    Uint16,
    /// Int32Array - 32-bit signed integers
    Int32,
    /// Uint32Array - 32-bit unsigned integers
    Uint32,
    /// Float32Array - 32-bit floating point
    Float32,
    /// Float64Array - 64-bit floating point
    Float64,
    /// BigInt64Array - 64-bit signed integers (BigInt)
    BigInt64,
    /// BigUint64Array - 64-bit unsigned integers (BigInt)
    BigUint64,
}

impl TypedArrayKind {
    /// Get the byte size of each element
    pub fn element_size(&self) -> usize {
        match self {
            TypedArrayKind::Int8 | TypedArrayKind::Uint8 | TypedArrayKind::Uint8Clamped => 1,
            TypedArrayKind::Int16 | TypedArrayKind::Uint16 => 2,
            TypedArrayKind::Int32 | TypedArrayKind::Uint32 | TypedArrayKind::Float32 => 4,
            TypedArrayKind::Float64 | TypedArrayKind::BigInt64 | TypedArrayKind::BigUint64 => 8,
        }
    }

    /// Get the name of this TypedArray type
    pub fn name(&self) -> &'static str {
        match self {
            TypedArrayKind::Int8 => "Int8Array",
            TypedArrayKind::Uint8 => "Uint8Array",
            TypedArrayKind::Uint8Clamped => "Uint8ClampedArray",
            TypedArrayKind::Int16 => "Int16Array",
            TypedArrayKind::Uint16 => "Uint16Array",
            TypedArrayKind::Int32 => "Int32Array",
            TypedArrayKind::Uint32 => "Uint32Array",
            TypedArrayKind::Float32 => "Float32Array",
            TypedArrayKind::Float64 => "Float64Array",
            TypedArrayKind::BigInt64 => "BigInt64Array",
            TypedArrayKind::BigUint64 => "BigUint64Array",
        }
    }

    /// Check if this is a BigInt typed array
    pub fn is_bigint(&self) -> bool {
        matches!(self, TypedArrayKind::BigInt64 | TypedArrayKind::BigUint64)
    }
}

/// A JavaScript TypedArray
///
/// TypedArray is a view over an ArrayBuffer, providing typed access to binary data.
/// It does not copy data - it references the underlying buffer.
#[derive(Debug)]
pub struct JsTypedArray {
    /// Associated JavaScript object (for properties and prototype)
    pub object: GcRef<JsObject>,
    /// The underlying ArrayBuffer
    buffer: GcRef<JsArrayBuffer>,
    /// Byte offset into the buffer
    byte_offset: usize,
    /// Number of elements (not bytes)
    length: usize,
    /// The kind of typed array
    kind: TypedArrayKind,
}

impl otter_vm_gc::GcTraceable for JsTypedArray {
    const NEEDS_TRACE: bool = true;
    const TYPE_ID: u8 = otter_vm_gc::object::tags::TYPED_ARRAY;
    fn trace(&self, tracer: &mut dyn FnMut(*const otter_vm_gc::GcHeader)) {
        // Trace the object field
        tracer(self.object.header() as *const _);
        // Trace the buffer's object field
        tracer(self.buffer.object.header() as *const _);
    }
}

impl JsTypedArray {
    /// Create a new TypedArray view over an ArrayBuffer
    pub fn new(
        object: GcRef<JsObject>,
        buffer: GcRef<JsArrayBuffer>,
        kind: TypedArrayKind,
        byte_offset: usize,
        length: usize,
    ) -> Result<Self, &'static str> {
        let elem_size = kind.element_size();

        // Validate alignment
        if byte_offset % elem_size != 0 {
            return Err("byte offset must be aligned to element size");
        }

        // Validate bounds
        let byte_length = length
            .checked_mul(elem_size)
            .ok_or("TypedArray length overflow")?;
        if byte_offset + byte_length > buffer.byte_length() {
            return Err("TypedArray would extend past end of buffer");
        }

        Ok(Self {
            object,
            buffer,
            byte_offset,
            length,
            kind,
        })
    }

    /// Create a new TypedArray with its own buffer
    pub fn with_length(
        kind: TypedArrayKind,
        length: usize,
        prototype: Option<GcRef<JsObject>>,
        memory_manager: Arc<MemoryManager>,
    ) -> Self {
        let byte_length = length * kind.element_size();
        let buffer = GcRef::new(JsArrayBuffer::new(
            byte_length,
            None,
            memory_manager.clone(),
        ));
        let proto_value = prototype.map(Value::object).unwrap_or_else(Value::null);
        let object = GcRef::new(JsObject::new(proto_value, memory_manager));
        Self {
            object,
            buffer,
            byte_offset: 0,
            length,
            kind,
        }
    }

    /// Get the kind of this TypedArray
    pub fn kind(&self) -> TypedArrayKind {
        self.kind
    }

    /// Get the underlying ArrayBuffer
    pub fn buffer(&self) -> GcRef<JsArrayBuffer> {
        self.buffer
    }

    /// Get the byte offset into the buffer
    pub fn byte_offset(&self) -> usize {
        self.byte_offset
    }

    /// Get the byte length of the view
    pub fn byte_length(&self) -> usize {
        if self.buffer.is_detached() {
            0
        } else {
            self.length * self.kind.element_size()
        }
    }

    /// Get the number of elements
    pub fn length(&self) -> usize {
        if self.buffer.is_detached() {
            0
        } else {
            self.length
        }
    }

    /// Check if the underlying buffer is detached
    pub fn is_detached(&self) -> bool {
        self.buffer.is_detached()
    }

    /// Get an element as f64 (for non-BigInt arrays)
    pub fn get(&self, index: usize) -> Option<f64> {
        if self.buffer.is_detached() || index >= self.length {
            return None;
        }

        let byte_index = self.byte_offset + index * self.kind.element_size();

        self.buffer.with_data(|data| {
            let bytes = &data[byte_index..];
            match self.kind {
                TypedArrayKind::Int8 => bytes[0] as i8 as f64,
                TypedArrayKind::Uint8 | TypedArrayKind::Uint8Clamped => bytes[0] as f64,
                TypedArrayKind::Int16 => i16::from_le_bytes([bytes[0], bytes[1]]) as f64,
                TypedArrayKind::Uint16 => u16::from_le_bytes([bytes[0], bytes[1]]) as f64,
                TypedArrayKind::Int32 => {
                    i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as f64
                }
                TypedArrayKind::Uint32 => {
                    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as f64
                }
                TypedArrayKind::Float32 => {
                    f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as f64
                }
                TypedArrayKind::Float64 => f64::from_le_bytes([
                    bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
                ]),
                // BigInt arrays return NaN for regular get - use get_bigint
                TypedArrayKind::BigInt64 | TypedArrayKind::BigUint64 => f64::NAN,
            }
        })
    }

    /// Get an element as i64 (for BigInt arrays)
    pub fn get_bigint(&self, index: usize) -> Option<i64> {
        if self.buffer.is_detached() || index >= self.length {
            return None;
        }

        let byte_index = self.byte_offset + index * self.kind.element_size();

        self.buffer.with_data(|data| {
            let bytes = &data[byte_index..];
            match self.kind {
                TypedArrayKind::BigInt64 => i64::from_le_bytes([
                    bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
                ]),
                TypedArrayKind::BigUint64 => u64::from_le_bytes([
                    bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
                ]) as i64,
                _ => 0,
            }
        })
    }

    /// Set an element from f64 (for non-BigInt arrays)
    pub fn set(&self, index: usize, value: f64) -> bool {
        if self.buffer.is_detached() || index >= self.length {
            return false;
        }

        let byte_index = self.byte_offset + index * self.kind.element_size();

        self.buffer.with_data_mut(|data| {
            let bytes = &mut data[byte_index..];
            match self.kind {
                TypedArrayKind::Int8 => {
                    bytes[0] = value as i8 as u8;
                }
                TypedArrayKind::Uint8 => {
                    bytes[0] = value as u8;
                }
                TypedArrayKind::Uint8Clamped => {
                    bytes[0] = if value.is_nan() {
                        0
                    } else if value < 0.0 {
                        0
                    } else if value > 255.0 {
                        255
                    } else {
                        value.round() as u8
                    };
                }
                TypedArrayKind::Int16 => {
                    let v = (value as i16).to_le_bytes();
                    bytes[0] = v[0];
                    bytes[1] = v[1];
                }
                TypedArrayKind::Uint16 => {
                    let v = (value as u16).to_le_bytes();
                    bytes[0] = v[0];
                    bytes[1] = v[1];
                }
                TypedArrayKind::Int32 => {
                    let v = (value as i32).to_le_bytes();
                    bytes[..4].copy_from_slice(&v);
                }
                TypedArrayKind::Uint32 => {
                    let v = (value as u32).to_le_bytes();
                    bytes[..4].copy_from_slice(&v);
                }
                TypedArrayKind::Float32 => {
                    let v = (value as f32).to_le_bytes();
                    bytes[..4].copy_from_slice(&v);
                }
                TypedArrayKind::Float64 => {
                    let v = value.to_le_bytes();
                    bytes[..8].copy_from_slice(&v);
                }
                // BigInt arrays ignore regular set - use set_bigint
                TypedArrayKind::BigInt64 | TypedArrayKind::BigUint64 => {}
            }
        });

        true
    }

    /// Set an element from i64 (for BigInt arrays)
    pub fn set_bigint(&self, index: usize, value: i64) -> bool {
        if self.buffer.is_detached() || index >= self.length {
            return false;
        }

        let byte_index = self.byte_offset + index * self.kind.element_size();

        self.buffer.with_data_mut(|data| {
            let bytes = &mut data[byte_index..];
            match self.kind {
                TypedArrayKind::BigInt64 => {
                    bytes[..8].copy_from_slice(&value.to_le_bytes());
                }
                TypedArrayKind::BigUint64 => {
                    bytes[..8].copy_from_slice(&(value as u64).to_le_bytes());
                }
                _ => {}
            }
        });

        true
    }

    /// Create a subarray view (shares the same buffer)
    pub fn subarray(&self, begin: i64, end: Option<i64>) -> Result<JsTypedArray, &'static str> {
        if self.buffer.is_detached() {
            return Err("buffer is detached");
        }

        let len = self.length as i64;

        let start = if begin < 0 {
            (len + begin).max(0) as usize
        } else {
            (begin as usize).min(self.length)
        };

        let end_pos = match end {
            Some(e) if e < 0 => (len + e).max(0) as usize,
            Some(e) => (e as usize).min(self.length),
            None => self.length,
        };

        let new_length = end_pos.saturating_sub(start);
        let new_byte_offset = self.byte_offset + start * self.kind.element_size();

        // Create new object with same prototype as original
        let prototype = self.object.prototype();
        let memory_manager = self.object.memory_manager().clone();
        let object = GcRef::new(JsObject::new(prototype, memory_manager));

        Ok(JsTypedArray {
            object,
            buffer: self.buffer.clone(),
            byte_offset: new_byte_offset,
            length: new_length,
            kind: self.kind,
        })
    }

    /// Create a slice (copies data to a new buffer)
    pub fn slice(&self, begin: i64, end: Option<i64>) -> Result<JsTypedArray, &'static str> {
        if self.buffer.is_detached() {
            return Err("buffer is detached");
        }

        let len = self.length as i64;

        let start = if begin < 0 {
            (len + begin).max(0) as usize
        } else {
            (begin as usize).min(self.length)
        };

        let end_pos = match end {
            Some(e) if e < 0 => (len + e).max(0) as usize,
            Some(e) => (e as usize).min(self.length),
            None => self.length,
        };

        let new_length = end_pos.saturating_sub(start);
        let elem_size = self.kind.element_size();
        let new_byte_length = new_length * elem_size;

        // Get prototype and memory_manager from existing buffer
        let prototype = self.buffer.object.prototype().as_object();
        let memory_manager = self.buffer.object.memory_manager().clone();
        let new_buffer = GcRef::new(JsArrayBuffer::new(
            new_byte_length,
            prototype,
            memory_manager,
        ));

        // Copy data
        let src_offset = self.byte_offset + start * elem_size;
        self.buffer.with_data(|src| {
            new_buffer.write_bytes(0, &src[src_offset..src_offset + new_byte_length]);
        });

        // Create new object with same prototype as original
        let object_prototype = self.object.prototype();
        let object_memory_manager = self.object.memory_manager().clone();
        let object = GcRef::new(JsObject::new(object_prototype, object_memory_manager));

        Ok(JsTypedArray {
            object,
            buffer: new_buffer,
            byte_offset: 0,
            length: new_length,
            kind: self.kind,
        })
    }

    /// Fill the array with a value
    pub fn fill(&self, value: f64, start: Option<i64>, end: Option<i64>) -> bool {
        if self.buffer.is_detached() {
            return false;
        }

        let len = self.length as i64;

        let start_idx = match start {
            Some(s) if s < 0 => (len + s).max(0) as usize,
            Some(s) => (s as usize).min(self.length),
            None => 0,
        };

        let end_idx = match end {
            Some(e) if e < 0 => (len + e).max(0) as usize,
            Some(e) => (e as usize).min(self.length),
            None => self.length,
        };

        for i in start_idx..end_idx {
            self.set(i, value);
        }

        true
    }

    /// Copy elements within the array
    pub fn copy_within(&self, target: i64, start: i64, end: Option<i64>) -> bool {
        if self.buffer.is_detached() {
            return false;
        }

        let len = self.length as i64;

        let to = if target < 0 {
            (len + target).max(0) as usize
        } else {
            (target as usize).min(self.length)
        };

        let from = if start < 0 {
            (len + start).max(0) as usize
        } else {
            (start as usize).min(self.length)
        };

        let final_end = match end {
            Some(e) if e < 0 => (len + e).max(0) as usize,
            Some(e) => (e as usize).min(self.length),
            None => self.length,
        };

        let count = (final_end.saturating_sub(from)).min(self.length - to);

        if count == 0 {
            return true;
        }

        let elem_size = self.kind.element_size();
        let src_offset = self.byte_offset + from * elem_size;
        let dst_offset = self.byte_offset + to * elem_size;
        let byte_count = count * elem_size;

        self.buffer.with_data_mut(|data| {
            // Use copy_within for overlapping regions
            data.copy_within(src_offset..src_offset + byte_count, dst_offset);
        });

        true
    }

    /// Reverse the array in place
    pub fn reverse(&self) -> bool {
        if self.buffer.is_detached() {
            return false;
        }

        let len = self.length;
        if len <= 1 {
            return true;
        }

        let elem_size = self.kind.element_size();

        self.buffer.with_data_mut(|data| {
            let mut i = 0;
            let mut j = len - 1;
            while i < j {
                let offset_i = self.byte_offset + i * elem_size;
                let offset_j = self.byte_offset + j * elem_size;

                // Swap elements
                for k in 0..elem_size {
                    data.swap(offset_i + k, offset_j + k);
                }

                i += 1;
                j -= 1;
            }
        });

        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::MemoryManager;

    fn make_test_env() -> (Arc<MemoryManager>, crate::runtime::VmRuntime) {
        let rt = crate::runtime::VmRuntime::new();
        let mm = rt.memory_manager().clone();
        (mm, rt)
    }

    #[test]
    fn test_create_int32_array() {
        let (mm, _rt) = make_test_env();
        let buf = GcRef::new(JsArrayBuffer::new(16, None, mm.clone()));
        let object = GcRef::new(JsObject::new(Value::null(), mm));
        let arr = JsTypedArray::new(object, buf, TypedArrayKind::Int32, 0, 4).unwrap();
        assert_eq!(arr.length(), 4);
        assert_eq!(arr.byte_length(), 16);
        assert_eq!(arr.byte_offset(), 0);
    }

    #[test]
    fn test_with_length() {
        let (mm, _rt) = make_test_env();
        let arr = JsTypedArray::with_length(TypedArrayKind::Float64, 10, None, mm);
        assert_eq!(arr.length(), 10);
        assert_eq!(arr.byte_length(), 80);
    }

    #[test]
    fn test_get_set_int32() {
        let (mm, _rt) = make_test_env();
        let arr = JsTypedArray::with_length(TypedArrayKind::Int32, 4, None, mm);
        arr.set(0, 42.0);
        arr.set(1, -100.0);
        arr.set(2, 0.0);
        arr.set(3, 2147483647.0);

        assert_eq!(arr.get(0), Some(42.0));
        assert_eq!(arr.get(1), Some(-100.0));
        assert_eq!(arr.get(2), Some(0.0));
        assert_eq!(arr.get(3), Some(2147483647.0));
    }

    #[test]
    fn test_uint8_clamped() {
        let (mm, _rt) = make_test_env();
        let arr = JsTypedArray::with_length(TypedArrayKind::Uint8Clamped, 4, None, mm);
        arr.set(0, 300.0); // Should clamp to 255
        arr.set(1, -50.0); // Should clamp to 0
        arr.set(2, 127.5); // Should round to 128

        assert_eq!(arr.get(0), Some(255.0));
        assert_eq!(arr.get(1), Some(0.0));
        assert_eq!(arr.get(2), Some(128.0));
    }

    #[test]
    fn test_subarray() {
        let (mm, _rt) = make_test_env();
        let arr = JsTypedArray::with_length(TypedArrayKind::Int32, 10, None, mm);
        for i in 0..10 {
            arr.set(i, i as f64);
        }

        let sub = arr.subarray(2, Some(5)).unwrap();
        assert_eq!(sub.length(), 3);
        assert_eq!(sub.get(0), Some(2.0));
        assert_eq!(sub.get(1), Some(3.0));
        assert_eq!(sub.get(2), Some(4.0));

        // Verify it's a view (modifying subarray affects original)
        sub.set(0, 100.0);
        assert_eq!(arr.get(2), Some(100.0));
    }

    #[test]
    fn test_slice() {
        let (mm, _rt) = make_test_env();
        let arr = JsTypedArray::with_length(TypedArrayKind::Int32, 10, None, mm);
        for i in 0..10 {
            arr.set(i, i as f64);
        }

        let sliced = arr.slice(2, Some(5)).unwrap();
        assert_eq!(sliced.length(), 3);
        assert_eq!(sliced.get(0), Some(2.0));

        // Verify it's a copy (modifying slice doesn't affect original)
        sliced.set(0, 100.0);
        assert_eq!(arr.get(2), Some(2.0));
    }

    #[test]
    fn test_fill() {
        let (mm, _rt) = make_test_env();
        let arr = JsTypedArray::with_length(TypedArrayKind::Int32, 5, None, mm);
        arr.fill(42.0, None, None);

        for i in 0..5 {
            assert_eq!(arr.get(i), Some(42.0));
        }
    }

    #[test]
    fn test_reverse() {
        let (mm, _rt) = make_test_env();
        let arr = JsTypedArray::with_length(TypedArrayKind::Int32, 5, None, mm);
        for i in 0..5 {
            arr.set(i, i as f64);
        }

        arr.reverse();

        assert_eq!(arr.get(0), Some(4.0));
        assert_eq!(arr.get(1), Some(3.0));
        assert_eq!(arr.get(2), Some(2.0));
        assert_eq!(arr.get(3), Some(1.0));
        assert_eq!(arr.get(4), Some(0.0));
    }

    #[test]
    fn test_detached_buffer() {
        let (mm, _rt) = make_test_env();
        let buf = GcRef::new(JsArrayBuffer::new(16, None, mm.clone()));
        let object = GcRef::new(JsObject::new(Value::null(), mm));
        let arr = JsTypedArray::new(object, buf.clone(), TypedArrayKind::Int32, 0, 4).unwrap();

        arr.set(0, 42.0);
        assert_eq!(arr.get(0), Some(42.0));

        buf.detach();

        assert!(arr.is_detached());
        assert_eq!(arr.length(), 0);
        assert_eq!(arr.byte_length(), 0);
        assert_eq!(arr.get(0), None);
    }

    #[test]
    fn test_alignment_error() {
        let (mm, _rt) = make_test_env();
        let buf = GcRef::new(JsArrayBuffer::new(16, None, mm.clone()));
        let object = GcRef::new(JsObject::new(Value::null(), mm));
        let result = JsTypedArray::new(object, buf, TypedArrayKind::Int32, 1, 3);
        assert!(result.is_err());
    }
}
