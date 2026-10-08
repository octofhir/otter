//! Isolate-wide shared property actions under one `(shape, atom)` key.
//!
//! # Contents
//! - [`PropertyActionCache`] owns one fixed set-associative scalar table.
//! - Independent load and store facts share each key and retained proof owners.
//! - [`jit`] describes the same entries to all generated property probes.
//!
//! # Invariants
//! Exact immutable shape identity fixes own descriptors and the first prototype.
//! A key can simultaneously load an inherited slot and append an own slot;
//! those actions have separate slots and nonreviving validity proofs. Generated
//! probes do not allocate or reenter. Target shapes are traced in their actual
//! entry words; holder references are weak and cleared by their own mark state
//! before reuse. A runtime allocating replay releases all proof borrows and
//! rereads the canonical child word after GC. Every miss precedes effects and
//! enters the source operation once.
//!
//! # See also
//! - `crate::object::shape_transition` owns the one add-property replay kernel.
//! - `crate::cache_ir` owns bounded per-site programs over the same semantics.

use std::cell::{Cell, RefCell};
use std::sync::Arc;

use crate::cache_ir;
use crate::object::prototype_validity::PrototypeValidity;
use crate::object::{self, AtomOwnPropertyHit, JsObject, ShapeId};
use crate::property_atom::{AtomId, AtomizedPropertyKey};

pub(crate) mod jit;

const SETS: usize = 512;
const WAYS: usize = 4;
const HASH_SHAPE_MULTIPLIER: u64 = 0x9E37_79B9_7F4A_7C15;
const HASH_ATOM_MULTIPLIER: u64 = 0xC2B2_AE3D_27D4_EB4F;
const HASH_SHIFT: u8 = 32;

/// Selected live shared-table load action. Unknown and Unresolvable are misses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PropertyLoadAction {
    /// No load resolution is recorded under this key.
    Unknown = 0,
    /// A full walk found no cacheable data slot; retain the semantic ladder.
    Unresolvable = 1,
    /// Load a current own data slot.
    OwnData = 2,
    /// Load a current holder data slot under a complete chain proof.
    InheritedData = 3,
    /// The key is absent from the receiver and its whole chain: `undefined`
    /// under the chain proof (none when the receiver's shape fixes a `null`
    /// prototype). V8's stub cache holds `LoadNonExistent` handlers alike.
    NonExistent = 4,
}

/// Selected live shared-table store action, independently authorized from loads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PropertyStoreAction {
    /// No store authorization is recorded under this key.
    Unknown = 0,
    /// Overwrite a current own writable data slot.
    OwnWritable = 1,
    /// Append an own slot and publish the recorded child shape.
    AddOwn = 2,
}

/// The one physical scalar row read by generated and runtime probes.
#[derive(Debug, Clone, Copy)]
#[repr(C)]
struct PropertyActionEntry {
    receiver_shape_id: ShapeId,
    atom: AtomId,
    load_action: PropertyLoadAction,
    store_action: PropertyStoreAction,
    load_slot: u16,
    store_slot: u16,
    load_writable: bool,
    reserved: u8,
    holder_shape: object::ShapeHandle,
    holder_root: object::ShapeHandle,
    target_shape: object::ShapeHandle,
    holder_shape_id: ShapeId,
    target_shape_id: ShapeId,
    load_validity: usize,
    store_validity: usize,
}

impl PropertyActionEntry {
    const EMPTY: Self = Self {
        receiver_shape_id: ShapeId::UNASSIGNED,
        atom: AtomId::NONE,
        load_action: PropertyLoadAction::Unknown,
        store_action: PropertyStoreAction::Unknown,
        load_slot: 0,
        store_slot: 0,
        load_writable: false,
        reserved: 0,
        holder_shape: object::ShapeHandle::null(),
        holder_root: object::ShapeHandle::null(),
        target_shape: object::ShapeHandle::null(),
        holder_shape_id: ShapeId::UNASSIGNED,
        target_shape_id: ShapeId::UNASSIGNED,
        load_validity: 0,
        store_validity: 0,
    };

