//! Tri-color mark-sweep collector for old generation.
//!
//! Uses a worklist-based BFS with tri-color invariant:
//! - **White**: not yet visited (presumed dead).
//! - **Gray**: discovered (in worklist) but children not yet scanned.
//! - **Black**: fully scanned, all children gray or black.
//!
//! Supports **incremental marking**: the mark phase can be split across
//! multiple steps with a per-step budget (number of objects to process).
//! Between steps the mutator runs. A write barrier (see `barrier.rs`)
//! maintains the tri-color invariant during incremental marking.
//!
//! After marking, the sweep phase walks old-space pages and rebuilds
//! free lists from dead (white) objects.

use std::collections::VecDeque;

use crate::align_up;
use crate::header::{GcHeader, MarkColor};
use crate::page::{CELL_SIZE, PAGE_HEADER_SIZE};
use crate::space::OldSpace;
use crate::trace::TraceTable;

/// State of an in-progress or completed mark phase.
pub struct MarkingState {
    /// Gray worklist — objects discovered but not yet scanned.
    worklist: VecDeque<*mut GcHeader>,
    /// Whether marking is currently in progress (for write barrier activation).
    pub is_marking: bool,
    /// Number of objects marked in the current cycle.
    pub objects_marked: usize,
    /// Number of objects discovered (pushed to worklist) in the current cycle.
    pub objects_discovered: usize,
}

impl MarkingState {
    /// Creates a fresh marking state.
    pub fn new() -> Self {
        Self {
            worklist: VecDeque::with_capacity(1024),
            is_marking: false,
            objects_marked: 0,
            objects_discovered: 0,
        }
    }

    /// Starts a new marking cycle. Clears previous state.
    pub fn begin(&mut self) {
        self.worklist.clear();
        self.is_marking = true;
        self.objects_marked = 0;
        self.objects_discovered = 0;
    }

    /// Marks an object gray (pushes to worklist) if it is currently white.
    /// Returns true if the object was newly discovered.
    ///
    /// Thread-safe: uses atomic CAS on the mark color.
    ///
    /// # Safety
    ///
    /// `header` must point to a valid, live GC object.
    #[inline]
    pub unsafe fn shade_gray(&mut self, header: *mut GcHeader) -> bool {
        let h = unsafe { &*header };
        if h.try_mark_gray() {
            self.worklist.push_back(header);
            self.objects_discovered += 1;
            true
        } else {
            false
        }
    }

    /// Processes root slots: for each slot that points to a white object,
    /// shades it gray.
    ///
    /// # Safety
    ///
    /// All slot pointers must be valid and dereferenceable.
    pub unsafe fn mark_roots(&mut self, root_slots: &[*mut *const GcHeader]) {
        for &slot in root_slots {
            let obj_ptr = unsafe { *slot };
            if !obj_ptr.is_null() {
                unsafe { self.shade_gray(obj_ptr as *mut GcHeader) };
            }
        }
    }

    /// Processes root handles (raw pointers to GcHeaders, not pointer-to-pointer).
    /// Used for objects on the handle stack.
    ///
    /// # Safety
    ///
    /// All pointers must point to valid, live GC objects.
    pub unsafe fn mark_root_objects(&mut self, roots: &[*const GcHeader]) {
        for &obj_ptr in roots {
            if !obj_ptr.is_null() {
                unsafe { self.shade_gray(obj_ptr as *mut GcHeader) };
            }
        }
    }

    /// Drains the worklist completely — processes all gray objects until none
    /// remain. This is the non-incremental (stop-the-world) marking path.
    pub fn drain_worklist(&mut self, trace_table: &TraceTable) {
        while let Some(header_ptr) = self.worklist.pop_front() {
            self.process_object(header_ptr, trace_table);
        }
    }

    /// Processes up to `budget` gray objects from the worklist.
    /// Returns `true` if marking is complete (worklist empty).
    ///
    /// This is the incremental marking entry point. Call repeatedly with
    /// a budget between mutator slices to spread marking work.
    pub fn drain_with_budget(&mut self, trace_table: &TraceTable, budget: usize) -> bool {
        let mut remaining = budget;
        while remaining > 0 {
            let Some(header_ptr) = self.worklist.pop_front() else {
                break;
            };
            self.process_object(header_ptr, trace_table);
            remaining -= 1;
        }
        self.worklist.is_empty()
    }

