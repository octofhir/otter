//! Cooperative cancellation shared by an isolate and one compiler job.
//!
//! # Contents
//! - [`JitCompileCancellation`] is the one monotonic stop token carried by
//!   the pending owner, queued request and active compiler.
//!
//! # Invariants
//! Cancellation never releases source or GC owners. The queue retains those
//! until the compiler acknowledges completion; publication checks the token
//! again even when a compiler does not cooperate. The flag publishes no data:
//! request and result publication use the queue's mutex.
//!
//! # See also
//! - [`crate::jit_compile_queue`] owns dispatch, acknowledgement and shutdown.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

/// Monotonic cancellation of one compile request and every clone of it.
#[derive(Debug, Clone, Default)]
pub struct JitCompileCancellation(Arc<AtomicBool>);

impl JitCompileCancellation {
    /// Ask the compiler to stop at its next cooperative boundary.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    /// Whether this request has been cancelled.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}
