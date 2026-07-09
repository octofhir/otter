//! `OutOfMemory` error type surfaced by allocation paths.
//!
//! # Contents
//!
//! - [`OutOfMemory`] — refusal returned by [`crate::heap::GcHeap::alloc`]
//!   and [`crate::heap::GcHeap::reserve_bytes`].
//!
//! # Invariants
//!
//! - The cap is **load-bearing**: when the heap rejects an
//!   allocation, the slot is **not** materialised. The legacy
//!   "cap check sets a flag but the alloc still runs" anti-pattern
//!   is forbidden (GC architecture plan §2.1 caveat).
//! - `HeapCapExceeded` carries `requested_bytes` and
//!   `heap_limit_bytes` so the host can map directly onto
//!   `OtterError::OutOfMemory` without re-deriving them.
//!
//! # See also
//!
//! - GC architecture plan §1.2 NF3 (cap enforcement),
//!   §2.1 (legacy `would_exceed_limit` caveat),
//!   §2.3 inheritance ledger row 4, §7.5.
//! - Task 73 — `Runtime::max_heap_bytes` from informational →
//!   load-bearing.

/// Allocation refused — cage exhausted or heap cap exceeded.
///
/// Returned by [`crate::heap::GcHeap::alloc`] and
/// [`crate::heap::GcHeap::reserve_bytes`] as a recoverable error;
/// surfaces through `otter-runtime` as
/// `OtterError::OutOfMemory` (catchable as JS `RangeError` after
/// task 84's spec-shaped wrapper).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum OutOfMemory {
    /// The cage's page free-list is empty — no further pages can
    /// be carved.
    #[error("out of memory: cage exhausted")]
    CageExhausted,
    /// The configured per-heap cap was exceeded; an emergency
    /// full GC could not free enough room for the allocation.
    #[error(
        "out of memory: heap cap exceeded ({requested_bytes} requested, limit {heap_limit_bytes})"
    )]
    HeapCapExceeded {
        /// Bytes requested by the rejected allocation.
        requested_bytes: u64,
        /// Configured heap cap (`0` = disabled — never raised in
        /// that case).
        heap_limit_bytes: u64,
    },
    /// A single allocation cannot fit in the collector's one-page large-object
    /// layout. This is reported explicitly instead of panicking in `bump_alloc`.
    #[error(
        "out of memory: allocation too large ({requested_bytes} requested, maximum {max_bytes})"
    )]
    AllocationTooLarge {
        /// Aligned bytes requested, including the GC header.
        requested_bytes: u64,
        /// Maximum bytes supported by one GC page.
        max_bytes: u64,
    },
}

impl OutOfMemory {
    /// Bytes the rejected allocation requested. `0` for cage
    /// exhaustion (we cannot attribute a single requester).
    #[must_use]
    pub fn requested_bytes(&self) -> u64 {
        match self {
            Self::CageExhausted => 0,
            Self::HeapCapExceeded {
                requested_bytes, ..
            }
            | Self::AllocationTooLarge {
                requested_bytes, ..
            } => *requested_bytes,
        }
    }

    /// Configured heap cap at the time of the refusal. `0` for
    /// cage exhaustion (the cage has its own size, not a cap).
    #[must_use]
    pub fn heap_limit_bytes(&self) -> u64 {
        match self {
            Self::CageExhausted | Self::AllocationTooLarge { .. } => 0,
            Self::HeapCapExceeded {
                heap_limit_bytes, ..
            } => *heap_limit_bytes,
        }
    }
}
