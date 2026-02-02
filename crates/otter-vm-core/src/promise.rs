//! JavaScript Promise implementation for the VM
//!
//! Promises are used for async/await support.
//!
//! ## Rust API
//!
//! Create promises from Rust code using `with_resolvers()`:
//!
//! ```ignore
//! let resolvers = JsPromise::with_resolvers(mm.clone(), |_, _| {});
//! // Later, resolve the promise
//! (resolvers.resolve)(Value::number(42.0));
//! // Return the promise to JS
//! Value::promise(resolvers.promise)
//! ```

use crate::string::JsString;
use crate::value::Value;
use parking_lot::Mutex;
use std::sync::Arc;

/// Promise state
#[derive(Debug, Clone)]
pub enum PromiseState {
    /// Not yet settled
    Pending,
    /// Resolving a thenable (still pending)
    PendingThenable(Value),
    /// Resolved with value
    Fulfilled(Value),
    /// Rejected with error
    Rejected(Value),
}

impl PromiseState {
    /// Check if settled (fulfilled or rejected)
    pub fn is_settled(&self) -> bool {
        !matches!(self, PromiseState::Pending | PromiseState::PendingThenable(_))
    }
}

/// Callback for promise resolution
type ResolveCallback = Box<dyn FnOnce(Value) + Send>;

/// Kind of JS Promise reaction
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JsPromiseJobKind {
    /// Call onFulfilled handler
    Fulfill,
    /// Call onRejected handler
    Reject,
    /// Call onFinally after fulfillment
    FinallyFulfill,
    /// Call onFinally after rejection
    FinallyReject,
    /// Resolve a thenable by looking up its `then` method
    ResolveThenableLookup,
    /// Resolve a thenable by calling its `then` method
    ResolveThenable,
    /// Pass through fulfillment (identity)
    PassthroughFulfill,
    /// Pass through rejection (thrower)
    PassthroughReject,
}

/// JS callback job for Promise reactions
#[derive(Clone)]
pub struct JsPromiseJob {
    /// Reaction kind
    pub kind: JsPromiseJobKind,
    /// The JavaScript function to call
    pub callback: Value,
    /// The `this` binding for the call
    pub this_arg: Value,
    /// The result promise to resolve/reject with the callback's return value
    pub result_promise: Option<Arc<JsPromise>>,
}

/// A JavaScript Promise
pub struct JsPromise {
    /// Current state
    pub(crate) state: Mutex<PromiseState>,
    /// Rust callbacks to run on fulfillment (for internal use)
    on_fulfilled: Mutex<Vec<ResolveCallback>>,
    /// Rust callbacks to run on rejection (for internal use)
    on_rejected: Mutex<Vec<ResolveCallback>>,
    /// Callbacks to run on settlement (finally)
    on_finally: Mutex<Vec<Box<dyn FnOnce() + Send>>>,
    /// JS callback jobs for fulfillment (Promise.then onFulfilled)
    js_fulfill_jobs: Mutex<Vec<JsPromiseJob>>,
    /// JS callback jobs for rejection (Promise.then/catch onRejected)
    js_reject_jobs: Mutex<Vec<JsPromiseJob>>,
}

impl std::fmt::Debug for JsPromise {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self.state.lock();
        match &*state {
            PromiseState::Pending => write!(f, "Promise {{ <pending> }}"),
            PromiseState::PendingThenable(_) => write!(f, "Promise {{ <pending> }}"),
            PromiseState::Fulfilled(v) => write!(f, "Promise {{ <fulfilled>: {:?} }}", v),
            PromiseState::Rejected(v) => write!(f, "Promise {{ <rejected>: {:?} }}", v),
        }
    }
}

/// Result of `JsPromise::with_resolvers()` - ES2024 Promise.withResolvers() pattern
///
/// Provides a promise along with its resolve and reject functions for manual control.
pub struct PromiseWithResolvers {
    /// The promise
    pub promise: Arc<JsPromise>,
    /// Function to resolve the promise
    pub resolve: Arc<dyn Fn(Value) + Send + Sync>,
    /// Function to reject the promise
    pub reject: Arc<dyn Fn(Value) + Send + Sync>,
}

