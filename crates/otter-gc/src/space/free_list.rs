//! Intrusive size-class lists for reusable old-generation ranges.
//!
//! # Contents
//! - [`OldFreeList`] selects a fitting range using one nonempty-class bitmap.
//! - [`FreeEntry`] transfers exclusive range ownership to the linear allocator.
//!
//! # Invariants
//! - Links occupy dead filler payloads, never live objects or moving cells.
//!   The owner keeps every listed page alive until the list is cleared.
//! - Each range belongs to exactly one list or to the active allocation area.
//! - Filler headers remain intact and walkable while ranges are listed.
//! - A request probes its own class's head, then the smallest nonempty larger
//!   class. Larger classes guarantee a fit; no operation scans a list or sorts
//!   holes. A smaller head stays available for a later smaller request.
//! - Clearing before sweep invalidates all links without reading dead pages.
//!
//! # See also
//! - `super::OldSpace` owns pages and consumes ranges through a linear area.
//! - V8 `src/heap/free-list.cc`: `FreeListManyCached::Allocate` and
//!   `FreeListCategory::PickNodeFromList` use category skipping and head probes.

use crate::header::{GcHeader, HEADER_SIZE};
use crate::page::{CELL_SIZE, PAGE_PAYLOAD_SIZE};

const MIN_BYTES: usize = 32;
const DOUBLING_MIN: usize = 256;
const PRECISE_COUNT: usize = (DOUBLING_MIN - MIN_BYTES) / CELL_SIZE;
const DOUBLING_COUNT: usize = 10;
const CLASS_COUNT: usize = PRECISE_COUNT + DOUBLING_COUNT;
const _: () = assert!(CLASS_COUNT < u64::BITS as usize);
const _: () = assert!(MIN_BYTES >= HEADER_SIZE + size_of::<u32>());

/// A filler-capped range transferred from a list to its exclusive consumer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct FreeEntry {
    pub(super) offset: u32,
    pub(super) size: u32,
}

/// Non-owning cage links; the enclosing old space owns their page lifetimes.
pub(super) struct OldFreeList {
    heads: [u32; CLASS_COUNT],
    nonempty: u64,
}

impl Default for OldFreeList {
    fn default() -> Self {
        Self {
            heads: [0; CLASS_COUNT],
            nonempty: 0,
        }
    }
}

impl OldFreeList {
    fn class_of(size: usize) -> usize {
        if size < DOUBLING_MIN {
            (size - MIN_BYTES) / CELL_SIZE
        } else {
            PRECISE_COUNT + (size.ilog2() - DOUBLING_MIN.ilog2()) as usize
        }
    }

    pub(super) fn clear(&mut self) {
        self.heads.fill(0);
        self.nonempty = 0;
    }

    /// Relinquish a dead range to this list. Tiny fillers remain unlisted.
    ///
    /// # Safety
    /// `offset` names an initialized FREE_TAG header covering `size` aligned
    /// bytes in a live old page. The range is exclusively owned, contains no
    /// live payloads and is not already listed. Its page must remain alive and
    /// unmodified until the range is taken or the list is cleared.
    pub(super) unsafe fn push(&mut self, offset: u32, size: usize) {
        debug_assert!(size.is_multiple_of(CELL_SIZE));
        debug_assert!(size <= PAGE_PAYLOAD_SIZE);
        if size < MIN_BYTES {
            return;
        }
        let class = Self::class_of(size);
        // SAFETY: the caller transferred an initialized, exclusive free range.
        // MIN_BYTES leaves room for the link after its intact header.
        unsafe {
            let header = crate::compressed::cage_base()
                .add(offset as usize)
                .cast::<GcHeader>();
            debug_assert_eq!((*header).type_tag(), crate::header::FREE_TAG);
            debug_assert_eq!((*header).size_bytes() as usize, size);
            header
                .byte_add(HEADER_SIZE)
                .cast::<u32>()
                .write(self.heads[class]);
        }
        self.heads[class] = offset;
        self.nonempty |= 1u64 << class;
    }

    fn head(&self, class: usize) -> Option<FreeEntry> {
        let offset = self.heads[class];
        if offset == 0 {
            return None;
        }
        // SAFETY: push's ownership contract keeps every listed header alive.
        let size = unsafe {
            (*crate::compressed::cage_base()
                .add(offset as usize)
                .cast::<GcHeader>())
            .size_bytes()
        };
        Some(FreeEntry { offset, size })
    }

    fn pop(&mut self, class: usize, entry: FreeEntry) -> FreeEntry {
        // SAFETY: entry is this class's live head. Read its link before
        // returning exclusive ownership to the allocator, which may overwrite it.
        let next = unsafe {
            crate::compressed::cage_base()
                .add(entry.offset as usize + HEADER_SIZE)
                .cast::<u32>()
                .read()
        };
        self.heads[class] = next;
        if next == 0 {
            self.nonempty &= !(1u64 << class);
        }
        entry
    }

