//! Global redundancy elimination: a value, load or check that a dominating
//! node with the same operation and inputs already produced, over memory no
//! path between them writes, is that node.
//!
//! The pass walks the dominator tree with a table of available nodes. A node
//! whose operation, payload, inputs and representation match a live entry is
//! replaced by it: its uses, frame states included, read the dominating node
//! and it leaves its block. Otherwise a node that may be reused becomes
//! available until a write that overlaps what it reads. Entering a block,
//! the writes of every block on a path from its immediate dominator to it
//! (the block itself when it heads a loop) retire the entries they overlap.
//!
//! # Contents
//! - [`eliminate`] — replace every redundant node in `layout`; a field or
//!   context-slot store's value also answers later reads of the same word.
//!
//! # Invariants
//! - A node whose payload names a site of the body it was built for (a
//!   byte pc into that body's feedback and proofs) matches only nodes of
//!   the same body.
//! - Only nodes that write nothing, cannot call, collect, throw or deopt
//!   lazily, and are values or guards take part. A guard that passed with
//!   the same inputs over the same memory passes again; a load over the same
//!   memory reads the same word; a value of the same inputs is the same.
//! - A store's value stands for a later read of exactly the word it wrote
//!   (the same object or base input, field or offset), in the read's
//!   representation: the tagged value, or the int32 it tagged.
//! - What a node reads and writes is [`effects::clobberize`]; an entry lives
//!   only while nothing that overlaps its reads may have run since it.
//! - Nodes whose value differs between two runs with the same inputs (the
//!   back-edge countdown, entry registers, phis, allocations, native leaves)
//!   and the subscription proofs [`super::dependency_checks`] places never
//!   take part.
//! - A replaced node's eager or lazy state goes with it: the dominating node
//!   ran first, so the state it was checked against already held.
//!
//! # See also
//! - [`super::effects`] — the alias model.
//! - [`super::dominators`] — the tree the walk follows.
//! - [`super::licm`] — moves invariant nodes out of loops before this pass.

use rustc_hash::{FxHashMap, FxHashSet};
use smallvec::SmallVec;

use super::dominators::Dominators;
use super::effects::{self, AbstractHeap, HeapSet};
use super::ir::{BlockId, Graph, Kind, NodeId, Repr};

/// The identity of an available node, short of its payload: the operation,
/// its inputs and representation, and the body whose source view a
/// source-relative payload indexes.
type Key = (
    std::mem::Discriminant<Kind>,
    SmallVec<[NodeId; 3]>,
    Repr,
    u16,
);

/// Whether `kind` means the same thing in every inlined body: its payload
/// names no site of the body it was built for (a byte pc indexes that
/// body's own feedback and proofs).
fn origin_free(kind: &Kind) -> bool {
    matches!(
        kind,
        Kind::Int32Add
            | Kind::Int32Sub
            | Kind::Int32AddWrapping
            | Kind::Int32SubWrapping
            | Kind::Int32Mul
            | Kind::Int32MulExact
            | Kind::Int32MulIdentifyZeros
            | Kind::Int32Div
            | Kind::Int32Mod
            | Kind::Int32Negate
            | Kind::Int32BitAnd
            | Kind::Int32BitOr
            | Kind::Int32BitXor
            | Kind::Int32BitNot
            | Kind::Int32ShiftLeft
            | Kind::Int32ShiftRight
            | Kind::Int32ShiftRightLogical
            | Kind::Uint32ShiftRightToFloat64
            | Kind::Int32Compare(_)
            | Kind::Float64Add
            | Kind::Float64Sub
            | Kind::Float64Mul
            | Kind::Float64Div
            | Kind::Float64Negate
            | Kind::Float64Compare(_)
            | Kind::Int32ToTagged
            | Kind::Float64ToTagged
            | Kind::Int32ToFloat64
            | Kind::TruncateFloat64ToInt32
            | Kind::CheckedNumberToInt32
            | Kind::CheckedTaggedToFloat64
            | Kind::CheckedFloat64ToInt32
            | Kind::CheckedTaggedToIndex
            | Kind::CheckedFloat64ToIndex
            | Kind::BooleanToInt32
            | Kind::CheckBounds
            | Kind::CheckNumber
            | Kind::CheckNotHole
            | Kind::LoadReceiverShape
            | Kind::CheckShapes { .. }
            | Kind::CheckElements { .. }
            | Kind::CheckElementPresent
            | Kind::LoadPresentElement
            | Kind::LoadShapedProperty { .. }
            | Kind::LoadOwnField { .. }
            | Kind::LoadNumberOwnField { .. }
            | Kind::LoadTaggedField(_)
            | Kind::LoadContextParent
            | Kind::LoadClosureContext
            | Kind::LoadArrayLength { .. }
            | Kind::LoadElementsLength { .. }
            | Kind::LoadElementsBase { .. }
            | Kind::LoadElement(_)
            | Kind::StringCharCodeAt
    )
}

