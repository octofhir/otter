//! Escape analysis: constructor receivers that never leave the compiled
//! function are not allocated.
//!
//! A receiver an inlined `[[Construct]]` allocates ([`Kind::NewReceiver`])
//! often lives only to have a few fields written and read back: a vector
//! built, read and dropped by the caller it was inlined into. Such an object
//! is replaced by its fields. Each read becomes the value last written, and
//! a deopt that needs the object materializes it from the fields it holds at
//! that point.
//!
//! # Contents
//! - [`eliminate_allocations`] — select the receivers that do not escape,
//!   track their fields through the graph and rewrite it.
//! - [`fold_conversions`] — a number boxed and unboxed again is the number,
//!   or the direct conversion between its two representations.
//! - [`remove_dead_values`] — drop pure values nothing reads.
//!
//! # Invariants
//! - An object is elided only when every use is a field write or read whose
//!   slot and hidden class are known at that point, a shape check it passes,
//!   a write barrier on it, a method read the receiver does not feed, or an
//!   eager frame state. Any other use, a phi, a lazy frame state, a loop
//!   header's state, or a write inside a loop its allocation precedes keeps
//!   the allocation.
//! - An object stored into another object is elided only with that object;
//!   one escaping escapes everything stored into it.
//! - Graph owns merge phis from creation; speculative phis stay unplaced
//!   and are truncated when a walk retries. Stores, uses and frame states
//!   change only after escape analysis converges.
//! - A virtual field retains its one typed SSA value. Tagged reads box it;
//!   typed reads require a pure projection of that value. Observing stores
//!   keep the allocation so later historical field proofs see their widening.
//! - Fields that differ between the predecessors of a merge meet in a phi of
//!   their common representation; a merge where an object's hidden class
//!   differs, or where the representations differ, keeps the allocation.
//! - The allocation's proofs stay as [`Kind::CheckReceiverPlan`]: the
//!   object's hidden class fixes a prototype the live constructor must have.
//! - A node whose eager state names an elided object gets its own copy of
//!   that state's chain, naming a [`Kind::VirtualObject`] of the fields at
//!   the node: frame states are shared, field values are not.
//!
//! # See also
//! - [`super::frame::deopt_frames`] turns virtual objects into recipes.
//! - `otter_vm::Interpreter::materialize_virtual_objects` rebuilds them.

use rustc_hash::{FxHashMap, FxHashSet};
use smallvec::SmallVec;

use otter_vm::jit::{JitCacheIrOp, JitCompileSnapshot};
use otter_vm::object::FieldLocation;

use super::ir::{BlockId, FrameStateId, Graph, Kind, NodeId, Repr};

/// What is known of one candidate object at a point.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Object {
    shape: u32,
    fields: SmallVec<[NodeId; 8]>,
}

/// Every live candidate's object at a point.
type State = FxHashMap<NodeId, Object>;

/// How the walk ended.
enum Walk {
    /// These candidates turned out to escape; walk again without them.
    Escaped(FxHashSet<NodeId>),
    /// Every remaining candidate is elided with this rewrite.
    Done(Rewrite),
}

/// The changes one successful walk decided.
#[derive(Default)]
struct Rewrite {
    /// A load's replacement value.
    replace: FxHashMap<NodeId, NodeId>,
    /// Nodes that disappear: writes, barriers and passed shape checks.
    remove: FxHashSet<NodeId>,
    /// A method read from a code-retained cell, which no longer takes the
    /// receiver.
    cell_reads: FxHashMap<NodeId, u64>,
    /// Graph-owned merge phis, placed only after the walk succeeds.
    phis: Vec<(BlockId, NodeId)>,
    /// Pure conversions placed immediately before the read they replace.
    conversions: Vec<(NodeId, NodeId)>,
    /// The candidates' objects each node's eager state names.
    snapshots: FxHashMap<NodeId, State>,
}

/// Elide the receivers of `graph` that do not escape.
pub(crate) fn eliminate_allocations(
    graph: &mut Graph,
    layout: &[BlockId],
    loop_states: &[FrameStateId],
    view: &JitCompileSnapshot,
    inline_views: &[std::sync::Arc<JitCompileSnapshot>],
) {
    let mut candidates: FxHashSet<NodeId> = layout
        .iter()
        .flat_map(|&block| graph.block(block).body.iter().copied())
        .filter(|&node| matches!(graph.node(node).kind, Kind::NewReceiver(_)))
        .collect();
    if candidates.is_empty() {
        return;
    }
    escape_by_use(graph, layout, &mut candidates);
    // A loop header's own state outlives this pass: guards hoisted to the
    // preheader resume through it.
    for &state in loop_states {
        for value in chain_values(graph, state) {
            candidates.remove(&value);
        }
    }
    propagate_escapes(graph, layout, &mut candidates);
    escape_through_loops(graph, layout, &mut candidates);
    if candidates.is_empty() {
        return;
    }
    let undefined = graph.constant(Kind::ConstTagged(crate::entry::VALUE_UNDEFINED));
    let rewrite = loop {
        if candidates.is_empty() {
            return;
        }
        let nodes_before = graph.nodes.len();
        match walk(graph, layout, view, inline_views, &candidates, undefined) {
            Walk::Escaped(escaped) => {
                graph.nodes.truncate(nodes_before);
                for node in escaped {
                    candidates.remove(&node);
                }
                propagate_escapes(graph, layout, &mut candidates);
            }
            Walk::Done(rewrite) => break rewrite,
        }
    };
    apply(graph, layout, &candidates, rewrite);
}

