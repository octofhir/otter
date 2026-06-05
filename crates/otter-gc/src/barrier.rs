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
//! - The dirty card is derived from the **parent header**, never
//!   from the mutated slot. Traced slots routinely live outside
//!   the parent's cell (boxed frames, `VecDeque` buffers, spilled
//!   `SmallVec` storage are malloc-owned); masking such a slot
//!   address to "its page" fabricates a page header in foreign
//!   memory and the card-bit `|=` becomes a wild single-bit
//!   write. The scavenger's dirty-card scan re-traces every
//!   object intersecting a dirty card in full, so header-granular
//!   marking loses no edges.
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
/// Run after every pointer store of `child` performed into an
/// object whose [`GcHeader`] is `parent_header` — including
/// stores into malloc-owned side storage reachable from the
/// parent's payload (boxed frames, collection buffers).
///
/// # Safety
///
/// - `parent_header` must be a live `GcHeader` inside a heap page.
/// - `child` is the offset just stored into the slot. Zero is
///   accepted (no child = no-op).
/// - `marking` is the [`MarkingState`] owned by the same
///   [`crate::heap::GcHeap`] as `parent_header`.
#[inline]
pub unsafe fn write_barrier(
    parent_header: *mut GcHeader,
    child: RawGc,
    marking: &mut MarkingState,
) {
    // SAFETY: per docstring preconditions.
    unsafe {
        // 1) Generational barrier — old → young pointer. Mark the
        // card containing the parent's header dirty so the next
        // scavenge re-traces the parent and evacuates the young
        // child. The card is computed from the parent header — not
        // the slot — because traced slots may live in malloc-owned
        // side storage outside any heap page (see module
        // invariants).
        if !child.is_null() && (*parent_header).is_old() {
            // SAFETY: child offset is valid in-cage.
            let child_header = cage_base().add(child.0 as usize) as *mut GcHeader;
            if (*child_header).is_young() {
                let parent_addr = parent_header as *mut u8;
                let page_header = Page::header_of_mut(parent_addr);
                let page_base = Page::page_base_of(parent_addr);
                let byte_offset = (parent_addr as usize) - (page_base as usize);
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
