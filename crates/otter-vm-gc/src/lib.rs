//! # Otter VM Garbage Collector
//!
//! Generational, concurrent garbage collector.
//!
//! ## Design
//!
//! - **Young generation**: Per-thread bump allocation, no synchronization
//! - **Old generation**: Shared heap with concurrent mark-sweep
//! - **Large objects**: Separate allocation for objects > 8KB

#![warn(clippy::all)]
#![warn(missing_docs)]

pub mod allocator;
pub mod collector;
pub mod heap;
pub mod object;

pub use allocator::Allocator;
pub use collector::{write_barrier, Collector, GcStats};
pub use heap::{GcConfig, GcHeap};
pub use object::{GcHeader, GcObject};
