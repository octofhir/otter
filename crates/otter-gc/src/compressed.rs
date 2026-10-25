//! Pointer compression: the heap cage and 32-bit `Gc<T>` handles.
//!
//! Every page allocation in the GC is carved from a process-global
//! virtual cage. All GC pointers are 32-bit offsets relative to the
//! cage base, decompressed to a real `*mut u8` via
//! `cage_base + offset`. This halves heap-pointer footprint vs. raw
//! 64-bit pointers.
//!
//! # Contents
//!
//! - [`Gc<T>`] — opaque 32-bit compressed pointer, `repr(transparent)`.
//! - [`RawGc`] — typeless slot value (`u32`), used by trace
//!   functions that walk slots polymorphically.
//! - [`init_cage_with_size`] — debug-only init API: tests/benches
//!   override the default cage size before any heap is created.
//! - [`Cage`] — process-global cage owning the 4 GiB virtual region,
//!   the page free-list and the pool of released pages still committed.
//! - [`age_retained_pages`] / [`release_retained_pages`] — give pooled
//!   pages' memory back once they sit unused, or at once.
//! - [`discard`] — address-preserving physical release of unused cage ranges.
//!
//! # Invariants
//!
//! - `cage_base() + offset` is always within the cage for any
//!   `Gc<T>` produced by `Cage::alloc_page`. Decompression of an
//!   offset that came from this cage never reads OOB.
//! - `Gc::null()` has offset 0; offset 0 corresponds to the cage's
//!   first byte which is reserved (page 0 is never handed out as a
//!   valid GC page).
//! - The cage is initialised at most once per process; subsequent
//!   `init_cage_with_size` calls return [`CageError::AlreadyInit`].
//! - Page allocation from the cage is single-mutex-protected; one
//!   isolate per process today, so contention is zero.
//! - A released page keeps its memory and its stale bytes until it is
//!   reused or aged out: [`crate::page::Page`] rewrites every header
//!   field on reuse and every allocator initializes each byte it hands
//!   out, so nothing reads a page's prior contents. A pooled page left
//!   unused across a whole aging window is remapped, which releases its
//!   memory and leaves it reading as zero.
//!
//! # See also
//!
//! - GC architecture plan §1.2 NF9 (pointer-compression invariants),
//!   §2.3 ("Pointer compression — add in Phase 1").

use std::alloc::{Layout, alloc, dealloc};
use std::marker::PhantomData;
use std::sync::Mutex;
use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use crate::page::PAGE_SIZE;

pub(crate) mod discard;

/// Default cage size when [`init_cage_with_size`] has not been
/// called: 256 MiB on 64-bit hosts.
///
/// Production embedders that want the full 4 GiB cage
/// must call `init_cage_with_size(4 * 1024 * 1024 * 1024)` before
/// the first [`crate::heap::GcHeap::new`].
pub const DEFAULT_CAGE_SIZE_BYTES: usize = 2 * 1024 * 1024 * 1024;

/// Maximum cage size — the compressed-pointer cap. `Gc<T>` is a
/// `u32`, so the cage cannot exceed `u32::MAX + 1` bytes.
pub const MAX_CAGE_SIZE_BYTES: usize = 1usize << 32;

/// Cage-base alignment: 4 GiB (`1 << 32`).
///
/// The base is aligned so that `cage_base & 0xFFFF_FFFF == 0`. With a
/// cage no larger than [`MAX_CAGE_SIZE_BYTES`], every in-cage address
/// is then `cage_base | offset` with the 32-bit offset occupying the
/// low word verbatim. That is the load-bearing property of the
/// pointer-cheap value encoding: a heap pointer stored as a full
/// `cage_base + offset` address carries its compressed offset in its
/// own low 32 bits, so the moving collector can still rewrite that
/// 4-byte offset in place during relocation (the high half is the
/// constant cage prefix) while the interpreter dereferences the full
/// address with no unmask. See `otter_vm::value`.
pub const CAGE_ALIGN_BYTES: usize = 1usize << 32;

/// Process-global cage base (null before init). Stored as
/// `AtomicPtr<u8>` rather than `AtomicUsize` so cage offsets can
/// be decompressed via `cage_base().add(offset)` — pointer
/// arithmetic that preserves provenance through Stacked /
/// Tree Borrows. The matching `Release` store happens inside
/// [`Cage::ensure_inner`].
pub(crate) static CAGE_BASE: AtomicPtr<u8> = AtomicPtr::new(core::ptr::null_mut());

/// Process-global cage size in bytes (zero before init).
pub(crate) static CAGE_SIZE: AtomicUsize = AtomicUsize::new(0);

/// Errors produced by cage initialisation.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CageError {
    /// `init_cage_with_size` was called after the cage had already
    /// been initialised. Cage initialisation is one-shot per
    /// process.
    #[error("cage already initialised")]
    AlreadyInit,

    /// Requested cage size is zero, exceeds [`MAX_CAGE_SIZE_BYTES`],
    /// or is not a multiple of [`PAGE_SIZE`].
    #[error(
        "invalid cage size: {0} bytes (must be a non-zero multiple of PAGE_SIZE ≤ MAX_CAGE_SIZE_BYTES)"
    )]
    InvalidSize(usize),

    /// The host allocator refused the cage allocation.
    #[error("cage allocation failed")]
    AllocFailed,
}