    fn hit(self) -> AtomOwnPropertyHit {
        AtomOwnPropertyHit {
            shape_id: self.holder_shape_id,
            shape: self.holder_shape,
            atom_id: self.atom,
            slot: self.load_slot,
            is_data: true,
        }
    }

    fn clear_load(&mut self) {
        self.load_action = PropertyLoadAction::Unknown;
        self.load_slot = 0;
        self.load_writable = false;
        self.holder_shape = object::ShapeHandle::null();
        self.holder_root = object::ShapeHandle::null();
        self.holder_shape_id = ShapeId::UNASSIGNED;
        self.load_validity = 0;
    }
}

/// Retention metadata only; the key, slots and child live solely in the scalar row.
#[derive(Debug, Default)]
struct ActionOwners {
    load: Option<Arc<PrototypeValidity>>,
    store: Option<object::StorePropertyTransitionKind>,
}

/// The selected runtime load fact, including an existing semantic resolution.
#[derive(Debug)]
pub(crate) enum PropertyProbe {
    /// A live data slot authorized under the key and retained chain proof.
    Resolved(cache_ir::ResolvedDataSlot),
    /// The key is absent from the receiver and its chain, under the live
    /// proof (none when the receiver's shape fixes a `null` prototype).
    Absent(Option<Arc<PrototypeValidity>>),
    /// A guarded walk found no cacheable data slot.
    Unresolvable,
    /// No matching valid fact exists.
    Unknown,
}

/// A named load's resolution through the shared table.
#[derive(Debug)]
pub(crate) enum PropertyLoad {
    /// A live own or inherited data slot.
    Data(cache_ir::ResolvedDataSlot),
    /// The key is absent from the receiver and its whole ordinary chain,
    /// under this proof (none for a `null` prototype).
    Absent(Option<Arc<PrototypeValidity>>),
    /// Anything the full `[[Get]]` must complete.
    Other,
}

/// One fixed key table for independent property read and write actions.
#[derive(Debug)]
pub(crate) struct PropertyActionCache {
    entries: Box<[Cell<PropertyActionEntry>]>,
    owners: Box<[RefCell<ActionOwners>]>,
}

impl Default for PropertyActionCache {
    fn default() -> Self {
        Self {
            entries: (0..SETS * WAYS)
                .map(|_| Cell::new(PropertyActionEntry::EMPTY))
                .collect(),
            owners: (0..SETS * WAYS)
                .map(|_| RefCell::new(ActionOwners::default()))
                .collect(),
        }
    }
}

impl PropertyActionCache {
    fn set_start(shape: ShapeId, atom: AtomId) -> usize {
        let mixed = shape.raw().wrapping_mul(HASH_SHAPE_MULTIPLIER)
            ^ u64::from(atom.raw()).wrapping_mul(HASH_ATOM_MULTIPLIER);
        (((mixed >> HASH_SHIFT) as usize) & (SETS - 1)) * WAYS
    }

    fn find(&self, shape: ShapeId, atom: AtomId) -> Option<usize> {
        let start = Self::set_start(shape, atom);
        (start..start + WAYS).find(|&index| {
            let entry = self.entries[index].get();
            entry.receiver_shape_id == shape && entry.atom == atom
        })
    }

