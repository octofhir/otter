//! Heap-owned canonical hidden-class interning and weak lifetime.
//!
//! # Contents
//! - `ShapeInterner` owns the isolate's one id, transition and state-variant table.
//! - Heap-only preparation registers roots and interns every append transition.
//! - Root tracing, turn pins, weak pruning and restored-page rebuilding share that owner.
//!
//! # Invariants
//! - The heap's `IsolateNames` owns exactly one interner; no heap pointer is retained.
//! - Shape handles are non-moving old-space cells. Only current-turn pins are strong.
//! - No table or payload borrow crosses GC allocation, root publication or observation.
//! - Transition identity uses stable parent and atom ids, descriptors.
//! - Shapes encode immutable geometry and descriptors; numeric stores never
//!   create variants, rewrite prefixes or change transition identity.
//! - Dictionary shapes never resolve through ordinary shape ids.
//!
//! # See also
//! - `super::shape_runtime` is the interpreter's observation facade.
//! - `crate::isolate_names` owns this value and dispatches its GC callbacks.

use super::descriptor::PropertyFlags;
use super::shape_body::ShapeKey;
use super::shape_body::{self, ShapeBody, ShapeHandle};
use super::{ShapeId, ShapeState};
use crate::isolate_names::IsolateNames;
use crate::property_atom::AtomId;
use crate::string::{JsStringHandle, gc_body::alloc_old_string_body_with_roots};
use otter_gc::heap::RootSlotVisitor;
use otter_gc::raw::{RawGc, SlotVisitor};
use otter_gc::{GcHeap, HandleScope};
use rustc_hash::{FxHashMap, FxHashSet};

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

/// The weakly held tables: every shape by id and the transition cache.
#[derive(Default)]
pub(crate) struct WeakShapeTables {
    handles_by_id: FxHashMap<ShapeId, ShapeHandle>,
    transitions: FxHashMap<TransitionKey, ShapeHandle>,
    /// The same transitions by parent: every interned child of each shape.
    children: FxHashMap<ShapeId, smallvec::SmallVec<[ShapeHandle; 2]>>,
    /// Rooted state variants are weak and keyed by stable source identity.
    state_variants: FxHashMap<(ShapeId, ShapeState), ShapeHandle>,
    /// Transitions out of each provisional shape: the tree slack tracking
    /// finishes in place.
    provisional_children: FxHashMap<ShapeId, smallvec::SmallVec<[ShapeHandle; 2]>>,
    /// Distinct shapes pinned during the current outermost turn, in first-pin order.
    /// The stable id is only an index; the handle is the sole strong root slot.
    pinned: indexmap::IndexMap<
        ShapeId,
        ShapeHandle,
        std::hash::BuildHasherDefault<rustc_hash::FxHasher>,
    >,
}

impl WeakShapeTables {
    /// Retain a shape once per turn. Shapes are non-moving and their ids are
    /// never reused; ending a turn clears this same strong-root authority.
    fn pin(&mut self, heap: &GcHeap, shape: ShapeHandle) {
        let id = heap.read_payload(shape, ShapeBody::id);
        self.pinned.entry(id).or_insert(shape);
    }
}

/// One isolate's canonical shape authority, owned by its heap.
#[derive(Default)]
pub(crate) struct ShapeInterner {
    /// Shared with the isolate's compilations, which read it while the
    /// mutator is not writing it.
    tables: std::sync::Arc<crate::mutator_owned::MutatorCell<WeakShapeTables>>,
}

impl ShapeInterner {
    fn of(heap: &GcHeap) -> &Self {
        IsolateNames::of(heap).shapes()
    }

    /// Run `f` over the tables, as the mutator reads them.
    pub(crate) fn with_tables<R>(
        heap: &GcHeap,
        f: impl FnOnce(&WeakShapeTables) -> R,
    ) -> Option<R> {
        Some(f(&Self::of(heap).tables.borrow()))
    }

    /// The tables, for an isolate's compilations.
    pub(crate) fn shared_tables(
        heap: &GcHeap,
    ) -> std::sync::Arc<crate::mutator_owned::MutatorCell<WeakShapeTables>> {
        std::sync::Arc::clone(&Self::of(heap).tables)
    }

