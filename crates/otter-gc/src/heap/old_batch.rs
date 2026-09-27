//! Reserve old-space storage for independently collectible repeated cells.
//!
//! # Contents
//! - `GcHeap::alloc_old_batch_with_roots` initializes caller-owned handle slots.
//!
//! # Invariants
//! - Every element has its own ordinary header, trace entry and allocation count.
//! - The complete byte budget is checked before reserving storage; pending value
//!   and caller roots participate in any collection at that boundary.
//! - Reservation and publication contain no safepoint. Every reserved range is
//!   filled with walkable objects before another fallible reservation begins.
//! - A failed reservation leaves any completed prefix as ordinary GC objects.
//!
//! # See also
//! - `super::GcHeap::alloc_old_with_roots` allocates one arbitrary owned payload.

use super::{Gc, GcHeader, GcHeap, MarkColor, OutOfMemory, RootSlotVisitor, Traceable};
use crate::compressed::cage_base;
use crate::page::{CELL_SIZE, PAGE_PAYLOAD_SIZE, align_up};

impl GcHeap {
    /// Initialize a batch of independent old-space cells with the same value.
    ///
    /// This is engine allocation plumbing for frame entry. `T` is a fixed-size
    /// `Copy` payload; it must not require trailing storage. Each cell remains
    /// individually traced and reclaimed. Handles may be non-contiguous when
    /// the batch exceeds one page. No collection occurs after the budget check.
    ///
    /// On failure, an initialized prefix may have been written to `output`.
    /// Those handles remain valid, and their cells are included in GC accounting.
    pub fn alloc_old_batch_with_roots<T: Traceable + Copy>(
        &mut self,
        mut value: T,
        output: &mut [Gc<T>],
        external_visit: &mut RootSlotVisitor<'_>,
    ) -> Result<(), OutOfMemory> {
        const {
            assert!(std::mem::align_of::<T>() <= crate::OBJECT_ALIGNMENT);
        }
        if output.is_empty() {
            return Ok(());
        }
        let aligned = align_up(
            std::mem::size_of::<GcHeader>() + std::mem::size_of::<T>(),
            CELL_SIZE,
        );
        let per_page = PAGE_PAYLOAD_SIZE / aligned;
        let total = aligned
            .checked_mul(output.len())
            .filter(|_| per_page != 0)
            .ok_or(OutOfMemory::AllocationTooLarge {
                requested_bytes: u64::MAX,
                max_bytes: PAGE_PAYLOAD_SIZE as u64,
            })?;
        if self.trace_table.get(T::TYPE_TAG).is_none() {
            self.trace_table.register::<T>();
        }
        if self.max_heap_bytes != 0 {
            let mut roots = |visitor: &mut dyn FnMut(*mut crate::raw::RawGc)| {
                external_visit(visitor);
                // SAFETY: the fixed pending payload remains live until the
                // check finishes; all copies below use the rewritten value.
                unsafe { T::trace_pending_slots(&raw mut value, visitor) };
            };
            self.account_or_collect_with_roots(total as u64, &mut roots)?;
        }
        let is_marking = self.marking.is_marking();
        let mut remaining = total;
        for chunk in output.chunks_mut(per_page) {
            let bytes = chunk.len() * aligned;
            let offset = match self.old_space.alloc(bytes) {
                Ok(offset) => offset,
                Err(error) => {
                    if self.max_heap_bytes != 0 {
                        self.tracked_bytes = self.tracked_bytes.saturating_sub(remaining as u64);
                    }
                    return Err(error);
                }
            };
            for (index, destination) in chunk.iter_mut().enumerate() {
                let offset = offset + (index * aligned) as u32;
                // SAFETY: this range belongs to this reservation, is aligned,
                // and fits on one old-space page. No collection can observe
                // it until every header and payload has been initialized.
                unsafe {
                    let header = cage_base().add(offset as usize).cast::<GcHeader>();
                    let payload = header.add(1).cast::<T>();
                    let contents = GcHeader::new(T::TYPE_TAG, aligned as u32);
                    if is_marking {
                        contents.set_mark_color(MarkColor::Black);
                    }
                    header.write(contents);
                    payload.write(value);
                    T::trace_slots(payload, &mut |slot| {
                        crate::barrier::write_barrier(
                            header,
                            *slot,
                            &mut self.marking,
                            &mut self.remembered_parents,
                        );
                    });
                    *destination = Gc::from_offset(offset);
                }
            }
            let row = &mut self.gc_stats.by_type[T::TYPE_TAG as usize];
            row.live_bytes = row.live_bytes.wrapping_add(bytes);
            row.alloc_count_total = row.alloc_count_total.wrapping_add(chunk.len() as u64);
            row.alloc_bytes_total = row.alloc_bytes_total.wrapping_add(bytes as u64);
            remaining -= bytes;
        }
        Ok(())
    }
}
