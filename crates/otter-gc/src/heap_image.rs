//! Capture and restore the old generation as a page image.
//!
//! A runtime spends most of its startup building a graph that never
//! changes: intrinsics, prototypes, shapes, native callables. Restoring
//! that graph is strictly cheaper than rebuilding it, and this module is
//! the mechanism — dump the old-space pages verbatim, put them back
//! later.
//!
//! # Why this is cheap here
//!
//! Every GC pointer is a 32-bit cage offset ([`crate::compressed`]), not
//! an address, so a restored page needs no address translation. What it
//! does need is *relocation*: the cage hands out pages from a free list
//! and cannot promise the same offsets twice, and a second isolate in the
//! same process could not have them anyway. Relocation is therefore a
//! uniform per-page delta added to every pointer slot, and the trace
//! table already knows where every pointer slot is — one linear pass over
//! the restored objects, no per-type fixup code.
//!
//! # Contents
//!
//! - [`HeapImage`] — the captured pages.
//! - [`GcHeap::capture_old_space`] — build one.
//! - [`GcHeap::restore_old_space`] — put one back, relocated.
//! - [`Relocation`] — the offset mapping a restore produced, so callers
//!   can rewrite the root handles they held outside the heap.
//! - [`ImageError`] — why a restore refused.
//!
//! # Invariants
//!
//! - A capture is only meaningful when the nursery is empty: the image is
//!   old space, and a pointer from it into a young object would not
//!   survive. [`GcHeap::capture_old_space`] rejects a non-empty nursery
//!   rather than producing an image that restores to a dangling edge.
//! - Every pointer slot reachable by the trace table is relocated exactly
//!   once. A slot pointing outside the image is an error, not a silently
//!   kept stale offset.
//! - Restored pages are old-generation and unmarked. The image carries no
//!   mark state; a restore starts a fresh collection cycle.
//! - Payload bytes are copied verbatim, so any non-GC pointer a body
//!   holds (a Rust `fn` entry, a `&'static str` into rodata) is copied
//!   verbatim too. Those are the caller's to fix — see
//!   [`Relocation::image_pointer_slide`].
//! - **Bodies must be self-contained.** An image carries page bytes and
//!   nothing else, so a body that owns storage outside the heap — a
//!   `Vec` slab, a `Box<[u16]>` cache — restores as a second owner of
//!   the original buffer, and a trace walk over it yields slot addresses
//!   the image does not own. Restoring such a body is unsound. The VM's
//!   own object and string bodies are not yet self-contained; moving
//!   their storage into the heap is what makes them restorable.
//!
//! # See also
//!
//! - [`crate::census`] — what a runtime actually leaves in old space.
//! - [`crate::external_refs`] — how bodies name non-GC addresses.

use crate::compressed::RawGc;
use crate::header::FREE_TAG;
use crate::heap::GcHeap;
use crate::oom::OutOfMemory;
use crate::page::{PAGE_SIZE, Page, SpaceKind};

/// Why a capture or restore refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageError {
    /// Capture ran against a heap whose nursery still held objects, so
    /// the old-space image would not have been the whole live graph.
    NurseryNotEmpty {
        /// Live objects found outside old space.
        objects: u64,
    },
    /// Restore ran against a heap that already owns old-space pages.
    /// An image is a whole generation, not an addition to one.
    OldSpaceNotEmpty {
        /// Pages the heap already held.
        pages: usize,
    },
    /// A relocated slot pointed outside every page in the image.
    DanglingSlot {
        /// The offending cage offset as captured.
        offset: u32,
    },
    /// A body carried a type tag with no trace-table registration, so its
    /// pointer slots could not be found. Register every body type before
    /// restoring.
    UnregisteredTypeTag {
        /// The tag that had no entry.
        type_tag: u8,
    },
    /// The cage could not supply the pages the image needs.
    OutOfMemory(OutOfMemory),
}

