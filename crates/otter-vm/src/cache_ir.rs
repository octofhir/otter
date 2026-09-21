//! One inline-cache representation shared by every property opcode.
//!
//! A cache stub is a short linear program — a sequence of [`CacheOp`] guards
//! and loads over a small operand file, plus per-stub "data" (shape ids and
//! resolved own-property hits) the ops reference by index rather than bake in.
//! One [`CacheStub`] shape serves `LoadProperty` and `StoreProperty`: the guard
//! ops are shared and only the terminal op differs. `HasProperty` completes
//! through the rooted committed-value kernel and owns no property IC.
//!
//! # Contents
//!
//! - [`CacheOp`] — the guard/load opcodes.
//! - [`CacheStub`] — an op sequence plus its referenced shape ids and hits.
//! - executor entry points: [`CacheStub::run_load`] and
//!   [`CacheStub::run_store`], plus complete immutable JIT snapshots.
//!
//! # Invariants
//! - Store misses are allocation-free; failure while growing a matched
//!   transition propagates as OOM rather than falling through to another stub.
//!
//! - Operand `0` is always the receiver; `1` is the receiver's prototype once a
//!   [`CacheOp::LoadPrototype`] has run. No op reads an operand before it is
//!   defined (guaranteed by the builders).
//! - Shapes referenced by stub data are interned and immortal (rooted by the
//!   transition tables, pinned in non-moving old space), so the stored
//!   [`crate::object::ShapeId`]/hit metadata never dangles.

use smallvec::SmallVec;

use otter_gc::raw::SlotVisitor;

use crate::object::{self, AtomOwnPropertyHit, ShapeId, StorePropertyTransition};
use crate::property_atom::AtomizedPropertyKey;
use crate::{JsObject, Value};

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
    /// Terminal (load): produce the data value at `hits[hit]` on
    /// `operands[obj]`, validating the slot's shape/atom/key guard.
    LoadDataSlotResult {
        /// Operand holding the object that owns the slot.
        obj: OperandId,
        /// Index into the stub's `hits`.
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
    /// Atom-aware own-property hits consumed by load terminals.
    hits: SmallVec<[AtomOwnPropertyHit; 1]>,
    /// Hidden-class transitions replayed by [`CacheOp::StoreAddTransition`].
    /// Their target shapes are GC roots, visited by [`CacheStub::trace_roots`].
    transitions: SmallVec<[StorePropertyTransition; 1]>,
}

