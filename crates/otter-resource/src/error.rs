//! Typed resource rejection diagnostics.
//!
//! # Contents
//! - [`ResourceError::Exhausted`] reports a finite-cap rejection.
//! - [`ResourceError::Overflow`] reports `u64` addition overflow.
//! - [`ResourceError::Allocation`] preserves the actual fallible allocator cause.
//!
//! # Invariants
//! - Quota errors carry their class, the increment that failed checked
//!   arithmetic, the left-hand amount, and the applicable optional limit.
//! - A rejected quota operation leaves current and peak usage unchanged while
//!   incrementing only its selected class's rejection counter.
//! - Allocation errors carry the requested capacity/bytes and actual allocator
//!   cause, without fabricated usage/limit. Diagnostic serialization is one-way;
//!   it never reconstructs an allocator failure from text.
//!
//! # See also
//! - [`crate::ResourceSnapshot`] provides cumulative rejection counts.

use std::error::Error;
use std::fmt;

use serde::Serialize;

use crate::ResourceClass;

/// A failed resource-accounting operation.
///
/// Quota variants report the requested replacement or increment, the amount it
/// was added to, and the applicable limit. The left-hand amount is normally
/// independently retained ledger usage; for duplicate aggregation overflow in
/// [`crate::ResourceAccount::reserve_exact_many`] it is the already-aggregated
/// part of that class's request. This makes the error sufficient for
/// deterministic diagnostics without taking a snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum ResourceError {
    /// A fallible backing-buffer allocation rejected the requested capacity.
    Allocation {
        /// Resource class admitting the physical buffer.
        class: ResourceClass,
        /// Total requested element capacity of the replacement buffer.
        capacity: usize,
        /// Total physical bytes requested for that buffer.
        requested: u64,
        /// Actual allocator failure, retained for typed source inspection.
        #[serde(serialize_with = "serialize_allocation_cause")]
        cause: std::collections::TryReserveError,
    },
    /// The requested usage is numerically valid but exceeds a finite limit.
    Exhausted {
        /// Resource class whose limit was reached.
        class: ResourceClass,
        /// Amount the operation attempted to add or commit.
        requested: u64,
        /// Amount to which `requested` was added.
        in_use: u64,
        /// Configured finite limit.
        limit: u64,
    },
    /// Adding the requested usage would overflow the `u64` counter.
    Overflow {
        /// Resource class whose counter would overflow.
        class: ResourceClass,
        /// Amount the operation attempted to add or commit.
        requested: u64,
        /// Amount to which `requested` was added.
        in_use: u64,
        /// Configured limit, or `None` when the class is unlimited.
        limit: Option<u64>,
    },
}

impl ResourceError {
    /// Return the resource class associated with the failure.
    #[must_use]
    pub const fn class(&self) -> ResourceClass {
        match self {
            Self::Exhausted { class, .. }
            | Self::Overflow { class, .. }
            | Self::Allocation { class, .. } => *class,
        }
    }

    /// Return the amount the failed operation requested.
    #[must_use]
    pub const fn requested(&self) -> u64 {
        match self {
            Self::Exhausted { requested, .. }
            | Self::Overflow { requested, .. }
            | Self::Allocation { requested, .. } => *requested,
        }
    }

    /// Return the quota usage to which the failed request was added.
    /// Allocation failures have no quota usage and return `None`.
    #[must_use]
    pub const fn in_use(&self) -> Option<u64> {
        match self {
            Self::Exhausted { in_use, .. } | Self::Overflow { in_use, .. } => Some(*in_use),
            Self::Allocation { .. } => None,
        }
    }

    /// Return the quota limit, or `None` for an unlimited quota or an allocation failure.
    #[must_use]
    pub const fn limit(&self) -> Option<u64> {
        match self {
            Self::Exhausted { limit, .. } => Some(*limit),
            Self::Overflow { limit, .. } => *limit,
            Self::Allocation { .. } => None,
        }
    }
}

impl fmt::Display for ResourceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Allocation {
                class,
                capacity,
                requested,
                cause,
            } => write!(
                formatter,
                "{class} allocation failed: capacity {capacity}, requested {requested} bytes: {cause}"
            ),
            Self::Exhausted {
                class,
                requested,
                in_use,
                limit,
            } => write!(
                formatter,
                "{class} limit exhausted: requested {requested}, in use {in_use}, limit {limit}"
            ),
            Self::Overflow {
                class,
                requested,
                in_use,
                limit,
            } => {
                write!(
                    formatter,
                    "{class} counter overflow: requested {requested}, in use {in_use}"
                )?;
                if let Some(limit) = limit {
                    write!(formatter, ", limit {limit}")
                } else {
                    formatter.write_str(", unlimited")
                }
            }
        }
    }
}

impl Error for ResourceError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Allocation { cause, .. } => Some(cause),
            Self::Exhausted { .. } | Self::Overflow { .. } => None,
        }
    }
}

fn serialize_allocation_cause<S: serde::Serializer>(
    cause: &std::collections::TryReserveError,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.collect_str(cause)
}