/// Drop every candidate some use lets escape, and everything stored into an
/// escaping one.
fn escape_by_use(graph: &Graph, layout: &[BlockId], candidates: &mut FxHashSet<NodeId>) {
    let mut escaped = FxHashSet::default();
    let all = candidates.clone();
    let mut named = StateCandidates::new(&all);
    for &block in layout {
        let data = graph.block(block);
        for &node in data
            .phis
            .iter()
            .chain(&data.body)
            .chain(data.control.iter())
        {
            let node_data = graph.node(node);
            for (index, &input) in node_data.inputs.iter().enumerate() {
                if candidates.contains(&input) && !admitted_use(&node_data.kind, index) {
                    escaped.insert(input);
                }
            }
            if let Some(state) = node_data.lazy {
                escaped.extend(named.of(graph, state));
            }
        }
    }
    for node in escaped {
        candidates.remove(&node);
    }
    propagate_escapes(graph, layout, candidates);
}

/// Whether input `index` of a node of `kind` may be an elided object.
fn admitted_use(kind: &Kind, index: usize) -> bool {
    match kind {
        // The receiver of a field write; a value stored into a candidate
        // escapes with it ([`propagate_escapes`]).
        Kind::StoreNamedProperty { .. }
        | Kind::StoreOwnField { .. }
        | Kind::StoreDoubleOwnField { .. } => index <= 1,
        Kind::LoadOwnField { .. }
        | Kind::LoadNumberOwnField { .. }
        | Kind::LoadNamedProperty { .. }
        | Kind::CheckShapes { .. }
        | Kind::LoadGuardedMethod { .. } => index == 0,
        Kind::WriteBarrier => index <= 1,
        _ => false,
    }
}

/// An object stored into an escaping object, or into anything that is not a
/// candidate, escapes too.
fn propagate_escapes(graph: &Graph, layout: &[BlockId], candidates: &mut FxHashSet<NodeId>) {
    loop {
        let mut escaped = Vec::new();
        for &block in layout {
            for &node in &graph.block(block).body {
                let data = graph.node(node);
                let stores = matches!(
                    data.kind,
                    Kind::StoreNamedProperty { .. }
                        | Kind::StoreOwnField { .. }
                        | Kind::StoreDoubleOwnField { .. }
                );
                if stores
                    && let [receiver, value, ..] = data.inputs[..]
                    && candidates.contains(&value)
                    && (!candidates.contains(&receiver) || receiver == value)
                {
                    escaped.push(value);
                }
                if matches!(data.kind, Kind::WriteBarrier)
                    && let [object, value, ..] = data.inputs[..]
                    && candidates.contains(&value)
                    && !candidates.contains(&object)
                {
                    escaped.push(value);
                }
            }
        }
        if escaped.is_empty() {
            return;
        }
        for node in escaped {
            candidates.remove(&node);
        }
    }
}

/// A write inside a loop the allocation precedes would need a loop phi per
/// field; such an object keeps its allocation.
fn escape_through_loops(graph: &Graph, layout: &[BlockId], candidates: &mut FxHashSet<NodeId>) {
    let mut escaped = Vec::new();
    for &header in layout {
        if !graph.block(header).is_loop {
            continue;
        }
        let body = super::licm::loop_body(graph, header);
        for &block in &body {
            for &node in &graph.block(block).body {
                let data = graph.node(node);
                if !matches!(
                    data.kind,
                    Kind::StoreNamedProperty { .. }
                        | Kind::StoreOwnField { .. }
                        | Kind::StoreDoubleOwnField { .. }
                ) {
                    continue;
                }
                let receiver = data.inputs[0];
                if candidates.contains(&receiver)
                    && graph
                        .node(receiver)
                        .block
                        .is_none_or(|defined| !body.contains(&defined))
                {
                    escaped.push(receiver);
                }
            }
        }
    }
    for node in escaped {
        candidates.remove(&node);
    }
    propagate_escapes(graph, layout, candidates);
}

/// The values every frame state of `state`'s chain names.
fn chain_values(graph: &Graph, state: FrameStateId) -> SmallVec<[NodeId; 16]> {
    let mut values = SmallVec::new();
    for state in graph.state_chain(state) {
        let mut copy = graph.frame_state(state).clone();
        copy.for_each_value_mut(|value| values.push(*value));
    }
    values
}

