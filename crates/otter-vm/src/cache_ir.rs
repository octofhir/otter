//! One inline-cache representation shared by every property opcode.
//!
//! A cache stub is a short linear program — a sequence of [`CacheOp`] guards
//! and loads over a small operand file, plus per-stub "data" (shape ids and
//! resolved own-property hits) the ops reference by index rather than bake in.
//! One [`CacheStub`] shape serves `LoadProperty`, `HasProperty`, and
//! `StoreProperty`: the guard ops are identical and only the terminal op (and
//! the executor entry point) differ. This is the contract a later optimizing
//! JIT lowers, so the interpreter and the JIT never describe the same cache
//! twice (the divergence class that produced the compiled-tier crash).
//!
//! # Contents
//!
//! - [`CacheOp`] — the guard/load opcodes.
//! - [`CacheStub`] — an op sequence plus its referenced shape ids and hits.
//! - executor entry points: [`CacheStub::run_load`], [`CacheStub::run_has`],
//!   [`CacheStub::run_store`].
//!
//! # Invariants
//!
//! - Operand `0` is always the receiver; `1` is the receiver's prototype once a
//!   [`CacheOp::LoadPrototype`] has run. No op reads an operand before it is
//!   defined (guaranteed by the builders).
//! - Shapes referenced by stub data are interned and immortal (rooted by the
//!   transition tables, pinned in non-moving old space), so the stored
//!   [`crate::object::ShapeId`]/hit metadata never dangles.

use smallvec::SmallVec;

use otter_gc::raw::SlotVisitor;

use crate::object::{
    self, AtomOwnPropertyHit, OwnPropertySlotHit, ShapeHandle, ShapeId, StorePropertyTransition,
};
use crate::property_atom::AtomizedPropertyKey;
use crate::{JsObject, JsString, Value};

/// Version stamp for the [`CacheStub`] / [`CacheOp`] feedback ABI.
///
/// The optimizing tier transpiles a [`CacheStubSnapshot`] taken at compile
/// time and records the version it compiled against, so a stub whose op
/// semantics or table encoding later change (a bumped version) is recognized as
/// incompatible rather than mis-transpiled. Bump whenever a [`CacheOp`]
/// variant's meaning, operand assignment, or table indexing changes.
pub(crate) const CACHE_STUB_ABI_VERSION: u32 = 1;

/// Operand slot in a stub's tiny register file. `0` is the receiver; `1` is the
/// receiver's prototype after [`CacheOp::LoadPrototype`].
type OperandId = u8;

/// A single guard or load in a cache stub. Data-bearing ops carry an index into
/// the owning [`CacheStub`]'s `shape_ids` / `hits` tables.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CacheOp {
    /// Guard that `operands[obj]`'s hidden-class id equals `shape_ids[shape]`.
    GuardShapeId {
        /// Operand whose shape is guarded.
        obj: OperandId,
        /// Index into the stub's `shape_ids`.
        shape: u8,
    },
    /// Load `operands[obj]`'s `[[Prototype]]` into `operands[dst]`. Fails the
    /// stub when there is no prototype or it is not fast-IC compatible.
    LoadPrototype {
        /// Operand to read the prototype of.
        obj: OperandId,
        /// Operand to write the prototype into.
        dst: OperandId,
    },
    /// Guard that the runtime key equals `keys[key]`. Used by `HasProperty`,
    /// whose `in` key is a dynamic operand rather than a bytecode atom.
    GuardKey {
        /// Index into the stub's `keys`.
        key: u8,
    },
    /// Terminal (load): produce the data value at `hits[hit]` on
    /// `operands[obj]`, validating the slot's shape/atom/key guard.
    LoadDataSlotResult {
        /// Operand holding the object that owns the slot.
        obj: OperandId,
        /// Index into the stub's `hits`.
        hit: u8,
    },
    /// Terminal (has): succeed when `slot_hits[hit]` is still present on
    /// `operands[obj]`.
    HasDataSlot {
        /// Operand holding the object that owns the slot.
        obj: OperandId,
        /// Index into the stub's `slot_hits`.
        hit: u8,
    },
    /// Terminal (store): write the rhs into the existing writable data slot at
    /// `hits[hit]` on `operands[obj]`.
    StoreDataSlot {
        /// Operand holding the object that owns the slot.
        obj: OperandId,
        /// Index into the stub's `hits`.
        hit: u8,
    },
    /// Terminal (store): add a data slot by replaying `transitions[transition]`.
    StoreAddTransition {
        /// Index into the stub's `transitions`.
        transition: u8,
    },
}

