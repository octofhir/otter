//! 8-byte GC object header.
//!
//! Every GC-managed allocation in the cage starts with a
//! [`GcHeader`]. The header is 8 bytes — a u8 type tag, an atomic
//! u8 flag byte (mark color, young flag, forwarded flag, pinned
//! flag), 2 reserved bytes, and a u32 size — chosen to fit in a
//! single cache-line fetch alongside the object's first field.
//!
//! # Contents
//!
//! - [`GcHeader`] — the fixed 8-byte prefix on every allocation.
//! - [`MarkColor`] — tri-color marker state (White / Gray / Black).
//! - [`HEADER_SIZE`] — `size_of::<GcHeader>()` (== 8).
//!
//! # Invariants
//!
//! - `size_of::<GcHeader>() == 8` (static assertion).
//! - `align_of::<GcHeader>() <= 8` so it fits at the start of a
//!   cell-aligned allocation.
//! - `flags` is `AtomicU8` so the marker and mutator can race on
//!   mark transitions when incremental marking lands (Phase 2);
//!   today the GC is STW so atomics serve as a forward-compatible
//!   capability.
//! - When the forwarded flag is set the first 8 bytes after the
//!   header carry a forwarding offset (`u32`) — this is enforced by
//!   [`crate::scavenger`] during evacuation, which in turn
//!   guarantees the minimum payload size on every alloc that ever
//!   reaches young-gen.
//!
//! # See also
//!
//! - GC architecture plan §2.3 ("`GcHeader` reproduce in Phase 1")
//!   and §5 (write barriers, mark transitions).

use std::sync::atomic::{AtomicU8, Ordering};

const MARK_COLOR_MASK: u8 = 0b0000_0011;
const FLAG_YOUNG: u8 = 0b0000_0100;

/// Byte offset of the [`GcHeader`] flag byte from the header base. The
/// generational write barrier emitted in compiled code reads this byte to test
/// the young flag without a Rust call.
pub const HEADER_FLAGS_BYTE_OFFSET: usize = 1;

/// The young-generation flag bit within the [`GcHeader`] flag byte
/// ([`HEADER_FLAGS_BYTE_OFFSET`]). Exposed so the JIT can emit an inline
/// generational write barrier (`flags & GENERATION_YOUNG_FLAG`).
pub const GENERATION_YOUNG_FLAG: u8 = FLAG_YOUNG;
const FLAG_FORWARDED: u8 = 0b0000_1000;
const FLAG_PINNED: u8 = 0b0001_0000;
/// Set once the sweeper has finalized + dropped a dead old/large-space
/// object. Old-space is non-moving and reaps memory only at whole-page
/// granularity, so a dead corpse stays in its page's bump range and is
/// re-walked by every later sweep. Without this flag a subsequent sweep
/// would `drop_in_place` the same payload twice — a double free of any
/// owned buffer (e.g. a string body's `Vec<u16>`). The bit survives
/// `clear_mark` (which only touches the color bits) so the drop is
/// idempotent across GC cycles.
const FLAG_SWEPT: u8 = 0b0010_0000;

/// Tri-color marker state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum MarkColor {
    /// Unvisited; presumed dead unless proven reachable.
    White = 0,
    /// In the worklist; children not yet scanned.
    Gray = 1,
    /// Fully scanned; all children are gray or black.
    Black = 2,
}

/// 8-byte GC object header at the start of every cage allocation.
#[repr(C)]
pub struct GcHeader {
    type_tag: u8,
    flags: AtomicU8,
    _reserved: u16,
    size_bytes: u32,
}

impl GcHeader {
    /// Build a fresh old-generation header with the given type tag
    /// and total allocation size (header + payload).
    #[inline]
    pub const fn new(type_tag: u8, size_bytes: u32) -> Self {
        Self {
            type_tag,
            flags: AtomicU8::new(0),
            _reserved: 0,
            size_bytes,
        }
    }

    /// Build a fresh young-generation header.
    #[inline]
    pub const fn new_young(type_tag: u8, size_bytes: u32) -> Self {
        Self {
            type_tag,
            flags: AtomicU8::new(FLAG_YOUNG),
            _reserved: 0,
            size_bytes,
        }
    }

