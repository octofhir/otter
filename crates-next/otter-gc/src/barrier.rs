//! Write barriers for the page heap.
//!
//! V8/JSC-shape: every heap pointer store goes through a
//! barrier. Two duties stack on the same call site:
//!
//! 1. **Generational barrier** (load-bearing in Phase 1) — when
//!    an old-gen object is updated to point at a young-gen
//!    object, mark the card containing the slot dirty. The
//!    scavenger's dirty-card scan then knows to evacuate that
//!    young child even when no other root references it.
//! 2. **Insertion (Dijkstra) barrier** (dormant in Phase 1; lit
//!    in Phase 2 / task 86) — while marking is active, shading
//!    a fresh white child gray on every insertion preserves
//!    the strong tri-color invariant.
//!
//! # Contents
//!
//! - [`write_barrier`] — single hot-path entry point used at
//!   every store of a `Gc<T>` field.
//!
//! # Invariants
//!
//! - Calling `write_barrier` is sound for any combination of
//!   parent / child generations and marking states; it
//!   short-circuits to a couple of branches in the common case.
//! - Card-table mark uses a single `(byte_offset / CARD_SIZE)`
//!   index + atomic-or; zero allocation.
//!
//! # See also
//!
//! - GC architecture plan §5 (write barriers).

use crate::compressed::{RawGc, cage_base};
use crate::header::{GcHeader, MarkColor};
use crate::marking::MarkingState;
use crate::page::Page;

/// Single write-barrier entry point.
///
/// Run after every pointer store `*slot_addr = child` performed
/// inside an object whose [`GcHeader`] is `parent_header`.
///
/// `slot_addr` may be any address inside the parent object —
/// the barrier locates the owning page via the address bitmask,
/// not via the parent header.
///
/// # Safety
///
/// - `parent_header` must be a live `GcHeader`.
/// - `slot_addr` must be inside the same page as `parent_header`
///   (typically a field address inside the parent payload).
/// - `child` is the offset just stored into the slot. Zero is
///   accepted (no child = no-op).
/// - `marking` is the [`MarkingState`] owned by the same
///   [`crate::heap::GcHeap`] as `parent_header`.
#[inline]
pub unsafe fn write_barrier(
    parent_header: *mut GcHeader,
    slot_addr: *mut u8,
    child: RawGc,
    marking: &mut MarkingState,
) {
    // SAFETY: per docstring preconditions.
    unsafe {
        // 1) Generational barrier — old → young pointer. Mark
        // card dirty so the next scavenge picks up the young
        // child.
        if !child.is_null() && (*parent_header).is_old() {
            // SAFETY: child offset is valid in-cage.
            let child_header = cage_base().add(child.0 as usize) as *mut GcHeader;
            if (*child_header).is_young() {
                let page_header = Page::header_of_mut(slot_addr);
                let page_base = Page::page_base_of(slot_addr);
                let byte_offset = (slot_addr as usize) - (page_base as usize);
                page_header.mark_card(byte_offset);
            }
        }

        // 2) Insertion barrier — only fires when a marking
        // cycle is in progress. Phase-1 STW marker leaves
        // `is_marking` false at the call site.
        if marking.is_marking() && !child.is_null() {
            // SAFETY: child offset is valid in-cage.
            let child_header = cage_base().add(child.0 as usize) as *mut GcHeader;
            if matches!((*child_header).mark_color(), MarkColor::White) {
                marking.shade_gray(child_header);
            }
        }
    }
}
