//! Weak-reference and finalization registry bookkeeping.
//!
//! The collector owns only type-erased handles here. JavaScript
//! semantics stay in `otter-vm`: after ordinary marking and
//! ephemeron fixpoint work, the VM walks these registered objects,
//! clears dead `WeakRef` targets, and enqueues
//! `FinalizationRegistry` cleanup jobs on its isolate-local
//! microtask queue. The GC never calls JS during raw sweep.
//!
//! # Contents
//!
//! - [`WeakFinalizationRegistry`] — per-heap raw-handle lists for
//!   `WeakRef` and `FinalizationRegistry` bodies.
//!
//! # Invariants
//!
//! - Registered handles are not strong roots.
//! - Dead registered handles are pruned during full-GC sweep.
//! - `has_finalization_registries` is a lazy fast-path flag: heaps
//!   that never allocate a `FinalizationRegistry` skip VM
//!   finalizer processing except for one branch.
//!
//! # See also
//!
//! - <https://tc39.es/ecma262/#sec-weak-ref-objects>
//! - <https://tc39.es/ecma262/#sec-finalization-registry-objects>

use crate::compressed::RawGc;

/// Per-heap raw-handle registry for weak references and finalizers.
#[derive(Debug, Default)]
pub struct WeakFinalizationRegistry {
    weak_refs: Vec<RawGc>,
    finalization_registries: Vec<RawGc>,
    has_finalization_registries: bool,
}

impl WeakFinalizationRegistry {
    /// Register a live `WeakRef` body.
    pub fn register_weak_ref(&mut self, handle: RawGc) {
        if handle.is_null() || self.weak_refs.contains(&handle) {
            return;
        }
        self.weak_refs.push(handle);
    }

    /// Register a live `FinalizationRegistry` body.
    pub fn register_finalization_registry(&mut self, handle: RawGc) {
        self.has_finalization_registries = true;
        if handle.is_null() || self.finalization_registries.contains(&handle) {
            return;
        }
        self.finalization_registries.push(handle);
    }

    /// Snapshot live `WeakRef` body handles.
    #[must_use]
    pub fn weak_refs_snapshot(&self) -> Vec<RawGc> {
        self.weak_refs.clone()
    }

    /// Snapshot live `FinalizationRegistry` body handles.
    #[must_use]
    pub fn finalization_registries_snapshot(&self) -> Vec<RawGc> {
        self.finalization_registries.clone()
    }

    /// Whether the heap has ever allocated a finalization registry.
    #[must_use]
    pub fn has_finalization_registries(&self) -> bool {
        self.has_finalization_registries
    }

    /// Whether no weak-finalization handles are currently tracked.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.weak_refs.is_empty() && self.finalization_registries.is_empty()
    }

    /// Count registered `WeakRef` bodies.
    #[must_use]
    pub fn weak_ref_count(&self) -> usize {
        self.weak_refs.len()
    }

    /// Count registered `FinalizationRegistry` bodies.
    #[must_use]
    pub fn finalization_registry_count(&self) -> usize {
        self.finalization_registries.len()
    }

    /// Retain only handles still marked live in the current full-GC cycle.
    pub fn retain_marked(&mut self, mut is_marked: impl FnMut(RawGc) -> bool) {
        self.weak_refs.retain(|raw| is_marked(*raw));
        self.finalization_registries.retain(|raw| is_marked(*raw));
    }

    /// Mutable raw slots for young-generation relocation.
    ///
    /// These slots are not roots. The scavenger rewrites a slot only when the
    /// registered object was already forwarded through ordinary roots; otherwise
    /// it nulls the entry so the registry can prune it.
    pub fn handle_slots(&mut self) -> Vec<*mut RawGc> {
        self.weak_refs
            .iter_mut()
            .chain(self.finalization_registries.iter_mut())
            .map(|raw| raw as *mut RawGc)
            .collect()
    }

    /// Drop entries nulled by a young-generation collection.
    pub fn retain_non_null(&mut self) {
        self.weak_refs.retain(|raw| !raw.is_null());
        self.finalization_registries.retain(|raw| !raw.is_null());
    }
}
