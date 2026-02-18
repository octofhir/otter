//! # Otter VM Garbage Collector
//!
//! Incremental mark-sweep garbage collector with block-based allocation.
//!
//! ## Design
//!
//! - **Block-based allocation**: 16KB blocks with segregated size classes
//! - **Incremental marking**: Budgeted tri-color marking at interpreter safepoints
//! - **Write barriers**: Dijkstra insertion barriers maintain tri-color invariant
//! - **Black allocation**: Objects allocated during marking are pre-marked live
//! - **Logical versioning**: O(1) mark reset via version counter
//! - **Ephemeron support**: WeakMap/WeakSet with proper weak collection semantics
//! - **Large objects**: Separate allocation for objects > 8KB

#![warn(clippy::all)]
#![warn(missing_docs)]

pub mod barrier;
pub mod ephemeron;
pub mod mark_sweep;
pub mod marked_block;
pub mod object;

pub use barrier::{
    CARD_SIZE, CardState, CardTable, RememberedSet, WriteBarrierBuffer, combined_barrier,
    deletion_barrier, generational_barrier, insertion_barrier, insertion_barrier_buffered,
};
pub use ephemeron::EphemeronTable;
pub use mark_sweep::{
    AllocationRegistry, GcPhase, GcTraceable, RegistryStats, barrier_push, clear_thread_registry,
    clear_thread_registry_if, gc_alloc, gc_alloc_in, global_registry, is_dealloc_in_progress,
    set_thread_registry,
};
pub use object::{GcHeader, GcObject};
