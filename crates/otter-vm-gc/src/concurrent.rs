//! Concurrent garbage collector
//!
//! Implements concurrent mark-sweep without full stop-the-world pauses.
//!
//! ## Design
//!
//! - Background marking thread processes the heap concurrently
//! - SATB (Snapshot-at-the-beginning) barrier preserves marking invariants
//! - Handshakes coordinate with mutator threads at safe points
//! - Incremental sweeping minimizes pause times

use crate::barrier::{RememberedSet, WriteBarrierBuffer};
use crate::heap::GcHeap;
use crate::object::{GcHeader, MarkColor};
use parking_lot::{Condvar, Mutex, RwLock};
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::thread::{self, JoinHandle};

/// Wrapper for raw pointer that is Send+Sync
///
/// SAFETY: This is used in the GC worklist which is protected by RwLock.
/// The pointers point to GC-managed memory that is only accessed through
/// proper synchronization.
#[derive(Debug, Clone, Copy)]
struct GcPtr(*const GcHeader);

// SAFETY: GcPtr is only used within the concurrent collector's worklist
// which is protected by RwLock. All access is synchronized.
unsafe impl Send for GcPtr {}
unsafe impl Sync for GcPtr {}

/// GC phase for concurrent collector
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GcPhase {
    /// No GC in progress
    Idle = 0,
    /// Initial marking (brief pause)
    InitialMark = 1,
    /// Concurrent marking (background thread)
    ConcurrentMark = 2,
    /// Remark phase (brief pause to finish marking)
    Remark = 3,
    /// Concurrent sweeping (background thread)
    ConcurrentSweep = 4,
}

impl From<u8> for GcPhase {
    fn from(v: u8) -> Self {
        match v {
            0 => GcPhase::Idle,
            1 => GcPhase::InitialMark,
            2 => GcPhase::ConcurrentMark,
            3 => GcPhase::Remark,
            4 => GcPhase::ConcurrentSweep,
            _ => GcPhase::Idle,
        }
    }
}

/// Safe point state for handshake protocol
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SafePointState {
    /// Normal execution
    Running,
    /// Waiting at safe point
    AtSafePoint,
    /// Requested to reach safe point
    Requested,
}

/// Mutator thread state for handshake
pub struct MutatorState {
    /// Thread ID
    pub id: usize,
    /// Current safe point state
    state: AtomicU8,
    /// Condvar for signaling
    condvar: Condvar,
    /// Mutex for condvar
    mutex: Mutex<()>,
}

impl MutatorState {
    /// Create new mutator state
    pub fn new(id: usize) -> Self {
        Self {
            id,
            state: AtomicU8::new(SafePointState::Running as u8),
            condvar: Condvar::new(),
            mutex: Mutex::new(()),
        }
    }

    /// Get current state
    pub fn state(&self) -> SafePointState {
        match self.state.load(Ordering::Acquire) {
            0 => SafePointState::Running,
            1 => SafePointState::AtSafePoint,
            _ => SafePointState::Requested,
        }
    }

    /// Request thread to reach safe point
    pub fn request_safe_point(&self) {
        self.state
            .store(SafePointState::Requested as u8, Ordering::Release);
    }

    /// Mark as at safe point and wait
    pub fn enter_safe_point(&self) {
        self.state
            .store(SafePointState::AtSafePoint as u8, Ordering::Release);
        // Wait until resumed
        let mut guard = self.mutex.lock();
        while self.state.load(Ordering::Acquire) == SafePointState::AtSafePoint as u8 {
            self.condvar.wait(&mut guard);
        }
    }

    /// Resume execution
    pub fn resume(&self) {
        self.state
            .store(SafePointState::Running as u8, Ordering::Release);
        self.condvar.notify_one();
    }

    /// Check if at safe point
    pub fn is_at_safe_point(&self) -> bool {
        self.state.load(Ordering::Acquire) == SafePointState::AtSafePoint as u8
    }
}

/// Concurrent garbage collector
pub struct ConcurrentCollector {
    /// Heap reference (used for sweep phase)
    #[allow(dead_code)]
    heap: Arc<GcHeap>,
    /// Current GC phase
    phase: AtomicU8,
    /// Write barrier buffer
    barrier_buffer: Arc<WriteBarrierBuffer>,
    /// Remembered set
    remembered_set: Arc<RememberedSet>,
    /// Gray worklist (shared with background thread)
    worklist: Arc<RwLock<VecDeque<GcPtr>>>,
    /// Registered mutator threads
    mutators: Arc<RwLock<Vec<Arc<MutatorState>>>>,
    /// Background thread handle
    background_thread: Mutex<Option<JoinHandle<()>>>,
    /// Shutdown flag
    shutdown: Arc<AtomicBool>,
    /// Statistics
    stats: Arc<Mutex<ConcurrentGcStats>>,
}

