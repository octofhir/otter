//! Host service interfaces used by the isolate runner.
//!
//! The runtime owns VM state; host executors own blocking, async,
//! and OS-facing work. This module defines the narrow synchronous
//! service handles that the runtime may call after a message has
//! already crossed back onto the isolate thread.
//!
//! # Contents
//!
//! - [`HttpsModuleFetcher`] — starts UTF-8 module source fetches
//!   for capability-approved HTTPS dynamic imports.
//! - [`HttpsModuleFetchSink`] — receives owned fetch results and
//!   posts them back to the runtime inbox.
//! - [`HttpsModuleFetcherHandle`] — cloneable shared service handle.
//!
//! # Invariants
//!
//! - Service handles carry no VM, GC, or bytecode state.
//! - Tokio and other executor-specific types stay behind service
//!   implementations, not on [`crate::Runtime`].
//! - Results are owned data that can be parsed, compiled, and
//!   executed only after returning to the isolate thread.
//!
//! # See also
//!
//! - [`crate::handle`] for the isolate runner wiring.
//! - [`crate::event_loop`] for the Tokio-backed default host loop.

use std::sync::Arc;

/// Sink notified when an HTTPS module fetch completes.
pub(crate) trait HttpsModuleFetchSink: Send + Sync + 'static {
    /// Deliver the owned UTF-8 source text or a diagnostic string.
    fn fetched(&self, result: Result<String, String>);
}

/// Host service for HTTPS module source loading.
pub(crate) trait HttpsModuleFetcher: std::fmt::Debug + Send + Sync + 'static {
    /// Start fetching `url` and deliver the result through `sink`.
    fn fetch_utf8(&self, url: String, sink: Arc<dyn HttpsModuleFetchSink>);
}

/// Cloneable handle for the HTTPS module fetch service.
pub(crate) type HttpsModuleFetcherHandle = Arc<dyn HttpsModuleFetcher>;
