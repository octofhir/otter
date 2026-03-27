//! GC object header — 8-byte fixed layout at the start of every allocation.
//!
//! Every object managed by the GC starts with a [`GcHeader`]. The header
//! encodes the object's type, size, GC generation, and forwarding state in
//! a compact 8-byte representation that fits in a single cache line fetch
//! alongside the first field of the object.
//!
//! # Layout (8 bytes, `#[repr(C)]`)
//!
//! ```text
//! ┌─────────┬─────────┬──────────┬──────────────────────────┐
//! │ type_tag │  flags  │ reserved │       size_bytes         │
//! │  (u8)   │  (u8)   │  (u16)   │        (u32)             │
//! └─────────┴─────────┴──────────┴──────────────────────────┘
//!   byte 0    byte 1   byte 2-3         byte 4-7
//! ```
//!
//! - `type_tag`: Discriminates the object variant (Object, Array, String, Closure,
//!   etc.). Used as index into the trace function table for O(1) dispatch.
//! - `flags`: Packed bitfield — mark color (2 bits), is_young (1 bit),
//!   is_forwarded (1 bit), 4 bits reserved.
//! - `size_bytes`: Total allocation size including header. Max 4 GB per object
//!   (large objects get dedicated pages anyway).

use std::sync::atomic::{AtomicU8, Ordering};

/// 8-byte GC object header at the start of every heap allocation.
///
/// Uses `AtomicU8` for the flags field to support concurrent marking
/// (multiple threads may read/write mark bits simultaneously).
#[repr(C)]
pub struct GcHeader {
    /// Object type discriminant. Index into the trace function table.
    type_tag: u8,
    /// Packed GC flags (mark color, generation, forwarding state).
    /// Atomic because concurrent marker may update mark bits while mutator reads.
    flags: AtomicU8,
    /// Reserved for future use (e.g. hash cache, age bits). Zero-initialized.
    _reserved: u16,
    /// Total allocation size in bytes (including this header).
    size_bytes: u32,
}

// Flag bit positions within the `flags` byte.
const MARK_COLOR_MASK: u8 = 0b0000_0011; // Bits 0-1: mark color
const FLAG_YOUNG: u8 = 0b0000_0100; // Bit 2: allocated in young generation
const FLAG_FORWARDED: u8 = 0b0000_1000; // Bit 3: contains forwarding pointer (scavenger)
const FLAG_PINNED: u8 = 0b0001_0000; // Bit 4: cannot be moved by compactor

/// Tri-color mark state for the marking phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum MarkColor {
    /// Not yet visited. Presumed dead unless proven reachable.
    White = 0,
    /// Discovered (in the marking worklist) but children not yet scanned.
    Gray = 1,
    /// Fully scanned — all children are gray or black.
    Black = 2,
}

impl GcHeader {
    /// Creates a new header for an object of the given type and size.
    ///
    /// The object starts white (unmarked), in old generation, not forwarded.
    #[inline]
    pub const fn new(type_tag: u8, size_bytes: u32) -> Self {
        Self {
            type_tag,
            flags: AtomicU8::new(0),
            _reserved: 0,
            size_bytes,
        }
    }

    /// Creates a new header for a young-generation (nursery) allocation.
    #[inline]
    pub const fn new_young(type_tag: u8, size_bytes: u32) -> Self {
        Self {
            type_tag,
            flags: AtomicU8::new(FLAG_YOUNG),
            _reserved: 0,
            size_bytes,
        }
    }

    // -----------------------------------------------------------------------
    // Type tag
    // -----------------------------------------------------------------------

    /// Returns the object type tag.
    #[inline]
    pub const fn type_tag(&self) -> u8 {
        self.type_tag
    }

    // -----------------------------------------------------------------------
    // Size
    // -----------------------------------------------------------------------

    /// Returns the total allocation size in bytes (including this header).
    #[inline]
    pub const fn size_bytes(&self) -> u32 {
        self.size_bytes
    }

    /// Returns the payload size (total size minus header).
    #[inline]
    pub const fn payload_size(&self) -> u32 {
        self.size_bytes.saturating_sub(HEADER_SIZE as u32)
    }

    // -----------------------------------------------------------------------
    // Mark color (atomic — safe for concurrent marking)
    // -----------------------------------------------------------------------

