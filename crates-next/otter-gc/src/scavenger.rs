//! Cheney young-generation scavenger.
//!
//! BFS copying collector. Roots and dirty cards point into
//! from-space. Each from-space pointer is followed: if already
//! forwarded, the slot is rewritten to the existing forwarding
//! offset; otherwise the object is copied to to-space (or
//! promoted to old-space) and the slot is rewritten to the new
//! offset. After all roots and dirty cards are processed a
//! Cheney scan walks freshly-copied objects in to-space (and
//! freshly-promoted objects in old-space), evacuating any
//! children that still point into from-space.
//!
//! # Algorithm
//!
//! 1. Walk root slots — evacuate any from-space target.
//! 2. Walk every dirty card on every old-space page; trace each
//!    object overlapping the card and evacuate young children.
//!    Clear the card bits.
//! 3. Cheney scan: walk to-space pages and freshly-promoted
//!    bytes in old-space pages; trace each newly-copied object
//!    and evacuate children. Iterate until convergence.
//! 4. Bump every from-space page's `survival_age` so the next
//!    scavenge knows whether to promote.
//! 5. Flip from↔to. The new from-space is recycled and starts
//!    fresh; the new to-space is the prior from-space.
//!
//! # Promotion
//!
//! A page's `survival_age` increments on every scavenge it
//! survives. Once it reaches [`PROMOTE_AFTER_SURVIVALS`], the
//! scavenger promotes survivors copied off that page into
//! old-space rather than into to-space. V8 uses the same
//! single-survival heuristic.
//!
//! # Design
//!
//! Every Cheney implementation in Rust hits the same closure /
//! borrow conflict — the visitor mutates two spaces and the
//! scan loop iterates pages of one of them. We sidestep this by
//! holding the spaces through raw pointers (`NonNull`) inside a
//! [`ScavCtx`] struct that never aliases through `&mut Self`.
//! The mutator is paused (STW) for the duration, so single-
//! threaded uniqueness is upheld manually at the call site.
//!
//! # Contents
//!
//! - [`PROMOTE_AFTER_SURVIVALS`] — survival threshold.
//! - [`ScavengeStats`] — counters returned by `scavenge`.
//! - [`scavenge`] — the entry point.
//!
//! # See also
//!
//! - GC architecture plan §2.3, §4.4 (handle survival across
//!   moves), §5 (generational barrier feeding card table).

use std::ptr::NonNull;

use crate::compressed::{RawGc, cage_base};
use crate::header::GcHeader;
use crate::heap::RootSlotVisitor;
use crate::page::{CARD_SIZE, CELL_SIZE, PAGE_HEADER_SIZE, Page, PageHeader, align_up};
use crate::space::{NewSpace, OldSpace};
use crate::trace::TraceTable;

/// Promote after this many surviving scavenges. V8 uses 1.
pub const PROMOTE_AFTER_SURVIVALS: u32 = 1;

/// Stats returned by [`scavenge`].
#[derive(Debug, Default, Clone, Copy)]
pub struct ScavengeStats {
    /// Bytes copied to to-space (survived but not promoted).
    pub copied_bytes: usize,
    /// Bytes promoted to old-space.
    pub promoted_bytes: usize,
    /// Slot updates performed.
    pub slot_updates: usize,
}

/// Internal scavenge context — raw-pointer state under STW.
struct ScavCtx {
    new_space: NonNull<NewSpace>,
    old_space: NonNull<OldSpace>,
    trace_table: NonNull<TraceTable>,
    stats: ScavengeStats,
}

impl ScavCtx {
    #[inline]
    fn new_space(&mut self) -> &mut NewSpace {
        // SAFETY: STW pause + single-mutator invariant.
        unsafe { self.new_space.as_mut() }
    }

    #[inline]
    fn old_space(&mut self) -> &mut OldSpace {
        // SAFETY: STW pause + single-mutator invariant.
        unsafe { self.old_space.as_mut() }
    }
}