/// A cache stub: a linear op program plus the data its ops reference.
#[derive(Debug, Clone, Default)]
pub(crate) struct CacheStub {
    /// The guard/load program, run in order against operand `0` (the receiver).
    ops: SmallVec<[CacheOp; 4]>,
    /// Receiver / prototype shape ids guarded by [`CacheOp::GuardShapeId`].
    shape_ids: SmallVec<[ShapeId; 1]>,
    /// Receiver shapes for direct-prototype load stubs. The interpreter guards
    /// by [`ShapeId`], while the JIT needs the immortal shape handle's
    /// compressed offset for an inline shape compare.
    receiver_shapes: SmallVec<[ShapeHandle; 1]>,
    /// Atom-aware own-property hits consumed by load terminals.
    hits: SmallVec<[AtomOwnPropertyHit; 1]>,
    /// Slot hits consumed by `has` terminals.
    slot_hits: SmallVec<[OwnPropertySlotHit; 1]>,
    /// Property-name keys guarded by [`CacheOp::GuardKey`]. Interned/long-lived
    /// like every cached property name, so they are not separately traced
    /// (matching the load/has IC root set).
    keys: SmallVec<[JsString; 1]>,
    /// Hidden-class transitions replayed by [`CacheOp::StoreAddTransition`].
    /// Their target shapes are GC roots, visited by [`CacheStub::trace_roots`].
    transitions: SmallVec<[StorePropertyTransition; 1]>,
}

impl CacheStub {
    /// Own-data load: receiver owns the slot.
    #[must_use]
    pub(crate) fn load_own_data(hit: AtomOwnPropertyHit) -> Self {
        let mut ops = SmallVec::new();
        ops.push(CacheOp::LoadDataSlotResult { obj: 0, hit: 0 });
        Self {
            ops,
            hits: SmallVec::from_elem(hit, 1),
            ..Self::default()
        }
    }

    /// Direct-prototype data load: the receiver's prototype owns the slot. The
    /// receiver shape is guarded before the prototype hop so a transitioned
    /// receiver re-resolves.
    #[must_use]
    pub(crate) fn load_direct_prototype_data(
        receiver_shape_id: ShapeId,
        receiver_shape: ShapeHandle,
        hit: AtomOwnPropertyHit,
    ) -> Self {
        let mut ops = SmallVec::new();
        ops.push(CacheOp::GuardShapeId { obj: 0, shape: 0 });
        ops.push(CacheOp::LoadPrototype { obj: 0, dst: 1 });
        ops.push(CacheOp::LoadDataSlotResult { obj: 1, hit: 0 });
        Self {
            ops,
            shape_ids: SmallVec::from_elem(receiver_shape_id, 1),
            receiver_shapes: SmallVec::from_elem(receiver_shape, 1),
            hits: SmallVec::from_elem(hit, 1),
            ..Self::default()
        }
    }

    /// Resolve operand `idx`, where `0` is the receiver and `1` is the
    /// prototype once loaded.
    #[inline]
    fn operand(operands: &[Option<JsObject>; 2], idx: OperandId) -> Option<JsObject> {
        operands.get(idx as usize).copied().flatten()
    }

    /// Run the shared guard prefix, resolving operands; returns the operand file
    /// on success or `None` on any guard miss. `has_key` is the runtime key for
    /// [`CacheOp::GuardKey`] (only present on `HasProperty` stubs).
    #[inline]
    fn run_guards(
        &self,
        recv: JsObject,
        heap: &otter_gc::GcHeap,
        has_key: Option<JsString>,
    ) -> Option<[Option<JsObject>; 2]> {
        let mut operands: [Option<JsObject>; 2] = [Some(recv), None];
        for op in &self.ops {
            match *op {
                CacheOp::GuardShapeId { obj, shape } => {
                    let obj = Self::operand(&operands, obj)?;
                    if object::shape_id(obj, heap) != self.shape_ids[shape as usize] {
                        return None;
                    }
                }
                CacheOp::GuardKey { key } => {
                    if !self.keys[key as usize].equals(has_key?, heap) {
                        return None;
                    }
                }
                CacheOp::LoadPrototype { obj, dst } => {
                    let obj = Self::operand(&operands, obj)?;
                    let proto = object::prototype(obj, heap)?;
                    if !object::supports_fast_property_ic(proto, heap) {
                        return None;
                    }
                    operands[dst as usize] = Some(proto);
                }
                // Terminals are handled by the per-kind executors below.
                CacheOp::LoadDataSlotResult { .. }
                | CacheOp::HasDataSlot { .. }
                | CacheOp::StoreDataSlot { .. }
                | CacheOp::StoreAddTransition { .. } => break,
            }
        }
        Some(operands)
    }