    /// Register a rooted ordinary lineage, pinning each identity once this turn.
    ///
    /// An interned shape's whole lineage is interned with it (a live child
    /// keeps its ancestors), so registering it again only pins it for the
    /// current turn.
    pub(crate) fn register_shape(heap: &GcHeap, shape: ShapeHandle) {
        if !shape_body::is_dictionary_of(shape) {
            let id = heap.read_payload(shape, ShapeBody::id);
            let mut tables = Self::of(heap).tables.borrow_mut();
            if tables.handles_by_id.get(&id) == Some(&shape) {
                tables.pinned.entry(id).or_insert(shape);
                return;
            }
        }
        let root = shape_body::lineage_root_of(heap, shape);
        let dictionary = shape_body::dictionary_of(root);
        let mut tables = Self::of(heap).tables.borrow_mut();
        tables
            .handles_by_id
            .insert(heap.read_payload(root, ShapeBody::id), root);
        tables.pin(heap, root);
        tables.pin(heap, dictionary);
        if shape_body::is_dictionary_of(shape) {
            return;
        }
        let mut current = shape;
        while !heap.read_payload(current, ShapeBody::is_root) {
            Self::register_child(heap, &mut tables, current);
            tables.pin(heap, current);
            current = heap.read_payload(current, ShapeBody::parent);
        }
    }

    fn register_child(heap: &GcHeap, tables: &mut WeakShapeTables, child: ShapeHandle) {
        let (id, parent, atom, flags, is_accessor, provisional) =
            heap.read_payload(child, |body| {
                (
                    body.id(),
                    body.parent(),
                    body.transition_atom(),
                    body.own_flags(),
                    body.own_is_accessor(),
                    shape_body::state_of(child).is_provisional(),
                )
            });
        let parent = heap.read_payload(parent, ShapeBody::id);
        let key = TransitionKey {
            parent,
            atom,
            flags,
            is_accessor,
        };
        tables.handles_by_id.insert(id, child);
        if let Some(existing) = tables.transitions.get(&key) {
            assert_eq!(*existing, child, "one canonical child per transition");
        } else {
            tables.transitions.insert(key, child);
            tables.children.entry(parent).or_default().push(child);
            if provisional {
                tables
                    .provisional_children
                    .entry(parent)
                    .or_default()
                    .push(child);
            }
        }
    }

    /// Allocate a root/dictionary pair and register it before returning.
    pub(crate) fn new_root(
        heap: &mut GcHeap,
        prototype: shape_body::ShapePrototype,
        inline_capacity: usize,
        previous_layout_root: ShapeHandle,
        state: ShapeState,
        external_visit: &mut RootSlotVisitor<'_>,
    ) -> Result<ShapeHandle, otter_gc::OutOfMemory> {
        let root = shape_body::alloc_root_shape_body_with_roots(
            heap,
            prototype,
            inline_capacity,
            previous_layout_root,
            state,
            external_visit,
        )?;
        Self::register_shape(heap, root);
        Ok(root)
    }