impl CacheStub {
    /// Copy this complete program into the owned, target-neutral JIT DTO.
    ///
    /// `resolve_shape` is the VM's compile-boundary validation: it maps an
    /// interned semantic shape id to its current stable compressed token. If
    /// any referenced fact cannot be validated, or any CacheIR op has no
    /// native representation yet, the whole program is rejected.
    pub(crate) fn snapshot_for_jit(
        &self,
        mut resolve_shape: impl FnMut(ShapeId) -> Option<u32>,
    ) -> Option<crate::jit::JitCacheIrProgram> {
        use crate::jit::JitCacheIrOp;

        let mut ops = Vec::with_capacity(self.ops.len().saturating_add(1));
        for op in &self.ops {
            match *op {
                CacheOp::GuardShapeId { obj, shape } => {
                    let expected = *self.shape_ids.get(shape as usize)?;
                    let shape = resolve_shape(expected)?;
                    if shape == 0 {
                        return None;
                    }
                    ops.push(JitCacheIrOp::GuardShape { object: obj, shape });
                }
                CacheOp::LoadPrototype { obj, dst } => {
                    ops.push(JitCacheIrOp::LoadPrototype {
                        object: obj,
                        result: dst,
                    });
                }
                CacheOp::LoadDataSlotResult { obj, hit } => {
                    let hit = *self.hits.get(hit as usize)?;
                    let shape = resolve_shape(hit.shape_id)?;
                    if shape == 0 {
                        return None;
                    }
                    ops.push(JitCacheIrOp::GuardShape { object: obj, shape });
                    ops.push(JitCacheIrOp::GuardAtomSlot {
                        object: obj,
                        atom: hit.atom_id.raw(),
                        value_byte: slot_value_byte(hit.slot),
                        writable: false,
                    });
                    ops.push(JitCacheIrOp::LoadField {
                        object: obj,
                        value_byte: slot_value_byte(hit.slot),
                    });
                }
                CacheOp::StoreDataSlot { obj, hit } => {
                    let hit = *self.hits.get(hit as usize)?;
                    let shape = resolve_shape(hit.shape_id)?;
                    if shape == 0 {
                        return None;
                    }
                    ops.push(JitCacheIrOp::GuardShape { object: obj, shape });
                    ops.push(JitCacheIrOp::GuardAtomSlot {
                        object: obj,
                        atom: hit.atom_id.raw(),
                        value_byte: slot_value_byte(hit.slot),
                        writable: true,
                    });
                    ops.push(JitCacheIrOp::StoreField {
                        object: obj,
                        value_byte: slot_value_byte(hit.slot),
                    });
                }
                CacheOp::StoreAddTransition { transition } => {
                    let transition = self.transitions.get(transition as usize)?;
                    let from_shape = resolve_shape(transition.from_shape_id)?;
                    let to_shape = resolve_shape(transition.to_shape_id)?;
                    if from_shape == 0 || to_shape == 0 {
                        return None;
                    }
                    ops.push(JitCacheIrOp::GuardShape {
                        object: 0,
                        shape: from_shape,
                    });
                    match &transition.kind {
                        object::StorePropertyTransitionKind::OwnAdd => {
                            ops.push(JitCacheIrOp::GuardPrototypeNull { object: 0 });
                        }
                        object::StorePropertyTransitionKind::PrototypeChainMissing { chain } => {
                            let mut object = 0;
                            for expected in chain {
                                let shape = resolve_shape(*expected)?;
                                if shape == 0 {
                                    return None;
                                }
                                ops.push(JitCacheIrOp::LoadPrototype { object, result: 1 });
                                ops.push(JitCacheIrOp::GuardShape { object: 1, shape });
                                object = 1;
                            }
                            ops.push(JitCacheIrOp::GuardPrototypeNull { object });
                        }
                        object::StorePropertyTransitionKind::DirectPrototypeWritableData {
                            prototype_hit,
                        } => {
                            let shape = resolve_shape(prototype_hit.shape_id)?;
                            if shape == 0 {
                                return None;
                            }
                            ops.push(JitCacheIrOp::LoadPrototype {
                                object: 0,
                                result: 1,
                            });
                            ops.push(JitCacheIrOp::GuardShape { object: 1, shape });
                            ops.push(JitCacheIrOp::GuardAtomSlot {
                                object: 1,
                                atom: prototype_hit.atom_id.raw(),
                                value_byte: slot_value_byte(prototype_hit.slot),
                                writable: true,
                            });
                        }
                    }
                    let value_byte = slot_value_byte(transition.slot);
                    ops.push(JitCacheIrOp::GuardExtensible {
                        object: 0,
                        value_byte,
                    });
                    ops.push(JitCacheIrOp::StoreField {
                        object: 0,
                        value_byte,
                    });
                    ops.push(JitCacheIrOp::PublishShape {
                        object: 0,
                        shape: to_shape,
                        new_len: transition.slot.checked_add(1)?,
                        initialize_inline: transition.slot == 0,
                    });
                }
            }
        }
        (!ops.is_empty()).then(|| crate::jit::JitCacheIrProgram {
            ops: ops.into_boxed_slice(),
        })
    }

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
        hit: AtomOwnPropertyHit,
    ) -> Self {
        let mut ops = SmallVec::new();
        ops.push(CacheOp::GuardShapeId { obj: 0, shape: 0 });
        ops.push(CacheOp::LoadPrototype { obj: 0, dst: 1 });
        ops.push(CacheOp::LoadDataSlotResult { obj: 1, hit: 0 });
        Self {
            ops,
            shape_ids: SmallVec::from_elem(receiver_shape_id, 1),
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
    /// on success or `None` on any guard miss.
    #[inline]
    fn run_guards(&self, recv: JsObject, heap: &otter_gc::GcHeap) -> Option<[Option<JsObject>; 2]> {
        let mut operands: [Option<JsObject>; 2] = [Some(recv), None];
        for op in &self.ops {
            match *op {
                CacheOp::GuardShapeId { obj, shape } => {
                    let obj = Self::operand(&operands, obj)?;
                    if object::shape_id(obj, heap) != self.shape_ids[shape as usize] {
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
        let operands = self.run_guards(recv, heap)?;
        let CacheOp::LoadDataSlotResult { obj, hit } = self.ops.last().copied()? else {
            return None;
        };
        let obj = Self::operand(&operands, obj)?;
        object::load_own_data_slot_atom(obj, heap, key, self.hits[hit as usize])
    }

    /// The load program for an already-resolved data slot.
    #[must_use]
    pub(crate) fn from_resolved_load(
        receiver_shape_id: ShapeId,
        resolved: &ResolvedDataSlot,
    ) -> Self {
        match resolved.hops {
            0 => Self::load_own_data(resolved.hit),
            _ => Self::load_direct_prototype_data(receiver_shape_id, resolved.hit),
        }
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

    /// Execute as a `StoreProperty` cache. `Ok(Some(()))` commits the write;
    /// `Ok(None)` is an allocation-free miss, and OOM stops the probe bank.
    pub(crate) fn run_store(
        &self,
        recv: JsObject,
        heap: &mut otter_gc::GcHeap,
        key: AtomizedPropertyKey<'_>,
        value: &Value,
    ) -> Result<Option<()>, otter_gc::OutOfMemory> {
        if !object::supports_fast_property_ic(recv, heap) {
            return Ok(None);
        }
        let Some(op) = self.ops.last().copied() else {
            return Ok(None);
        };
        match op {
            CacheOp::StoreDataSlot { obj: 0, hit } => Ok(object::store_own_data_slot_atom(
                recv,
                heap,
                key,
                self.hits[hit as usize],
                value,
            )),
            CacheOp::StoreAddTransition { transition } => object::replay_store_property_transition(
                recv,
                heap,
                key,
                &self.transitions[transition as usize],
                value,
            ),
            _ => Ok(None),
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
}

/// Where a named data property lives relative to the receiver, and what it
/// currently holds.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ResolvedDataSlot {
    /// Prototype hops from the receiver to the holder: `0` own, `1` the
    /// receiver's direct prototype. Deeper holders are not resolved here —
    /// proving the property absent on every object in between costs a guard
    /// per link, which is exactly the boundary the cache stubs already draw.
    pub(crate) hops: u8,
    /// The holder's own-slot hit, guarded by its shape.
    pub(crate) hit: AtomOwnPropertyHit,
    /// The value the slot holds right now.
    pub(crate) value: Value,
    /// Whether the resolved data descriptor accepts an ordinary assignment.
    pub(crate) is_writable: bool,
}

/// Resolve a named property to a plain data slot on the receiver or its direct
/// prototype.
///
/// This is the one resolution both consumers share: the per-site stub builder
/// above, and the isolate-wide `(shape, atom)` cache that answers sites the
/// stub layer has given up on. `None` for accessors, deeper holders, absent
/// properties, and any receiver hidden classes cannot describe.
#[must_use]
pub(crate) fn resolve_atom_data_slot(
    obj: JsObject,
    heap: &otter_gc::GcHeap,
    key: AtomizedPropertyKey<'_>,
) -> Option<ResolvedDataSlot> {
    if !object::supports_fast_property_ic(obj, heap) {
        return None;
    }
    let own = object::lookup_own_atom(obj, heap, key);
    if let (Some(hit), object::PropertyLookup::Data { value, flags }) = (own.hit, own.lookup) {
        return Some(ResolvedDataSlot {
            hops: 0,
            hit,
            value,
            is_writable: flags.writable(),
        });
    }
    if own.hit.is_some() {
        return None;
    }
    let proto = object::prototype(obj, heap)?;
    if !object::supports_fast_property_ic(proto, heap) {
        return None;
    }
    let inherited = object::lookup_own_atom(proto, heap, key);
    if let (Some(hit), object::PropertyLookup::Data { value, flags }) =
        (inherited.hit, inherited.lookup)
    {
        return Some(ResolvedDataSlot {
            hops: 1,
            hit,
            value,
            is_writable: flags.writable(),
        });
    }
    None
}

/// Byte offset of a string-keyed own slot inside the object's value slab.
fn slot_value_byte(slot: u16) -> u32 {
    u32::from(slot) * std::mem::size_of::<crate::Value>() as u32
}
