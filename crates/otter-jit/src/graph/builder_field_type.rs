//! Shape-owned value bounds for ordinary field loads and stores.
//!
//! # Contents
//! - [`FieldValueBound`] describes an SSA value's nullish and ordinary arms.
//! - Builder helpers guard owner summaries, prove int32 payloads and refine edges.
//! - Nullish comparisons reuse owner guards already demanded by other consumers.
//! - Store selection forms each native store's one observation record.
//!
//! # Invariants
//! - Owner summaries authorize category unions or an exact Int32 constant.
//! - Exact target shapes require a live stability check before layout use.
//! - Owner guards stay at their source loads and survive only when consumed.
//! - Nullable bounds never enter ordinary layout facts before refinement.
//! - Exact Int32 or Number summaries authorize untagged field loads.
//! - Every admitted store receiver selects its own defining field owner.
//! - Named and literal stores select records from proofs the path already
//!   owns: current exact layouts, nullish constants, or a field bound whose
//!   guards other consumers demand and whose targets this path rechecked.
//!   They never add a guard to elide an observation.
//!
//! # See also
//! - [`super::Known`] for path facts and effect invalidation.
//! - `otter_vm::object::field_type` for the monotone runtime summary.

use super::{BranchKind, Builder, Graph, Kind, Known, NodeId, Repr, SmallVec};
use crate::field_store::{FieldStoreOwner, StoreValueBound};
use otter_vm::object::FieldLocation;
use otter_vm::object::field_type;
use otter_vm::value::tag;

const UNDEFINED: u8 = field_type::UNDEFINED as u8;
const NULL: u8 = field_type::NULL as u8;
const NULLISH: u8 = UNDEFINED | NULL;
const TOP: u64 = field_type::TOP;

/// A numeric field's exact value, or its unboxed payload, behind owner guards.
#[derive(Debug, Clone, Copy)]
pub(super) enum FieldInt32 {
    Constant(i32),
    Payload,
}

/// A value bound whose exact ordinary targets are contingent on stability.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct FieldValueBound {
    pub(super) shapes: SmallVec<[u32; 2]>,
    pub(super) nullish: u8,
    pub(super) guards: SmallVec<[NodeId; 4]>,
}

impl FieldValueBound {
    pub(super) fn union(&self, other: &Self) -> Option<Self> {
        let mut shapes = self.shapes.clone();
        for &shape in &other.shapes {
            if !shapes.contains(&shape) {
                if shapes.len() == 2 {
                    return None;
                }
                shapes.push(shape);
            }
        }
        shapes.sort_unstable();
        let mut guards = self.guards.clone();
        for &guard in &other.guards {
            if !guards.contains(&guard) {
                guards.push(guard);
            }
        }
        guards.sort_unstable();
        Some(Self {
            shapes,
            nullish: self.nullish | other.nullish,
            guards,
        })
    }

    pub(super) fn of(
        graph: &Graph,
        value: NodeId,
        known: &Known,
        stable: &rustc_hash::FxHashSet<u32>,
    ) -> Option<Self> {
        if let Some(bound) = known.field_values.get(&value) {
            return Some(bound.clone());
        }
        if let Some(shapes) = known
            .layout(value)
            .and_then(|layout| layout.shapes.as_ref())
            && !shapes.is_empty()
            && shapes.len() <= 2
            && shapes.iter().all(|shape| stable.contains(shape))
        {
            return Some(Self {
                shapes: shapes.iter().copied().collect(),
                nullish: 0,
                guards: SmallVec::new(),
            });
        }
        let nullish = match graph.node(value).kind {
            Kind::ConstTagged(bits) if bits == tag::VALUE_UNDEFINED => UNDEFINED,
            Kind::ConstTagged(bits) if bits == tag::VALUE_NULL => NULL,
            _ => return None,
        };
        Some(Self {
            shapes: SmallVec::new(),
            nullish,
            guards: SmallVec::new(),
        })
    }
}