    /// Execute as a `LoadProperty` cache. `None` on a miss.
    #[must_use]
    pub(crate) fn run_load(
        &self,
        recv: JsObject,
        heap: &otter_gc::GcHeap,
        key: AtomizedPropertyKey<'_>,
    ) -> Option<Value> {
        // Fast path for the common monomorphic own-data stub: the receiver owns
        // the slot, so skip the operand file and run the single load terminal
        // directly (its own shape/atom guard validates the hit).
        if let [CacheOp::LoadDataSlotResult { obj: 0, hit: 0 }] = self.ops.as_slice() {
            return object::load_own_data_slot_atom(recv, heap, key, self.hits[0]);
        }
        let operands = self.run_guards(recv, heap, None)?;
        let CacheOp::LoadDataSlotResult { obj, hit } = self.ops.last().copied()? else {
            return None;
        };
        let obj = Self::operand(&operands, obj)?;
        object::load_own_data_slot_atom(obj, heap, key, self.hits[hit as usize])
    }

    /// Own-data presence: receiver owns the slot.
    #[must_use]
    pub(crate) fn has_own_data(key: JsString, hit: OwnPropertySlotHit) -> Self {
        let mut ops = SmallVec::new();
        ops.push(CacheOp::GuardKey { key: 0 });
        ops.push(CacheOp::HasDataSlot { obj: 0, hit: 0 });
        Self {
            ops,
            slot_hits: SmallVec::from_elem(hit, 1),
            keys: SmallVec::from_elem(key, 1),
            ..Self::default()
        }
    }

    /// Direct-prototype presence: the receiver's prototype owns the slot.
    #[must_use]
    pub(crate) fn has_direct_prototype_data(
        receiver_shape_id: ShapeId,
        key: JsString,
        hit: OwnPropertySlotHit,
    ) -> Self {
        let mut ops = SmallVec::new();
        ops.push(CacheOp::GuardKey { key: 0 });
        ops.push(CacheOp::GuardShapeId { obj: 0, shape: 0 });
        ops.push(CacheOp::LoadPrototype { obj: 0, dst: 1 });
        ops.push(CacheOp::HasDataSlot { obj: 1, hit: 0 });
        Self {
            ops,
            shape_ids: SmallVec::from_elem(receiver_shape_id, 1),
            slot_hits: SmallVec::from_elem(hit, 1),
            keys: SmallVec::from_elem(key, 1),
            ..Self::default()
        }
    }

    /// Execute as a `HasProperty` cache. `Some(())` on a confirmed hit.
    #[must_use]
    pub(crate) fn run_has(
        &self,
        recv: JsObject,
        heap: &otter_gc::GcHeap,
        key: JsString,
    ) -> Option<()> {
        let operands = self.run_guards(recv, heap, Some(key))?;
        let CacheOp::HasDataSlot { obj, hit } = self.ops.last().copied()? else {
            return None;
        };
        let obj = Self::operand(&operands, obj)?;
        object::has_own_slot(obj, heap, self.slot_hits[hit as usize]).then_some(())
    }

    /// Build a `HasProperty` stub for the current receiver/key pair. `None` when
    /// the access is not IC-eligible.
    #[must_use]
    pub(crate) fn install_has(
        obj: JsObject,
        heap: &otter_gc::GcHeap,
        key: JsString,
    ) -> Option<Self> {
        if !object::supports_fast_property_ic(obj, heap) {
            return None;
        }
        let key_name = key.to_lossy_string(heap);
        let receiver_shape_id = object::shape_id(obj, heap);
        let (own_hit, own_lookup) = object::lookup_own_slot(obj, heap, &key_name);
        if let (Some(hit), object::PropertyLookup::Data { .. }) = (own_hit, own_lookup) {
            return Some(Self::has_own_data(key, hit));
        }
        let proto = object::prototype(obj, heap)?;
        if !object::supports_fast_property_ic(proto, heap) {
            return None;
        }
        let (proto_hit, proto_lookup) = object::lookup_own_slot(proto, heap, &key_name);
        if let (Some(hit), object::PropertyLookup::Data { .. }) = (proto_hit, proto_lookup) {
            return Some(Self::has_direct_prototype_data(receiver_shape_id, key, hit));
        }
        None
    }

