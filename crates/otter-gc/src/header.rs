//! 8-byte GC object header.
//!
//! Every GC-managed allocation in the cage starts with a
//! [`GcHeader`]. The header is 8 bytes — a u8 type tag, an atomic
//! u8 flag byte (mark color, young flag, forwarded flag, pinned
//! flag, swept flag, remembered flag, scavenger copying flag), a young
//! cell's u16 allocation
//! site, and a u32
//! size — chosen to fit in a single cache-line fetch alongside the
//! object's first field.
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
//! - `flags` is `AtomicU8`: scavenger threads claim cells through it,
//!   and the marker and mutator race on mark transitions.
//! - When the forwarded flag is set the first 8 bytes after the
//!   header carry a forwarding offset (`u32`) — this is enforced by
//!   [`crate::scavenger`] during evacuation, which in turn
//!   guarantees the minimum payload size on every alloc that ever
//!   reaches young-gen.
//! - Scavenger threads claim a young cell by setting the forwarded and
//!   copying bits in one compare-and-swap of the flag byte; the claimer
//!   copies the cell, writes the forwarding offset and clears the copying
//!   bit with release ordering. Any other thread reads the offset only after
//!   an acquire load sees the copying bit clear.
//! - Old and large cells are never forwarded, so they reuse the copying bit
//!   as a layout lock: the concurrent marker holds it while it reads a body
//!   whose pointer words the mutator may rewrite in place, and the mutator
//!   holds it for that rewrite while a marking cycle runs.
//!
//! # See also
//!
//! - [`crate::barrier`] (write barriers) and [`crate::scavenger`]
//!   (forwarding + mark transitions).

use std::sync::atomic::{AtomicU8, Ordering};

const MARK_COLOR_MASK: u8 = 0b0000_0011;

/// The mark-color bits within the [`GcHeader`] flag byte: zero for white.
/// Compiled code's insertion barrier sends only a white old child to the
/// runtime.
pub const MARK_COLOR_BITS: u8 = MARK_COLOR_MASK;
const FLAG_YOUNG: u8 = 0b0000_0100;

/// Byte offset of the [`GcHeader`] flag byte from the header base. The
/// generational write barrier emitted in compiled code reads this byte to test
/// the young flag without a Rust call.
pub const HEADER_FLAGS_BYTE_OFFSET: usize = 1;

/// Byte offset of the `u32` cell size (header included) inside a header,
/// read by generated code that publishes a cell it sized itself.
pub const HEADER_SIZE_BYTES_OFFSET: usize = 4;

/// Byte offset of the two header bytes after the flags. A young cell
/// carries its allocation site id there (little-endian, zero for none); its
/// first evacuation counts and clears it, so an old cell holds zero. No body
/// type stores data in them.
pub const HEADER_BODY_BYTES_OFFSET: usize = 2;

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

/// Set on an old/large parent once it has been recorded in the per-isolate
/// remembered set after an old→young store. This is the dedup bit for the
/// object-granular remembered set: the barrier pushes a parent into the store
/// buffer at most once between scavenges, and the scavenge that consumes the
/// buffer clears the bit before re-tracing so a parent that still holds an
/// old→young edge after evacuation is re-pushed. Outside the mark-color mask,
/// so it survives [`GcHeader::clear_mark`].
const FLAG_REMEMBERED: u8 = 0b0100_0000;

/// The remembered-set dedup bit within the [`GcHeader`] flag byte
/// ([`HEADER_FLAGS_BYTE_OFFSET`]). Exposed as a frozen constant so the future
/// JIT can lower the generational barrier as `load parent.flags; test
/// REMEMBERED_FLAG; if old ∧ young-child ∧ ¬remembered → cold-call push`.
pub const REMEMBERED_FLAG: u8 = FLAG_REMEMBERED;