    /// Reuse an exact key, or move one complete row plus its proof owners.
    /// Publication cannot interleave with generated probes or collection.
    fn record_way(&self, shape: ShapeId, atom: AtomId) -> usize {
        if let Some(index) = self.find(shape, atom) {
            return index;
        }
        let start = Self::set_start(shape, atom);
        let vacate = (start..start + WAYS)
            .find(|&index| self.entries[index].get().receiver_shape_id == ShapeId::UNASSIGNED)
            .unwrap_or(start + WAYS - 1);
        self.entries[vacate].set(PropertyActionEntry::EMPTY);
        self.owners[vacate].replace(ActionOwners::default());
        for index in (start + 1..=vacate).rev() {
            let owners = self.owners[index - 1].replace(ActionOwners::default());
            self.owners[index].replace(owners);
            self.entries[index].set(self.entries[index - 1].get());
        }
        self.entries[start].set(PropertyActionEntry {
            receiver_shape_id: shape,
            atom,
            ..PropertyActionEntry::EMPTY
        });
        start
    }

    fn entry_for(
        &self,
        obj: JsObject,
        heap: &otter_gc::GcHeap,
        key: AtomizedPropertyKey<'_>,
    ) -> Option<(usize, PropertyActionEntry)> {
        if key.atom().id() == AtomId::NONE || !object::supports_fast_property_ic(obj, heap) {
            return None;
        }
        let index = self.find(object::shape_id(obj, heap), key.atom().id())?;
        Some((index, self.entries[index].get()))
    }

    pub(crate) fn probe_load(
        &self,
        obj: JsObject,
        heap: &otter_gc::GcHeap,
        key: AtomizedPropertyKey<'_>,
    ) -> PropertyProbe {
        let Some((index, entry)) = self.entry_for(obj, heap, key) else {
            return PropertyProbe::Unknown;
        };
        let validity = self.owners[index].borrow().load.clone();
        if matches!(
            entry.load_action,
            PropertyLoadAction::Unresolvable | PropertyLoadAction::NonExistent
        ) {
            return if !validity.as_ref().is_none_or(|cell| cell.is_valid()) {
                PropertyProbe::Unknown
            } else if entry.load_action == PropertyLoadAction::NonExistent {
                PropertyProbe::Absent(validity)
            } else {
                PropertyProbe::Unresolvable
            };
        }
        let hit = entry.hit();
        let (hops, value) = match entry.load_action {
            PropertyLoadAction::OwnData => {
                (0, object::load_own_data_slot_atom(obj, heap, key, hit))
            }
            PropertyLoadAction::InheritedData
                if validity.as_ref().is_some_and(|cell| cell.is_valid())
                    && !heap.read_payload(obj, |body| body.chain_link_opaque()) =>
            {
                let value = match object::shape_body::prototype_of(entry.holder_root) {
                    object::shape_body::ShapePrototype::Object(holder)
                        if object::shape(holder, heap) == entry.holder_shape
                            && object::shape_id(holder, heap) == entry.holder_shape_id =>
                    {
                        Some(object::load_proven_data_slot(holder, heap, entry.load_slot))
                    }
                    _ => None,
                };
                (1, value)
            }
            _ => return PropertyProbe::Unknown,
        };
        let Some(value) = value else {
            return PropertyProbe::Unknown;
        };
        let holder_root_id = if entry.holder_root.is_null() {
            ShapeId::UNASSIGNED
        } else {
            heap.read_payload(entry.holder_root, object::shape_body::ShapeBody::id)
        };
        PropertyProbe::Resolved(cache_ir::ResolvedDataSlot {
            hops,
            hit,
            value,
            is_writable: entry.load_writable,
            validity,
            holder_root: entry.holder_root,
            holder_root_id,
        })
    }