impl std::fmt::Display for ImageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NurseryNotEmpty { objects } => {
                write!(
                    f,
                    "cannot capture: {objects} live objects outside old space"
                )
            }
            Self::OldSpaceNotEmpty { pages } => {
                write!(f, "cannot restore into a heap holding {pages} old pages")
            }
            Self::DanglingSlot { offset } => {
                write!(f, "image slot {offset:#x} points outside the image")
            }
            Self::UnregisteredTypeTag { type_tag } => {
                write!(f, "image body has unregistered type tag {type_tag:#04x}")
            }
            Self::OutOfMemory(err) => write!(f, "cage could not back the image: {err}"),
        }
    }
}

impl std::error::Error for ImageError {}

impl From<OutOfMemory> for ImageError {
    fn from(value: OutOfMemory) -> Self {
        Self::OutOfMemory(value)
    }
}

/// One captured page: where it lived and the bytes that were live in it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PageImage {
    /// Cage offset of the page base at capture time.
    cage_offset: u32,
    /// Bytes `[0, bump_cursor)` of the page — its header followed by
    /// every object it had bump-allocated.
    bytes: Vec<u8>,
}

/// A captured old generation.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HeapImage {
    pages: Vec<PageImage>,
    /// Address of the binary image at capture time, so a restore can tell
    /// how far the loader moved the executable. See
    /// [`Relocation::image_pointer_slide`].
    image_anchor: usize,
    /// Live objects captured, for diagnostics and for sizing a restore.
    object_count: u64,
    /// Live bytes captured.
    live_bytes: u64,
}

impl HeapImage {
    /// Pages in the image.
    #[must_use]
    pub fn page_count(&self) -> usize {
        self.pages.len()
    }

    /// Live objects the image holds.
    #[must_use]
    pub fn object_count(&self) -> u64 {
        self.object_count
    }

    /// Live bytes the image holds.
    #[must_use]
    pub fn live_bytes(&self) -> u64 {
        self.live_bytes
    }

    /// Bytes the image occupies at rest.
    #[must_use]
    pub fn byte_len(&self) -> usize {
        self.pages.iter().map(|p| p.bytes.len()).sum()
    }
}

/// The mapping a restore produced, so callers can rewrite handles they
/// were holding outside the heap.
#[derive(Debug, Clone, Default)]
pub struct Relocation {
    /// `(captured page base, restored page base)`, ascending by captured
    /// base so a lookup can binary-search.
    pages: Vec<(u32, u32)>,
    /// How far the binary image moved between capture and restore.
    image_pointer_slide: isize,
}

impl Relocation {
    /// Rewrite one captured cage offset to where it now lives.
    ///
    /// `None` for a null offset, and for any offset that did not fall in a
    /// captured page — a caller holding one of those was holding something
    /// the image never described.
    #[must_use]
    pub fn relocate(&self, offset: u32) -> Option<u32> {
        if offset == 0 {
            return None;
        }
        let base = offset & !(PAGE_SIZE as u32 - 1);
        let index = self
            .pages
            .binary_search_by_key(&base, |(from, _)| *from)
            .ok()?;
        let (from, to) = self.pages[index];
        Some(offset - from + to)
    }

    /// Rewrite a captured handle in place, reporting whether it named a
    /// page the image carried.
    #[must_use]
    pub fn relocate_raw(&self, raw: RawGc) -> Option<RawGc> {
        if raw.is_null() {
            return Some(raw);
        }
        self.relocate(raw.0).map(RawGc)
    }

    /// Rewrite a typed handle to where its object now lives.
    ///
    /// Safe: the offset stays inside the same object the caller already
    /// held, so the payload type behind it is unchanged.
    #[must_use]
    pub fn relocate_gc<T: ?Sized>(
        &self,
        handle: crate::compressed::Gc<T>,
    ) -> Option<crate::compressed::Gc<T>> {
        if handle.is_null() {
            return Some(handle);
        }
        // SAFETY: the relocated offset names the same object at its new
        // page, so it still precedes a `T` payload.
        self.relocate(handle.offset())
            .map(|offset| unsafe { crate::compressed::Gc::from_offset(offset) })
    }