// Frozen flag-byte assignment. The inline generational barrier the optimizing
// tier will emit tests the young and remembered bits at a fixed mask, and the
// flag byte sits at a fixed offset in the header, so pin every bit. A reorder
// or overlap is a compile error rather than a barrier that tests the wrong bit.
const _: () = assert!(MARK_COLOR_MASK == 0b0000_0011);
const _: () = assert!(FLAG_YOUNG == 0b0000_0100);
const _: () = assert!(FLAG_FORWARDED == 0b0000_1000);
const _: () = assert!(FLAG_PINNED == 0b0001_0000);
const _: () = assert!(FLAG_SWEPT == 0b0010_0000);
const _: () = assert!(FLAG_REMEMBERED == 0b0100_0000);
const _: () = assert!(HEADER_FLAGS_BYTE_OFFSET == 1);
// Each flag is a single distinct bit, none overlapping another or the 2-bit
// mark-color field — so a flag mutation never disturbs the color and vice
// versa. The OR of all five single-bit flags equals their sum iff they are
// pairwise disjoint.
const _: () = assert!(
    FLAG_YOUNG + FLAG_FORWARDED + FLAG_PINNED + FLAG_SWEPT + FLAG_REMEMBERED
        == (FLAG_YOUNG | FLAG_FORWARDED | FLAG_PINNED | FLAG_SWEPT | FLAG_REMEMBERED)
);
const _: () = assert!(
    (FLAG_YOUNG | FLAG_FORWARDED | FLAG_PINNED | FLAG_SWEPT | FLAG_REMEMBERED) & MARK_COLOR_MASK
        == 0
);
// `clear_mark` clears only the color field, so the swept and remembered bits
// survive it — load-bearing for idempotent sweep and for a remembered parent
// staying recorded across an old-gen mark reset.
const _: () = assert!(FLAG_SWEPT & MARK_COLOR_MASK == 0);
const _: () = assert!(FLAG_REMEMBERED & MARK_COLOR_MASK == 0);
/// Set together with the forwarded bit while one scavenger thread copies a
/// claimed young cell: its forwarding offset is not written yet. Never set
/// outside a scavenge.
const FLAG_COPYING: u8 = 0b1000_0000;
/// The same bit on an old or large cell, which is never forwarded: held
/// while one thread reads or rewrites a body whose pointer words change in
/// place (see [`GcHeader::lock_layout`]).
const FLAG_LAYOUT_LOCKED: u8 = FLAG_COPYING;
const _: () = assert!(
    (MARK_COLOR_MASK
        | FLAG_YOUNG
        | FLAG_FORWARDED
        | FLAG_PINNED
        | FLAG_SWEPT
        | FLAG_REMEMBERED
        | FLAG_COPYING)
        == 0b1111_1111
);
const _: () = assert!(FLAG_COPYING & (MARK_COLOR_MASK | FLAG_REMEMBERED | FLAG_FORWARDED) == 0);

/// Outcome of claiming a young cell for evacuation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ForwardingClaim {
    /// This thread copies the cell; the flag byte it had before the claim.
    Claimed(u8),
    /// The cell already lives at this offset.
    Forwarded(u32),
}

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
    /// Visited once this cycle, then changed by the mutator through a store
    /// the barrier did not see: it waits for a second visit, which does not
    /// count its bytes again.
    Regray = 3,
}

/// 8-byte GC object header at the start of every cage allocation.
#[repr(C)]
pub struct GcHeader {
    type_tag: u8,
    flags: AtomicU8,
    body_bytes: [u8; 2],
    size_bytes: u32,
}

const _: () = assert!(std::mem::offset_of!(GcHeader, body_bytes) == HEADER_BODY_BYTES_OFFSET);

const _: () = assert!(std::mem::offset_of!(GcHeader, size_bytes) == HEADER_SIZE_BYTES_OFFSET);

/// Type tag reserved for free-space fillers in old-space pages. A filler
/// header keeps the page's linear object walk intact over reclaimed
/// ranges; it is never traced, never dropped, and never registered in the
/// trace table.
pub const FREE_TAG: u8 = u8::MAX;

impl GcHeader {
    /// Build a free-space filler header covering `size_bytes` (header
    /// included). Old-generation, white, no flags: the sweeper treats it
    /// as reclaimable space and the marker can never reach it.
    #[inline]
    pub const fn new_free(size_bytes: u32) -> Self {
        Self {
            type_tag: FREE_TAG,
            flags: AtomicU8::new(0),
            body_bytes: [0; 2],
            size_bytes,
        }
    }

    /// Build a fresh old-generation header with the given type tag
    /// and total allocation size (header + payload).
    #[inline]
    pub const fn new(type_tag: u8, size_bytes: u32) -> Self {
        Self {
            type_tag,
            flags: AtomicU8::new(0),
            body_bytes: [0; 2],
            size_bytes,
        }
    }

    /// Build a fresh young-generation header.
    #[inline]
    pub const fn new_young(type_tag: u8, size_bytes: u32) -> Self {
        Self {
            type_tag,
            flags: AtomicU8::new(FLAG_YOUNG),
            body_bytes: [0; 2],
            size_bytes,
        }
    }

    /// Build a fresh young-generation header tagged with an allocation site.
    #[inline]
    pub const fn new_young_at_site(type_tag: u8, size_bytes: u32, site: u16) -> Self {
        Self {
            type_tag,
            flags: AtomicU8::new(FLAG_YOUNG),
            body_bytes: site.to_le_bytes(),
            size_bytes,
        }
    }