impl JsPromise {
    /// Create a new pending promise
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(PromiseState::Pending),
            on_fulfilled: Mutex::new(Vec::new()),
            on_rejected: Mutex::new(Vec::new()),
            on_finally: Mutex::new(Vec::new()),
            js_fulfill_jobs: Mutex::new(Vec::new()),
            js_reject_jobs: Mutex::new(Vec::new()),
        })
    }

    /// Create a promise with resolve/reject handles (ES2024 Promise.withResolvers pattern)
    ///
    /// This is the recommended way to create promises from Rust when you need
    /// to resolve/reject them later (e.g., from callbacks or async operations).
    ///
    /// # Example
    ///
    /// ```ignore
    /// let resolvers = JsPromise::with_resolvers(mm.clone(), |_, _| {});
    ///
    /// // Pass promise to JS
    /// let js_promise = Value::promise(resolvers.promise.clone());
    ///
    /// // Later, resolve from Rust
    /// (resolvers.resolve)(Value::number(42.0));
    /// ```
    pub fn with_resolvers<E>(
        mm: Arc<crate::memory::MemoryManager>,
        enqueue: E,
    ) -> PromiseWithResolvers
    where
        E: Fn(JsPromiseJob, Vec<Value>) + Send + Sync + 'static,
    {
        JsPromise::with_resolvers_with_js_jobs(mm, enqueue)
    }

    /// Create a promise with resolve/reject handles that enqueue JS jobs
    ///
    /// Use this when resolving from Rust should follow JS Promise semantics
    /// (including thenable assimilation via the JS job queue).
    pub fn with_resolvers_with_js_jobs<E>(
        _mm: Arc<crate::memory::MemoryManager>,
        enqueue: E,
    ) -> PromiseWithResolvers
    where
        E: Fn(JsPromiseJob, Vec<Value>) + Send + Sync + 'static,
    {
        let promise = JsPromise::new();
        let enqueue = Arc::new(enqueue);

        let resolve = {
            let p = promise.clone();
            let enqueue = Arc::clone(&enqueue);
            Arc::new(move |v: Value| {
                let enqueue = Arc::clone(&enqueue);
                p.resolve_with_js_jobs(v, move |job, args| {
                    (enqueue)(job, args);
                });
            }) as Arc<dyn Fn(Value) + Send + Sync>
        };

        let reject = {
            let p = promise.clone();
            let enqueue = Arc::clone(&enqueue);
            Arc::new(move |e: Value| {
                let enqueue = Arc::clone(&enqueue);
                p.reject_with_js_jobs(e, move |job, args| {
                    (enqueue)(job, args);
                });
            }) as Arc<dyn Fn(Value) + Send + Sync>
        };

        PromiseWithResolvers {
            promise,
            resolve,
            reject,
        }
    }

    /// Create an already resolved promise
    pub fn resolved(value: Value) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(PromiseState::Fulfilled(value)),
            on_fulfilled: Mutex::new(Vec::new()),
            on_rejected: Mutex::new(Vec::new()),
            on_finally: Mutex::new(Vec::new()),
            js_fulfill_jobs: Mutex::new(Vec::new()),
            js_reject_jobs: Mutex::new(Vec::new()),
        })
    }

    /// Create an already rejected promise
    pub fn rejected(error: Value) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(PromiseState::Rejected(error)),
            on_fulfilled: Mutex::new(Vec::new()),
            on_rejected: Mutex::new(Vec::new()),
            on_finally: Mutex::new(Vec::new()),
            js_fulfill_jobs: Mutex::new(Vec::new()),
            js_reject_jobs: Mutex::new(Vec::new()),
        })
    }

    /// Resolve the promise with a value
    ///
    /// If the promise is already settled, this is a no-op.
    /// Callbacks registered via `then()` will be called synchronously.
    /// For microtask-queue behavior, use `resolve_with_queue()`.
    pub fn resolve(&self, value: Value) {
        let mut state = self.state.lock();
        if matches!(*state, PromiseState::Pending) {
            *state = PromiseState::Fulfilled(value.clone());
            drop(state);

            // Run fulfillment callbacks
            let callbacks = std::mem::take(&mut *self.on_fulfilled.lock());
            for callback in callbacks {
                callback(value.clone());
            }

            // Run finally callbacks
            let finally_callbacks = std::mem::take(&mut *self.on_finally.lock());
            for callback in finally_callbacks {
                callback();
            }
        }
    }

    /// Resolve the promise, enqueueing callbacks via the provided function
    ///
    /// This follows the ES spec where promise callbacks are microtasks.
    pub fn resolve_with_enqueue<E>(&self, value: Value, enqueue: E)
    where
        E: Fn(Box<dyn FnOnce() + Send>) + Send + Sync,
    {
        let mut state = self.state.lock();
        if matches!(*state, PromiseState::Pending) {
            *state = PromiseState::Fulfilled(value.clone());
            drop(state);

            // Enqueue fulfillment callbacks as microtasks
            let callbacks = std::mem::take(&mut *self.on_fulfilled.lock());
            for callback in callbacks {
                let v = value.clone();
                enqueue(Box::new(move || callback(v)));
            }

            // Enqueue finally callbacks
            let finally_callbacks = std::mem::take(&mut *self.on_finally.lock());
            for callback in finally_callbacks {
                enqueue(Box::new(callback));
            }
        }
    }

    /// Reject the promise with an error
    ///
    /// If the promise is already settled, this is a no-op.
    /// Callbacks registered via `catch()` will be called synchronously.
    /// For microtask-queue behavior, use `reject_with_queue()`.
    pub fn reject(&self, error: Value) {
        let mut state = self.state.lock();
        if matches!(*state, PromiseState::Pending) {
            *state = PromiseState::Rejected(error.clone());
            drop(state);

            // Run rejection callbacks
            let callbacks = std::mem::take(&mut *self.on_rejected.lock());
            for callback in callbacks {
                callback(error.clone());
            }

            // Run finally callbacks
            let finally_callbacks = std::mem::take(&mut *self.on_finally.lock());
            for callback in finally_callbacks {
                callback();
            }
        }
    }

    /// Reject the promise, enqueueing callbacks via the provided function
    ///
    /// This follows the ES spec where promise callbacks are microtasks.
    pub fn reject_with_enqueue<E>(&self, error: Value, enqueue: E)
    where
        E: Fn(Box<dyn FnOnce() + Send>) + Send + Sync,
    {
        let mut state = self.state.lock();
        if matches!(*state, PromiseState::Pending) {
            *state = PromiseState::Rejected(error.clone());
            drop(state);

            // Enqueue rejection callbacks as microtasks
            let callbacks = std::mem::take(&mut *self.on_rejected.lock());
            for callback in callbacks {
                let e = error.clone();
                enqueue(Box::new(move || callback(e)));
            }

            // Enqueue finally callbacks
            let finally_callbacks = std::mem::take(&mut *self.on_finally.lock());
            for callback in finally_callbacks {
                enqueue(Box::new(callback));
            }
        }
    }

    /// Register a fulfillment callback
    ///
    /// If the promise is already fulfilled, the callback is called immediately.
    pub fn then<F>(&self, callback: F)
    where
        F: FnOnce(Value) + Send + 'static,
    {
        let state = self.state.lock().clone();
        match state {
            PromiseState::Fulfilled(value) => callback(value),
            PromiseState::Pending | PromiseState::PendingThenable(_) => {
                self.on_fulfilled.lock().push(Box::new(callback));
            }
            PromiseState::Rejected(_) => {}
        }
    }

    /// Register a fulfillment callback with microtask enqueueing
    ///
    /// If the promise is already fulfilled, the callback is enqueued (not called immediately).
    pub fn then_with_enqueue<F, E>(&self, callback: F, enqueue: E)
    where
        F: FnOnce(Value) + Send + 'static,
        E: Fn(Box<dyn FnOnce() + Send>),
    {
        let state = self.state.lock().clone();
        match state {
            PromiseState::Fulfilled(value) => {
                enqueue(Box::new(move || callback(value)));
            }
            PromiseState::Pending | PromiseState::PendingThenable(_) => {
                self.on_fulfilled.lock().push(Box::new(callback));
            }
            PromiseState::Rejected(_) => {}
        }
    }

    /// Register a rejection callback
    ///
    /// If the promise is already rejected, the callback is called immediately.
    pub fn catch<F>(&self, callback: F)
    where
        F: FnOnce(Value) + Send + 'static,
    {
        let state = self.state.lock().clone();
        match state {
            PromiseState::Rejected(error) => callback(error),
            PromiseState::Pending | PromiseState::PendingThenable(_) => {
                self.on_rejected.lock().push(Box::new(callback));
            }
            PromiseState::Fulfilled(_) => {}
        }
    }

    /// Register a rejection callback with microtask enqueueing
    ///
    /// If the promise is already rejected, the callback is enqueued (not called immediately).
    pub fn catch_with_enqueue<F, E>(&self, callback: F, enqueue: E)
    where
        F: FnOnce(Value) + Send + 'static,
        E: Fn(Box<dyn FnOnce() + Send>),
    {
        let state = self.state.lock().clone();
        match state {
            PromiseState::Rejected(error) => {
                enqueue(Box::new(move || callback(error)));
            }
            PromiseState::Pending | PromiseState::PendingThenable(_) => {
                self.on_rejected.lock().push(Box::new(callback));
            }
            PromiseState::Fulfilled(_) => {}
        }
    }

    /// Register a finally callback (runs on either fulfillment or rejection)
    ///
    /// If the promise is already settled, the callback is called immediately.
    pub fn finally<F>(&self, callback: F)
    where
        F: FnOnce() + Send + 'static,
    {
        let state = self.state.lock().clone();
        match state {
            PromiseState::Fulfilled(_) | PromiseState::Rejected(_) => callback(),
            PromiseState::Pending | PromiseState::PendingThenable(_) => {
                self.on_finally.lock().push(Box::new(callback));
            }
        }
    }

    /// Register a finally callback with microtask enqueueing
    pub fn finally_with_enqueue<F, E>(&self, callback: F, enqueue: E)
    where
        F: FnOnce() + Send + 'static,
        E: Fn(Box<dyn FnOnce() + Send>),
    {
        let state = self.state.lock().clone();
        match state {
            PromiseState::Fulfilled(_) | PromiseState::Rejected(_) => {
                enqueue(Box::new(callback));
            }
            PromiseState::Pending | PromiseState::PendingThenable(_) => {
                self.on_finally.lock().push(Box::new(callback));
            }
        }
    }

    /// Get current state
    pub fn state(&self) -> PromiseState {
        self.state.lock().clone()
    }

    /// Check if promise is pending
    pub fn is_pending(&self) -> bool {
        matches!(
            *self.state.lock(),
            PromiseState::Pending | PromiseState::PendingThenable(_)
        )
    }

    /// Check if promise is fulfilled
    pub fn is_fulfilled(&self) -> bool {
        matches!(*self.state.lock(), PromiseState::Fulfilled(_))
    }

    /// Check if promise is rejected
    pub fn is_rejected(&self) -> bool {
        matches!(*self.state.lock(), PromiseState::Rejected(_))
    }

    /// Check if promise is settled (fulfilled or rejected)
    pub fn is_settled(&self) -> bool {
        !self.is_pending()
    }

    /// Trace GC roots held by this promise (state + JS callback jobs).
    pub fn trace_roots(&self, tracer: &mut dyn FnMut(*const crate::gc::GcHeader)) {
        let state = self.state.lock();
        match &*state {
            PromiseState::PendingThenable(value) => {
                value.trace(tracer);
            }
            PromiseState::Fulfilled(value) | PromiseState::Rejected(value) => {
                value.trace(tracer);
            }
            PromiseState::Pending => {}
        }
        drop(state);

        // Trace JS fulfillment jobs
        for job in self.js_fulfill_jobs.lock().iter() {
            job.callback.trace(tracer);
            job.this_arg.trace(tracer);
            if let Some(promise) = &job.result_promise {
                promise.trace_roots(tracer);
            }
        }

        // Trace JS rejection jobs
        for job in self.js_reject_jobs.lock().iter() {
            job.callback.trace(tracer);
            job.this_arg.trace(tracer);
            if let Some(promise) = &job.result_promise {
                promise.trace_roots(tracer);
            }
        }
    }

    /// Register a JS callback for fulfillment
    ///
    /// If the promise is already fulfilled, the job is enqueued immediately.
    /// Otherwise, it's stored for when the promise resolves.
    pub fn then_js<E>(&self, job: JsPromiseJob, enqueue: E)
    where
        E: Fn(JsPromiseJob, Vec<Value>),
    {
        // eprintln!("DEBUG: then_js called, callback is_function: {}", job.callback.is_function());
        let state = self.state.lock().clone();
        match state {
            PromiseState::Fulfilled(value) => {
                // eprintln!("DEBUG: Promise already fulfilled, enqueueing job immediately");
                // Promise already fulfilled - enqueue job immediately
                enqueue(job, vec![value]);
            }
            PromiseState::Pending | PromiseState::PendingThenable(_) => {
                // Promise pending - store job for later
                self.js_fulfill_jobs.lock().push(job);
            }
            PromiseState::Rejected(_) => {
                // Promise rejected - don't run fulfillment handler
            }
        }
    }

    /// Register a JS callback for rejection
    ///
    /// If the promise is already rejected, the job is enqueued immediately.
    /// Otherwise, it's stored for when the promise rejects.
    pub fn catch_js<E>(&self, job: JsPromiseJob, enqueue: E)
    where
        E: Fn(JsPromiseJob, Vec<Value>),
    {
        let state = self.state.lock().clone();
        match state {
            PromiseState::Rejected(error) => {
                // Promise already rejected - enqueue job immediately
                enqueue(job, vec![error]);
            }
            PromiseState::Pending | PromiseState::PendingThenable(_) => {
                // Promise pending - store job for later
                self.js_reject_jobs.lock().push(job);
            }
            PromiseState::Fulfilled(_) => {
                // Promise fulfilled - don't run rejection handler
            }
        }
    }

    /// Resolve the promise and enqueue JS callback jobs
    ///
    /// This version enqueues both Rust closures and JS callbacks via the provided function.
    pub fn resolve_with_js_jobs<E>(self: &Arc<Self>, value: Value, enqueue: E)
    where
        E: Fn(JsPromiseJob, Vec<Value>) + Clone,
    {
        self.resolve_with_js_jobs_internal(value, enqueue, false, true);
    }

    /// Resolve the promise from a thenable (allows settling while resolving)
    pub fn resolve_from_thenable_with_js_jobs<E>(self: &Arc<Self>, value: Value, enqueue: E)
    where
        E: Fn(JsPromiseJob, Vec<Value>) + Clone,
    {
        self.resolve_with_js_jobs_internal(value, enqueue, true, true);
    }

    /// Fulfill the promise without thenable assimilation (used after then lookup)
    pub fn fulfill_with_js_jobs<E>(self: &Arc<Self>, value: Value, enqueue: E)
    where
        E: Fn(JsPromiseJob, Vec<Value>) + Clone,
    {
        self.resolve_with_js_jobs_internal(value, enqueue, true, false);
    }

    fn resolve_with_js_jobs_internal<E>(
        self: &Arc<Self>,
        value: Value,
        enqueue: E,
        allow_pending_thenable: bool,
        check_thenable: bool,
    ) where
        E: Fn(JsPromiseJob, Vec<Value>) + Clone,
    {
        let mut state = self.state.lock();
        match &*state {
            PromiseState::Pending => {}
            PromiseState::PendingThenable(_) if allow_pending_thenable => {}
            _ => return,
        }

        if check_thenable && value.is_object() {
            if let Some(inner) = value.as_promise() {
                let self_ptr = self.as_ref() as *const JsPromise;
                if std::ptr::eq(Arc::as_ptr(inner), self_ptr) {
                    drop(state);
                    let error = Value::string(JsString::intern(
                        "TypeError: Promise cannot resolve itself",
                    ));
                    self.reject_from_thenable_with_js_jobs(error, enqueue);
                    return;
                }
            }

            *state = PromiseState::PendingThenable(value.clone());
            drop(state);
            let job = JsPromiseJob {
                kind: JsPromiseJobKind::ResolveThenableLookup,
                callback: Value::undefined(),
                this_arg: value,
                result_promise: Some(self.clone()),
            };
            enqueue(job, Vec::new());
            return;
        }

        *state = PromiseState::Fulfilled(value.clone());
        drop(state);

        // Enqueue JS callback jobs
        let js_jobs = std::mem::take(&mut *self.js_fulfill_jobs.lock());
        for job in js_jobs {
            enqueue(job, vec![value.clone()]);
        }

        // Run Rust closure callbacks
        let callbacks = std::mem::take(&mut *self.on_fulfilled.lock());
        for callback in callbacks {
            callback(value.clone());
        }

        // Run finally callbacks
        let finally_callbacks = std::mem::take(&mut *self.on_finally.lock());
        for callback in finally_callbacks {
            callback();
        }
    }

    /// Reject the promise and enqueue JS callback jobs
    ///
    /// This version enqueues both Rust closures and JS callbacks via the provided function.
    pub fn reject_with_js_jobs<E>(self: &Arc<Self>, error: Value, enqueue: E)
    where
        E: Fn(JsPromiseJob, Vec<Value>) + Clone,
    {
        self.reject_with_js_jobs_internal(error, enqueue, false);
    }

    /// Reject the promise from a thenable (allows settling while resolving)
    pub fn reject_from_thenable_with_js_jobs<E>(self: &Arc<Self>, error: Value, enqueue: E)
    where
        E: Fn(JsPromiseJob, Vec<Value>) + Clone,
    {
        self.reject_with_js_jobs_internal(error, enqueue, true);
    }

    fn reject_with_js_jobs_internal<E>(
        self: &Arc<Self>,
        error: Value,
        enqueue: E,
        allow_pending_thenable: bool,
    ) where
        E: Fn(JsPromiseJob, Vec<Value>) + Clone,
    {
        let mut state = self.state.lock();
        match &*state {
            PromiseState::Pending => {}
            PromiseState::PendingThenable(_) if allow_pending_thenable => {}
            _ => return,
        }

        *state = PromiseState::Rejected(error.clone());
        drop(state);

        // Enqueue JS callback jobs
        let js_jobs = std::mem::take(&mut *self.js_reject_jobs.lock());
        for job in js_jobs {
            enqueue(job, vec![error.clone()]);
        }

        // Run Rust closure callbacks
        let callbacks = std::mem::take(&mut *self.on_rejected.lock());
        for callback in callbacks {
            callback(error.clone());
        }

        // Run finally callbacks
        let finally_callbacks = std::mem::take(&mut *self.on_finally.lock());
        for callback in finally_callbacks {
            callback();
        }
    }

    /// Extract values from this promise and clear its state.
    /// Used for iterative destruction to prevent stack overflow.
    pub fn clear_and_extract_values(&self) -> Vec<Value> {
        let mut values = Vec::new();

        // Clear state and extract value
        let mut state = self.state.lock();
        let old_state = std::mem::replace(&mut *state, PromiseState::Pending);
        match old_state {
            PromiseState::Fulfilled(v) => values.push(v),
            PromiseState::Rejected(v) => values.push(v),
            PromiseState::PendingThenable(v) => values.push(v),
            _ => {}
        }
        drop(state);

        // Clear callbacks to break references.
        // NOTE: We cannot easily extract values from within the boxed closures,
        // but clearing the vectors will at least prevent further reference accumulation.
        self.on_fulfilled.lock().clear();
        self.on_rejected.lock().clear();
        self.on_finally.lock().clear();
        self.js_fulfill_jobs.lock().clear();
        self.js_reject_jobs.lock().clear();

        values
    }
}

