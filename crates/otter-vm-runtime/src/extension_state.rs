//! Per-extension typed state storage.
//!
//! `ExtensionState` is a type-map that allows extensions to store and retrieve
//! per-extension state using Rust's type system. Inspired by Deno's `OpState`.
//!
//! Each extension can `put()` its own state types during initialization and
//! later `borrow()` them from within native function handlers via `NativeContext`.

use std::any::{Any, TypeId};
use std::collections::BTreeMap;

/// Type-map for storing per-extension state.
///
/// Each unique Rust type can be stored exactly once. Extensions use this to
/// store configuration, caches, or other per-extension data that native
/// functions need access to at runtime.
///
/// # Example
///
/// ```ignore
/// struct PathConfig { separator: char }
///
/// // During init:
/// state.put(PathConfig { separator: '/' });
///
/// // In a native function:
/// let config = state.get::<PathConfig>();
/// ```
pub struct ExtensionState {
    data: BTreeMap<TypeId, Box<dyn Any>>,
}

impl ExtensionState {
    /// Create an empty state.
    pub fn new() -> Self {
        Self {
            data: BTreeMap::new(),
        }
    }

    /// Store a value of type `T`. Overwrites any existing value of the same type.
    pub fn put<T: 'static>(&mut self, val: T) {
        self.data.insert(TypeId::of::<T>(), Box::new(val));
    }

    /// Get a reference to a stored value of type `T`.
    ///
    /// # Panics
    ///
    /// Panics if no value of type `T` has been stored.
    pub fn get<T: 'static>(&self) -> &T {
        self.data
            .get(&TypeId::of::<T>())
            .and_then(|v| v.downcast_ref::<T>())
            .unwrap_or_else(|| {
                panic!(
                    "ExtensionState: no value of type {} stored",
                    std::any::type_name::<T>()
                )
            })
    }

    /// Get a mutable reference to a stored value of type `T`.
    ///
    /// # Panics
    ///
    /// Panics if no value of type `T` has been stored.
    pub fn get_mut<T: 'static>(&mut self) -> &mut T {
        self.data
            .get_mut(&TypeId::of::<T>())
            .and_then(|v| v.downcast_mut::<T>())
            .unwrap_or_else(|| {
                panic!(
                    "ExtensionState: no value of type {} stored",
                    std::any::type_name::<T>()
                )
            })
    }

    /// Try to borrow a reference to a stored value of type `T`.
    /// Returns `None` if no value of that type has been stored.
    pub fn try_borrow<T: 'static>(&self) -> Option<&T> {
        self.data
            .get(&TypeId::of::<T>())
            .and_then(|v| v.downcast_ref::<T>())
    }

    /// Remove and return a stored value of type `T`.
    ///
    /// # Panics
    ///
    /// Panics if no value of type `T` has been stored.
    pub fn take<T: 'static>(&mut self) -> T {
        *self
            .data
            .remove(&TypeId::of::<T>())
            .and_then(|v| v.downcast::<T>().ok())
            .unwrap_or_else(|| {
                panic!(
                    "ExtensionState: no value of type {} stored",
                    std::any::type_name::<T>()
                )
            })
    }

    /// Check if a value of type `T` has been stored.
    pub fn has<T: 'static>(&self) -> bool {
        self.data.contains_key(&TypeId::of::<T>())
    }
}

impl Default for ExtensionState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_put_and_get() {
        let mut state = ExtensionState::new();
        state.put(42_i32);
        state.put("hello".to_string());

        assert_eq!(*state.get::<i32>(), 42);
        assert_eq!(state.get::<String>(), "hello");
    }

    #[test]
    fn test_get_mut() {
        let mut state = ExtensionState::new();
        state.put(vec![1, 2, 3]);

        state.get_mut::<Vec<i32>>().push(4);
        assert_eq!(state.get::<Vec<i32>>(), &vec![1, 2, 3, 4]);
    }

    #[test]
    fn test_try_borrow() {
        let mut state = ExtensionState::new();
        assert!(state.try_borrow::<i32>().is_none());

        state.put(10_i32);
        assert_eq!(state.try_borrow::<i32>(), Some(&10));
    }

    #[test]
    fn test_has() {
        let mut state = ExtensionState::new();
        assert!(!state.has::<i32>());

        state.put(1_i32);
        assert!(state.has::<i32>());
    }

    #[test]
    fn test_take() {
        let mut state = ExtensionState::new();
        state.put("owned".to_string());

        let val = state.take::<String>();
        assert_eq!(val, "owned");
        assert!(!state.has::<String>());
    }

    #[test]
    fn test_overwrite() {
        let mut state = ExtensionState::new();
        state.put(1_i32);
        state.put(2_i32);
        assert_eq!(*state.get::<i32>(), 2);
    }

    #[test]
    #[should_panic(expected = "no value of type")]
    fn test_get_missing_panics() {
        let state = ExtensionState::new();
        let _ = state.get::<i32>();
    }
}