    /// Build a fresh young-generation header marked black, used by
    /// the black-allocation path while a marking cycle is in
    /// progress (V8 standard since 2018).
    #[inline]
    pub const fn new_young_black(type_tag: u8, size_bytes: u32) -> Self {
        Self {
            type_tag,
            flags: AtomicU8::new(FLAG_YOUNG | (MarkColor::Black as u8)),
            _reserved: 0,
            size_bytes,
        }
    }

    /// Returns the object type tag (index into the trace table).
    #[inline]
    pub const fn type_tag(&self) -> u8 {
        self.type_tag
    }

    /// Returns the total allocation size (header + payload).
    #[inline]
    pub const fn size_bytes(&self) -> u32 {
        self.size_bytes
    }

    /// Returns the payload size (total minus the header).
    #[inline]
    pub const fn payload_size(&self) -> u32 {
        self.size_bytes.saturating_sub(HEADER_SIZE as u32)
    }

    /// Returns the current mark color.
    #[inline]
    pub fn mark_color(&self) -> MarkColor {
        match self.flags.load(Ordering::Acquire) & MARK_COLOR_MASK {
            0 => MarkColor::White,
            1 => MarkColor::Gray,
            2 => MarkColor::Black,
            _ => MarkColor::White,
        }
    }

    /// Atomically set the mark color preserving the other flags.
    #[inline]
    pub fn set_mark_color(&self, color: MarkColor) {
        let mut current = self.flags.load(Ordering::Relaxed);
        loop {
            let new = (current & !MARK_COLOR_MASK) | (color as u8);
            match self.flags.compare_exchange_weak(
                current,
                new,
                Ordering::Release,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => current = actual,
            }
        }
    }

    /// Returns true iff the mark color is gray or black.
    #[inline]
    pub fn is_marked(&self) -> bool {
        self.flags.load(Ordering::Acquire) & MARK_COLOR_MASK != 0
    }

    /// Reset the mark color to white. Used at the end of a sweep
    /// or to prepare for a new mark phase.
    #[inline]
    pub fn clear_mark(&self) {
        self.flags.fetch_and(!MARK_COLOR_MASK, Ordering::Release);
    }

    /// True iff the object lives in young generation.
    #[inline]
    pub fn is_young(&self) -> bool {
        self.flags.load(Ordering::Relaxed) & FLAG_YOUNG != 0
    }

    /// True iff the object lives in old generation (the dual of
    /// [`Self::is_young`]).
    #[inline]
    pub fn is_old(&self) -> bool {
        !self.is_young()
    }

    /// Promote the object to old gen (clears the young flag).
    #[inline]
    pub fn promote_to_old(&self) {
        self.flags.fetch_and(!FLAG_YOUNG, Ordering::Relaxed);
    }

    /// Set the young flag.
    #[inline]
    pub fn set_young(&self) {
        self.flags.fetch_or(FLAG_YOUNG, Ordering::Relaxed);
    }

    /// True iff the object has been forwarded by the scavenger.
    /// When this is set the first u32 of the payload holds the
    /// new compressed offset.
    #[inline]
    pub fn is_forwarded(&self) -> bool {
        self.flags.load(Ordering::Acquire) & FLAG_FORWARDED != 0
    }

    /// Mark the object as forwarded.
    #[inline]
    pub fn set_forwarded(&self) {
        self.flags.fetch_or(FLAG_FORWARDED, Ordering::Release);
    }

    /// True iff the sweeper has already finalized + dropped this dead
    /// object. Guards against a second `drop_in_place` on a later
    /// sweep (see [`FLAG_SWEPT`]).
    #[inline]
    pub fn is_swept(&self) -> bool {
        self.flags.load(Ordering::Acquire) & FLAG_SWEPT != 0
    }

    /// Record that the sweeper has finalized + dropped this object.
    #[inline]
    pub fn set_swept(&self) {
        self.flags.fetch_or(FLAG_SWEPT, Ordering::Release);
    }

    /// Returns true iff the object is pinned (future compactor
    /// opt-out; not used by the Phase-1 scavenger which doesn't
    /// reorder old-gen).
    #[inline]
    pub fn is_pinned(&self) -> bool {
        self.flags.load(Ordering::Relaxed) & FLAG_PINNED != 0
    }

