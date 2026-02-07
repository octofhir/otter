//! Thread-confined interior mutability for VM objects.
//!
//! `ObjectCell<T>` provides zero-cost interior mutability for the single-threaded VM.
//! In release builds, `borrow()` and `borrow_mut()` compile to raw pointer dereferences.
//! In debug builds, runtime borrow tracking catches overlapping mutable borrows.
//!
//! # Safety
//!
//! This type must only be accessed from a single thread. The VM enforces thread
//! confinement at the `VmRuntime`/`VmContext` level.

use std::cell::UnsafeCell;
use std::ops::{Deref, DerefMut};

/// Thread-confined interior mutability wrapper.
///
/// Replaces `parking_lot::RwLock` in `JsObject` and similar hot-path types.
/// Zero overhead in release builds; debug-mode borrow tracking in debug builds.
pub struct ObjectCell<T> {
    value: UnsafeCell<T>,
    #[cfg(debug_assertions)]
    borrow_state: std::cell::Cell<isize>, // >0 = shared borrows, -1 = exclusive
}

impl<T> ObjectCell<T> {
    /// Create a new `ObjectCell` with the given value.
    #[inline]
    pub fn new(value: T) -> Self {
        Self {
            value: UnsafeCell::new(value),
            #[cfg(debug_assertions)]
            borrow_state: std::cell::Cell::new(0),
        }
    }

    /// Borrow the value immutably.
    ///
    /// In debug builds, panics if an exclusive borrow is active.
    #[inline]
    pub fn borrow(&self) -> ObjectCellRef<'_, T> {
        #[cfg(debug_assertions)]
        {
            let state = self.borrow_state.get();
            if state < 0 {
                panic!("ObjectCell: immutable borrow while mutably borrowed");
            }
            self.borrow_state.set(state + 1);
        }
        ObjectCellRef {
            // SAFETY: Single-threaded access guaranteed by VM thread confinement.
            // Debug builds verify no exclusive borrow is active.
            value: unsafe { &*self.value.get() },
            #[cfg(debug_assertions)]
            borrow_state: &self.borrow_state,
        }
    }

    /// Borrow the value mutably.
    ///
    /// In debug builds, panics if any borrow is active.
    #[inline]
    pub fn borrow_mut(&self) -> ObjectCellRefMut<'_, T> {
        #[cfg(debug_assertions)]
        {
            let state = self.borrow_state.get();
            if state != 0 {
                panic!(
                    "ObjectCell: mutable borrow while {} active (state={})",
                    if state > 0 { "immutably borrowed" } else { "mutably borrowed" },
                    state
                );
            }
            self.borrow_state.set(-1);
        }
        ObjectCellRefMut {
            // SAFETY: Single-threaded access guaranteed by VM thread confinement.
            // Debug builds verify no other borrow is active.
            value: unsafe { &mut *self.value.get() },
            #[cfg(debug_assertions)]
            borrow_state: &self.borrow_state,
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
unsafe impl<T: Send> Send for ObjectCell<T> {}
unsafe impl<T: Send + Sync> Sync for ObjectCell<T> {}

impl<T: std::fmt::Debug> std::fmt::Debug for ObjectCell<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // SAFETY: Only called for debugging, single-threaded context
        let value = unsafe { &*self.value.get() };
        f.debug_struct("ObjectCell").field("value", value).finish()
    }
}

/// Immutable borrow guard for `ObjectCell<T>`.
pub struct ObjectCellRef<'a, T> {
    value: &'a T,
    #[cfg(debug_assertions)]
    borrow_state: &'a std::cell::Cell<isize>,
}

impl<T> Deref for ObjectCellRef<'_, T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &T {
        self.value
    }
}

impl<T> Drop for ObjectCellRef<'_, T> {
    #[inline]
    fn drop(&mut self) {
        #[cfg(debug_assertions)]
        {
            let state = self.borrow_state.get();
            debug_assert!(state > 0);
            self.borrow_state.set(state - 1);
        }
    }
}

/// Mutable borrow guard for `ObjectCell<T>`.
pub struct ObjectCellRefMut<'a, T> {
    value: &'a mut T,
    #[cfg(debug_assertions)]
    borrow_state: &'a std::cell::Cell<isize>,
}

impl<T> Deref for ObjectCellRefMut<'_, T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &T {
        self.value
    }
}

impl<T> DerefMut for ObjectCellRefMut<'_, T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut T {
        self.value
    }
}

impl<T> Drop for ObjectCellRefMut<'_, T> {
    #[inline]
    fn drop(&mut self) {
        #[cfg(debug_assertions)]
        {
            let state = self.borrow_state.get();
            debug_assert_eq!(state, -1);
            self.borrow_state.set(0);
        }
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
    #[cfg(debug_assertions)]
    #[should_panic(expected = "ObjectCell: mutable borrow while immutably borrowed")]
    fn test_borrow_mut_while_borrowed() {
        let cell = ObjectCell::new(42);
        let _a = cell.borrow();
        let _b = cell.borrow_mut(); // should panic
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "ObjectCell: immutable borrow while mutably borrowed")]
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
