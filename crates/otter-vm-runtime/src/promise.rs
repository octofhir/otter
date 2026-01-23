//! Promise implementation

use parking_lot::Mutex;
use std::sync::Arc;

/// Promise state
#[derive(Debug, Clone)]
pub enum PromiseState<T, E> {
    /// Not yet settled
    Pending,
    /// Resolved with value
    Fulfilled(T),
    /// Rejected with error
    Rejected(E),
}

/// Callback list type alias
type CallbackList<T> = Arc<Mutex<Vec<Box<dyn FnOnce(T) + Send>>>>;

/// A Promise
pub struct Promise<T, E> {
    state: Arc<Mutex<PromiseState<T, E>>>,
    on_fulfilled: CallbackList<T>,
    on_rejected: CallbackList<E>,
}

impl<T: Clone + Send + 'static, E: Clone + Send + 'static> Promise<T, E> {
    /// Create a new pending promise
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(PromiseState::Pending)),
            on_fulfilled: Arc::new(Mutex::new(Vec::new())),
            on_rejected: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Resolve the promise
    pub fn resolve(&self, value: T) {
        let mut state = self.state.lock();
        if matches!(*state, PromiseState::Pending) {
            *state = PromiseState::Fulfilled(value.clone());

            let callbacks = std::mem::take(&mut *self.on_fulfilled.lock());
            for callback in callbacks {
                callback(value.clone());
            }
        }
    }

    /// Reject the promise
    pub fn reject(&self, error: E) {
        let mut state = self.state.lock();
        if matches!(*state, PromiseState::Pending) {
            *state = PromiseState::Rejected(error.clone());

            let callbacks = std::mem::take(&mut *self.on_rejected.lock());
            for callback in callbacks {
                callback(error.clone());
            }
        }
    }

    /// Register fulfillment callback
    pub fn then<F>(&self, callback: F)
    where
        F: FnOnce(T) + Send + 'static,
    {
        let state = self.state.lock().clone();
        match state {
            PromiseState::Fulfilled(value) => callback(value),
            PromiseState::Pending => {
                self.on_fulfilled.lock().push(Box::new(callback));
            }
            _ => {}
        }
    }

    /// Register rejection callback
    pub fn catch<F>(&self, callback: F)
    where
        F: FnOnce(E) + Send + 'static,
    {
        let state = self.state.lock().clone();
        match state {
            PromiseState::Rejected(error) => callback(error),
            PromiseState::Pending => {
                self.on_rejected.lock().push(Box::new(callback));
            }
            _ => {}
        }
    }

    /// Get current state
    pub fn state(&self) -> PromiseState<T, E> {
        self.state.lock().clone()
    }
}

impl<T: Clone + Send + 'static, E: Clone + Send + 'static> Default for Promise<T, E> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn test_promise_resolve() {
        let promise = Promise::<i32, String>::new();
        let called = Arc::new(AtomicBool::new(false));
        let called_clone = called.clone();

        promise.then(move |v| {
            assert_eq!(v, 42);
            called_clone.store(true, Ordering::Relaxed);
        });

        promise.resolve(42);
        assert!(called.load(Ordering::Relaxed));
    }

    #[test]
    fn test_promise_reject() {
        let promise = Promise::<i32, String>::new();
        let called = Arc::new(AtomicBool::new(false));
        let called_clone = called.clone();

        promise.catch(move |e| {
            assert_eq!(e, "error");
            called_clone.store(true, Ordering::Relaxed);
        });

        promise.reject("error".to_string());
        assert!(called.load(Ordering::Relaxed));
    }
}
