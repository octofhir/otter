//! JavaScript Promise implementation for the VM
//!
//! Promises are used for async/await support.

use crate::value::Value;
use parking_lot::Mutex;
use std::sync::Arc;

/// Promise state
#[derive(Debug, Clone)]
pub enum PromiseState {
    /// Not yet settled
    Pending,
    /// Resolved with value
    Fulfilled(Value),
    /// Rejected with error
    Rejected(Value),
}

/// Callback for promise resolution
type ResolveCallback = Box<dyn FnOnce(Value) + Send>;

/// A JavaScript Promise
pub struct JsPromise {
    /// Current state
    state: Mutex<PromiseState>,
    /// Callbacks to run on fulfillment
    on_fulfilled: Mutex<Vec<ResolveCallback>>,
    /// Callbacks to run on rejection
    on_rejected: Mutex<Vec<ResolveCallback>>,
}

impl std::fmt::Debug for JsPromise {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self.state.lock();
        match &*state {
            PromiseState::Pending => write!(f, "Promise {{ <pending> }}"),
            PromiseState::Fulfilled(v) => write!(f, "Promise {{ <fulfilled>: {:?} }}", v),
            PromiseState::Rejected(v) => write!(f, "Promise {{ <rejected>: {:?} }}", v),
        }
    }
}

impl JsPromise {
    /// Create a new pending promise
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(PromiseState::Pending),
            on_fulfilled: Mutex::new(Vec::new()),
            on_rejected: Mutex::new(Vec::new()),
        })
    }

    /// Create an already resolved promise
    pub fn resolved(value: Value) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(PromiseState::Fulfilled(value)),
            on_fulfilled: Mutex::new(Vec::new()),
            on_rejected: Mutex::new(Vec::new()),
        })
    }

    /// Create an already rejected promise
    pub fn rejected(error: Value) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(PromiseState::Rejected(error)),
            on_fulfilled: Mutex::new(Vec::new()),
            on_rejected: Mutex::new(Vec::new()),
        })
    }

    /// Resolve the promise with a value
    pub fn resolve(&self, value: Value) {
        let mut state = self.state.lock();
        if matches!(*state, PromiseState::Pending) {
            *state = PromiseState::Fulfilled(value.clone());
            drop(state);

            let callbacks = std::mem::take(&mut *self.on_fulfilled.lock());
            for callback in callbacks {
                callback(value.clone());
            }
        }
    }

    /// Reject the promise with an error
    pub fn reject(&self, error: Value) {
        let mut state = self.state.lock();
        if matches!(*state, PromiseState::Pending) {
            *state = PromiseState::Rejected(error.clone());
            drop(state);

            let callbacks = std::mem::take(&mut *self.on_rejected.lock());
            for callback in callbacks {
                callback(error.clone());
            }
        }
    }

    /// Register a fulfillment callback
    ///
    /// If the promise is already fulfilled, the callback is called immediately.
    /// Returns the callback result or None if pending.
    pub fn then<F>(&self, callback: F)
    where
        F: FnOnce(Value) + Send + 'static,
    {
        let state = self.state.lock().clone();
        match state {
            PromiseState::Fulfilled(value) => callback(value),
            PromiseState::Pending => {
                self.on_fulfilled.lock().push(Box::new(callback));
            }
            PromiseState::Rejected(_) => {}
        }
    }

    /// Register a rejection callback
    pub fn catch<F>(&self, callback: F)
    where
        F: FnOnce(Value) + Send + 'static,
    {
        let state = self.state.lock().clone();
        match state {
            PromiseState::Rejected(error) => callback(error),
            PromiseState::Pending => {
                self.on_rejected.lock().push(Box::new(callback));
            }
            PromiseState::Fulfilled(_) => {}
        }
    }

    /// Get current state
    pub fn state(&self) -> PromiseState {
        self.state.lock().clone()
    }

    /// Check if promise is pending
    pub fn is_pending(&self) -> bool {
        matches!(*self.state.lock(), PromiseState::Pending)
    }

    /// Check if promise is fulfilled
    pub fn is_fulfilled(&self) -> bool {
        matches!(*self.state.lock(), PromiseState::Fulfilled(_))
    }

    /// Check if promise is rejected
    pub fn is_rejected(&self) -> bool {
        matches!(*self.state.lock(), PromiseState::Rejected(_))
    }

    /// Get the resolved value if fulfilled
    pub fn value(&self) -> Option<Value> {
        match &*self.state.lock() {
            PromiseState::Fulfilled(v) => Some(v.clone()),
            _ => None,
        }
    }

    /// Get the rejection reason if rejected
    pub fn reason(&self) -> Option<Value> {
        match &*self.state.lock() {
            PromiseState::Rejected(v) => Some(v.clone()),
            _ => None,
        }
    }
}

impl Default for JsPromise {
    fn default() -> Self {
        Self {
            state: Mutex::new(PromiseState::Pending),
            on_fulfilled: Mutex::new(Vec::new()),
            on_rejected: Mutex::new(Vec::new()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn test_promise_resolve() {
        let promise = JsPromise::new();
        let called = Arc::new(AtomicBool::new(false));
        let called_clone = called.clone();

        promise.then(move |v| {
            assert_eq!(v.as_number(), Some(42.0));
            called_clone.store(true, Ordering::Relaxed);
        });

        promise.resolve(Value::number(42.0));
        assert!(called.load(Ordering::Relaxed));
        assert!(promise.is_fulfilled());
    }

    #[test]
    fn test_promise_reject() {
        let promise = JsPromise::new();
        let called = Arc::new(AtomicBool::new(false));
        let called_clone = called.clone();

        promise.catch(move |v| {
            assert!(v.is_string());
            called_clone.store(true, Ordering::Relaxed);
        });

        promise.reject(Value::string(crate::string::JsString::intern("error")));
        assert!(called.load(Ordering::Relaxed));
        assert!(promise.is_rejected());
    }

    #[test]
    fn test_promise_already_resolved() {
        let promise = JsPromise::resolved(Value::number(100.0));
        let called = Arc::new(AtomicBool::new(false));
        let called_clone = called.clone();

        // Callback should be called immediately
        promise.then(move |v| {
            assert_eq!(v.as_number(), Some(100.0));
            called_clone.store(true, Ordering::Relaxed);
        });

        assert!(called.load(Ordering::Relaxed));
    }

    #[test]
    fn test_promise_state() {
        let promise = JsPromise::new();
        assert!(promise.is_pending());
        assert!(!promise.is_fulfilled());
        assert!(!promise.is_rejected());

        promise.resolve(Value::undefined());
        assert!(!promise.is_pending());
        assert!(promise.is_fulfilled());
    }
}