/// The candidates each frame state's chain names, computed once per state:
/// a state names its own values' candidates and its caller's.
struct StateCandidates<'g> {
    candidates: &'g FxHashSet<NodeId>,
    named: FxHashMap<FrameStateId, SmallVec<[NodeId; 2]>>,
}

impl<'g> StateCandidates<'g> {
    fn new(candidates: &'g FxHashSet<NodeId>) -> Self {
        Self {
            candidates,
            named: FxHashMap::default(),
        }
    }

    fn of(&mut self, graph: &Graph, state: FrameStateId) -> SmallVec<[NodeId; 2]> {
        if let Some(named) = self.named.get(&state) {
            return named.clone();
        }
        let data = graph.frame_state(state);
        let mut named = match &data.caller {
            Some(caller) => self.of(graph, caller.state),
            None => SmallVec::new(),
        };
        let own = data
            .caller
            .iter()
            .flat_map(|caller| {
                [caller.this, caller.closure, caller.new_target]
                    .into_iter()
                    .chain(caller.arguments.iter().flatten().copied())
            })
            .chain(data.registers.iter().map(|&(_, value)| value));
        for value in own {
            if self.candidates.contains(&value) && !named.contains(&value) {
                named.push(value);
            }
        }
        self.named.insert(state, named.clone());
        named
    }
}

