//! Top-level GC heap — coordinates spaces, triggers collection, public API.
//!
//! The `GcHeap` is the single owner of all GC-managed memory. It drives
//! allocation, collection triggers, and provides the interface that the
//! interpreter uses for allocation and rooting.

use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::align_up;
use crate::barrier::WriteBarrier;
use crate::handle::{GlobalHandle, GlobalHandleTable, HandleScopeLevel, HandleStack, LocalHandle};
use crate::header::{GcHeader, HEADER_SIZE};
use crate::marking::{MarkingState, SweepResult, sweep_old_space};
use crate::page::CELL_SIZE;
use crate::scavenger::{ScavengeResult, scavenge};
use crate::space::{LargeObjectSpace, NewSpace, OldSpace};
use crate::trace::TraceTable;

/// Configuration for the GC heap.
#[derive(Debug, Clone)]
pub struct GcConfig {
    /// Young generation capacity in bytes. Default: 4 MB.
    pub young_gen_size: usize,
    /// Old generation byte threshold for triggering a full GC. Default: 8 MB.
    /// After each GC, the threshold is set to `2 * live_bytes` (adaptive).
    pub old_gen_threshold: usize,
    /// Hard cap for the total heap size (young + old + large-object, in bytes).
    /// `None` disables the cap (legacy behavior — the GC will only trigger on
    /// generational thresholds). This is the Otter analogue of Node.js's
    /// `--max-old-space-size`.
    pub max_heap_bytes: Option<usize>,
    /// Soft-limit ratio in `(0.0, 1.0]`. When the projected heap size crosses
    /// `soft_limit_ratio * max_heap_bytes`, a young GC is triggered early. The
    /// hard limit is the full value of `max_heap_bytes`. Default: 0.85.
    pub soft_limit_ratio: f32,
}

impl Default for GcConfig {
    fn default() -> Self {
        Self {
            young_gen_size: 4 * 1024 * 1024,
            old_gen_threshold: 8 * 1024 * 1024,
            max_heap_bytes: None,
            soft_limit_ratio: 0.85,
        }
    }
}

/// Aggregate GC statistics.
#[derive(Debug, Clone, Default)]
pub struct GcStats {
    pub scavenges: u32,
    pub full_collections: u32,
    pub total_scavenged_bytes: usize,
    pub total_promoted_bytes: usize,
    pub total_swept_bytes: usize,
    pub young_gen_bytes: usize,
    pub old_gen_bytes: usize,
    pub large_object_bytes: usize,
}

/// The top-level garbage-collected heap.
///
/// Owns all spaces, the handle stack, write barrier, and trace table.
/// Single-threaded: one isolate = one heap = one thread.
pub struct GcHeap {
    new_space: NewSpace,
    old_space: OldSpace,
    large_space: LargeObjectSpace,
    trace_table: TraceTable,
    handle_stack: HandleStack,
    global_handles: GlobalHandleTable,
    write_barrier: WriteBarrier,
    marking: MarkingState,
    config: GcConfig,
    stats: GcStats,
    /// Out-of-memory flag. Set by allocation paths when the hard heap limit
    /// is exceeded after emergency GC. Polled by the interpreter at GC
    /// safepoints to raise a catchable `RangeError`. Cloned from the heap
    /// via [`GcHeap::oom_flag`] so embedders share the same signal.
    oom_flag: Arc<AtomicBool>,
}