    /// Publish one load fact without discarding an independent append action.
    /// `absent` carries the chain proof of a key no link owns (`Some(None)`
    /// for a `null` prototype); it is ignored when `resolved` is present.
    pub(crate) fn record_load(
        &self,
        obj: JsObject,
        heap: &otter_gc::GcHeap,
        key: AtomizedPropertyKey<'_>,
        resolved: Option<&cache_ir::ResolvedDataSlot>,
        absent: Option<Option<Arc<PrototypeValidity>>>,
    ) {
        if key.atom().id() == AtomId::NONE || !object::supports_fast_property_ic(obj, heap) {
            return;
        }
        let nonexistent = resolved.is_none() && absent.is_some();
        let validity = if let Some(resolved) = resolved {
            resolved.validity.clone()
        } else if let Some(proof) = absent {
            proof
        } else if let Some(first) = object::prototype(obj, heap) {
            let Some(cell) = object::prototype_validity::chain_validity(first, heap) else {
                return;
            };
            Some(cell)
        } else if object::prototype_value(obj, heap).is_none() {
            None
        } else {
            return;
        };
        let index = self.record_way(object::shape_id(obj, heap), key.atom().id());
        let mut entry = self.entries[index].get();
        entry.clear_load();
        entry.load_validity = validity.as_ref().map_or(0, |cell| cell.address());
        if let Some(resolved) = resolved {
            entry.load_action = if resolved.hops == 0 {
                PropertyLoadAction::OwnData
            } else {
                PropertyLoadAction::InheritedData
            };
            entry.load_slot = resolved.hit.slot;
            entry.load_writable = resolved.is_writable;
            entry.holder_shape = resolved.hit.shape;
            entry.holder_shape_id = resolved.hit.shape_id;
            entry.holder_root = resolved.holder_root;
            if resolved.hops == 0 && resolved.is_writable {
                entry.store_action = PropertyStoreAction::OwnWritable;
                entry.store_slot = resolved.hit.slot;
                entry.target_shape = object::ShapeHandle::null();
                entry.target_shape_id = ShapeId::UNASSIGNED;
                entry.store_validity = 0;
                self.owners[index].borrow_mut().store = None;
            }
        } else if nonexistent {
            entry.load_action = PropertyLoadAction::NonExistent;
        } else {
            entry.load_action = PropertyLoadAction::Unresolvable;
        }
        self.owners[index].borrow_mut().load = validity;
        self.entries[index].set(entry);
    }

    /// Publish the existing own-data hit of a committed writable store program.
    /// The program's constructor already proved the data descriptor writable;
    /// immutable shape identity makes its load and overwrite facts compatible.
    pub(crate) fn record_own_store(&self, hit: AtomOwnPropertyHit) {
        if hit.atom_id == AtomId::NONE {
            return;
        }
        let index = self.record_way(hit.shape_id, hit.atom_id);
        self.owners[index].replace(ActionOwners::default());
        self.entries[index].set(PropertyActionEntry {
            receiver_shape_id: hit.shape_id,
            atom: hit.atom_id,
            load_action: PropertyLoadAction::OwnData,
            store_action: PropertyStoreAction::OwnWritable,
            load_slot: hit.slot,
            store_slot: hit.slot,
            load_writable: true,
            holder_shape: hit.shape,
            holder_shape_id: hit.shape_id,
            ..PropertyActionEntry::EMPTY
        });
    }

    /// Merge a committed add sample into its original key, retaining any load.
    pub(crate) fn record_store(&self, transition: object::StorePropertyTransition) {
        if transition.atom_id == AtomId::NONE {
            return;
        }
        let index = self.record_way(transition.from_shape_id, transition.atom_id);
        let mut entry = self.entries[index].get();
        entry.store_action = PropertyStoreAction::AddOwn;
        entry.store_slot = transition.slot;
        entry.target_shape = transition.to_shape.get();
        entry.target_shape_id = transition.to_shape_id;
        entry.store_validity = match &transition.kind {
            object::StorePropertyTransitionKind::OwnAdd => 0,
            object::StorePropertyTransitionKind::PrototypeChainMissing { validity }
            | object::StorePropertyTransitionKind::PrototypeWritableData { validity } => {
                validity.address()
            }
        };
        self.owners[index].borrow_mut().store = Some(transition.kind);
        self.entries[index].set(entry);
    }

