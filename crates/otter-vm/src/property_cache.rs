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
//! Store owners are retained by their receiver or append child. Weak cleanup
//! clears a whole row before a dead receiver or defining owner can be reused.
//! Store facts exist only for shapes whose stores stay in place (not a
//! dictionary, opaque or prototype shape) and appends only from extensible
//! ones; shape state is immutable, so probes test no state.
//! A shaped store uses zero owner only for an absorbing Top summary. An append
//! with a null child has zero owner for VM dictionary replay; generated probes
//! reject null children before effects and never use that replay marker.
//! Inherited loads and own additions retain independent slots. All data words
//! are valid Values; reads preserve their encoding and writes observe the
//! incoming category even when the summary is already Top.
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

/// Sets of the shared table. It serves every keyed and megamorphic named
/// site of the isolate under `(shape, atom)` keys, so it is sized for a large
/// program's live (shape, name) working set: 64K entries. The rows are
/// committed lazily — an all-zero row is empty — so an isolate pays only for
/// the rows its program fills.
const SETS: usize = 16384;
const WAYS: usize = 4;
/// A key hashes as one multiplicative product: the atom shifted above the
/// shape identity, times the multiplier, whose top bits select the set.
const HASH_MULTIPLIER: u64 = 0x9E37_79B9_7F4A_7C15;
const HASH_ATOM_SHIFT: u8 = 32;
const HASH_SHIFT: u8 = 64 - SETS.trailing_zeros() as u8;

/// Whether a receiver of a shape in `state` stores through the table: not a
/// dictionary, not an opaque lookup and not a prototype, whose semantic
/// mutations retire dependent proofs in the runtime. Shape state is
/// immutable, so a recorded store fact needs no state test at its probe.
fn stores_in_place(state: object::ShapeState) -> bool {
    !state.is_dictionary() && !state.is_opaque() && !state.is_prototype()
}

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
    /// prototype).
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
    holder_shape: object::ShapeHandle,
    holder_root: object::ShapeHandle,
    target_shape: object::ShapeHandle,
    store_owner: object::ShapeHandle,
    store_receiver: object::ShapeHandle,
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
        holder_shape: object::ShapeHandle::null(),
        holder_root: object::ShapeHandle::null(),
        target_shape: object::ShapeHandle::null(),
        store_owner: object::ShapeHandle::null(),
        store_receiver: object::ShapeHandle::null(),
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
    /// Proof owners of the rows that have any, by row index.
    owners: RefCell<rustc_hash::FxHashMap<u32, ActionOwners>>,
}

impl Default for PropertyActionCache {
    fn default() -> Self {
        // SAFETY: every field of a row is an integer, a compressed handle, a
        // `bool` or a `u8` enum whose zero is a variant, so all-zero bits are
        // a valid row: the empty one, whose unassigned shape id no receiver
        // has. The zeroed allocation commits pages only as rows are written.
        let entries = unsafe { Box::new_zeroed_slice(SETS * WAYS).assume_init() };
        Self {
            entries,
            owners: RefCell::new(rustc_hash::FxHashMap::default()),
        }
    }
}

impl PropertyActionCache {
    /// Take row `index`'s proof owners, leaving it none.
    fn take_owners(&self, index: usize) -> ActionOwners {
        self.owners
            .borrow_mut()
            .remove(&(index as u32))
            .unwrap_or_default()
    }

    /// Give row `index` exactly `owners`.
    fn put_owners(&self, index: usize, owners: ActionOwners) {
        let mut table = self.owners.borrow_mut();
        if owners.load.is_none() && owners.store.is_none() {
            table.remove(&(index as u32));
        } else {
            table.insert(index as u32, owners);
        }
    }

    /// Change row `index`'s proof owners in place.
    fn update_owners(&self, index: usize, change: impl FnOnce(&mut ActionOwners)) {
        let mut owners = self.take_owners(index);
        change(&mut owners);
        self.put_owners(index, owners);
    }

    /// Row `index`'s load proof.
    fn load_owner(&self, index: usize) -> Option<Arc<PrototypeValidity>> {
        self.owners
            .borrow()
            .get(&(index as u32))
            .and_then(|owners| owners.load.clone())
    }

    /// Row `index`'s store transition kind.
    fn store_owner(&self, index: usize) -> Option<object::StorePropertyTransitionKind> {
        self.owners
            .borrow()
            .get(&(index as u32))
            .and_then(|owners| owners.store.clone())
    }

