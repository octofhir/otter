//! Rooting handles: `Local<'gc, T>`, `HandleScope<'gc>`,
//! `EscapableHandleScope<'gc>`, and the internal persistent-handle
//! table behind branded `Root<'iso, T>`.
//!
//! Phase-1 GC is moving (the scavenger relocates young objects).
//! Native code that holds a `Gc<T>` across a safepoint must root
//! it through a handle so the GC can rewrite the slot when the
//! object moves. V8 / Oilpan use the same pattern.
//!
//! # Contents
//!
//! - [`HandleScope`] — RAII scope that bounds the lifetime of
//!   every [`Local`] created inside it.
//! - [`EscapableHandleScope`] — nested handle scope with one explicit
//!   escape slot for returning a local to its parent scope.
//! - [`Local`] — typed root; backed by an entry on the
//!   [`HandleStack`].
//! - `GlobalHandle` — crate-internal explicit-drop root used by
//!   [`crate::Root`] for long-lived pointers held outside any scope.
//! - [`HandleStack`] — owned by [`crate::heap::GcHeap`]; walked
//!   by the scavenger to fix up moved pointers.
//!
//! # Invariants
//!
//! - Every [`Local`] borrows from its [`HandleScope`] at the
//!   type level; the borrow checker proves it cannot outlive
//!   its scope.
//! - On `HandleScope::drop` the stack truncates to the saved
//!   index — entries from this scope are reclaimed.
//! - The handle stack is walked as `*mut RawGc` slot pointers,
//!   so the scavenger updates entries in-place when objects
//!   move.
//!
//! # See also
//!
//! - GC architecture plan §4.4 ("Pointer-stored roots survive
//!   moves").

use std::cell::UnsafeCell;
use std::marker::PhantomData;

use crate::compressed::{Gc, RawGc};

/// Handle stack — owned by [`crate::heap::GcHeap`]. Each entry
/// is a `RawGc`; the scavenger walks every entry as a `*mut
/// RawGc` slot pointer.
pub struct HandleStack {
    entries: UnsafeCell<Vec<RawGc>>,
    /// One past the last live entry; matches `entries.len()` and
    /// is bumped/truncated by [`HandleScope`].
    top: UnsafeCell<u32>,
    /// Sticky-flag — true while a [`HandleScope`] is open. Used
    /// in debug assertions; production code path is
    /// `enter_scope` returning `saved_top`.
    open_scopes: UnsafeCell<u32>,
}

impl Default for HandleStack {
    fn default() -> Self {
        Self::new()
    }
}

impl HandleStack {
    /// Empty stack.
    pub fn new() -> Self {
        Self {
            entries: UnsafeCell::new(Vec::with_capacity(64)),
            top: UnsafeCell::new(0),
            open_scopes: UnsafeCell::new(0),
        }
    }

    /// Push a new entry; returns its index.
    fn push(&self, raw: RawGc) -> u32 {
        // SAFETY: GcHeap is single-threaded; UnsafeCell access
        // is uniquely owned at the call site.
        unsafe {
            let entries = &mut *self.entries.get();
            let top = &mut *self.top.get();
            let idx = *top;
            // Resize if needed: idx == len => push, idx < len =>
            // overwrite (slot was vacated by a parent scope's
            // truncate).
            if (idx as usize) < entries.len() {
                entries[idx as usize] = raw;
            } else {
                entries.push(raw);
            }
            *top += 1;
            idx
        }
    }

    /// Overwrite an existing live entry.
    fn write(&self, idx: u32, raw: RawGc) {
        // SAFETY: GcHeap is single-threaded; callers only pass
        // indices allocated by `push` and still live in the current
        // scope chain.
        unsafe {
            let entries = &mut *self.entries.get();
            debug_assert!((idx as usize) < entries.len());
            entries[idx as usize] = raw;
        }
    }

    /// Truncate the stack to `new_top` entries.
    fn truncate(&self, new_top: u32) {
        // SAFETY: see [`Self::push`].
        unsafe {
            let top = &mut *self.top.get();
            debug_assert!(new_top <= *top);
            *top = new_top;
        }
    }

