//! `Local<T>` and `HandleScope<'gc>` — V8-style typed root handles.
//!
//! `GcRef<T>` (see `gc_ref.rs`) is the raw, lifetime-free GC pointer that
//! every VM call uses. It is `Copy`, 8 bytes, and has no rooting
//! discipline of its own — the holder is responsible for ensuring the
//! referenced object stays reachable across allocation safepoints.
//!
//! `Local<'gc, T>` is the ergonomic API on top: a typed handle whose
//! lifetime `'gc` is tied to the enclosing [`HandleScope`]. The handle
//! is rooted on the GC's [`HandleStack`](crate::handle::HandleStack)
//! at creation and dropped (along with every other Local in its scope)
//! when the scope exits. Holders use `Local<'gc, T>` whenever they need
//! to hold a GC reference across a call that might allocate.
//!
//! ```ignore
//! // pseudo-code
//! let mut scope = HandleScope::new(&mut heap);
//! let s: Local<JsStringGc> = scope.alloc_typed(STRING_TAG, payload)?; // rooted
//! let s2: Local<JsStringGc> = scope.alloc_typed(STRING_TAG, payload2)?;
//! do_work(s, s2); // both stay alive even if `do_work` allocates
//! drop(scope);    // both `s` and `s2` are no longer roots
//! ```
//!
//! # Why `'gc`?
//!
//! The lifetime parameter is purely a borrow-checker proof: Rust verifies
//! at compile time that no `Local` escapes its enclosing scope. The
//! lifetime is unrelated to the underlying object's lifetime — the GC
//! still owns reclamation. We just stop the user from accidentally using
//! a `Local` from an outer call after its scope dropped.
//!
//! # Cost
//!
//! `Local<'gc, T>` is `Copy`, 8 bytes (a `GcRef<T>`). Creating one is
//! one push to the handle stack (single-threaded, no atomics). Dropping
//! is bulk: the entire scope truncates the handle stack in one shot at
//! exit.

use std::marker::PhantomData;

use crate::gc_ref::GcRef;
use crate::handle::HandleScopeLevel;
use crate::header::GcHeader;
use crate::heap::GcHeap;
use crate::typed::OutOfMemory;

/// A typed, scope-rooted reference to a GC object.
///
/// `Local<'gc, T>` is a [`GcRef<T>`] that is also rooted on the
/// [`HandleStack`](crate::handle::HandleStack) for the duration of the
/// enclosing [`HandleScope<'gc>`]. The lifetime `'gc` ties the handle
/// to its scope so the borrow checker prevents use-after-scope-exit.
///
/// `Local` is `Copy`: cloning is free, and the same root is shared.
pub struct Local<'gc, T> {
    inner: GcRef<T>,
    _scope: PhantomData<&'gc ()>,
}

impl<T> Copy for Local<'_, T> {}

impl<T> Clone for Local<'_, T> {
    #[inline]
    fn clone(&self) -> Self {
        *self
    }
}

impl<'gc, T> Local<'gc, T> {
    /// Constructs a `Local` from an already-rooted `GcRef<T>`.
    ///
    /// # Safety
    ///
    /// The caller must guarantee that `inner` is currently held in the
    /// [`HandleStack`](crate::handle::HandleStack) entry that the
    /// enclosing scope established. In practice this is only called by
    /// [`HandleScope::root`] / [`HandleScope::alloc_typed`].
    #[inline]
    pub const unsafe fn from_rooted_ref(inner: GcRef<T>) -> Self {
        Self {
            inner,
            _scope: PhantomData,
        }
    }

    /// Returns the underlying `GcRef<T>`.
    #[inline]
    pub fn as_ref(&self) -> GcRef<T> {
        self.inner
    }

    /// Returns the typed payload.
    #[inline]
    pub fn payload(&self) -> &T {
        self.inner.payload()
    }

    /// Returns the GC header.
    #[inline]
    pub fn header(&self) -> &GcHeader {
        self.inner.header()
    }
}

impl<T> std::fmt::Debug for Local<'_, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Local").field(&self.inner).finish()
    }
}

impl<T> PartialEq for Local<'_, T> {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.inner == other.inner
    }
}
impl<T> Eq for Local<'_, T> {}

// ---------------------------------------------------------------------------
// HandleScope
// ---------------------------------------------------------------------------

