//! Heap-owned roots, registration scopes and strong marking roots.
//!
//! # Contents
//! - Frame, handle, embedder and isolate-data roots.
//! - Runtime source deduplication and collection preparation.
//!
//! # Invariants
//! - Every source is visited at most once per phase; nested owners remain live.
//! - Root visitors never allocate in this heap.
//!
//! # See also
//! - [`crate::extra_roots`].

use super::{GcHeap, RootSlotVisitor};
use crate::compressed::RawGc;
use crate::extra_roots::{ExtraRoots, ExtraRootsGuard, RootSet};
use crate::frame_roots::{FrameRoots, FrameRootsGuard};
use crate::handle::{GlobalHandleTable, HandleStack};
use crate::marking::MarkingState;

impl GcHeap {
    /// Register an active interpreter frame-stack root provider.
    ///
    /// Returns the new provider depth; callers pass `depth - 1` to
    /// [`Self::pop_frame_roots_to`] when leaving the matching dispatch scope.
    #[doc(hidden)]
    pub fn push_frame_roots(&mut self, provider: *const dyn FrameRoots) -> usize {
        self.frame_root_providers.push(provider)
    }

    /// Register active interpreter/JIT frame roots until the guard drops.
    pub fn register_frame_roots(&mut self, provider: *const dyn FrameRoots) -> FrameRootsGuard {
        let depth = self.push_frame_roots(provider) - 1;
        FrameRootsGuard::new(self, depth)
    }

    /// Open one root-scope marker and return its provider index.
    pub(crate) fn push_root_scope(&mut self) -> usize {
        self.frame_root_providers.push_scope() - 1
    }

    /// Root `slot` for the open root scope at provider index `owner`.
    pub(crate) fn push_root_scope_slot(
        &mut self,
        owner: usize,
        slot: *mut (),
        tracer: crate::root_scope::ErasedSlotTracer,
    ) {
        self.frame_root_providers
            .push_scope_slot(owner, slot, tracer);
    }

    /// Truncate active frame-root providers back to `depth`.
    #[doc(hidden)]
    pub fn pop_frame_roots_to(&mut self, depth: usize) {
        self.frame_root_providers.pop_to(depth);
    }

    /// Visit every active interpreter frame-stack root provider.
    pub fn trace_frame_root_providers(&self, visitor: &mut dyn FnMut(*mut RawGc)) {
        self.frame_root_providers.trace(visitor);
    }

    /// `true` when an active interpreter frame stack is registered.
    #[must_use]
    pub fn has_frame_root_providers(&self) -> bool {
        !self.frame_root_providers.is_empty()
    }

    /// Borrow the heap's handle stack.
    pub fn handle_stack(&self) -> &HandleStack {
        &self.handle_stack
    }

    /// Raw pointer to the heap's handle stack. Used by
    /// [`HandleScope::from_ptr`] to open a scope without
    /// holding an immutable borrow on the heap — the heap can
    /// continue to be mutated (`alloc`, `collect_*`,
    /// `write_barrier`) while the scope is open. The Box-owned
    /// stack has a stable address that lives as long as the
    /// `GcHeap`.
    pub fn handle_stack_ptr(&self) -> *const HandleStack {
        &*self.handle_stack as *const _
    }

    /// Borrow the heap's global handle table for crate-internal
    /// rooting APIs.
    pub(crate) fn global_handles(&self) -> &GlobalHandleTable {
        &self.global_handles
    }

    /// The handle in embedder root slot `index`; null until set.
    #[must_use]
    pub fn embedder_root(&self, index: usize) -> RawGc {
        self.embedder_roots[index].get()
    }

    /// Keep `raw` alive in embedder root slot `index`.
    pub fn set_embedder_root(&self, index: usize, raw: RawGc) {
        self.embedder_roots[index].set(raw);
    }

    /// Visit every embedder root slot, filled or not, in index order: the
    /// fixed-shape walk a heap snapshot captures and restores.
    pub fn visit_embedder_root_slots(&self, visitor: &mut dyn FnMut(*mut RawGc)) {
        for slot in &self.embedder_roots {
            visitor(slot.as_ptr());
        }
    }

    /// Take ownership of the embedder's per-isolate state for the heap's
    /// life. Its old-space handles are roots of every marking from here on.
    ///
    /// # Panics
    /// When the heap already holds a value.
    pub fn install_isolate_data<T: crate::isolate_data::IsolateData>(&mut self, data: T) {
        assert!(
            self.isolate_data.is_none(),
            "a heap owns one isolate data value"
        );
        self.isolate_data = Some(crate::isolate_data::IsolateDataSlot::new(data));
    }

    /// The per-isolate state, when it was installed as `T`.
    #[inline]
    #[must_use]
    pub fn isolate_data<T: crate::isolate_data::IsolateData>(&self) -> Option<&T> {
        self.isolate_data.as_ref()?.get()
    }