/// Track every candidate's hidden class and fields in layout order.
fn walk(
    graph: &mut Graph,
    layout: &[BlockId],
    view: &JitCompileSnapshot,
    inline_views: &[std::sync::Arc<JitCompileSnapshot>],
    candidates: &FxHashSet<NodeId>,
    undefined: NodeId,
) -> Walk {
    let mut escaped = FxHashSet::default();
    let mut rewrite = Rewrite::default();
    let mut exits: FxHashMap<BlockId, State> = FxHashMap::default();
    let mut named_by = StateCandidates::new(candidates);
    for &block in layout {
        let mut state = match entry_state(graph, block, &exits, &mut escaped) {
            Some(Entry::State(state)) => state,
            Some(Entry::Merge(mut state, merges)) => {
                for (candidate, slot, inputs, repr) in merges {
                    let phi = graph.add_node(Kind::Phi, &inputs, repr);
                    rewrite.phis.push((block, phi));
                    if let Some(object) = state.get_mut(&candidate) {
                        object.fields[slot] = phi;
                    }
                }
                state
            }
            None => State::default(),
        };
        // A field read a merge phi replaces reads the phi's representation.
        let data = graph.block(block).clone();
        for &node in data.body.iter().chain(data.control.iter()) {
            let node_data = graph.node(node).clone();
            if let Some(eager) = node_data.eager {
                let named: State = named_by
                    .of(graph, eager)
                    .into_iter()
                    .filter_map(|value| {
                        let object = state.get(&value).cloned();
                        if object.is_none() {
                            escaped.insert(value);
                        }
                        object.map(|object| (value, object))
                    })
                    .collect();
                if !named.is_empty() {
                    rewrite.snapshots.insert(node, named);
                }
            }
            let source = super::metadata::source_view(view, inline_views, graph, node);
            match &node_data.kind {
                Kind::NewReceiver(plan) if candidates.contains(&node) => {
                    let shape = if plan.receiver_shape == 0 {
                        plan.prototype_root
                    } else {
                        plan.receiver_shape
                    };
                    state.insert(
                        node,
                        Object {
                            shape,
                            fields: std::iter::repeat_n(
                                undefined,
                                usize::from(plan.initial_field_count),
                            )
                            .collect(),
                        },
                    );
                }
                Kind::StoreNamedProperty {
                    byte_pc,
                    receiver_shape,
                    fresh,
                    plan,
                    owners,
                } => {
                    let receiver = node_data.inputs[0];
                    let Some(object) = candidate_object(&state, candidates, receiver, &mut escaped)
                    else {
                        continue;
                    };
                    let index = plan.program().map(usize::from).or_else(|| {
                        source.property_programs.get(byte_pc)?.iter().position(|program| {
                            matches!(program.ops.first(), Some(JitCacheIrOp::GuardShape { object: 0, shape }) if *shape == object.shape)
                        })
                    });
                    if index
                        .and_then(|index| owners.get(index))
                        .is_none_or(|owner| owner.observes())
                    {
                        // Historical field-type proofs after the store still
                        // need the actual widening/flag effect when it observes.
                        escaped.insert(receiver);
                        continue;
                    }
                    match store_effect(source, *byte_pc, object.shape, *fresh)
                        .filter(|_| receiver_shape.is_none_or(|shape| shape == object.shape))
                    {
                        Some((slot, next)) => {
                            let object = state.get_mut(&receiver).expect("a live object");
                            if !write_slot(object, slot, node_data.inputs[1]) {
                                escaped.insert(receiver);
                                continue;
                            }
                            object.shape = next;
                            rewrite.remove.insert(node);
                        }
                        None => {
                            escaped.insert(receiver);
                        }
                    }
                }
                Kind::StoreOwnField { field, owners, .. }
                | Kind::StoreDoubleOwnField { field, owners, .. } => {
                    let receiver = node_data.inputs[0];
                    if candidate_object(&state, candidates, receiver, &mut escaped).is_none() {
                        continue;
                    }
                    let object = state.get_mut(&receiver).expect("a live object");
                    if !owners.is_empty()
                        && owners
                            .iter()
                            .find(|(shape, _)| *shape == object.shape)
                            .is_none_or(|(_, owner)| owner.observes())
                    {
                        escaped.insert(receiver);
                        continue;
                    }
                    match inline_slot(*field) {
                        Some(slot) if slot < object.fields.len() => {
                            object.fields[slot] = node_data.inputs[1];
                            rewrite.remove.insert(node);
                        }
                        _ => {
                            escaped.insert(receiver);
                        }
                    }
                }
                Kind::LoadOwnField { field, .. } | Kind::LoadNumberOwnField { field, .. } => {
                    let receiver = node_data.inputs[0];
                    let Some(object) = candidate_object(&state, candidates, receiver, &mut escaped)
                    else {
                        continue;
                    };
                    match inline_slot(*field).and_then(|slot| object.fields.get(slot)) {
                        Some(&value) => match virtual_load(graph, &mut rewrite, node, value) {
                            Some(value) => {
                                rewrite.replace.insert(node, value);
                            }
                            None => {
                                escaped.insert(receiver);
                            }
                        },
                        None => {
                            escaped.insert(receiver);
                        }
                    }
                }
                Kind::LoadNamedProperty { byte_pc, .. } => {
                    let receiver = node_data.inputs[0];
                    let Some(object) = candidate_object(&state, candidates, receiver, &mut escaped)
                    else {
                        continue;
                    };
                    match own_load_slot(source, *byte_pc, object.shape)
                        .and_then(|slot| object.fields.get(slot))
                    {
                        Some(&value) => match virtual_load(graph, &mut rewrite, node, value) {
                            Some(value) => {
                                rewrite.replace.insert(node, value);
                            }
                            None => {
                                escaped.insert(receiver);
                            }
                        },
                        None => {
                            escaped.insert(receiver);
                        }
                    }
                }
                Kind::CheckShapes { shapes, .. } => {
                    let receiver = node_data.inputs[0];
                    let Some(object) = candidate_object(&state, candidates, receiver, &mut escaped)
                    else {
                        continue;
                    };
                    if shapes.contains(&object.shape) {
                        rewrite.remove.insert(node);
                    } else {
                        escaped.insert(receiver);
                    }
                }
                Kind::LoadGuardedMethod {
                    byte_pc,
                    target,
                    receiver_proved,
                    ..
                } => {
                    let receiver = node_data.inputs[0];
                    let Some(object) = candidate_object(&state, candidates, receiver, &mut escaped)
                    else {
                        continue;
                    };
                    let cell = source
                        .method_targets
                        .get(byte_pc)
                        .and_then(|methods| methods.get(usize::from(*target)))
                        .filter(|method| {
                            method.guard.method_cell != 0
                                && method.guard.accessor_byte.is_none()
                                && (*receiver_proved || method.guard.recv_shape == object.shape)
                        })
                        .map(|method| method.guard.method_cell);
                    match cell {
                        Some(cell) => {
                            rewrite.cell_reads.insert(node, cell);
                        }
                        None => {
                            escaped.insert(receiver);
                        }
                    }
                }
                Kind::WriteBarrier if candidates.contains(&node_data.inputs[0]) => {
                    rewrite.remove.insert(node);
                }
                _ => {}
            }
        }
        exits.insert(block, state);
    }
    if escaped.is_empty() {
        Walk::Done(rewrite)
    } else {
        Walk::Escaped(escaped)
    }
}