    /// How far the binary image moved between the capture process and
    /// this one.
    ///
    /// Payload bytes are copied verbatim, so a body that stored a Rust
    /// `fn` entry or a `&'static str` still holds the capturing process's
    /// address. Adding this slide to such a pointer yields the address it
    /// has here. Zero when both processes loaded the image at the same
    /// place, which is the common case only without ASLR.
    #[must_use]
    pub fn image_pointer_slide(&self) -> isize {
        self.image_pointer_slide
    }

    /// Apply [`Self::image_pointer_slide`] to one captured address.
    #[must_use]
    pub fn slide_image_pointer(&self, addr: usize) -> usize {
        if addr == 0 {
            return 0;
        }
        addr.wrapping_add_signed(self.image_pointer_slide)
    }
}

/// A function in this crate, used only for its address: the difference
/// between its address in two processes is how far the loader moved the
/// whole binary image.
fn image_anchor() -> usize {
    image_anchor as *const () as usize
}

impl GcHeap {
    /// Capture the old generation as a relocatable page image.
    ///
    /// # Errors
    ///
    /// [`ImageError::NurseryNotEmpty`] when live objects sit outside old
    /// space; the image would then describe only part of the graph.
    pub fn capture_old_space(&self) -> Result<HeapImage, ImageError> {
        let census = self.census();
        let stray = census.young.object_count + census.large.object_count;
        if stray != 0 {
            return Err(ImageError::NurseryNotEmpty { objects: stray });
        }
        let mut pages = Vec::new();
        for page in self.census_spaces()[0].1 {
            let header = page.header();
            let len = header.bump_cursor;
            // SAFETY: `[0, bump_cursor)` is initialised page memory — the
            // header plus every object bump-allocated into it.
            let bytes = unsafe { std::slice::from_raw_parts(page.base_ptr(), len) }.to_vec();
            pages.push(PageImage {
                cage_offset: page.cage_offset(),
                bytes,
            });
        }
        pages.sort_by_key(|p| p.cage_offset);
        Ok(HeapImage {
            pages,
            image_anchor: image_anchor(),
            object_count: census.old.object_count,
            live_bytes: census.old.live_bytes,
        })
    }

