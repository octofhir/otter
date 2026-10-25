//! Address-preserving release of unused physical cage pages.
//!
//! # Contents
//! - Host OS page size, queried once by each cold discard operation.
//! - Inward-rounded discard of unused bytes with unchanged cage ownership.
//! - Anonymous writable remapping shared with aged-out pooled pages.
//!
//! # Invariants
//! - Only whole OS pages strictly within the supplied unused range change.
//! - Successful discard preserves virtual addresses and read/write access.
//! - Unsupported hosts and failed remaps leave every byte untouched.
//! - The caller owns the range and has finished every trace/finalizer read.
//!
//! # See also
//! - [`crate::space::NewSpace`] retains nursery headers and allocation capacity.
//! - `super::release_memory` remaps pooled free pages that aged out.

/// The host's actual OS page size; unsupported hosts have no discard path.
pub(crate) fn system_page_size() -> Option<usize> {
    #[cfg(unix)]
    {
        // SAFETY: sysconf has no pointer arguments or side effects on cage memory.
        usize::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) })
            .ok()
            .filter(|size| size.is_power_of_two())
    }
    #[cfg(not(unix))]
    {
        None
    }
}

/// Replace an aligned owned range with zero-filled anonymous writable pages.
/// Failure is a no-op; this function never faults the range in by zeroing it.
///
/// # Safety
/// `ptr..ptr + len` must be OS-page aligned, inside the cage reservation and
/// exclusively owned. No live object, pending root, tracing worker or finalizer
/// may read or reference the replaced bytes.
pub(super) unsafe fn remap_zeroed(ptr: *mut u8, len: usize) -> bool {
    #[cfg(unix)]
    {
        // SAFETY: MAP_FIXED replaces exactly the unused range owned by the caller.
        let mapped = unsafe {
            libc::mmap(
                ptr.cast::<libc::c_void>(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_FIXED | libc::MAP_PRIVATE | libc::MAP_ANON,
                -1,
                0,
            )
        };
        mapped == ptr.cast::<libc::c_void>()
    }
    #[cfg(not(unix))]
    {
        let _ = (ptr, len);
        false
    }
}

/// Release whole OS pages inside an unused byte range. Returns the bytes
/// actually remapped; zero also covers an empty range or unsupported host.
/// The allocation/cage/free-list owners and their counters remain unchanged.
///
/// # Safety
/// `ptr..ptr + len` must be exclusively owned cage storage. Every byte in the
/// range is unused, with no pending root, live cell, worker or finalizer access.
/// `granule` must be the host OS page size returned by [`system_page_size`].
pub(crate) unsafe fn unused_range(ptr: *mut u8, len: usize, granule: usize) -> usize {
    if !granule.is_power_of_two() {
        return 0;
    }
    let start = ptr as usize;
    let Some(end) = start.checked_add(len) else {
        return 0;
    };
    let Some(aligned_start) = start.checked_add(granule - 1).map(|n| n & !(granule - 1)) else {
        return 0;
    };
    let aligned_end = end & !(granule - 1);
    let Some(bytes) = aligned_end.checked_sub(aligned_start).filter(|&n| n != 0) else {
        return 0;
    };
    debug_assert!(aligned_start >= super::cage_base() as usize);
    debug_assert!(aligned_end <= super::cage_base() as usize + super::cage_size());
    // SAFETY: inward rounding preserves the supplied ownership/no-read contract;
    // derive the aligned pointer from the caller's pointer provenance.
    if unsafe { remap_zeroed(ptr.add(aligned_start - start), bytes) } {
        bytes
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compressed::Cage;
    use crate::page::{PAGE_HEADER_SIZE, PAGE_SIZE, Page, SpaceKind};

    #[test]
    fn nursery_discard_rounds_inward_preserves_canaries_and_stays_writable() {
        Cage::ensure_default().unwrap();
        let page = Page::new(SpaceKind::NewFrom).unwrap();
        let base = page.base_ptr();
        let granule = system_page_size().unwrap_or(4096);
        assert!(granule < PAGE_SIZE / 2);
        let offset = page.cage_offset();
        let cursor = page.header().bump_cursor;
        // SAFETY: this freshly owned page has no cells. Its header stays outside
        // the painted payload and the requested discard range.
        unsafe {
            core::ptr::write_bytes(
                base.add(PAGE_HEADER_SIZE),
                0xa5,
                PAGE_SIZE - PAGE_HEADER_SIZE,
            )
        };
        let start = PAGE_HEADER_SIZE + 17;
        let end = PAGE_SIZE - 17;
        let aligned_start = crate::page::align_up(start, granule);
        let aligned_end = end & !(granule - 1);
        // SAFETY: only painted unused bytes are supplied. On unsupported hosts
        // the operation is a no-op regardless of this valid test granule.
        let discarded = unsafe { unused_range(base.add(start), end - start, granule) };
        assert_eq!(page.cage_offset(), offset);
        assert_eq!(page.header().bump_cursor, cursor);
        assert_eq!(page.header().space, SpaceKind::NewFrom);
        assert!(
            !Cage::is_free_page(offset),
            "discard cannot return ownership"
        );
        // SAFETY: the owned page is still readable/writable at the same address.
        unsafe {
            assert_eq!(*base.add(aligned_start - 1), 0xa5);
            assert_eq!(*base.add(aligned_end), 0xa5);
            #[cfg(unix)]
            {
                assert_eq!(discarded, aligned_end - aligned_start);
                assert_eq!(*base.add(aligned_start), 0);
                assert_eq!(*base.add(aligned_end - 1), 0);
                core::ptr::write_bytes(base.add(aligned_start), 0x5a, discarded);
                assert_eq!(*base.add(aligned_start), 0x5a);
                assert_eq!(*base.add(aligned_end - 1), 0x5a);
            }
            #[cfg(not(unix))]
            {
                assert_eq!(discarded, 0);
                assert_eq!(*base.add(aligned_start), 0xa5);
                assert_eq!(*base.add(aligned_end - 1), 0xa5);
            }
        }
    }
}