/// The table key of `node`.
fn key(graph: &Graph, node: NodeId) -> Key {
    let data = graph.node(node);
    let origin = if origin_free(&data.kind) {
        0
    } else {
        data.origin
    };
    (
        std::mem::discriminant(&data.kind),
        data.inputs.clone(),
        data.repr,
        origin,
    )
}

struct Entry {
    /// The node that stands for later matches.
    node: NodeId,
    /// The operation and table key a later node matches: the node's own,
    /// or for a value a store made available, the load that reads it.
    kind: Kind,
    key: Key,
    reads: SmallVec<[AbstractHeap; 2]>,
    live: bool,
}

/// The loads a store makes answerable by its value: `(load kind, input,
/// representation, value)`. A field store's value answers a tagged read of
/// the same field of the same object, and its untagged int32 an int32
/// read; a context-slot store answers a read of the same slot.
fn forwarded(graph: &Graph, node: NodeId) -> SmallVec<[(Kind, NodeId, Repr, NodeId); 2]> {
    let data = graph.node(node);
    let mut out = SmallVec::new();
    match data.kind {
        Kind::StoreDoubleOwnField { field, atom, .. } => {
            for double_only in [false, true] {
                out.push((
                    Kind::LoadNumberOwnField {
                        field,
                        atom,
                        double_only,
                    },
                    data.inputs[0],
                    Repr::Float64,
                    data.inputs[1],
                ));
            }
        }
        Kind::StoreOwnField { field, atom, .. } => {
            let (object, value) = (data.inputs[0], data.inputs[1]);
            let load = Kind::LoadOwnField { field, atom };
            if graph.node(value).repr == Repr::Tagged {
                out.push((load.clone(), object, Repr::Tagged, value));
            }
            let stored = graph.node(value);
            if stored.kind == Kind::Int32ToTagged
                && graph.node(stored.inputs[0]).repr == Repr::Int32
            {
                out.push((load, object, Repr::Int32, stored.inputs[0]));
            }
        }
        Kind::StoreTaggedField(offset) => {
            let (base, value) = (data.inputs[0], data.inputs[1]);
            if graph.node(value).repr == Repr::Tagged {
                out.push((Kind::LoadTaggedField(offset), base, Repr::Tagged, value));
            }
        }
        _ => {}
    }
    out
}

/// Whether `kind` may stand for another node of the same operation and
/// inputs, or be replaced by one.
fn reusable(graph: &Graph, node: NodeId) -> bool {
    let data = graph.node(node);
    match data.kind {
        Kind::Phi
        | Kind::InitialRegister(_)
        | Kind::LoadBackedgeFuel
        | Kind::DecrementBackedgeFuel
        | Kind::StoreBackedgeFuel
        | Kind::Generic { .. }
        | Kind::VirtualObject { .. }
        | Kind::AllocationProjection(_)
        | Kind::NativeLeaf(_)
        | Kind::CheckDependencies
        | Kind::CheckStableShapes(_)
        | Kind::CheckFieldTypes(_)
        | Kind::CheckContextSlot { .. }
        | Kind::CheckClosureContext(_) => return false,
        _ => {}
    }
    let effects = effects::clobberize(&data.kind);
    let properties = data.kind.properties();
    effects.writes.is_empty()
        && !properties.call
        && !properties.lazy_deopt
        && !properties.can_throw
        && !properties.may_collect
        && (data.repr != Repr::None || effects.guard)
}