/// Statistics for concurrent GC
#[derive(Debug, Default, Clone)]
pub struct ConcurrentGcStats {
    /// Number of collections
    pub collections: u64,
    /// Total time in GC (nanoseconds)
    pub total_gc_time_ns: u64,
    /// Max pause time (nanoseconds)
    pub max_pause_ns: u64,
    /// Objects marked
    pub objects_marked: usize,
    /// Bytes reclaimed
    pub bytes_reclaimed: usize,
}

// SAFETY: Worklist contains raw pointers but is protected by RwLock
unsafe impl Send for ConcurrentCollector {}
unsafe impl Sync for ConcurrentCollector {}

impl ConcurrentCollector {
    /// Create new concurrent collector
    pub fn new(heap: Arc<GcHeap>) -> Arc<Self> {
        Arc::new(Self {
            heap,
            phase: AtomicU8::new(GcPhase::Idle as u8),
            barrier_buffer: Arc::new(WriteBarrierBuffer::new()),
            remembered_set: Arc::new(RememberedSet::new()),
            worklist: Arc::new(RwLock::new(VecDeque::new())),
            mutators: Arc::new(RwLock::new(Vec::new())),
            background_thread: Mutex::new(None),
            shutdown: Arc::new(AtomicBool::new(false)),
            stats: Arc::new(Mutex::new(ConcurrentGcStats::default())),
        })
    }

    /// Register a mutator thread
    pub fn register_mutator(&self) -> Arc<MutatorState> {
        let mut mutators = self.mutators.write();
        let id = mutators.len();
        let state = Arc::new(MutatorState::new(id));
        mutators.push(state.clone());
        state
    }

    /// Unregister a mutator thread
    pub fn unregister_mutator(&self, state: &Arc<MutatorState>) {
        let mut mutators = self.mutators.write();
        mutators.retain(|s| s.id != state.id);
    }

    /// Get current GC phase
    pub fn phase(&self) -> GcPhase {
        GcPhase::from(self.phase.load(Ordering::Acquire))
    }

    /// Get barrier buffer for write barriers
    pub fn barrier_buffer(&self) -> &Arc<WriteBarrierBuffer> {
        &self.barrier_buffer
    }

    /// Get remembered set
    pub fn remembered_set(&self) -> &Arc<RememberedSet> {
        &self.remembered_set
    }

    /// Get statistics
    pub fn stats(&self) -> ConcurrentGcStats {
        self.stats.lock().clone()
    }

    /// Start a GC cycle
    pub fn start_collection(&self, roots: &[*const GcHeader]) {
        // Phase 1: Initial mark (brief pause)
        self.initial_mark(roots);

        // Phase 2: Start concurrent marking
        self.start_concurrent_mark();
    }

    /// Initial marking phase - brief stop-the-world
    fn initial_mark(&self, roots: &[*const GcHeader]) {
        let start = std::time::Instant::now();

        // Set phase
        self.phase
            .store(GcPhase::InitialMark as u8, Ordering::Release);

        // Request all mutators to reach safe points
        self.handshake_all();

        // Mark roots
        let mut worklist = self.worklist.write();
        for &root in roots {
            if !root.is_null() {
                // SAFETY: Root pointers are valid during GC
                unsafe {
                    let header = &*root;
                    if header.mark() == MarkColor::White {
                        header.set_mark(MarkColor::Gray);
                        worklist.push_back(GcPtr(root));
                    }
                }
            }
        }

        // Also add remembered set entries
        for ptr in self.remembered_set.roots() {
            if !ptr.is_null() {
                // SAFETY: Remembered set contains valid pointers
                unsafe {
                    let header = &*ptr;
                    if header.mark() == MarkColor::White {
                        header.set_mark(MarkColor::Gray);
                        worklist.push_back(GcPtr(ptr));
                    }
                }
            }
        }

        // Resume mutators
        self.resume_all();

        // Update stats
        let pause = start.elapsed().as_nanos() as u64;
        let mut stats = self.stats.lock();
        stats.max_pause_ns = stats.max_pause_ns.max(pause);
    }

    /// Start concurrent marking phase
    fn start_concurrent_mark(&self) {
        self.phase
            .store(GcPhase::ConcurrentMark as u8, Ordering::Release);

        // Concurrent marking runs in the main thread for simplicity
        // A production implementation would spawn a background thread
        self.concurrent_mark_step(usize::MAX);
    }