/// Initialise the process-global cage with the given size in
/// bytes. Must be called before the first [`crate::heap::GcHeap::new`];
/// returns [`CageError::AlreadyInit`] otherwise.
///
/// # Errors
///
/// - [`CageError::InvalidSize`] — size is zero, not a multiple of
///   [`PAGE_SIZE`], or exceeds [`MAX_CAGE_SIZE_BYTES`].
/// - [`CageError::AlreadyInit`] — the cage has already been set up
///   (by a prior `init_cage_with_size` call or by an implicit
///   default-size init from `GcHeap::new`).
/// - [`CageError::AllocFailed`] — the host allocator refused the
///   request.
pub fn init_cage_with_size(size_bytes: usize) -> Result<(), CageError> {
    if size_bytes == 0 || size_bytes > MAX_CAGE_SIZE_BYTES || !size_bytes.is_multiple_of(PAGE_SIZE)
    {
        return Err(CageError::InvalidSize(size_bytes));
    }
    Cage::ensure_with_size(size_bytes)
}

/// Returns the cage base pointer. Returns `null_mut()` before the
/// cage has been initialised.
///
/// Every decompression reads it, so the load is `Relaxed`: a plain load
/// rather than an ordering one on the hottest path of the VM. That is
/// enough because the base is written once, inside
/// [`Cage::ensure_inner`] under the cage mutex, before any page exists.
/// Whoever holds a compressed offset reached it through a heap whose
/// pages came out of that mutex after the store, so the store happens
/// before this load and coherence makes the load see it.
///
/// Decompression of a `Gc<T>` offset goes through
/// `cage_base().add(offset as usize)` so pointer provenance is
/// preserved (Stacked / Tree Borrows clean).
#[inline]
pub fn cage_base() -> *mut u8 {
    CAGE_BASE.load(Ordering::Relaxed)
}

/// Compatibility helper: returns the cage base address as a
/// `usize` for callers that want to compare against a raw
/// pointer cast. Avoid using this for decompression — call
/// [`cage_base`] and arithmetic instead so provenance is
/// preserved.
#[inline]
pub fn cage_base_addr() -> usize {
    cage_base() as usize
}

/// Returns the cage size in bytes. Zero before initialisation.
#[inline]
pub fn cage_size() -> usize {
    CAGE_SIZE.load(Ordering::Acquire)
}

/// Process-global cage occupancy snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CageStats {
    /// Cage size in bytes.
    pub size_bytes: usize,
    /// Total page slots in the cage, including reserved page 0.
    pub total_pages: u32,
    /// Page slots currently on the cage free-list.
    pub free_pages: usize,
    /// Page slots currently owned by GC heaps.
    pub allocated_pages: usize,
    /// Free page slots whose memory is still committed for reuse.
    pub retained_pages: usize,
}

/// Return cage occupancy diagnostics after initialisation.
#[inline]
#[must_use]
pub fn cage_stats() -> Option<CageStats> {
    Cage::stats()
}

/// How long a released page stays committed in the pool, unused, before
/// its memory goes back to the system: a page pooled just after an aging
/// step is released between one and two windows later.
const RETAINED_PAGE_WINDOW: Duration = Duration::from_secs(8);

/// Age the pool of released pages at `now`: once a window has passed since
/// the previous step, every page pooled before that step and still unused
/// is released. Collections call this at their start.
pub fn age_retained_pages(now: Instant) {
    let mut guard = CAGE_GUARD.lock().expect("cage mutex poisoned");
    if let Some(cage) = guard.as_mut() {
        cage.age_retained(now);
    }
}

/// Release the memory of every pooled page at once, as a collection that
/// gives back unused physical memory does.
pub fn release_retained_pages() {
    let mut guard = CAGE_GUARD.lock().expect("cage mutex poisoned");
    if let Some(cage) = guard.as_mut() {
        cage.release_retained(|_| true);
    }
}

// ---------------------------------------------------------------------------
// Gc<T>
// ---------------------------------------------------------------------------

/// A 32-bit compressed pointer to a `T`-typed object on the GC heap.
///
/// `Gc<T>(0)` is the null offset; valid `Gc<T>` values point at a
/// [`crate::header::GcHeader`] plus payload inside the cage.
///
/// # Layout
///
/// `#[repr(transparent)]` over `u32`. `size_of::<Gc<T>>() == 4` —
/// that is the load-bearing property: heap-side `Value` slots,
/// object property storage, and array element storage all use the
/// 4-byte form.
#[repr(transparent)]
pub struct Gc<T: ?Sized> {
    offset: u32,
    _marker: PhantomData<*const T>,
}

impl<T: ?Sized> Gc<T> {
    /// The null offset. Decompresses to the cage base, which is
    /// reserved (no valid GC object lives at offset 0).
    #[inline]
    pub const fn null() -> Self {
        Self {
            offset: 0,
            _marker: PhantomData,
        }
    }

