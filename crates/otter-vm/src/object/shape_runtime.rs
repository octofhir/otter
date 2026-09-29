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
//! - [`ShapeRuntime::trace_roots`] — root walker for the strong handles: the
//!   root shape and the interned key strings.
//! - [`ShapeRuntime::sweep_dead`] — the full collection's weak pass over the
//!   id and transition tables.
//!
//! # Invariants
//! - Side-table keys never contain `Gc` offsets; moving GC may rewrite handles,
//!   so keys use stable [`super::ShapeId`] plus isolate-global
//!   [`crate::property_atom::AtomId`] values.
//! - A name's atom comes from the isolate's interner; this module's
//!   `interned_keys` map only caches that answer next to the name's GC string
//!   body, so a repeated transition never takes the interner lock.
//! - Hidden classes are collectable (V8 maps, JSC structures and SpiderMonkey
//!   shapes all are): the id and transition tables hold shapes weakly, and a
//!   shape lives while an object, a compiled code object or a strong cache
//!   names it. [`Self::sweep_dead`] forgets every shape the marking left
//!   unreached before the sweep frees it, so no table ever hands out a dead
//!   cell; ids are never reused, so an id that outlives its shape only misses.
//! - Shapes live in non-moving old space: a weak entry never needs rewriting.
//! - No table borrow is held across an allocation — the collection it may
//!   run prunes the tables through `&self`.
//! - A shape created during a runtime turn is pinned (strong) until the
//!   outermost turn ends: Rust code holds new shapes in locals across the
//!   allocations that precede installing them into an object, and the pin
//!   keeps every such window sound without rooting each local. Shapes
//!   become collectable at the next full collection after their turn.
//! - Caches are derived data and can be cleared without semantic changes.
//! - Shape bodies themselves remain mutation-free after allocation.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-ordinary-object-internal-methods-and-internal-slots>
//! - <https://tc39.es/ecma262/#sec-ordinary-object-internal-methods-and-internal-slots-ownpropertykeys>
//! - Architecture plan §4.1 (hidden classes).

use rustc_hash::{FxHashMap, FxHashSet};
use std::cell::{Cell, RefCell};
use std::sync::Arc;

use otter_gc::GcHeap;
use otter_gc::heap::RootSlotVisitor;
use otter_gc::raw::{RawGc, SlotVisitor};

use crate::inspect::{ShapeTransitionEvent, ShapeTransitionObserver};
use crate::property_atom::{AtomId, NameInterner};
use crate::string::{JsStringHandle, JsStringId, alloc_flat_string_body_with_roots};

use super::ShapeId;
use super::descriptor::PropertyFlags;
use super::shape_body::{
    ShapeBody, ShapeHandle, alloc_child_shape_body_with_roots, alloc_root_shape_body_with_roots,
    shape_atoms_ordered,
};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct TransitionKey {
    parent: ShapeId,
    atom: AtomId,
    /// Attribute bits of the appended slot. Keying transitions by attributes
    /// (not just the key) keeps a `define`-with-non-default-attributes append
    /// on a distinct shape from an ordinary default-data append of the same
    /// key, so shape-id IC guards invalidate correctly.
    flags: PropertyFlags,
    /// Data vs accessor for the appended slot — part of the transition
    /// identity for the same reason as [`Self::flags`].
    is_accessor: bool,
}

/// One property name as the shape layer needs it: the isolate-global atom that
/// keys transitions and shape nodes, plus the GC string body shape nodes and
/// key enumeration hold.
#[derive(Debug)]
struct ShapeKey {
    handle: Cell<JsStringHandle>,
    atom: AtomId,
}

/// The weakly held tables: every shape by id, the transition cache, and the
/// flattened offset cache.
#[derive(Default)]
struct WeakShapeTables {
    handles_by_id: FxHashMap<ShapeId, ShapeHandle>,
    transitions: FxHashMap<TransitionKey, ShapeHandle>,
    offset_cache: FxHashMap<ShapeId, FxHashMap<AtomId, u32>>,
    /// Shapes created during the current outermost runtime turn — strong.
    pinned: Vec<ShapeHandle>,
}

/// Mutable side tables for GC-managed hidden classes.
pub(crate) struct ShapeRuntime {
    root: Cell<ShapeHandle>,
    tables: RefCell<WeakShapeTables>,
    next_string_id: u32,
    /// Names this shape layer has already seen, spelling → (string body, atom).
    /// The isolate interner is the identity authority; this map is the layer's
    /// own cache of it, so taking a known transition costs one hash of the
    /// spelling and never the interner lock.
    interned_keys: FxHashMap<Box<str>, ShapeKey>,
    /// This isolate's property-name interner, shared with the interpreter that
    /// owns this runtime.
    names: Arc<NameInterner>,
    observer: Option<Box<dyn ShapeTransitionObserver>>,
}

