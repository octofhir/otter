//! SharedArrayBuffer implementation
//!
//! SharedArrayBuffer allows sharing raw binary data between workers.
//! Unlike ArrayBuffer, SharedArrayBuffer can be shared across threads.

use std::sync::atomic::{AtomicU8, Ordering};

/// A shared array buffer that can be transferred between workers
///
/// This is thread-safe and can be cloned to share between workers.
/// The underlying memory is shared (not copied) when transferred.
#[derive(Debug)]
pub struct SharedArrayBuffer {
    /// The underlying atomic byte array
    data: Box<[AtomicU8]>,
}

impl otter_vm_gc::GcTraceable for SharedArrayBuffer {
    const NEEDS_TRACE: bool = false;
    fn trace(&self, _tracer: &mut dyn FnMut(*const otter_vm_gc::GcHeader)) {
        // SharedArrayBuffer contains only Box<[AtomicU8]>, no GC references
    }
}

impl SharedArrayBuffer {
    /// Create a new SharedArrayBuffer with the specified byte length
    pub fn new(byte_length: usize) -> Self {
        let data: Vec<AtomicU8> = (0..byte_length).map(|_| AtomicU8::new(0)).collect();
        Self {
            data: data.into_boxed_slice(),
        }
    }

    /// Get the byte length of this buffer
    #[inline]
    pub fn byte_length(&self) -> usize {
        self.data.len()
    }

    /// Read a byte at the given index
    #[inline]
    pub fn get(&self, index: usize) -> Option<u8> {
        self.data.get(index).map(|v| v.load(Ordering::SeqCst))
    }

    /// Write a byte at the given index
    #[inline]
    pub fn set(&self, index: usize, value: u8) -> bool {
        if let Some(cell) = self.data.get(index) {
            cell.store(value, Ordering::SeqCst);
            true
        } else {
            false
        }
    }

    // === Atomics operations ===

    /// Atomics.load - Load a value atomically
    #[inline]
    pub fn atomic_load(&self, index: usize) -> Option<u8> {
        self.data.get(index).map(|v| v.load(Ordering::SeqCst))
    }

    /// Atomics.store - Store a value atomically
    #[inline]
    pub fn atomic_store(&self, index: usize, value: u8) -> bool {
        if let Some(cell) = self.data.get(index) {
            cell.store(value, Ordering::SeqCst);
            true
        } else {
            false
        }
    }

    /// Atomics.add - Add and return the old value
    #[inline]
    pub fn atomic_add(&self, index: usize, value: u8) -> Option<u8> {
        self.data
            .get(index)
            .map(|v| v.fetch_add(value, Ordering::SeqCst))
    }

    /// Atomics.sub - Subtract and return the old value
    #[inline]
    pub fn atomic_sub(&self, index: usize, value: u8) -> Option<u8> {
        self.data
            .get(index)
            .map(|v| v.fetch_sub(value, Ordering::SeqCst))
    }

    /// Atomics.and - Bitwise AND and return the old value
    #[inline]
    pub fn atomic_and(&self, index: usize, value: u8) -> Option<u8> {
        self.data
            .get(index)
            .map(|v| v.fetch_and(value, Ordering::SeqCst))
    }

    /// Atomics.or - Bitwise OR and return the old value
    #[inline]
    pub fn atomic_or(&self, index: usize, value: u8) -> Option<u8> {
        self.data
            .get(index)
            .map(|v| v.fetch_or(value, Ordering::SeqCst))
    }

    /// Atomics.xor - Bitwise XOR and return the old value
    #[inline]
    pub fn atomic_xor(&self, index: usize, value: u8) -> Option<u8> {
        self.data
            .get(index)
            .map(|v| v.fetch_xor(value, Ordering::SeqCst))
    }

    /// Atomics.exchange - Exchange and return the old value
    #[inline]
    pub fn atomic_exchange(&self, index: usize, value: u8) -> Option<u8> {
        self.data
            .get(index)
            .map(|v| v.swap(value, Ordering::SeqCst))
    }