/// V8-style RAII handle scope. Holds an exclusive borrow on a
/// [`GcHeap`] for the lifetime `'gc`; every [`Local<'gc, T>`] created
/// via the scope stays rooted on the heap's handle stack until the
/// scope drops. On drop the saved level is restored — every Local
/// from this scope (and any nested scopes that did not finish before
/// drop) is unrooted in one shot.
///
/// All heap operations during a scope go through the scope itself —
/// either via [`HandleScope::heap`] / [`HandleScope::heap_mut`] (raw
/// access) or via the convenience methods `alloc_typed`,
/// `alloc_typed_var`, `root`. The compiler guarantees no other
/// `&mut GcHeap` borrow exists for `'gc`.
///
/// Scopes nest: just call [`HandleScope::nested`] from inside an
/// outer scope to obtain a borrowed inner scope.
pub struct HandleScope<'gc> {
    heap: &'gc mut GcHeap,
    saved_level: HandleScopeLevel,
}

impl<'gc> HandleScope<'gc> {
    /// Enters a fresh scope on the given heap.
    pub fn new(heap: &'gc mut GcHeap) -> Self {
        let saved_level = heap.enter_scope();
        Self { heap, saved_level }
    }

    /// Returns the saved level — useful for callers that bridge into
    /// the legacy `GcHeap::exit_scope` API.
    #[inline]
    pub fn level(&self) -> HandleScopeLevel {
        self.saved_level
    }

    /// Borrowed access to the underlying heap.
    #[inline]
    pub fn heap(&self) -> &GcHeap {
        self.heap
    }

    /// Mutable access to the underlying heap. Used when the caller
    /// needs to call heap methods that this scope does not wrap.
    #[inline]
    pub fn heap_mut(&mut self) -> &mut GcHeap {
        self.heap
    }

    /// Roots an existing `GcRef<T>` on the handle stack. Returns a
    /// typed `Local<'gc, T>` for use within this scope.
    pub fn root<T>(&mut self, gc_ref: GcRef<T>) -> Local<'gc, T> {
        self.heap.root(gc_ref.as_ptr().as_ptr() as *const GcHeader);
        // SAFETY: `heap.root` pushed `gc_ref`'s header onto the
        // handle stack at a level above `self.saved_level`. The
        // returned `Local`'s `'gc` lifetime is bounded by this scope,
        // so the borrow checker proves the root is alive while the
        // Local is reachable.
        unsafe { Local::from_rooted_ref(gc_ref) }
    }

    /// Allocates a fresh young-gen object of type `T`, writes the
    /// header, moves `value` into the payload, and returns a rooted
    /// `Local<'gc, T>`.
    ///
    /// Returns `Err(OutOfMemory)` if the heap cap is exceeded.
    pub fn alloc_typed<T>(
        &mut self,
        type_tag: u8,
        value: T,
    ) -> Result<Local<'gc, T>, OutOfMemory> {
        let gc_ref = self
            .heap
            .alloc_typed(type_tag, value)
            .ok_or(OutOfMemory)?;
        Ok(self.root(gc_ref))
    }

    /// Variable-payload-size variant of [`alloc_typed`].
    ///
    /// Reserves `header + payload_bytes` and lets `init` populate the
    /// payload area, then roots the resulting `GcRef<T>`.
    ///
    /// # Safety
    ///
    /// `init` must fully initialise the payload area, treating the
    /// pointer as the start of `T` followed by `payload_bytes -
    /// size_of::<T>()` trailing bytes that `T`'s trace function knows
    /// how to read.
    pub unsafe fn alloc_typed_var<T, F>(
        &mut self,
        type_tag: u8,
        payload_bytes: usize,
        init: F,
    ) -> Result<Local<'gc, T>, OutOfMemory>
    where
        F: FnOnce(*mut u8),
    {
        // SAFETY: forwarded to `GcHeap::alloc_typed_var`'s contract.
        let gc_ref = unsafe { self.heap.alloc_typed_var::<T, F>(type_tag, payload_bytes, init) }
            .ok_or(OutOfMemory)?;
        Ok(self.root(gc_ref))
    }

    /// Enters a nested scope using the same heap.
    ///
    /// The borrow checker prevents the caller from touching `self`
    /// while the inner scope is alive — exactly the V8 model.
    pub fn nested<'inner>(&'inner mut self) -> HandleScope<'inner>
    where
        'gc: 'inner,
    {
        HandleScope::new(self.heap)
    }
}

