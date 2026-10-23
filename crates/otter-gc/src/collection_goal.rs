//! Physical-memory intent of one full collection request.
//!
//! # Contents
//! - [`FullGcGoal`] distinguishes resident reuse from releasing unused payload.
//!
//! # Invariants
//! - The goal travels with the request to the shared closed-reader sweep boundary.
//! - Both goals run identical tracing, reclamation and free-range publication.
//! - Observation capture and trigger labels never select allocation policy.
//!
//! # See also
//! - [`crate::GcHeap::collect_full_observed`] accepts the owning request's goal.

/// The physical-memory goal of one full collection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FullGcGoal {
    /// Retain unused resident payload for mutator allocation reuse.
    Throughput,
    /// Release unused physical payload while preserving virtual ownership.
    ReleaseMemory,
}