    /// Returns the raw 32-bit offset.
    #[inline]
    pub const fn offset(self) -> u32 {
        self.offset
    }

    /// Returns true iff this is the null pointer.
    #[inline]
    pub const fn is_null(self) -> bool {
        self.offset == 0
    }

    /// Type-erases `self` into the collector backend slot type.
    ///
    /// Normal contributor code must not persist or inspect this
    /// value. It exists for audited VM adapter layers and the
    /// collector's own root/trace walkers.
    #[doc(hidden)]
    #[inline]
    pub const fn raw(self) -> RawGc {
        RawGc(self.offset)
    }

    /// Reinterprets a raw offset as `Gc<T>`.
    ///
    /// # Safety
    ///
    /// `offset` must either be zero or point at a header for an
    /// object of type `T` (or a layout-compatible supertype). The
    /// caller is responsible for ensuring this — `Gc<T>` is by
    /// construction unsound if used with the wrong `T`.
    #[inline]
    pub const unsafe fn from_offset(offset: u32) -> Self {
        Self {
            offset,
            _marker: PhantomData,
        }
    }

    /// Decompresses to a raw header pointer.
    ///
    /// Returns `core::ptr::null_mut()` if `self.is_null()`.
    /// Otherwise the result is `cage_base.add(offset)`. The
    /// caller must not dereference the result before the cage
    /// is initialised. Uses pointer arithmetic (not int math)
    /// so provenance flows through Stacked / Tree Borrows.
    #[inline]
    pub fn as_header_ptr(self) -> *mut crate::header::GcHeader {
        if self.offset == 0 {
            return core::ptr::null_mut();
        }
        let base = cage_base();
        debug_assert!(
            !base.is_null(),
            "decompressing Gc<T> before the cage has been initialised"
        );
        // SAFETY: offset comes from a successful `Cage::alloc_page`
        // path; it is in-cage by construction.
        unsafe { base.add(self.offset as usize) as *mut crate::header::GcHeader }
    }
}

impl<T: ?Sized> Clone for Gc<T> {
    #[inline]
    fn clone(&self) -> Self {
        *self
    }
}

impl<T: ?Sized> Copy for Gc<T> {}

impl<T: ?Sized> Default for Gc<T> {
    #[inline]
    fn default() -> Self {
        Self::null()
    }
}

impl<T: ?Sized> PartialEq for Gc<T> {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.offset == other.offset
    }
}

impl<T: ?Sized> Eq for Gc<T> {}

impl<T: ?Sized> std::fmt::Debug for Gc<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Gc<{}>(0x{:08x})",
            std::any::type_name::<T>(),
            self.offset
        )
    }
}

impl<T: ?Sized> std::hash::Hash for Gc<T> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.offset.hash(state);
    }
}

// ---------------------------------------------------------------------------
// RawGc
// ---------------------------------------------------------------------------

/// Type-erased compressed pointer. `repr(transparent)` over `u32`.
///
/// Used inside trace functions which walk slot pointers without
/// knowing the element type, and in the marker / scavenger
/// worklists (which only ever read the `GcHeader`).
#[doc(hidden)]
#[repr(transparent)]
#[derive(Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct RawGc(pub u32);

impl RawGc {
    /// The null offset.
    pub const NULL: Self = Self(0);

    /// Returns true iff this is the null offset.
    #[inline]
    pub const fn is_null(self) -> bool {
        self.0 == 0
    }

    /// Decompresses to a raw header pointer; null offsets map to
    /// `core::ptr::null_mut()`. Uses pointer arithmetic so
    /// provenance is preserved.
    #[inline]
    pub fn as_header_ptr(self) -> *mut crate::header::GcHeader {
        if self.0 == 0 {
            return core::ptr::null_mut();
        }
        let base = cage_base();
        debug_assert!(!base.is_null());
        // SAFETY: offset was issued by `Cage::alloc_page`; it is
        // in-cage by construction.
        unsafe { base.add(self.0 as usize) as *mut crate::header::GcHeader }
    }

    /// Reinterprets `self` as `Gc<T>`.
    ///
    /// # Safety
    ///
    /// The caller must ensure the offset references an object of
    /// type `T` (or a layout-compatible supertype).
    #[inline]
    pub const unsafe fn cast<T: ?Sized>(self) -> Gc<T> {
        unsafe { Gc::from_offset(self.0) }
    }

    /// Read the underlying object's [`crate::header::GcHeader::type_tag`].
    ///
    /// Returns `None` for the null offset. Safe in callers because
    /// every non-null `RawGc` was issued by a successful
    /// `Cage::alloc_page` and the cage stays mapped for the lifetime
    /// of the runtime, so the header read is sound by construction.
    #[inline]
    #[must_use]
    pub fn header_type_tag(self) -> Option<u8> {
        if self.0 == 0 {
            return None;
        }
        let header = self.as_header_ptr();
        // SAFETY: `self` originates from a successful GC allocation;
        // the cage remains live for the runtime's lifetime, so
        // `header` points at an initialised `GcHeader`. Reading the
        // `type_tag` byte is a plain load with no aliasing concerns.
        Some(unsafe { (*header).type_tag() })
    }

