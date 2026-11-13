//! Heap liveness traps and exhaustive optional edge verification.
//!
//! # Contents
//! - Stale-handle validation and remembered-edge coverage.
//! - Post-mark closure checks and bounded failure provenance.
//!
//! # Invariants
//! - Verification never bypasses the incoming-owner garbage filter.
//! - A remembered miss stops before movement in every build profile.
//!
//! # See also
//! - [`super::minor`].

use super::GcHeap;
use crate::compressed::{Gc, cage_base};
use crate::header::GcHeader;
use crate::trace::Traceable;
#[path = "verification_index.rs"]
mod verification_index;

/// Debug-only stale-handle trap. A live handle is always updated to its
/// to-space copy by the collector, so a mutator payload access that lands on
/// a *forwarded* cell means a raw `Gc<T>` was held across an allocation that
/// scavenged — a use-after-move rooting bug that is *never* legitimate.
/// Trapping here names the type and condition at the offending read instead
/// of leaving a cryptic downstream shape/tag corruption.
///
/// The stricter *swept* (use-after-free) check reads dropped old-space memory;
/// a handful of low-level collector tests deliberately probe non-moving
/// old-space cells after a full GC, so it is gated behind `OTTER_GC_VERIFY=1`
/// alongside the other opt-in scavenge diagnostics.
///
/// Compiled out entirely in release builds. Type-tag mismatch is left to the
/// existing per-payload asserts; this only checks liveness.
#[inline]
pub(super) fn debug_assert_handle_live<T: Traceable>(handle: Gc<T>, context: &str) {
    if cfg!(debug_assertions) && !handle.is_null() {
        // SAFETY: header read only (no payload touch); single mutator, STW GC.
        unsafe {
            let header = &*handle.as_header_ptr();
            debug_assert!(
                !header.is_forwarded(),
                "use-after-move: {context} on a forwarded {} cell (tag={:#x}) — a raw Gc handle \
                 was held across a scavenging allocation and never re-read from its rooted slot",
                std::any::type_name::<T>(),
                header.type_tag(),
            );
            if gc_verify_use_after_free_enabled() {
                debug_assert!(
                    !header.is_swept(),
                    "use-after-free: {context} on a swept {} cell (tag={:#x}) — the handle \
                     outlived a full collection that finalized and dropped the payload",
                    std::any::type_name::<T>(),
                    header.type_tag(),
                );
            }
        }
    }
}

/// Cached `OTTER_GC_VERIFY` gate for the opt-in use-after-free trap. Read once;
/// a per-read env lookup would dominate the debug-build hot path.
fn gc_verify_use_after_free_enabled() -> bool {
    use std::sync::atomic::{AtomicU8, Ordering};
    static STATE: AtomicU8 = AtomicU8::new(0);
    match STATE.load(Ordering::Relaxed) {
        0 => {
            let on = std::env::var_os("OTTER_GC_VERIFY").is_some_and(|v| v != "0");
            STATE.store(if on { 2 } else { 1 }, Ordering::Relaxed);
            on
        }
        2 => true,
        _ => false,
    }
}