pub(super) fn merged_bound<'a>(
    graph: &Graph,
    stable: &rustc_hash::FxHashSet<u32>,
    inputs: impl Iterator<Item = (NodeId, &'a Known)>,
) -> Option<FieldValueBound> {
    let mut result: Option<FieldValueBound> = None;
    for (value, known) in inputs {
        let bound = FieldValueBound::of(graph, value, known, stable)?;
        result = Some(match result {
            Some(ref old) => old.union(&bound)?,
            None => bound,
        });
    }
    result
}

impl Builder<'_> {
    pub(super) fn field_load_bound(
        &mut self,
        receivers: &[u32],
        field: FieldLocation,
    ) -> Option<FieldValueBound> {
        let mut bound: Option<FieldValueBound> = None;
        let mut guards = SmallVec::new();
        for &receiver in receivers {
            let record = self.view.field_types.get(&(receiver, field))?;
            if record.shapes.is_empty() {
                return None;
            }
            let next = FieldValueBound {
                shapes: record.shapes.clone(),
                nullish: record.nullish_mask,
                guards: SmallVec::new(),
            };
            bound = Some(match bound {
                Some(ref old) => old.union(&next)?,
                None => next,
            });
            if !guards.contains(&record.guard) {
                guards.push(record.guard);
            }
        }
        if let Some(bound) = &mut bound {
            bound.guards = self.guard_field_owners(guards);
        }
        bound
    }

    /// Every receiver's defining cell contains only int32-tagged values.
    /// Demand the owner proof before the load replaces its tagged value.
    pub(super) fn field_load_int32(
        &mut self,
        receivers: &[u32],
        field: FieldLocation,
    ) -> Option<FieldInt32> {
        if receivers.is_empty() {
            return None;
        }
        let mut guards = SmallVec::new();
        let mut constant = None;
        let mut all_constant = true;
        for &receiver in receivers {
            let record = self.view.field_types.get(&(receiver, field))?;
            if !field_type::is_int32_only(record.guard.expected) {
                return None;
            }
            let value = field_type::int32_constant(record.guard.expected);
            all_constant &= value.is_some() && constant.is_none_or(|old| Some(old) == value);
            constant = constant.or(value);
            if !guards.contains(&record.guard) {
                guards.push(record.guard);
            }
        }
        let proofs = self.guard_field_owners(guards);
        self.required_field_guards.extend(proofs);
        Some(if all_constant {
            FieldInt32::Constant(constant.expect("every owner has the same constant"))
        } else {
            FieldInt32::Payload
        })
    }

    /// Every defining summary is purely numeric. Int32-only loads keep their
    /// raw payload path; this proof authorizes direct or mixed Float64 decoding.
    pub(super) fn field_load_number(
        &mut self,
        receivers: &[u32],
        field: FieldLocation,
    ) -> Option<bool> {
        if receivers.is_empty() {
            return None;
        }
        let mut guards = SmallVec::new();
        let mut double_only = true;
        for &receiver in receivers {
            let record = self.view.field_types.get(&(receiver, field))?;
            if !field_type::is_number_only(record.guard.expected) {
                return None;
            }
            double_only &= record.guard.expected & field_type::INT32 == 0;
            if !guards.contains(&record.guard) {
                guards.push(record.guard);
            }
        }
        let proofs = self.guard_field_owners(guards);
        self.required_field_guards.extend(proofs);
        Some(double_only)
    }

    fn guard_field_owners(
        &mut self,
        owners: SmallVec<[otter_vm::jit::JitFieldTypeGuard; 4]>,
    ) -> SmallVec<[NodeId; 4]> {
        let mut guards = SmallVec::new();
        let mut existing = SmallVec::new();
        for guard in owners {
            if let Some(&(expected, node)) = self.known.checked_field_types.get(&guard.owner_shape)
                && expected == guard.expected
            {
                if !existing.contains(&node) {
                    existing.push(node);
                }
            } else {
                guards.push(guard);
            }
        }
        if !guards.is_empty() {
            let node = self.add(Kind::CheckFieldTypes(guards.clone()), &[], Repr::None);
            existing.push(node);
            for guard in guards {
                self.known
                    .checked_field_types
                    .insert(guard.owner_shape, (guard.expected, node));
            }
        }
        existing
    }

    pub(super) fn prune_field_guards(&mut self) {
        for (&id, guards) in &self.field_nullish_candidates {
            if guards
                .iter()
                .all(|guard| self.required_field_guards.contains(guard))
                && let Kind::NullishEqual {
                    object_or_nullish, ..
                } = &mut self.graph.node_mut(id).kind
            {
                *object_or_nullish = true;
            }
        }
        let mut removed = rustc_hash::FxHashSet::default();
        for (index, node) in self.graph.nodes.iter_mut().enumerate() {
            let id = NodeId(index as u32);
            if matches!(node.kind, Kind::CheckFieldTypes(_))
                && !self.required_field_guards.contains(&id)
            {
                removed.insert(id);
                node.block = None;
                node.eager = None;
            }
        }
        for block in &mut self.graph.blocks {
            block.body.retain(|node| !removed.contains(node));
        }
    }

    pub(super) fn field_nullish_equal(&mut self, value: NodeId, negate: bool) -> NodeId {
        let object_or_nullish = self
            .known
            .get(value)
            .is_some_and(|info| info.ordinary_object);
        let guards = self
            .known
            .field_values
            .get(&value)
            .map(|bound| bound.guards.clone());
        let result = self.add(
            Kind::NullishEqual {
                negate,
                object_or_nullish,
            },
            &[value],
            Repr::Tagged,
        );
        if !object_or_nullish && let Some(guards) = guards {
            self.field_nullish_candidates.insert(result, guards);
        }
        result
    }

    fn check_field_targets(&mut self, shapes: &[u32]) {
        let unchecked: SmallVec<[u32; 4]> = shapes
            .iter()
            .copied()
            .filter(|shape| !self.known.stable_checked.contains(shape))
            .collect();
        if !unchecked.is_empty() {
            self.add(Kind::CheckStableShapes(unchecked.clone()), &[], Repr::None);
            self.known.stable_checked.extend(unchecked);
        }
    }

    pub(super) fn field_result_shapes(&mut self, value: NodeId) -> Option<SmallVec<[u32; 4]>> {
        let bound = self.known.field_values.get(&value)?.clone();
        if bound.shapes.is_empty() {
            return None;
        }
        self.required_field_guards.extend(&bound.guards);
        if bound.nullish != 0 {
            self.add(
                Kind::CheckNotNullish {
                    mask: bound.nullish,
                },
                &[value],
                Repr::None,
            );
            self.known.field_values.get_mut(&value)?.nullish = 0;
        }
        self.check_field_targets(&bound.shapes);
        self.prove_ordinary_object(value);
        let shapes: SmallVec<[u32; 4]> = bound.shapes.into_iter().collect();
        let layout = self.known.layout_entry(value);
        layout.shapes = Some(shapes.clone());
        layout.writable = false;
        Some(shapes)
    }

    pub(super) fn field_store_owners(
        &mut self,
        receivers: &[u32],
        field: FieldLocation,
        value: NodeId,
    ) -> Option<SmallVec<[(u32, FieldStoreOwner); 4]>> {
        let records: Option<Vec<_>> = receivers
            .iter()
            .map(|&shape| {
                self.view
                    .field_types
                    .get(&(shape, field))
                    .cloned()
                    .map(|record| (shape, record))
            })
            .collect();
        let records = records?;
        if records
            .iter()
            .all(|(_, record)| record.guard.expected & TOP != 0)
        {
            return Some(SmallVec::new());
        }
        let current = self
            .known
            .layout(value)
            .and_then(|layout| layout.shapes.as_ref());
        let fresh = current.is_some_and(|shapes| !shapes.is_empty() && shapes.len() <= 2);
        let rhs = current
            .filter(|shapes| !shapes.is_empty() && shapes.len() <= 2)
            .map(|shapes| FieldValueBound {
                shapes: shapes.iter().copied().collect(),
                nullish: 0,
                guards: SmallVec::new(),
            })
            .or_else(|| {
                FieldValueBound::of(&self.graph, value, &self.known, &self.root.stable_shapes)
            });
        let bound = self.store_value_bound(value).or_else(|| {
            rhs.as_ref().map(|rhs| StoreValueBound {
                shapes: rhs.shapes.clone(),
                nullish: rhs.nullish,
                int32: false,
                double: false,
                int32_constant: None,
            })
        });
        let mut owners: SmallVec<[(u32, FieldStoreOwner); 4]> = SmallVec::new();
        let mut uses_bound = false;
        for (receiver_shape, record) in records {
            let owner = FieldStoreOwner::select(&record, bound.as_ref());
            if owner.observes() {
                self.known
                    .checked_field_types
                    .remove(&record.guard.owner_shape);
            } else if record.guard.expected & TOP == 0 {
                uses_bound = true;
            }
            owners.push((receiver_shape, owner));
        }
        if uses_bound && let Some(rhs) = rhs {
            self.required_field_guards.extend(rhs.guards);
            if !fresh {
                self.check_field_targets(&rhs.shapes);
            }
        }
        if owners.iter().all(|(_, owner)| !owner.observes()) {
            owners.clear();
        }
        Some(owners)
    }

    /// One selected record per literal input, in source order, for the fit of
    /// this source body's prepared geometry. The candidate is born after
    /// every input, so no input aliases it, and the fit neither collects nor
    /// reenters: each input keeps its current bound. `None` leaves only the
    /// committed allocation: no geometry, or a field without authority.
    pub(super) fn literal_field_owners(
        &self,
        inputs: &[NodeId],
    ) -> Option<SmallVec<[FieldStoreOwner; 4]>> {
        let plan = self.view.literal_allocations.objects.get(&self.pc)?;
        if plan.value_count as usize != inputs.len() {
            return None;
        }
        inputs
            .iter()
            .enumerate()
            .map(|(index, &value)| {
                let record = self
                    .view
                    .field_types
                    .get(&(plan.shape, FieldLocation::inline(index as u32)))?;
                Some(FieldStoreOwner::select(
                    record,
                    self.store_value_bound(value).as_ref(),
                ))
            })
            .collect()
    }

    /// The bound `value` has at a native store, from proofs this path already
    /// owns: an int32 representation, a current exact layout, a nullish constant, or a field bound
    /// whose owner guards other consumers demand and whose ordinary targets
    /// this path already rechecked. Adds no node and demands no guard.
    pub(super) fn store_value_bound(&self, value: NodeId) -> Option<StoreValueBound> {
        if self.graph.node(value).repr == Repr::Float64
            || self.graph.node(value).kind == Kind::Float64ToTagged
        {
            return Some(StoreValueBound {
                shapes: SmallVec::new(),
                nullish: 0,
                int32: false,
                double: true,
                int32_constant: None,
            });
        }
        if self.graph.node(value).repr == Repr::Int32
            || matches!(self.graph.node(value).kind, Kind::Int32ToTagged)
            || matches!(self.graph.node(value).kind, Kind::ConstTagged(bits)
                if otter_vm::Value::from_bits(bits).as_i32().is_some())
        {
            return Some(StoreValueBound {
                shapes: SmallVec::new(),
                nullish: 0,
                int32: true,
                double: false,
                int32_constant: self.store_int32_constant(value),
            });
        }
        if self
            .known
            .get(value)
            .is_some_and(|info| info.int32.is_some() || info.float64.is_some())
        {
            // An exact integer Number can still be an encoded Double.
            // Semantic narrowing never proves the stored word's encoding.
            return Some(StoreValueBound {
                shapes: SmallVec::new(),
                nullish: 0,
                int32: true,
                double: true,
                int32_constant: None,
            });
        }
        if matches!(self.graph.node(value).kind, Kind::ConstTagged(bits)
            if otter_vm::Value::from_bits(bits).is_number())
        {
            return Some(StoreValueBound {
                shapes: SmallVec::new(),
                nullish: 0,
                int32: false,
                double: true,
                int32_constant: None,
            });
        }
        if let Some(shapes) = self
            .known
            .layout(value)
            .and_then(|layout| layout.shapes.as_ref())
            && !shapes.is_empty()
            && shapes.len() <= 2
        {
            return Some(StoreValueBound {
                shapes: shapes.iter().copied().collect(),
                nullish: 0,
                int32: false,
                double: false,
                int32_constant: None,
            });
        }
        match self.graph.node(value).kind {
            Kind::ConstTagged(bits) if bits == tag::VALUE_UNDEFINED => {
                return Some(StoreValueBound {
                    shapes: SmallVec::new(),
                    nullish: UNDEFINED,
                    int32: false,
                    double: false,
                    int32_constant: None,
                });
            }
            Kind::ConstTagged(bits) if bits == tag::VALUE_NULL => {
                return Some(StoreValueBound {
                    shapes: SmallVec::new(),
                    nullish: NULL,
                    int32: false,
                    double: false,
                    int32_constant: None,
                });
            }
            _ => {}
        }
        let bound = self.known.field_values.get(&value)?;
        (bound
            .guards
            .iter()
            .all(|guard| self.required_field_guards.contains(guard))
            && bound
                .shapes
                .iter()
                .all(|shape| self.known.stable_checked.contains(shape)))
        .then(|| StoreValueBound {
            shapes: bound.shapes.clone(),
            nullish: bound.nullish,
            int32: false,
            double: false,
            int32_constant: None,
        })
    }

    fn store_int32_constant(&self, value: NodeId) -> Option<i32> {
        let node = self.graph.node(value);
        match node.kind {
            Kind::ConstInt32(value) => Some(value),
            Kind::ConstTagged(bits) => otter_vm::Value::from_bits(bits).as_i32(),
            Kind::Int32ToTagged => self.int32_constant(node.inputs[0]),
            _ => None,
        }
    }

    pub(super) fn refine_field_branch(&mut self, kind: BranchKind, inputs: &[NodeId], truth: bool) {
        let mut test = None;
        match kind {
            BranchKind::Nullish => test = Some((inputs[0], NULLISH, truth)),
            BranchKind::TaggedEqual => test = self.nullish_equality(inputs, truth, false),
            BranchKind::Truthy => {
                let value = inputs[0];
                match self.graph.node(value).kind {
                    Kind::NullishEqual { negate, .. } => {
                        test = Some((self.graph.node(value).inputs[0], NULLISH, truth != negate))
                    }
                    Kind::StrictEqual { negate } => {
                        test = self.nullish_equality(
                            &self.graph.node(value).inputs,
                            truth != negate,
                            false,
                        )
                    }
                    Kind::LooseEqual { negate } => {
                        test = self.nullish_equality(
                            &self.graph.node(value).inputs,
                            truth != negate,
                            true,
                        )
                    }
                    _ if self.known.field_values.contains_key(&value) => {
                        test = Some((value, NULLISH, !truth))
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        let Some((value, mask, equal)) = test else {
            return;
        };
        let Some(bound) = self.known.field_values.get_mut(&value) else {
            return;
        };
        if equal {
            bound.nullish &= mask;
            bound.shapes.clear();
        } else {
            bound.nullish &= !mask;
        }
    }

    fn nullish_equality(
        &self,
        inputs: &[NodeId],
        equal: bool,
        loose: bool,
    ) -> Option<(NodeId, u8, bool)> {
        for (value, constant) in [(inputs[0], inputs[1]), (inputs[1], inputs[0])] {
            let mask = match self.graph.node(constant).kind {
                Kind::ConstTagged(bits) if bits == tag::VALUE_UNDEFINED => UNDEFINED,
                Kind::ConstTagged(bits) if bits == tag::VALUE_NULL => NULL,
                _ => continue,
            };
            return Some((value, if loose { NULLISH } else { mask }, equal));
        }
        None
    }
}
