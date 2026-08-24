//! Immutable resource-limit configuration.
//!
//! # Contents
//! - [`ResourceLimits`] is the finished fixed-array configuration.
//! - [`ResourceLimitsBuilder`] provides value-style construction.
//!
//! # Invariants
//! - `None` always means unlimited and `Some(value)` is an inclusive cap.
//! - A finished limit set exposes no mutating operation.
//!
//! # See also
//! - [`crate::ResourceAccount::new`] installs a limit set in a ledger.

use crate::class::{RESOURCE_CLASS_COUNT, ResourceClass};

/// Immutable limits for all resource classes.
///
/// A `None` entry means that the corresponding class is unlimited. Construct
/// a value with [`ResourceLimits::builder`], then share it by value or pass it
/// to [`crate::ResourceAccount::new`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResourceLimits {
    pub(crate) limits: [Option<u64>; RESOURCE_CLASS_COUNT],
}

impl ResourceLimits {
    /// Return a limit set in which every resource class is unlimited.
    #[must_use]
    pub const fn unlimited() -> Self {
        Self {
            limits: [None; RESOURCE_CLASS_COUNT],
        }
    }

    /// Start building a limit set in which every class is initially unlimited.
    #[must_use]
    pub const fn builder() -> ResourceLimitsBuilder {
        ResourceLimitsBuilder::new()
    }

    /// Return the configured limit for `class`, or `None` when it is unlimited.
    #[must_use]
    pub const fn get(&self, class: ResourceClass) -> Option<u64> {
        self.limits[class.index()]
    }
}

impl Default for ResourceLimits {
    fn default() -> Self {
        Self::unlimited()
    }
}

/// Fluent builder for [`ResourceLimits`].
///
/// Calling [`ResourceLimitsBuilder::limit`] again for the same class replaces
/// the earlier value. [`ResourceLimitsBuilder::unlimited`] removes a previously
/// configured cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResourceLimitsBuilder {
    limits: [Option<u64>; RESOURCE_CLASS_COUNT],
}

impl ResourceLimitsBuilder {
    /// Create a builder with every resource class unlimited.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            limits: [None; RESOURCE_CLASS_COUNT],
        }
    }

    /// Set `class` to a finite `limit`.
    #[must_use]
    pub const fn limit(mut self, class: ResourceClass, limit: u64) -> Self {
        self.limits[class.index()] = Some(limit);
        self
    }

    /// Make `class` unlimited.
    #[must_use]
    pub const fn unlimited(mut self, class: ResourceClass) -> Self {
        self.limits[class.index()] = None;
        self
    }

    /// Finish building the immutable limit set.
    #[must_use]
    pub const fn build(self) -> ResourceLimits {
        ResourceLimits {
            limits: self.limits,
        }
    }
}

impl Default for ResourceLimitsBuilder {
    fn default() -> Self {
        Self::new()
    }
}