    /// Allocation site a young cell was tagged with; zero for none.
    #[inline]
    pub const fn allocation_site(&self) -> u16 {
        u16::from_le_bytes(self.body_bytes)
    }

    /// Build a fresh young-generation header marked black, used by
    /// the black-allocation path while a marking cycle is in
    /// progress (a freshly published object is never traced this cycle).
    #[inline]
    pub const fn new_young_black(type_tag: u8, size_bytes: u32) -> Self {
        Self {
            type_tag,
            flags: AtomicU8::new(FLAG_YOUNG | (MarkColor::Black as u8)),
            body_bytes: [0; 2],
            size_bytes,
        }
    }

    /// Returns the object type tag (index into the trace table).
    #[inline]
    pub const fn type_tag(&self) -> u8 {
        self.type_tag
    }

    /// Tag a young cell with its allocation site; zero clears the tag.
    ///
    /// # Safety
    /// `this` must address the header of a live cell the collector owns at
    /// this point: a fresh allocation before publication, or an evacuated copy.
    #[inline]
    pub(crate) unsafe fn set_allocation_site(this: *mut Self, site: u16) {
        // SAFETY: caller contract; the field is a plain byte pair distinct
        // from the atomic flag byte.
        unsafe { std::ptr::addr_of_mut!((*this).body_bytes).write(site.to_le_bytes()) };
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
            _ => MarkColor::Regray,
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

    /// Paint a white header gray; `false` when it was already gray or
    /// black. Of several threads shading one header, exactly one wins.
    #[inline]
    pub fn try_shade_gray(&self) -> bool {
        let mut current = self.flags.load(Ordering::Relaxed);
        loop {
            if current & MARK_COLOR_MASK != MarkColor::White as u8 {
                return false;
            }
            match self.flags.compare_exchange_weak(
                current,
                current | MarkColor::Gray as u8,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(actual) => current = actual,
            }
        }
    }

    /// Claim a gray or re-grayed header for its visit by painting it black
    /// first, so a mutator store racing the visit finds it black and re-grays
    /// it. `Some(true)` for its first visit this cycle, `Some(false)` for a
    /// re-gray's, `None` when it is not waiting for a visit.
    #[inline]
    pub fn try_begin_visit(&self) -> Option<bool> {
        let mut current = self.flags.load(Ordering::Relaxed);
        loop {
            let first = match current & MARK_COLOR_MASK {
                c if c == MarkColor::Gray as u8 => true,
                c if c == MarkColor::Regray as u8 => false,
                _ => return None,
            };
            match self.flags.compare_exchange_weak(
                current,
                (current & !MARK_COLOR_MASK) | MarkColor::Black as u8,
                Ordering::SeqCst,
                Ordering::Relaxed,
            ) {
                Ok(_) => return Some(first),
                Err(actual) => current = actual,
            }
        }
    }

    /// Paint a black header re-gray; `false` when it was not black.
    #[inline]
    pub fn try_regray(&self) -> bool {
        let mut current = self.flags.load(Ordering::Relaxed);
        loop {
            if current & MARK_COLOR_MASK != MarkColor::Black as u8 {
                return false;
            }
            match self.flags.compare_exchange_weak(
                current,
                current | MarkColor::Regray as u8,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(actual) => current = actual,
            }
        }
    }

    /// Take the layout lock of an old or large cell, waiting while another
    /// thread holds it. Pairs with [`Self::unlock_layout`]; the acquire
    /// ordering makes the previous holder's rewrite visible.
    #[inline]
    pub fn lock_layout(&self) {
        debug_assert!(!self.is_young());
        loop {
            let current = self.flags.load(Ordering::Relaxed);
            if current & FLAG_LAYOUT_LOCKED == 0
                && self
                    .flags
                    .compare_exchange_weak(
                        current,
                        current | FLAG_LAYOUT_LOCKED,
                        Ordering::Acquire,
                        Ordering::Relaxed,
                    )
                    .is_ok()
            {
                return;
            }
            std::hint::spin_loop();
        }
    }

    /// Release the layout lock [`Self::lock_layout`] took, publishing every
    /// write made under it.
    #[inline]
    pub fn unlock_layout(&self) {
        self.flags.fetch_and(!FLAG_LAYOUT_LOCKED, Ordering::Release);
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

    /// Clear the young flag; `true` for the one caller that cleared it,
    /// `false` when the cell was already old.
    #[inline]
    pub fn try_promote_in_place(&self) -> bool {
        self.flags.fetch_and(!FLAG_YOUNG, Ordering::AcqRel) & FLAG_YOUNG != 0
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

    /// [`Self::is_swept`] once no sweep runs beside the caller: a pause that
    /// completed the sweep already ordered its writes before the read.
    #[inline]
    pub(crate) fn is_swept_settled(&self) -> bool {
        self.flags.load(Ordering::Relaxed) & FLAG_SWEPT != 0
    }

    /// Record that the sweeper has finalized + dropped this object.
    #[inline]
    pub fn set_swept(&self) {
        self.flags.fetch_or(FLAG_SWEPT, Ordering::Release);
    }

    /// True iff this old/large parent is currently recorded in the
    /// remembered set (see [`FLAG_REMEMBERED`]). Drives the barrier's
    /// dedup: a parent already in the set is not pushed again.
    ///
    /// The remembered bit orders nothing: the mutator owns it between
    /// collections, and a scavenge's workers own the parents they trace,
    /// each side separated from the other by the pause.
    #[inline]
    pub fn is_remembered(&self) -> bool {
        self.flags.load(Ordering::Relaxed) & FLAG_REMEMBERED != 0
    }

    /// Record that this parent is now in the remembered set.
    #[inline]
    pub fn set_remembered(&self) {
        self.flags.fetch_or(FLAG_REMEMBERED, Ordering::Relaxed);
    }

    /// Clear the remembered bit. Called by the scavenge that consumes the
    /// store buffer, before re-tracing, so a parent that still holds an
    /// old→young edge after evacuation is re-pushed for the next scavenge.
    #[inline]
    pub fn clear_remembered(&self) {
        self.flags.fetch_and(!FLAG_REMEMBERED, Ordering::Relaxed);
    }

    /// Returns true iff the object is pinned (future compactor
    /// opt-out; not used by the Phase-1 scavenger which doesn't
    /// reorder old-gen).
    #[inline]
    pub fn is_pinned(&self) -> bool {
        self.flags.load(Ordering::Relaxed) & FLAG_PINNED != 0
    }

    /// Claim a young cell for evacuation, or wait until the thread that
    /// claimed it has published where it went.
    ///
    /// # Safety
    /// `this` must address a young cell in from-space during a scavenge.
    #[inline]
    pub(crate) unsafe fn claim_forwarding(this: *const Self) -> ForwardingClaim {
        // SAFETY: caller contract; the flag byte is atomic.
        let flags = unsafe { &(*this).flags };
        // Only a published forwarding offset is read after another worker
        // wrote it, so only that path acquires: relaxed loads let a worker
        // overlap the cache misses of consecutive cells. A claimed cell's
        // payload was written before the pause.
        let mut current = flags.load(Ordering::Relaxed);
        loop {
            if current & FLAG_FORWARDED != 0 {
                if current & FLAG_COPYING == 0 {
                    std::sync::atomic::fence(Ordering::Acquire);
                    // SAFETY: the fence orders the read after the load that
                    // saw the publishing store.
                    return ForwardingClaim::Forwarded(unsafe {
                        Self::read_forwarding_offset(this)
                    });
                }
                std::hint::spin_loop();
                current = flags.load(Ordering::Relaxed);
                continue;
            }
            match flags.compare_exchange_weak(
                current,
                current | FLAG_FORWARDED | FLAG_COPYING,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return ForwardingClaim::Claimed(current),
                Err(actual) => current = actual,
            }
        }
    }

    /// Publish where a cell this thread claimed went.
    ///
    /// # Safety
    /// `this` must be a cell this thread claimed with
    /// [`Self::claim_forwarding`], already copied; `claimed` is the flag
    /// byte the claim returned.
    #[inline]
    pub(crate) unsafe fn publish_forwarding(this: *mut Self, target_offset: u32, claimed: u8) {
        // SAFETY: caller contract; the claimed cell's payload is at least
        // one word and no other thread reads it before the release below.
        unsafe {
            (this.add(1) as *mut u32).write(target_offset);
            (*this)
                .flags
                .store(claimed | FLAG_FORWARDED, Ordering::Release);
        }
    }

    /// Give an evacuated copy the flag byte its source had before the claim.
    ///
    /// # Safety
    /// `this` must address a copy no other thread can reach yet.
    #[inline]
    pub(crate) unsafe fn restore_flags(this: *mut Self, claimed: u8) {
        // SAFETY: caller contract.
        unsafe { (*this).flags.store(claimed, Ordering::Relaxed) };
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
// The baked flag-byte offset must match the real field: the inline barrier
// loads `flags` at this displacement off the header base.
const _: () = assert!(std::mem::offset_of!(GcHeader, flags) == HEADER_FLAGS_BYTE_OFFSET);

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
