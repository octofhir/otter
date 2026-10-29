//! Isolate state the mutator owns and compiler threads read.
//!
//! # Contents
//! - [`MutatorToken`] — the mutator's proof of ownership, held by the
//!   interpreter: it moves with the interpreter and is never shared.
//! - [`MutatorOwned`] — one value the mutator reads freely, changes under a
//!   short lock, and a compiler thread reads under that lock when it is free.
//! - [`MutatorCell`] — the same protocol for a value the mutator changes
//!   through shared access (a collector pass, an `&self` index), with the
//!   mutator's borrows checked at run time.
//!
//! # Invariants
//! - Only the mutator writes, and only while it holds the lock and the token
//!   mutably, so no mutator read is live across its own write.
//! - A mutator read takes no lock: the only other access a write excludes is
//!   a compiler thread's read, which holds the lock for its whole duration.
//! - A compiler thread never blocks: [`MutatorOwned::try_read`] fails while
//!   the mutator writes. [`MutatorOwned::read_briefly`] retries over a
//!   bounded number of scheduler yields, as long as a mutator write lasts,
//!   and a compilation that still fails is retried whole, never published
//!   without the fact. Compiler threads share the lock, so they never fail
//!   for each other.
//!
//! # See also
//! - `interp::jit_bake` — the compiler-side reader.

use std::cell::UnsafeCell;

/// Scheduler yields a compiler-thread read waits through a mutator write.
const READ_YIELDS: u32 = 64;

/// `read` retried through [`READ_YIELDS`] yields while it fails.
pub(crate) fn retry_briefly<R>(mut read: impl FnMut() -> Option<R>) -> Option<R> {
    for _ in 0..READ_YIELDS {
        if let Some(value) = read() {
            return Some(value);
        }
        std::thread::yield_now();
    }
    read()
}
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard, TryLockError};

/// The isolate thread's ownership of every [`MutatorOwned`] value of its
/// isolate. Reads borrow it shared, writes mutably, so the borrow checker
/// keeps a mutator read from outliving the next write.
#[derive(Debug)]
pub(crate) struct MutatorToken {
    /// Not `Sync`: one thread at a time holds the mutator's role.
    _unshared: std::marker::PhantomData<std::cell::Cell<()>>,
}

impl MutatorToken {
    /// The token of a new isolate.
    pub(crate) fn new() -> Self {
        Self {
            _unshared: std::marker::PhantomData,
        }
    }
}

/// A value the mutator owns and compiler threads may read.
pub(crate) struct MutatorOwned<T> {
    lock: RwLock<()>,
    value: UnsafeCell<T>,
}

// SAFETY: the value is reached from another thread only through `try_read`,
// which holds the lock that every write also holds; `T: Sync` lets those
// reads share the value with the mutator's unlocked reads.
unsafe impl<T: Send + Sync> Sync for MutatorOwned<T> {}

impl<T: Default> Default for MutatorOwned<T> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

impl<T: std::fmt::Debug> std::fmt::Debug for MutatorOwned<T> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MutatorOwned")
            .finish_non_exhaustive()
    }
}

impl<T> MutatorOwned<T> {
    pub(crate) fn new(value: T) -> Self {
        Self {
            lock: RwLock::new(()),
            value: UnsafeCell::new(value),
        }
    }

    /// The mutator's read.
    #[inline]
    pub(crate) fn get<'a>(&'a self, _owner: &'a MutatorToken) -> &'a T {
        // SAFETY: only the token's holder writes, and only through
        // `get_mut`, which needs the token mutably: no write is live.
        unsafe { &*self.value.get() }
    }

    /// The mutator's write, excluding compiler-thread reads while it lasts.
    #[inline]
    pub(crate) fn get_mut<'a>(&'a self, _owner: &'a mut MutatorToken) -> MutatorWrite<'a, T> {
        let guard = self
            .lock
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        MutatorWrite {
            _guard: guard,
            // SAFETY: the token is held mutably (no mutator read is live)
            // and the lock excludes every compiler-thread read.
            value: unsafe { &mut *self.value.get() },
        }
    }