    /// Type-tag-checked downcast to `Gc<T>`.
    ///
    /// Returns `Some(Gc<T>)` iff the body header's
    /// [`crate::header::GcHeader::type_tag`] matches
    /// [`crate::trace::SafeTraceable::TYPE_TAG`]. Returns `None` for
    /// the null offset or any tag mismatch. This is the safe accessor
    /// `forbid(unsafe_code)` crates use when downcasting `RawGc`.
    #[inline]
    #[must_use]
    pub fn checked_cast<T: crate::trace::SafeTraceable>(self) -> Option<Gc<T>> {
        if self.0 == 0 {
            return None;
        }
        let header = self.as_header_ptr();
        // SAFETY: `self` originates from a successful GC allocation
        // (see `header_type_tag`). The tag check below gates the
        // `from_offset` reinterpret on byte-for-byte compatibility
        // with `T`.
        let tag = unsafe { (*header).type_tag() };
        if tag != <T as crate::trace::SafeTraceable>::TYPE_TAG {
            return None;
        }
        // SAFETY: type-tag matches the trait's reserved value, so
        // this cell holds a `T` payload.
        Some(unsafe { Gc::from_offset(self.0) })
    }
}

impl std::fmt::Debug for RawGc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "RawGc(0x{:08x})", self.0)
    }
}

const _: () = assert!(std::mem::size_of::<Gc<()>>() == 4, "Gc<T> must be 4 bytes");
const _: () = assert!(std::mem::size_of::<RawGc>() == 4, "RawGc must be 4 bytes");

// ---------------------------------------------------------------------------
// Cage
// ---------------------------------------------------------------------------

/// Process-global state guarding the cage allocation and the
/// page free-list. The mutex is taken on page alloc / free only;
/// hot-path bump alloc inside a page does not touch it.
pub(crate) struct Cage {
    /// Cage base pointer — 4 GiB-aligned (see [`CAGE_ALIGN_BYTES`]).
    /// Lives until process exit. This is the address handed out by
    /// [`cage_base`]; it is carved from the interior of
    /// `reservation` so that `base & 0xFFFF_FFFF == 0`.
    base: *mut u8,
    /// Cage size in bytes (multiple of [`PAGE_SIZE`]).
    size: usize,
    /// Raw allocation pointer returned by the host allocator, kept so
    /// the over-aligned reservation can be returned with the exact
    /// layout it was requested with. `base` points at or after this.
    reservation: *mut u8,
    /// Layout the reservation was allocated with — required for a
    /// sound `dealloc`.
    reservation_layout: Layout,
    /// Released page runs, keyed by first index and coalesced with their
    /// neighbours on release. A run list rather than a list of loose
    /// indices because a body larger than one page needs consecutive
    /// pages: with loose indices, freed space could never satisfy a
    /// multi-page request again once the fresh tail ran out.
    free_runs: std::collections::BTreeMap<u32, u32>,
    /// Bump cursor over never-used pages. Page 0 is reserved so `Gc<T>(0)`
    /// stays null, so this starts at 1; released pages come back through
    /// [`Cage::free_page`] into `free_runs` instead.
    next_fresh: u32,
    /// Total page count (`size / PAGE_SIZE`).
    page_count: u32,
    /// Free pages whose memory is still committed, one bit per page: a
    /// single-page request takes one of these before anything else, so
    /// reuse neither faults memory in nor costs a system call.
    retained: Vec<u64>,
    /// Retained pages already pooled at the previous aging step.
    aging: Vec<u64>,
    /// Set bits in `retained`.
    retained_count: u32,
    /// When the next aging step is due; `None` while the pool is empty.
    next_aging: Option<Instant>,
}

// SAFETY: Cage is mutex-protected at the call site (CAGE_GUARD).
// The internal raw pointer is only ever dereferenced for offset
// arithmetic, never as a Rust reference, so the !Send/!Sync of
// `*mut u8` is conservative — Cage itself is logically safe to
// share if the mutex protects it.
unsafe impl Send for Cage {}
unsafe impl Sync for Cage {}

static CAGE_GUARD: Mutex<Option<Cage>> = Mutex::new(None);

#[cfg(test)]
pub(crate) static CAGE_TEST_LOCK: Mutex<()> = Mutex::new(());

impl Cage {
    /// Ensure the cage has been initialised at the default size.
    /// No-op if it is already up.
    pub(crate) fn ensure_default() -> Result<(), CageError> {
        Self::ensure_inner(DEFAULT_CAGE_SIZE_BYTES, false)
    }

    /// Initialise the cage at the explicit size, failing if it
    /// has already been brought up.
    pub(crate) fn ensure_with_size(size_bytes: usize) -> Result<(), CageError> {
        Self::ensure_inner(size_bytes, true)
    }