/// Run a full Cheney scavenge.
///
/// `root_slots` is a list of `*mut RawGc` slot addresses. The
/// scavenger may rewrite each in place. `external_visit` is a
/// hook for additional root sources (handle stack, global
/// handles); each call yields the additional slots.
///
/// # Safety
///
/// - The mutator must be paused for the duration of the call.
/// - Every `*mut RawGc` from `root_slots` and `external_visit`
///   must address a valid slot inside the cage (or null).
/// - The trace table must register every type tag occurring in
///   from-space and old-space.
pub unsafe fn scavenge(
    new_space: &mut NewSpace,
    old_space: &mut OldSpace,
    trace_table: &TraceTable,
    root_slots: &[*mut RawGc],
    external_visit: &mut RootSlotVisitor<'_>,
) -> ScavengeStats {
    let mut ctx = ScavCtx {
        // SAFETY: borrows are valid for the duration of this fn.
        new_space: unsafe { NonNull::new_unchecked(new_space as *mut _) },
        old_space: unsafe { NonNull::new_unchecked(old_space as *mut _) },
        trace_table: unsafe { NonNull::new_unchecked(trace_table as *const _ as *mut _) },
        stats: ScavengeStats::default(),
    };

    // 1) Explicit root slots.
    for &slot in root_slots {
        // SAFETY: caller guarantees slot is a valid pointer.
        unsafe { process_slot(&mut ctx, slot) };
    }

    // 2) External roots (handle stack, global handles).
    let ctx_ptr = &mut ctx as *mut ScavCtx;
    external_visit(&mut move |slot: *mut RawGc| {
        // SAFETY: ctx is alive on the surrounding stack frame.
        unsafe { process_slot(&mut *ctx_ptr, slot) };
    });

    // 3) Walk dirty cards on old-space pages — every card may
    // hold old→young pointers.
    // SAFETY: STW + raw-pointer state.
    unsafe { scan_old_dirty_cards(&mut ctx) };

    // 4) Cheney scan to-space (and freshly-promoted bytes in
    // old-space) until convergence.
    // SAFETY: STW + raw-pointer state.
    unsafe { cheney_scan(&mut ctx) };

    // 5) Bump survival ages on to-space pages — those are
    // the pages that received survivors during this scavenge.
    // After the flip below they become the new from-space; the
    // next scavenge reads their (now-bumped) survival_age and
    // promotes accordingly.
    for page in ctx.new_space().to_pages() {
        if page.header().allocated_bytes > 0 {
            let h = page.header_mut();
            h.survival_age = h.survival_age.saturating_add(1);
        }
    }

    // 6) Flip from↔to.
    ctx.new_space().flip();

    ctx.stats
}

/// Evacuate the target of a single slot if it is in from-space.
///
/// # Safety
///
/// `slot` must be a dereferenceable `*mut RawGc`.
unsafe fn process_slot(ctx: &mut ScavCtx, slot: *mut RawGc) {
    // SAFETY: slot is dereferenceable per precondition.
    unsafe {
        let raw = (*slot).0;
        if raw == 0 {
            return;
        }
        // SAFETY: raw is a valid in-cage offset by precondition.
        let header_ptr = cage_base().add(raw as usize) as *mut GcHeader;
        if !(*header_ptr).is_young() {
            return; // old / large objects do not move on scavenge.
        }
        let new_offset = evacuate(ctx, header_ptr);
        (*slot).0 = new_offset;
        ctx.stats.slot_updates += 1;
    }
}

/// Evacuate one young-gen object: forward if already evacuated,
/// otherwise copy + install forwarding pointer.
///
/// # Safety
///
/// `header` must reference a from-space object.
unsafe fn evacuate(ctx: &mut ScavCtx, header: *mut GcHeader) -> u32 {
    // SAFETY: header is a from-space object per caller. Every
    // dereference goes through raw pointers so no shared
    // reference outlives a write through the same allocation.
    unsafe {
        if (*header).is_forwarded() {
            return GcHeader::read_forwarding_offset(header);
        }
        let size = (*header).size_bytes() as usize;
        let aligned = align_up(size, CELL_SIZE);

        let promote = {
            let page_header = Page::header_of(header as *const u8);
            page_header.survival_age >= PROMOTE_AFTER_SURVIVALS
        };

        let new_offset = if promote {
            ctx.old_space()
                .alloc(aligned)
                .expect("old-space alloc during scavenge")
        } else {
            match ctx.new_space().alloc_in_to(aligned) {
                Some(off) => off,
                None => ctx
                    .old_space()
                    .alloc(aligned)
                    .expect("old-space alloc during scavenge fallback"),
            }
        };

        // Copy header + payload to the new location.
        let dest_ptr = cage_base().add(new_offset as usize);
        core::ptr::copy_nonoverlapping(header as *const u8, dest_ptr, size);

        // Promote / re-flag the destination header.
        let dest_header = dest_ptr as *mut GcHeader;
        if promote {
            (*dest_header).promote_to_old();
            ctx.stats.promoted_bytes += size;
        } else {
            ctx.stats.copied_bytes += size;
        }

        // Install forwarding pointer at the original location.
        GcHeader::write_forwarding_offset(header, new_offset);

        new_offset
    }
}

