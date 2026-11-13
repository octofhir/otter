//! Heap page census, snapshot walks and restored-page adoption.
//!
//! # Contents
//! - Raw type guards and stopped live-object/slot walks.
//! - Census space order, page geometry and image page ownership.
//!
//! # Invariants
//! - Walks synchronize allocation windows and require completed sweeps.
//! - Image-page adoption invalidates the all-white assumption before marking.
//!
//! # See also
//! - [`crate::heap_image`].

use super::GcHeap;
use crate::compressed::{Gc, RawGc};
use crate::header::GcHeader;
use crate::page::{PAGE_HEADER_SIZE, page_base_from_offset};
use crate::trace::Traceable;

impl GcHeap {
    /// Type tag for a raw heap object.
    #[doc(hidden)]
    #[must_use]
    pub fn raw_type_tag(&self, raw: RawGc) -> Option<u8> {
        if raw.is_null() {
            return None;
        }
        // SAFETY: caller supplies a heap-issued raw handle.
        Some(unsafe { (*raw.as_header_ptr()).type_tag() })
    }

    /// Type-check and cast a raw heap object to `Gc<T>`.
    #[doc(hidden)]
    #[must_use]
    pub fn cast_raw_if_type<T: Traceable>(&self, raw: RawGc) -> Option<Gc<T>> {
        if self.raw_type_tag(raw)? != T::TYPE_TAG {
            return None;
        }
        // SAFETY: the runtime type tag matches `T`.
        Some(unsafe { raw.cast::<T>() })
    }

    /// Iterate every live object in every space (debug / snapshot
    /// hook). Used by [`crate::devtools_snapshot`] and tests.
    ///
    /// # Safety
    ///
    /// Must run under STW pause; visitor must not allocate.
    pub unsafe fn for_each_live_object<F>(&self, mut visitor: F)
    where
        F: FnMut(*mut GcHeader),
    {
        debug_assert!(self.sweeping.is_none(), "finish the sweep before walking");
        self.sync_lab();
        // SAFETY: STW pause + valid pages.
        unsafe {
            for page in self.new_space.from_pages() {
                page.for_each_object(|h, _| visitor(h));
            }
            for page in self.old_space.pages() {
                page.for_each_object(|h, _| visitor(h));
            }
            for page in self.large_space.pages() {
                page.for_each_object(|h, _| visitor(h));
            }
            for page in self.young_large_space.pages() {
                page.for_each_object(|h, _| visitor(h));
            }
        }
    }

    /// Old-space pages currently owned.
    #[must_use]
    pub fn old_space_page_count(&self) -> usize {
        self.old_space.page_count()
    }

    /// Take ownership of a page already filled by
    /// [`crate::heap_image::GcHeap::restore_old_space`].
    ///
    /// The page's objects are live and relocated; the sweeper treats it
    /// like any other old page from here.
    pub(crate) fn adopt_restored_old_page(&mut self, page: crate::page::Page) {
        self.old_space.adopt_page(page);
        self.marks_white = false;
    }

    /// Page lists for every space, in census order: old, young
    /// from-space, large, young large. Lets [`crate::census`] attribute
    /// objects to a space without opening up the space fields.
    pub(crate) fn census_spaces(&self) -> [(crate::page::SpaceKind, &[crate::page::Page]); 4] {
        debug_assert!(self.sweeping.is_none(), "finish the sweep before walking");
        self.sync_lab();
        [
            (crate::page::SpaceKind::Old, self.old_space.pages()),
            (crate::page::SpaceKind::NewFrom, self.new_space.from_pages()),
            (crate::page::SpaceKind::Large, self.large_space.pages()),
            (
                crate::page::SpaceKind::NewLarge,
                self.young_large_space.pages(),
            ),
        ]
    }

    /// Trace the slots of a single object — exposed for the
    /// snapshot writer.
    ///
    /// # Safety
    ///
    /// `header` is a valid live header registered in the trace
    /// table.
    pub unsafe fn trace_one(&self, header: *mut GcHeader, visitor: &mut dyn FnMut(*mut RawGc)) {
        // SAFETY: per docstring.
        unsafe {
            self.trace_table.trace(header, visitor);
        }
    }

    /// Page-base lookup helper used by the snapshot.
    pub fn page_base_of(offset: u32) -> *mut u8 {
        page_base_from_offset(offset)
    }

    /// Page-header-size helper.
    pub fn page_header_size() -> usize {
        PAGE_HEADER_SIZE
    }
}
