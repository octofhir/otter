//! Compiler threads' right to read the heap, which collection excludes.
//!
//! # Contents
//! - [`HeapReadRight`] — held by a compiler thread while it reads heap
//!   bodies; every collection pause takes it exclusively.
//!
//! # Invariants
//! - While any reader holds the right, no pause has begun: no cell moves and
//!   no cell is freed, so every handle a reader follows stays valid.
//! - A pause first raises the yield request, then waits for the readers to
//!   leave. Readers poll the request at short intervals and leave promptly,
//!   abandoning what they read; a reader arriving during a pause waits for
//!   it to end.
//! - Pauses nest on the mutator; the right is released when the outermost
//!   ends.
//!
//! # See also
//! - [`crate::GcHeap`] — whose pauses take the right.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};

#[derive(Default)]
struct RightState {
    readers: u32,
    excluded: bool,
}

/// Compiler threads' right to read one heap.
#[derive(Default)]
pub struct HeapReadRight {
    state: Mutex<RightState>,
    changed: Condvar,
    /// Set while a pause waits for, or runs without, the readers.
    yield_requested: AtomicBool,
    /// Pauses in progress on the mutator, nested.
    depth: AtomicU32,
}

impl std::fmt::Debug for HeapReadRight {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HeapReadRight")
            .finish_non_exhaustive()
    }
}

/// A compiler thread holding the right to read the heap.
pub struct HeapReadGuard<'a> {
    right: &'a HeapReadRight,
}

impl Drop for HeapReadGuard<'_> {
    fn drop(&mut self) {
        let mut state = self.right.lock();
        state.readers -= 1;
        if state.readers == 0 {
            self.right.changed.notify_all();
        }
    }
}

impl HeapReadGuard<'_> {
    /// Whether a pause waits for this reader to leave.
    #[must_use]
    pub fn yield_requested(&self) -> bool {
        self.right.yield_requested.load(Ordering::Acquire)
    }
}

/// A collection pause holding the heap exclusively.
pub(crate) struct ExclusionGuard<'a> {
    right: &'a HeapReadRight,
}

impl Drop for ExclusionGuard<'_> {
    fn drop(&mut self) {
        if self.right.depth.fetch_sub(1, Ordering::Relaxed) != 1 {
            return;
        }
        let mut state = self.right.lock();
        state.excluded = false;
        self.right.yield_requested.store(false, Ordering::Release);
        self.right.changed.notify_all();
    }
}

impl HeapReadRight {
    fn lock(&self) -> MutexGuard<'_, RightState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Take the right to read, waiting while a pause runs.
    pub fn read(&self) -> HeapReadGuard<'_> {
        let mut state = self.lock();
        while state.excluded {
            state = self
                .changed
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
        state.readers += 1;
        HeapReadGuard { right: self }
    }

    /// Exclude every reader for one pause: ask them to yield, then wait
    /// until none holds the right.
    pub(crate) fn exclude(&self) -> ExclusionGuard<'_> {
        if self.depth.fetch_add(1, Ordering::Relaxed) == 0 {
            self.yield_requested.store(true, Ordering::Release);
            let mut state = self.lock();
            state.excluded = true;
            while state.readers != 0 {
                state = self
                    .changed
                    .wait(state)
                    .unwrap_or_else(PoisonError::into_inner);
            }
        }
        ExclusionGuard { right: self }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pause_waits_for_the_reader_it_asked_to_yield() {
        let right = std::sync::Arc::new(HeapReadRight::default());
        let reader = right.read();
        let pausing = std::sync::Arc::clone(&right);
        let pause = std::thread::spawn(move || {
            let _exclusive = pausing.exclude();
            !pausing.yield_requested.load(Ordering::Acquire)
        });
        while !reader.yield_requested() {
            std::thread::yield_now();
        }
        drop(reader);
        assert!(
            !pause.join().unwrap(),
            "the request holds through the pause"
        );
        assert!(!right.read().yield_requested(), "released with the pause");
    }
}
