//! Backing bytes of a non-shared `ArrayBuffer`.
//!
//! Most buffers own a plain heap allocation. A fixed-length buffer shaped
//! like a large asm.js heap (a multiple of 16 MiB, at most 2 GiB) is mapped
//! at the start of a 4 GiB virtual reservation instead: the bytes past the
//! buffer read as zero and an inaccessible guard follows. That is the layout
//! under which every 32-bit WebAssembly access needs no bounds check (V8
//! reserves wasm memory the same way), so a linked asm.js module aliases the
//! buffer as its memory, and a read past the end yields the `0` a typed-array
//! miss coerces to.
//!
//! # Contents
//! - [`ByteStorage`] — the heap or reserved bytes of one buffer.
//! - [`RESERVED_SPAN`] / [`RESERVED_GUARD`] — the reserved layout.
//!
//! # Invariants
//! - Reserved storage: `[base, base + len)` is readable and writable,
//!   `[base + len, base + RESERVED_SPAN)` is read-only zero pages, and
//!   `[base + RESERVED_SPAN, + RESERVED_GUARD)` is inaccessible. It never
//!   moves or resizes; clearing it unmaps the whole reservation.
//! - Heap storage may resize (resizable buffers); reserved storage is only
//!   ever created for fixed-length buffers.
//!
//! # See also
//! - [`super::array_buffer`] — the buffer body that owns this storage.

/// Virtual span reserved for a large buffer: the whole 32-bit address range.
pub const RESERVED_SPAN: usize = 1 << 32;
/// Inaccessible bytes after [`RESERVED_SPAN`], covering an 8-byte access at
/// the last 32-bit address.
pub const RESERVED_GUARD: usize = 1 << 16;

/// The bytes of one non-shared buffer.
#[derive(Debug)]
pub(crate) enum ByteStorage {
    /// An ordinary allocation.
    Heap(Vec<u8>),
    /// A large fixed-length buffer at the start of a reservation.
    Reserved(Reservation),
}

impl From<Vec<u8>> for ByteStorage {
    fn from(bytes: Vec<u8>) -> Self {
        Self::Heap(bytes)
    }
}

impl ByteStorage {
    /// `len` zero bytes for a fixed-length buffer: reserved when the length
    /// is a large asm.js heap length and the platform maps reservations,
    /// otherwise `None` so the caller allocates on the heap.
    pub(crate) fn reserved_zeroed(len: usize) -> Option<Self> {
        if !(len >= 1 << 24 && len % (1 << 24) == 0 && len <= 1 << 31) {
            return None;
        }
        Reservation::map(len).map(Self::Reserved)
    }

    pub(crate) fn as_slice(&self) -> &[u8] {
        match self {
            Self::Heap(bytes) => bytes,
            // SAFETY: `[base, base + len)` is mapped readable and writable
            // for the reservation's lifetime.
            Self::Reserved(reservation) => unsafe {
                std::slice::from_raw_parts(reservation.base, reservation.len)
            },
        }
    }

    pub(crate) fn as_mut_slice(&mut self) -> &mut [u8] {
        match self {
            Self::Heap(bytes) => bytes,
            // SAFETY: as in `as_slice`; `&mut self` makes the borrow unique.
            Self::Reserved(reservation) => unsafe {
                std::slice::from_raw_parts_mut(reservation.base, reservation.len)
            },
        }
    }

    /// `true` for the reserved layout described in the module docs.
    pub(crate) fn is_reserved(&self) -> bool {
        matches!(self, Self::Reserved(_))
    }

    /// Release the bytes (detach).
    pub(crate) fn clear(&mut self) {
        *self = Self::Heap(Vec::new());
    }

    /// Resize heap storage, zero-filling growth. Reserved storage is
    /// fixed-length and never reaches here.
    pub(crate) fn resize(&mut self, new_len: usize) {
        match self {
            Self::Heap(bytes) => bytes.resize(new_len, 0),
            Self::Reserved(_) => unreachable!("reserved storage is fixed-length"),
        }
    }
}

/// A mapped reservation holding `len` buffer bytes at its base.
#[derive(Debug)]
pub(crate) struct Reservation {
    base: *mut u8,
    len: usize,
}

impl Reservation {
    #[cfg(all(unix, target_pointer_width = "64"))]
    fn map(len: usize) -> Option<Self> {
        let total = RESERVED_SPAN + RESERVED_GUARD;
        #[cfg(target_os = "linux")]
        let flags = libc::MAP_PRIVATE | libc::MAP_ANON | libc::MAP_NORESERVE;
        #[cfg(not(target_os = "linux"))]
        let flags = libc::MAP_PRIVATE | libc::MAP_ANON;
        // SAFETY: a fresh anonymous mapping; protections are then narrowed
        // to the layout in the module docs before the base escapes.
        unsafe {
            let base = libc::mmap(std::ptr::null_mut(), total, libc::PROT_NONE, flags, -1, 0);
            if base == libc::MAP_FAILED {
                return None;
            }
            if libc::mprotect(base, RESERVED_SPAN, libc::PROT_READ) != 0
                || libc::mprotect(base, len, libc::PROT_READ | libc::PROT_WRITE) != 0
            {
                libc::munmap(base, total);
                return None;
            }
            Some(Self {
                base: base.cast(),
                len,
            })
        }
    }

    #[cfg(not(all(unix, target_pointer_width = "64")))]
    fn map(_len: usize) -> Option<Self> {
        None
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        #[cfg(all(unix, target_pointer_width = "64"))]
        // SAFETY: the reservation was mapped by `map` with exactly this span
        // and nothing borrows it once its owner is dropped.
        unsafe {
            libc::munmap(self.base.cast(), RESERVED_SPAN + RESERVED_GUARD);
        }
    }
}
