//! RAII accounting for native and off-object backing stores.
//!
//! GC cells do not include bytes owned by Rust containers such as
//! array elements, typed-array buffers, module source caches, or host
//! native resources. [`ExternalMemory`] books those bytes against the
//! owning heap cap and releases them on drop.
//!
//! # Contents
//!
//! - [`ExternalMemory`] — non-send RAII reservation token.
//! - [`SharedExternalMemory`] — thread-safe release token for shared backing
//!   stores.
//!
//! # Invariants
//!
//! - Tokens hold no heap pointer: releases record into the owning heap's
//!   shared release channel and are drained by the next accounting
//!   operation or stats query. A token dropped during or after heap
//!   teardown (payload drops, moved isolates) is therefore sound.
//! - Resizing books the delta against the caller-provided owning heap
//!   before publishing a larger byte count.
//! - Dropping, shrinking, or releasing gives back exactly the currently
//!   booked bytes, saturating at the heap's outstanding reservation.
//!
//! # See also
//!
//! - [GC API](../../../docs/book/src/engine/gc-api.md)
//! - [Startup performance](../../../docs/book/src/performance/startup.md)

use std::marker::PhantomData;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use crate::heap::GcHeap;
use crate::oom::OutOfMemory;

/// Heap-owned state for external reservations whose owner may be
/// dropped away from the mutator thread.
#[derive(Debug, Default)]
pub(crate) struct SharedExternalState {
    released_bytes: AtomicU64,
}

impl SharedExternalState {
    pub(crate) fn release(&self, bytes: u64) {
        self.released_bytes.fetch_add(bytes, Ordering::AcqRel);
    }

    pub(crate) fn released_bytes(&self) -> u64 {
        self.released_bytes.load(Ordering::Acquire)
    }

    pub(crate) fn take_released_bytes(&self) -> u64 {
        self.released_bytes.swap(0, Ordering::AcqRel)
    }
}

/// RAII reservation for memory outside GC cell payloads.
///
/// The token is isolate-local and intentionally `!Send + !Sync`. It holds
/// no heap pointer: dropping or releasing records the byte count in the
/// owning heap's shared release channel, which the heap drains on its next
/// accounting operation or stats query. This keeps payload-embedded tokens
/// sound across isolate moves and heap teardown.
///
/// # Example
///
/// ```
/// let mut heap = otter_gc::GcHeap::with_max_heap_bytes(4096).unwrap();
/// let mut backing = heap.reserve_external(1024).unwrap();
/// assert_eq!(backing.bytes(), 1024);
///
/// backing.resize(&mut heap, 2048).unwrap();
/// assert_eq!(backing.bytes(), 2048);
///
/// backing.release();
/// assert_eq!(heap.tracked_bytes(), 0);
/// ```
pub struct ExternalMemory {
    state: Arc<SharedExternalState>,
    bytes: u64,
    _not_send: PhantomData<*mut ()>,
}

impl ExternalMemory {
    pub(crate) fn new(heap: &mut GcHeap, bytes: u64) -> Result<Self, OutOfMemory> {
        heap.reserve_bytes(bytes)?;
        Ok(Self {
            state: heap.shared_external_state(),
            bytes,
            _not_send: PhantomData,
        })
    }

    pub(crate) fn new_with_roots(
        heap: &mut GcHeap,
        bytes: u64,
        external_visit: &mut crate::heap::RootSlotVisitor<'_>,
    ) -> Result<Self, OutOfMemory> {
        heap.reserve_bytes_with_roots(bytes, external_visit)?;
        Ok(Self {
            state: heap.shared_external_state(),
            bytes,
            _not_send: PhantomData,
        })
    }

    /// Currently reserved byte count.
    #[must_use]
    pub const fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Resize this reservation against its owning heap.
    ///
    /// # Errors
    ///
    /// Returns [`OutOfMemory`] when growing would exceed the owning
    /// heap cap or the shared external-bytes budget.
    pub fn resize(&mut self, heap: &mut GcHeap, new_bytes: u64) -> Result<(), OutOfMemory> {
        debug_assert!(
            Arc::ptr_eq(&self.state, &heap.shared_external_state()),
            "external tokens resize only against their owning heap"
        );
        if new_bytes == self.bytes {
            return Ok(());
        }
        if new_bytes > self.bytes {
            let delta = new_bytes - self.bytes;
            heap.reserve_bytes(delta)?;
        } else {
            heap.release_bytes(self.bytes - new_bytes);
        }
        self.bytes = new_bytes;
        Ok(())
    }

    /// Release the full reservation before drop.
    pub fn release(mut self) {
        self.release_current();
    }

    fn release_current(&mut self) {
        if self.bytes == 0 {
            return;
        }
        self.state.release(self.bytes);
        self.bytes = 0;
    }
}

impl Drop for ExternalMemory {
    fn drop(&mut self) {
        self.release_current();
    }
}

impl std::fmt::Debug for ExternalMemory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExternalMemory")
            .field("bytes", &self.bytes)
            .finish_non_exhaustive()
    }
}

/// RAII reservation for backing stores shared outside the mutator thread.
///
/// Dropping this token does not touch [`GcHeap`] directly. Instead it records
/// the released byte count in heap-owned atomic state; the owning heap drains
/// those releases on the next accounting operation or stats query. This keeps
/// cross-thread `Arc` payload drops from mutating isolate-local heap state.
#[derive(Debug)]
pub struct SharedExternalMemory {
    state: Arc<SharedExternalState>,
    bytes: u64,
}

impl SharedExternalMemory {
    pub(crate) fn new(state: Arc<SharedExternalState>, bytes: u64) -> Self {
        Self { state, bytes }
    }

    /// Currently reserved byte count.
    #[must_use]
    pub const fn bytes(&self) -> u64 {
        self.bytes
    }
}

impl Drop for SharedExternalMemory {
    fn drop(&mut self) {
        if self.bytes == 0 {
            return;
        }
        self.state.release(self.bytes);
        self.bytes = 0;
    }
}