/// Project a virtual field into the load's representation without adding a
/// speculative check. An incompatible typed read keeps the actual object and
/// its observing store, whose dependency exit owns that mismatch.
fn virtual_load(
    graph: &mut Graph,
    rewrite: &mut Rewrite,
    load: NodeId,
    mut value: NodeId,
) -> Option<NodeId> {
    while let Some(&next) = rewrite.replace.get(&value) {
        value = next;
    }
    let required = graph.node(load).repr;
    let stored = graph.node(value);
    let (kind, input) = match (required, stored.repr, &stored.kind) {
        (target, actual, _) if target == actual => return Some(value),
        (Repr::Tagged, Repr::Float64, _) => (Kind::Float64ToTagged, value),
        (Repr::Tagged, Repr::Int32, _) => (Kind::Int32ToTagged, value),
        (Repr::Float64, Repr::Int32, _) => (Kind::Int32ToFloat64, value),
        (Repr::Float64, Repr::Tagged, Kind::Float64ToTagged) => return Some(stored.inputs[0]),
        (Repr::Float64, Repr::Tagged, Kind::Int32ToTagged) => {
            (Kind::Int32ToFloat64, stored.inputs[0])
        }
        (Repr::Int32, Repr::Tagged, Kind::Int32ToTagged) => return Some(stored.inputs[0]),
        (Repr::Float64, Repr::Tagged, Kind::ConstTagged(bits)) => {
            let number = otter_vm::Value::from_bits(*bits).as_f64()?;
            // This walk can roll back the node arena. Do not publish a
            // provisional constant into Graph's permanent interning table.
            return Some(graph.add_node(Kind::ConstFloat64(number.to_bits()), &[], Repr::Float64));
        }
        (Repr::Int32, Repr::Tagged, Kind::ConstTagged(bits)) => {
            let number = otter_vm::Value::from_bits(*bits).as_i32()?;
            return Some(graph.add_node(Kind::ConstInt32(number), &[], Repr::Int32));
        }
        _ => return None,
    };
    let conversion = graph.add_node(kind, &[input], required);
    let (pc, origin) = (graph.node(load).pc, graph.node(load).origin);
    graph.node_mut(conversion).pc = pc;
    graph.node_mut(conversion).origin = origin;
    rewrite.conversions.push((load, conversion));
    Some(conversion)
}

/// A block's entry: its one predecessor's exit, or the merge of every
/// forward predecessor's exit with the phis that merge needs.
enum Entry {
    State(State),
    Merge(State, Vec<(NodeId, usize, SmallVec<[NodeId; 2]>, Repr)>),
}

fn entry_state(
    graph: &Graph,
    block: BlockId,
    exits: &FxHashMap<BlockId, State>,
    escaped: &mut FxHashSet<NodeId>,
) -> Option<Entry> {
    let predecessors = &graph.block(block).predecessors;
    // A loop header enters from its forward predecessors; its back edge
    // carries no write to an object allocated before the loop.
    let forward: SmallVec<[BlockId; 2]> = if graph.block(block).is_loop {
        predecessors[..predecessors.len().saturating_sub(1)]
            .iter()
            .copied()
            .collect()
    } else {
        predecessors.iter().copied().collect()
    };
    let states: SmallVec<[&State; 2]> = forward
        .iter()
        .map(|predecessor| exits.get(predecessor))
        .collect::<Option<_>>()?;
    match states[..] {
        [] => None,
        [single] => Some(Entry::State(single.clone())),
        [first, ..] => {
            let mut merged = State::default();
            let mut merges = Vec::new();
            for (&candidate, object) in first {
                let others: SmallVec<[&Object; 2]> = states[1..]
                    .iter()
                    .filter_map(|state| state.get(&candidate))
                    .collect();
                if others.len() + 1 != states.len() {
                    // Not allocated on every path: unreachable past the merge.
                    continue;
                }
                if others.iter().any(|other| {
                    other.shape != object.shape || other.fields.len() != object.fields.len()
                }) {
                    escaped.insert(candidate);
                    continue;
                }
                for slot in 0..object.fields.len() {
                    let inputs: SmallVec<[NodeId; 2]> = std::iter::once(object.fields[slot])
                        .chain(others.iter().map(|other| other.fields[slot]))
                        .collect();
                    if inputs.iter().all(|&input| input == inputs[0]) {
                        continue;
                    }
                    let repr = graph.node(inputs[0]).repr;
                    if inputs.iter().any(|&input| graph.node(input).repr != repr)
                        || graph.block(block).is_loop
                    {
                        escaped.insert(candidate);
                        continue;
                    }
                    merges.push((candidate, slot, inputs, repr));
                }
                merged.insert(candidate, object.clone());
            }
            Some(Entry::Merge(merged, merges))
        }
    }
}

/// The live object of `receiver` when it is a candidate; a candidate not
/// live here escapes.
fn candidate_object<'s>(
    state: &'s State,
    candidates: &FxHashSet<NodeId>,
    receiver: NodeId,
    escaped: &mut FxHashSet<NodeId>,
) -> Option<&'s Object> {
    if !candidates.contains(&receiver) {
        return None;
    }
    let object = state.get(&receiver);
    if object.is_none() {
        escaped.insert(receiver);
    }
    object
}

/// Write `value` at `slot`: an existing slot or the next appended one.
fn write_slot(object: &mut Object, slot: usize, value: NodeId) -> bool {
    match slot.cmp(&object.fields.len()) {
        std::cmp::Ordering::Less => {
            object.fields[slot] = value;
            true
        }
        std::cmp::Ordering::Equal => {
            object.fields.push(value);
            true
        }
        std::cmp::Ordering::Greater => false,
    }
}

/// The logical slot of an in-object field.
fn inline_slot(field: FieldLocation) -> Option<usize> {
    field.is_inline().then(|| field.index() as usize)
}