impl<'gc> Drop for HandleScope<'gc> {
    fn drop(&mut self) {
        self.heap.exit_scope(self.saved_level);
    }
}

// HandleScope is intentionally non-`Send` — moving an active scope
// across threads would let the borrow checker's exclusivity proof
// leak without resealing.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gc_ref::type_tag;
    use crate::heap::{GcConfig, GcHeap};

    #[repr(C)]
    #[derive(Copy, Clone, Debug)]
    struct StringPayload {
        len: u32,
        flags: u32,
    }

    fn fresh_heap() -> GcHeap {
        GcHeap::new(GcConfig {
            young_gen_size: 1024 * 1024,
            old_gen_threshold: 512 * 1024,
            ..GcConfig::default()
        })
    }

    #[test]
    fn scope_alloc_roots_local_and_drops_on_exit() {
        let mut heap = fresh_heap();
        {
            let mut scope = HandleScope::new(&mut heap);
            let l = scope
                .alloc_typed(type_tag::STRING, StringPayload { len: 4, flags: 0 })
                .expect("alloc fits");
            assert_eq!(l.payload().len, 4);
            assert_eq!(l.header().type_tag(), type_tag::STRING);
            // While the scope is alive, the local stays rooted.
        }
        // After the scope drops, the handle stack is back at level 0.
        // We cannot inspect it directly here (HandleStack is private to
        // GcHeap), but the next scope should observe an empty stack.
        let scope = HandleScope::new(&mut heap);
        assert_eq!(scope.level().level(), 0);
    }

    #[test]
    fn nested_scope_only_releases_inner_handles() {
        let mut heap = fresh_heap();
        let mut outer = HandleScope::new(&mut heap);
        let outer_local = outer
            .alloc_typed(type_tag::STRING, StringPayload { len: 1, flags: 0 })
            .expect("outer alloc");

        {
            let mut inner = outer.nested();
            let inner_local = inner
                .alloc_typed(type_tag::STRING, StringPayload { len: 2, flags: 0 })
                .expect("inner alloc");
            assert_eq!(inner_local.payload().len, 2);
            // inner drops here — its handle is released.
        }

        // Outer's handle is still alive and usable.
        assert_eq!(outer_local.payload().len, 1);
    }

    #[test]
    fn alloc_typed_var_initialises_trailing_payload() {
        let mut heap = fresh_heap();
        let mut scope = HandleScope::new(&mut heap);

        let trailing = 16usize;
        let total = std::mem::size_of::<StringPayload>() + trailing;

        let l = unsafe {
            scope
                .alloc_typed_var::<StringPayload, _>(type_tag::STRING, total, |raw| {
                    let head = raw as *mut StringPayload;
                    head.write(StringPayload { len: 99, flags: 0xCC });
                    let tail = raw.add(std::mem::size_of::<StringPayload>());
                    std::ptr::write_bytes(tail, 0xEF, trailing);
                })
                .expect("var alloc")
        };

        assert_eq!(l.payload().len, 99);
        assert_eq!(l.payload().flags, 0xCC);
    }

    #[test]
    fn local_is_eight_bytes_and_copy() {
        // `Local<'gc, T>` is a `GcRef<T>` plus zero-sized phantom →
        // it must remain pointer-sized.
        assert_eq!(std::mem::size_of::<Local<'static, u32>>(), 8);
    }

    #[test]
    fn local_eq_uses_pointer_identity() {
        let mut heap = fresh_heap();
        let mut scope = HandleScope::new(&mut heap);
        let l1 = scope
            .alloc_typed(type_tag::STRING, StringPayload { len: 0, flags: 0 })
            .expect("a");
        let l2 = scope
            .alloc_typed(type_tag::STRING, StringPayload { len: 0, flags: 0 })
            .expect("b");
        assert_ne!(l1, l2); // distinct allocations → distinct pointers
    }

    #[test]
    fn scope_oom_returns_err_when_cap_exceeded() {
        // Cap = 8 < HEADER_SIZE(8) + StringPayload(8) → fail.
        let mut heap = GcHeap::with_max_heap_bytes(8);
        let mut scope = HandleScope::new(&mut heap);
        let res = scope.alloc_typed(type_tag::STRING, StringPayload { len: 0, flags: 0 });
        assert!(matches!(res, Err(OutOfMemory)));
    }
}