    /// Build a `LoadProperty` stub for the current receiver/key pair, returning
    /// the stub and the value it would have loaded. `None` when the access is
    /// not IC-eligible.
    #[must_use]
    pub(crate) fn install_load(
        obj: JsObject,
        heap: &otter_gc::GcHeap,
        key: AtomizedPropertyKey<'_>,
    ) -> Option<(Self, Value)> {
        if !object::supports_fast_property_ic(obj, heap) {
            return None;
        }
        let receiver_shape_id = object::shape_id(obj, heap);
        let receiver_shape = object::shape(obj, heap);
        let atom_lookup = object::lookup_own_atom(obj, heap, key);
        if let (Some(hit), object::PropertyLookup::Data { value, .. }) =
            (atom_lookup.hit, atom_lookup.lookup)
        {
            return Some((Self::load_own_data(hit), value));
        }
        if atom_lookup.hit.is_some() {
            return None;
        }
        let proto = object::prototype(obj, heap)?;
        if !object::supports_fast_property_ic(proto, heap) {
            return None;
        }
        let proto_lookup = object::lookup_own_atom(proto, heap, key);
        if let (Some(hit), object::PropertyLookup::Data { value, .. }) =
            (proto_lookup.hit, proto_lookup.lookup)
        {
            return Some((
                Self::load_direct_prototype_data(receiver_shape_id, receiver_shape, hit),
                value,
            ));
        }
        None
    }

    /// The own-data hit when this is a single-op own-data load stub. Lets the
    /// compiled-call plan and devtools read the resolved slot without
    /// re-deriving it.
    #[must_use]
    pub(crate) fn own_data_hit(&self) -> Option<AtomOwnPropertyHit> {
        match (self.ops.as_slice(), self.hits.as_slice()) {
            ([CacheOp::LoadDataSlotResult { obj: 0, hit: 0 }], [hit]) => Some(*hit),
            _ => None,
        }
    }

    /// The `(receiver shape id, prototype hit)` of a direct-prototype data load
    /// stub, for devtools rendering.
    #[must_use]
    pub(crate) fn direct_prototype_load(&self) -> Option<(ShapeId, AtomOwnPropertyHit)> {
        match (
            self.ops.as_slice(),
            self.shape_ids.as_slice(),
            self.hits.as_slice(),
        ) {
            (
                [
                    CacheOp::GuardShapeId { obj: 0, shape: 0 },
                    CacheOp::LoadPrototype { obj: 0, dst: 1 },
                    CacheOp::LoadDataSlotResult { obj: 1, hit: 0 },
                ],
                [shape_id],
                [hit],
            ) => Some((*shape_id, *hit)),
            _ => None,
        }
    }

    /// The `(receiver shape, prototype hit)` of a direct-prototype data load
    /// stub, for JIT lowering.
    #[must_use]
    pub(crate) fn direct_prototype_load_jit(&self) -> Option<(ShapeHandle, AtomOwnPropertyHit)> {
        match (
            self.ops.as_slice(),
            self.receiver_shapes.as_slice(),
            self.hits.as_slice(),
        ) {
            (
                [
                    CacheOp::GuardShapeId { obj: 0, shape: 0 },
                    CacheOp::LoadPrototype { obj: 0, dst: 1 },
                    CacheOp::LoadDataSlotResult { obj: 1, hit: 0 },
                ],
                [shape],
                [hit],
            ) => Some((*shape, *hit)),
            _ => None,
        }
    }

    /// The own-data slot hit of an own-data presence stub, for devtools.
    #[must_use]
    pub(crate) fn has_own_slot_hit(&self) -> Option<OwnPropertySlotHit> {
        match (self.ops.as_slice(), self.slot_hits.as_slice()) {
            (
                [
                    CacheOp::GuardKey { key: 0 },
                    CacheOp::HasDataSlot { obj: 0, hit: 0 },
                ],
                [hit],
            ) => Some(*hit),
            _ => None,
        }
    }

