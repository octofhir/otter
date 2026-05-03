//! `OutOfMemory` error type surfaced by allocation paths.
//!
//! # Contents
//!
//! - [`OutOfMemory`] — thrown when (a) the cage is exhausted or
//!   (b) (Phase 2) the configured per-heap cap is exceeded.
//!
//! # See also
//!
//! - GC architecture plan §1.2 NF3 (cap enforcement) and Task 73
//!   for the `Runtime::max_heap_bytes` integration.

/// Allocation refused — cage exhausted or heap cap exceeded.
///
/// Returned by [`crate::heap::GcHeap::alloc`] as a recoverable
/// error; surfaces through `otter-runtime` as a catchable
/// `RangeError`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum OutOfMemory {
    /// The cage's page free-list is empty — no further pages can
    /// be carved.
    #[error("out of memory: cage exhausted")]
    CageExhausted,
    /// The configured per-heap soft cap was exceeded. Reserved
    /// for Task 73; not raised by Task 72 logic.
    #[error("out of memory: heap cap exceeded")]
    HeapCapExceeded,
}
