//! `GcRef<T>` — V8-style raw GC reference.
//!
//! `GcRef<T>` is the production-grade replacement for the legacy
//! [`crate::typed::Handle`] (a `u32` slot index). Instead of indexing a
//! side-table, `GcRef<T>` holds a `NonNull<GcHeader>` pointing directly
//! at an in-page allocation. Access to the payload is one indirection,
//! the same as a Rust reference.
//!
//! # Layout contract
//!
//! Every allocation managed by [`GcHeap`](crate::heap::GcHeap) starts with
//! an 8-byte [`GcHeader`]. The `T` payload immediately follows the header:
//!
//! ```text
//! ┌──────────────────────┬──────────────────────────────────────┐
//! │   GcHeader (8 B)     │              T payload                │
//! └──────────────────────┴──────────────────────────────────────┘
//! ▲                       ▲
//! ptr (= GcRef::as_ptr)   payload_ptr (= ptr + HEADER_SIZE)
//! ```
//!
//! `GcRef<T>` stores `ptr`. The payload pointer is computed at access time.
//!
//! # Alignment
//!
//! `T` must have `align_of::<T>() <= 8` because `GcHeader` is exactly
//! 8 bytes (and 8-aligned), so the payload is always 8-byte-aligned.
//! If `T` requires stricter alignment, the allocator must prepend
//! padding — but this is not exercised yet in the migration. A static
//! assertion enforces the invariant in [`GcHeap::alloc_typed`].
//!
//! # Identity / equality
//!
//! Two `GcRef<T>`s compare equal iff they point to the same header. This
//! is V8's `IsIdentical` invariant: identity is the raw pointer, not the
//! payload contents. Hash uses the raw pointer too.
//!
//! # Lifetime
//!
//! `GcRef<T>` is `Copy` and carries no lifetime. The borrow checker does
//! NOT prevent dangling refs. Holders must root the underlying object
//! through the [`crate::handle::HandleStack`] (V8's HandleScope pattern)
//! whenever a GC safepoint can fire while the ref is live. This matches
//! V8's `Local<T>` / `Tagged<T>` discipline. See `MIGRATION.md` for the
//! root-set audit.
//!
//! # Safety
//!
//! Constructing a `GcRef<T>` is `unsafe`: the caller must ensure the
//! pointer references a valid, header-prefixed allocation containing `T`
//! at the payload offset, and that the type tag in the header matches.
//! Constructors are gated on `unsafe fn from_raw_unchecked` at this
//! layer; higher layers (`GcHeap::alloc_typed`, the per-type wrappers
//! defined in `otter-vm`) provide safe creation.

use std::cmp::Ordering;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::marker::PhantomData;
use std::ptr::NonNull;

use crate::header::{GcHeader, HEADER_SIZE};

/// V8-style raw GC reference: a `NonNull<GcHeader>` typed for a payload `T`.
///
/// `Copy`, 8 bytes, no reference counting. Unlike `Rc<T>`, it does **not**
/// keep the object alive — the GC does. Holders must root the object on
/// the handle stack across allocation safepoints.
#[repr(transparent)]
pub struct GcRef<T> {
    ptr: NonNull<GcHeader>,
    _phantom: PhantomData<*const T>,
}

// `GcRef<T>` is a raw pointer wrapped in a typed marker. It is `Send`
// when the entire isolate moves between threads (single-threaded mutator
// model). Not `Sync` — concurrent access from multiple mutator threads
// is forbidden until the explicit concurrent-marking phase lands.
unsafe impl<T> Send for GcRef<T> {}

impl<T> GcRef<T> {
    /// Constructs a `GcRef<T>` from a raw `NonNull<GcHeader>`.
    ///
    /// # Safety
    ///
    /// - `ptr` must reference a live, header-prefixed allocation that
    ///   stores `T` at byte offset `HEADER_SIZE` from the header.
    /// - The header's `type_tag` must match the type tag that has been
    ///   registered for `T` in the [`crate::trace::TraceTable`].
    /// - The allocation must remain live for as long as the ref is used.
    ///   Rooting is the caller's responsibility (typically via
    ///   [`crate::handle::HandleStack`]).
    #[inline]
    pub const unsafe fn from_raw_unchecked(ptr: NonNull<GcHeader>) -> Self {
        Self {
            ptr,
            _phantom: PhantomData,
        }
    }

