//! ArrayBuffer implementation
//!
//! ArrayBuffer is the foundation for TypedArrays and binary data handling.
//! Unlike SharedArrayBuffer, regular ArrayBuffer is not shareable between threads.

use crate::gc::GcRef;
use crate::object::JsObject;
use crate::value::Value;
use std::cell::RefCell;
use std::sync::Arc;

/// A JavaScript ArrayBuffer
///
/// ArrayBuffer represents a raw buffer of binary data. It can be detached
/// (transferred) and optionally resizable (ES2024).
#[derive(Debug)]
pub struct JsArrayBuffer {
    /// The object portion (properties, prototype, etc.)
    pub object: GcRef<JsObject>,
    /// The underlying byte data. None if detached.
    data: RefCell<Option<Vec<u8>>>,
    /// Maximum byte length for resizable buffers (ES2024)
    max_byte_length: Option<usize>,
}

// SAFETY: JsArrayBuffer is only accessed from the single VM thread.
// Thread confinement is enforced by the Isolate abstraction.
// (SharedArrayBuffer correctly uses AtomicU8 and is NOT changed.)
unsafe impl Send for JsArrayBuffer {}
unsafe impl Sync for JsArrayBuffer {}

impl otter_vm_gc::GcTraceable for JsArrayBuffer {
    const NEEDS_TRACE: bool = true;
    fn trace(&self, tracer: &mut dyn FnMut(*const otter_vm_gc::GcHeader)) {
        // Trace the object field (GC-managed)
        tracer(self.object.header() as *const _);
    }
}

impl JsArrayBuffer {
    /// Create a new ArrayBuffer with the specified byte length
    pub fn new(
        byte_length: usize,
        prototype: Option<GcRef<JsObject>>,
        memory_manager: Arc<crate::memory::MemoryManager>,
    ) -> Self {
        let proto_value = prototype.map(Value::object).unwrap_or_else(Value::null);
        let object = GcRef::new(JsObject::new(proto_value, memory_manager));
        Self {
            object,
            data: RefCell::new(Some(vec![0; byte_length])),
            max_byte_length: None,
        }
    }

    /// Create a new resizable ArrayBuffer (ES2024)
    pub fn new_resizable(
        byte_length: usize,
        max_byte_length: usize,
        prototype: Option<GcRef<JsObject>>,
        memory_manager: Arc<crate::memory::MemoryManager>,
    ) -> Self {
        let proto_value = prototype.map(Value::object).unwrap_or_else(Value::null);
        let object = GcRef::new(JsObject::new(proto_value, memory_manager));
        Self {
            object,
            data: RefCell::new(Some(vec![0; byte_length])),
            max_byte_length: Some(max_byte_length),
        }
    }

    /// Check if the buffer is detached
    pub fn is_detached(&self) -> bool {
        self.data.borrow().is_none()
    }

    /// Detach the buffer (for transfer operations)
    pub fn detach(&self) {
        *self.data.borrow_mut() = None;
    }

    /// Get the byte length (0 if detached)
    pub fn byte_length(&self) -> usize {
        self.data.borrow().as_ref().map_or(0, |d| d.len())
    }

    /// Get the max byte length for resizable buffers
    pub fn max_byte_length(&self) -> Option<usize> {
        self.max_byte_length
    }

    /// Check if this is a resizable buffer
    pub fn is_resizable(&self) -> bool {
        self.max_byte_length.is_some()
    }

    /// Transfer the buffer contents to a new ArrayBuffer
    /// The original buffer becomes detached.
    pub fn transfer(&self) -> Option<JsArrayBuffer> {
        let data = self.data.borrow_mut().take()?;
        // Create new object with same memory manager but fresh object identity/proto
        // Note: Callers might need to set correct proto if not default
        let mm = self.object.memory_manager().clone();
        let object = GcRef::new(JsObject::new(Value::null(), mm));
        Some(JsArrayBuffer {
            object,
            data: RefCell::new(Some(data)),
            max_byte_length: self.max_byte_length,
        })
    }

    /// Transfer to a new size (ES2024)
    pub fn transfer_to_fixed_length(&self, new_length: usize) -> Option<JsArrayBuffer> {
        let mut guard = self.data.borrow_mut();
        let old_data = guard.take()?;
        let mut new_data = vec![0u8; new_length];
        let copy_len = old_data.len().min(new_length);
        new_data[..copy_len].copy_from_slice(&old_data[..copy_len]);

        let mm = self.object.memory_manager().clone();
        let object = GcRef::new(JsObject::new(Value::null(), mm));

        Some(JsArrayBuffer {
            object,
            data: RefCell::new(Some(new_data)),
            max_byte_length: None, // Fixed length
        })
    }