/// The slot a named store at `byte_pc` writes on a receiver of `shape`, and
/// the receiver's hidden class after it. A store that also proves the
/// prototype chain does so only for a fresh receiver, whose allocation
/// already did.
fn store_effect(
    view: &JitCompileSnapshot,
    byte_pc: u32,
    shape: u32,
    fresh: bool,
) -> Option<(usize, u32)> {
    let program = receiver_program(view, byte_pc, shape)?;
    let mut slot = None;
    let mut next = shape;
    for op in program {
        match *op {
            JitCacheIrOp::GuardShape { object: 0, .. }
            | JitCacheIrOp::GuardExtensible { object: 0, .. } => {}
            JitCacheIrOp::GuardPrototypeValidity { .. } if fresh => {}
            JitCacheIrOp::StoreField { object: 0, field } => slot = Some(inline_slot(field)?),
            JitCacheIrOp::PublishShape { object: 0, shape } => next = shape,
            _ => return None,
        }
    }
    Some((slot?, next))
}

/// The slot a named load at `byte_pc` reads from a receiver of `shape`
/// itself.
fn own_load_slot(view: &JitCompileSnapshot, byte_pc: u32, shape: u32) -> Option<usize> {
    let program = receiver_program(view, byte_pc, shape)?;
    match program {
        [
            JitCacheIrOp::GuardShape { object: 0, .. },
            JitCacheIrOp::LoadField { object: 0, field },
        ] => inline_slot(*field),
        _ => None,
    }
}

/// The ops of the program at `byte_pc` that guards a receiver of `shape`.
fn receiver_program(
    view: &JitCompileSnapshot,
    byte_pc: u32,
    shape: u32,
) -> Option<&[JitCacheIrOp]> {
    view.property_programs
        .get(&byte_pc)?
        .iter()
        .map(|program| &program.ops[..])
        .find(|ops| {
            matches!(ops.first(), Some(JitCacheIrOp::GuardShape { object: 0, shape: guarded }) if *guarded == shape)
        })
}

/// Rewrite the graph as `rewrite` decided.
fn apply(graph: &mut Graph, layout: &[BlockId], candidates: &FxHashSet<NodeId>, rewrite: Rewrite) {
    // Phis already have one owner in Graph; publication only places them.
    for &(block, phi) in &rewrite.phis {
        graph.node_mut(phi).block = Some(block);
        graph.block_mut(block).phis.push(phi);
    }
    let resolve = |replace: &FxHashMap<NodeId, NodeId>, mut value: NodeId| {
        while let Some(&next) = replace.get(&value) {
            value = next;
        }
        value
    };
    // Frame states first: a node's own copy of its eager chain names virtual
    // objects of its fields there.
    let mut snapshots: Vec<(NodeId, State)> = rewrite.snapshots.into_iter().collect();
    snapshots.sort_unstable_by_key(|(node, _)| *node);
    for (node, objects) in snapshots {
        let eager = graph
            .node(node)
            .eager
            .expect("a snapshot has an eager state");
        let mut virtuals: FxHashMap<NodeId, NodeId> = FxHashMap::default();
        for &candidate in objects.keys() {
            virtual_object(
                graph,
                candidate,
                &objects,
                &rewrite.replace,
                &mut virtuals,
                &mut Vec::new(),
            );
        }
        let copy = copy_chain(graph, eager, &|value| {
            let value = resolve(&rewrite.replace, value);
            virtuals.get(&value).copied().unwrap_or(value)
        });
        graph.node_mut(node).eager = Some(copy);
    }
    for &block in layout {
        let mut body = Vec::new();
        for node in graph.block(block).body.clone() {
            for &(_, conversion) in rewrite
                .conversions
                .iter()
                .filter(|&&(before, _)| before == node)
            {
                graph.node_mut(conversion).block = Some(block);
                body.push(conversion);
            }
            if !rewrite.remove.contains(&node) && !rewrite.replace.contains_key(&node) {
                body.push(node);
            }
        }
        graph.block_mut(block).body = body;
    }
    for (&node, &cell) in &rewrite.cell_reads {
        let data = graph.node_mut(node);
        data.kind = Kind::LoadCellValue(cell);
        data.inputs.clear();
        data.eager = None;
    }
    for &candidate in candidates {
        let data = graph.node_mut(candidate);
        let Kind::NewReceiver(plan) = data.kind else {
            unreachable!("a candidate is a receiver allocation");
        };
        data.kind = Kind::CheckReceiverPlan(plan);
        data.repr = Repr::None;
    }
    // Every remaining read of a load reads its value.
    for node in 0..graph.nodes.len() {
        let inputs = graph.nodes[node].inputs.clone();
        for (index, input) in inputs.into_iter().enumerate() {
            let value = resolve(&rewrite.replace, input);
            if value != input {
                graph.nodes[node].inputs[index] = value;
            }
        }
    }
    for state in 0..graph.frame_states.len() {
        graph.frame_states[state]
            .for_each_value_mut(|value| *value = resolve(&rewrite.replace, *value));
    }
}