    /// Returns the raw header pointer.
    #[inline]
    pub const fn as_ptr(&self) -> NonNull<GcHeader> {
        self.ptr
    }

    /// Returns a reference to the GC header.
    #[inline]
    pub fn header(&self) -> &GcHeader {
        // SAFETY: `from_raw_unchecked`'s contract guarantees the pointer
        // references a valid, live allocation. Single-threaded mutator
        // means no concurrent mutation of header fields except for the
        // atomic mark-color bits, which `GcHeader` already exposes via
        // its internal `AtomicU8`.
        unsafe { self.ptr.as_ref() }
    }

    /// Returns a reference to the payload.
    ///
    /// Single-threaded contract: while `&T` is held, no GC safepoint
    /// fires, so the underlying memory cannot move or be reclaimed.
    #[inline]
    pub fn payload(&self) -> &T {
        // SAFETY: Construction contract guarantees the layout
        // `[GcHeader | T]` and that `T` was initialized at allocation
        // time. The payload pointer is always 8-byte-aligned because
        // `GcHeader` is 8 bytes and 8-aligned.
        unsafe {
            let payload_ptr = (self.ptr.as_ptr() as *const u8)
                .add(HEADER_SIZE)
                .cast::<T>();
            &*payload_ptr
        }
    }

    /// Returns a raw payload pointer.
    ///
    /// Useful for FFI / unsafe interop. Callers that want a `&mut T`
    /// must bring their own `unsafe` block — `GcRef<T>` is `Copy`, so
    /// handing out `&mut T` from an `&GcRef` would alias.
    #[inline]
    pub fn payload_ptr(&self) -> *mut T {
        // SAFETY: see `payload`.
        unsafe {
            (self.ptr.as_ptr() as *mut u8)
                .add(HEADER_SIZE)
                .cast::<T>()
        }
    }

    /// Returns `true` if both refs point at the same header.
    #[inline]
    pub fn ptr_eq(&self, other: &Self) -> bool {
        self.ptr == other.ptr
    }
}

// `Copy` despite holding `NonNull` — this is the cheap-pointer model
// the entire migration is built on. Cloning a ref does NOT clone the
// underlying object.
impl<T> Copy for GcRef<T> {}

impl<T> Clone for GcRef<T> {
    #[inline]
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> PartialEq for GcRef<T> {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.ptr_eq(other)
    }
}
impl<T> Eq for GcRef<T> {}

impl<T> PartialOrd for GcRef<T> {
    #[inline]
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl<T> Ord for GcRef<T> {
    #[inline]
    fn cmp(&self, other: &Self) -> Ordering {
        self.ptr.as_ptr().cmp(&other.ptr.as_ptr())
    }
}

impl<T> Hash for GcRef<T> {
    #[inline]
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.ptr.as_ptr().hash(state);
    }
}

impl<T> fmt::Debug for GcRef<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let tag = self.header().type_tag();
        let size = self.header().size_bytes();
        f.debug_struct("GcRef")
            .field("type", &std::any::type_name::<T>())
            .field("ptr", &self.ptr.as_ptr())
            .field("type_tag", &tag)
            .field("size_bytes", &size)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Type tags
// ---------------------------------------------------------------------------

/// Reserved type tags for GC-managed object kinds.
///
/// Each tag is a `u8` index into the global [`crate::trace::TraceTable`].
/// The tag is written into [`GcHeader::type_tag`] at allocation time and
/// read back during marking / scavenging to dispatch the correct
/// [`crate::trace::TraceFn`].
///
/// The order is stable: never renumber an existing tag — that would
/// break heap snapshots and saved bytecode. Append new tags at the end.
///
/// `0` is reserved as "unset / leaf without registered trace" — the
/// `TraceTable` initializer leaves slot `0` `None`, so a `GcHeader` with
/// `type_tag == 0` is treated as a leaf. We do not use `0` for any real
/// type to avoid ambiguity.
pub mod type_tag {
    /// Reserved for legacy `TypedHeap` allocations during the migration.
    /// Once Phase 6 retires `TypedHeap`, this tag becomes unused and may
    /// be repurposed.
    pub const TYPED_HEAP_LEGACY: u8 = 0;

    /// `JsStringGc` — UTF-16 / Latin-1 / Cons / Sliced / Thin string.
    /// First per-type tag introduced in Phase 2 of the GC migration.
    pub const STRING: u8 = 1;