/// The heaps the nodes of `block` write.
fn block_writes(graph: &Graph, block: BlockId) -> HeapSet {
    let mut writes = HeapSet::default();
    let data = graph.block(block);
    for &node in data.body.iter().chain(data.control.iter()) {
        for heap in effects::clobberize(&graph.node(node).kind).writes {
            writes.add(heap);
        }
    }
    writes
}

/// The heaps a path from the end of `dominator` to the start of `block`
/// may write: the writes of every block such a path passes, `block`
/// included when a path returns to it.
fn entry_kills(
    graph: &Graph,
    writes: &FxHashMap<BlockId, HeapSet>,
    dominator: BlockId,
    block: BlockId,
) -> HeapSet {
    let mut kills = HeapSet::default();
    let predecessors = &graph.block(block).predecessors;
    if predecessors.as_slice() == [dominator] {
        return kills;
    }
    let mut seen: FxHashSet<BlockId> = FxHashSet::default();
    let mut work: Vec<BlockId> = predecessors
        .iter()
        .copied()
        .filter(|&p| p != dominator)
        .collect();
    while let Some(current) = work.pop() {
        if !seen.insert(current) {
            continue;
        }
        if let Some(set) = writes.get(&current) {
            for heap in set.iter() {
                kills.add(heap);
            }
        }
        if current == block {
            continue;
        }
        for &predecessor in &graph.block(current).predecessors {
            if predecessor != dominator {
                work.push(predecessor);
            }
        }
    }
    kills
}