/// The virtual object of `candidate` with the fields of `objects`, its
/// nested candidates made first.
fn virtual_object(
    graph: &mut Graph,
    candidate: NodeId,
    objects: &State,
    replace: &FxHashMap<NodeId, NodeId>,
    virtuals: &mut FxHashMap<NodeId, NodeId>,
    making: &mut Vec<NodeId>,
) -> Option<NodeId> {
    if let Some(&made) = virtuals.get(&candidate) {
        return Some(made);
    }
    let object = objects.get(&candidate)?;
    if making.contains(&candidate) {
        return None;
    }
    making.push(candidate);
    let mut fields: SmallVec<[NodeId; 8]> = SmallVec::new();
    for &field in &object.fields {
        let mut field = field;
        while let Some(&next) = replace.get(&field) {
            field = next;
        }
        let field = if objects.contains_key(&field) {
            virtual_object(graph, field, objects, replace, virtuals, making)?
        } else {
            field
        };
        fields.push(field);
    }
    making.pop();
    let made = graph.add_node(
        Kind::VirtualObject {
            shape: object.shape,
        },
        &fields,
        Repr::Tagged,
    );
    virtuals.insert(candidate, made);
    Some(made)
}

/// A copy of `state`'s chain with every value mapped by `map`.
fn copy_chain(
    graph: &mut Graph,
    state: FrameStateId,
    map: &dyn Fn(NodeId) -> NodeId,
) -> FrameStateId {
    let mut copy = graph.frame_state(state).clone();
    if let Some(caller) = &mut copy.caller {
        caller.state = copy_chain(graph, caller.state, map);
    }
    copy.for_each_value_mut(|value| *value = map(*value));
    graph.add_frame_state(copy)
}

/// Replace matching numeric box/unbox pairs or widen an Int32 source.
/// Exact int32 Number checks are independent of the input's physical encoding;
/// peeling a Double box keeps the exact narrowing and its eager recovery.
pub(crate) fn fold_conversions(graph: &mut Graph, layout: &[BlockId]) {
    let mut replace: FxHashMap<NodeId, NodeId> = FxHashMap::default();
    let mut convert: Vec<(NodeId, Kind, NodeId)> = Vec::new();
    for &block in layout {
        for &node in &graph.block(block).body {
            let data = graph.node(node);
            let Some(&input) = data.inputs.first() else {
                continue;
            };
            let boxed = graph.node(input);
            match (&data.kind, &boxed.kind) {
                (Kind::CheckedTaggedToFloat64, Kind::Float64ToTagged)
                | (Kind::CheckedNumberToInt32, Kind::Int32ToTagged) => {
                    replace.insert(node, boxed.inputs[0]);
                }
                (Kind::CheckedTaggedToFloat64, Kind::Int32ToTagged) => {
                    convert.push((node, Kind::Int32ToFloat64, boxed.inputs[0]));
                }
                (Kind::CheckedNumberToInt32, Kind::Float64ToTagged) => {
                    convert.push((node, Kind::CheckedFloat64ToInt32, boxed.inputs[0]));
                }
                _ => {}
            }
        }
    }
    for (node, kind, value) in convert {
        let data = graph.node_mut(node);
        // A widening never leaves; an exact narrowing keeps its exit.
        if kind == Kind::Int32ToFloat64 {
            data.eager = None;
        }
        data.kind = kind;
        data.inputs = [value].into_iter().collect();
    }
    if replace.is_empty() {
        return;
    }
    for &block in layout {
        let body: Vec<NodeId> = graph
            .block(block)
            .body
            .iter()
            .copied()
            .filter(|node| !replace.contains_key(node))
            .collect();
        graph.block_mut(block).body = body;
    }
    for node in 0..graph.nodes.len() {
        for index in 0..graph.nodes[node].inputs.len() {
            let input = graph.nodes[node].inputs[index];
            if let Some(&value) = replace.get(&input) {
                graph.nodes[node].inputs[index] = value;
            }
        }
    }
    for state in 0..graph.frame_states.len() {
        graph.frame_states[state].for_each_value_mut(|value| {
            if let Some(&replacement) = replace.get(value) {
                *value = replacement;
            }
        });
    }
}

