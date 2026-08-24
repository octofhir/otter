//! Synchronous resource accounting primitives for Otter hosts.
//!
//! This crate provides a small, runtime-independent ledger for enforcing hard
//! resource limits. Callers reserve capacity before performing an effect, then
//! either let the provisional reservation roll back or commit its exact size
//! into a lease. The ledger performs no I/O and starts no background work.
//!
//! # Contents
//! - [`ResourceClass`] identifies each independently accounted resource.
//! - [`ResourceLimits`] and [`ResourceLimitsBuilder`] define immutable caps.
//! - [`ResourceAccount`] owns a cloneable, thread-safe ledger.
//! - [`ResourceReservation`], [`ResourceLease`], and [`ResourceLeaseSet`]
//!   provide RAII accounting.
//! - [`SharedSource`] and [`SharedSourceBuilder`] keep retained UTF-8 source
//!   text inseparable from its exact byte charge.
//! - [`ResourceSnapshot`] reports deterministic current and historical usage.
//! - [`ResourceError`] distinguishes limit exhaustion from integer overflow.
//!
//! # Invariants
//! - Every reservation is charged atomically before it is returned to its
//!   caller, so an effect guarded by a successful reservation cannot overshoot
//!   its class limit.
//! - A multi-class exact reservation aggregates duplicate classes and either
//!   publishes every requested charge together or publishes none of them.
//! - Dropping a reservation rolls it back; dropping a lease releases its exact
//!   committed amount; dropping a lease set releases all of its amounts under
//!   one lock. A failed commit rolls back, while a failed lease resize preserves
//!   the already-retained charge.
//! - All clones of an account share one fixed-size array protected by one
//!   mutex. A poisoned mutex is recovered without discarding its state.
//! - Limits never change after account construction, and snapshots always use
//!   [`ResourceClass::ALL`] order.
//! - Reservations and leases own only an `Arc` handle plus scalar or
//!   fixed-array metadata; ledger operations allocate no per-operation
//!   collections or callbacks.
//! - Cloning a [`SharedSource`] never duplicates its text or charge. Its
//!   private `Arc` allocation contains both the text and the sole lease.
//!
//! # See also
//! - [`ResourceAccount::reserve`] for estimate-first accounting.
//! - [`ResourceAccount::reserve_exact`] for known-size accounting.
//! - [`ResourceAccount::reserve_exact_many`] for atomic multi-class accounting.
//! - [`ResourceReservation::commit_exact`] for replacing an estimate with its
//!   exact retained size.
//! - [`ResourceLease::resize`] for atomically changing retained usage.
//! - [`SharedSource::read_utf8`] for bounded-chunk source ingestion.

mod account;
mod class;
mod error;
mod limits;
mod shared_source;
mod snapshot;

pub use account::{ResourceAccount, ResourceLease, ResourceLeaseSet, ResourceReservation};
pub use class::ResourceClass;
pub use error::ResourceError;
pub use limits::{ResourceLimits, ResourceLimitsBuilder};
pub use shared_source::{AccountedBytes, SharedSource, SharedSourceBuilder, SharedSourceError};
pub use snapshot::{ResourceSnapshot, ResourceSnapshotEntry};

#[cfg(test)]
mod tests;