impl Default for JsPromise {
    fn default() -> Self {
        Self {
            state: Mutex::new(PromiseState::Pending),
            on_fulfilled: Mutex::new(Vec::new()),
            on_rejected: Mutex::new(Vec::new()),
            on_finally: Mutex::new(Vec::new()),
            js_fulfill_jobs: Mutex::new(Vec::new()),
            js_reject_jobs: Mutex::new(Vec::new()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

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
        assert!(!promise.is_settled());

        promise.resolve(Value::undefined());
        assert!(!promise.is_pending());
        assert!(promise.is_fulfilled());
        assert!(promise.is_settled());
    }

    #[test]
    fn test_with_resolvers() {
        let resolvers = JsPromise::with_resolvers(
            Arc::new(crate::memory::MemoryManager::test()),
            |_, _| {},
        );
        let called = Arc::new(AtomicBool::new(false));
        let called_clone = called.clone();

        resolvers.promise.then(move |v| {
            assert_eq!(v.as_number(), Some(99.0));
            called_clone.store(true, Ordering::Relaxed);
        });

        assert!(!called.load(Ordering::Relaxed));
        (resolvers.resolve)(Value::number(99.0));
        assert!(called.load(Ordering::Relaxed));
    }

    #[test]
    fn test_with_resolvers_reject() {
        let resolvers = JsPromise::with_resolvers(
            Arc::new(crate::memory::MemoryManager::test()),
            |_, _| {},
        );
        let called = Arc::new(AtomicBool::new(false));
        let called_clone = called.clone();

        resolvers.promise.catch(move |_| {
            called_clone.store(true, Ordering::Relaxed);
        });

        (resolvers.reject)(Value::undefined());
        assert!(called.load(Ordering::Relaxed));
        assert!(resolvers.promise.is_rejected());
    }

    #[test]
    fn test_finally() {
        let promise = JsPromise::new();
        let called = Arc::new(AtomicBool::new(false));
        let called_clone = called.clone();

        promise.finally(move || {
            called_clone.store(true, Ordering::Relaxed);
        });

        promise.resolve(Value::undefined());
        assert!(called.load(Ordering::Relaxed));
    }

    #[test]
    fn test_finally_on_reject() {
        let promise = JsPromise::new();
        let called = Arc::new(AtomicBool::new(false));
        let called_clone = called.clone();

        promise.finally(move || {
            called_clone.store(true, Ordering::Relaxed);
        });

        promise.reject(Value::undefined());
        assert!(called.load(Ordering::Relaxed));
    }

    #[test]
    fn test_resolve_with_enqueue() {
        let promise = JsPromise::new();
        let order = Arc::new(AtomicU32::new(0));
        let order_clone = order.clone();

        promise.then(move |_| {
            order_clone.store(2, Ordering::Relaxed);
        });

        let enqueued = Arc::new(Mutex::new(Vec::new()));
        let enqueued_clone = enqueued.clone();

        promise.resolve_with_enqueue(Value::number(1.0), move |task| {
            enqueued_clone.lock().push(task);
        });

        // Callback not yet called
        assert_eq!(order.load(Ordering::Relaxed), 0);

        // Run enqueued tasks
        let tasks = std::mem::take(&mut *enqueued.lock());
        for task in tasks {
            task();
        }

        assert_eq!(order.load(Ordering::Relaxed), 2);
    }
}
