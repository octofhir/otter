//! Atomic resource admission for runtime execution roles.
//!
//! # Contents
//! - [`RuntimeAdmissionKind`] describes the resources retained by each runtime
//!   construction path.
//! - [`AdmittedRuntimeConfig`] carries one validated configuration and its
//!   non-cloneable aggregate lease into runtime construction.
//! - [`RUNTIME_THREAD_STACK_BYTES`] is the single stack-size authority shared
//!   by every runtime-owned native thread and the resource ledger.
//!
//! # Invariants
//! - Configuration validation completes before any resource counter changes.
//! - One `reserve_exact_many` call admits the complete role tuple atomically;
//!   rejection cannot publish a partial current or peak charge.
//! - The carrier is not cloneable. Every successful admission becomes exactly
//!   one live runtime or rolls back as the carrier unwinds.
//! - The charged stack byte count exactly matches the stack size passed to
//!   `std::thread::Builder` for runtime-owned threads.
//!
//! # See also
//! - [`crate::RuntimeBuilder`] selects direct or handle-thread admission.
//! - [`crate::worker::WorkerBuilder`] selects worker-thread admission.
//! - [`otter_resource::ResourceAccount::reserve_exact_many`] provides the
//!   all-or-nothing ledger operation.

use otter_resource::{ResourceClass, ResourceLeaseSet};

use crate::{OtterError, Runtime, RuntimeConfig};

/// Native stack retained by every runtime-owned isolate or worker thread.
///
/// Resource planners can use this value when setting
/// [`ResourceClass::WorkerStackBytes`] limits; the same value is passed to the
/// native thread builder.
pub const RUNTIME_THREAD_STACK_BYTES: usize = 16 * 1024 * 1024;

/// Resource tuple selected by a runtime construction path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RuntimeAdmissionKind {
    /// A thread-pinned runtime whose embedder owns the execution thread.
    Direct,
    /// A sendable runtime backed by one dedicated isolate thread.
    HandleThread,
    /// A host or JavaScript worker backed by one dedicated worker thread.
    WorkerThread,
}

impl RuntimeAdmissionKind {
    const DIRECT_RESOURCES: [(ResourceClass, u64); 1] = [(ResourceClass::Isolates, 1)];
    const HANDLE_THREAD_RESOURCES: [(ResourceClass, u64); 2] = [
        (ResourceClass::Isolates, 1),
        (
            ResourceClass::WorkerStackBytes,
            RUNTIME_THREAD_STACK_BYTES as u64,
        ),
    ];
    const WORKER_THREAD_RESOURCES: [(ResourceClass, u64); 3] = [
        (ResourceClass::Isolates, 1),
        (ResourceClass::Workers, 1),
        (
            ResourceClass::WorkerStackBytes,
            RUNTIME_THREAD_STACK_BYTES as u64,
        ),
    ];

    fn resources(self) -> &'static [(ResourceClass, u64)] {
        match self {
            Self::Direct => &Self::DIRECT_RESOURCES,
            Self::HandleThread => &Self::HANDLE_THREAD_RESOURCES,
            Self::WorkerThread => &Self::WORKER_THREAD_RESOURCES,
        }
    }
}

/// A validated runtime configuration with its complete role resource tuple.
///
/// This carrier is intentionally not cloneable: its lease set is the unique
/// ownership token for one not-yet-built or live runtime.
pub(crate) struct AdmittedRuntimeConfig {
    pub(crate) config: RuntimeConfig,
    pub(crate) resource_leases: ResourceLeaseSet,
    pub(crate) kind: RuntimeAdmissionKind,
}

impl AdmittedRuntimeConfig {
    /// Validate `config`, then atomically reserve every resource for `kind`.
    pub(crate) fn admit(
        config: RuntimeConfig,
        kind: RuntimeAdmissionKind,
    ) -> Result<Self, OtterError> {
        Runtime::validate_config(&config)?;
        let resource_leases = config
            .resource_account
            .reserve_exact_many(kind.resources())?;
        Ok(Self {
            config,
            resource_leases,
            kind,
        })
    }
}