    fn ensure_inner(size_bytes: usize, strict: bool) -> Result<(), CageError> {
        if size_bytes == 0
            || size_bytes > MAX_CAGE_SIZE_BYTES
            || !size_bytes.is_multiple_of(PAGE_SIZE)
        {
            return Err(CageError::InvalidSize(size_bytes));
        }
        let mut guard = CAGE_GUARD.lock().expect("cage mutex poisoned");
        if guard.is_some() {
            if strict {
                return Err(CageError::AlreadyInit);
            }
            return Ok(());
        }
        // Over-allocate by one full alignment so a 4 GiB-aligned base
        // is guaranteed to exist inside the reservation, then carve
        // the aligned cage from its interior. The 4 GiB alignment is
        // what lets a full `cage_base + offset` address keep its
        // 32-bit offset in the low word (see [`CAGE_ALIGN_BYTES`]).
        // The reservation is virtual until faulted, so the extra
        // alignment slack costs address space, not physical pages.
        //
        // SAFETY: Layout is built from a non-zero size we just
        // validated; `alloc` obtains the region without eagerly
        // zeroing it. Individual pages initialise their headers in
        // `Page::new`; nothing reads the bytes a reused page held before.
        let reservation_size = size_bytes + CAGE_ALIGN_BYTES;
        let reservation_layout = Layout::from_size_align(reservation_size, PAGE_SIZE)
            .map_err(|_| CageError::InvalidSize(size_bytes))?;
        let reservation = unsafe { alloc(reservation_layout) };
        if reservation.is_null() {
            return Err(CageError::AllocFailed);
        }
        let base_addr = (reservation as usize)
            .checked_add(CAGE_ALIGN_BYTES - 1)
            .map(|a| a & !(CAGE_ALIGN_BYTES - 1))
            .expect("cage base alignment overflow");
        // SAFETY: `base_addr` lies within `[reservation, reservation +
        // reservation_size - size_bytes]` by construction, so the
        // aligned cage of `size_bytes` is fully inside the reservation.
        let ptr = base_addr as *mut u8;
        // The pointer-cheap value encoding requires the cage to sit in
        // the low-48-bit virtual-address window (so heap pointers have
        // their top 16 bits clear) and to be 4 GiB-aligned (so the low
        // 32 bits are exactly the compressed offset). Both hold on
        // arm64 / x86-64 user space; assert so a future port that
        // breaks them fails loudly here rather than via silent pointer
        // corruption.
        assert!(
            base_addr & (CAGE_ALIGN_BYTES - 1) == 0,
            "cage base must be 4 GiB-aligned"
        );
        assert!(
            base_addr >> 48 == 0,
            "cage base must lie in the low 48-bit VA window"
        );
        let page_count = (size_bytes / PAGE_SIZE) as u32;
        // Page 0 is reserved so that Gc<T>(0) stays null.
        // Page 0 stays reserved so `Gc<T>(0)` is null.
        let free_runs: std::collections::BTreeMap<u32, u32> = std::collections::BTreeMap::new();
        let words = (page_count as usize).div_ceil(64);
        let cage = Cage {
            base: ptr,
            size: size_bytes,
            reservation,
            reservation_layout,
            free_runs,
            next_fresh: 1,
            page_count,
            retained: vec![0; words],
            aging: vec![0; words],
            retained_count: 0,
            next_aging: None,
        };
        CAGE_BASE.store(ptr, Ordering::Release);
        CAGE_SIZE.store(size_bytes, Ordering::Release);
        *guard = Some(cage);
        Ok(())
    }

    /// Pop a free page index. Returns `None` if the cage is
    /// exhausted; the caller surfaces this as
    /// [`crate::oom::OutOfMemory`].
    pub(crate) fn alloc_page() -> Option<CagePage> {
        let mut guard = CAGE_GUARD.lock().expect("cage mutex poisoned");
        let cage = guard.as_mut().expect("cage not initialised");
        let idx = cage.take_run(1)?;
        // SAFETY: `idx < page_count` by construction of `free_pages`,
        // and `cage.base + idx * PAGE_SIZE` is therefore inside the
        // cage region.
        let page_ptr = unsafe { cage.base.add(idx as usize * PAGE_SIZE) };
        Some(CagePage {
            base: page_ptr,
            offset: idx * (PAGE_SIZE as u32),
        })
    }

    /// Atomically acquire `count` page slots under one cage lock.
    ///
    /// If fewer slots are free, returns `None` without removing any. Promotion
    /// preflight uses this all-or-nothing operation so a failed reservation
    /// cannot perturb the cage or require rollback allocations.
    pub(crate) fn alloc_pages(count: usize) -> Option<Vec<CagePage>> {
        let mut guard = CAGE_GUARD.lock().expect("cage mutex poisoned");
        let cage = guard.as_mut().expect("cage not initialised");
        if cage.available_pages() < count {
            return None;
        }
        let mut pages = Vec::with_capacity(count);
        for _ in 0..count {
            let idx = cage.take_run(1).expect("availability checked above");
            // SAFETY: every free-list index is inside the cage.
            let page_ptr = unsafe { cage.base.add(idx as usize * PAGE_SIZE) };
            pages.push(CagePage {
                base: page_ptr,
                offset: idx * (PAGE_SIZE as u32),
            });
        }
        Some(pages)
    }