    /// Read the current top.
    fn top(&self) -> u32 {
        // SAFETY: see [`Self::push`].
        unsafe { *self.top.get() }
    }

    /// Visit every live entry as a `*mut RawGc`. Called by the
    /// scavenger.
    ///
    /// # Safety
    ///
    /// The visitor must run under STW pause; mutating the stack
    /// concurrently is UB.
    pub unsafe fn visit_slots(&self, visitor: &mut dyn FnMut(*mut RawGc)) {
        // SAFETY: STW pause.
        unsafe {
            let entries = &mut *self.entries.get();
            let top = *self.top.get();
            for i in 0..top as usize {
                visitor(entries.as_mut_ptr().add(i));
            }
        }
    }

    /// Number of live entries.
    pub fn len(&self) -> usize {
        self.top() as usize
    }

    /// `true` if no live entries.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// RAII scope that bounds the lifetime of every [`Local`]
/// created inside it. Drop truncates the handle stack.
///
/// The scope holds the handle stack via a raw pointer, not via
/// a borrow, so opening a scope does not lock the heap into an
/// immutable borrow — the user can call mutating heap methods
/// while the scope is open. Soundness is upheld by the `'gc`
/// lifetime parameter: callers cannot construct a scope that
/// outlives its stack.
pub struct HandleScope<'gc> {
    stack: *const HandleStack,
    saved_top: u32,
    _life: PhantomData<&'gc HandleStack>,
}

impl<'gc> HandleScope<'gc> {
    /// Open a new scope on `stack`, borrowing `stack` only for
    /// the duration of this constructor call. The 'gc lifetime
    /// of the returned scope is bound to the input borrow — use
    /// [`HandleScope::from_ptr`] when the caller needs to
    /// decouple the scope's lifetime from a `&heap` borrow.
    pub fn new(stack: &'gc HandleStack) -> Self {
        // SAFETY: from_ptr's safety contract is upheld: stack
        // is a live borrow whose lifetime is 'gc.
        unsafe { Self::from_ptr(stack as *const HandleStack) }
    }

    /// Open a new scope from a raw pointer. The `'gc` lifetime
    /// is chosen by the caller and is *not* tied to any heap
    /// borrow.
    ///
    /// # Safety
    ///
    /// `stack` must outlive every `Local<'gc, T>` produced from
    /// this scope. In practice the caller takes the pointer
    /// from `GcHeap::handle_stack_ptr()` and keeps the heap
    /// alive for the duration of the scope.
    pub unsafe fn from_ptr(stack: *const HandleStack) -> Self {
        // SAFETY: caller's contract upholds stack liveness.
        unsafe {
            let scopes = &mut *(*stack).open_scopes.get();
            *scopes += 1;
            let saved_top = (*stack).top();
            Self {
                stack,
                saved_top,
                _life: PhantomData,
            }
        }
    }

    #[inline]
    fn stack(&self) -> &HandleStack {
        // SAFETY: stack outlives 'gc by the lifetime contract.
        unsafe { &*self.stack }
    }

    /// Root a `Gc<T>` inside this scope and return a [`Local`].
    pub fn local<T: ?Sized>(&self, gc: Gc<T>) -> Local<'gc, T> {
        let idx = self.stack().push(gc.raw());
        Local {
            idx,
            stack: self.stack,
            _t: PhantomData,
        }
    }
}

impl Drop for HandleScope<'_> {
    fn drop(&mut self) {
        // SAFETY: stack outlives 'gc by the lifetime contract.
        unsafe {
            (*self.stack).truncate(self.saved_top);
            let scopes = &mut *(*self.stack).open_scopes.get();
            *scopes = scopes.saturating_sub(1);
        }
    }
}

