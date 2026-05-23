//! Cold side records for interpreter call frames.
//!
//! Each [`crate::Frame`] carries an `Option<ColdFrameIdx>` slot. When
//! an opcode needs cold protocol state (try handlers, async parking,
//! pending ToPrimitive / bind / iterator ladder, etc.) it acquires a
//! slot from the per-interpreter [`ColdFramePool`] and writes through
//! it. Frames that never run a cold-state opcode (most short helpers,
//! arithmetic-only inner loops) never touch the pool.
//!
//! # Contents
//! - [`ColdFrame`] — the cold half of a call frame's bookkeeping.
//! - [`ColdFrameIdx`] — niche-encoded handle into the pool.
//! - [`ColdFramePool`] — Interpreter-owned slot + freelist storage.
//!
//! # Invariants
//! - `ColdFrameIdx` indexes `ColdFramePool::slots` as `idx.get() - 1`.
//! - Released slots are reset to [`ColdFrame::default`] so a freshly
//!   acquired slot never observes a previous frame's state.
//! - Frames parked off the dispatcher stack (async await / generator
//!   yield) **must** detach their cold record (see
//!   [`ColdFramePool::detach`]) before being stored on a heap-owned
//!   continuation, then re-attach via [`ColdFramePool::attach`] on
//!   resume. Pool indices are not stable across detach/attach.
//!
//! # See also
//! - [`crate::frame_state`]

use std::num::NonZeroU32;

/// Niche-encoded handle into a [`ColdFramePool`]. Stored as
/// `Option<ColdFrameIdx>` (4 bytes) on the hot frame; `None` means no
/// cold record has been acquired yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(transparent)]
pub struct ColdFrameIdx(NonZeroU32);

impl ColdFrameIdx {
    /// Index into `ColdFramePool::slots` (zero-based).
    #[inline]
    #[must_use]
    pub fn slot(self) -> usize {
        self.0.get() as usize - 1
    }

    #[inline]
    fn from_slot(slot: u32) -> Self {
        // Slot indices originate from `slots.len()` (already < u32::MAX
        // for any practical pool) — `slot + 1` cannot wrap.
        Self(NonZeroU32::new(slot + 1).expect("pool slot fits in u32"))
    }
}

/// Cold half of a call frame. Pool-allocated and shared via
/// [`ColdFrameIdx`].
///
/// Empty in this scaffolding commit; subsequent commits migrate the
/// individual cold fields (`handlers`, `module_url`, `pending_*`,
/// `async_state`, `generator_owner`, …) out of [`crate::Frame`] and
/// into this struct.
#[derive(Debug, Default, Clone)]
pub struct ColdFrame {}

impl ColdFrame {
    /// Whether this slot is logically empty (no cold state worth
    /// keeping). The pool consults this on `release` to assert that
    /// callers cleared their state before handing the slot back.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        // No fields yet; always empty.
        true
    }
}

/// Per-interpreter pool of cold frame records.
///
/// Growth is monotonic; the freelist makes reuse O(1). Typical peak
/// occupancy is on the order of the live frame stack depth (dozens),
/// so the backing `Vec<ColdFrame>` stays small.
#[derive(Debug, Default)]
pub struct ColdFramePool {
    slots: Vec<ColdFrame>,
    free: Vec<u32>,
}

impl ColdFramePool {
    /// Construct an empty pool. Equivalent to [`Self::default`].
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Drop every pool slot. Called when the interpreter is reset
    /// between top-level runs.
    pub fn clear(&mut self) {
        self.slots.clear();
        self.free.clear();
    }

    /// Acquire a fresh, zeroed slot and return its handle.
    pub fn acquire(&mut self) -> ColdFrameIdx {
        if let Some(slot) = self.free.pop() {
            // Slot was reset on release; nothing to do here.
            return ColdFrameIdx::from_slot(slot);
        }
        let slot = u32::try_from(self.slots.len()).expect("cold-frame pool fits in u32");
        self.slots.push(ColdFrame::default());
        ColdFrameIdx::from_slot(slot)
    }

    /// Hand a slot back to the freelist. Resets the slot so future
    /// callers never observe leftover state.
    pub fn release(&mut self, idx: ColdFrameIdx) {
        let slot = idx.slot();
        self.slots[slot] = ColdFrame::default();
        self.free.push(slot as u32);
    }

    /// Detach an owned cold record from the pool. Used when a frame is
    /// parked off the dispatcher stack (async await, generator yield)
    /// and must carry its cold state on a heap-owned continuation.
    pub fn detach(&mut self, idx: ColdFrameIdx) -> ColdFrame {
        let slot = idx.slot();
        let owned = std::mem::take(&mut self.slots[slot]);
        self.free.push(slot as u32);
        owned
    }

    /// Re-attach an owned cold record into the pool. Used on
    /// async/generator resume to restore the parked cold state.
    pub fn attach(&mut self, cold: ColdFrame) -> ColdFrameIdx {
        let idx = self.acquire();
        self.slots[idx.slot()] = cold;
        idx
    }

    /// Borrow a slot. `None` is not possible for valid `idx`; the
    /// option shape is for caller ergonomics with `Option<ColdFrameIdx>`.
    #[inline]
    #[must_use]
    pub fn get(&self, idx: ColdFrameIdx) -> &ColdFrame {
        &self.slots[idx.slot()]
    }

    /// Mutable borrow of a slot.
    #[inline]
    #[must_use]
    pub fn get_mut(&mut self, idx: ColdFrameIdx) -> &mut ColdFrame {
        &mut self.slots[idx.slot()]
    }

    /// Number of live (acquired but not released) slots.
    #[must_use]
    pub fn live_len(&self) -> usize {
        self.slots.len() - self.free.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acquire_release_round_trip() {
        let mut pool = ColdFramePool::new();
        let a = pool.acquire();
        let b = pool.acquire();
        assert_ne!(a, b);
        assert_eq!(pool.live_len(), 2);
        pool.release(a);
        assert_eq!(pool.live_len(), 1);
        let c = pool.acquire();
        // Freelist reuse: c should reclaim a's slot.
        assert_eq!(c, a);
        pool.release(b);
        pool.release(c);
        assert_eq!(pool.live_len(), 0);
    }

    #[test]
    fn idx_size_is_four() {
        // Hot frame depends on this niche encoding for its size goal.
        assert_eq!(
            std::mem::size_of::<Option<ColdFrameIdx>>(),
            std::mem::size_of::<u32>(),
        );
    }

    #[test]
    fn detach_attach_round_trip() {
        let mut pool = ColdFramePool::new();
        let a = pool.acquire();
        let cold = pool.detach(a);
        assert!(cold.is_empty());
        // Detach freed the slot; next acquire reuses it.
        let b = pool.acquire();
        assert_eq!(a, b);
        let _re = pool.attach(cold);
    }
}