/// Drop the pure body values no node and no frame state reads, until none
/// is left. Phis stay: a loop header's state may name them.
pub(crate) fn remove_dead_values(graph: &mut Graph, layout: &[BlockId]) {
    loop {
        let mut used: FxHashSet<NodeId> = FxHashSet::default();
        for &block in layout {
            let data = graph.block(block);
            for &node in data
                .phis
                .iter()
                .chain(&data.body)
                .chain(data.control.iter())
            {
                let node_data = graph.node(node);
                used.extend(node_data.inputs.iter().copied());
                for state in [node_data.eager, node_data.lazy].into_iter().flatten() {
                    used.extend(graph.state_values(state));
                }
            }
        }
        let mut removed = false;
        for &block in layout {
            let dead = |node: &NodeId| {
                let data = graph.node(*node);
                let properties = data.kind.properties();
                data.repr != Repr::None
                    && !properties.effectful
                    && !properties.eager_deopt
                    && !properties.lazy_deopt
                    && !properties.call
                    && !used.contains(node)
            };
            let keep: Vec<NodeId> = graph
                .block(block)
                .body
                .iter()
                .copied()
                .filter(|node| !dead(node))
                .collect();
            if keep.len() != graph.block(block).body.len() {
                removed = true;
                graph.block_mut(block).body = keep;
            }
        }
        if !removed {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::ir::FrameState;
    use super::*;
    use crate::field_store::FieldStoreOwner;

    #[test]
    fn numeric_virtual_fields_preserve_typed_values_recovery_and_required_observation() {
        for (observing, read_repr) in [
            (false, Repr::Tagged),
            (false, Repr::Float64),
            (true, Repr::Float64),
            (false, Repr::Int32),
        ] {
            let mut graph = Graph::default();
            let block = graph.new_block();
            let place = |graph: &mut Graph, kind, inputs: &[NodeId], repr| {
                let node = graph.add_node(kind, inputs, repr);
                graph.node_mut(node).block = Some(block);
                graph.block_mut(block).body.push(node);
                node
            };
            let callee = graph.constant(Kind::ConstTagged(otter_vm::Value::undefined().to_bits()));
            let value = graph.constant(Kind::ConstFloat64((-0.0f64).to_bits()));
            let receiver = place(
                &mut graph,
                Kind::NewReceiver(otter_vm::jit::JitReceiverAllocationPlan {
                    new_target_function_id: 1,
                    new_target_is_class: false,
                    family_id: 1,
                    family_cell: 8,
                    receiver_shape: 128,
                    initial_field_count: 1,
                    inline_capacity: 1,
                    prototype_validity: None,
                    prototype_root: 16,
                }),
                &[callee],
                Repr::Tagged,
            );
            let owners = if observing {
                smallvec::smallvec![(
                    128,
                    FieldStoreOwner {
                        owner_shape: 128,
                        stored_shapes: Default::default(),
                        nullish_mask: 0,
                        int32: true,
                        double: false,
                        int32_constant: None,
                    }
                )]
            } else {
                SmallVec::new()
            };
            let field = FieldLocation::inline(0);
            let store = place(
                &mut graph,
                Kind::StoreDoubleOwnField {
                    field,
                    atom: 1,
                    owners,
                },
                &[receiver, value],
                Repr::None,
            );
            let load = place(
                &mut graph,
                if read_repr == Repr::Float64 {
                    Kind::LoadNumberOwnField {
                        field,
                        atom: 1,
                        double_only: true,
                    }
                } else {
                    Kind::LoadOwnField { field, atom: 1 }
                },
                &[receiver],
                read_repr,
            );
            let state = graph.add_frame_state(FrameState {
                pc: 3,
                byte_pc: 12,
                function_id: 1,
                register_count: 1,
                registers: vec![(0, receiver)],
                caller: None,
            });
            let check = place(&mut graph, Kind::CheckDependencies, &[], Repr::None);
            graph.node_mut(check).eager = Some(state);
            let returned = graph.add_node(Kind::Return, &[load], Repr::None);
            graph.node_mut(returned).block = Some(block);
            graph.block_mut(block).control = Some(returned);
            let view = JitCompileSnapshot::without_feedback(1, 0, 0, vec![]);
            eliminate_allocations(&mut graph, &[block], &[], &view, &[]);
            let elided = !observing && read_repr != Repr::Int32;
            assert_eq!(
                matches!(graph.node(receiver).kind, Kind::CheckReceiverPlan(_)),
                elided
            );
            assert_eq!(graph.block(block).body.contains(&store), !elided);
            if elided {
                let result = graph.node(returned).inputs[0];
                if read_repr == Repr::Float64 {
                    assert_eq!(result, value);
                } else {
                    assert_eq!(graph.node(result).kind, Kind::Float64ToTagged);
                    assert_eq!(graph.node(result).inputs.as_slice(), &[value]);
                    let narrow = place(
                        &mut graph,
                        Kind::CheckedNumberToInt32,
                        &[result],
                        Repr::Int32,
                    );
                    graph.node_mut(narrow).eager = Some(state);
                    fold_conversions(&mut graph, &[block]);
                    assert_eq!(
                        graph.node(narrow).kind,
                        Kind::CheckedFloat64ToInt32,
                        "the peeled numeric check must still reject -0"
                    );
                    assert_eq!(graph.node(narrow).inputs.as_slice(), &[value]);
                    assert_eq!(graph.node(narrow).eager, Some(state));
                }
                let virtual_object = graph
                    .frame_state(graph.node(check).eager.unwrap())
                    .registers[0]
                    .1;
                assert!(matches!(
                    graph.node(virtual_object).kind,
                    Kind::VirtualObject { shape: 128 }
                ));
                assert_eq!(graph.node(virtual_object).inputs.as_slice(), &[value]);
            }
        }
    }
}