    /// Atomics.compareExchange - Compare and exchange, return old value
    #[inline]
    pub fn atomic_compare_exchange(
        &self,
        index: usize,
        expected: u8,
        replacement: u8,
    ) -> Option<u8> {
        self.data.get(index).map(|v| {
            match v.compare_exchange(expected, replacement, Ordering::SeqCst, Ordering::SeqCst) {
                Ok(old) | Err(old) => old,
            }
        })
    }

    /// Get raw pointer to data for typed array views
    ///
    /// # Safety
    /// Caller must ensure proper synchronization when accessing the data.
    #[inline]
    pub fn as_ptr(&self) -> *const AtomicU8 {
        self.data.as_ptr()
    }

    /// Read bytes into a slice
    pub fn read_bytes(&self, offset: usize, dest: &mut [u8]) -> bool {
        if offset + dest.len() > self.data.len() {
            return false;
        }
        for (i, byte) in dest.iter_mut().enumerate() {
            *byte = self.data[offset + i].load(Ordering::SeqCst);
        }
        true
    }

    /// Write bytes from a slice
    pub fn write_bytes(&self, offset: usize, src: &[u8]) -> bool {
        if offset + src.len() > self.data.len() {
            return false;
        }
        for (i, &byte) in src.iter().enumerate() {
            self.data[offset + i].store(byte, Ordering::SeqCst);
        }
        true
    }
}

// SAFETY: SharedArrayBuffer uses AtomicU8 for all data access
unsafe impl Send for SharedArrayBuffer {}
unsafe impl Sync for SharedArrayBuffer {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gc::GcRef;
    
    use std::thread;

    #[test]
    fn test_create_shared_array_buffer() {
        let sab = SharedArrayBuffer::new(16);
        assert_eq!(sab.byte_length(), 16);
    }

    #[test]
    fn test_get_set() {
        let sab = SharedArrayBuffer::new(4);
        assert!(sab.set(0, 42));
        assert_eq!(sab.get(0), Some(42));
        assert_eq!(sab.get(4), None); // Out of bounds
    }

    #[test]
    fn test_atomics_add() {
        let sab = SharedArrayBuffer::new(1);
        sab.set(0, 10);
        let old = sab.atomic_add(0, 5);
        assert_eq!(old, Some(10));
        assert_eq!(sab.get(0), Some(15));
    }

    #[test]
    fn test_atomics_compare_exchange() {
        let sab = SharedArrayBuffer::new(1);
        sab.set(0, 10);

        // Successful exchange
        let old = sab.atomic_compare_exchange(0, 10, 20);
        assert_eq!(old, Some(10));
        assert_eq!(sab.get(0), Some(20));

        // Failed exchange (expected doesn't match)
        let old = sab.atomic_compare_exchange(0, 10, 30);
        assert_eq!(old, Some(20)); // Returns current value
        assert_eq!(sab.get(0), Some(20)); // Unchanged
    }

    #[test]
    fn test_shared_between_threads() {
        let _rt = crate::runtime::VmRuntime::new();
        let sab = GcRef::new(SharedArrayBuffer::new(1));
        let sab_clone = sab; // GcRef is Copy

        let handle = thread::spawn(move || {
            sab_clone.set(0, 42);
        });

        handle.join().unwrap();
        assert_eq!(sab.get(0), Some(42));
    }

    #[test]
    fn test_concurrent_atomics() {
        let _rt = crate::runtime::VmRuntime::new();
        let sab = GcRef::new(SharedArrayBuffer::new(1));
        let mut handles = vec![];

        // Spawn 10 threads that each add 1 to the counter
        for _ in 0..10 {
            let sab_clone = sab; // GcRef is Copy
            handles.push(thread::spawn(move || {
                for _ in 0..100 {
                    sab_clone.atomic_add(0, 1);
                }
            }));
        }

        for handle in handles {
            handle.join().unwrap();
        }

        // Should have added 1 * 100 * 10 = 1000, but u8 overflows
        // With wrapping: (1000 % 256) = 232
        assert_eq!(sab.get(0), Some(232));
    }

    #[test]
    fn test_read_write_bytes() {
        let sab = SharedArrayBuffer::new(8);
        let src = [1, 2, 3, 4];
        assert!(sab.write_bytes(2, &src));

        let mut dest = [0u8; 4];
        assert!(sab.read_bytes(2, &mut dest));
        assert_eq!(dest, [1, 2, 3, 4]);
    }
}
