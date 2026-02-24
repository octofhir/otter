//! Async runtime abstraction for the event loop.
//!
//! Provides the [`AsyncRuntime`] trait to decouple the event loop from tokio,
//! enabling testability with controllable time and deterministic scheduling.

use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, Instant};

/// Abstraction over async primitives used by the event loop.
///
/// The default implementation ([`TokioRuntime`]) delegates to tokio.
/// Tests can use [`MockRuntime`] for deterministic, controllable time.
pub trait AsyncRuntime: Send + Sync + 'static {
    /// Returns the current instant (monotonic clock).
    fn now(&self) -> Instant;

    /// Returns a future that completes after `duration`.
    fn sleep(&self, duration: Duration) -> Pin<Box<dyn Future<Output = ()> + Send>>;

    /// Returns a future that yields control to the async executor.
    fn yield_now(&self) -> Pin<Box<dyn Future<Output = ()> + Send>>;
}

/// Production [`AsyncRuntime`] backed by tokio.
pub struct TokioRuntime;

impl AsyncRuntime for TokioRuntime {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn sleep(&self, duration: Duration) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        Box::pin(tokio::time::sleep(duration))
    }

    fn yield_now(&self) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        Box::pin(tokio::task::yield_now())
    }
}

#[cfg(test)]
pub mod mock {
    use super::*;
    use std::sync::Mutex;

    /// Mock [`AsyncRuntime`] with controllable time for deterministic tests.
    pub struct MockRuntime {
        now: Mutex<Instant>,
    }

    impl MockRuntime {
        /// Create a new mock runtime with time starting at `Instant::now()`.
        pub fn new() -> Self {
            Self {
                now: Mutex::new(Instant::now()),
            }
        }

        /// Advance the mock clock by `duration`.
        pub fn advance(&self, duration: Duration) {
            let mut now = self.now.lock().unwrap();
            *now += duration;
        }
    }

    impl AsyncRuntime for MockRuntime {
        fn now(&self) -> Instant {
            *self.now.lock().unwrap()
        }

        fn sleep(&self, _duration: Duration) -> Pin<Box<dyn Future<Output = ()> + Send>> {
            // In mock mode, sleep completes immediately — tests advance time explicitly.
            Box::pin(std::future::ready(()))
        }

        fn yield_now(&self) -> Pin<Box<dyn Future<Output = ()> + Send>> {
            Box::pin(std::future::ready(()))
        }
    }
}