    fn set_start(shape: ShapeId, atom: AtomId) -> usize {
        let key = shape.raw() ^ (u64::from(atom.raw()) << HASH_ATOM_SHIFT);
        ((key.wrapping_mul(HASH_MULTIPLIER) >> HASH_SHIFT) as usize) * WAYS
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
        self.take_owners(vacate);
        for index in (start + 1..=vacate).rev() {
            let owners = self.take_owners(index - 1);
            self.put_owners(index, owners);
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
        let validity = self.load_owner(index);
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
                let Some(owner) = object::field_type::observer_owner(
                    resolved.hit.shape,
                    u32::from(resolved.hit.slot),
                ) else {
                    entry.store_action = PropertyStoreAction::Unknown;
                    entry.store_owner = object::ShapeHandle::null();
                    entry.store_receiver = object::ShapeHandle::null();
                    self.update_owners(index, |owners| {
                        owners.store = None;
                        owners.load = validity;
                    });
                    self.entries[index].set(entry);
                    return;
                };
                entry.store_action = PropertyStoreAction::OwnWritable;
                entry.store_slot = resolved.hit.slot;

                entry.target_shape = object::ShapeHandle::null();
                entry.target_shape_id = ShapeId::UNASSIGNED;
                entry.store_validity = 0;
                entry.store_owner = owner;
                entry.store_receiver = resolved.hit.shape;
                self.update_owners(index, |owners| owners.store = None);
            }
        } else if nonexistent {
            entry.load_action = PropertyLoadAction::NonExistent;
        } else {
            entry.load_action = PropertyLoadAction::Unresolvable;
        }
        self.update_owners(index, |owners| owners.load = validity);
        self.entries[index].set(entry);
    }

    /// Publish the existing own-data hit of a committed writable store program.
    /// The program's constructor already proved the data descriptor writable;
    /// immutable shape identity makes its load and overwrite facts compatible.
    pub(crate) fn record_own_store(&self, hit: AtomOwnPropertyHit) {
        if hit.atom_id == AtomId::NONE {
            return;
        }
        let Some(owner) = object::field_type::observer_owner(hit.shape, u32::from(hit.slot)) else {
            return;
        };
        let index = self.record_way(hit.shape_id, hit.atom_id);
        self.take_owners(index);
        let mut entry = PropertyActionEntry {
            receiver_shape_id: hit.shape_id,
            atom: hit.atom_id,
            load_action: PropertyLoadAction::OwnData,
            store_action: PropertyStoreAction::OwnWritable,
            load_slot: hit.slot,
            store_slot: hit.slot,
            load_writable: true,
            holder_shape: hit.shape,
            holder_shape_id: hit.shape_id,
            store_owner: owner,
            store_receiver: hit.shape,
            ..PropertyActionEntry::EMPTY
        };
        // The receiver's shape fixes its state: a shape whose stores the
        // runtime owns keeps the load fact alone.
        if !stores_in_place(object::shape_body::state_of(hit.shape)) {
            entry.store_action = PropertyStoreAction::Unknown;
            entry.store_slot = 0;
            entry.store_owner = object::ShapeHandle::null();
            entry.store_receiver = object::ShapeHandle::null();
        }

        self.entries[index].set(entry);
    }

    /// Merge a committed add sample into its original key, retaining any load.
    pub(crate) fn record_store(&self, transition: object::StorePropertyTransition) {
        if transition.atom_id == AtomId::NONE {
            return;
        }
        let target = transition.to_shape.get();
        if !target.is_null() {
            let parent = object::shape_body::parent_of(target);
            if parent.is_null() || object::shape_body::id_of(parent) != transition.from_shape_id {
                return;
            }
            // The source shape fixes its state: only an extensible shape
            // whose stores stay in place appends through the table.
            let state = object::shape_body::state_of(parent);
            if !stores_in_place(state) || !state.is_extensible() {
                return;
            }
        }
        let owner = if transition.to_shape.get().is_null() {
            object::ShapeHandle::null()
        } else {
            let Some(owner) = object::field_type::observer_owner(
                transition.to_shape.get(),
                u32::from(transition.slot),
            ) else {
                return;
            };
            owner
        };
        let index = self.record_way(transition.from_shape_id, transition.atom_id);
        let mut entry = self.entries[index].get();
        entry.store_action = PropertyStoreAction::AddOwn;
        entry.store_slot = transition.slot;

        entry.target_shape = transition.to_shape.get();
        entry.target_shape_id = transition.to_shape_id;
        entry.store_owner = owner;
        entry.store_receiver = object::ShapeHandle::null();
        entry.store_validity = match &transition.kind {
            object::StorePropertyTransitionKind::OwnAdd => 0,
            object::StorePropertyTransitionKind::PrototypeChainMissing { validity }
            | object::StorePropertyTransitionKind::PrototypeWritableData { validity } => {
                validity.address()
            }
        };
        self.update_owners(index, |owners| owners.store = Some(transition.kind));
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
            PropertyStoreAction::OwnWritable => {
                object::store_proven_data_slot(obj, heap, entry.store_slot, *value);
                Ok(Some(PropertyStoreAction::OwnWritable))
            }
            PropertyStoreAction::AddOwn => {
                // No proof borrow crosses the allocating replay/weak sweep.
                let Some(kind) = self.store_owner(index) else {
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
            if dead(entry.store_receiver) || dead(entry.store_owner) {
                way.set(PropertyActionEntry::EMPTY);
                self.take_owners(index);
                continue;
            }
            if dead(entry.holder_shape) || dead(entry.holder_root) {
                entry.clear_load();
                if entry.store_action == PropertyStoreAction::Unknown {
                    entry = PropertyActionEntry::EMPTY;
                }
                way.set(entry);
                self.update_owners(index, |owners| owners.load = None);
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

    /// Complete a named load through one source-preserving storage projection.
    /// An array's live expando admits only existing own data fields; its shape
    /// does not authorize inherited or missing keys on the original array.
    /// Clean lookup-start shapes fix the receiver's whole ordinary lookup:
    /// - an array without rare state or an expando starts non-index names other
    ///   than `length` at the retained default `%Array.prototype%`;
    /// - a Map, Set, WeakMap, WeakSet, DataView, ArrayBuffer, TypedArray,
    ///   Promise or RegExp keeps user-defined own properties in a lazy
    ///   expando bag; without one it owns no named key but the ones its kind
    ///   defines (a TypedArray's numeric keys, a RegExp's `lastIndex`), so
    ///   every other name starts at its `[[Prototype]]`;
    /// - a closure with an own-property bag, or a native function without a
    ///   `[[Prototype]]` override, in the default realm, starts at that bag,
    ///   whose prototype mirrors the function's, for every name it does not synthesize
    ///   (`name` and `length` of a native; a closure's `prototype`, and its
    ///   `name`, `length`, `caller` and `arguments` unless the bag owns them);
    /// - a class constructor in the default realm starts at its statics, which
    ///   carry its `[[Prototype]]` chain, for every name but its virtual
    ///   `prototype`, and `name` and `length` while no static shadows them.
    ///
    /// A native's bag starts as a prototype-less dictionary. Its first miss
    /// gives it the function's prototype and a hidden class, which allocates:
    /// a caller that continues after `None` re-reads its receiver from a
    /// root.
    pub(crate) fn projected_load(
        &mut self,
        slot: crate::feedback::PropertyFeedbackSlot<'_>,
        receiver: crate::Value,
        key: AtomizedPropertyKey<'_>,
    ) -> Option<crate::property_ic::LoadHit> {
        use crate::property_ic::{LoadHit, PropertyStorageProjection as Projection};
        let (start, projection) = if let Some(array) = receiver.as_array()
            && let Some(bag) = crate::array::expando_object(array, &self.gc_heap)
        {
            (bag, Projection::ArrayExpando)
        } else if let Some(view) = receiver.as_typed_array(&self.gc_heap) {
            let start = self.lookup_start_object(receiver)?;
            // A view on its realm-default prototype records it as its start,
            // which generated code then reads.
            if self.active_realm_id == 0 && view.lookup_start(&self.gc_heap).is_none() {
                view.prove_default_lookup_start(&mut self.gc_heap, start);
            }
            (start, Projection::TypedArrayStart)
        } else {
            (self.lookup_start_object(receiver)?, Projection::LookupStart)
        };
        if let Some(hit) = slot
            .native()
            .probe_projected_load(start, projection, &self.gc_heap)
        {
            slot.record_hit();
            return Some(hit);
        }
        slot.record_miss();
        let start = if projection == Projection::ArrayExpando {
            start
        } else {
            self.admit_lookup_start(receiver, start, key)?
        };
        let load = self.resolve_property_load(start, key);
        let shape = object::keyed_shape(start, &self.gc_heap);
        if projection == Projection::ArrayExpando
            && !matches!(&load, PropertyLoad::Data(resolved) if resolved.hops == 0)
        {
            return None;
        }
        let (handler, value) = match load {
            PropertyLoad::Data(resolved) => (
                crate::property_ic::IcHandler::load_resolved(shape, &resolved),
                resolved.value,
            ),
            PropertyLoad::Absent(proof) => (
                crate::property_ic::IcHandler::load_nonexistent(shape, proof),
                crate::Value::undefined(),
            ),
            PropertyLoad::Other => {
                return self
                    .cache_inherited_getter(slot, start, key, projection)
                    .map(LoadHit::Getter);
            }
        };
        if !slot.is_megamorphic()
            && let Some(handler) = handler
            && let Some(handler) = handler.projected(projection)
        {
            slot.install(handler);
        }
        Some(LoadHit::Value(value))
    }

    /// The getter of an accessor the lookup-start object `start` or one of
    /// its prototypes owns, with the site's handler for it installed: the
    /// next load calls the getter without walking the chain. `None` when the
    /// key is not such an accessor or it has no getter.
    pub(crate) fn cache_inherited_getter(
        &self,
        slot: crate::feedback::PropertyFeedbackSlot<'_>,
        start: JsObject,
        key: AtomizedPropertyKey<'_>,
        projection: crate::property_ic::PropertyStorageProjection,
    ) -> Option<crate::Value> {
        let own = object::lookup_own_atom(start, &self.gc_heap, key);
        if let (Some(hit), object::PropertyLookup::Accessor { getter, .. }) = (own.hit, own.lookup)
        {
            let getter = getter?;
            if object::supports_fast_property_ic(start, &self.gc_heap) {
                self.shape_runtime
                    .register_shape(&self.gc_heap, object::shape(start, &self.gc_heap));
                if !slot.is_megamorphic()
                    && let Some(handler) = crate::property_ic::IcHandler::load_own_accessor(
                        object::keyed_shape(start, &self.gc_heap),
                        &hit,
                    )
                    && let Some(handler) = handler.projected(projection)
                {
                    slot.install(handler);
                }
            }
            return Some(getter);
        }
        let resolved = cache_ir::resolve_atom_inherited_accessor(start, &self.gc_heap, key)?;
        let getter = resolved.getter?;
        self.shape_runtime
            .register_shape(&self.gc_heap, object::shape(start, &self.gc_heap));
        self.shape_runtime
            .register_shape(&self.gc_heap, resolved.holder_root);
        if !slot.is_megamorphic()
            && let Some(handler) = crate::property_ic::IcHandler::load_inherited_accessor(
                object::keyed_shape(start, &self.gc_heap),
                &resolved,
            )
            && let Some(handler) = handler.projected(projection)
        {
            slot.install(handler);
        }
        Some(getter)
    }

    /// `[[Set]]` of `key` on an array, closure or native function whose property
    /// bag owns it as writable data: the store lands in the bag, as the site's
    /// projected store handler directs or, on a miss, as one installed
    /// after resolving the bag's own slot. `false` leaves the full `[[Set]]`
    /// to the caller. Neither allocates nor runs code.
    ///
    /// A key the function may synthesize ahead of its bag never qualifies
    /// (`name` and `length`, and a closure's `prototype`, `caller` and
    /// `arguments`). An own writable data property ends OrdinarySet at
    /// the bag whatever the function's prototype, so the bag need not mirror
    /// it, and a native's bag that a load has not yet prepared simply misses.
    pub(crate) fn projected_store(
        &mut self,
        slot: crate::feedback::PropertyFeedbackSlot<'_>,
        receiver: crate::Value,
        key: AtomizedPropertyKey<'_>,
        value: crate::Value,
    ) -> bool {
        use crate::property_ic::PropertyStorageProjection as Projection;
        let (bag, projection) = if let Some(array) = receiver.as_array() {
            let Some(bag) = crate::array::expando_object(array, &self.gc_heap) else {
                return false;
            };
            (bag, Projection::ArrayExpando)
        } else {
            let Some(bag) = self.function_bag(receiver) else {
                return false;
            };
            (bag, Projection::LookupStart)
        };
        if slot
            .native()
            .probe_projected_store(bag, projection, &mut self.gc_heap, value)
        {
            slot.record_hit();
            return true;
        }
        slot.record_miss();
        let heap = &self.gc_heap;
        let native = receiver.as_native_function().is_some();
        let synthesized = match key.name() {
            "name" | "length" => true,
            "prototype" | "caller" | "arguments" => !native,
            _ => false,
        };
        if projection == Projection::LookupStart && synthesized {
            return false;
        }
        let Some((handler, hit)) =
            crate::property_ic::IcHandler::store_existing_hit(bag, heap, key)
        else {
            return false;
        };
        object::store_proven_data_slot(bag, &mut self.gc_heap, hit.slot, value);
        if !slot.is_megamorphic()
            && let Some(handler) = handler.projected(projection)
        {
            slot.install(handler);
        }
        true
    }

    /// The `[[Prototype]]` a closure's own-property bag takes for the
    /// function's `[[Prototype]]` `prototype`. A closure with a bag or a class
    /// stands for itself by its bag or statics, so a re-parented constructor's
    /// lookup is one chain of ordinary objects the property caches walk; the
    /// keys a function synthesizes (`name`, `length`, `prototype`, `caller`,
    /// `arguments`) never resolve along it. Any other value is itself.
    pub(crate) fn function_lookup_parent(&self, prototype: crate::Value) -> crate::Value {
        if let Some(closure) = prototype.as_closure(&self.gc_heap) {
            return closure
                .own_props(&self.gc_heap)
                .map_or(prototype, crate::Value::object);
        }
        if let Some(class) = prototype.as_class_constructor() {
            return crate::Value::object(class.statics(&self.gc_heap));
        }
        prototype
    }

    /// The property bag a closure, native function or class constructor (its
    /// statics) keeps its own named properties in, when present and in the
    /// default realm (for a native, with no `[[Prototype]]` override).
    fn function_bag(&self, receiver: crate::Value) -> Option<JsObject> {
        if self.active_realm_id != 0 {
            return None;
        }
        if let Some(native) = receiver.as_native_function() {
            return (!native.has_prototype_override(&self.gc_heap))
                .then(|| native.own_properties_bag(&self.gc_heap))
                .filter(|bag| !bag.is_null());
        }
        if let Some(class) = receiver.as_class_constructor() {
            return Some(class.statics(&self.gc_heap));
        }
        // A closure's bag mirrors its [[Prototype]], override or not.
        receiver.as_closure(&self.gc_heap)?.own_props(&self.gc_heap)
    }

    /// The object a named lookup on `receiver` starts at, whatever the key,
    /// or `None` when the receiver's own lookup is not a bag or array.
    fn lookup_start_object(&self, receiver: crate::Value) -> Option<JsObject> {
        if let Some(array) = receiver.as_array() {
            return if crate::array::is_ordinary_dense(array, &self.gc_heap) {
                self.default_array_prototype()
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
        if let Some(class) = receiver.as_class_constructor() {
            return Some(class.statics(&self.gc_heap));
        }
        let closure = receiver.as_closure(&self.gc_heap)?;
        // A closure's bag mirrors its [[Prototype]], override or not.
        if let Some(bag) = closure.own_props(&self.gc_heap) {
            return Some(bag);
        }
        // A closure without an own-property bag owns only its virtual
        // `name`, `length` and `prototype`, which admission refuses: every
        // other key starts at the `%Function.prototype%` an ordinary kind
        // inherits from.
        (closure.named_lookup(&self.gc_heap) == crate::closure::CLOSURE_LOOKUP_ORDINARY)
            .then(|| self.function_prototype_object().ok())
            .flatten()
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
        } else {
            let regexp = receiver.as_regexp()?;
            (
                regexp.expando(heap),
                regexp.prototype_override(heap),
                Intrinsic::RegExpPrototype,
            )
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
        if receiver.is_class_constructor() {
            // A class's statics carry its [[Prototype]] chain; only its
            // virtual `prototype`, and `name` and `length` while no static
            // shadows them, live outside.
            let admitted = match name {
                "prototype" => false,
                "name" | "length" => object::lookup_own_atom(start, &self.gc_heap, key)
                    .hit
                    .is_some(),
                _ => true,
            };
            if !admitted {
                return None;
            }
            // Re-seating the statics on their [[Prototype]] after the class
            // body defined members leaves them a dictionary; the first miss
            // gives them a hidden class, as a native's bag gets one.
            if object::state(start, &self.gc_heap).is_dictionary() {
                self.migrate_slow_to_fast(&mut start);
            }
            return Some(start);
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
        let prototype = if native {
            prototype
        } else {
            self.function_lookup_parent(prototype)
        };
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
        // A native's first miss, or a closure's bag re-seated on a new
        // [[Prototype]] with properties in it, leaves a dictionary.
        if object::state(start, &self.gc_heap).is_dictionary() {
            self.migrate_slow_to_fast(&mut start);
        }
        Some(start)
    }

    /// Cache update for an ordinary receiver that missed its site: install
    /// the handler of the resolved data slot, or of a key the receiver and
    /// its whole chain lack. Neither allocates nor runs code.
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