/// Nested handle scope that can return exactly one [`Local`] to the
/// parent scope.
///
/// The escape slot is reserved before the nested scope starts, so
/// dropping the nested scope truncates only its temporaries and keeps
/// the escaped handle live in the parent scope. This mirrors the V8
/// `EscapableHandleScope` shape without exposing raw handle-table
/// entries to native authors.
///
/// # Example
///
/// ```
/// use otter_gc::test_support::OpaqueLeaf;
/// use otter_gc::{EscapableHandleScope, GcHeap};
///
/// let mut heap = GcHeap::new().unwrap();
/// heap.register_traceable::<OpaqueLeaf>();
/// let gc = heap.alloc(OpaqueLeaf { payload: 94 }).unwrap();
/// let stack = heap.handle_stack();
///
/// let escaped = {
///     let mut inner = EscapableHandleScope::new(stack);
///     let local = inner.local(gc);
///     inner.escape(&local)
/// };
///
/// assert_eq!(escaped.get().offset(), gc.offset());
/// ```
pub struct EscapableHandleScope<'gc> {
    scope: HandleScope<'gc>,
    escape_idx: u32,
    escaped: bool,
}

impl<'gc> EscapableHandleScope<'gc> {
    /// Open an escapable scope on `stack`.
    ///
    /// The returned scope reserves one parent-visible slot. Use
    /// [`Self::local`] for temporaries and [`Self::escape`] for the
    /// single value that must survive the nested scope.
    pub fn new(stack: &'gc HandleStack) -> Self {
        let escape_idx = stack.push(RawGc::NULL);
        Self {
            scope: HandleScope::new(stack),
            escape_idx,
            escaped: false,
        }
    }

    /// Root a temporary value inside the nested scope.
    pub fn local<T: ?Sized>(&self, gc: Gc<T>) -> Local<'gc, T> {
        self.scope.local(gc)
    }

    /// Escape `local` into the parent scope.
    ///
    /// # Panics
    ///
    /// Panics in debug and release builds if called more than once.
    /// A second escape would make ownership of the reserved slot
    /// ambiguous; callers that need multiple values should escape a
    /// container object.
    pub fn escape<T: ?Sized>(&mut self, local: &Local<'gc, T>) -> Local<'gc, T> {
        assert!(!self.escaped, "EscapableHandleScope::escape called twice");
        self.escaped = true;
        self.scope.stack().write(self.escape_idx, local.raw());
        Local {
            idx: self.escape_idx,
            stack: self.scope.stack,
            _t: PhantomData,
        }
    }
}

/// Typed local handle. Lifetime-bound to its [`HandleScope`].
pub struct Local<'gc, T: ?Sized> {
    idx: u32,
    stack: *const HandleStack,
    _t: PhantomData<&'gc T>,
}

impl<T: ?Sized> Local<'_, T> {
    #[inline]
    fn stack(&self) -> &HandleStack {
        // SAFETY: stack outlives 'gc by the lifetime contract.
        unsafe { &*self.stack }
    }

    /// Read the current `Gc<T>` value. The slot may have been
    /// rewritten by the scavenger since the handle was created.
    pub fn get(&self) -> Gc<T> {
        // SAFETY: STW invariant + index in-range by
        // construction.
        unsafe {
            let entries: &Vec<RawGc> = &*self.stack().entries.get();
            Gc::from_offset(entries[self.idx as usize].0)
        }
    }

    /// Compressed backend form of the current value.
    ///
    /// Normal contributor code should use [`Self::get`]. Raw access is
    /// retained only for collector/root adapter code.
    #[doc(hidden)]
    pub fn raw(&self) -> RawGc {
        // SAFETY: see [`Local::get`].
        unsafe {
            let entries: &Vec<RawGc> = &*self.stack().entries.get();
            entries[self.idx as usize]
        }
    }
}

impl<T: ?Sized> Clone for Local<'_, T> {
    fn clone(&self) -> Self {
        // Locals are RAII, but reading the current value and
        // re-rooting it inside the same scope is cheap and
        // sound.
        let raw = self.raw();
        let idx = self.stack().push(raw);
        Local {
            idx,
            stack: self.stack,
            _t: PhantomData,
        }
    }
}

impl<T: ?Sized> std::fmt::Debug for Local<'_, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Local")
            .field("idx", &self.idx)
            .field("offset", &format_args!("0x{:x}", self.raw().0))
            .finish()
    }
}