    /// The `(receiver shape id, prototype slot hit)` of a direct-prototype
    /// presence stub, for devtools.
    #[must_use]
    pub(crate) fn has_direct_prototype(&self) -> Option<(ShapeId, OwnPropertySlotHit)> {
        match (
            self.ops.as_slice(),
            self.shape_ids.as_slice(),
            self.slot_hits.as_slice(),
        ) {
            (
                [
                    CacheOp::GuardKey { key: 0 },
                    CacheOp::GuardShapeId { obj: 0, shape: 0 },
                    CacheOp::LoadPrototype { obj: 0, dst: 1 },
                    CacheOp::HasDataSlot { obj: 1, hit: 0 },
                ],
                [shape_id],
                [hit],
            ) => Some((*shape_id, *hit)),
            _ => None,
        }
    }

    /// Existing-own-data store: receiver owns a writable data slot.
    #[must_use]
    pub(crate) fn store_own_data(hit: AtomOwnPropertyHit) -> Self {
        let mut ops = SmallVec::new();
        ops.push(CacheOp::StoreDataSlot { obj: 0, hit: 0 });
        Self {
            ops,
            hits: SmallVec::from_elem(hit, 1),
            ..Self::default()
        }
    }

    /// Add-a-slot store: replay a captured hidden-class transition.
    #[must_use]
    pub(crate) fn store_transition(transition: StorePropertyTransition) -> Self {
        let mut ops = SmallVec::new();
        ops.push(CacheOp::StoreAddTransition { transition: 0 });
        Self {
            ops,
            transitions: SmallVec::from_elem(transition, 1),
            ..Self::default()
        }
    }

    /// Execute as a `StoreProperty` cache. `Some(())` once the write completes.
    pub(crate) fn run_store(
        &self,
        recv: JsObject,
        heap: &mut otter_gc::GcHeap,
        key: AtomizedPropertyKey<'_>,
        value: &Value,
    ) -> Option<()> {
        if !object::supports_fast_property_ic(recv, heap) {
            return None;
        }
        match self.ops.last().copied()? {
            CacheOp::StoreDataSlot { obj: 0, hit } => {
                object::store_own_data_slot_atom(recv, heap, key, self.hits[hit as usize], value)
            }
            CacheOp::StoreAddTransition { transition } => object::replay_store_property_transition(
                recv,
                heap,
                key,
                &self.transitions[transition as usize],
                value,
            ),
            _ => None,
        }
    }

    /// Build an existing-own-data store stub for the current receiver/key pair.
    /// Add-transition stubs are captured by the shape-transition layer (which
    /// performs the write while recording replay metadata) and installed via
    /// [`Self::store_transition`].
    #[must_use]
    pub(crate) fn install_store_existing(
        obj: JsObject,
        heap: &otter_gc::GcHeap,
        key: AtomizedPropertyKey<'_>,
    ) -> Option<Self> {
        if !object::supports_fast_property_ic(obj, heap) {
            return None;
        }
        let atom_lookup = object::lookup_own_atom(obj, heap, key);
        let (Some(hit), object::PropertyLookup::Data { flags, .. }) =
            (atom_lookup.hit, atom_lookup.lookup)
        else {
            return None;
        };
        flags.writable().then(|| Self::store_own_data(hit))
    }

    /// The existing-own-data store hit, for the compiled-call plan.
    #[must_use]
    pub(crate) fn store_own_data_hit(&self) -> Option<AtomOwnPropertyHit> {
        match (self.ops.as_slice(), self.hits.as_slice()) {
            ([CacheOp::StoreDataSlot { obj: 0, hit: 0 }], [hit]) => Some(*hit),
            _ => None,
        }
    }

    /// The replayed transition of an add-transition store stub, for devtools.
    #[must_use]
    pub(crate) fn store_transition_ref(&self) -> Option<&StorePropertyTransition> {
        match (self.ops.as_slice(), self.transitions.as_slice()) {
            ([CacheOp::StoreAddTransition { transition: 0 }], [t]) => Some(t),
            _ => None,
        }
    }

    /// Visit GC roots in stub data — the target shapes of replayed transitions.
    pub(crate) fn trace_roots(&self, visitor: &mut SlotVisitor<'_>) {
        for transition in &self.transitions {
            transition.trace_roots(visitor);
        }
    }