    /// Carve `count` **consecutive** pages and return the first.
    ///
    /// A body larger than one page needs its bytes contiguous. Runs come
    /// from the cage's never-used tail rather than from the free list: the
    /// list holds pages in release order, so satisfying a run from it would
    /// mean searching for a gap, and the whole point of a bump cursor is
    /// that fresh space is already contiguous. The cage is a reservation
    /// with a bump cursor for new chunks and a free list for recycled ones.
    ///
    /// Returns `None` when the tail cannot cover the run; a single-page
    /// caller still recycles through the free list.
    pub(crate) fn alloc_contiguous_pages(count: usize) -> Option<CagePage> {
        debug_assert!(count > 0);
        let mut guard = CAGE_GUARD.lock().expect("cage mutex poisoned");
        let cage = guard.as_mut().expect("cage not initialised");
        let first = cage.take_run(u32::try_from(count).ok()?)?;
        // SAFETY: the run lies below `page_count`, so it is inside the cage.
        let page_ptr = unsafe { cage.base.add(first as usize * PAGE_SIZE) };
        Some(CagePage {
            base: page_ptr,
            offset: first * (PAGE_SIZE as u32),
        })
    }

    /// Pages available to hand out: released runs plus the untouched tail.
    fn available_pages(&self) -> usize {
        self.free_runs.values().map(|&n| n as usize).sum::<usize>()
            + (self.page_count - self.next_fresh) as usize
    }

    /// Take `count` consecutive pages: one page from the retained pool
    /// when it holds any, else a released run, else the untouched tail.
    /// First fit: runs are few, and a best-fit scan would cost more than
    /// the fragmentation it saves.
    fn take_run(&mut self, count: u32) -> Option<u32> {
        if count == 1
            && let Some(index) = self.first_retained()
        {
            let (&start, &len) = self
                .free_runs
                .range(..=index)
                .next_back()
                .expect("a retained page lies in a free run");
            debug_assert!(index < start + len);
            self.free_runs.remove(&start);
            if index > start {
                self.free_runs.insert(start, index - start);
            }
            if start + len > index + 1 {
                self.free_runs.insert(index + 1, start + len - index - 1);
            }
            self.claim_retained(index, 1);
            return Some(index);
        }
        if let Some((&start, &len)) = self.free_runs.iter().find(|entry| *entry.1 >= count) {
            self.free_runs.remove(&start);
            if len > count {
                self.free_runs.insert(start + count, len - count);
            }
            self.claim_retained(start, count);
            return Some(start);
        }
        let end = self.next_fresh.checked_add(count)?;
        if end > self.page_count {
            return None;
        }
        let first = self.next_fresh;
        self.next_fresh = end;
        Some(first)
    }

    /// The lowest retained page, if any.
    fn first_retained(&self) -> Option<u32> {
        if self.retained_count == 0 {
            return None;
        }
        self.retained
            .iter()
            .enumerate()
            .find(|(_, word)| **word != 0)
            .map(|(index, word)| index as u32 * 64 + word.trailing_zeros())
    }

    /// Drop pages `start..start + count`, being handed out, from the pool.
    fn claim_retained(&mut self, start: u32, count: u32) {
        if self.retained_count == 0 {
            return;
        }
        for index in start..start + count {
            let (word, bit) = (index as usize / 64, 1u64 << (index % 64));
            if self.retained[word] & bit != 0 {
                self.retained[word] &= !bit;
                self.aging[word] &= !bit;
                self.retained_count -= 1;
            }
        }
        if self.retained_count == 0 {
            self.next_aging = None;
        }
    }

    /// Pool pages `start..start + count`, just released, with their memory.
    fn retain(&mut self, start: u32, count: u32) {
        for index in start..start + count {
            self.retained[index as usize / 64] |= 1u64 << (index % 64);
        }
        self.retained_count += count;
    }

    /// One aging step at `now`, when due: release the pages pooled before
    /// the previous step, then age the rest.
    fn age_retained(&mut self, now: Instant) {
        if self.retained_count == 0 {
            return;
        }
        let Some(due) = self.next_aging else {
            self.next_aging = Some(now + RETAINED_PAGE_WINDOW);
            return;
        };
        if now < due {
            return;
        }
        let aging = self.aging.clone();
        self.release_retained(|index| aging[index as usize / 64] & (1u64 << (index % 64)) != 0);
        self.aging.clone_from(&self.retained);
        self.next_aging = (self.retained_count != 0).then(|| now + RETAINED_PAGE_WINDOW);
    }

    /// Release the memory of every retained page `select` picks, one
    /// remap per consecutive stretch.
    fn release_retained(&mut self, select: impl Fn(u32) -> bool) {
        let mut index = 0;
        while let Some(start) = self.next_released(index, &select) {
            let mut end = start + 1;
            while end < self.page_count && self.is_retained(end) && select(end) {
                end += 1;
            }
            // SAFETY: a retained page is free: no live object refers to
            // it and the mutex makes the cage the range's only owner.
            unsafe {
                release_memory(
                    self.base.add(start as usize * PAGE_SIZE),
                    (end - start) as usize * PAGE_SIZE,
                );
            }
            for page in start..end {
                let (word, bit) = (page as usize / 64, 1u64 << (page % 64));
                self.retained[word] &= !bit;
                self.aging[word] &= !bit;
            }
            self.retained_count -= end - start;
            index = end;
        }
        if self.retained_count == 0 {
            self.aging.fill(0);
            self.next_aging = None;
        }
    }

