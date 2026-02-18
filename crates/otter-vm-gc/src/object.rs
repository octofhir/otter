//! GC object layout

use std::sync::atomic::{AtomicU32, AtomicU8, Ordering};

/// Global mark version counter.
/// Bumped at the start of each GC cycle instead of iterating all objects
/// to reset marks to White. An object is "white" (unmarked) if its
/// `mark_version` doesn't match this global counter — O(1) phase reset.
///
/// u32 (4 billion cycles) prevents the wrap-around correctness bug that
/// u8 had after 256 incremental GC cycles.
static MARK_VERSION: AtomicU32 = AtomicU32::new(0);

/// Get the current global mark version.
#[inline]
pub fn current_mark_version() -> u32 {
    MARK_VERSION.load(Ordering::Acquire)
}

/// Bump the global mark version (O(1) mark reset).
///
/// After bumping, all objects are effectively "white" because their
/// `mark_version` no longer matches the new global version.
#[inline]
pub fn bump_mark_version() -> u32 {
    MARK_VERSION.fetch_add(1, Ordering::AcqRel).wrapping_add(1)
}

/// GC object header (8 bytes, repr(C), alignment 4)
#[repr(C)]
pub struct GcHeader {
    /// Mark bits for tri-color marking (White=0, Gray=1, Black=2)
    mark: AtomicU8,
    /// Object type tag
    tag: u8,
    /// Explicit padding to align `mark_version` to a 4-byte boundary.
    _pad: [u8; 2],
    /// Logical mark version. Object is "white" if
    /// this doesn't match `MARK_VERSION`. u32 prevents wrap-around after
    /// 256 GC cycles that u8 was susceptible to.
    mark_version: AtomicU32,
}

/// Mark color for tri-color marking
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkColor {
    /// Not yet visited
    White = 0,
    /// In worklist
    Gray = 1,
    /// Fully scanned
    Black = 2,
}

impl GcHeader {
    /// Create new header
    pub const fn new(tag: u8) -> Self {
        Self {
            mark: AtomicU8::new(MarkColor::White as u8),
            tag,
            _pad: [0; 2],
            mark_version: AtomicU32::new(0),
        }
    }

    /// Get mark color, taking logical versioning into account.
    ///
    /// If this object's `mark_version` doesn't match the global version,
    /// it's considered White (unmarked) regardless of the mark byte.
    #[inline]
    pub fn mark(&self) -> MarkColor {
        if self.mark_version.load(Ordering::Acquire) != current_mark_version() {
            return MarkColor::White;
        }
        match self.mark.load(Ordering::Acquire) {
            1 => MarkColor::Gray,
            2 => MarkColor::Black,
            _ => MarkColor::White,
        }
    }

    /// Set mark color.
    ///
    /// Also stamps the current global `mark_version` so the object is
    /// recognized as belonging to the current GC cycle.
    #[inline]
    pub fn set_mark(&self, color: MarkColor) {
        self.mark.store(color as u8, Ordering::Release);
        self.mark_version
            .store(current_mark_version(), Ordering::Release);
    }

    /// Get object tag
    pub fn tag(&self) -> u8 {
        self.tag
    }
}

impl Clone for GcHeader {
    fn clone(&self) -> Self {
        // Cloned header starts with White mark (fresh GC state)
        Self {
            mark: AtomicU8::new(MarkColor::White as u8),
            tag: self.tag,
            _pad: [0; 2],
            mark_version: AtomicU32::new(0),
        }
    }
}

/// Trait for GC-managed objects
pub trait GcObject {
    /// Get the GC header
    fn header(&self) -> &GcHeader;

    /// Trace references to other objects
    fn trace(&self, tracer: &mut dyn FnMut(*const GcHeader));
}

/// Object type tags
pub mod tags {
    /// String object
    pub const STRING: u8 = 1;
    /// Array object
    pub const ARRAY: u8 = 2;
    /// Plain object
    pub const OBJECT: u8 = 3;
    /// Function object
    pub const FUNCTION: u8 = 4;
    /// Closure object
    pub const CLOSURE: u8 = 5;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_header_marking() {
        let header = GcHeader::new(tags::OBJECT);
        assert_eq!(header.mark(), MarkColor::White);

        header.set_mark(MarkColor::Gray);
        assert_eq!(header.mark(), MarkColor::Gray);

        header.set_mark(MarkColor::Black);
        assert_eq!(header.mark(), MarkColor::Black);
    }

    #[test]
    fn test_logical_versioning() {
        let header = GcHeader::new(tags::OBJECT);

        // Mark it black in current version
        header.set_mark(MarkColor::Black);
        assert_eq!(header.mark(), MarkColor::Black);

        // Bump version → header is now white (version mismatch)
        bump_mark_version();
        assert_eq!(header.mark(), MarkColor::White);

        // Re-mark it in the new version
        header.set_mark(MarkColor::Gray);
        assert_eq!(header.mark(), MarkColor::Gray);
    }
}