    /// Take an immutable copy-on-compile snapshot of this stub. Captures the
    /// stub's program and tables at this instant so the optimizing tier reads a
    /// stable view for the duration of a compile, decoupled from later
    /// interpreter updates to the live site.
    #[allow(dead_code)] // consumed by the optimizing tier when it re-enables.
    #[must_use]
    pub(crate) fn snapshot(&self) -> CacheStubSnapshot {
        CacheStubSnapshot {
            stub: self.clone(),
            version: CACHE_STUB_ABI_VERSION,
        }
    }
}

/// Immutable copy-on-compile view of a [`CacheStub`].
///
/// The optimizing tier must read a site's cache from a stable snapshot for the
/// duration of a compile, never the live mutable stub the interpreter keeps
/// updating. A snapshot owns an independent clone of the stub's program and
/// tables taken at one instant, so a later interpreter update to the site never
/// shifts the shape ids or slot offsets the compile baked. The captured
/// [`CACHE_STUB_ABI_VERSION`] lets a transpiler reject a stub whose ABI changed
/// under it.
///
/// GC-safe with no tracing required: every shape the tables reference is
/// interned, immortal, and pinned in non-moving old space, so a snapshot's
/// captured shape ids and transition targets neither dangle nor relocate while
/// it is held. [`Self::trace_roots`] is offered for callers that root it anyway.
#[allow(dead_code)] // the read surface the optimizing tier lowers from on re-enable.
#[derive(Debug, Clone)]
pub(crate) struct CacheStubSnapshot {
    stub: CacheStub,
    version: u32,
}

#[allow(dead_code)] // forward ABI: every accessor is a compile-time reader for the JIT.
impl CacheStubSnapshot {
    /// ABI version this snapshot was taken under.
    #[must_use]
    pub(crate) fn version(&self) -> u32 {
        self.version
    }

    /// Own-data load hit, if this site is a monomorphic own-data load.
    #[must_use]
    pub(crate) fn own_data_hit(&self) -> Option<AtomOwnPropertyHit> {
        self.stub.own_data_hit()
    }

    /// Direct-prototype data load: the guarded prototype shape and the hit.
    #[must_use]
    pub(crate) fn direct_prototype_load(&self) -> Option<(ShapeId, AtomOwnPropertyHit)> {
        self.stub.direct_prototype_load()
    }

    /// Direct-prototype data load as JIT-readable shape handles.
    #[must_use]
    pub(crate) fn direct_prototype_load_jit(&self) -> Option<(ShapeHandle, AtomOwnPropertyHit)> {
        self.stub.direct_prototype_load_jit()
    }

    /// Existing-own-data store hit.
    #[must_use]
    pub(crate) fn store_own_data_hit(&self) -> Option<AtomOwnPropertyHit> {
        self.stub.store_own_data_hit()
    }

    /// Replayed add-transition of an add-a-slot store stub.
    #[must_use]
    pub(crate) fn store_transition_ref(&self) -> Option<&StorePropertyTransition> {
        self.stub.store_transition_ref()
    }

    /// Visit GC roots — the target shapes of replayed transitions. Not required
    /// (referenced shapes are immortal and pinned) but offered for callers that
    /// choose to root the snapshot.
    pub(crate) fn trace_roots(&self, visitor: &mut SlotVisitor<'_>) {
        self.stub.trace_roots(visitor);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_is_independent_of_later_site_updates() {
        // A monomorphic own-data store stub at slot 7. The hit's exact
        // contents are irrelevant here; the snapshot semantics are.
        let hit = AtomOwnPropertyHit {
            shape_id: ShapeId::UNASSIGNED,
            shape: object::ShapeHandle::null(),
            atom_id: crate::property_atom::AtomId::from_constant_index(7),
            slot: 7,
            is_data: true,
        };
        let mut site = CacheStub::store_own_data(hit);
        let snap = site.snapshot();

        // The snapshot reads the captured store hit and the current ABI version.
        assert_eq!(snap.version(), CACHE_STUB_ABI_VERSION);
        assert!(snap.store_own_data_hit().is_some());
        assert!(snap.own_data_hit().is_none());

        // The live site is then replaced by a different (load) stub, as happens
        // when the interpreter re-profiles the site. The snapshot, owning its
        // own clone, is unaffected.
        site = CacheStub::load_own_data(hit);
        assert!(site.own_data_hit().is_some());
        assert!(snap.store_own_data_hit().is_some());
        assert!(snap.own_data_hit().is_none());
    }
}