    // Future tags reserved for forthcoming variants. Do not delete or
    // renumber.
    pub const _NEXT_AVAILABLE: u8 = 2;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ptr::NonNull;

    /// A fake GC-managed type used for layout tests. Not registered with
    /// the heap; we hand-construct the header + payload to exercise
    /// `GcRef` arithmetic directly.
    #[repr(C)]
    struct FakeString {
        len: u32,
        flags: u32,
    }

    /// Allocates a `[GcHeader | FakeString]` layout in a heap-allocated
    /// buffer and returns a `GcRef<FakeString>` over it. The buffer is
    /// leaked — only used in tests, so per-test memory leak is fine.
    fn alloc_fake(tag: u8, len: u32, flags: u32) -> GcRef<FakeString> {
        let total = HEADER_SIZE + std::mem::size_of::<FakeString>();
        let layout = std::alloc::Layout::from_size_align(total, 8).expect("layout");
        let raw = unsafe { std::alloc::alloc_zeroed(layout) };
        let ptr = NonNull::new(raw).expect("alloc");

        // Write header.
        unsafe {
            let header_ptr = ptr.as_ptr() as *mut GcHeader;
            header_ptr.write(GcHeader::new_young(tag, total as u32));
        }

        // Write payload.
        unsafe {
            let payload_ptr = ptr.as_ptr().add(HEADER_SIZE) as *mut FakeString;
            payload_ptr.write(FakeString { len, flags });
        }

        unsafe { GcRef::<FakeString>::from_raw_unchecked(ptr.cast::<GcHeader>()) }
    }

    #[test]
    fn gc_ref_is_eight_bytes_and_copy() {
        assert_eq!(std::mem::size_of::<GcRef<FakeString>>(), 8);

        let r = alloc_fake(type_tag::STRING, 4, 0);
        let r2 = r; // Copy works
        assert_eq!(r, r2);
    }

    #[test]
    fn header_is_readable_via_gc_ref() {
        let r = alloc_fake(type_tag::STRING, 7, 0);
        let h = r.header();
        assert_eq!(h.type_tag(), type_tag::STRING);
        assert!(h.is_young());
    }

    #[test]
    fn payload_round_trips_through_gc_ref() {
        let r = alloc_fake(type_tag::STRING, 13, 0xCAFE);
        let p = r.payload();
        assert_eq!(p.len, 13);
        assert_eq!(p.flags, 0xCAFE);
    }

    #[test]
    fn payload_ptr_lets_callers_mutate_with_explicit_unsafe() {
        let r = alloc_fake(type_tag::STRING, 0, 0);
        unsafe {
            let payload = r.payload_ptr();
            (*payload).len = 99;
        }
        assert_eq!(r.payload().len, 99);
    }

    #[test]
    fn equality_is_by_pointer_not_by_value() {
        let r1 = alloc_fake(type_tag::STRING, 1, 1);
        let r2 = alloc_fake(type_tag::STRING, 1, 1); // Same payload, different pointer
        assert_ne!(r1, r2);
        assert!(!r1.ptr_eq(&r2));
    }

    #[test]
    fn ord_uses_pointer_address() {
        let r1 = alloc_fake(type_tag::STRING, 0, 0);
        let r2 = alloc_fake(type_tag::STRING, 0, 0);
        // Whichever is at the lower address compares less. We don't care
        // which one — we just verify the relation is consistent.
        let ord = r1.cmp(&r2);
        assert_ne!(ord, Ordering::Equal);
        assert_eq!(ord.reverse(), r2.cmp(&r1));
    }

    #[test]
    fn debug_format_prints_type_name_and_tag() {
        let r = alloc_fake(type_tag::STRING, 0, 0);
        let s = format!("{r:?}");
        assert!(s.contains("FakeString"), "got: {s}");
        assert!(s.contains("type_tag: 1"), "got: {s}");
    }

    #[test]
    fn type_tag_constants_are_stable() {
        // These are part of the heap-snapshot ABI. Changing them
        // breaks saved bytecode and DevTools snapshots.
        assert_eq!(type_tag::TYPED_HEAP_LEGACY, 0);
        assert_eq!(type_tag::STRING, 1);
        assert_eq!(type_tag::_NEXT_AVAILABLE, 2);
    }
}