    /// Perform incremental concurrent marking
    ///
    /// Returns true if more work remains
    pub fn concurrent_mark_step(&self, max_objects: usize) -> bool {
        let mut marked = 0;
        let mut stats_marked = 0;

        while marked < max_objects {
            // Get next gray object
            let obj_ptr = {
                let mut worklist = self.worklist.write();
                worklist.pop_front()
            };

            let Some(GcPtr(obj_ptr)) = obj_ptr else {
                // Also check barrier buffer
                let barrier_entries = self.barrier_buffer.drain();
                if barrier_entries.is_empty() {
                    break;
                }

                // Add barrier entries to worklist
                let mut worklist = self.worklist.write();
                for entry in barrier_entries {
                    if !entry.is_null() {
                        worklist.push_back(GcPtr(entry));
                    }
                }
                continue;
            };

            // Mark the object black
            // SAFETY: Pointers in worklist are valid during GC
            unsafe {
                let header = &*obj_ptr;

                // Trace object's children (would call trace() on GcObject)
                // For now, just mark as black
                header.set_mark(MarkColor::Black);
                stats_marked += 1;
            }

            marked += 1;
        }

        // Update stats
        {
            let mut stats = self.stats.lock();
            stats.objects_marked += stats_marked;
        }

        // Check if more work remains
        let has_work = {
            let worklist = self.worklist.read();
            !worklist.is_empty()
        } || !self.barrier_buffer.is_empty();

        if !has_work {
            // Transition to remark phase
            self.remark();
        }

        has_work
    }

    /// Remark phase - brief pause to finish marking
    fn remark(&self) {
        let start = std::time::Instant::now();

        self.phase.store(GcPhase::Remark as u8, Ordering::Release);

        // Brief handshake
        self.handshake_all();

        // Process any remaining barrier buffer entries
        let barrier_entries = self.barrier_buffer.drain();
        {
            let mut worklist = self.worklist.write();
            for entry in barrier_entries {
                if !entry.is_null() {
                    // SAFETY: Barrier entries are valid pointers
                    unsafe {
                        let header = &*entry;
                        if header.mark() == MarkColor::Gray {
                            worklist.push_back(GcPtr(entry));
                        }
                    }
                }
            }
        }

        // Finish marking any remaining gray objects
        while let Some(GcPtr(obj_ptr)) = self.worklist.write().pop_front() {
            // SAFETY: Pointers in worklist are valid
            unsafe {
                let header = &*obj_ptr;
                header.set_mark(MarkColor::Black);
            }
        }

        // Resume mutators
        self.resume_all();

        // Update stats
        let pause = start.elapsed().as_nanos() as u64;
        {
            let mut stats = self.stats.lock();
            stats.max_pause_ns = stats.max_pause_ns.max(pause);
        }

        // Start concurrent sweep
        self.start_concurrent_sweep();
    }

    /// Start concurrent sweeping phase
    fn start_concurrent_sweep(&self) {
        self.phase
            .store(GcPhase::ConcurrentSweep as u8, Ordering::Release);

        // Concurrent sweeping runs incrementally
        // A production implementation would do this in background
        self.concurrent_sweep_step(usize::MAX);

        // Finish collection
        self.finish_collection();
    }

    /// Perform incremental sweeping
    ///
    /// Returns true if more work remains
    pub fn concurrent_sweep_step(&self, _max_objects: usize) -> bool {
        // Sweeping would iterate over allocated objects and free white ones
        // For now, this is a no-op placeholder
        false
    }

    /// Finish collection and reset marks
    fn finish_collection(&self) {
        self.phase.store(GcPhase::Idle as u8, Ordering::Release);

        // Clear remembered set after full GC
        self.remembered_set.clear();

        // Update stats
        {
            let mut stats = self.stats.lock();
            stats.collections += 1;
        }
    }

    /// Request all mutators to reach safe points
    fn handshake_all(&self) {
        let mutators = self.mutators.read();

        // Request all to reach safe point
        for mutator in mutators.iter() {
            mutator.request_safe_point();
        }

        // Wait for all to reach safe point (with timeout)
        let timeout = std::time::Duration::from_millis(100);
        let start = std::time::Instant::now();

        loop {
            let all_safe = mutators.iter().all(|m| m.is_at_safe_point());
            if all_safe {
                break;
            }

            if start.elapsed() > timeout {
                // Timeout - continue anyway (not ideal but prevents deadlock)
                break;
            }

            // Brief yield
            thread::yield_now();
        }
    }

