//! Interpreter-owned hidden-class side tables.
//!
//! GC-managed [`super::shape_body::ShapeBody`] nodes stay immutable and contain
//! only collector handles plus numeric layout metadata. This module owns the
//! mutable runtime state around those nodes: string-key interning, transition
//! reuse, and flattened offset caches. Keeping those tables off-GC mirrors the
//! VM's single-mutator model while preserving a simple traced payload shape.
//!
//! # Contents
//! - [`ShapeRuntime`] — root shape, key interner, transitions, and offset cache.
//! - [`ShapeRuntime::trace_roots`] — root walker for GC handles stored in side
//!   tables.
//!
//! # Invariants
//! - Side-table keys never contain `Gc` offsets; moving GC may rewrite handles,
//!   so keys use stable [`super::ShapeId`] plus interned
//!   [`crate::string::JsStringId`] values.
//! - Every `Gc` stored in the tables is yielded by [`Self::trace_roots`].
//! - Caches are derived data and can be cleared without semantic changes.
//! - Shape bodies themselves remain mutation-free after allocation.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-ordinary-object-internal-methods-and-internal-slots>
//! - <https://tc39.es/ecma262/#sec-ordinary-object-internal-methods-and-internal-slots-ownpropertykeys>
//! - Architecture plan §4.1 (hidden classes).

use rustc_hash::FxHashMap;
use std::cell::Cell;

use otter_gc::GcHeap;
use otter_gc::heap::RootSlotVisitor;
use otter_gc::raw::{RawGc, SlotVisitor};

use crate::inspect::{ShapeTransitionEvent, ShapeTransitionObserver};
use crate::string::{JsStringBody, JsStringHandle, JsStringId, alloc_flat_string_body_with_roots};

use super::ShapeId;
use super::descriptor::PropertyFlags;
use super::shape_body::{
    ShapeBody, ShapeHandle, alloc_child_shape_body_with_roots, alloc_root_shape_body_with_roots,
    shape_keys_ordered,
};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct TransitionKey {
    parent: ShapeId,
    key: JsStringId,
    /// Attribute bits of the appended slot. Keying transitions by attributes
    /// (not just the key) keeps a `define`-with-non-default-attributes append
    /// on a distinct shape from an ordinary default-data append of the same
    /// key, so shape-id IC guards invalidate correctly.
    flags: PropertyFlags,
    /// Data vs accessor for the appended slot — part of the transition
    /// identity for the same reason as [`Self::flags`].
    is_accessor: bool,
}

/// Mutable side tables for GC-managed hidden classes.
pub(crate) struct ShapeRuntime {
    root: Cell<ShapeHandle>,
    handles_by_id: FxHashMap<ShapeId, Cell<ShapeHandle>>,
    next_string_id: u32,
    interned_keys: FxHashMap<String, Cell<JsStringHandle>>,
    transitions: FxHashMap<TransitionKey, Cell<ShapeHandle>>,
    offset_cache: FxHashMap<ShapeId, FxHashMap<JsStringId, u32>>,
    observer: Option<Box<dyn ShapeTransitionObserver>>,
}

impl std::fmt::Debug for ShapeRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShapeRuntime")
            .field("root", &self.root)
            .field("next_string_id", &self.next_string_id)
            .field("interned_keys", &self.interned_keys.len())
            .field("transitions", &self.transitions.len())
            .field("offset_cache", &self.offset_cache.len())
            .field("observer_installed", &self.observer.is_some())
            .finish()
    }
}

impl ShapeRuntime {
    /// Allocate a fresh root shape and empty side tables.
    pub(crate) fn new(heap: &mut GcHeap) -> Result<Self, otter_gc::OutOfMemory> {
        let mut roots = |_visitor: &mut dyn FnMut(*mut RawGc)| {};
        let root = alloc_root_shape_body_with_roots(heap, &mut roots)?;
        let root_id = heap.read_payload(root, ShapeBody::id);
        let mut handles_by_id = FxHashMap::default();
        handles_by_id.insert(root_id, Cell::new(root));
        Ok(Self {
            root: Cell::new(root),
            handles_by_id,
            next_string_id: 1,
            interned_keys: FxHashMap::default(),
            transitions: FxHashMap::default(),
            offset_cache: FxHashMap::default(),
            observer: None,
        })
    }

    /// Install or clear the shape-transition observer. The
    /// observer fires on every transition take —
    /// [`ShapeTransitionEvent::reused`] distinguishes cached lookups
    /// from fresh shape allocations.
    pub(crate) fn set_observer(&mut self, observer: Option<Box<dyn ShapeTransitionObserver>>) {
        self.observer = observer;
    }