impl GcHeap {
    /// Creates a new heap with the given configuration.
    pub fn new(config: GcConfig) -> Self {
        let new_space = NewSpace::new(config.young_gen_size)
            .expect("failed to allocate initial young generation page");
        Self {
            new_space,
            old_space: OldSpace::new(),
            large_space: LargeObjectSpace::new(),
            trace_table: TraceTable::new(),
            handle_stack: HandleStack::new(),
            global_handles: GlobalHandleTable::new(),
            write_barrier: WriteBarrier::new(),
            marking: MarkingState::new(),
            config,
            stats: GcStats::default(),
            oom_flag: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Creates a heap with default configuration.
    pub fn with_defaults() -> Self {
        Self::new(GcConfig::default())
    }

    /// Creates a heap with the default configuration plus a hard cap on the
    /// total heap size (analogue of Node.js's `--max-old-space-size`).
    pub fn with_max_heap_bytes(max_bytes: usize) -> Self {
        Self::new(GcConfig {
            max_heap_bytes: Some(max_bytes),
            ..GcConfig::default()
        })
    }

    /// Returns a clone of the OOM signal flag. The interpreter polls this at
    /// GC safepoints and raises `RangeError: out of memory` when set.
    pub fn oom_flag(&self) -> Arc<AtomicBool> {
        self.oom_flag.clone()
    }

    /// Clears the OOM flag. Called by the runtime when starting a new script
    /// so one OOM does not cause every subsequent script to abort immediately.
    pub fn clear_oom_flag(&self) {
        self.oom_flag.store(false, Ordering::Relaxed);
    }

    /// Returns the current total allocated bytes across all spaces.
    ///
    /// This is an exact running total maintained by the spaces themselves
    /// (never cached), so it is safe to call from the hot allocation path.
    #[inline]
    pub fn current_heap_bytes(&self) -> usize {
        self.new_space.allocated_bytes()
            + self.old_space.allocated_bytes()
            + self.large_space.allocated_bytes()
    }

    /// Returns the configured hard cap on the heap size, if any.
    #[inline]
    pub fn max_heap_bytes(&self) -> Option<usize> {
        self.config.max_heap_bytes
    }

    /// Computes the soft limit in bytes, rounded down. Returns `None` if no
    /// hard limit is configured.
    #[inline]
    fn soft_limit_bytes(&self) -> Option<usize> {
        self.config.max_heap_bytes.map(|max| {
            let ratio = self.config.soft_limit_ratio.clamp(0.1, 1.0);
            (max as f64 * ratio as f64) as usize
        })
    }

    /// Emergency GC sequence when an allocation is about to exceed a limit.
    ///
    /// Two-phase, mirroring V8's Orinoco fallback: first a scavenge to
    /// reclaim the young generation, then — if still over — a full GC to
    /// reclaim old/large spaces. Returns `true` if the projected allocation
    /// fits under the hard cap after reclamation.
    fn try_reclaim(&mut self, incoming: usize) -> bool {
        let hard_limit = match self.config.max_heap_bytes {
            Some(limit) => limit,
            None => return true,
        };

        // Phase 1: scavenge young generation.
        self.collect_young();
        if self.current_heap_bytes() + incoming <= hard_limit {
            return true;
        }

        // Phase 2: full old-generation GC.
        self.collect_full();
        if self.current_heap_bytes() + incoming <= hard_limit {
            return true;
        }

        false
    }

    /// Checks whether allocating `aligned` bytes would cross the configured
    /// heap limits and, if so, triggers GC. Sets `oom_flag` and returns
    /// `false` when the hard cap cannot be respected even after a full GC.
    ///
    /// When `None`/no-limit is configured this is a cheap no-op.
    #[inline]
    fn enforce_heap_limit(&mut self, aligned: usize) -> bool {
        let hard_limit = match self.config.max_heap_bytes {
            Some(limit) => limit,
            None => return true,
        };

        let current = self.current_heap_bytes();
        let projected = current.saturating_add(aligned);

        // Fast path: well below any limit — no-op.
        if let Some(soft) = self.soft_limit_bytes()
            && projected < soft
        {
            return true;
        }

        // Soft-limit crossing: trigger a young GC early. If that restores
        // headroom below the soft limit we're done; no need for a full GC.
        if projected >= self.soft_limit_bytes().unwrap_or(hard_limit) && projected < hard_limit {
            self.collect_young();
            if self.current_heap_bytes().saturating_add(aligned) < hard_limit {
                return true;
            }
        }

        // Hard-limit path: run the full emergency sequence.
        if self.current_heap_bytes().saturating_add(aligned) >= hard_limit
            && !self.try_reclaim(aligned)
        {
            self.oom_flag.store(true, Ordering::Relaxed);
            return false;
        }

        true
    }

    // -----------------------------------------------------------------------
    // Trace table registration
    // -----------------------------------------------------------------------

    /// Registers a trace function for the given type tag.
    pub fn register_trace_fn(&mut self, type_tag: u8, trace_fn: crate::trace::TraceFn) {
        self.trace_table.register(type_tag, trace_fn);
    }

    // -----------------------------------------------------------------------
    // Allocation
    // -----------------------------------------------------------------------

    /// Allocates `size` bytes in the young generation and returns a pointer
    /// to the start of the allocation (where the GcHeader should be written).
    ///
    /// If young space is full, triggers a scavenge first.
    /// If the object is too large for young space, allocates in large object space.
    ///
    /// When a hard heap cap is configured (see [`GcConfig::max_heap_bytes`]),
    /// the limit is enforced before the allocation. If the cap cannot be
    /// respected even after an emergency full GC, the OOM flag is raised and
    /// `None` is returned.
    ///
    /// The caller must immediately write a valid `GcHeader` at the returned
    /// pointer and initialize the object payload.
    pub fn alloc_young(&mut self, size: usize) -> Option<NonNull<u8>> {
        let aligned = align_up(size, CELL_SIZE);
        debug_assert!(aligned >= HEADER_SIZE);

        // Hard heap cap check (no-op when unconfigured).
        if !self.enforce_heap_limit(aligned) {
            return None;
        }

        // Large objects go directly to large object space.
        if aligned > crate::page::PAGE_PAYLOAD_SIZE / 2 {
            return self.large_space.alloc(aligned);
        }

        // Try young gen first.
        if let Some(ptr) = self.new_space.alloc(aligned) {
            return Some(ptr);
        }

        // Young gen full — trigger scavenge.
        self.collect_young();

        // Retry after scavenge.
        self.new_space.alloc(aligned)
    }

    /// Allocates `size` bytes directly in old generation.
    /// Used for promoted objects and pre-tenured allocations.
    ///
    /// Respects the hard heap cap like [`GcHeap::alloc_young`].
    pub fn alloc_old(&mut self, size: usize) -> Option<NonNull<u8>> {
        let aligned = align_up(size, CELL_SIZE);
        if !self.enforce_heap_limit(aligned) {
            return None;
        }
        self.old_space.alloc(aligned)
    }

    // -----------------------------------------------------------------------
    // Handle stack (rooting)
    // -----------------------------------------------------------------------

    /// Pushes a GC pointer onto the handle stack, returning a local handle.
    #[inline]
    pub fn root(&mut self, ptr: *const GcHeader) -> LocalHandle {
        let index = self.handle_stack.push(ptr);
        LocalHandle::from_raw(index)
    }

    /// Reads the pointer for a local handle.
    #[inline]
    pub fn deref_local(&self, handle: LocalHandle) -> Option<*const GcHeader> {
        self.handle_stack.get(handle.index())
    }

    /// Enters a new handle scope, returning the saved level.
    pub fn enter_scope(&self) -> HandleScopeLevel {
        HandleScopeLevel::enter(&self.handle_stack)
    }

    /// Exits a handle scope, releasing all handles created since entry.
    pub fn exit_scope(&mut self, scope: HandleScopeLevel) {
        scope.exit(&mut self.handle_stack);
    }

    // -----------------------------------------------------------------------
    // Global handles
    // -----------------------------------------------------------------------

    /// Creates a global (persistent) handle.
    pub fn create_global(&mut self, ptr: *const GcHeader) -> GlobalHandle {
        self.global_handles.create(ptr)
    }

    /// Reads the pointer for a global handle.
    pub fn deref_global(&self, handle: GlobalHandle) -> Option<*const GcHeader> {
        self.global_handles.get(handle)
    }

    /// Releases a global handle.
    pub fn release_global(&mut self, handle: GlobalHandle) {
        self.global_handles.release(handle);
    }

    // -----------------------------------------------------------------------
    // Write barrier
    // -----------------------------------------------------------------------

    /// Records a pointer store for the write barrier.
    ///
    /// Must be called after every store of a GC pointer into a heap object.
    ///
    /// # Safety
    ///
    /// `source` and `target` must be valid GC object pointers (or target null).
    #[inline]
    pub unsafe fn write_barrier(
        &mut self,
        source: *const GcHeader,
        slot: *mut *const GcHeader,
        target: *const GcHeader,
    ) {
        unsafe {
            self.write_barrier.record(source, slot, target);
        }
    }

    // -----------------------------------------------------------------------
    // Collection
    // -----------------------------------------------------------------------

    /// Triggers a young generation (scavenge) collection.
    pub fn collect_young(&mut self) -> ScavengeResult {
        // Gather roots: handle stack + global handles + remembered set.
        let mut root_slots = self.handle_stack.root_slots();
        root_slots.extend(self.global_handles.root_slots());
        root_slots.extend_from_slice(self.write_barrier.remembered_set.slots());

        let result = unsafe {
            scavenge(
                &mut self.new_space,
                &mut self.old_space,
                &self.trace_table,
                &root_slots,
            )
        };

        // Clear remembered set — it was consumed by the scavenger.
        self.write_barrier.remembered_set.clear();

        self.stats.scavenges += 1;
        self.stats.total_scavenged_bytes += result.copied_bytes;
        self.stats.total_promoted_bytes += result.promoted_bytes;
        self.update_stats();

        result
    }

    /// Triggers a full (mark-sweep) collection of old generation.
    pub fn collect_full(&mut self) -> SweepResult {
        // Phase 1: Mark.
        self.marking.begin();

        // Roots: handle stack + global handles.
        let root_ptrs: Vec<*const GcHeader> = self.handle_stack.root_pointers().to_vec();
        unsafe { self.marking.mark_root_objects(&root_ptrs) };

        let global_ptrs: Vec<*const GcHeader> = self
            .global_handles
            .root_slots()
            .iter()
            .map(|slot| unsafe { **slot })
            .collect();
        unsafe { self.marking.mark_root_objects(&global_ptrs) };

        // Drain worklist (stop-the-world).
        self.marking.drain_worklist(&self.trace_table);
        self.marking.finish();

        // Phase 2: Sweep.
        let result = unsafe { sweep_old_space(&mut self.old_space) };

        self.stats.full_collections += 1;
        self.stats.total_swept_bytes += result.freed_bytes;

        // Adaptive threshold: 2x live bytes, minimum = initial threshold.
        let min_threshold = self.config.old_gen_threshold;
        self.config.old_gen_threshold = (result.live_bytes * 2).max(min_threshold);

        self.update_stats();
        result
    }

    /// Whether a young-gen collection should be triggered.
    pub fn should_collect_young(&self) -> bool {
        self.new_space.should_scavenge()
    }

    /// Whether a full collection should be triggered.
    pub fn should_collect_full(&self) -> bool {
        self.old_space.allocated_bytes() >= self.config.old_gen_threshold
    }

    /// Performs the appropriate collection based on current memory pressure.
    /// Called at GC safepoints (loop back-edges, function calls).
    pub fn maybe_collect(&mut self) {
        if self.should_collect_young() {
            self.collect_young();
        }
        if self.should_collect_full() {
            self.collect_full();
        }
    }

    // -----------------------------------------------------------------------
    // Accessors
    // -----------------------------------------------------------------------

    pub fn stats(&self) -> &GcStats {
        &self.stats
    }
    pub fn new_space(&self) -> &NewSpace {
        &self.new_space
    }
    pub fn old_space(&self) -> &OldSpace {
        &self.old_space
    }
    pub fn large_space(&self) -> &LargeObjectSpace {
        &self.large_space
    }
    pub fn trace_table(&self) -> &TraceTable {
        &self.trace_table
    }

    fn update_stats(&mut self) {
        self.stats.young_gen_bytes = self.new_space.allocated_bytes();
        self.stats.old_gen_bytes = self.old_space.allocated_bytes();
        self.stats.large_object_bytes = self.large_space.allocated_bytes();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::header::GcHeader;
    use crate::page::CELL_SIZE;

    const TAG_LEAF: u8 = 20;
    const TAG_NODE: u8 = 21;

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
        let slot = unsafe { &raw const (*node).child } as *mut *const GcHeader;
        visit(slot);
    }

    fn setup_heap() -> GcHeap {
        let mut heap = GcHeap::new(GcConfig {
            young_gen_size: 1024 * 1024, // 1MB for tests
            old_gen_threshold: 512 * 1024,
            ..GcConfig::default()
        });
        heap.register_trace_fn(TAG_NODE, trace_node);
        heap
    }

    fn alloc_young_leaf(heap: &mut GcHeap, value: u64) -> *mut GcHeader {
        let size = align_up(std::mem::size_of::<Leaf>(), CELL_SIZE);
        let ptr = heap.alloc_young(size).expect("alloc young leaf");
        unsafe {
            let leaf = ptr.as_ptr() as *mut Leaf;
            (*leaf).header = GcHeader::new_young(TAG_LEAF, size as u32);
            (*leaf).value = value;
            ptr.as_ptr() as *mut GcHeader
        }
    }

    #[test]
    fn alloc_and_root() {
        let mut heap = setup_heap();
        let obj = alloc_young_leaf(&mut heap, 42);
        let handle = heap.root(obj);

        assert_eq!(heap.deref_local(handle), Some(obj as *const GcHeader));
    }

    #[test]
    fn handle_scope_releases_handles() {
        let mut heap = setup_heap();

        let scope = heap.enter_scope();
        let obj = alloc_young_leaf(&mut heap, 1);
        let h = heap.root(obj);
        assert!(heap.deref_local(h).is_some());

        heap.exit_scope(scope);
        assert!(heap.deref_local(h).is_none()); // Released
    }

    #[test]
    fn scavenge_preserves_rooted_objects() {
        let mut heap = setup_heap();

        let obj = alloc_young_leaf(&mut heap, 999);
        let handle = heap.root(obj);

        // Also allocate garbage (not rooted).
        alloc_young_leaf(&mut heap, 0);

        let result = heap.collect_young();

        assert!(result.copied_count >= 1); // At least the rooted object
        // The handle should still be valid (updated by scavenger).
        let new_ptr = heap
            .deref_local(handle)
            .expect("handle should be valid after scavenge");
        let new_leaf = unsafe { &*(new_ptr as *const Leaf) };
        assert_eq!(new_leaf.value, 999);
    }

    #[test]
    fn global_handle_survives_scavenge() {
        let mut heap = setup_heap();

        let obj = alloc_young_leaf(&mut heap, 777);
        let global = heap.create_global(obj);

        heap.collect_young();

        let new_ptr = heap.deref_global(global).expect("global should survive");
        let leaf = unsafe { &*(new_ptr as *const Leaf) };
        assert_eq!(leaf.value, 777);
    }

    #[test]
    fn full_gc_collects_unreachable_old_gen() {
        let mut heap = setup_heap();

        // Allocate directly in old space.
        let size = align_up(std::mem::size_of::<Leaf>(), CELL_SIZE);
        let alive_ptr = heap.alloc_old(size).expect("alloc old");
        unsafe {
            let leaf = alive_ptr.as_ptr() as *mut Leaf;
            (*leaf).header = GcHeader::new(TAG_LEAF, size as u32);
            (*leaf).value = 100;
        }
        let alive = alive_ptr.as_ptr() as *const GcHeader;
        let _handle = heap.root(alive);

        // Allocate dead object (not rooted).
        let dead_ptr = heap.alloc_old(size).expect("alloc old dead");
        unsafe {
            let leaf = dead_ptr.as_ptr() as *mut Leaf;
            (*leaf).header = GcHeader::new(TAG_LEAF, size as u32);
            (*leaf).value = 200;
        }

        let result = heap.collect_full();

        assert_eq!(result.live_count, 1);
        assert!(result.freed_bytes > 0);
    }

    #[test]
    fn maybe_collect_is_safe_to_call_repeatedly() {
        let mut heap = setup_heap();

        // Allocate some objects and call maybe_collect multiple times.
        for i in 0..100 {
            let obj = alloc_young_leaf(&mut heap, i);
            heap.root(obj);
            heap.maybe_collect();
        }

        // Should not crash or panic.
        // Should not crash or panic. Stats are valid.
    }

    #[test]
    fn stats_track_collections() {
        let mut heap = setup_heap();

        alloc_young_leaf(&mut heap, 1);
        heap.collect_young();

        assert_eq!(heap.stats().scavenges, 1);
        assert_eq!(heap.stats().full_collections, 0);

        heap.collect_full();
        assert_eq!(heap.stats().full_collections, 1);
    }

    // -----------------------------------------------------------------------
    // Heap limit (max_heap_bytes) tests
    // -----------------------------------------------------------------------

    fn heap_with_max(max_bytes: usize) -> GcHeap {
        let mut heap = GcHeap::new(GcConfig {
            young_gen_size: 256 * 1024,
            old_gen_threshold: 128 * 1024,
            max_heap_bytes: Some(max_bytes),
            ..GcConfig::default()
        });
        heap.register_trace_fn(TAG_NODE, trace_node);
        heap
    }

    #[test]
    fn default_config_has_no_heap_cap() {
        let heap = GcHeap::with_defaults();
        assert!(heap.max_heap_bytes().is_none());
        assert!(!heap.oom_flag().load(Ordering::Relaxed));
    }

    #[test]
    fn current_heap_bytes_sums_all_spaces() {
        let mut heap = setup_heap();
        let before = heap.current_heap_bytes();
        alloc_young_leaf(&mut heap, 1);
        let after = heap.current_heap_bytes();
        assert!(after > before, "heap byte count should grow after alloc");
    }

    #[test]
    fn unrooted_allocs_stay_below_limit() {
        // Plenty of headroom (1 MB) — leaves are unrooted so scavenge reclaims them.
        let mut heap = heap_with_max(1024 * 1024);
        for i in 0..10_000_u64 {
            let _ = alloc_young_leaf(&mut heap, i);
        }
        assert!(
            !heap.oom_flag().load(Ordering::Relaxed),
            "OOM should not trip when the working set is collectible"
        );
    }

    #[test]
    fn oom_flag_trips_when_live_set_exceeds_cap() {
        // Tight cap: rooted allocations cannot be reclaimed, so the flag should
        // eventually trip. Cap sized well below what 2_000 live leaves need.
        let mut heap = heap_with_max(16 * 1024);
        let mut rooted = 0usize;
        for i in 0..4_096_u64 {
            let size = align_up(std::mem::size_of::<Leaf>(), CELL_SIZE);
            match heap.alloc_young(size) {
                Some(ptr) => unsafe {
                    let leaf = ptr.as_ptr() as *mut Leaf;
                    (*leaf).header = GcHeader::new_young(TAG_LEAF, size as u32);
                    (*leaf).value = i;
                    heap.root(ptr.as_ptr() as *const GcHeader);
                    rooted += 1;
                },
                None => break,
            }
        }
        assert!(
            heap.oom_flag().load(Ordering::Relaxed),
            "OOM flag should be set after exhausting a tight heap cap"
        );
        assert!(
            rooted > 0,
            "some allocations must succeed before the cap is hit"
        );
    }

    #[test]
    fn clear_oom_flag_resets_signal() {
        let heap = heap_with_max(8 * 1024);
        heap.oom_flag().store(true, Ordering::Relaxed);
        heap.clear_oom_flag();
        assert!(!heap.oom_flag().load(Ordering::Relaxed));
    }

    #[test]
    fn with_max_heap_bytes_constructor() {
        let heap = GcHeap::with_max_heap_bytes(64 * 1024);
        assert_eq!(heap.max_heap_bytes(), Some(64 * 1024));
    }
}