    /// Resize the buffer (only for resizable buffers)
    pub fn resize(&self, new_length: usize) -> Result<(), &'static str> {
        let max = self.max_byte_length.ok_or("ArrayBuffer is not resizable")?;
        if new_length > max {
            return Err("new length exceeds maxByteLength");
        }
        let mut guard = self.data.borrow_mut();
        let data = guard.as_mut().ok_or("ArrayBuffer is detached")?;
        data.resize(new_length, 0);
        Ok(())
    }

    /// Slice the buffer to create a new ArrayBuffer
    pub fn slice(&self, start: usize, end: usize) -> Option<JsArrayBuffer> {
        let guard = self.data.borrow();
        let data = guard.as_ref()?;
        let len = data.len();
        let actual_start = start.min(len);
        let actual_end = end.min(len);
        let slice_len = actual_end.saturating_sub(actual_start);
        let mut new_data = vec![0u8; slice_len];
        if slice_len > 0 {
            new_data.copy_from_slice(&data[actual_start..actual_end]);
        }

        let mm = self.object.memory_manager().clone();
        let object = GcRef::new(JsObject::new(Value::null(), mm));

        Some(JsArrayBuffer {
            object,
            data: RefCell::new(Some(new_data)),
            max_byte_length: None,
        })
    }

    /// Read a byte at the given index
    pub fn get(&self, index: usize) -> Option<u8> {
        self.data.borrow().as_ref()?.get(index).copied()
    }

    /// Write a byte at the given index
    pub fn set(&self, index: usize, value: u8) -> bool {
        if let Some(data) = self.data.borrow_mut().as_mut() {
            if let Some(cell) = data.get_mut(index) {
                *cell = value;
                return true;
            }
        }
        false
    }

    /// Read bytes into a slice
    pub fn read_bytes(&self, offset: usize, dest: &mut [u8]) -> bool {
        let guard = self.data.borrow();
        if let Some(data) = guard.as_ref() {
            if offset + dest.len() <= data.len() {
                dest.copy_from_slice(&data[offset..offset + dest.len()]);
                return true;
            }
        }
        false
    }

    /// Write bytes from a slice
    pub fn write_bytes(&self, offset: usize, src: &[u8]) -> bool {
        let mut guard = self.data.borrow_mut();
        if let Some(data) = guard.as_mut() {
            if offset + src.len() <= data.len() {
                data[offset..offset + src.len()].copy_from_slice(src);
                return true;
            }
        }
        false
    }

    /// Get raw access to the data (for TypedArray views)
    /// Returns None if detached.
    pub fn with_data<F, R>(&self, f: F) -> Option<R>
    where
        F: FnOnce(&[u8]) -> R,
    {
        let guard = self.data.borrow();
        guard.as_ref().map(|d| f(d))
    }

    /// Get mutable raw access to the data
    /// Returns None if detached.
    pub fn with_data_mut<F, R>(&self, f: F) -> Option<R>
    where
        F: FnOnce(&mut Vec<u8>) -> R,
    {
        let mut guard = self.data.borrow_mut();
        guard.as_mut().map(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::MemoryManager;

    fn make_mm() -> Arc<MemoryManager> {
        Arc::new(MemoryManager::new(1024 * 1024))
    }

    #[test]
    fn test_create_array_buffer() {
        let mm = make_mm();
        let ab = JsArrayBuffer::new(16, None, mm);
        assert_eq!(ab.byte_length(), 16);
        assert!(!ab.is_detached());
        assert!(!ab.is_resizable());
    }

    #[test]
    fn test_create_resizable() {
        let mm = make_mm();
        let ab = JsArrayBuffer::new_resizable(8, 16, None, mm);
        assert_eq!(ab.byte_length(), 8);
        assert_eq!(ab.max_byte_length(), Some(16));
        assert!(ab.is_resizable());
    }

    #[test]
    fn test_get_set() {
        let mm = make_mm();
        let ab = JsArrayBuffer::new(4, None, mm);
        assert!(ab.set(0, 42));
        assert_eq!(ab.get(0), Some(42));
        assert_eq!(ab.get(4), None); // Out of bounds
    }

    #[test]
    fn test_detach() {
        let mm = make_mm();
        let ab = JsArrayBuffer::new(8, None, mm);
        assert!(!ab.is_detached());
        ab.detach();
        assert!(ab.is_detached());
        assert_eq!(ab.byte_length(), 0);
    }

    #[test]
    fn test_transfer() {
        let mm = make_mm();
        let ab = JsArrayBuffer::new(8, None, mm);
        ab.set(0, 42);
        let new_ab = ab.transfer().unwrap();
        assert!(ab.is_detached());
        assert!(!new_ab.is_detached());
        assert_eq!(new_ab.get(0), Some(42));
    }

    #[test]
    fn test_slice() {
        let mm = make_mm();
        let ab = JsArrayBuffer::new(16, None, mm);
        ab.set(4, 1);
        ab.set(5, 2);
        ab.set(6, 3);
        ab.set(7, 4);

        let slice = ab.slice(4, 8).unwrap();
        assert_eq!(slice.byte_length(), 4);
        assert_eq!(slice.get(0), Some(1));
        assert_eq!(slice.get(1), Some(2));
        assert_eq!(slice.get(2), Some(3));
        assert_eq!(slice.get(3), Some(4));
    }

    #[test]
    fn test_resize() {
        let mm = make_mm();
        let ab = JsArrayBuffer::new_resizable(8, 16, None, mm);
        ab.set(0, 42);

        assert!(ab.resize(12).is_ok());
        assert_eq!(ab.byte_length(), 12);
        assert_eq!(ab.get(0), Some(42)); // Data preserved

        assert!(ab.resize(20).is_err()); // Exceeds max
    }

    #[test]
    fn test_read_write_bytes() {
        let mm = make_mm();
        let ab = JsArrayBuffer::new(8, None, mm);
        let src = [1, 2, 3, 4];
        assert!(ab.write_bytes(2, &src));

        let mut dest = [0u8; 4];
        assert!(ab.read_bytes(2, &mut dest));
        assert_eq!(dest, [1, 2, 3, 4]);
    }
}