    /// Replay only the selected store action, preserving a matched refusal.
    pub(crate) fn replay_store(
        &self,
        obj: JsObject,
        heap: &mut otter_gc::GcHeap,
        key: AtomizedPropertyKey<'_>,
        value: &crate::Value,
    ) -> Result<Option<PropertyStoreAction>, otter_gc::OutOfMemory> {
        if object::state(obj, heap).is_prototype() {
            return Ok(None);
        }
        let Some((index, entry)) = self.entry_for(obj, heap, key) else {
            return Ok(None);
        };
        match entry.store_action {
            PropertyStoreAction::OwnWritable => Ok(object::store_own_data_slot_atom(
                obj,
                heap,
                key,
                AtomOwnPropertyHit {
                    shape_id: entry.receiver_shape_id,
                    shape: object::shape(obj, heap),
                    atom_id: entry.atom,
                    slot: entry.store_slot,
                    is_data: true,
                },
                value,
            )
            .map(|()| PropertyStoreAction::OwnWritable)),
            PropertyStoreAction::AddOwn => {
                // No proof borrow crosses the allocating replay/weak sweep.
                let Some(kind) = self.owners[index].borrow().store.clone() else {
                    return Ok(None);
                };
                object::replay_store_property_transition(
                    obj,
                    heap,
                    key,
                    entry.receiver_shape_id,
                    entry.atom,
                    entry.target_shape_id,
                    || self.entries[index].get().target_shape,
                    &kind,
                    entry.store_slot,
                    value,
                )
                .map(|result| result.map(|()| PropertyStoreAction::AddOwn))
            }
            PropertyStoreAction::Unknown => Ok(None),
        }
    }

    /// The full collection's weak pass: clear every load fact whose holder
    /// shape or holder root the marking left unreached, before the sweep frees
    /// those cells, without retiring a separately retained store.
    ///
    /// Liveness is read per reference from the mark bits, not from the shape
    /// id registry: heap-only producers (prototype-state and other state
    /// variants) create holder shapes that no id table names, and their death
    /// must still clear the weak word before its cell can be reused.
    pub(crate) fn sweep_dead_holders(&self, heap: &otter_gc::GcHeap) {
        let dead = |shape: object::ShapeHandle| !shape.is_null() && !heap.is_marked(shape.raw());
        for (index, way) in self.entries.iter().enumerate() {
            let mut entry = way.get();
            if dead(entry.holder_shape) || dead(entry.holder_root) {
                entry.clear_load();
                if entry.store_action == PropertyStoreAction::Unknown {
                    entry = PropertyActionEntry::EMPTY;
                }
                way.set(entry);
                self.owners[index].borrow_mut().load = None;
            }
        }
    }

    /// Trace each append child at its actual scalar word, with no mirrored row.
    pub(crate) fn trace_roots(&self, visitor: &mut otter_gc::raw::SlotVisitor<'_>) {
        for way in &self.entries {
            if !way.get().target_shape.is_null() {
                // SAFETY: the initialized Cell owns this interior word for the
                // entire fixed table lifetime. ShapeHandle and RawGc have the
                // same compressed-word representation. No borrow survives GC.
                let slot = unsafe { std::ptr::addr_of_mut!((*way.as_ptr()).target_shape) }
                    .cast::<otter_gc::raw::RawGc>();
                visitor(slot);
            }
        }
    }
}

impl crate::Interpreter {
    /// Resolve a live named data slot using the one key/action cache.
    #[must_use]
    pub(crate) fn resolve_property_data_slot(
        &self,
        obj: JsObject,
        key: AtomizedPropertyKey<'_>,
    ) -> Option<cache_ir::ResolvedDataSlot> {
        match self.resolve_property_load(obj, key) {
            PropertyLoad::Data(resolved) => Some(resolved),
            PropertyLoad::Absent(_) | PropertyLoad::Other => None,
        }
    }

