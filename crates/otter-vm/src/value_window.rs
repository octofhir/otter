//! Argument and capture windows the collector rewrites in place.
//!
//! A native call reads its arguments from the caller's frame window and
//! its captures from the callee's capture slab. Both are root slots: a
//! moving collection triggered inside the native rewrites them where they
//! live, and the native's next read must see the forwarded handle.
//!
//! A `&[Value]` parameter cannot carry that contract. The optimizer treats a
//! shared-slice parameter as unchanging for the whole call, so a native that
//! reads `args[0]`, allocates, and reads `args[0]` again may observe the
//! pre-move handle. [`ValueWindow`] holds the window as a raw start pointer
//! and a length: every element access is a fresh load the optimizer cannot
//! hoist across an allocating call.
//!
//! # Contents
//! - [`ValueWindow`] — copyable view over live value slots, dereferencing to
//!   `[Value]` for element access.
//!
//! # Invariants
//! - The window's slots outlive `'a` and stay at a stable address for it.
//! - Slots a collector may rewrite reach a window only through
//!   [`ValueWindow::from_raw_parts`] (frame windows, capture slabs) or a
//!   slice the caller owns; nothing else writes them while the window lives.
//! - A slice borrowed from a window is short-lived: hold the window, not the
//!   slice, across an allocation.
//!
//! # See also
//! - [`crate::native_function::NativeFastFn`] — the native ABI that takes it.
//! - [`crate::runtime_cx::NativeCallRoots`] — traces the windows of one call.

use std::marker::PhantomData;
use std::ops::Deref;
use std::ptr::NonNull;

use crate::Value;

/// Copyable view over `len` live [`Value`] slots starting at `start`.
#[derive(Clone, Copy)]
pub struct ValueWindow<'a> {
    start: NonNull<Value>,
    len: usize,
    _slots: PhantomData<&'a [std::cell::Cell<Value>]>,
}

impl<'a> ValueWindow<'a> {
    /// The empty window.
    pub const EMPTY: ValueWindow<'static> = ValueWindow {
        start: NonNull::dangling(),
        len: 0,
        _slots: PhantomData,
    };

    /// A window over `len` slots at `start`.
    ///
    /// # Safety
    /// `start..start + len` are initialized `Value` slots that stay valid and
    /// in place for `'a`; only the collector writes them meanwhile.
    #[must_use]
    pub unsafe fn from_raw_parts(start: *const Value, len: usize) -> Self {
        Self {
            start: NonNull::new(start.cast_mut()).unwrap_or(NonNull::dangling()),
            len,
            _slots: PhantomData,
        }
    }

    /// A window over values the caller owns.
    #[must_use]
    pub fn from_slice(values: &'a [Value]) -> Self {
        // SAFETY: the borrow keeps the slots alive and in place for `'a`.
        unsafe { Self::from_raw_parts(values.as_ptr(), values.len()) }
    }

    /// The slots from `start` to the end, or the empty window past the end.
    #[must_use]
    pub fn tail(self, start: usize) -> Self {
        let start = start.min(self.len);
        // SAFETY: `start <= len` stays inside the same live window.
        unsafe { Self::from_raw_parts(self.start.as_ptr().add(start), self.len - start) }
    }

    /// The first `len` slots, or the whole window when it is shorter.
    #[must_use]
    pub fn prefix(self, len: usize) -> Self {
        // SAFETY: a shorter length stays inside the same live window.
        unsafe { Self::from_raw_parts(self.start.as_ptr(), len.min(self.len)) }
    }

    /// Yield every slot to a collector visitor, which may rewrite it.
    pub(crate) fn trace_slots(self, visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)) {
        let start = self.start.as_ptr();
        for index in 0..self.len {
            // SAFETY: the slots are live for the window and the collector is
            // the only writer while it traces them.
            unsafe { (*start.add(index)).trace_value_slot_mut(visitor) };
        }
    }
}

impl Deref for ValueWindow<'_> {
    type Target = [Value];

    #[inline]
    fn deref(&self) -> &[Value] {
        // SAFETY: the window's slots are live and initialized for its
        // lifetime; the slice is re-derived on every access.
        unsafe { std::slice::from_raw_parts(self.start.as_ptr(), self.len) }
    }
}

impl<'a> From<&'a [Value]> for ValueWindow<'a> {
    fn from(values: &'a [Value]) -> Self {
        Self::from_slice(values)
    }
}

impl<'a, const N: usize> From<&'a [Value; N]> for ValueWindow<'a> {
    fn from(values: &'a [Value; N]) -> Self {
        Self::from_slice(values)
    }
}

impl<'a> From<&'a Vec<Value>> for ValueWindow<'a> {
    fn from(values: &'a Vec<Value>) -> Self {
        Self::from_slice(values)
    }
}

impl<'a, A: smallvec::Array<Item = Value>> From<&'a smallvec::SmallVec<A>> for ValueWindow<'a> {
    fn from(values: &'a smallvec::SmallVec<A>) -> Self {
        Self::from_slice(values)
    }
}

impl<'a> IntoIterator for ValueWindow<'a> {
    type Item = &'a Value;
    type IntoIter = std::slice::Iter<'a, Value>;

    fn into_iter(self) -> Self::IntoIter {
        // SAFETY: the window's slots are live for `'a`.
        unsafe { std::slice::from_raw_parts::<'a, Value>(self.start.as_ptr(), self.len) }.iter()
    }
}

impl std::fmt::Debug for ValueWindow<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}