impl GcHeap {
    /// `OTTER_GC_VERIFY`: before a scavenge, every young child an old cell
    /// holds is reached by a trace the scavenge runs: of a remembered parent
    /// or of a young cell, either of which may walk storage it keeps out of
    /// line (an object's old slot slab). A miss names a store no generational barrier saw: the
    /// scavenge would move the child and leave that slot pointing at its
    /// forwarded copy. Reports the holder's tag and the slot's byte offset,
    /// and the cell referencing it; an unreferenced holder is garbage no
    /// trace reads. A remembered parent counts as traced in full: a type
    /// whose re-trace reads only its dirty interval (an element slab) is
    /// checked no closer than that, since reading the interval consumes it.
    /// A reported miss is fatal before the scavenge can move its child and
    /// leave the unvisited slot stale, including in optimized builds.
    pub(super) fn verify_remembered_complete(&self) {
        let trace_table = &self.trace_table;
        let old_pages = || {
            self.old_space
                .pages()
                .iter()
                .chain(self.large_space.pages().iter())
        };
        let live = |h: *mut GcHeader| {
            // SAFETY: STW pause; `h` is a walkable header.
            unsafe {
                (*h).type_tag() != crate::header::FREE_TAG
                    && (*h).size_bytes() != 0
                    && !(*h).is_swept_settled()
            }
        };
        let mut covered = verification_index::SlotCoverage::default();
        // Holder of each referenced cell, to name a reported cell's owner.
        let mut holder = verification_index::ReferenceOwners::default();
        let young_pages = self
            .new_space
            .from_pages()
            .iter()
            .chain(self.young_large_space.pages().iter());
        for (page, young) in old_pages()
            .map(|page| (page, false))
            .chain(young_pages.map(|page| (page, true)))
        {
            // SAFETY: STW pause; every non-filler cell is a registered body.
            unsafe {
                page.for_each_object(|h, _| {
                    if !live(h) {
                        return;
                    }
                    let traced = young || (*h).is_remembered();
                    trace_table.trace(h, &mut |slot| {
                        if traced {
                            covered.insert(slot as usize);
                        }
                        if (*slot).0 != 0 {
                            holder.insert((*slot).0, h as usize);
                        }
                    });
                });
            }
        }
        let mut reported = 0usize;
        for page in old_pages() {
            // SAFETY: as above.
            unsafe {
                page.for_each_object(|h, _| {
                    if !live(h) {
                        return;
                    }
                    trace_table.trace(h, &mut |slot| {
                        let raw = (*slot).0;
                        if raw == 0 || covered.contains(&(slot as usize)) {
                            return;
                        }
                        let child = cage_base().add(raw as usize) as *const GcHeader;
                        let offset = (h as usize - cage_base() as usize) as u32;
                        // An unreferenced cell is garbage the next full
                        // collection reclaims; no trace reads it.
                        let Some(&owner) = holder.get(&offset) else {
                            return;
                        };
                        if (*child).is_young() && reported < 16 {
                            reported += 1;
                            let owner = owner as *const GcHeader;
                            eprintln!(
                                "OTTER_GC_VERIFY: old cell {offset:#x} (tag {:#x}, {} bytes) holds \
                                 young cell {raw:#x} (tag {:#x}) at byte {} that no traced cell \
                                 reaches; referenced by {:#x} (tag {:#x}, young {}, remembered {}); \
                                 spaces {:?}/{:?}/{:?} flags {:#x}/{:#x}",
                                (*h).type_tag(),
                                (*h).size_bytes(),
                                (*child).type_tag(),
                                slot as usize - h as usize,
                                owner as usize - cage_base() as usize,
                                (*owner).type_tag(),
                                (*owner).is_young(),
                                (*owner).is_remembered(),
                                crate::page::Page::space_of(h as *const u8),
                                crate::page::Page::space_of(owner as *const u8),
                                crate::page::Page::space_of(child as *const u8),
                                *(h as *const u8).add(1),
                                *(owner as *const u8).add(1),
                            );
                            if std::env::var_os("OTTER_GC_VERIFY_BT").is_some() {
                                eprintln!("{}", std::backtrace::Backtrace::force_capture());
                            }
                        }
                    });
                });
            }
        }
        assert_eq!(reported, 0, "GC remembered-set verification failed");
    }

    /// `OTTER_GC_VERIFY`: after the final drain, every marked old cell's old
    /// children are marked. A miss names a store no barrier saw.
    pub(super) fn verify_marking_closed(&self) {
        let trace_table = &self.trace_table;
        let check = |page: &crate::page::Page| {
            // SAFETY: STW pause; every non-filler cell is a registered body.
            unsafe {
                page.for_each_object(|h, _| {
                    if (*h).type_tag() == crate::header::FREE_TAG || !(*h).is_marked() {
                        return;
                    }
                    trace_table.trace(h, &mut |slot| {
                        let raw = (*slot).0;
                        if raw == 0 {
                            return;
                        }
                        let child = cage_base().add(raw as usize) as *const GcHeader;
                        if !(*child).is_young() && !(*child).is_marked() {
                            panic!(
                                "OTTER_GC_VERIFY: marked cell {:#x} (tag {:#x}) points at unmarked \
                                 old cell {raw:#x} (tag {:#x})",
                                h as usize - cage_base() as usize,
                                (*h).type_tag(),
                                (*child).type_tag(),
                            );
                        }
                    });
                });
            }
        };
        self.old_space.pages().iter().for_each(check);
        self.large_space.pages().iter().for_each(check);
        self.young_large_space.pages().iter().for_each(check);
    }
}