    /// Resolve a named load using the one key/action cache: a live data slot,
    /// a key absent from the receiver and its whole ordinary chain, or
    /// neither (accessors, proxies, opaque lookup). Records the answer.
    #[must_use]
    pub(crate) fn resolve_property_load(
        &self,
        obj: JsObject,
        key: AtomizedPropertyKey<'_>,
    ) -> PropertyLoad {
        if key.atom().id() == AtomId::NONE {
            return cache_ir::resolve_atom_data_slot(obj, &self.gc_heap, key)
                .map_or(PropertyLoad::Other, PropertyLoad::Data);
        }
        match self.property_cache.probe_load(obj, &self.gc_heap, key) {
            PropertyProbe::Resolved(resolved) => return PropertyLoad::Data(resolved),
            PropertyProbe::Absent(proof) => return PropertyLoad::Absent(proof),
            PropertyProbe::Unresolvable => return PropertyLoad::Other,
            PropertyProbe::Unknown => {}
        }
        let resolved = cache_ir::resolve_atom_data_slot(obj, &self.gc_heap, key);
        let absent = if resolved.is_none() {
            cache_ir::resolve_absent_atom(obj, &self.gc_heap, key)
        } else {
            None
        };
        if resolved.is_some() || absent.is_some() {
            self.shape_runtime
                .register_shape(&self.gc_heap, object::shape(obj, &self.gc_heap));
        }
        if let Some(resolved) = &resolved
            && !resolved.holder_root.is_null()
        {
            self.shape_runtime
                .register_shape(&self.gc_heap, resolved.holder_root);
        }
        self.property_cache
            .record_load(obj, &self.gc_heap, key, resolved.as_ref(), absent.clone());
        match (resolved, absent) {
            (Some(resolved), _) => PropertyLoad::Data(resolved),
            (None, Some(proof)) => PropertyLoad::Absent(proof),
            (None, None) => PropertyLoad::Other,
        }
    }

    /// Complete a named load on a receiver whose lookup starts at another
    /// object (V8's `lookup_start_object`) through the site's handlers,
    /// installing one on a miss. That object's shape, keyed with the
    /// lookup-start bit, fixes the receiver's whole ordinary lookup:
    /// - an ordinary dense array owns no named property but `length`, so
    ///   every other non-index name starts at `%Array.prototype%`;
    /// - a Map, Set, WeakMap, WeakSet, DataView, ArrayBuffer, TypedArray,
    ///   Promise or RegExp keeps user-defined own properties in a lazy
    ///   expando bag; without one it owns no named key but the ones its kind
    ///   defines (a TypedArray's numeric keys, a RegExp's `lastIndex`), so
    ///   every other name starts at its `[[Prototype]]`;
    /// - a closure or native function without a `[[Prototype]]` override, in
    ///   the default realm, starts at its own-property bag, whose prototype
    ///   mirrors the function's, for every name it does not synthesize
    ///   (`name` and `length` of a native; a closure's `prototype`, and its
    ///   `name`, `length`, `caller` and `arguments` unless the bag owns them).
    ///
    /// A native's bag starts as a prototype-less dictionary. Its first miss
    /// gives it the function's prototype and a hidden class, which allocates:
    /// a caller that continues after `None` re-reads its receiver from a
    /// root.
    pub(crate) fn lookup_start_load(
        &mut self,
        slot: crate::feedback::PropertyFeedbackSlot<'_>,
        receiver: crate::Value,
        key: AtomizedPropertyKey<'_>,
    ) -> Option<crate::Value> {
        let start = self.lookup_start_object(receiver)?;
        if let Some(value) = slot.native().probe_lookup_start_load(start, &self.gc_heap) {
            slot.record_hit();
            return Some(value);
        }
        slot.record_miss();
        let start = self.admit_lookup_start(receiver, start, key)?;
        let load = self.resolve_property_load(start, key);
        let shape = object::keyed_shape(start, &self.gc_heap);
        let (handler, value) = match load {
            PropertyLoad::Data(resolved) => (
                crate::property_ic::IcHandler::load_resolved(shape, &resolved),
                resolved.value,
            ),
            PropertyLoad::Absent(proof) => (
                crate::property_ic::IcHandler::load_nonexistent(shape, proof),
                crate::Value::undefined(),
            ),
            PropertyLoad::Other => return None,
        };
        if !slot.is_megamorphic()
            && let Some(handler) = handler
        {
            slot.install(handler.for_lookup_start());
        }
        Some(value)
    }

