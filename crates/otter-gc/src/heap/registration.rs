//! Heap trace registration, native ownership and weak lifecycle hooks.
//!
//! # Contents
//! - Trace, finalizer and restore registrations.
//! - External/host tables, weak registries and post-mark processing.
//!
//! # Invariants
//! - Registrations belong to this heap and retain their current indices.
//! - Weak post-mark processing completes before reclamation.
//!
//! # See also
//! - [`crate::trace`].

use super::{GcHeap, PostMarkProcessor};
use crate::compressed::{Gc, RawGc};
use crate::external_refs::ExternalRefTable;
use crate::extra_roots::ExtraRoots;
use crate::observation::{GcPauseKind, GcPauseOutcome, GcPauseTrigger};
use crate::trace::{TraceTable, Traceable};

impl GcHeap {
    /// Register a [`Traceable`] type so allocations of `T` can be
    /// traced during marking and scavenging.
    pub fn register_traceable<T: Traceable>(&mut self) {
        self.trace_table.register::<T>();
    }

    /// Register a [`SafeFinalize`] body so the sweeper calls
    /// `finalize_safe` on dead instances before their storage is
    /// reclaimed. Idempotent; must be paired with an earlier
    /// `register_traceable::<T>()` (the lazy `alloc_*` path
    /// handles the trace registration for you).
    pub fn register_finalize<T: Traceable + crate::trace::SafeFinalize>(&mut self) {
        self.trace_table.register_finalize::<T>();
    }

    /// Register a GC-managed weak collection table for ephemeron
    /// fixpoint work between full-GC marking and sweeping.
    pub fn register_ephemeron_table<T: ?Sized>(&mut self, table: Gc<T>) {
        self.ephemerons.register(table.raw());
    }

    /// Register a GC-managed `WeakRef` body for post-mark clearing.
    pub fn register_weak_ref<T: ?Sized>(&mut self, weak_ref: Gc<T>) {
        self.weak_finalization.register_weak_ref(weak_ref.raw());
    }

    /// Register a GC-managed `FinalizationRegistry` body for
    /// post-mark cleanup enqueueing.
    pub fn register_finalization_registry<T: ?Sized>(&mut self, registry: Gc<T>) {
        self.weak_finalization
            .register_finalization_registry(registry.raw());
    }

    /// Snapshot registered weak collection tables.
    #[doc(hidden)]
    #[must_use]
    pub fn ephemeron_tables_snapshot(&self) -> Vec<RawGc> {
        self.ephemerons.snapshot()
    }

    /// Number of registered weak collection tables.
    #[must_use]
    pub fn ephemeron_table_count(&self) -> usize {
        self.ephemerons.len()
    }

    /// Snapshot registered `WeakRef` body handles.
    #[doc(hidden)]
    #[must_use]
    pub fn weak_refs_snapshot(&self) -> Vec<RawGc> {
        self.weak_finalization.weak_refs_snapshot()
    }

    /// Snapshot registered `FinalizationRegistry` body handles.
    #[doc(hidden)]
    #[must_use]
    pub fn finalization_registries_snapshot(&self) -> Vec<RawGc> {
        self.weak_finalization.finalization_registries_snapshot()
    }

    /// Whether this heap has ever allocated a finalization registry.
    #[must_use]
    pub fn has_finalization_registries(&self) -> bool {
        self.weak_finalization.has_finalization_registries()
    }

    /// Whether there is no registered weak-reference/finalization work.
    #[must_use]
    pub fn weak_finalization_registry_is_empty(&self) -> bool {
        self.weak_finalization.is_empty()
    }

    /// Count registered `WeakRef` bodies.
    #[must_use]
    pub fn weak_ref_count(&self) -> usize {
        self.weak_finalization.weak_ref_count()
    }

    /// Count registered `FinalizationRegistry` bodies.
    #[must_use]
    pub fn finalization_registry_count(&self) -> usize {
        self.weak_finalization.finalization_registry_count()
    }

    /// Whether a header read at `offset` could belong to a live body.
    /// See [`TraceTable::header_could_be_live`].
    #[must_use]
    pub fn header_could_be_live(&self, offset: u32, size_bytes: u32, type_tag: u8) -> bool {
        self.trace_table
            .header_could_be_live(offset, size_bytes, type_tag)
    }