    /// Restore a captured old generation into this heap, relocating every
    /// pointer slot to wherever the cage placed the pages.
    ///
    /// The heap must already have every body type registered — the
    /// relocation pass finds pointer slots through the trace table.
    ///
    /// # Errors
    ///
    /// See [`ImageError`].
    pub fn restore_old_space(&mut self, image: &HeapImage) -> Result<Relocation, ImageError> {
        if self.old_space_page_count() != 0 {
            return Err(ImageError::OldSpaceNotEmpty {
                pages: self.old_space_page_count(),
            });
        }
        // 1) Place the pages and record where each one landed.
        let mut placed: Vec<(u32, Page)> = Vec::with_capacity(image.pages.len());
        for entry in &image.pages {
            let page = Page::new(SpaceKind::Old).ok_or(OutOfMemory::CageExhausted)?;
            // SAFETY: `page` is a live cage page of `PAGE_SIZE` bytes and
            // `entry.bytes` is at most that long; the two never overlap.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    entry.bytes.as_ptr(),
                    page.base_ptr(),
                    entry.bytes.len(),
                );
            }
            // The copied header still names the capturing page.
            let header = page.header_mut();
            header.cage_offset = page.cage_offset();
            header.space = SpaceKind::Old;
            placed.push((entry.cage_offset, page));
        }
        let mut relocation = Relocation {
            pages: placed
                .iter()
                .map(|(from, page)| (*from, page.cage_offset()))
                .collect(),
            image_pointer_slide: (image_anchor() as isize)
                .wrapping_sub(image.image_anchor as isize),
        };
        relocation.pages.sort_by_key(|(from, _)| *from);

        // 2) Rewrite every pointer slot in the restored objects. Done
        // before the pages are adopted so a failure leaves the heap
        // untouched and the pages simply drop back to the cage.
        let mut failure: Option<ImageError> = None;
        for (_, page) in &placed {
            // SAFETY: the copied bytes are a faithful page prefix, so every
            // header up to `bump_cursor` is one this heap's trace table can
            // describe.
            unsafe {
                page.for_each_object(|header, _| {
                    if failure.is_some() {
                        return;
                    }
                    let tag = (*header).type_tag();
                    if tag == FREE_TAG {
                        return;
                    }
                    // A restored generation starts a fresh cycle.
                    (*header).clear_mark();
                    let Some(trace) = self.trace_table().get(tag) else {
                        failure = Some(ImageError::UnregisteredTypeTag { type_tag: tag });
                        return;
                    };
                    trace(header, &mut |slot: *mut RawGc| {
                        let old = *slot;
                        if old.is_null() {
                            return;
                        }
                        match relocation.relocate(old.0) {
                            Some(new) => *slot = RawGc(new),
                            None => {
                                if failure.is_none() {
                                    failure = Some(ImageError::DanglingSlot { offset: old.0 });
                                }
                            }
                        }
                    });
                });
            }
        }
        if let Some(err) = failure {
            return Err(err);
        }

        // 3) Adopt the pages and account for what they hold.
        for (_, page) in placed {
            self.adopt_restored_old_page(page);
        }
        self.account_restored_image(image.live_bytes);
        Ok(relocation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compressed::{CAGE_TEST_LOCK, Gc};
    use crate::test_support::{OpaqueLeaf, OpaquePair};

    /// Build a heap holding a small pointer-bearing graph in old space.
    fn tenured_heap() -> (GcHeap, Gc<OpaquePair>) {
        let mut heap = GcHeap::new().expect("heap");
        heap.set_tenure_all(true);
        let leaf = heap.alloc(OpaqueLeaf { payload: 7 }).expect("leaf");
        let other = heap.alloc(OpaqueLeaf { payload: 9 }).expect("leaf");
        let pair = heap
            .alloc(OpaquePair {
                first: leaf.raw(),
                second: other.raw(),
            })
            .expect("pair");
        heap.set_tenure_all(false);
        (heap, pair)
    }

    #[test]
    fn capture_refuses_a_non_empty_nursery() {
        let _guard = CAGE_TEST_LOCK.lock().expect("cage test lock");
        let mut heap = GcHeap::new().expect("heap");
        heap.alloc(OpaqueLeaf { payload: 1 }).expect("young leaf");
        assert!(matches!(
            heap.capture_old_space(),
            Err(ImageError::NurseryNotEmpty { .. })
        ));
    }

    #[test]
    fn round_trip_preserves_the_graph_through_relocation() {
        let _guard = CAGE_TEST_LOCK.lock().expect("cage test lock");
        let (source, pair) = tenured_heap();
        let image = source.capture_old_space().expect("capture");
        assert_eq!(image.object_count(), 3);
        assert!(image.page_count() >= 1);

        let mut target = GcHeap::new().expect("target heap");
        // The target must know the body types before pointer slots can be
        // found; allocating one of each is the same registration the
        // interpreter performs at construction.
        target.register_traceable::<OpaqueLeaf>();
        target.register_traceable::<OpaquePair>();
        let relocation = target.restore_old_space(&image).expect("restore");

        let restored: Gc<OpaquePair> = unsafe {
            Gc::from_offset(
                relocation
                    .relocate_raw(pair.raw())
                    .expect("pair was captured")
                    .0,
            )
        };
        let (first, second) = target.read_payload(restored, |p| (p.first, p.second));
        // Both edges must now name the restored copies, not the source
        // heap's offsets.
        let first_leaf: Gc<OpaqueLeaf> = unsafe { Gc::from_offset(first.0) };
        let second_leaf: Gc<OpaqueLeaf> = unsafe { Gc::from_offset(second.0) };
        assert_eq!(target.read_payload(first_leaf, |l| l.payload), 7);
        assert_eq!(target.read_payload(second_leaf, |l| l.payload), 9);
        // And the source heap is untouched.
        let source_first: Gc<OpaqueLeaf> =
            unsafe { Gc::from_offset(source.read_payload(pair, |p| p.first).0) };
        assert_eq!(source.read_payload(source_first, |l| l.payload), 7);
    }

    #[test]
    fn restore_refuses_a_heap_that_already_holds_old_pages() {
        let _guard = CAGE_TEST_LOCK.lock().expect("cage test lock");
        let (source, _pair) = tenured_heap();
        let image = source.capture_old_space().expect("capture");
        let (mut target, _) = tenured_heap();
        target.register_traceable::<OpaqueLeaf>();
        target.register_traceable::<OpaquePair>();
        assert!(matches!(
            target.restore_old_space(&image),
            Err(ImageError::OldSpaceNotEmpty { .. })
        ));
    }
}