    /// Read-only access to the live transition table for the
    /// snapshot builder. Each entry yields the parent shape id and
    /// the child shape handle; callers read the child's stored
    /// transition key from the heap as needed.
    pub(crate) fn transitions_for_snapshot(
        &self,
    ) -> impl Iterator<Item = (ShapeId, ShapeHandle)> + '_ {
        self.transitions
            .iter()
            .map(|(key, child)| (key.parent, child.get()))
    }

    /// Empty hidden-class root.
    #[must_use]
    pub(crate) fn root(&self) -> ShapeHandle {
        self.root.get()
    }

    /// Remove every side-table entry before heap teardown.
    pub(crate) fn clear(&mut self) {
        self.interned_keys.clear();
        self.transitions.clear();
        self.offset_cache.clear();
        self.handles_by_id.clear();
        self.root.set(ShapeHandle::null());
    }

    /// Yield every GC handle stored in side tables as a mutable root slot.
    pub(crate) fn trace_roots(&self, visitor: &mut SlotVisitor<'_>) {
        if !self.root.get().is_null() {
            let p = self.root.as_ptr() as *mut RawGc;
            visitor(p);
        }
        for key in self.interned_keys.values() {
            let p = key.as_ptr() as *mut RawGc;
            visitor(p);
        }
        for shape in self.transitions.values() {
            let p = shape.as_ptr() as *mut RawGc;
            visitor(p);
        }
        for shape in self.handles_by_id.values() {
            let p = shape.as_ptr() as *mut RawGc;
            visitor(p);
        }
    }

    /// Resolve stable feedback identity back to the isolate-local GC handle.
    #[must_use]
    pub(crate) fn handle_for_id(&self, id: ShapeId) -> Option<ShapeHandle> {
        self.handles_by_id.get(&id).map(Cell::get)
    }

    /// Intern a property key as a GC-managed string body.
    pub(crate) fn intern_key_with_roots(
        &mut self,
        heap: &mut GcHeap,
        key: &str,
        external_visit: &mut RootSlotVisitor<'_>,
    ) -> Result<JsStringHandle, otter_gc::OutOfMemory> {
        if let Some(existing) = self.interned_keys.get(key) {
            return Ok(existing.get());
        }
        let units: Vec<u16> = key.encode_utf16().collect();
        let id = JsStringId::new(self.next_string_id);
        let mut visit_roots = |visitor: &mut dyn FnMut(*mut RawGc)| {
            self.trace_roots(visitor);
            external_visit(visitor);
        };
        let handle = alloc_flat_string_body_with_roots(heap, id, &units, &mut visit_roots)?;
        self.next_string_id = self.next_string_id.saturating_add(1);
        self.interned_keys.insert(key.to_owned(), Cell::new(handle));
        Ok(handle)
    }

    /// Return the cached transition for appending `key` to `parent`, or
    /// `None` when no such transition exists yet.
    ///
    /// This is the allocation-free fast path: an already-interned key plus a
    /// recorded transition resolve through two hash lookups and two payload
    /// reads with **no rooting and no allocation**. Construction of an object
    /// with a previously seen field shape (the overwhelmingly common case)
    /// hits here, so callers can skip the eager root-set collection that the
    /// allocating [`Self::child_with_roots`] path requires. A `None` result
    /// means the key has never been interned or the transition has not been
    /// taken before — the caller must fall back to `child_with_roots`.
    pub(crate) fn child_if_cached(
        &mut self,
        heap: &GcHeap,
        parent: ShapeHandle,
        key: &str,
        flags: PropertyFlags,
        is_accessor: bool,
    ) -> Option<ShapeHandle> {
        let key_handle = self.interned_keys.get(key)?.get();
        let parent_id = heap.read_payload(parent, ShapeBody::id);
        let key_id = heap.read_payload(key_handle, JsStringBody::id);
        let transition_key = TransitionKey {
            parent: parent_id,
            key: key_id,
            flags,
            is_accessor,
        };
        let child = self.transitions.get(&transition_key)?.get();
        self.notify_observer(heap, parent_id, child, key, true);
        Some(child)
    }

    /// Return the transition reached by appending `key` to `parent`.
    pub(crate) fn child_with_roots(
        &mut self,
        heap: &mut GcHeap,
        mut parent: ShapeHandle,
        key: &str,
        flags: PropertyFlags,
        is_accessor: bool,
        external_visit: &mut RootSlotVisitor<'_>,
    ) -> Result<ShapeHandle, otter_gc::OutOfMemory> {
        let mut visit_parent = |visitor: &mut dyn FnMut(*mut RawGc)| {
            let p = &mut parent as *mut ShapeHandle as *mut RawGc;
            visitor(p);
            external_visit(visitor);
        };
        let key_handle = self.intern_key_with_roots(heap, key, &mut visit_parent)?;
        let parent_id = heap.read_payload(parent, ShapeBody::id);
        let key_id = heap.read_payload(key_handle, JsStringBody::id);
        let transition_key = TransitionKey {
            parent: parent_id,
            key: key_id,
            flags,
            is_accessor,
        };
        if let Some(existing) = self.transitions.get(&transition_key) {
            let child = existing.get();
            self.notify_observer(heap, parent_id, child, key, true);
            return Ok(child);
        }

        let mut visit_child_roots = |visitor: &mut dyn FnMut(*mut RawGc)| {
            self.trace_roots(visitor);
            external_visit(visitor);
        };
        let child = alloc_child_shape_body_with_roots(
            heap,
            parent,
            key_handle,
            flags,
            is_accessor,
            &mut visit_child_roots,
        )?;
        let child_id = heap.read_payload(child, ShapeBody::id);
        self.handles_by_id.insert(child_id, Cell::new(child));
        self.transitions.insert(transition_key, Cell::new(child));
        self.notify_observer(heap, parent_id, child, key, false);
        Ok(child)
    }

    fn notify_observer(
        &mut self,
        heap: &GcHeap,
        parent_id: ShapeId,
        child: ShapeHandle,
        key: &str,
        reused: bool,
    ) {
        let Some(observer) = self.observer.as_deref_mut() else {
            return;
        };
        let child_id = heap.read_payload(child, ShapeBody::id);
        observer.on_transition(&ShapeTransitionEvent {
            from_shape_id: parent_id.raw(),
            to_shape_id: child_id.raw(),
            key: key.to_string(),
            reused,
        });
    }

    /// Lookup `key` in a shape, using the flattened cache when available.
    #[must_use]
    pub(crate) fn offset_of(
        &mut self,
        heap: &GcHeap,
        shape: ShapeHandle,
        key: &str,
    ) -> Option<u32> {
        let key_id = self
            .interned_keys
            .get(key)
            .map(|handle| heap.read_payload(handle.get(), JsStringBody::id))?;
        let shape_id = heap.read_payload(shape, ShapeBody::id);
        if let Some(cache) = self.offset_cache.get(&shape_id) {
            return cache.get(&key_id).copied();
        }

        let mut cache = FxHashMap::default();
        for (key_handle, offset) in shape_keys_ordered(heap, shape) {
            let id = heap.read_payload(key_handle, JsStringBody::id);
            cache.insert(id, offset);
        }
        let result = cache.get(&key_id).copied();
        self.offset_cache.insert(shape_id, cache);
        result
    }

    /// Number of interned shape property names.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn interned_key_count(&self) -> usize {
        self.interned_keys.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reuses_child_transition_and_interned_key() {
        let mut heap = GcHeap::new().expect("heap");
        let mut runtime = ShapeRuntime::new(&mut heap).expect("runtime");
        let mut roots = |_visitor: &mut dyn FnMut(*mut RawGc)| {};

        let flags = PropertyFlags::data_default();
        let first = runtime
            .child_with_roots(&mut heap, runtime.root(), "x", flags, false, &mut roots)
            .expect("first child");
        let second = runtime
            .child_with_roots(&mut heap, runtime.root(), "x", flags, false, &mut roots)
            .expect("second child");

        assert_eq!(first, second);
        assert_eq!(runtime.interned_key_count(), 1);
        assert_eq!(runtime.offset_of(&heap, first, "x"), Some(0));
    }

    #[test]
    fn interpreter_roots_shape_runtime_across_force_gc() {
        let mut interp = crate::Interpreter::new();
        let root = interp.shape_root();
        let first = interp.shape_child(root, "x").expect("child");
        let first_id = interp.gc_heap.read_payload(first, ShapeBody::id);
        assert_eq!(interp.shape_runtime.handle_for_id(first_id), Some(first));
        assert_eq!(interp.shape_offset_of(first, "x"), Some(0));

        interp.force_gc().expect("force GC");

        let root = interp.shape_root();
        let second = interp.shape_child(root, "x").expect("child after gc");
        assert_eq!(interp.shape_runtime.handle_for_id(first_id), Some(second));
        assert_eq!(interp.shape_offset_of(second, "x"), Some(0));
    }
}