/// Replace every node of `layout` a dominating equivalent makes redundant.
/// Returns how many nodes left the graph.
pub(crate) fn eliminate(graph: &mut Graph, layout: &[BlockId]) -> usize {
    let dominators = Dominators::compute(graph, layout);
    let mut children: FxHashMap<BlockId, Vec<BlockId>> = FxHashMap::default();
    let mut roots = Vec::new();
    for &block in layout {
        match dominators.immediate(block) {
            Some(parent) => children.entry(parent).or_default().push(block),
            None if dominators.reachable(block) => roots.push(block),
            None => {}
        }
    }
    let writes: FxHashMap<BlockId, HeapSet> = layout
        .iter()
        .map(|&block| (block, block_writes(graph, block)))
        .collect();
    let mut entries: Vec<Entry> = Vec::new();
    let mut index: FxHashMap<Key, Vec<usize>> = FxHashMap::default();
    let mut replaced: FxHashMap<NodeId, NodeId> = FxHashMap::default();
    // Depth-first over the tree; each frame undoes its block's changes to
    // the table before its siblings run.
    enum Step {
        Enter(BlockId, Option<BlockId>),
        Leave { added: usize, killed: Vec<usize> },
    }
    let mut stack: Vec<Step> = roots
        .iter()
        .rev()
        .map(|&root| Step::Enter(root, None))
        .collect();
    while let Some(step) = stack.pop() {
        match step {
            Step::Leave { added, killed } => {
                for slot in killed {
                    entries[slot].live = true;
                }
                while entries.len() > added {
                    let entry = entries.pop().expect("an added entry");
                    if let Some(slots) = index.get_mut(&entry.key) {
                        slots.pop();
                    }
                }
            }
            Step::Enter(block, parent) => {
                let added = entries.len();
                let mut killed = Vec::new();
                let mut kill = |entries: &mut Vec<Entry>, heaps: &HeapSet| {
                    if heaps.is_empty() {
                        return;
                    }
                    for (slot, entry) in entries.iter_mut().enumerate() {
                        if entry.live && entry.reads.iter().any(|&read| heaps.overlaps(read)) {
                            entry.live = false;
                            killed.push(slot);
                        }
                    }
                };
                if let Some(parent) = parent {
                    let kills = entry_kills(graph, &writes, parent, block);
                    kill(&mut entries, &kills);
                } else if graph.block(block).is_loop {
                    kill(&mut entries, &writes[&block]);
                }
                let body = graph.block(block).body.clone();
                let mut kept = Vec::with_capacity(body.len());
                for node in body {
                    let inputs: SmallVec<[NodeId; 3]> = graph
                        .node(node)
                        .inputs
                        .iter()
                        .map(|input| *replaced.get(input).unwrap_or(input))
                        .collect();
                    graph.node_mut(node).inputs = inputs;
                    let effects = effects::clobberize(&graph.node(node).kind);
                    if reusable(graph, node) {
                        let data = graph.node(node);
                        let key = key(graph, node);
                        let found = index.get(&key).and_then(|slots| {
                            slots
                                .iter()
                                .rev()
                                .map(|&slot| &entries[slot])
                                .find(|entry| entry.live && entry.kind == data.kind)
                        });
                        if let Some(entry) = found {
                            replaced.insert(node, entry.node);
                            continue;
                        }
                        let kind = data.kind.clone();
                        index.entry(key.clone()).or_default().push(entries.len());
                        entries.push(Entry {
                            node,
                            kind,
                            key,
                            reads: effects.reads.clone(),
                            live: true,
                        });
                    } else if !effects.writes.is_empty() {
                        let mut heaps = HeapSet::default();
                        for heap in effects.writes {
                            heaps.add(heap);
                        }
                        kill(&mut entries, &heaps);
                        // What the store wrote is what the same word reads
                        // until the next overlapping write.
                        for (load, input, repr, value) in forwarded(graph, node) {
                            let reads = effects::clobberize(&load).reads;
                            let key: Key = (
                                std::mem::discriminant(&load),
                                smallvec::smallvec![input],
                                repr,
                                0,
                            );
                            index.entry(key.clone()).or_default().push(entries.len());
                            entries.push(Entry {
                                node: value,
                                kind: load,
                                key,
                                reads,
                                live: true,
                            });
                        }
                    }
                    kept.push(node);
                }
                graph.block_mut(block).body = kept;
                if let Some(control) = graph.block(block).control {
                    let inputs: SmallVec<[NodeId; 3]> = graph
                        .node(control)
                        .inputs
                        .iter()
                        .map(|input| *replaced.get(input).unwrap_or(input))
                        .collect();
                    graph.node_mut(control).inputs = inputs;
                }
                stack.push(Step::Leave { added, killed });
                if let Some(blocks) = children.get(&block) {
                    for &child in blocks.iter().rev() {
                        stack.push(Step::Enter(child, Some(block)));
                    }
                }
            }
        }
    }
    if replaced.is_empty() {
        return 0;
    }
    // A replacement may itself have been replaced by a dominating node.
    let resolve = |mut node: NodeId| {
        while let Some(&next) = replaced.get(&node) {
            node = next;
        }
        node
    };
    for &block in layout {
        let data = graph.block(block).clone();
        for node in data
            .phis
            .iter()
            .chain(&data.body)
            .chain(data.control.iter())
        {
            let inputs: SmallVec<[NodeId; 3]> = graph
                .node(*node)
                .inputs
                .iter()
                .map(|&input| resolve(input))
                .collect();
            graph.node_mut(*node).inputs = inputs;
        }
    }
    for state in &mut graph.frame_states {
        state.for_each_value_mut(|value| *value = resolve(*value));
    }
    for node in &mut graph.nodes {
        if let Kind::VirtualObject { .. } = node.kind {
            for input in &mut node.inputs {
                *input = resolve(*input);
            }
        }
    }
    replaced.len()
}

#[cfg(test)]
#[path = "redundancy_tests.rs"]
mod tests;
