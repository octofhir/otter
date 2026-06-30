//! Write barriers for the page heap.
//!
//! V8/JSC-shape: every heap pointer store goes through a
//! barrier. Two duties stack on the same call site:
//!
//! 1. **Generational barrier** (load-bearing) — when an old-gen
//!    object is updated to point at a young-gen object, record the
//!    parent in the per-isolate remembered set (the object-granular
//!    store buffer, deduped by `GcHeader` `FLAG_REMEMBERED`). The
//!    scavenger re-traces every remembered parent and evacuates its
//!    young children even when no other root references them.
//! 2. **Insertion (Dijkstra) barrier** (dormant while the STW
//!    marker leaves marking inactive) — while marking is active,
//!    shading a fresh white child gray on every insertion
//!    preserves the strong tri-color invariant.
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
//! - The remembered entry is the **parent object**, never the
//!   mutated slot. Traced slots routinely live outside the
//!   parent's cell (boxed frames, `VecDeque` buffers, spilled
//!   `SmallVec` storage are malloc-owned); there is no in-cage slot
//!   address to record. The scavenger re-traces every remembered
//!   parent in full — reaching every off-page/exotic slot through
//!   the refreshed slab base — so object-granular recording loses
//!   no edges. Dedup via `FLAG_REMEMBERED` keeps the buffer bounded
//!   by the number of distinct mutated old parents, not write count.
//! - The card table (`mark_card` / `card_bitmap`) is **not** touched
//!   by the live minor-GC path anymore. It remains in `page.rs` only
//!   as the frozen-JIT barrier ABI surface (`JitGcBarrierLayout`);
//!   the JIT is off this session and will be re-pointed at the
//!   remembered-set insert sequence when it re-enables (ABI A9).
//!
//! # See also
//!
//! - GC architecture plan §5 (write barriers).
//! - `VM_GC_REDESIGN.md` — the object-granular remembered set.

use crate::compressed::{RawGc, cage_base};
use crate::header::{GcHeader, MarkColor};
use crate::marking::MarkingState;

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
    remembered: &mut Vec<RawGc>,
) {
    // SAFETY: per docstring preconditions.
    unsafe {
        // 1) Generational barrier — old → young pointer. Record the
        // parent in the remembered set so the next scavenge re-traces it
        // and evacuates the young child. We record the parent object, not
        // the slot, because traced slots may live in malloc-owned side
        // storage outside any heap page (see module invariants). Deduped
        // by FLAG_REMEMBERED: pushed at most once per scavenge interval.
        if !child.is_null() && (*parent_header).is_old() && !(*parent_header).is_remembered() {
            // SAFETY: child offset is valid in-cage.
            let child_header = cage_base().add(child.0 as usize) as *mut GcHeader;
            if (*child_header).is_young() {
                (*parent_header).set_remembered();
                // Parent is old/large and in-cage; record its cage-relative
                // offset. Old objects do not move on a minor GC, so the
                // offset stays valid until the scavenge that drains it.
                let offset = (parent_header as usize - cage_base() as usize) as u32;
                remembered.push(RawGc(offset));
            }
        }

        // 2) Insertion barrier — only fires when a marking
        // cycle is in progress. The STW marker leaves
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