/// Walk every dirty card on every old-space page. Each card is
/// 512 B wide; objects whose body intersects a dirty card may
/// hold old→young pointers and must be re-traced.
///
/// # Safety
///
/// STW pause + valid pages.
unsafe fn scan_old_dirty_cards(ctx: &mut ScavCtx) {
    // SAFETY: per docstring.
    unsafe {
        let page_count = ctx.old_space().page_count();
        for idx in 0..page_count {
            let (base, bump_cursor) = {
                let page = &ctx.old_space().pages()[idx];
                (page.base_ptr(), page.header().bump_cursor)
            };
            let page_header = &mut *(base as *mut PageHeader);
            // Snapshot dirty card offsets and clear bits.
            let mut dirty: smallvec::SmallVec<[usize; 8]> = smallvec::SmallVec::new();
            page_header.for_each_dirty_card(|_, off| dirty.push(off));
            page_header.clear_cards();
            if dirty.is_empty() {
                continue;
            }
            // Walk every header on the page; if the body
            // intersects a dirty card, trace it.
            let mut hoff = PAGE_HEADER_SIZE;
            while hoff < bump_cursor {
                let header_ptr = base.add(hoff) as *mut GcHeader;
                let size = (*header_ptr).size_bytes() as usize;
                if size == 0 {
                    break;
                }
                let body_start = hoff;
                let body_end = hoff + align_up(size, CELL_SIZE);
                let overlaps = dirty
                    .iter()
                    .any(|&card_off| body_start < card_off + CARD_SIZE && body_end > card_off);
                if overlaps {
                    trace_one(ctx, header_ptr);
                }
                hoff = body_end;
            }
        }
    }
}

/// Cheney scan to convergence. Walks to-space pages (and
/// freshly-promoted bytes in old-space pages) and traces every
/// new object, evacuating any from-space children. Iterates
/// until no page advances its bump cursor in a pass — that's
/// when all reachable objects have been copied.
///
/// # Safety
///
/// STW pause + valid pages.
unsafe fn cheney_scan(ctx: &mut ScavCtx) {
    // Per-page scan cursors.
    let to_count = ctx.new_space().to_pages().len();
    let mut to_cursors: smallvec::SmallVec<[usize; 16]> =
        std::iter::repeat_n(PAGE_HEADER_SIZE, to_count).collect();
    let initial_old_count = ctx.old_space().page_count();
    let mut old_cursors: smallvec::SmallVec<[usize; 16]> = ctx
        .old_space()
        .pages()
        .iter()
        .map(|p| p.header().bump_cursor)
        .collect();

    // SAFETY: STW pause; raw page walk on owned spaces.
    unsafe {
        loop {
            let mut progress = false;

            // To-space scan.
            for idx in 0..ctx.new_space().to_pages().len() {
                let (base, limit) = {
                    let page = &ctx.new_space().to_pages()[idx];
                    (page.base_ptr(), page.header().bump_cursor)
                };
                let scan_from = if idx < to_cursors.len() {
                    to_cursors[idx]
                } else {
                    PAGE_HEADER_SIZE
                };
                if scan_from >= limit {
                    continue;
                }
                progress = true;
                scan_range_raw(ctx, base, scan_from, limit);
                if idx < to_cursors.len() {
                    to_cursors[idx] = limit;
                } else {
                    to_cursors.push(limit);
                }
            }

            // Old-space scan: walk newly-promoted bytes.
            let cur_old_count = ctx.old_space().page_count();
            // Extend cursors for any new pages added during this
            // pass.
            while old_cursors.len() < cur_old_count {
                old_cursors.push(PAGE_HEADER_SIZE);
            }
            for idx in 0..cur_old_count {
                let (base, limit) = {
                    let page = &ctx.old_space().pages()[idx];
                    (page.base_ptr(), page.header().bump_cursor)
                };
                // Both branches converge after our cursor-extend
                // step above; idx < initial_old_count picked up
                // its prior watermark, fresh pages start at
                // PAGE_HEADER_SIZE.
                let scan_from = old_cursors[idx];
                let _ = initial_old_count;
                if scan_from >= limit {
                    continue;
                }
                progress = true;
                scan_range_raw(ctx, base, scan_from, limit);
                old_cursors[idx] = limit;
            }

            if !progress {
                break;
            }
        }
    }
}

/// Trace every header in `[from, to)` on the page rooted at
/// `base`.
///
/// # Safety
///
/// `base` is a live page; the bytes between `from` and `to` are
/// initialised GcHeaders with valid `size_bytes`.
unsafe fn scan_range_raw(ctx: &mut ScavCtx, base: *mut u8, from: usize, to: usize) {
    // SAFETY: per docstring.
    unsafe {
        let mut offset = from;
        while offset < to {
            let header_ptr = base.add(offset) as *mut GcHeader;
            let size = (*header_ptr).size_bytes() as usize;
            if size == 0 {
                break;
            }
            trace_one(ctx, header_ptr);
            offset += align_up(size, CELL_SIZE);
        }
    }
}

/// Trace one header — visitor evacuates any from-space child.
///
/// # Safety
///
/// `header` is a live, valid GcHeader.
unsafe fn trace_one(ctx: &mut ScavCtx, header: *mut GcHeader) {
    // SAFETY: per docstring; trace table guaranteed to register
    // the type tag.
    unsafe {
        let table_ptr = ctx.trace_table.as_ptr();
        let ctx_ptr = ctx as *mut ScavCtx;
        let mut visitor = move |slot: *mut RawGc| {
            process_slot(&mut *ctx_ptr, slot);
        };
        (*table_ptr).trace(header, &mut visitor);
    }
}
