//! Tri-color marking worklist for STW old-gen collection.
//!
//! # Algorithm
//!
//! 1. Roots → enqueue each header onto the worklist, paint gray.
//! 2. Drain the worklist: pop a gray header, trace its slots,
//!    paint each unmarked child gray and enqueue it, paint the
//!    parent black. Repeat until empty.
//! 3. After drain, every reachable object is black; everything
//!    still white is garbage.
//!
//! # Contents
//!
//! - [`MarkingState`] — the worklist + the `is_marking` flag the
//!   barrier checks.
//!
//! # Invariants
//!
//! - `is_marking == true` only between [`MarkingState::start_cycle`]
//!   and [`MarkingState::finish_cycle`]. The Phase-1 STW path
//!   never observes the flag set; Phase 2 lights the insertion
//!   barrier when this flag is true.
//! - The worklist contains only gray headers. Black and white
//!   never appear in the queue.
//! - `drain_with_budget` is dormant in Phase 1 (the STW path uses
//!   `drain_full`); the API exists so Phase 2 / task 86 can
//!   adopt incremental marking without an audit sweep.
//!
//! # See also
//!
//! - GC architecture plan §2.3 (MarkingState row), §5 (barriers).
//! - V8 incremental marker, [V8 blog 2018-04 Concurrent
//!   marking](https://v8.dev/blog/concurrent-marking) (read-only
//!   inspiration; we are STW in Phase 1).

use std::collections::VecDeque;

use crate::compressed::{RawGc, cage_base};
use crate::header::{GcHeader, MarkColor};
use crate::page::Page;
use crate::trace::TraceTable;

/// Worklist + book-keeping for a marking cycle.
pub struct MarkingState {
    worklist: VecDeque<*mut GcHeader>,
    /// Bytes covered by black objects discovered so far this
    /// cycle. The sweeper consults this for live-set accounting.
    pub live_bytes: usize,
    /// `true` while a marking cycle is active. Phase-1 keeps it
    /// false everywhere except inside `drain_full`; Phase 2 (task
    /// 86) flips it on at cycle start and off at cycle finish so
    /// the insertion barrier wakes up.
    is_marking: bool,
}

impl Default for MarkingState {
    fn default() -> Self {
        Self::new()
    }
}

impl MarkingState {
    /// Empty state.
    pub fn new() -> Self {
        Self {
            worklist: VecDeque::new(),
            live_bytes: 0,
            is_marking: false,
        }
    }

    /// True while a marking cycle is in progress.
    #[inline]
    pub fn is_marking(&self) -> bool {
        self.is_marking
    }

    /// Push `header` onto the worklist if it is currently white.
    /// Idempotent — re-marking the same header is a no-op.
    ///
    /// # Safety
    ///
    /// `header` must point to a live, valid `GcHeader`.
    pub unsafe fn shade_gray(&mut self, header: *mut GcHeader) {
        // SAFETY: caller guarantees liveness of header.
        unsafe {
            let h = &*header;
            if matches!(h.mark_color(), MarkColor::White) {
                h.set_mark_color(MarkColor::Gray);
                self.worklist.push_back(header);
            }
        }
    }

    /// Mark the header reached from a [`RawGc`] slot, no-op if
    /// the slot is null.
    ///
    /// # Safety
    ///
    /// `slot` must address a valid initialised `RawGc` pointing
    /// inside the cage (or null).
    pub unsafe fn shade_from_slot(&mut self, slot: *mut RawGc) {
        // SAFETY: by precondition slot is dereferenceable and
        // (if non-null) points at a live cage object.
        unsafe {
            let raw = (*slot).0;
            if raw == 0 {
                return;
            }
            // SAFETY: raw is a valid in-cage offset by trace
            // table contract.
            let header = cage_base().add(raw as usize) as *mut GcHeader;
            self.shade_gray(header);
        }
    }

    /// Begin a marking cycle (Phase 2 hook).
    pub fn start_cycle(&mut self) {
        debug_assert!(!self.is_marking);
        self.is_marking = true;
        self.live_bytes = 0;
        debug_assert!(self.worklist.is_empty());
    }

    /// End the marking cycle.
    pub fn finish_cycle(&mut self) {
        self.is_marking = false;
        debug_assert!(self.worklist.is_empty());
    }

    /// Drain the worklist completely under the supplied trace
    /// table. Used by the STW old-gen mark phase.
    ///
    /// # Safety
    ///
    /// Every header pushed via `shade_gray` must outlive the
    /// drain (Phase-1 STW guarantees this — the mutator is
    /// paused).
    pub unsafe fn drain_full(&mut self, trace_table: &TraceTable) {
        // SAFETY: caller-side invariant from STW pause.
        unsafe {
            while let Some(header) = self.worklist.pop_front() {
                self.process_one(header, trace_table);
            }
        }
    }

    /// Drain at most `budget` headers and return how many were
    /// processed. Phase-1 caller never invokes this — it exists
    /// so the Phase-2 incremental driver (task 86) can reuse the
    /// same marking surface.
    ///
    /// # Safety
    ///
    /// Same as [`drain_full`].
    pub unsafe fn drain_with_budget(&mut self, budget: usize, trace_table: &TraceTable) -> usize {
        let mut done = 0;
        // SAFETY: see drain_full.
        unsafe {
            while done < budget {
                let Some(header) = self.worklist.pop_front() else {
                    break;
                };
                self.process_one(header, trace_table);
                done += 1;
            }
        }
        done
    }

    /// Process one gray header → traced + painted black.
    ///
    /// # Safety
    ///
    /// `header` must be valid and was placed on the worklist
    /// while gray.
    unsafe fn process_one(&mut self, header: *mut GcHeader, trace_table: &TraceTable) {
        // SAFETY: header is alive (worklist invariant).
        unsafe {
            let h = &*header;
            // Account for live bytes BEFORE we paint black so a
            // re-entry trace can read the size off the header.
            self.live_bytes += h.size_bytes() as usize;
            // Mark the owning page's live-bytes counter so the
            // sweeper knows the page is non-empty.
            let page_header = Page::header_of_mut(header as *const u8);
            page_header.live_bytes += h.size_bytes() as usize;
            // Trace children.
            let mut visitor = |slot: *mut RawGc| {
                let raw = (*slot).0;
                if raw == 0 {
                    return;
                }
                // SAFETY: raw is a valid in-cage offset.
                let child = cage_base().add(raw as usize) as *mut GcHeader;
                let child_h = &*child;
                if matches!(child_h.mark_color(), MarkColor::White) {
                    child_h.set_mark_color(MarkColor::Gray);
                    self.worklist.push_back(child);
                }
            };
            trace_table.trace(header, &mut visitor);
            h.set_mark_color(MarkColor::Black);
        }
    }

    /// Number of gray headers waiting on the worklist.
    pub fn pending(&self) -> usize {
        self.worklist.len()
    }
}
