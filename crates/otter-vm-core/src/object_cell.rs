//! Thread-confined interior mutability for VM objects.
//!
//! `ObjectCell<T>` provides safe interior mutability for the single-threaded VM,
//! backed by `RefCell<T>`. Runtime borrow checking catches overlapping mutable
//! borrows in both debug AND release builds.
//!
//! # Safety
//!
//! This type must only be accessed from a single thread. The VM enforces thread
//! confinement at the `VmRuntime`/`VmContext` level. The `Send+Sync` impls
//! are justified by this thread confinement guarantee (same pattern used by
//! `Shape::transitions` which also uses `RefCell`).

use std::cell::{Ref, RefCell, RefMut};
use std::ops::{Deref, DerefMut};

/// Thread-confined interior mutability wrapper.
///
/// Replaces `parking_lot::RwLock` in `JsObject` and similar hot-path types.
/// Uses `RefCell` internally for safe runtime borrow checking in all build modes.
pub struct ObjectCell<T> {
    value: RefCell<T>,
}

impl<T> ObjectCell<T> {
    /// Create a new `ObjectCell` with the given value.
    #[inline]
    pub fn new(value: T) -> Self {
        Self {
            value: RefCell::new(value),
        }
    }

    /// Borrow the value immutably.
    ///
    /// Panics if an exclusive borrow is active.
    #[inline]
    pub fn borrow(&self) -> ObjectCellRef<'_, T> {
        ObjectCellRef {
            inner: self.value.borrow(),
        }
    }

    /// Borrow the value mutably.
    ///
    /// Panics if any borrow (shared or exclusive) is active.
    #[inline]
    pub fn borrow_mut(&self) -> ObjectCellRefMut<'_, T> {
        ObjectCellRefMut {
            inner: self.value.borrow_mut(),
        }
    }

    /// Consume the cell and return the inner value.
    #[inline]
    pub fn into_inner(self) -> T {
        self.value.into_inner()
    }
}

// SAFETY: ObjectCell is only ever accessed from a single thread.
// The VM enforces thread confinement at the VmRuntime/VmContext level.
// We implement Send+Sync so that GcRef<JsObject> can be stored in
// types that need to be Send (e.g., across .await points in the engine).
// This is the same pattern used by Shape::transitions (RefCell + unsafe Send+Sync).
unsafe impl<T: Send> Send for ObjectCell<T> {}
unsafe impl<T: Send + Sync> Sync for ObjectCell<T> {}

impl<T: std::fmt::Debug> std::fmt::Debug for ObjectCell<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.value.try_borrow() {
            Ok(value) => f
                .debug_struct("ObjectCell")
                .field("value", &*value)
                .finish(),
            Err(_) => f
                .debug_struct("ObjectCell")
                .field("value", &"<borrowed>")
                .finish(),
        }
    }
}

/// Immutable borrow guard for `ObjectCell<T>`.
pub struct ObjectCellRef<'a, T> {
    inner: Ref<'a, T>,
}

impl<T> Deref for ObjectCellRef<'_, T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &T {
        &self.inner
    }
}

/// Mutable borrow guard for `ObjectCell<T>`.
pub struct ObjectCellRefMut<'a, T> {
    inner: RefMut<'a, T>,
}

impl<T> Deref for ObjectCellRefMut<'_, T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &T {
        &self.inner
    }
}

impl<T> DerefMut for ObjectCellRefMut<'_, T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut T {
        &mut self.inner
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_borrow() {
        let cell = ObjectCell::new(42);
        assert_eq!(*cell.borrow(), 42);
        *cell.borrow_mut() = 100;
        assert_eq!(*cell.borrow(), 100);
    }

    #[test]
    fn test_multiple_shared_borrows() {
        let cell = ObjectCell::new(42);
        let a = cell.borrow();
        let b = cell.borrow();
        assert_eq!(*a, 42);
        assert_eq!(*b, 42);
    }

    #[test]
    #[should_panic(expected = "already borrowed")]
    fn test_borrow_mut_while_borrowed() {
        let cell = ObjectCell::new(42);
        let _a = cell.borrow();
        let _b = cell.borrow_mut(); // should panic
    }

    #[test]
    #[should_panic(expected = "already mutably borrowed")]
    fn test_borrow_while_mut_borrowed() {
        let cell = ObjectCell::new(42);
        let _a = cell.borrow_mut();
        let _b = cell.borrow(); // should panic
    }

    #[test]
    fn test_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<ObjectCell<i32>>();
        assert_send_sync::<ObjectCell<Vec<String>>>();
    }
}
