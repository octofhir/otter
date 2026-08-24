//! Runtime-owned snapshot DTOs and resource-ledger provenance.
//!
//! # Contents
//! - [`RuntimeSnapshot`] pairs an in-process VM image with its donor runtime
//!   configuration.
//! - [`RuntimeSnapshotDiagnostics`] copies deterministic structural metadata.
//! - [`SnapshotRuntimeOptions`] carries per-isolate restore configuration.
//!
//! # Invariants
//! - Every in-process runtime restore joins the snapshot donor's
//!   [`ResourceAccount`]; restore options cannot substitute another ledger.
//! - Capabilities, hooks, loaders, hosted modules, console policy, and child
//!   isolate inheritance remain those of the donor; the heap and host policy
//!   can never describe different authorities.
//! - The account is runtime metadata and is never encoded into the VM image.
//! - The VM image is opaque and process-local; this module exposes owned
//!   diagnostics, not raw heap pages or a serialization boundary.
//!
//! # See also
//! - [`crate::Runtime::capture_isolate_snapshot`] creates an in-process image.
//! - [`crate::Runtime::from_isolate_snapshot`] restores one.

use std::time::Duration;

use crate::{JitSelection, ResourceAccount, RuntimeConfig};

/// A runtime-owned in-process isolate image and its donor configuration.
///
/// The VM image may retain shared code and dynamic-native closures from its
/// donor. Keeping the complete donor configuration beside that image makes
/// every restore retain the same capability policy, loaders, hooks, and child
/// resource account instead of combining captured closures with unrelated host
/// authority.
pub struct RuntimeSnapshot {
    pub(crate) isolate: otter_vm::snapshot::IsolateSnapshot,
    pub(crate) donor_config: RuntimeConfig,
}

impl RuntimeSnapshot {
    /// Clone the donor resource account shared by in-process restores.
    #[must_use]
    pub fn resource_account(&self) -> ResourceAccount {
        self.donor_config.resource_account.clone()
    }

    /// Copy deterministic, VM-independent structural diagnostics.
    ///
    /// The returned DTO contains no heap handles or raw snapshot carrier, so
    /// inspecting a snapshot cannot bypass runtime admission on restore.
    #[must_use]
    pub fn diagnostics(&self) -> RuntimeSnapshotDiagnostics {
        RuntimeSnapshotDiagnostics {
            atom_names: self.isolate.atom_names().to_vec(),
            fixed_root_count: self.isolate.fixed_root_count(),
            global_lexical_names: self
                .isolate
                .global_lexical_names()
                .map(Box::<str>::from)
                .collect(),
            object_count: self.isolate.object_count(),
        }
    }
}

/// Owned structural diagnostics copied from a [`RuntimeSnapshot`].
///
/// This DTO is deterministic for equivalent runtime surfaces and contains no
/// VM values, GC handles, or restoration capability.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeSnapshotDiagnostics {
    atom_names: Vec<Box<str>>,
    fixed_root_count: usize,
    global_lexical_names: Vec<Box<str>>,
    object_count: u64,
}

impl RuntimeSnapshotDiagnostics {
    /// Borrow captured atom names in atom-id order.
    #[must_use]
    pub fn atom_names(&self) -> &[Box<str>] {
        &self.atom_names
    }

    /// Return the number of captured fixed roots.
    #[must_use]
    pub const fn fixed_root_count(&self) -> usize {
        self.fixed_root_count
    }

    /// Borrow global lexical names in deterministic name order.
    #[must_use]
    pub fn global_lexical_names(&self) -> &[Box<str>] {
        &self.global_lexical_names
    }

    /// Return the number of heap bodies in the captured image.
    #[must_use]
    pub const fn object_count(&self) -> u64 {
        self.object_count
    }
}

/// Per-isolate knobs used while restoring a runtime snapshot.
///
/// In-process [`RuntimeSnapshot`] restores always use the snapshot donor's
/// resource account; restore options cannot redirect shared ownership to a
/// different ledger.
#[derive(Debug, Clone, Default)]
pub struct SnapshotRuntimeOptions {
    /// Per-run wall-clock timeout; `Duration::ZERO` disables it.
    pub timeout: Duration,
    /// Heap cap in bytes; `0` disables the cap.
    pub max_heap_bytes: u64,
    /// Whether `Atomics.wait` may block this isolate's thread.
    pub allow_blocking_atomics_wait: bool,
    /// JIT tier selection.
    pub jit_selection: JitSelection,
}