#[cfg(test)]
mod self_contained_tests {
    use super::*;
    use crate::compressed::{CAGE_TEST_LOCK, Gc};
    use crate::test_support::{OpaqueLeaf, OpaqueVector};

    /// A body whose references live in trailing storage inside its own
    /// cell restores completely: the array travels with the object, and
    /// relocation rewrites every element.
    ///
    /// This is the property the VM's own bodies do not yet have. An
    /// object that keeps its overflow slab in a `Vec` restores as a
    /// second owner of the original buffer; this one owns nothing
    /// outside the heap, so the image is the whole truth about it.
    #[test]
    fn a_body_with_trailing_storage_restores_whole() {
        let _guard = CAGE_TEST_LOCK.lock().expect("cage test lock");
        let mut source = GcHeap::new().expect("heap");
        source.set_tenure_all(true);

        const LEN: usize = 64;
        let mut leaves = Vec::with_capacity(LEN);
        for payload in 0..LEN as u64 {
            leaves.push(source.alloc(OpaqueLeaf { payload }).expect("leaf"));
        }
        let vector: Gc<OpaqueVector> = source
            .alloc_variable_with_roots(
                OpaqueVector::new(LEN),
                OpaqueVector::trailing_bytes(LEN),
                &mut |_| {},
            )
            .expect("vector");
        for (index, leaf) in leaves.iter().enumerate() {
            source.with_payload(vector, |v| {
                v.set(index, leaf.raw());
                true
            });
        }
        source.set_tenure_all(false);

        let image = source.capture_old_space().expect("capture");
        let mut target = GcHeap::new().expect("target heap");
        target.register_traceable::<OpaqueLeaf>();
        target.register_traceable::<OpaqueVector>();
        let relocation = target.restore_old_space(&image).expect("restore");

        let restored = relocation.relocate_gc(vector).expect("vector was captured");
        assert_eq!(target.read_payload(restored, OpaqueVector::len), LEN);
        for (index, source_leaf) in leaves.iter().enumerate() {
            let element = target.read_payload(restored, |v| v.get(index));
            let leaf: Gc<OpaqueLeaf> = relocation
                .relocate_gc(*source_leaf)
                .expect("leaf was captured");
            assert_eq!(element, leaf.raw(), "element {index} must be relocated");
            assert_eq!(
                target.read_payload(leaf, |l| l.payload),
                index as u64,
                "and must still name the right object",
            );
        }
    }

    /// Trailing storage starts zeroed, so a body may treat it as empty
    /// slots without writing each one.
    #[test]
    fn trailing_storage_starts_zeroed() {
        let _guard = CAGE_TEST_LOCK.lock().expect("cage test lock");
        let mut heap = GcHeap::new().expect("heap");
        let vector: Gc<OpaqueVector> = heap
            .alloc_variable_with_roots(
                OpaqueVector::new(16),
                OpaqueVector::trailing_bytes(16),
                &mut |_| {},
            )
            .expect("vector");
        for index in 0..16 {
            assert!(heap.read_payload(vector, |v| v.get(index)).is_null());
        }
    }
}
