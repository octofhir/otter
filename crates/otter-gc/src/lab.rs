//! Linear allocation buffer: the mutator's nursery bump window.
//!
//! Ordinary young allocation carves cells from a `[top, limit)` window the
//! heap holds directly, so the common case is one add, one compare and one
//! store — no page walk, no heap-cap arithmetic, no stress or major-GC test.
//! Those policies run once per window, when the slow path refills it.
//! Generated code bumps the same two words through
//! [`crate::MachineAllocationWindow::lab`], so compiled and runtime
//! allocation share one cursor.
//!
//! # Contents
//!
//! - [`LinearAllocationArea`] — the `#[repr(C)]` `{top, limit}` pair of
//!   absolute addresses. `top == limit == 0` is the empty window: every
//!   bump misses without a separate test.
//!
//! # Invariants
//!
//! - The window is non-empty only while plain young allocation is legal:
//!   never during incremental marking (new cells must be born black),
//!   under GC stress (every allocation must reach the stress counter), or
//!   while bootstrap tenuring routes allocation to old space.
//! - The window is always the tail of one from-space page. While it is
//!   live that page's bump cursor lags `top`, and nothing else bumps the
//!   page; the heap publishes `top` into the page before any page walk and
//!   retires the window before every collection or direct nursery bump.
//! - With a heap cap configured, the whole window is charged to the cap
//!   when it is taken and the unused tail is refunded when it is retired,
//!   so a bump never has to account.
//! - The cage base is 4 GiB-aligned, so the low 32 bits of `top` are the
//!   cell's cage offset.
//!
//! # See also
//!
//! - V8 `LinearAllocationArea` (`src/heap/linear-allocation-area.h`) and
//!   the new-space allocation top/limit external references compiled code
//!   bumps inline; JSC `FreeList` bump intervals follow the same shape.
//! - [`crate::heap::GcHeap`] — refill, retire and publication.

/// Nursery bump window `[top, limit)` in absolute addresses.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LinearAllocationArea {
    /// Address of the next free byte.
    pub top: usize,
    /// One past the last byte the window may hand out.
    pub limit: usize,
}

impl LinearAllocationArea {
    /// The window that serves no allocation.
    pub const EMPTY: Self = Self { top: 0, limit: 0 };

    /// Carve `size` bytes, returning the cell address.
    #[inline(always)]
    pub fn bump(&mut self, size: usize) -> Option<usize> {
        let top = self.top;
        let next = top.wrapping_add(size);
        if next > self.limit {
            return None;
        }
        self.top = next;
        Some(top)
    }

    /// Bytes the window can still hand out.
    #[must_use]
    pub const fn remaining(&self) -> usize {
        self.limit - self.top
    }
}

/// Byte offset of [`LinearAllocationArea::top`], for generated code.
pub const LAB_TOP_OFFSET: u32 = std::mem::offset_of!(LinearAllocationArea, top) as u32;
/// Byte offset of [`LinearAllocationArea::limit`], for generated code.
pub const LAB_LIMIT_OFFSET: u32 = std::mem::offset_of!(LinearAllocationArea, limit) as u32;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_window_misses_every_bump() {
        let mut lab = LinearAllocationArea::EMPTY;
        assert_eq!(lab.bump(8), None);
        assert_eq!(lab, LinearAllocationArea::EMPTY);
    }

    #[test]
    fn bump_stops_at_limit() {
        let mut lab = LinearAllocationArea {
            top: 0x1000,
            limit: 0x1020,
        };
        assert_eq!(lab.bump(16), Some(0x1000));
        assert_eq!(lab.bump(16), Some(0x1010));
        assert_eq!(lab.bump(8), None);
        assert_eq!(lab.remaining(), 0);
    }

    #[test]
    fn generated_code_offsets_are_frozen() {
        assert_eq!(LAB_TOP_OFFSET, 0);
        assert_eq!(LAB_LIMIT_OFFSET, 8);
    }
}
