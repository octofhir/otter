//! # Otter VM Garbage Collector
//!
//! Generational, concurrent garbage collector.
//!
//! ## Design
//!
//! - **Young generation**: Per-thread bump allocation, no synchronization
//! - **Old generation**: Shared heap with concurrent mark-sweep
//! - **Large objects**: Separate allocation for objects > 8KB
//!
//! ## Write Barriers
//!
//! Write barriers maintain GC invariants during mutation:
//! - **Insertion barrier**: When storing a reference into an object
//! - **Deletion barrier**: When removing/overwriting a reference
//! - **Card marking**: For efficient generational collection

#![warn(clippy::all)]
#![warn(missing_docs)]

pub mod allocator;
pub mod barrier;
pub mod collector;
pub mod concurrent;
pub mod heap;
pub mod object;

pub use allocator::Allocator;
pub use barrier::{
    CARD_SIZE, CardState, CardTable, RememberedSet, WriteBarrierBuffer, combined_barrier,
    deletion_barrier, generational_barrier, insertion_barrier, insertion_barrier_buffered,
};
pub use collector::{Collector, GcStats, write_barrier};
pub use concurrent::{
    ConcurrentCollector, ConcurrentGcStats, GcPhase, MutatorState, SafePointState, safepoint_check,
};
pub use heap::{GcConfig, GcHeap};
pub use object::{GcHeader, GcObject};