    /// Complete one provisional family's physical geometry without allocating under a borrow.
    pub(crate) fn finish_slack_tracking(
        heap: &mut GcHeap,
        root: ShapeHandle,
        required: usize,
        external_visit: &mut RootSlotVisitor<'_>,
    ) -> Result<(), otter_gc::OutOfMemory> {
        let mut nodes = vec![root];
        {
            let tables = Self::of(heap).tables.borrow();
            let mut index = 0;
            while index < nodes.len() {
                let id = heap.read_payload(nodes[index], ShapeBody::id);
                if let Some(children) = tables.provisional_children.get(&id) {
                    nodes.extend(children.iter().copied());
                }
                index += 1;
            }
        }
        let capacity = shape_body::inline_capacity_of(root);
        let used = nodes
            .iter()
            .map(|&node| (shape_body::property_count_of(node) as usize).min(capacity))
            .max()
            .unwrap_or(0);
        let finished = used.max(required).min(capacity);
        let mut visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            for node in &mut nodes {
                visitor(std::ptr::from_mut(node).cast::<RawGc>());
            }
            external_visit(visitor);
        };
        let dictionary =
            shape_body::alloc_dictionary_shape_body_with_roots(heap, root, finished, &mut visit)?;
        for &node in &nodes {
            shape_body::finish_slack_tracking(heap, node, finished, dictionary);
        }
        let mut tables = Self::of(heap).tables.borrow_mut();
        tables.pin(heap, dictionary);
        for &node in &nodes {
            tables
                .provisional_children
                .remove(&heap.read_payload(node, ShapeBody::id));
        }
        Ok(())
    }

    /// Prepare an immutable same-geometry state variant through canonical children.
    pub(crate) fn state_with_roots(
        heap: &mut GcHeap,
        source: ShapeHandle,
        state: ShapeState,
        external_visit: &mut RootSlotVisitor<'_>,
    ) -> Result<ShapeHandle, otter_gc::OutOfMemory> {
        if shape_body::state_of(source) == state {
            return Ok(source);
        }
        // SAFETY: the source remains a real root even on the cached target's allocating repair.
        let scope = unsafe { HandleScope::from_ptr(heap.handle_stack_ptr()) };
        let source_root = scope.local(source);
        let source = source_root.get();
        let source_id = heap.read_payload(source, ShapeBody::id);
        let existing = Self::of(heap)
            .tables
            .borrow()
            .state_variants
            .get(&(source_id, state))
            .copied();
        if let Some(target) = existing {
            return Ok(target);
        }
        let result = super::state_transition::prepare_state_shape_uncached(
            heap,
            source,
            state,
            external_visit,
        )?;
        Self::register_shape(heap, result);
        Self::of(heap)
            .tables
            .borrow_mut()
            .state_variants
            .insert((source_id, state), result);
        Ok(result)
    }

    pub(crate) fn handle_for_id(
        heap: &impl crate::heap_read::HeapRead,
        id: ShapeId,
    ) -> Option<ShapeHandle> {
        heap.with_shape_tables(|tables| tables.handles_by_id.get(&id).copied())
            .flatten()
    }

    pub(crate) fn transitions_for_snapshot(heap: &GcHeap) -> Vec<(ShapeId, ShapeHandle)> {
        Self::of(heap)
            .tables
            .borrow()
            .transitions
            .iter()
            .map(|(key, child)| (key.parent, *child))
            .collect()
    }

    /// Intern the canonical old-space key, releasing isolate borrows before allocation.
    pub(crate) fn intern_key_with_roots(
        heap: &mut GcHeap,
        key: &str,
        external_visit: &mut RootSlotVisitor<'_>,
    ) -> Result<(JsStringHandle, AtomId), otter_gc::OutOfMemory> {
        {
            let isolate = IsolateNames::of(heap);
            let known = isolate.names().lookup(key);
            if let Some(handle) = isolate.key_string(known) {
                return Ok((handle, known));
            }
        }
        let units: Vec<u16> = crate::wtf16_escape::units(key).collect();
        let handle = alloc_old_string_body_with_roots(heap, &units, external_visit)?;
        let atom = {
            let isolate = IsolateNames::of_mut(heap);
            let atom = isolate.names_mut().intern(key);
            isolate.set_key_string(atom, handle);
            atom
        };
        heap.with_payload(handle, |body| body.set_cached_atom(atom));
        Ok((handle, atom))
    }

    /// Every interned child of `parent`: the shapes one own-property append
    /// to an object of that shape has produced. Never allocates.
    pub(crate) fn children_of(
        heap: &impl crate::heap_read::HeapRead,
        parent: ShapeHandle,
    ) -> smallvec::SmallVec<[ShapeHandle; 2]> {
        let parent = heap.read_payload(parent, ShapeBody::id);
        heap.with_shape_tables(|tables| tables.children.get(&parent).cloned())
            .flatten()
            .unwrap_or_default()
    }

    /// The interned default-data child of `parent` appending `atom`, if one
    /// exists. Never allocates.
    pub(crate) fn data_child(
        heap: &impl crate::heap_read::HeapRead,
        parent: ShapeHandle,
        atom: AtomId,
    ) -> Option<ShapeHandle> {
        Self::child_exact_if_cached(heap, parent, atom, PropertyFlags::data_default(), false)
    }

    pub(crate) fn child_if_cached(
        heap: &GcHeap,
        parent: ShapeHandle,
        key: &str,
        flags: PropertyFlags,
        is_accessor: bool,
    ) -> Option<ShapeHandle> {
        let atom = crate::isolate_names::names(heap).lookup(key);
        Self::child_exact_if_cached(heap, parent, atom, flags, is_accessor)
    }

    fn child_exact_if_cached(
        heap: &impl crate::heap_read::HeapRead,
        parent: ShapeHandle,
        atom: AtomId,
        flags: PropertyFlags,
        is_accessor: bool,
    ) -> Option<ShapeHandle> {
        let key = TransitionKey {
            parent: heap.read_payload(parent, ShapeBody::id),
            atom,
            flags,
            is_accessor,
        };
        heap.with_shape_tables(|tables| tables.transitions.get(&key).copied())
            .flatten()
    }

    /// Intern one immutable descriptor/key append on its exact parent.
    pub(crate) fn child_with_roots(
        heap: &mut GcHeap,
        parent: ShapeHandle,
        key: &str,
        flags: PropertyFlags,
        is_accessor: bool,
        external_visit: &mut RootSlotVisitor<'_>,
    ) -> Result<(ShapeHandle, bool), otter_gc::OutOfMemory> {
        // SAFETY: the parent survives collecting key preparation.
        let scope = unsafe { HandleScope::from_ptr(heap.handle_stack_ptr()) };
        let parent = scope.local(parent);
        let (key, atom) = Self::intern_key_with_roots(heap, key, external_visit)?;
        Self::child_atom_with_roots(
            heap,
            parent.get(),
            ShapeKey::string(key),
            atom,
            flags,
            is_accessor,
            external_visit,
        )
    }

    /// Intern one symbol-keyed append on its exact parent: the symbol keys
    /// the slot by its own atom.
    pub(crate) fn symbol_child_with_roots(
        heap: &mut GcHeap,
        parent: ShapeHandle,
        key: crate::symbol::JsSymbol,
        flags: PropertyFlags,
        is_accessor: bool,
        external_visit: &mut RootSlotVisitor<'_>,
    ) -> Result<(ShapeHandle, bool), otter_gc::OutOfMemory> {
        let atom = key.property_atom(heap);
        Self::child_atom_with_roots(
            heap,
            parent,
            ShapeKey::symbol(key.handle()),
            atom,
            flags,
            is_accessor,
            external_visit,
        )
    }

    /// Intern one atomized append; stored values never change shape identity.
    pub(crate) fn child_atom_with_roots(
        heap: &mut GcHeap,
        parent: ShapeHandle,
        key: ShapeKey,
        atom: AtomId,
        flags: PropertyFlags,
        is_accessor: bool,
        external_visit: &mut RootSlotVisitor<'_>,
    ) -> Result<(ShapeHandle, bool), otter_gc::OutOfMemory> {
        Self::child_exact_with_roots(heap, parent, key, atom, flags, is_accessor, external_visit)
    }

    fn child_exact_with_roots(
        heap: &mut GcHeap,
        parent: ShapeHandle,
        key: ShapeKey,
        atom: AtomId,
        flags: PropertyFlags,
        is_accessor: bool,
        external_visit: &mut RootSlotVisitor<'_>,
    ) -> Result<(ShapeHandle, bool), otter_gc::OutOfMemory> {
        if let Some(child) = Self::child_exact_if_cached(heap, parent, atom, flags, is_accessor) {
            return Ok((child, true));
        }
        // SAFETY: the stable stack survives collecting key-table/body allocations.
        let scope = unsafe { HandleScope::from_ptr(heap.handle_stack_ptr()) };
        let parent = scope.local(parent);
        // The key is traced in place across the collecting allocations.
        let mut key = key;
        let key_slot = &mut key as *mut ShapeKey;
        let mut visit = |visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
            external_visit(visitor);
            // SAFETY: `key` outlives the allocation this visit roots.
            visitor(unsafe { (*key_slot).slot() });
        };
        let child = shape_body::alloc_child_shape_body_with_roots(
            heap,
            parent.get(),
            key,
            atom,
            flags,
            is_accessor,
            &mut visit,
        )?;
        let mut tables = Self::of(heap).tables.borrow_mut();
        Self::register_child(heap, &mut tables, child);
        tables.pin(heap, child);
        Ok((child, false))
    }

    /// Only the distinct current-turn pins are strong roots.
    pub(crate) fn trace_roots(&self, visitor: &mut SlotVisitor<'_>) {
        for shape in self.tables.borrow().pinned.values() {
            visitor(std::ptr::from_ref(shape).cast_mut().cast::<RawGc>());
        }
    }

    pub(crate) fn unpin_turn_shapes(heap: &GcHeap) {
        Self::of(heap).tables.borrow_mut().pinned.clear();
    }

    /// Forget white old-space handles immediately before physical sweep.
    pub(crate) fn sweep_dead(&self, heap: &GcHeap) {
        let mut dead = FxHashSet::default();
        let mut tables = self.tables.borrow_mut();
        let WeakShapeTables {
            handles_by_id,
            transitions,
            children,
            state_variants,
            provisional_children,
            ..
        } = &mut *tables;
        handles_by_id.retain(|_, shape| {
            let live = heap.is_marked(shape.raw());
            if !live {
                dead.insert(shape.offset());
            }
            live
        });
        state_variants.retain(|(source, _), result| {
            handles_by_id.contains_key(source) && heap.is_marked(result.raw())
        });
        if dead.is_empty() {
            return;
        }
        transitions.retain(|_, child| !dead.contains(&child.offset()));
        children.retain(|parent, shapes| {
            shapes.retain(|child| !dead.contains(&child.offset()));
            handles_by_id.contains_key(parent) && !shapes.is_empty()
        });
        provisional_children.retain(|parent, children| {
            children.retain(|child| !dead.contains(&child.offset()));
            handles_by_id.contains_key(parent)
        });
    }

    /// Reconstruct the one authority from admitted, relocated live pages without GC allocation.
    /// Derived source-to-state associations start empty; structural child identity does not.
    pub(crate) fn rebuild_restored(heap: &mut GcHeap) {
        let mut shapes = Vec::new();
        let base = otter_gc::cage_base() as usize;
        heap.for_each_live_payload::<ShapeBody, _>(|_space, body| {
            if !body.is_dictionary() {
                let header =
                    body as *const ShapeBody as usize - std::mem::size_of::<otter_gc::GcHeader>();
                // SAFETY: this live payload walk supplies the exact ShapeBody header.
                let handle: ShapeHandle =
                    unsafe { otter_gc::Gc::from_offset((header - base) as u32) };
                shapes.push(handle);
            }
        });
        let mut tables = WeakShapeTables::default();
        let mut keys = Vec::new();
        for shape in shapes {
            let (id, root, atom, key) = heap.read_payload(shape, |body| {
                (
                    body.id(),
                    body.is_root(),
                    body.transition_atom(),
                    body.transition_key(),
                )
            });
            tables.handles_by_id.insert(id, shape);
            if !root {
                Self::register_child(heap, &mut tables, shape);
                keys.push((atom, key));
            }
        }
        let isolate = IsolateNames::of_mut(heap);
        for (atom, key) in keys {
            if let Some(key) = key.as_string()
                && isolate.key_string(atom).is_none()
            {
                isolate.set_key_string(atom, key);
            }
        }
        *isolate.shapes().tables.borrow_mut() = tables;
    }

    #[cfg(test)]
    pub(crate) fn pinned_shapes(heap: &GcHeap) -> Vec<ShapeHandle> {
        Self::of(heap)
            .tables
            .borrow()
            .pinned
            .values()
            .copied()
            .collect()
    }
}
