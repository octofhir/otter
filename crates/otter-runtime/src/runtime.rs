//! JSC Runtime pool for multi-threaded execution
//!
//! JSC contexts are not thread-safe, so we use a pool of contexts
//! and round-robin selection to support concurrent execution.

use crate::apis::register_all_apis;
use crate::context::JscContext;
use crate::error::{JscError, JscResult};
use crate::extension::Extension;
use crate::value::JscValue;
use parking_lot::Mutex;
use serde::Serialize;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tracing::{debug, warn};

/// Configuration for the JSC runtime pool
#[derive(Debug, Clone)]
pub struct JscConfig {
    /// Number of JSC context instances in the pool
    pub pool_size: usize,

    /// Maximum execution time for a script in milliseconds
    pub timeout_ms: u64,

    /// Enable console.log output
    pub enable_console: bool,
}

impl Default for JscConfig {
    fn default() -> Self {
        Self {
            pool_size: num_cpus::get().max(1),
            timeout_ms: 5000,
            enable_console: true,
        }
    }
}

/// A single JSC runtime instance
pub struct JscRuntime {
    context: JscContext,
    #[allow(dead_code)]
    config: JscConfig,
}

impl JscRuntime {
    /// Create a new runtime with the given configuration
    pub fn new(config: JscConfig) -> JscResult<Self> {
        let context = JscContext::new()?;

        // Register default native APIs (console, http)
        register_all_apis(context.raw())?;

        Ok(Self { context, config })
    }

    /// Evaluate a script with the given context variables
    pub fn eval(&self, script: &str) -> JscResult<JscValue> {
        let start = Instant::now();

        let result = self.context.eval(script)?;

        let elapsed = start.elapsed();
        debug!(
            elapsed_ms = elapsed.as_millis() as u64,
            "Script evaluation completed"
        );

        Ok(result)
    }

    /// Evaluate a script with context variables injected
    pub fn eval_with_context<T: Serialize>(
        &self,
        script: &str,
        context_name: &str,
        context_value: &T,
    ) -> JscResult<JscValue> {
        let json = serde_json::to_string(context_value)?;
        self.context.inject_json(context_name, &json)?;

        self.eval(script)
    }

    /// Get a reference to the underlying context
    pub fn context(&self) -> &JscContext {
        &self.context
    }

    /// Register a safe extension on this runtime
    pub fn register_extension(&self, extension: Extension) -> JscResult<()> {
        self.context.register_extension(extension)
    }

    /// Poll pending async ops and resolve Promises
    pub fn poll_promises(&self) -> JscResult<usize> {
        self.context.poll_promises()
    }

    /// Run the full event loop tick (microtasks + timers + async ops)
    pub fn poll_event_loop(&self) -> JscResult<usize> {
        self.context.poll_event_loop()
    }

    /// Run the event loop until idle or timeout
    pub fn run_event_loop_until_idle(&self, timeout: Duration) -> JscResult<()> {
        self.context.run_event_loop_until_idle(timeout)
    }

    pub fn has_pending_tasks(&self) -> bool {
        self.context.has_pending_tasks()
    }

    pub fn next_wake_delay(&self) -> Duration {
        self.context.next_wake_delay()
    }

    /// Force garbage collection
    pub fn gc(&self) {
        self.context.gc();
    }
}

/// A pool of JSC runtime instances for concurrent execution
///
/// Since JSC contexts are not thread-safe, this pool maintains
/// multiple instances and uses round-robin selection to distribute
/// evaluations across them.
pub struct JscRuntimePool {
    instances: Vec<Mutex<JscRuntime>>,
    counter: AtomicUsize,
    config: JscConfig,
}

/// Background promise driver for async ops.
///
/// Call `shutdown` to stop the polling loop gracefully.
pub struct PromiseDriver {
    stop: Option<oneshot::Sender<()>>,
    handle: JoinHandle<()>,
}

impl PromiseDriver {
    pub async fn shutdown(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        let _ = self.handle.await;
    }

    pub fn abort(&self) {
        self.handle.abort();
    }
}

impl JscRuntimePool {
    /// Create a new runtime pool with the given configuration
    pub fn new(config: JscConfig) -> JscResult<Self> {
        let pool_size = config.pool_size.max(1);
        let mut instances = Vec::with_capacity(pool_size);

        for i in 0..pool_size {
            let runtime = JscRuntime::new(config.clone())?;
            instances.push(Mutex::new(runtime));
            debug!(instance = i, "Created JSC runtime instance");
        }

        Ok(Self {
            instances,
            counter: AtomicUsize::new(0),
            config,
        })
    }

    /// Evaluate a script using a pooled runtime instance
    pub fn eval(&self, script: &str) -> JscResult<JscValue> {
        let idx = self.select_instance();
        let runtime = self.instances[idx].lock();
        runtime.eval(script)
    }

    /// Evaluate a script with context variables
    pub fn eval_with_context<T: Serialize>(
        &self,
        script: &str,
        context_name: &str,
        context_value: &T,
    ) -> JscResult<JscValue> {
        let idx = self.select_instance();
        let runtime = self.instances[idx].lock();
        runtime.eval_with_context(script, context_name, context_value)
    }

    /// Try to acquire a runtime instance with timeout
    pub fn try_eval(&self, script: &str, timeout: Duration) -> JscResult<JscValue> {
        let idx = self.select_instance();

        match self.instances[idx].try_lock_for(timeout) {
            Some(runtime) => runtime.eval(script),
            None => {
                warn!(
                    instance = idx,
                    timeout_ms = timeout.as_millis() as u64,
                    "Failed to acquire runtime instance"
                );
                Err(JscError::PoolExhausted)
            }
        }
    }

