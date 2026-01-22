//! Garbage collection support
//!
//! This module provides GC root tracking and tracing interfaces,
//! integrating with otter-vm-gc crate.

use std::any::Any;
use std::sync::Arc;

// Re-export GC types from otter-vm-gc
pub use otter_vm_gc::{
    Allocator as GcAllocator, Collector as GcCollector, GcConfig, GcHeader, GcHeap, GcObject,
    GcStats,
};

/// Trait for types that can be traced by the GC
pub trait Trace {
    /// Trace all references in this object
    fn trace(&self, tracer: &mut dyn Tracer);
}

/// Tracer interface for GC marking phase
pub trait Tracer {
    /// Mark an object as reachable
    fn mark(&mut self, obj: &dyn Any);

    /// Mark a value as reachable
    fn mark_value(&mut self, value: &crate::value::Value);

    /// Mark a GC header as reachable
    fn mark_header(&mut self, header: *const GcHeader);
}

/// A GC root - keeps values alive
pub struct GcRoot<T> {
    value: Arc<T>,
}

impl<T> GcRoot<T> {
    /// Create a new GC root
    pub fn new(value: T) -> Self {
        Self {
            value: Arc::new(value),
        }
    }

    /// Get reference to the value
    pub fn get(&self) -> &T {
        &self.value
    }

    /// Get the Arc
    pub fn arc(&self) -> Arc<T> {
        self.value.clone()
    }
}

impl<T> Clone for GcRoot<T> {
    fn clone(&self) -> Self {
        Self {
            value: self.value.clone(),
        }
    }
}

impl<T> std::ops::Deref for GcRoot<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.value
    }
}

/// Handle to a GC-managed object
///
/// This is a reference-counted handle that keeps objects alive.
/// The actual GC will use these for root tracking.
pub type GcHandle<T> = Arc<T>;

/// Create a new GC-managed handle
pub fn gc_alloc<T>(value: T) -> GcHandle<T> {
    Arc::new(value)
}

/// Safe handle to a raw GC pointer
///
/// Prevents collection while held.
pub struct Handle<T> {
    ptr: *const T,
    _marker: std::marker::PhantomData<T>,
}

impl<T> Handle<T> {
    /// Create a new handle
    ///
    /// # Safety
    /// The pointer must be valid and point to a live object.
    pub unsafe fn new(ptr: *const T) -> Self {
        Self {
            ptr,
            _marker: std::marker::PhantomData,
        }
    }

    /// Get reference to underlying object
    ///
    /// # Safety
    /// The pointer must still be valid.
    pub unsafe fn get(&self) -> &T {
        unsafe { &*self.ptr }
    }

    /// Get mutable reference
    ///
    /// # Safety
    /// Must have exclusive access.
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn get_mut(&self) -> &mut T {
        unsafe { &mut *(self.ptr as *mut T) }
    }

    /// Get raw pointer
    pub fn as_ptr(&self) -> *const T {
        self.ptr
    }
}

// Handle is Send if T is Send
unsafe impl<T: Send> Send for Handle<T> {}
// Handle is Sync if T is Sync
unsafe impl<T: Sync> Sync for Handle<T> {}

// Implement Trace for Value
impl Trace for crate::value::Value {
    fn trace(&self, tracer: &mut dyn Tracer) {
        tracer.mark_value(self);
    }
}

// Implement Trace for JsObject
impl Trace for crate::object::JsObject {
    fn trace(&self, tracer: &mut dyn Tracer) {
        // Trace all property values
        for key in self.own_keys() {
            if let Some(value) = self.get(&key) {
                tracer.mark_value(&value);
            }
        }

        // Trace prototype
        if let Some(proto) = self.prototype() {
            tracer.mark(proto.as_ref());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_gc_root() {
        let root = GcRoot::new(42);
        assert_eq!(*root.get(), 42);
    }

    #[test]
    fn test_gc_handle() {
        let handle = gc_alloc("hello".to_string());
        assert_eq!(handle.as_str(), "hello");
    }

    #[test]
    fn test_gc_heap_integration() {
        let heap = GcHeap::new();
        assert_eq!(heap.allocated(), 0);

        let ptr = heap.allocate_old(100);
        assert!(ptr.is_some());
    }

    #[test]
    fn test_gc_collector_integration() {
        let heap = GcHeap::new();
        let mut collector = GcCollector::new(heap);
        collector.collect(&[]);
        assert_eq!(collector.stats().collections, 1);
    }
}