    /// A compiler thread's read once a mutator write ends, waiting through
    /// a bounded number of yields; `None` when the write outlasts them.
    pub(crate) fn read_briefly(&self) -> Option<SharedRead<'_, T>> {
        retry_briefly(|| self.try_read())
    }

    /// A compiler thread's read, while the mutator is not writing; `None`
    /// otherwise.
    pub(crate) fn try_read(&self) -> Option<SharedRead<'_, T>> {
        let guard = match self.lock.try_read() {
            Ok(guard) => guard,
            Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(TryLockError::WouldBlock) => return None,
        };
        Some(SharedRead {
            _guard: guard,
            // SAFETY: the lock excludes every write for the guard's life.
            value: unsafe { &*self.value.get() },
        })
    }
}

/// The mutator writing a [`MutatorOwned`] value.
pub(crate) struct MutatorWrite<'a, T> {
    _guard: RwLockWriteGuard<'a, ()>,
    value: &'a mut T,
}

impl<T> std::ops::Deref for MutatorWrite<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        self.value
    }
}

impl<T> std::ops::DerefMut for MutatorWrite<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        self.value
    }
}

/// A compiler thread reading a [`MutatorOwned`] value.
pub(crate) struct SharedRead<'a, T> {
    _guard: RwLockReadGuard<'a, ()>,
    value: &'a T,
}

impl<T> std::ops::Deref for SharedRead<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        self.value
    }
}

/// A value the mutator changes through shared access and a compiler thread
/// may read. The mutator's borrows are checked like a `RefCell`'s; its writes
/// also hold the lock a compiler-thread read holds.
pub(crate) struct MutatorCell<T> {
    lock: RwLock<()>,
    cell: std::cell::RefCell<T>,
}

impl<T: Default> Default for MutatorCell<T> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

impl<T: std::fmt::Debug> std::fmt::Debug for MutatorCell<T> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MutatorCell")
            .finish_non_exhaustive()
    }
}

impl<T> MutatorCell<T> {
    pub(crate) fn new(value: T) -> Self {
        Self {
            lock: RwLock::new(()),
            cell: std::cell::RefCell::new(value),
        }
    }

    /// The mutator's read.
    #[inline]
    pub(crate) fn borrow(&self) -> std::cell::Ref<'_, T> {
        self.cell.borrow()
    }

    /// The mutator's write, excluding compiler-thread reads while it lasts.
    #[inline]
    pub(crate) fn borrow_mut(&self) -> MutatorCellWrite<'_, T> {
        let guard = self
            .lock
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        MutatorCellWrite {
            _guard: guard,
            value: self.cell.borrow_mut(),
        }
    }

    /// A compiler thread's read once a mutator write ends, waiting through
    /// a bounded number of yields; `None` when the write outlasts them.
    pub(crate) fn read_briefly(&self) -> Option<SharedRead<'_, T>> {
        retry_briefly(|| self.try_read())
    }

    /// A compiler thread's read, while the mutator is not writing; `None`
    /// otherwise.
    pub(crate) fn try_read(&self) -> Option<SharedRead<'_, T>> {
        let guard = match self.lock.try_read() {
            Ok(guard) => guard,
            Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(TryLockError::WouldBlock) => return None,
        };
        Some(SharedRead {
            _guard: guard,
            // SAFETY: every mutable borrow holds the lock, so none is live;
            // the mutator's shared borrows only read. The borrow flag is
            // left to the mutator.
            value: unsafe { &*self.cell.as_ptr() },
        })
    }
}

/// The mutator writing a [`MutatorCell`] value.
pub(crate) struct MutatorCellWrite<'a, T> {
    _guard: RwLockWriteGuard<'a, ()>,
    value: std::cell::RefMut<'a, T>,
}

impl<T> std::ops::Deref for MutatorCellWrite<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.value
    }
}

impl<T> std::ops::DerefMut for MutatorCellWrite<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_compiler_read_fails_while_the_mutator_writes_and_succeeds_after() {
        let mut token = MutatorToken::new();
        let owned = MutatorOwned::new(vec![1]);
        {
            let mut write = owned.get_mut(&mut token);
            write.push(2);
            assert!(owned.try_read().is_none());
        }
        assert_eq!(*owned.try_read().expect("free lock"), vec![1, 2]);
        assert_eq!(owned.get(&token).len(), 2);
    }
}
