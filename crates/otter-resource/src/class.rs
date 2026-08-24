//! Closed resource-class vocabulary for the ledger.
//!
//! # Contents
//! - [`ResourceClass`] defines every independently limited resource.
//! - [`ResourceClass::ALL`] provides stable iteration order.
//!
//! # Invariants
//! - The discriminants are contiguous and match [`ResourceClass::ALL`] order.
//! - Byte-valued variants end in `Bytes`; other variants count live items.
//!
//! # See also
//! - [`crate::ResourceLimits`] assigns limits to these classes.
//! - [`crate::ResourceSnapshot`] reports their usage in the same order.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Number of independently accounted resource classes.
pub(crate) const RESOURCE_CLASS_COUNT: usize = 12;

/// A resource category with an independent usage counter and optional limit.
///
/// Byte-valued classes use bytes as their unit; the remaining classes count
/// live objects or operations. The enum is deliberately closed so the ledger
/// can store all state in fixed-size arrays.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum ResourceClass {
    /// Garbage-collected heap bytes.
    HeapBytes,
    /// Bytes owned outside the garbage-collected heap, such as backing stores.
    ExternalBytes,
    /// Bytes occupied by generated native code and its retained metadata.
    GeneratedCodeBytes,
    /// Retained source text, parsed modules, and compiled module bytes.
    SourceModuleBytes,
    /// Live isolates.
    Isolates,
    /// Live workers.
    Workers,
    /// Bytes reserved for worker stacks.
    WorkerStackBytes,
    /// Tasks waiting to run.
    QueuedTasks,
    /// Messages waiting for delivery.
    QueuedMessages,
    /// Payload bytes retained by queued messages.
    QueuedMessageBytes,
    /// Live timers.
    Timers,
    /// In-flight host operations.
    HostOperations,
}

impl ResourceClass {
    /// Every resource class in stable snapshot order.
    pub const ALL: [Self; RESOURCE_CLASS_COUNT] = [
        Self::HeapBytes,
        Self::ExternalBytes,
        Self::GeneratedCodeBytes,
        Self::SourceModuleBytes,
        Self::Isolates,
        Self::Workers,
        Self::WorkerStackBytes,
        Self::QueuedTasks,
        Self::QueuedMessages,
        Self::QueuedMessageBytes,
        Self::Timers,
        Self::HostOperations,
    ];

    /// Number of variants in [`ResourceClass`].
    pub const COUNT: usize = RESOURCE_CLASS_COUNT;

    /// Return a stable, human-readable name for the class.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::HeapBytes => "heap bytes",
            Self::ExternalBytes => "external bytes",
            Self::GeneratedCodeBytes => "generated code bytes",
            Self::SourceModuleBytes => "source/module bytes",
            Self::Isolates => "isolates",
            Self::Workers => "workers",
            Self::WorkerStackBytes => "worker stack bytes",
            Self::QueuedTasks => "queued tasks",
            Self::QueuedMessages => "queued messages",
            Self::QueuedMessageBytes => "queued message bytes",
            Self::Timers => "timers",
            Self::HostOperations => "host operations",
        }
    }

    pub(crate) const fn index(self) -> usize {
        self as usize
    }
}

impl fmt::Display for ResourceClass {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}