    /// Read the forwarding offset from the first u32 of the
    /// payload area.
    ///
    /// # Safety
    ///
    /// `this` must be a valid `GcHeader` pointer derived from the
    /// cage allocation (so provenance covers header + payload),
    /// and `(*this).is_forwarded()` must be true.
    #[inline]
    pub unsafe fn read_forwarding_offset(this: *const Self) -> u32 {
        // SAFETY: by precondition `this` carries cage-allocation
        // provenance that covers the next 4 payload bytes; the
        // forwarded bit was set, so those bytes hold a valid u32.
        unsafe {
            debug_assert!((*this).is_forwarded());
            let payload_ptr = this.add(1) as *const u32;
            payload_ptr.read()
        }
    }

    /// Write a forwarding offset into the payload area and set
    /// the forwarded flag.
    ///
    /// # Safety
    ///
    /// `this` must be a valid `GcHeader` pointer derived from the
    /// cage allocation; the payload area must be at least 4
    /// bytes; the caller must hold exclusive access (Phase-1 GC
    /// is STW, so this is the scavenger thread).
    #[inline]
    pub unsafe fn write_forwarding_offset(this: *mut Self, target_offset: u32) {
        // SAFETY: `this` carries cage-allocation provenance and
        // the bytes immediately after the header are within the
        // same allocation.
        unsafe {
            let payload_ptr = this.add(1) as *mut u32;
            payload_ptr.write(target_offset);
            (*this).set_forwarded();
        }
    }
}

/// Size of [`GcHeader`] in bytes (== 8).
pub const HEADER_SIZE: usize = std::mem::size_of::<GcHeader>();

const _: () = assert!(HEADER_SIZE == 8, "GcHeader must be exactly 8 bytes");
const _: () = assert!(
    std::mem::align_of::<GcHeader>() <= 8,
    "GcHeader alignment must be <= 8 bytes"
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_is_8_bytes() {
        assert_eq!(HEADER_SIZE, 8);
    }

    #[test]
    fn fresh_header_is_white_old_gen() {
        let h = GcHeader::new(7, 64);
        assert_eq!(h.type_tag(), 7);
        assert_eq!(h.size_bytes(), 64);
        assert_eq!(h.payload_size(), 56);
        assert_eq!(h.mark_color(), MarkColor::White);
        assert!(!h.is_young());
        assert!(!h.is_forwarded());
    }

    #[test]
    fn young_header_carries_young_flag() {
        let h = GcHeader::new_young(3, 32);
        assert!(h.is_young());
        h.promote_to_old();
        assert!(!h.is_young());
    }

    #[test]
    fn mark_color_round_trip() {
        let h = GcHeader::new_young(0, 16);
        h.set_mark_color(MarkColor::Gray);
        assert_eq!(h.mark_color(), MarkColor::Gray);
        h.set_mark_color(MarkColor::Black);
        assert_eq!(h.mark_color(), MarkColor::Black);
        assert!(h.is_young(), "young flag preserved across mark color");
        h.clear_mark();
        assert_eq!(h.mark_color(), MarkColor::White);
    }

    #[test]
    fn black_alloc_constructor() {
        let h = GcHeader::new_young_black(0, 16);
        assert!(h.is_young());
        assert_eq!(h.mark_color(), MarkColor::Black);
    }

    #[test]
    fn forwarding_offset_round_trip() {
        // Allocate a small backing buffer with header + 4-byte
        // forwarding slot; we write into the buffer rather than
        // using a real cage page so this is purely a layout test.
        let mut buf = [0u8; 16];
        let header_ptr = buf.as_mut_ptr() as *mut GcHeader;
        // SAFETY: 16-byte buffer is large enough for header +
        // forwarding u32; we hold exclusive access via &mut buf.
        unsafe { std::ptr::write(header_ptr, GcHeader::new(0, 16)) };
        unsafe { GcHeader::write_forwarding_offset(header_ptr, 0xCAFE_BABE) };
        assert!(unsafe { (*header_ptr).is_forwarded() });
        assert_eq!(
            unsafe { GcHeader::read_forwarding_offset(header_ptr) },
            0xCAFE_BABE
        );
    }
}