/// Long-lived root unbound from any [`HandleScope`]. Drop must
/// fire to release the entry; otherwise the heap leaks.
pub(crate) struct GlobalHandle<T: ?Sized> {
    /// Index in the global handle table.
    idx: u32,
    /// Owning heap's global handle table is reached through a
    /// raw pointer set when the handle is created. The handle
    /// is sound only while the heap lives — same contract V8
    /// imposes on `v8::Persistent`.
    table: *const GlobalHandleTable,
    _t: PhantomData<*const T>,
}

impl<T: ?Sized> GlobalHandle<T> {
    /// Read the current `Gc<T>` value.
    pub fn get(&self) -> Gc<T> {
        // SAFETY: caller holds the heap alive (V8 Persistent
        // contract) and the index is in-range by construction.
        unsafe {
            let table = &*self.table;
            Gc::from_offset(table.read(self.idx).0)
        }
    }
}

impl<T: ?Sized> Drop for GlobalHandle<T> {
    fn drop(&mut self) {
        // SAFETY: same as [`Self::get`].
        unsafe {
            let table = &*self.table;
            table.release(self.idx);
        }
    }
}

/// Backing table for [`GlobalHandle`]. A free-list reclaims
/// indices to keep the table dense.
pub(crate) struct GlobalHandleTable {
    entries: UnsafeCell<Vec<RawGc>>,
    free: UnsafeCell<Vec<u32>>,
}

impl Default for GlobalHandleTable {
    fn default() -> Self {
        Self::new()
    }
}

impl GlobalHandleTable {
    /// Empty table.
    pub fn new() -> Self {
        Self {
            entries: UnsafeCell::new(Vec::new()),
            free: UnsafeCell::new(Vec::new()),
        }
    }

    /// Allocate a slot for `raw` and return its index.
    fn allocate(&self, raw: RawGc) -> u32 {
        // SAFETY: GcHeap is single-threaded.
        unsafe {
            let entries = &mut *self.entries.get();
            let free = &mut *self.free.get();
            if let Some(idx) = free.pop() {
                entries[idx as usize] = raw;
                idx
            } else {
                let idx = entries.len() as u32;
                entries.push(raw);
                idx
            }
        }
    }

    /// Read a slot.
    ///
    /// # Safety
    ///
    /// `idx` must point at a live (non-released) entry.
    unsafe fn read(&self, idx: u32) -> RawGc {
        // SAFETY: caller-side invariant.
        unsafe {
            let entries: &Vec<RawGc> = &*self.entries.get();
            entries[idx as usize]
        }
    }

    /// Release a slot back to the free list.
    ///
    /// # Safety
    ///
    /// `idx` must point at a live entry; the caller's
    /// [`GlobalHandle`] must be the unique owner.
    unsafe fn release(&self, idx: u32) {
        // SAFETY: see above.
        unsafe {
            let free: &mut Vec<u32> = &mut *self.free.get();
            free.push(idx);
            let entries: &mut Vec<RawGc> = &mut *self.entries.get();
            entries[idx as usize] = RawGc::NULL;
        }
    }

    /// Visit every live slot as a `*mut RawGc`. Called by the
    /// scavenger.
    ///
    /// # Safety
    ///
    /// STW pause.
    pub unsafe fn visit_slots(&self, visitor: &mut dyn FnMut(*mut RawGc)) {
        // SAFETY: STW invariant.
        unsafe {
            let entries = &mut *self.entries.get();
            let len = entries.len();
            for i in 0..len {
                if entries[i].is_null() {
                    continue; // free slot
                }
                visitor(entries.as_mut_ptr().add(i));
            }
        }
    }

    /// Allocate a [`GlobalHandle`] from a `Gc<T>`.
    pub(crate) fn create<T: ?Sized>(&self, gc: Gc<T>) -> GlobalHandle<T> {
        let idx = self.allocate(gc.raw());
        GlobalHandle {
            idx,
            table: self as *const _,
            _t: PhantomData,
        }
    }
}