    /// The object a named lookup on `receiver` starts at, whatever the key,
    /// or `None` when the receiver's own lookup is not a bag or array.
    fn lookup_start_object(&self, receiver: crate::Value) -> Option<JsObject> {
        if let Some(array) = receiver.as_array() {
            return if crate::array::is_ordinary_dense(array, &self.gc_heap) {
                self.realm_intrinsics.array_prototype()
            } else {
                None
            };
        }
        if let Some(start) = self.exotic_lookup_start(receiver) {
            return start;
        }
        if self.active_realm_id != 0 {
            return None;
        }
        if let Some(native) = receiver.as_native_function() {
            return (!native.has_prototype_override(&self.gc_heap))
                .then(|| native.own_properties_bag(&self.gc_heap));
        }
        let closure = receiver.as_closure(&self.gc_heap)?;
        if closure.proto_override(&self.gc_heap).is_some() {
            return None;
        }
        // A closure without an own-property bag owns only its virtual
        // `name`, `length` and `prototype`, which admission refuses: every
        // other key starts at the `%Function.prototype%` an ordinary kind
        // inherits from.
        closure.own_props(&self.gc_heap).or_else(|| {
            (closure.named_lookup(&self.gc_heap) == crate::closure::CLOSURE_LOOKUP_ORDINARY)
                .then(|| self.function_prototype_object().ok())
                .flatten()
        })
    }

    /// The lookup start of a bag-backed exotic receiver: `Some(None)` while
    /// its expando bag or a non-object prototype makes it own its lookup,
    /// `None` when the receiver is no such exotic.
    #[allow(clippy::option_option)]
    fn exotic_lookup_start(&self, receiver: crate::Value) -> Option<Option<JsObject>> {
        use crate::collections;
        use crate::realm_intrinsics::Intrinsic;
        let heap = &self.gc_heap;
        let (expando, prototype, intrinsic) = if let Some(map) = receiver.as_map() {
            (
                collections::map_expando(map, heap),
                collections::map_prototype_override(map, heap),
                Intrinsic::MapPrototype,
            )
        } else if let Some(set) = receiver.as_set() {
            (
                collections::set_expando(set, heap),
                collections::set_prototype_override(set, heap),
                Intrinsic::SetPrototype,
            )
        } else if let Some(map) = receiver.as_weak_map() {
            (
                collections::weak_map_expando(map, heap),
                collections::weak_map_prototype_override(map, heap),
                Intrinsic::WeakMapPrototype,
            )
        } else if let Some(set) = receiver.as_weak_set() {
            (
                collections::weak_set_expando(set, heap),
                collections::weak_set_prototype_override(set, heap),
                Intrinsic::WeakSetPrototype,
            )
        } else if let Some(view) = receiver.as_data_view() {
            (
                view.expando(heap),
                view.custom_proto(heap),
                Intrinsic::DataViewPrototype,
            )
        } else if let Some(buffer) = receiver.as_array_buffer() {
            let intrinsic = if buffer.is_shared() {
                Intrinsic::SharedArrayBufferPrototype
            } else {
                Intrinsic::ArrayBufferPrototype
            };
            (buffer.expando(heap), buffer.custom_proto(heap), intrinsic)
        } else if let Some(array) = receiver.as_typed_array(heap) {
            (
                array.expando(heap),
                array.custom_proto(heap),
                Intrinsic::typed_array_prototype(array.kind()),
            )
        } else if let Some(promise) = receiver.as_promise() {
            (
                promise.expando(heap),
                promise.prototype_override(heap),
                Intrinsic::PromisePrototype,
            )
        } else if let Some(regexp) = receiver.as_regexp() {
            (
                regexp.expando(heap),
                regexp.prototype_override(heap),
                Intrinsic::RegExpPrototype,
            )
        } else {
            return None;
        };
        if expando.is_some() {
            return Some(None);
        }
        Some(match prototype {
            Some(prototype) => prototype.as_object(),
            None => self.realm_intrinsics.get(intrinsic),
        })
    }

