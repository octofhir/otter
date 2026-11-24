//! Forward an ordinary field across a control-flow merge of completed stores.
//!
//! # Contents
//! - `forward` searches the incoming paths of guarded tagged own-field loads.
//! - `Search` proves a value for each predecessor and records any required phis.
//!
//! # Invariants
//! - Every leaf of the proof is a completed store of the same field and atom
//!   through the exact same receiver SSA value.
//! - Overlapping writes stop a search. The load's existing receiver/dependency
//!   guards stay in place. No new memory access or speculation is introduced.
//! - All incoming edges must be proved, including function and OSR entries.
//!   The search stops at a new definition of the receiver, including a loop
//!   phi. It declines unresolved cycles and has a work bound.
//! - Phis carry tagged values through the normal allocator and frame states.
//!
//! # See also
//! - `super::redundancy` handles dominating definitions.
//! - `super::effects` owns the overlapping-write relation.

use rustc_hash::{FxHashMap, FxHashSet};
use smallvec::SmallVec;

use super::effects::{AbstractHeap, HeapSet, clobberize};
use super::ir::{BlockId, Graph, Kind, NodeId, Repr};
use otter_vm::object::FieldLocation;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct Field {
    receiver: NodeId,
    bank: FieldLocation,
    atom: u32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Definition {
    StoreValue(NodeId),
    Phi(usize),
}

struct Merge {
    block: BlockId,
    inputs: SmallVec<[Definition; 3]>,
}

struct Search<'a> {
    graph: &'a Graph,
    field: Field,
    function_entry: BlockId,
    osr_entry: Option<BlockId>,
    /// The number of nodes/edges this one lookup can read.
    remaining: u16,
    /// An active query and an unavailable value both decline that path.
    exits: FxHashMap<BlockId, Option<Definition>>,
    merges: Vec<Merge>,
}

impl Search<'_> {
    fn entry(&mut self, block: BlockId) -> Option<Definition> {
        // An external entry has no predecessor store. A receiver defined
        // here may change on each loop iteration, even with the same SSA id.
        if block == self.function_entry
            || self.osr_entry == Some(block)
            || self.graph.node(self.field.receiver).block == Some(block)
        {
            return None;
        }
        let data = self.graph.block(block);
        self.remaining = self.remaining.checked_sub(1)?;
        let inputs: SmallVec<[Definition; 3]> = data
            .predecessors
            .iter()
            .map(|&predecessor| self.exit(predecessor))
            .collect::<Option<_>>()?;
        let first = *inputs.first()?;
        if inputs.iter().all(|&input| input == first) {
            return Some(first);
        }
        let index = self.merges.len();
        self.merges.push(Merge { block, inputs });
        Some(Definition::Phi(index))
    }

    fn exit(&mut self, block: BlockId) -> Option<Definition> {
        if let Some(&answer) = self.exits.get(&block) {
            return answer;
        }
        self.exits.insert(block, None);
        let answer = self.prove_exit(block);
        self.exits.insert(block, answer);
        answer
    }

    fn prove_exit(&mut self, block: BlockId) -> Option<Definition> {
        let data = self.graph.block(block);
        for &node in data.control.iter().chain(data.body.iter().rev()) {
            self.remaining = self.remaining.checked_sub(1)?;
            if node == self.field.receiver {
                return None;
            }
            let node = self.graph.node(node);
            if matches!(node.kind, Kind::StoreOwnField { field, atom, .. }
                if field == self.field.bank && atom == self.field.atom)
                && node.inputs[0] == self.field.receiver
                && self.graph.node(node.inputs[1]).repr == Repr::Tagged
            {
                return Some(Definition::StoreValue(node.inputs[1]));
            }
            if overwrites(&node.kind, self.field.atom) {
                return None;
            }
        }
        self.entry(block)
    }
}

fn overwrites(kind: &Kind, atom: u32) -> bool {
    clobberize(kind)
        .writes
        .iter()
        .any(|&heap| heap.overlaps(AbstractHeap::Field(atom)))
}

/// Replace a guarded tagged load only when different incoming stores supply it.
pub(super) fn forward(graph: &mut Graph, layout: &[BlockId], osr_entry: Option<BlockId>) {
    let Some(&function_entry) = layout.first() else {
        return;
    };
    let stored: FxHashSet<Field> = layout
        .iter()
        .flat_map(|&block| &graph.block(block).body)
        .filter_map(|&node| {
            let node = graph.node(node);
            let Kind::StoreOwnField { field, atom, .. } = node.kind else {
                return None;
            };
            (graph.node(node.inputs[1]).repr == Repr::Tagged).then_some(Field {
                receiver: node.inputs[0],
                bank: field,
                atom,
            })
        })
        .collect();
    if stored.is_empty() {
        return;
    }
    let mut replaced = FxHashMap::default();
    let source = (graph.position, graph.origin);
    for &block in layout {
        let body = graph.block(block).body.clone();
        let mut writes = HeapSet::default();
        for load in body {
            let data = graph.node(load);
            let Kind::LoadOwnField { field, atom } = data.kind else {
                for heap in clobberize(&data.kind).writes {
                    writes.add(heap);
                }
                continue;
            };
            let field = Field {
                receiver: data.inputs[0],
                bank: field,
                atom,
            };
            if data.repr != Repr::Tagged
                || !stored.contains(&field)
                || writes.overlaps(AbstractHeap::Field(atom))
            {
                continue;
            }
            let mut search = Search {
                graph,
                field,
                function_entry,
                osr_entry,
                remaining: 128,
                exits: FxHashMap::default(),
                merges: Vec::new(),
            };
            let Some(Definition::Phi(index)) = search.entry(block) else {
                continue;
            };
            let merges = search.merges;
            let pc = data.pc;
            let origin = data.origin;
            let mut phis = Vec::with_capacity(merges.len());
            for merge in merges {
                let inputs: SmallVec<[NodeId; 3]> = merge
                    .inputs
                    .iter()
                    .map(|input| match *input {
                        Definition::StoreValue(node) => node,
                        Definition::Phi(index) => phis[index],
                    })
                    .collect();
                graph.position = pc;
                graph.origin = origin;
                let phi = graph.add_node(Kind::Phi, &inputs, Repr::Tagged);
                graph.node_mut(phi).block = Some(merge.block);
                graph.block_mut(merge.block).phis.push(phi);
                phis.push(phi);
            }
            replaced.insert(load, phis[index]);
        }
    }
    (graph.position, graph.origin) = source;
    if replaced.is_empty() {
        return;
    }
    for &block in layout {
        graph
            .block_mut(block)
            .body
            .retain(|node| !replaced.contains_key(node));
    }
    super::redundancy::rewrite_uses(graph, layout, &replaced);
}