    pub(super) fn take(&mut self, size: usize) -> Option<FreeEntry> {
        debug_assert!(size.is_multiple_of(CELL_SIZE));
        if self.nonempty == 0 || size > PAGE_PAYLOAD_SIZE {
            return None;
        }
        let start = Self::class_of(size.max(MIN_BYTES));
        if let Some(entry) = self.head(start)
            && entry.size as usize >= size
        {
            return Some(self.pop(start, entry));
        }
        let larger = self.nonempty & (u64::MAX << (start + 1));
        if larger == 0 {
            return None;
        }
        let class = larger.trailing_zeros() as usize;
        let entry = self.head(class).expect("nonempty bitmap names a list head");
        debug_assert!(entry.size as usize >= size);
        Some(self.pop(class, entry))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compressed::{CAGE_TEST_LOCK, Cage};
    use crate::page::{Page, SpaceKind};

    fn range(page: &Page, list: &mut OldFreeList, size: usize) -> u32 {
        let offset = page.bump_alloc(size).expect("test page has space");
        // SAFETY: this test owns the new page range and keeps the page alive
        // for the entire list lifetime; no live payload occupies these bytes.
        unsafe {
            crate::compressed::cage_base()
                .add(offset as usize)
                .cast::<GcHeader>()
                .write(GcHeader::new_free(size as u32));
            list.push(offset, size);
        }
        offset
    }

    #[test]
    fn exact_classes_and_bitmap_skip_empty_classes() {
        let _lock = CAGE_TEST_LOCK.lock().unwrap();
        Cage::ensure_default().unwrap();
        let page = Page::new(SpaceKind::Old).unwrap();
        let mut list = OldFreeList::default();
        let a = range(&page, &mut list, 64);
        let b = range(&page, &mut list, 72);
        let c = range(&page, &mut list, 4096);
        assert_eq!(
            list.take(64),
            Some(FreeEntry {
                offset: a,
                size: 64
            })
        );
        assert_eq!(
            list.take(64),
            Some(FreeEntry {
                offset: b,
                size: 72
            })
        );
        assert_eq!(
            list.take(64),
            Some(FreeEntry {
                offset: c,
                size: 4096
            })
        );
        assert!(list.take(64).is_none());
        assert_eq!(list.nonempty, 0);
    }

    #[test]
    fn undersized_head_stays_available_without_scanning() {
        let _lock = CAGE_TEST_LOCK.lock().unwrap();
        Cage::ensure_default().unwrap();
        let page = Page::new(SpaceKind::Old).unwrap();
        let mut list = OldFreeList::default();
        let a = range(&page, &mut list, 904);
        let b = range(&page, &mut list, 520);
        let c = range(&page, &mut list, 1024);
        assert_eq!(list.take(704).unwrap().offset, c);
        assert!(list.take(704).is_none());
        assert_eq!(list.take(520).unwrap().offset, b);
        assert_eq!(list.take(704).unwrap().offset, a);
        assert_eq!(list.nonempty, 0);
    }

    #[test]
    fn clear_never_follows_links_in_released_pages() {
        let _lock = CAGE_TEST_LOCK.lock().unwrap();
        Cage::ensure_default().unwrap();
        let page = Page::new(SpaceKind::Old).unwrap();
        let mut list = OldFreeList::default();
        range(&page, &mut list, 256);
        list.clear();
        drop(page);
        assert!(list.take(256).is_none());
        let page = Page::new(SpaceKind::Old).unwrap();
        let fresh = range(&page, &mut list, 264);
        assert_eq!(list.take(256).unwrap().offset, fresh);
    }

    #[test]
    fn all_aligned_sizes_fit_their_class_and_preserve_page_walks() {
        let _lock = CAGE_TEST_LOCK.lock().unwrap();
        Cage::ensure_default().unwrap();
        // Exercise every class boundary and exact size, including the last
        // page-sized bin. No class lookup can index past the bitmap/table.
        let page = Page::new(SpaceKind::Old).unwrap();
        for size in (CELL_SIZE..=PAGE_PAYLOAD_SIZE).step_by(CELL_SIZE) {
            page.reset_bump();
            let mut list = OldFreeList::default();
            let offset = range(&page, &mut list, size);
            let mut walked = 0;
            // SAFETY: range initialized the page's only allocated header;
            // the intrusive link must leave that header intact.
            unsafe {
                page.for_each_object(|_, _| walked += 1);
            }
            assert_eq!(walked, 1);
            if size < MIN_BYTES {
                assert!(list.take(CELL_SIZE).is_none());
            } else {
                assert_eq!(
                    list.take(size),
                    Some(FreeEntry {
                        offset,
                        size: size as u32
                    })
                );
            }
        }
    }
}