    /// `start` as the lookup start of `key` on `receiver`, once the key is
    /// one the receiver does not synthesize and a function's bag mirrors its
    /// prototype.
    fn admit_lookup_start(
        &mut self,
        receiver: crate::Value,
        mut start: JsObject,
        key: AtomizedPropertyKey<'_>,
    ) -> Option<JsObject> {
        let name = key.name();
        if receiver.as_array().is_some() {
            return (name != "length" && object::array_index_property_name(name).is_none())
                .then_some(start);
        }
        if receiver.as_typed_array(&self.gc_heap).is_some() {
            return crate::property_dispatch::canonical_numeric_index_string(name)
                .is_none()
                .then_some(start);
        }
        if receiver.as_regexp().is_some() {
            return (name != "lastIndex").then_some(start);
        }
        if receiver.as_native_function().is_none() && receiver.as_closure(&self.gc_heap).is_none() {
            return Some(start);
        }
        let native = receiver.as_native_function().is_some();
        let bagless = receiver
            .as_closure(&self.gc_heap)
            .is_some_and(|closure| closure.own_props(&self.gc_heap).is_none());
        let admitted = match name {
            "name" | "length" if native => false,
            "prototype" if !native => false,
            "name" | "length" | "caller" | "arguments" if bagless => false,
            "name" | "length" | "caller" | "arguments" if !native => {
                object::lookup_own_atom(start, &self.gc_heap, key)
                    .hit
                    .is_some()
            }
            _ => true,
        };
        if !admitted {
            return None;
        }
        if bagless {
            // The lookup already starts at the inherited prototype.
            return Some(start);
        }
        let prototype = self.get_prototype_for_op(&receiver).ok()?;
        let mirrored = match object::prototype_value(start, &self.gc_heap) {
            Some(bag_prototype) => bag_prototype == prototype,
            None => prototype.is_null(),
        };
        if !mirrored {
            let prototype = prototype.as_object().filter(|_| native)?;
            if !object::set_prototype(&mut start, &mut self.gc_heap, Some(prototype)).ok()? {
                return None;
            }
        }
        if native && object::state(start, &self.gc_heap).is_dictionary() {
            self.migrate_slow_to_fast(&mut start);
        }
        Some(start)
    }

    /// V8 `LoadIC::UpdateCaches` for an ordinary receiver that missed its
    /// site: install the handler of the resolved data slot, or of a key the
    /// receiver and its whole chain lack. Neither allocates nor runs code.
    pub(crate) fn update_load_ic(
        &self,
        slot: crate::feedback::PropertyFeedbackSlot<'_>,
        obj: JsObject,
        load: &PropertyLoad,
    ) {
        if slot.is_megamorphic() {
            return;
        }
        let shape = object::keyed_shape(obj, &self.gc_heap);
        let handler = match load {
            PropertyLoad::Data(resolved) => {
                crate::property_ic::IcHandler::load_resolved(shape, resolved)
            }
            PropertyLoad::Absent(proof) => {
                crate::property_ic::IcHandler::load_nonexistent(shape, proof.clone())
            }
            PropertyLoad::Other => None,
        };
        if let Some(handler) = handler {
            slot.install(handler);
        }
    }
}

#[cfg(test)]
mod tests;