    /// Reference to the trace table (used by tests).
    #[doc(hidden)]
    pub fn trace_table(&self) -> &TraceTable {
        &self.trace_table
    }

    /// Intern a non-GC address so a body can reference it by a small
    /// index instead of storing the raw pointer. See
    /// [`crate::external_refs`].
    pub fn intern_external_ref(&mut self, addr: usize) -> u32 {
        self.external_refs.with_mut(|table| table.intern(addr))
    }

    /// The heap's external-reference table.
    #[must_use]
    pub fn external_refs(&self) -> &ExternalRefTable {
        self.external_refs.get()
    }

    /// Compiler threads' right to read this heap.
    #[must_use]
    pub fn read_right(&self) -> std::sync::Arc<crate::read_right::HeapReadRight> {
        std::sync::Arc::clone(&self.read_right)
    }

    /// The external-reference table, for compiler threads.
    #[must_use]
    pub fn shared_external_refs(&self) -> std::sync::Arc<crate::external_refs::SharedExternalRefs> {
        std::sync::Arc::clone(&self.external_refs)
    }

    /// Rebuild the external-reference table from a same-process captured
    /// address list, preserving indices. See
    /// [`ExternalRefTable::restore_from_addrs`].
    pub fn restore_external_refs(&mut self, addrs: impl IntoIterator<Item = usize>) {
        self.external_refs
            .with_mut(|table| table.restore_from_addrs(addrs));
    }

    /// Store an owned host payload and return its index. See
    /// [`crate::host_refs`].
    pub fn intern_host_ref(&mut self, payload: Box<dyn std::any::Any>) -> u32 {
        self.host_refs.insert(payload)
    }

    /// The heap's host-reference table.
    #[must_use]
    pub fn host_refs(&self) -> &crate::host_refs::HostRefTable {
        &self.host_refs
    }

    /// The heap's host-reference table, mutably. Restore-path use:
    /// payloads must land at the indices the captured bodies carry.
    pub fn host_refs_mut(&mut self) -> &mut crate::host_refs::HostRefTable {
        &mut self.host_refs
    }

    /// Register a [`crate::trace::ReleaseHostRefs`] body so the sweep
    /// releases its host-ref slots when it dies. Must be paired with an
    /// earlier `register_traceable::<T>()`.
    pub fn register_host_release<T: Traceable + crate::trace::ReleaseHostRefs>(&mut self) {
        self.trace_table.register_host_release::<T>();
    }

    /// Register the restore-time foreign-ownership sever hook. See
    /// [`crate::trace::SeverRestoredPayload`].
    pub fn register_sever_restored<T: Traceable + crate::trace::SeverRestoredPayload>(&mut self) {
        self.trace_table.register_sever_restored::<T>();
    }

    /// Install the embedder's weak-semantics pass. The VM installs it once
    /// when it takes ownership of the heap.
    pub fn set_post_mark_processor(&mut self, processor: PostMarkProcessor) {
        self.post_mark = Some(processor);
    }

    /// Run the installed weak-semantics pass after [`Self::mark_phase`] and
    /// before [`Self::sweep_phase`]. [`Self::collect_full`] calls it itself.
    pub fn run_post_mark_processing(&mut self) {
        self.with_gc_pause(GcPauseKind::SplitWeak, GcPauseTrigger::Explicit, |heap| {
            heap.run_post_mark_processing_work();
            ((), GcPauseOutcome::Completed)
        });
    }

    pub(super) fn run_post_mark_processing_work(&mut self) {
        if let Some(processor) = self.post_mark {
            processor(self);
        }
        // Owner tables forget unreached cells last: the processor's
        // ephemeron fixpoint may still mark objects.
        let sources = self.extra_roots.clone();
        let mut seen: Vec<ExtraRoots> = Vec::with_capacity(sources.len());
        for source in sources {
            if seen.iter().any(|earlier| earlier.same_source(&source)) {
                continue;
            }
            source.sweep_weak(self);
            seen.push(source);
        }
    }

    /// Whether a collection since the last call may have emptied a
    /// finalization cell. Resets the flag.
    pub fn take_weak_cleanup_due(&mut self) -> bool {
        std::mem::take(&mut self.weak_cleanup_due)
    }
}
