//! Pointer compression: the heap cage and 32-bit `Gc<T>` handles.
//!
//! Every page allocation in the GC is carved from a process-global
//! virtual cage. All GC pointers are 32-bit offsets relative to the
//! cage base, decompressed to a real `*mut u8` via
//! `cage_base + offset`. This halves heap-pointer footprint vs. raw
//! 64-bit pointers and matches V8's sandbox shape (V8 blog,
//! 2020-03 "Pointer Compression in V8").
//!
//! # Contents
//!
//! - [`Gc<T>`] — opaque 32-bit compressed pointer, `repr(transparent)`.
//! - [`RawGc`] — typeless slot value (`u32`), used by trace
//!   functions that walk slots polymorphically.
//! - [`init_cage_with_size`] — debug-only init API: tests/benches
//!   override the default cage size before any heap is created.
//! - [`Cage`] — process-global cage owning the 4 GiB virtual region
//!   and the page free-list.
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
//!
//! # See also
//!
//! - GC architecture plan §1.2 NF9 (pointer-compression invariants),
//!   §2.3 ("Pointer compression — add in Phase 1").
//! - V8 pointer compression: <https://v8.dev/blog/pointer-compression>.

use std::alloc::{Layout, alloc, dealloc};
use std::marker::PhantomData;
use std::sync::Mutex;
use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

use crate::page::PAGE_SIZE;

/// Default cage size when [`init_cage_with_size`] has not been
/// called: 256 MiB on 64-bit hosts.
///
/// Production embedders that want the full V8-shaped 4 GiB cage
/// must call `init_cage_with_size(4 * 1024 * 1024 * 1024)` before
/// the first [`crate::heap::GcHeap::new`].
pub const DEFAULT_CAGE_SIZE_BYTES: usize = 256 * 1024 * 1024;

/// Maximum cage size — V8's compressed-pointer cap. `Gc<T>` is a
/// `u32`, so the cage cannot exceed `u32::MAX + 1` bytes.
pub const MAX_CAGE_SIZE_BYTES: usize = 1usize << 32;

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
/// cage has been initialised. Reads with `Acquire` ordering — the
/// matching `Release` store happens inside [`Cage::ensure_inner`].
///
/// Decompression of a `Gc<T>` offset goes through
/// `cage_base().add(offset as usize)` so pointer provenance is
/// preserved (Stacked / Tree Borrows clean).
#[inline]
pub fn cage_base() -> *mut u8 {
    CAGE_BASE.load(Ordering::Acquire)
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
}

/// Return cage occupancy diagnostics after initialisation.
#[inline]
#[must_use]
pub fn cage_stats() -> Option<CageStats> {
    Cage::stats()
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
    /// Cage base pointer. Lives until process exit.
    base: *mut u8,
    /// Cage size in bytes (multiple of [`PAGE_SIZE`]).
    size: usize,
    /// Free page indices (page 0 is reserved so that `Gc<T>(0)`
    /// stays null). Indices count from 1 to `size / PAGE_SIZE - 1`.
    free_pages: Vec<u32>,
    /// Total page count (`size / PAGE_SIZE`).
    page_count: u32,
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
        // SAFETY: Layout is built from a non-zero multiple of
        // PAGE_SIZE that we just validated; `alloc` obtains a
        // page-aligned region without eagerly zeroing the whole
        // default cage. Individual pages initialise their headers
        // in `Page::new`, and returned pages are zeroed before
        // reuse in `free_page`.
        let layout = Layout::from_size_align(size_bytes, PAGE_SIZE)
            .map_err(|_| CageError::InvalidSize(size_bytes))?;
        let ptr = unsafe { alloc(layout) };
        if ptr.is_null() {
            return Err(CageError::AllocFailed);
        }
        let page_count = (size_bytes / PAGE_SIZE) as u32;
        // Page 0 is reserved so that Gc<T>(0) stays null.
        let free_pages: Vec<u32> = (1..page_count).rev().collect();
        let cage = Cage {
            base: ptr,
            size: size_bytes,
            free_pages,
            page_count,
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
        let idx = cage.free_pages.pop()?;
        // SAFETY: `idx < page_count` by construction of `free_pages`,
        // and `cage.base + idx * PAGE_SIZE` is therefore inside the
        // cage region.
        let page_ptr = unsafe { cage.base.add(idx as usize * PAGE_SIZE) };
        Some(CagePage {
            base: page_ptr,
            offset: idx * (PAGE_SIZE as u32),
        })
    }

    /// Return a page to the cage free-list.
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
        // Zero the page so reuse always sees a fresh PageHeader.
        // SAFETY: `idx` is in-range, page memory belongs to the
        // cage and is owned by the cage allocator (we hold the
        // mutex).
        unsafe {
            core::ptr::write_bytes(cage.base.add(idx as usize * PAGE_SIZE), 0, PAGE_SIZE);
        }
        cage.free_pages.push(idx);
    }

    /// Return the cage's free-page count for diagnostics.
    #[allow(dead_code)]
    pub(crate) fn free_page_count() -> usize {
        let guard = CAGE_GUARD.lock().expect("cage mutex poisoned");
        guard.as_ref().map(|c| c.free_pages.len()).unwrap_or(0)
    }

    /// Number of pages the cage carries in total (size / PAGE_SIZE).
    /// Kept `pub(crate)` for diagnostics — used by integration
    /// tests that assert on cage occupancy.
    #[allow(dead_code)]
    pub(crate) fn total_page_count() -> u32 {
        let guard = CAGE_GUARD.lock().expect("cage mutex poisoned");
        guard.as_ref().map(|c| c.page_count).unwrap_or(0)
    }

    pub(crate) fn stats() -> Option<CageStats> {
        let guard = CAGE_GUARD.lock().expect("cage mutex poisoned");
        guard.as_ref().map(|c| {
            let free_pages = c.free_pages.len();
            let reserved_pages = usize::from(c.page_count > 0);
            let allocated_pages = (c.page_count as usize)
                .saturating_sub(free_pages)
                .saturating_sub(reserved_pages);
            CageStats {
                size_bytes: c.size,
                total_pages: c.page_count,
                free_pages,
                allocated_pages,
            }
        })
    }
}

impl Drop for Cage {
    fn drop(&mut self) {
        // Cage lives for process lifetime; this Drop only fires if
        // the static guard is reset (test harness teardown). Free
        // the backing region so leak sanitiser is happy.
        if !self.base.is_null() {
            // SAFETY: layout matches the one used in
            // `ensure_inner`; the pointer was returned by `alloc`
            // and has not been freed yet.
            let layout = Layout::from_size_align(self.size, PAGE_SIZE)
                .expect("cage layout was valid at init");
            unsafe {
                dealloc(self.base, layout);
            }
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