    /// The first retained page at or after `from` that `select` picks.
    fn next_released(&self, from: u32, select: &impl Fn(u32) -> bool) -> Option<u32> {
        let mut word = from as usize / 64;
        let mut bits = self.retained.get(word)? & (!0u64 << (from % 64));
        loop {
            while bits != 0 {
                let index = word as u32 * 64 + bits.trailing_zeros();
                if select(index) {
                    return Some(index);
                }
                bits &= bits - 1;
            }
            word += 1;
            bits = *self.retained.get(word)?;
        }
    }

    fn is_retained(&self, index: u32) -> bool {
        self.retained[index as usize / 64] & (1u64 << (index % 64)) != 0
    }

    /// Return `count` consecutive pages, merging with any neighbouring
    /// free run so the space can satisfy a large request again.
    fn release_run(&mut self, start: u32, count: u32) {
        let mut start = start;
        let mut count = count;
        if let Some((&prev_start, &prev_len)) = self.free_runs.range(..start).next_back()
            && prev_start + prev_len == start
        {
            self.free_runs.remove(&prev_start);
            start = prev_start;
            count += prev_len;
        }
        if let Some(&next_len) = self.free_runs.get(&(start + count)) {
            self.free_runs.remove(&(start + count));
            count += next_len;
        }
        self.free_runs.insert(start, count);
    }

    /// Return `count` consecutive pages starting at `offset` to the
    /// free-list, their memory pooled for reuse.
    ///
    /// # Safety
    ///
    /// Same contract as [`Cage::free_page`], for every page in the run.
    pub(crate) unsafe fn free_pages_run(offset: u32, count: u32) {
        let mut guard = CAGE_GUARD.lock().expect("cage mutex poisoned");
        let cage = guard.as_mut().expect("cage not initialised");
        debug_assert!(offset.is_multiple_of(PAGE_SIZE as u32));
        let idx = offset / (PAGE_SIZE as u32);
        debug_assert!(idx + count <= cage.page_count);
        cage.retain(idx, count);
        cage.release_run(idx, count);
    }

    /// Return a page to the cage free-list, its memory pooled for reuse.
    ///
    /// # Safety
    ///
    /// The caller must guarantee no live `Gc<T>` still references
    /// any object on this page. Returning a still-referenced page
    /// will manifest as a use-after-free on the next alloc.
    pub(crate) unsafe fn free_page(offset: u32) {
        let mut guard = CAGE_GUARD.lock().expect("cage mutex poisoned");
        let cage = guard.as_mut().expect("cage not initialised");
        debug_assert!(offset.is_multiple_of(PAGE_SIZE as u32));
        debug_assert!(offset / (PAGE_SIZE as u32) < cage.page_count);
        let idx = offset / (PAGE_SIZE as u32);
        cage.retain(idx, 1);
        cage.release_run(idx, 1);
    }

    /// Whether the page at cage offset `offset` is free: in a released run
    /// or the untouched tail.
    #[cfg(test)]
    pub(crate) fn is_free_page(offset: u32) -> bool {
        let guard = CAGE_GUARD.lock().expect("cage mutex poisoned");
        let cage = guard.as_ref().expect("cage not initialised");
        let index = offset / PAGE_SIZE as u32;
        index >= cage.next_fresh
            || cage
                .free_runs
                .range(..=index)
                .next_back()
                .is_some_and(|(&start, &len)| index < start + len)
    }

    pub(crate) fn stats() -> Option<CageStats> {
        let guard = CAGE_GUARD.lock().expect("cage mutex poisoned");
        guard.as_ref().map(|c| {
            let free_pages = c.available_pages();
            let reserved_pages = usize::from(c.page_count > 0);
            let allocated_pages = (c.page_count as usize)
                .saturating_sub(free_pages)
                .saturating_sub(reserved_pages);
            CageStats {
                size_bytes: c.size,
                total_pages: c.page_count,
                free_pages,
                allocated_pages,
                retained_pages: c.retained_count as usize,
            }
        })
    }
}

/// Give the physical memory of `len` freed cage bytes at `ptr` back to the
/// system. A fresh anonymous mapping over the range releases the memory at
/// once, where a lazy advice would leave it charged to the process until
/// memory pressure; the address range stays reserved and writable. Should
/// the remap fail, the memory simply stays committed: nothing reads a free
/// page's bytes.
///
/// # Safety
///
/// `ptr..ptr + len` is page-aligned, inside the cage reservation, and no
/// live GC object refers to it.
unsafe fn release_memory(ptr: *mut u8, len: usize) {
    // SAFETY: per docstring; the range is aligned and exclusively owned.
    let _ = unsafe { discard::remap_zeroed(ptr, len) };
}

