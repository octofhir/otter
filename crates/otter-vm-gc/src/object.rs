//! GC object layout

use std::sync::atomic::{AtomicU8, Ordering};

/// GC object header
#[repr(C)]
pub struct GcHeader {
    /// Mark bits for tri-color marking
    mark: AtomicU8,
    /// Object type tag
    tag: u8,
    /// Reserved
    _reserved: [u8; 6],
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
            _reserved: [0; 6],
        }
    }

    /// Get mark color
    pub fn mark(&self) -> MarkColor {
        match self.mark.load(Ordering::Acquire) {
            0 => MarkColor::White,
            1 => MarkColor::Gray,
            _ => MarkColor::Black,
        }
    }

    /// Set mark color
    pub fn set_mark(&self, color: MarkColor) {
        self.mark.store(color as u8, Ordering::Release);
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
            _reserved: [0; 6],
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
}
