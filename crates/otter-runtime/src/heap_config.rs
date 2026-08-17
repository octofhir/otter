//! Process-level managed-heap configuration.
//!
//! # Contents
//! - [`MANAGED_HEAP_PAGE_BYTES`] — accounting unit used by runtime diagnostics.
//! - [`configured_managed_heap_bytes`] — current process reservation.
//! - [`initialize_managed_heap`] — early one-shot reservation for harnesses
//!   that create many isolates.
//!
//! # Invariants
//! - This surface exposes owned scalar configuration only: no heap reference,
//!   raw collector handle, visitor, root slot, or mutation API crosses the
//!   runtime boundary.
//! - Initialization remains process-global and must precede construction of
//!   the first runtime.
//!
//! # See also
//! - [`crate::RuntimeBuilder`] for per-isolate heap limits.

use crate::{ConfigError, OtterError};

/// Byte size of one managed-heap accounting page.
pub const MANAGED_HEAP_PAGE_BYTES: usize = otter_gc::PAGE_SIZE;

/// Current process-wide managed-heap reservation, or zero before startup.
///
/// This is harness plumbing, not an embedder mutation API.
#[doc(hidden)]
#[must_use]
pub fn configured_managed_heap_bytes() -> usize {
    otter_gc::cage_size()
}

/// Reserve the process-wide managed-heap region before the first runtime.
///
/// This narrow scalar API exists for multi-isolate conformance harnesses. It
/// deliberately does not re-export the collector or its error types.
///
/// # Errors
///
/// Returns a configuration error when `bytes` is invalid or the process heap
/// has already been initialized.
#[doc(hidden)]
pub fn initialize_managed_heap(bytes: usize) -> Result<(), OtterError> {
    otter_gc::init_cage_with_size(bytes).map_err(|error| OtterError::Config {
        reason: ConfigError::InvalidHeapLimit {
            message: error.to_string(),
        },
    })
}