    /// Returns the current mark color.
    ///
    /// Uses `Acquire` ordering so that after reading Black, we are guaranteed
    /// to see all the children that were traced before the mark was set.
    #[inline]
    pub fn mark_color(&self) -> MarkColor {
        let raw = self.flags.load(Ordering::Acquire) & MARK_COLOR_MASK;
        match raw {
            0 => MarkColor::White,
            1 => MarkColor::Gray,
            2 => MarkColor::Black,
            _ => MarkColor::White, // Defensive — treat unknown as white
        }
    }

    /// Sets the mark color atomically.
    ///
    /// Uses `Release` ordering so that all tracing work done before marking
    /// Black is visible to threads that subsequently read the mark.
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

    /// Attempts to transition from White to Gray atomically.
    /// Returns `true` if successful (this thread won the race).
    /// Used by concurrent marker to avoid duplicate worklist entries.
    #[inline]
    pub fn try_mark_gray(&self) -> bool {
        let current = self.flags.load(Ordering::Acquire);
        if current & MARK_COLOR_MASK != MarkColor::White as u8 {
            return false; // Already gray or black
        }
        let new = (current & !MARK_COLOR_MASK) | (MarkColor::Gray as u8);
        self.flags
            .compare_exchange(current, new, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// Returns `true` if the object is marked (gray or black).
    #[inline]
    pub fn is_marked(&self) -> bool {
        self.flags.load(Ordering::Acquire) & MARK_COLOR_MASK != 0
    }

    /// Resets the mark color to white. Used during sweep or mark-reset.
    #[inline]
    pub fn clear_mark(&self) {
        self.flags.fetch_and(!MARK_COLOR_MASK, Ordering::Release);
    }

    // -----------------------------------------------------------------------
    // Generation flags
    // -----------------------------------------------------------------------

    /// Returns `true` if this object is in the young generation.
    #[inline]
    pub fn is_young(&self) -> bool {
        self.flags.load(Ordering::Relaxed) & FLAG_YOUNG != 0
    }

    /// Promotes the object to old generation (clears the young flag).
    #[inline]
    pub fn promote_to_old(&self) {
        self.flags.fetch_and(!FLAG_YOUNG, Ordering::Relaxed);
    }

    /// Sets the young generation flag.
    #[inline]
    pub fn set_young(&self) {
        self.flags.fetch_or(FLAG_YOUNG, Ordering::Relaxed);
    }

    // -----------------------------------------------------------------------
    // Forwarding (semi-space scavenger)
    // -----------------------------------------------------------------------

    /// Returns `true` if this object has been forwarded (replaced by a
    /// forwarding pointer during scavenge).
    #[inline]
    pub fn is_forwarded(&self) -> bool {
        self.flags.load(Ordering::Acquire) & FLAG_FORWARDED != 0
    }

    /// Marks the object as forwarded. After this, the bytes following the
    /// header contain the forwarding address (a `*const GcHeader`), not
    /// the original payload.
    #[inline]
    pub fn set_forwarded(&self) {
        self.flags.fetch_or(FLAG_FORWARDED, Ordering::Release);
    }

    /// Clears the forwarded flag.
    #[inline]
    pub fn clear_forwarded(&self) {
        self.flags.fetch_and(!FLAG_FORWARDED, Ordering::Release);
    }

    // -----------------------------------------------------------------------
    // Pinning (compactor opt-out)
    // -----------------------------------------------------------------------

    /// Returns `true` if this object is pinned (cannot be moved by compaction).
    #[inline]
    pub fn is_pinned(&self) -> bool {
        self.flags.load(Ordering::Relaxed) & FLAG_PINNED != 0
    }

    /// Pins the object, preventing it from being relocated by the compactor.
    #[inline]
    pub fn pin(&self) {
        self.flags.fetch_or(FLAG_PINNED, Ordering::Relaxed);
    }

    // -----------------------------------------------------------------------
    // Raw forwarding pointer access (for scavenger)
    // -----------------------------------------------------------------------

    /// Reads the forwarding pointer from the payload area.
    ///
    /// # Safety
    ///
    /// Only valid if `is_forwarded()` is true. The caller must ensure this
    /// header belongs to a live (not yet swept) allocation.
    #[inline]
    pub unsafe fn forwarding_address(&self) -> *const GcHeader {
        debug_assert!(self.is_forwarded());
        unsafe {
            let payload_ptr = (self as *const Self).add(1) as *const *const GcHeader;
            payload_ptr.read()
        }
    }

    /// Writes a forwarding pointer into the payload area and sets the
    /// forwarded flag.
    ///
    /// # Safety
    ///
    /// The caller must ensure the payload area is large enough to hold a
    /// pointer (guaranteed if `size_bytes >= HEADER_SIZE + size_of::<usize>()`).
    #[inline]
    pub unsafe fn set_forwarding_address(&self, target: *const GcHeader) {
        unsafe {
            let payload_ptr = (self as *const Self).add(1) as *mut *const GcHeader;
            payload_ptr.write(target);
        }
        self.set_forwarded();
    }
}

/// Size of the GC header in bytes.
pub const HEADER_SIZE: usize = std::mem::size_of::<GcHeader>();

// Static assertions
const _: () = assert!(HEADER_SIZE == 8, "GcHeader must be exactly 8 bytes");
const _: () = assert!(
    std::mem::align_of::<GcHeader>() <= 8,
    "GcHeader alignment must be at most 8 bytes"
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_is_8_bytes() {
        assert_eq!(std::mem::size_of::<GcHeader>(), 8);
    }

    #[test]
    fn new_header_is_white_old_gen() {
        let h = GcHeader::new(5, 64);
        assert_eq!(h.type_tag(), 5);
        assert_eq!(h.size_bytes(), 64);
        assert_eq!(h.payload_size(), 56);
        assert_eq!(h.mark_color(), MarkColor::White);
        assert!(!h.is_young());
        assert!(!h.is_forwarded());
        assert!(!h.is_pinned());
    }

    #[test]
    fn new_young_header() {
        let h = GcHeader::new_young(3, 32);
        assert!(h.is_young());
        assert_eq!(h.mark_color(), MarkColor::White);

        h.promote_to_old();
        assert!(!h.is_young());
    }

    #[test]
    fn mark_color_transitions() {
        let h = GcHeader::new(0, 16);
        assert_eq!(h.mark_color(), MarkColor::White);

        h.set_mark_color(MarkColor::Gray);
        assert_eq!(h.mark_color(), MarkColor::Gray);
        assert!(h.is_marked());

        h.set_mark_color(MarkColor::Black);
        assert_eq!(h.mark_color(), MarkColor::Black);
        assert!(h.is_marked());

        h.clear_mark();
        assert_eq!(h.mark_color(), MarkColor::White);
        assert!(!h.is_marked());
    }

    #[test]
    fn try_mark_gray_wins_race() {
        let h = GcHeader::new(0, 16);
        assert!(h.try_mark_gray()); // White → Gray succeeds
        assert!(!h.try_mark_gray()); // Already Gray — fails
        assert_eq!(h.mark_color(), MarkColor::Gray);
    }

    #[test]
    fn mark_color_does_not_clobber_other_flags() {
        let h = GcHeader::new_young(7, 128);
        assert!(h.is_young());

        h.set_mark_color(MarkColor::Black);
        assert!(h.is_young()); // Young flag preserved
        assert_eq!(h.mark_color(), MarkColor::Black);

        h.promote_to_old();
        assert!(!h.is_young());
        assert_eq!(h.mark_color(), MarkColor::Black); // Mark preserved
    }

    #[test]
    fn forwarding_pointer_round_trip() {
        // Allocate a fake object with enough room for a forwarding pointer.
        let mut buf = [0u8; 24]; // 8 (header) + 8 (forwarding ptr) + 8 (padding)
        let header = unsafe { &mut *(buf.as_mut_ptr() as *mut GcHeader) };
        *header = GcHeader::new(0, 24);

        let target = GcHeader::new(1, 16);
        let target_ptr: *const GcHeader = &target;

        unsafe {
            header.set_forwarding_address(target_ptr);
        }

        assert!(header.is_forwarded());
        let recovered = unsafe { header.forwarding_address() };
        assert_eq!(recovered, target_ptr);
    }

    #[test]
    fn pinning() {
        let h = GcHeader::new(0, 16);
        assert!(!h.is_pinned());

        h.pin();
        assert!(h.is_pinned());

        // Pinning doesn't affect mark or generation
        assert_eq!(h.mark_color(), MarkColor::White);
        assert!(!h.is_young());
    }
}
