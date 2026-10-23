//! Full-sweep physical release inside old-space holes and unused bump tails.
//!
//! # Contents
//! - [`OldSpace::discard_swept_unused_payload`] consumes only existing sweep facts.
//!
//! # Invariants
//! - Reclamation hooks and every trace/weak reader finish before discard.
//! - The sweep's maximal free runs belong to retained pages and are not listed yet.
//! - Each FREE header and future intrusive link retain their containing OS pages.
//! - A page's bump suffix has no header, live cell or allocation lease to preserve.
//! - No address, allocation count, free-list fact or admission policy changes.
//!
//! # See also
//! - [`crate::heap::GcHeap::sweep_phase`] owns the shared full/split commit point.
//! - [`super::free_list`] owns the free-range metadata geometry.
//! - [`crate::compressed::discard`] owns OS rounding and no-touch remap failure.

use super::OldSpace;
use super::free_list::FREE_RANGE_METADATA_SIZE;
use crate::compressed::{cage_base, discard};
use crate::header::{FREE_TAG, GcHeader};
use crate::page::{PAGE_HEADER_SIZE, PAGE_SIZE};

impl OldSpace {
    /// Release physical pages inside recovered free runs and unused page tails.
    /// `ranges` is the sweep's existing vector of maximal, filler-capped runs.
    /// Whole dead pages must already have been reaped. Unsupported hosts and
    /// failed remaps preserve the unused bytes. Returns actual remapped bytes.
    ///
    /// # Safety
    /// The mutator and all marker/sweeper workers must be stopped, both LABs
    /// retired, and all weak/finalizer/payload readers finished. Every range
    /// must be nonempty, exclusively owned, FREE-capped and contained by one
    /// retained page's published bump interval. The free list must be clear;
    /// the caller publishes these same ranges only after this call returns.
    pub(crate) unsafe fn discard_swept_unused_payload(&self, ranges: &[(u32, usize)]) -> usize {
        let Some(granule) = discard::system_page_size() else {
            return 0;
        };
        debug_assert!(self.lab.is_none());
        let mut discarded = 0usize;
        for &(offset, bytes) in ranges {
            debug_assert!(bytes != 0);
            debug_assert_eq!(
                offset as usize / PAGE_SIZE,
                (offset as usize + bytes - 1) / PAGE_SIZE
            );
            // SAFETY: the completed sweep supplies this initialized filler.
            unsafe {
                let header = &*cage_base().add(offset as usize).cast::<GcHeader>();
                debug_assert_eq!(header.type_tag(), FREE_TAG);
                debug_assert_eq!(header.size_bytes() as usize, bytes);
            }
            // The prefix is owned by the filler and future intrusive link.
            // Rounding inward after it keeps both OS pages if they straddle.
            let prefix = bytes.min(FREE_RANGE_METADATA_SIZE);
            // SAFETY: only dead payload following the preserved metadata is
            // supplied, after all possible readers and before list publication.
            discarded = discarded.saturating_add(unsafe {
                discard::unused_range(
                    cage_base().add(offset as usize + prefix),
                    bytes - prefix,
                    granule,
                )
            });
        }
        for page in &self.pages {
            let cursor = page.header().bump_cursor;
            debug_assert!(cursor >= PAGE_HEADER_SIZE && cursor <= page.header().span_bytes());
            // SAFETY: a swept trailing dead run moved the cursor backwards;
            // neither it nor the already-unused suffix is in the walk/list.
            // Its reclamation hooks completed before the caller reached here.
            discarded = discarded.saturating_add(unsafe {
                discard::unused_range(
                    page.base_ptr().add(cursor),
                    page.header().span_bytes() - cursor,
                    granule,
                )
            });
        }
        discarded
    }
}