    /// Register additional APIs on all runtime contexts
    pub fn register_apis<F>(&self, register: F) -> JscResult<()>
    where
        F: Fn(crate::bindings::JSContextRef) -> JscResult<()>,
    {
        for instance in &self.instances {
            let runtime = instance.lock();
            register(runtime.context().raw())?;
        }
        Ok(())
    }

    /// Register a safe extension on all runtime contexts
    pub fn register_extension(&self, extension: Extension) -> JscResult<()> {
        for instance in &self.instances {
            let runtime = instance.lock();
            runtime.register_extension(extension.clone())?;
        }
        Ok(())
    }

    /// Poll pending async ops on all contexts
    pub fn poll_promises(&self) -> JscResult<usize> {
        let mut resolved = 0;
        for instance in &self.instances {
            let runtime = instance.lock();
            resolved += runtime.poll_promises()?;
        }
        Ok(resolved)
    }

    /// Run the full event loop tick (microtasks + timers + async ops)
    pub fn poll_event_loop(&self) -> JscResult<usize> {
        let mut handled = 0;
        for instance in &self.instances {
            let runtime = instance.lock();
            handled += runtime.poll_event_loop()?;
        }
        Ok(handled)
    }

    /// Run the event loop until idle or timeout
    pub fn run_event_loop_until_idle(&self, timeout: Duration) -> JscResult<()> {
        let start = Instant::now();
        loop {
            let mut handled = 0;
            let mut pending = false;
            let mut next_delay = Duration::from_millis(1);

            for instance in &self.instances {
                let runtime = instance.lock();
                handled += runtime.poll_event_loop()?;
                if runtime.has_pending_tasks() {
                    pending = true;
                    let candidate = runtime.next_wake_delay();
                    if candidate < next_delay {
                        next_delay = candidate;
                    }
                }
            }

            if handled == 0 && !pending {
                return Ok(());
            }

            if timeout != Duration::ZERO && start.elapsed() >= timeout {
                return Err(JscError::Timeout(timeout.as_millis() as u64));
            }

            std::thread::sleep(next_delay.max(Duration::from_millis(1)));
        }
    }

    /// Start a background promise driver that polls async ops.
    ///
    /// The pool must be wrapped in `Arc` before calling this method.
    pub fn start_promise_driver(self: Arc<Self>, interval: Duration) -> PromiseDriver {
        let (stop, mut stop_rx) = oneshot::channel();
        let handle = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            loop {
                tokio::select! {
                    _ = ticker.tick() => {
                        if let Err(err) = self.poll_event_loop() {
                            warn!(error = %err, "Failed to poll event loop");
                        }
                    }
                    _ = &mut stop_rx => {
                        break;
                    }
                }
            }
        });

        PromiseDriver {
            stop: Some(stop),
            handle,
        }
    }

    /// Get the pool size
    pub fn pool_size(&self) -> usize {
        self.instances.len()
    }

    /// Get the configuration
    pub fn config(&self) -> &JscConfig {
        &self.config
    }

    /// Force garbage collection on all instances
    pub fn gc_all(&self) {
        for (i, instance) in self.instances.iter().enumerate() {
            if let Some(runtime) = instance.try_lock() {
                runtime.gc();
                debug!(instance = i, "Garbage collected");
            }
        }
    }

    /// Select the next instance using round-robin
    fn select_instance(&self) -> usize {
        self.counter.fetch_add(1, Ordering::Relaxed) % self.instances.len()
    }
}

unsafe impl Send for JscRuntimePool {}
unsafe impl Sync for JscRuntimePool {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_runtime_creation() {
        let config = JscConfig {
            pool_size: 1,
            ..Default::default()
        };
        let runtime = JscRuntime::new(config).unwrap();
        drop(runtime);
    }

    #[test]
    fn test_runtime_eval() {
        let config = JscConfig {
            pool_size: 1,
            ..Default::default()
        };
        let runtime = JscRuntime::new(config).unwrap();
        let result = runtime.eval("1 + 1").unwrap();
        assert_eq!(result.to_number().unwrap(), 2.0);
    }

    #[test]
    fn test_pool_creation() {
        let config = JscConfig {
            pool_size: 4,
            ..Default::default()
        };
        let pool = JscRuntimePool::new(config).unwrap();
        assert_eq!(pool.pool_size(), 4);
    }

    #[test]
    fn test_pool_eval() {
        let config = JscConfig {
            pool_size: 2,
            ..Default::default()
        };
        let pool = JscRuntimePool::new(config).unwrap();

        let result = pool.eval("2 * 3").unwrap();
        assert_eq!(result.to_number().unwrap(), 6.0);

        for i in 0..10 {
            let script = format!("{} + 1", i);
            let result = pool.eval(&script).unwrap();
            assert_eq!(result.to_number().unwrap(), (i + 1) as f64);
        }
    }

    #[test]
    fn test_pool_with_context() {
        let config = JscConfig {
            pool_size: 1,
            ..Default::default()
        };
        let pool = JscRuntimePool::new(config).unwrap();

        #[derive(Serialize)]
        struct TestContext {
            value: i32,
        }

        let ctx = TestContext { value: 42 };
        let result = pool
            .eval_with_context("ctx.value * 2", "ctx", &ctx)
            .unwrap();
        assert_eq!(result.to_number().unwrap(), 84.0);
    }
}