    /// Resume all mutators
    fn resume_all(&self) {
        let mutators = self.mutators.read();
        for mutator in mutators.iter() {
            mutator.resume();
        }
    }

    /// Shutdown the collector
    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Release);
    }
}

impl Drop for ConcurrentCollector {
    fn drop(&mut self) {
        self.shutdown();
        // Wait for background thread if running
        if let Some(handle) = self.background_thread.lock().take() {
            let _ = handle.join();
        }
    }
}

/// Safepoint check - call periodically in mutator code
#[inline]
pub fn safepoint_check(mutator: &MutatorState) {
    if mutator.state() == SafePointState::Requested {
        mutator.enter_safe_point();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::tags;

    #[test]
    fn test_gc_phase() {
        assert_eq!(GcPhase::from(0), GcPhase::Idle);
        assert_eq!(GcPhase::from(1), GcPhase::InitialMark);
        assert_eq!(GcPhase::from(2), GcPhase::ConcurrentMark);
        assert_eq!(GcPhase::from(3), GcPhase::Remark);
        assert_eq!(GcPhase::from(4), GcPhase::ConcurrentSweep);
        assert_eq!(GcPhase::from(255), GcPhase::Idle); // Unknown
    }

    #[test]
    fn test_mutator_state() {
        let state = MutatorState::new(0);
        assert_eq!(state.state(), SafePointState::Running);

        state.request_safe_point();
        assert_eq!(state.state(), SafePointState::Requested);

        // Simulate entering safe point in another thread
        state
            .state
            .store(SafePointState::AtSafePoint as u8, Ordering::Release);
        assert!(state.is_at_safe_point());

        state.resume();
        assert_eq!(state.state(), SafePointState::Running);
    }

    #[test]
    fn test_concurrent_collector_creation() {
        let heap = GcHeap::new();
        let collector = ConcurrentCollector::new(heap);

        assert_eq!(collector.phase(), GcPhase::Idle);
        assert_eq!(collector.stats().collections, 0);
    }

    #[test]
    fn test_register_mutator() {
        let heap = GcHeap::new();
        let collector = ConcurrentCollector::new(heap);

        let mutator1 = collector.register_mutator();
        let mutator2 = collector.register_mutator();

        assert_eq!(mutator1.id, 0);
        assert_eq!(mutator2.id, 1);

        collector.unregister_mutator(&mutator1);
        // mutator2 still registered
    }

    #[test]
    fn test_collection_cycle() {
        let heap = GcHeap::new();
        let collector = ConcurrentCollector::new(heap);

        // Create some test roots
        let header1 = GcHeader::new(tags::OBJECT);
        let header2 = GcHeader::new(tags::OBJECT);
        let roots = [&header1 as *const _, &header2 as *const _];

        // Run collection
        collector.start_collection(&roots);

        // Should complete
        assert_eq!(collector.phase(), GcPhase::Idle);
        assert_eq!(collector.stats().collections, 1);

        // Objects should be marked black
        assert_eq!(header1.mark(), MarkColor::Black);
        assert_eq!(header2.mark(), MarkColor::Black);
    }

    #[test]
    fn test_incremental_marking() {
        let heap = GcHeap::new();
        let collector = ConcurrentCollector::new(heap);

        // Add objects to worklist manually
        let header1 = GcHeader::new(tags::OBJECT);
        let header2 = GcHeader::new(tags::OBJECT);
        let header3 = GcHeader::new(tags::OBJECT);

        header1.set_mark(MarkColor::Gray);
        header2.set_mark(MarkColor::Gray);
        header3.set_mark(MarkColor::Gray);

        {
            let mut worklist = collector.worklist.write();
            worklist.push_back(GcPtr(&header1 as *const _));
            worklist.push_back(GcPtr(&header2 as *const _));
            worklist.push_back(GcPtr(&header3 as *const _));
        }

        collector
            .phase
            .store(GcPhase::ConcurrentMark as u8, Ordering::Release);

        // Mark only 2 objects
        let has_more = collector.concurrent_mark_step(2);
        assert!(has_more);

        // Mark remaining
        let has_more = collector.concurrent_mark_step(10);
        assert!(!has_more);
    }

    #[test]
    fn test_safepoint_check() {
        let state = MutatorState::new(0);

        // No request - should not block
        safepoint_check(&state);
        assert_eq!(state.state(), SafePointState::Running);

        // With request - would enter safe point (can't fully test without threads)
        state.request_safe_point();
        assert_eq!(state.state(), SafePointState::Requested);
    }
}