impl Drop for Cage {
    fn drop(&mut self) {
        // Cage lives for process lifetime; this Drop only fires if
        // the static guard is reset (test harness teardown). Free
        // the backing region so leak sanitiser is happy.
        if !self.reservation.is_null() {
            // SAFETY: `reservation` / `reservation_layout` are the exact
            // pointer and layout returned by `alloc` in `ensure_inner`
            // and have not been freed yet. `base` is an interior,
            // aligned slice of this reservation and must NOT be freed
            // directly.
            unsafe {
                dealloc(self.reservation, self.reservation_layout);
            }
            self.reservation = core::ptr::null_mut();
            self.base = core::ptr::null_mut();
        }
    }
}

/// Owning handle to a freshly-carved cage page. Returned by
/// [`Cage::alloc_page`]; the [`crate::page::Page`] wrapper takes
/// ownership and ensures the page is returned via
/// [`Cage::free_page`] on drop.
pub(crate) struct CagePage {
    pub(crate) base: *mut u8,
    pub(crate) offset: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Force the cage up at default size for the unit test below.
    /// Module-level integration tests in `tests/*.rs` may call
    /// [`init_cage_with_size`] explicitly first.
    fn ensure_cage() {
        let _ = Cage::ensure_default();
    }

    #[test]
    fn gc_null_is_zero() {
        let g: Gc<u32> = Gc::null();
        assert_eq!(g.offset(), 0);
        assert!(g.is_null());
    }

    #[test]
    fn gc_size_is_4_bytes() {
        assert_eq!(std::mem::size_of::<Gc<u32>>(), 4);
        assert_eq!(std::mem::size_of::<Gc<[u8; 1024]>>(), 4);
    }

    #[test]
    fn cage_initialises_and_hands_out_pages() {
        let _guard = CAGE_TEST_LOCK.lock().expect("cage test lock");
        ensure_cage();
        let p1 = Cage::alloc_page().expect("page 1");
        let p2 = Cage::alloc_page().expect("page 2");
        assert_ne!(p1.offset, p2.offset);
        assert!(p1.offset >= PAGE_SIZE as u32);
        assert!(p2.offset >= PAGE_SIZE as u32);
        // SAFETY: we have not stored any Gc<T> referencing these
        // pages, so it is safe to free them.
        unsafe {
            Cage::free_page(p1.offset);
            Cage::free_page(p2.offset);
        }
    }

    /// A private cage of `pages` pages, outside the process-global one.
    fn local_cage(pages: u32) -> Cage {
        let size = pages as usize * PAGE_SIZE;
        let layout = Layout::from_size_align(size, PAGE_SIZE).expect("layout");
        // SAFETY: the layout has a non-zero size.
        let reservation = unsafe { alloc(layout) };
        assert!(!reservation.is_null());
        let words = (pages as usize).div_ceil(64);
        Cage {
            base: reservation,
            size,
            reservation,
            reservation_layout: layout,
            free_runs: std::collections::BTreeMap::new(),
            next_fresh: 1,
            page_count: pages,
            retained: vec![0; words],
            aging: vec![0; words],
            retained_count: 0,
            next_aging: None,
        }
    }

    #[test]
    fn released_pages_are_reused_committed_and_age_out() {
        let mut cage = local_cage(8);
        let byte = |cage: &Cage, page: u32| {
            // SAFETY: the page lies inside the local cage's reservation.
            unsafe { cage.base.add(page as usize * PAGE_SIZE + 64) }
        };
        let pages: Vec<u32> = (0..3).map(|_| cage.take_run(1).expect("page")).collect();
        assert_eq!(pages, [1, 2, 3]);
        for &page in &pages {
            // SAFETY: as above; the page is owned by this test.
            unsafe { byte(&cage, page).write(0xa5) };
        }
        cage.retain(2, 1);
        cage.release_run(2, 1);
        // A single page comes back from the pool with its memory.
        assert_eq!(cage.take_run(1), Some(2));
        // SAFETY: as above.
        assert_eq!(unsafe { byte(&cage, 2).read() }, 0xa5);
        assert_eq!(cage.retained_count, 0);

        cage.retain(2, 2);
        cage.release_run(2, 2);
        let start = Instant::now();
        cage.age_retained(start);
        cage.age_retained(start + RETAINED_PAGE_WINDOW);
        // The first step only ages what the pool held.
        assert_eq!(cage.retained_count, 2);
        // SAFETY: as above.
        assert_eq!(unsafe { byte(&cage, 3).read() }, 0xa5);
        cage.age_retained(start + RETAINED_PAGE_WINDOW * 2);
        assert_eq!(cage.retained_count, 0);
        assert!(cage.next_aging.is_none());
        #[cfg(unix)]
        for page in [2, 3] {
            // SAFETY: as above; the remap left the range readable.
            assert_eq!(unsafe { byte(&cage, page).read() }, 0);
        }
        // A run taken across retained pages leaves none of them pooled.
        cage.retain(1, 1);
        cage.release_run(1, 1);
        assert_eq!(cage.take_run(3), Some(1));
        assert_eq!(cage.retained_count, 0);
    }

    #[test]
    fn invalid_cage_size_rejected() {
        assert!(matches!(
            init_cage_with_size(0),
            Err(CageError::InvalidSize(0))
        ));
        assert!(matches!(
            init_cage_with_size(PAGE_SIZE - 1),
            Err(CageError::InvalidSize(_))
        ));
    }
}