    /// Ends the marking phase.
    pub fn finish(&mut self) {
        self.is_marking = false;
    }

    /// Whether the worklist is empty (marking is complete if all roots processed).
    pub fn is_worklist_empty(&self) -> bool {
        self.worklist.is_empty()
    }

    /// Process one gray object: trace its children (shade gray), then mark black.
    fn process_object(&mut self, header_ptr: *mut GcHeader, trace_table: &TraceTable) {
        // Collect child slots via trace table.
        let mut child_slots: Vec<*mut *const GcHeader> = Vec::new();
        unsafe {
            trace_table.trace_object(header_ptr as *const GcHeader, &mut |slot| {
                child_slots.push(slot);
            });
        }

        // Shade each child gray.
        for slot in child_slots {
            let child = unsafe { *slot };
            if !child.is_null() {
                unsafe { self.shade_gray(child as *mut GcHeader) };
            }
        }

        // Mark this object black.
        let header = unsafe { &*header_ptr };
        header.set_mark_color(MarkColor::Black);
        self.objects_marked += 1;
    }
}

impl Default for MarkingState {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Sweep phase
// ---------------------------------------------------------------------------

/// Result of a sweep pass over old-space.
#[derive(Debug, Clone)]
pub struct SweepResult {
    /// Number of live objects found.
    pub live_count: usize,
    /// Total bytes occupied by live objects.
    pub live_bytes: usize,
    /// Number of dead objects reclaimed.
    pub dead_count: usize,
    /// Total bytes freed.
    pub freed_bytes: usize,
    /// Number of pages swept.
    pub pages_swept: usize,
}

/// Sweeps all old-space pages, building free lists from dead objects.
///
/// For each page, walks the allocated region. Objects marked black (live)
/// have their mark cleared (for the next cycle). Objects still white (dead)
/// are converted to free blocks and linked into the page's free list.
///
/// # Safety
///
/// Must be called after marking is complete and before any new allocations.
/// All live objects must be marked black.
pub unsafe fn sweep_old_space(old_space: &mut OldSpace) -> SweepResult {
    let mut result = SweepResult {
        live_count: 0,
        live_bytes: 0,
        dead_count: 0,
        freed_bytes: 0,
        pages_swept: 0,
    };

    for page in old_space.pages() {
        let header_mut = page.header_mut();
        let base = page.base_ptr();
        let mut offset = PAGE_HEADER_SIZE;
        let limit = header_mut.bump_cursor;

        // Reset free list for this page.
        header_mut.free_list_head = 0;
        let mut last_free_offset: Option<usize> = None;

        // Track contiguous dead region for coalescing.
        let mut dead_start: Option<usize> = None;

        while offset < limit {
            let header_ptr = unsafe { base.add(offset) as *mut GcHeader };
            let header = unsafe { &*header_ptr };
            let size = header.size_bytes() as usize;
            if size == 0 {
                break;
            }
            let aligned_size = align_up(size, CELL_SIZE);

            if header.mark_color() == MarkColor::Black {
                // Live object — flush any preceding dead region as a free block.
                if let Some(start) = dead_start.take() {
                    let dead_size = offset - start;
                    unsafe {
                        install_free_block(
                            base,
                            start,
                            dead_size,
                            &mut last_free_offset,
                            header_mut,
                        );
                    }
                    result.dead_count += 1; // Approximate: one free block per gap
                    result.freed_bytes += dead_size;
                }

                // Clear mark for next cycle.
                header.clear_mark();
                result.live_count += 1;
                result.live_bytes += aligned_size;
            } else {
                // Dead object — extend or start a dead region.
                if dead_start.is_none() {
                    dead_start = Some(offset);
                }
            }

            offset += aligned_size;
        }

        // Flush trailing dead region.
        if let Some(start) = dead_start {
            let dead_size = offset - start;
            unsafe {
                install_free_block(base, start, dead_size, &mut last_free_offset, header_mut);
            }
            result.dead_count += 1;
            result.freed_bytes += dead_size;
        }

        header_mut.live_bytes = result.live_bytes;
        header_mut.clear_all_mark_bits();
        result.pages_swept += 1;
    }

    old_space.set_live_bytes(result.live_bytes);
    result
}

/// Installs a free block at the given offset and links it into the page's
/// free list.
///
/// # Safety
///
/// `base + offset` must be within the page's payload area and `size` must
/// be at least `MIN_FREE_BLOCK_SIZE`.
unsafe fn install_free_block(
    base: *mut u8,
    offset: usize,
    size: usize,
    last_free_offset: &mut Option<usize>,
    header: &mut crate::page::PageHeader,
) {
    use crate::space::FreeBlock;

    if size < std::mem::size_of::<FreeBlock>() {
        return; // Too small to hold a free block header — wasted space.
    }

    let block_ptr = unsafe { base.add(offset) as *mut FreeBlock };
    unsafe {
        (*block_ptr).size = size as u32;
        (*block_ptr).next_offset = 0;
    }

    // Link into the free list.
    if let Some(prev_offset) = last_free_offset {
        let prev_ptr = unsafe { base.add(*prev_offset) as *mut FreeBlock };
        unsafe {
            (*prev_ptr).next_offset = offset as u32;
        }
    } else {
        header.free_list_head = offset as u32;
    }

    *last_free_offset = Some(offset);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::header::GcHeader;
    use crate::page::CELL_SIZE;
    use crate::space::OldSpace;
    use crate::trace::TraceTable;

    const TAG_LEAF: u8 = 10;
    const TAG_NODE: u8 = 11;

    #[repr(C)]
    struct Leaf {
        header: GcHeader,
        value: u64,
    }

    #[repr(C)]
    struct Node {
        header: GcHeader,
        child: *const GcHeader,
    }

    fn trace_node(header: *const GcHeader, visit: &mut dyn FnMut(*mut *const GcHeader)) {
        let node = header as *const Node;
        let child_slot = unsafe { &raw const (*node).child } as *mut *const GcHeader;
        visit(child_slot);
    }

    fn make_trace_table() -> TraceTable {
        let mut table = TraceTable::new();
        table.register(TAG_NODE, trace_node);
        table
    }

    fn alloc_leaf(old_space: &mut OldSpace, value: u64) -> *mut GcHeader {
        let size = crate::align_up(std::mem::size_of::<Leaf>(), CELL_SIZE);
        let ptr = old_space.alloc(size).expect("alloc leaf");
        unsafe {
            let leaf = ptr.as_ptr() as *mut Leaf;
            (*leaf).header = GcHeader::new(TAG_LEAF, size as u32);
            (*leaf).value = value;
            ptr.as_ptr() as *mut GcHeader
        }
    }

    fn alloc_node(old_space: &mut OldSpace, child: *const GcHeader) -> *mut GcHeader {
        let size = crate::align_up(std::mem::size_of::<Node>(), CELL_SIZE);
        let ptr = old_space.alloc(size).expect("alloc node");
        unsafe {
            let node = ptr.as_ptr() as *mut Node;
            (*node).header = GcHeader::new(TAG_NODE, size as u32);
            (*node).child = child;
            ptr.as_ptr() as *mut GcHeader
        }
    }

    #[test]
    fn mark_roots_shades_gray() {
        let mut state = MarkingState::new();
        state.begin();

        let leaf = GcHeader::new(TAG_LEAF, 16);
        let leaf_ptr: *const GcHeader = &leaf;
        unsafe { state.mark_root_objects(&[leaf_ptr]) };

        assert_eq!(leaf.mark_color(), MarkColor::Gray);
        assert_eq!(state.objects_discovered, 1);
        assert!(!state.is_worklist_empty());
    }

    #[test]
    fn drain_worklist_marks_black() {
        let trace_table = TraceTable::new(); // Leaf: no trace fn
        let mut state = MarkingState::new();
        state.begin();

        let leaf = GcHeader::new(TAG_LEAF, 16);
        unsafe { state.mark_root_objects(&[&leaf as *const GcHeader]) };
        state.drain_worklist(&trace_table);

        assert_eq!(leaf.mark_color(), MarkColor::Black);
        assert_eq!(state.objects_marked, 1);
        assert!(state.is_worklist_empty());
    }

    #[test]
    fn marking_follows_references() {
        let trace_table = make_trace_table();
        let mut old_space = OldSpace::new();

        let leaf_ptr = alloc_leaf(&mut old_space, 42);
        let node_ptr = alloc_node(&mut old_space, leaf_ptr as *const GcHeader);

        let mut state = MarkingState::new();
        state.begin();
        unsafe { state.mark_root_objects(&[node_ptr as *const GcHeader]) };
        state.drain_worklist(&trace_table);

        // Both should be black.
        assert_eq!(unsafe { (*node_ptr).mark_color() }, MarkColor::Black);
        assert_eq!(unsafe { (*leaf_ptr).mark_color() }, MarkColor::Black);
        assert_eq!(state.objects_marked, 2);
    }

    #[test]
    fn incremental_marking_with_budget() {
        let trace_table = make_trace_table();
        let mut old_space = OldSpace::new();

        let leaf_ptr = alloc_leaf(&mut old_space, 1);
        let node_ptr = alloc_node(&mut old_space, leaf_ptr as *const GcHeader);

        let mut state = MarkingState::new();
        state.begin();
        unsafe { state.mark_root_objects(&[node_ptr as *const GcHeader]) };

        // Budget of 1: should process the node but not the leaf yet.
        let done = state.drain_with_budget(&trace_table, 1);
        assert!(!done); // Leaf is still gray in worklist.
        assert_eq!(state.objects_marked, 1);

        // Budget of 1 more: process the leaf.
        let done = state.drain_with_budget(&trace_table, 1);
        assert!(done);
        assert_eq!(state.objects_marked, 2);
    }

    #[test]
    fn sweep_frees_dead_objects() {
        let mut old_space = OldSpace::new();

        let alive = alloc_leaf(&mut old_space, 1);
        let _dead = alloc_leaf(&mut old_space, 2);

        // Mark only the alive object.
        unsafe { (*alive).set_mark_color(MarkColor::Black) };

        let result = unsafe { sweep_old_space(&mut old_space) };

        assert_eq!(result.live_count, 1);
        assert_eq!(result.dead_count, 1);
        assert!(result.freed_bytes > 0);

        // The alive object's mark should be cleared.
        assert_eq!(unsafe { (*alive).mark_color() }, MarkColor::White);
    }

    #[test]
    fn sweep_builds_free_list() {
        let mut old_space = OldSpace::new();

        let obj1 = alloc_leaf(&mut old_space, 1);
        let obj2 = alloc_leaf(&mut old_space, 2); // Will be dead
        let obj3 = alloc_leaf(&mut old_space, 3);

        // Verify all three are on the same page and obj2 is between obj1 and obj3.
        assert!(obj1 < obj2);
        assert!(obj2 < obj3);

        // Mark obj1 and obj3 as live, obj2 is dead (stays White).
        unsafe {
            (*obj1).set_mark_color(MarkColor::Black);
            (*obj3).set_mark_color(MarkColor::Black);
        }

        let result = unsafe { sweep_old_space(&mut old_space) };

        assert_eq!(result.live_count, 2);
        assert!(result.dead_count >= 1);
        assert!(result.freed_bytes > 0);

        // The page should have a free list entry where obj2 was.
        let page = &old_space.pages()[0];
        assert_ne!(
            page.header().free_list_head,
            0,
            "free_list_head should be non-zero after sweeping a dead object"
        );
    }

    #[test]
    fn full_mark_sweep_cycle() {
        let trace_table = make_trace_table();
        let mut old_space = OldSpace::new();

        // Build a graph: root → node → leaf. Also a disconnected dead leaf.
        let leaf = alloc_leaf(&mut old_space, 10);
        let node = alloc_node(&mut old_space, leaf as *const GcHeader);
        let _dead = alloc_leaf(&mut old_space, 99);

        // Mark phase.
        let mut marking = MarkingState::new();
        marking.begin();
        unsafe { marking.mark_root_objects(&[node as *const GcHeader]) };
        marking.drain_worklist(&trace_table);
        marking.finish();

        // Sweep phase.
        let result = unsafe { sweep_old_space(&mut old_space) };

        assert_eq!(result.live_count, 2); // node + leaf
        assert_eq!(result.dead_count, 1); // _dead
        assert!(result.freed_bytes > 0);
    }
}