impl std::fmt::Debug for ShapeRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let tables = self.tables.borrow();
        f.debug_struct("ShapeRuntime")
            .field("root", &self.root)
            .field("next_string_id", &self.next_string_id)
            .field("interned_keys", &self.interned_keys.len())
            .field("transitions", &tables.transitions.len())
            .field("offset_cache", &tables.offset_cache.len())
            .field("observer_installed", &self.observer.is_some())
            .finish()
    }
}

impl ShapeRuntime {
    /// Allocate a fresh root shape and empty side tables.
    pub(crate) fn new(
        heap: &mut GcHeap,
        names: Arc<NameInterner>,
    ) -> Result<Self, otter_gc::OutOfMemory> {
        let mut roots = |_visitor: &mut dyn FnMut(*mut RawGc)| {};
        let root = alloc_root_shape_body_with_roots(heap, &mut roots)?;
        let root_id = heap.read_payload(root, ShapeBody::id);
        let mut tables = WeakShapeTables::default();
        tables.handles_by_id.insert(root_id, root);
        Ok(Self {
            root: Cell::new(root),
            tables: RefCell::new(tables),
            next_string_id: 1,
            interned_keys: FxHashMap::default(),
            names,
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
    pub(crate) fn transitions_for_snapshot(&self) -> Vec<(ShapeId, ShapeHandle)> {
        self.tables
            .borrow()
            .transitions
            .iter()
            .map(|(key, child)| (key.parent, *child))
            .collect()
    }

    /// Empty hidden-class root.
    #[must_use]
    pub(crate) fn root(&self) -> ShapeHandle {
        self.root.get()
    }

    /// Remove every side-table entry before heap teardown.
    pub(crate) fn clear(&mut self) {
        self.interned_keys.clear();
        *self.tables.borrow_mut() = WeakShapeTables::default();
        self.root.set(ShapeHandle::null());
    }

    /// A shell for the snapshot restore path: null root, empty side
    /// tables. The snapshot root walk writes the restored root through
    /// [`Self::visit_root_slot`]; [`Self::register_restored_shape`]
    /// re-populates `handles_by_id` from the restored heap, and the
    /// remaining tables are lookup caches that refill on use.
    #[must_use]
    pub(crate) fn restored_shell(names: Arc<NameInterner>) -> Self {
        Self {
            root: Cell::new(ShapeHandle::null()),
            tables: RefCell::new(WeakShapeTables::default()),
            next_string_id: 0,
            interned_keys: FxHashMap::default(),
            names,
            observer: None,
        }
    }

    /// Record one restored shape body under its id.
    pub(crate) fn register_restored_shape(&mut self, id: ShapeId, handle: ShapeHandle) {
        self.tables.borrow_mut().handles_by_id.insert(id, handle);
    }

    /// Yield the root shape's cell slot for the snapshot root walk. The
    /// other side tables are caches a restore rebuilds from the heap.
    pub(crate) fn visit_root_slot(&self, visitor: &mut dyn FnMut(*mut RawGc)) {
        visitor(self.root.as_ptr() as *mut RawGc);
    }

    /// Yield the strong handles: the root shape and the interned key
    /// strings. The id and transition tables are weak ([`Self::sweep_dead`]).
    pub(crate) fn trace_roots(&self, visitor: &mut SlotVisitor<'_>) {
        if !self.root.get().is_null() {
            let p = self.root.as_ptr() as *mut RawGc;
            visitor(p);
        }
        for key in self.interned_keys.values() {
            let p = key.handle.as_ptr() as *mut RawGc;
            visitor(p);
        }
        for shape in &self.tables.borrow().pinned {
            // Shapes never move, so the slot is never rewritten.
            visitor(std::ptr::from_ref(shape).cast_mut().cast::<RawGc>());
        }
    }

    /// Release the shapes created during the turn that just ended: from the
    /// next full collection on they live only while something names them.
    pub(crate) fn unpin_turn_shapes(&self) {
        self.tables.borrow_mut().pinned.clear();
    }

    /// The full collection's weak pass: forget every shape the marking left
    /// unreached, and return their compressed offsets so the owner can drop
    /// every cache entry that names one before the sweep frees the cells.
    pub(crate) fn sweep_dead(&self, heap: &GcHeap) -> FxHashSet<u32> {
        let mut dead = FxHashSet::default();
        let mut tables = self.tables.borrow_mut();
        let WeakShapeTables {
            handles_by_id,
            transitions,
            offset_cache,
            ..
        } = &mut *tables;
        handles_by_id.retain(|_, shape| {
            let live = heap.is_marked(shape.raw());
            if !live {
                dead.insert(shape.offset());
            }
            live
        });
        if dead.is_empty() {
            return dead;
        }
        transitions.retain(|_, child| !dead.contains(&child.offset()));
        offset_cache.retain(|id, _| handles_by_id.contains_key(id));
        dead
    }

    /// Resolve stable feedback identity back to the isolate-local GC handle.
    #[must_use]
    pub(crate) fn handle_for_id(&self, id: ShapeId) -> Option<ShapeHandle> {
        self.tables.borrow().handles_by_id.get(&id).copied()
    }

    #[must_use]
    pub(crate) fn id_for_handle(&self, heap: &GcHeap, shape: ShapeHandle) -> ShapeId {
        heap.read_payload(shape, ShapeBody::id)
    }

    /// Intern a property key as a GC-managed string body plus its global atom.
    pub(crate) fn intern_key_with_roots(
        &mut self,
        heap: &mut GcHeap,
        key: &str,
        external_visit: &mut RootSlotVisitor<'_>,
    ) -> Result<(JsStringHandle, AtomId), otter_gc::OutOfMemory> {
        if let Some(existing) = self.interned_keys.get(key) {
            return Ok((existing.handle.get(), existing.atom));
        }
        let units: Vec<u16> = key.encode_utf16().collect();
        let id = JsStringId::new(self.next_string_id);
        let mut visit_roots = |visitor: &mut dyn FnMut(*mut RawGc)| {
            self.trace_roots(visitor);
            external_visit(visitor);
        };
        let handle = alloc_flat_string_body_with_roots(heap, id, &units, &mut visit_roots)?;
        self.next_string_id = self.next_string_id.saturating_add(1);
        let atom = self.names.intern(key);
        self.interned_keys.insert(
            key.into(),
            ShapeKey {
                handle: Cell::new(handle),
                atom,
            },
        );
        Ok((handle, atom))
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
        let atom = self.interned_keys.get(key)?.atom;
        let parent_id = heap.read_payload(parent, ShapeBody::id);
        let transition_key = TransitionKey {
            parent: parent_id,
            atom,
            flags,
            is_accessor,
        };
        let child = self.tables.borrow().transitions.get(&transition_key).copied()?;
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
        let (key_handle, atom) = self.intern_key_with_roots(heap, key, &mut visit_parent)?;
        let parent_id = heap.read_payload(parent, ShapeBody::id);
        let transition_key = TransitionKey {
            parent: parent_id,
            atom,
            flags,
            is_accessor,
        };
        let existing = self.tables.borrow().transitions.get(&transition_key).copied();
        if let Some(child) = existing {
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
            atom,
            flags,
            is_accessor,
            &mut visit_child_roots,
        )?;
        let child_id = heap.read_payload(child, ShapeBody::id);
        let mut tables = self.tables.borrow_mut();
        tables.handles_by_id.insert(child_id, child);
        tables.transitions.insert(transition_key, child);
        tables.pinned.push(child);
        drop(tables);
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
        let atom = self.interned_keys.get(key)?.atom;
        let shape_id = heap.read_payload(shape, ShapeBody::id);
        if let Some(cache) = self.tables.borrow().offset_cache.get(&shape_id) {
            return cache.get(&atom).copied();
        }

        let mut cache = FxHashMap::default();
        for (atom, offset) in shape_atoms_ordered(heap, shape) {
            cache.insert(atom, offset);
        }
        let result = cache.get(&atom).copied();
        self.tables.borrow_mut().offset_cache.insert(shape_id, cache);
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
        let mut runtime = ShapeRuntime::new(&mut heap, Arc::default()).expect("runtime");
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

    #[test]
    fn unnamed_shapes_are_collected_after_their_turn() {
        let mut interp = crate::Interpreter::new();
        let root = interp.shape_root();
        let unused = interp.shape_child(root, "only_in_tables").expect("child");
        let unused_id = interp.gc_heap.read_payload(unused, ShapeBody::id);
        let kept = interp.shape_child(root, "kept").expect("child");
        let kept_id = interp.gc_heap.read_payload(kept, ShapeBody::id);
        assert_ne!(kept_id, unused_id);
        let object = interp
            .alloc_runtime_rooted_object_with_roots(&[], &[])
            .expect("object");
        interp
            .set_property(object, "kept", crate::Value::boolean(true))
            .expect("store");
        // The global object keeps the object, and so its shape, alive.
        let global = interp.global_this;
        interp
            .set_property(global, "holder", crate::Value::object(object))
            .expect("hold object");

        // Pinned while the (implicit) turn that created them is open.
        interp.force_gc().expect("force GC");
        assert!(interp.shape_runtime.handle_for_id(unused_id).is_some());

        interp.shape_runtime.unpin_turn_shapes();
        interp.force_gc().expect("force GC");
        assert!(interp.shape_runtime.handle_for_id(unused_id).is_none());
        assert!(interp.shape_runtime.handle_for_id(kept_id).is_some());
    }
}