    /// [`Self::isolate_data`] for mutation.
    #[inline]
    pub fn isolate_data_mut<T: crate::isolate_data::IsolateData>(&mut self) -> Option<&mut T> {
        self.isolate_data.as_mut()?.get_mut()
    }

    /// Visit the filled embedder root slots.
    pub(super) fn trace_embedder_roots(&self, visitor: &mut dyn FnMut(*mut RawGc)) {
        for slot in &self.embedder_roots {
            if !slot.get().is_null() {
                visitor(slot.as_ptr());
            }
        }
    }

    /// Push an extra runtime root source onto the heap's LIFO
    /// registration stack.
    ///
    /// Returns the new stack depth; callers pass `depth - 1` to
    /// [`Self::pop_extra_roots_to`] when the source goes out of scope.
    /// Every live entry is traced during collection, so a nested
    /// registration never hides an outer scope's roots.
    #[doc(hidden)]
    #[must_use]
    #[inline]
    pub fn push_extra_roots(&mut self, roots: ExtraRoots) -> usize {
        self.extra_roots.push(roots);
        self.extra_roots.len()
    }

    /// Register an owner-managed root source until the returned guard drops.
    ///
    /// This is the standard API for VM/runtime root owners. It keeps cleanup
    /// panic-safe while allowing normal mutable heap access inside the scope.
    #[inline]
    pub fn register_extra_roots(&mut self, roots: ExtraRoots) -> ExtraRootsGuard {
        let depth = self.push_extra_roots(roots) - 1;
        ExtraRootsGuard::new(self, depth)
    }

    /// Whether any extra-roots provider is registered. The interpreter installs
    /// one (over its full runtime root set) for the duration of the dispatch
    /// loop, so a collection pulls those roots itself — callers that would
    /// otherwise snapshot the runtime root set on every allocation can skip it.
    #[must_use]
    pub fn has_extra_roots(&self) -> bool {
        !self.extra_roots.is_empty()
    }

    /// Whether `roots` is already registered as this exact source.
    ///
    /// A caller that needs one particular owner (rather than merely any
    /// transient root provider) uses this to avoid both an unsound false
    /// positive and a duplicate registration. Source identity includes the
    /// concrete dispatch thunk, so equal data addresses of different source
    /// types do not alias.
    #[must_use]
    pub fn has_extra_root_source(&self, roots: ExtraRoots) -> bool {
        self.extra_roots
            .iter()
            .any(|registered| registered.same_source(&roots))
    }

    /// Truncate the extra-roots stack back down to `depth`.
    #[doc(hidden)]
    #[inline]
    pub fn pop_extra_roots_to(&mut self, depth: usize) {
        debug_assert!(depth <= self.extra_roots.len());
        self.extra_roots.truncate(depth);
    }

    /// Reference to the active old-generation marking state.
    pub fn marking(&self) -> &MarkingState {
        &self.marking
    }

    /// Mutable old-generation marking state.
    pub fn marking_mut(&mut self) -> &mut MarkingState {
        &mut self.marking
    }

    pub(super) fn prepare_collection_observations(&self) {
        if let Some(data) = &self.isolate_data {
            data.prepare_collection();
        }
        for (index, source) in self.extra_roots.iter().enumerate() {
            if !self.extra_roots[..index]
                .iter()
                .any(|prior| prior.same_source(source))
            {
                source.prepare_collection(self);
            }
        }
    }

    pub(super) fn shade_roots(&mut self, external_visit: &mut RootSlotVisitor<'_>) {
        let handle_stack: *const HandleStack = &*self.handle_stack;
        let global_handles: *const GlobalHandleTable = &*self.global_handles;
        let marking_ptr = &mut self.marking as *mut MarkingState;
        let mut shade = move |slot: *mut RawGc| {
            // SAFETY: STW pause; valid slot per visitor contract.
            unsafe { (*marking_ptr).shade_from_slot(slot) };
        };
        // SAFETY: STW pause.
        unsafe {
            (*handle_stack).visit_slots(&mut shade);
            (*global_handles).visit_slots(&mut shade);
        }
        self.trace_embedder_roots(&mut shade);
        if let Some(data) = &self.isolate_data {
            data.visit_old_roots(&mut shade);
        }
        external_visit(&mut shade);
        visit_extra_roots_deduped(&self.extra_roots, RootSet::All, &mut shade);
        self.frame_root_providers.trace(&mut shade);
    }
}

/// Visit every registered extra-roots entry, skipping later entries
/// that dispatch to the same source as an earlier one. Re-entrant VM
/// scopes re-register the same interpreter; tracing it once per pause
/// keeps the root walk linear in distinct sources. Visiting is
/// idempotent, so a false-negative match only costs a redundant walk.
pub(super) fn visit_extra_roots_deduped(
    entries: &[ExtraRoots],
    roots: RootSet,
    visitor: &mut dyn FnMut(*mut RawGc),
) {
    for (i, entry) in entries.iter().enumerate() {
        if entries[..i].iter().any(|prev| prev.same_source(entry)) {
            continue;
        }
        entry.visit(roots, visitor);
    }
}
