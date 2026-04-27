//! JavaScript array value with dense element storage.
//!
//! Slice 20 ships the dense path: elements live in a
//! `SmallVec<[Value; 4]>` so short literals stay inline. Out-of-
//! bounds writes extend the storage with `Value::Undefined`,
//! matching JS dense-array semantics. Sparse-fallback lands later
//! once a real workload demands it.
//!
//! # Contents
//! - [`JsArray`] — cheap-to-clone array handle (`Rc`-shared).
//! - [`ArrayBody`] — internal element storage.
//!
//! # Invariants
//! - `len` always equals the number of slots in `elements`.
//! - Out-of-range reads return `Value::Undefined` (foundation
//!   approximation; spec returns `undefined` for missing indices,
//!   so behaviour matches when the array is dense).
//! - Cloning shares storage — both handles see mutations.
//!
//! # See also
//! - foundation plan §M9.
//! - [`docs/new-engine/tasks/21-array-prototype-essentials.md`](
//!     ../../../docs/new-engine/tasks/21-array-prototype-essentials.md
//!   )

use std::cell::{Ref, RefCell, RefMut};
use std::rc::Rc;

use smallvec::SmallVec;

use crate::Value;

/// Cheap-to-clone array handle.
#[derive(Debug, Clone)]
pub struct JsArray {
    inner: Rc<RefCell<ArrayBody>>,
}

/// Internal storage; `RefCell` only because the public surface
/// keeps `&self` while mutating, mirroring how `JsObject` is
/// borrowed.
#[derive(Debug, Default)]
pub struct ArrayBody {
    /// Element storage. Crate-internal — outside callers should
    /// go through `JsArray::{get, set, push, pop, ...}`.
    pub(crate) elements: SmallVec<[Value; 4]>,
}

impl JsArray {
    /// Allocate a fresh empty array.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Construct from a vector of initial elements (used by
    /// `[a, b, c]` literal lowering).
    #[must_use]
    pub fn from_elements(values: impl IntoIterator<Item = Value>) -> Self {
        let mut body = ArrayBody::default();
        for v in values {
            body.elements.push(v);
        }
        Self {
            inner: Rc::new(RefCell::new(body)),
        }
    }

    /// Length in elements (O(1)).
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.borrow().elements.len()
    }

    /// `true` for an empty array.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.inner.borrow().elements.is_empty()
    }

    /// Read element at `idx`. Out-of-range returns `Value::Undefined`.
    #[must_use]
    pub fn get(&self, idx: usize) -> Value {
        self.inner
            .borrow()
            .elements
            .get(idx)
            .cloned()
            .unwrap_or(Value::Undefined)
    }

    /// Write element at `idx`. Extends with `Value::Undefined`
    /// when `idx >= len`.
    pub fn set(&self, idx: usize, value: Value) {
        let mut body = self.inner.borrow_mut();
        if idx < body.elements.len() {
            body.elements[idx] = value;
            return;
        }
        while body.elements.len() < idx {
            body.elements.push(Value::Undefined);
        }
        body.elements.push(value);
    }

    /// Push to the tail (used by `Array.prototype.push` in slice
    /// 21). Returns the new length.
    pub fn push(&self, value: Value) -> usize {
        let mut body = self.inner.borrow_mut();
        body.elements.push(value);
        body.elements.len()
    }

    /// Pop from the tail (used by `Array.prototype.pop` in slice
    /// 21). Returns `Value::Undefined` for an empty array.
    pub fn pop(&self) -> Value {
        self.inner
            .borrow_mut()
            .elements
            .pop()
            .unwrap_or(Value::Undefined)
    }

    /// Borrow the underlying storage read-only.
    #[must_use]
    pub fn borrow_body(&self) -> Ref<'_, ArrayBody> {
        self.inner.borrow()
    }

    /// Mutable borrow of the underlying storage. Discouraged
    /// outside the VM core.
    #[must_use]
    pub fn borrow_body_mut(&self) -> RefMut<'_, ArrayBody> {
        self.inner.borrow_mut()
    }

    /// Identity comparison.
    #[must_use]
    pub fn ptr_eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.inner, &other.inner)
    }
}

impl Default for JsArray {
    fn default() -> Self {
        Self {
            inner: Rc::new(RefCell::new(ArrayBody::default())),
        }
    }
}

impl ArrayBody {
    /// Iterate over elements.
    pub fn iter(&self) -> impl Iterator<Item = &Value> {
        self.elements.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_constructor() {
        let a = JsArray::from_elements([Value::Boolean(true), Value::Null, Value::Boolean(false)]);
        assert_eq!(a.len(), 3);
        assert_eq!(a.get(0), Value::Boolean(true));
        assert_eq!(a.get(1), Value::Null);
        assert_eq!(a.get(2), Value::Boolean(false));
    }

    #[test]
    fn out_of_range_read_is_undefined() {
        let a = JsArray::new();
        assert_eq!(a.get(0), Value::Undefined);
    }

    #[test]
    fn out_of_range_write_extends_with_undefined() {
        let a = JsArray::new();
        a.set(2, Value::Boolean(true));
        assert_eq!(a.len(), 3);
        assert_eq!(a.get(0), Value::Undefined);
        assert_eq!(a.get(1), Value::Undefined);
        assert_eq!(a.get(2), Value::Boolean(true));
    }

    #[test]
    fn push_and_pop() {
        let a = JsArray::new();
        assert_eq!(a.push(Value::Boolean(true)), 1);
        assert_eq!(a.push(Value::Null), 2);
        assert_eq!(a.pop(), Value::Null);
        assert_eq!(a.pop(), Value::Boolean(true));
        assert_eq!(a.pop(), Value::Undefined);
        assert!(a.is_empty());
    }

    #[test]
    fn cloning_shares_storage() {
        let a = JsArray::new();
        let b = a.clone();
        a.push(Value::Boolean(true));
        assert!(a.ptr_eq(&b));
        assert_eq!(b.len(), 1);
    }
}
