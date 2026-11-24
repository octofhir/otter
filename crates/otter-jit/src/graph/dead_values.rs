//! Remove unused, effect-free Graph body values by marking their consumers.
//!
//! # Contents
//! - remove follows uses backwards from controls, phis, and body
//!   operations that may have observable effects.
//!
//! # Invariants
//! - Only pure body nodes can disappear. Every phi, control, check, and
//!   effectful operation keeps its placement.
//! - A visited reconstruction state keeps its frame-chain values and all
//!   virtual materialization inputs, even when no instruction uses them.
//! - Each node and each distinct referenced state is visited once.
//!
//! # See also
//! - super::ir::Graph::state_values defines the reconstruction inputs.

use super::ir::{BlockId, Graph, NodeId, Repr};

pub(crate) fn remove(graph: &mut Graph, layout: &[BlockId]) {
    let mut used = vec![false; graph.nodes.len()];
    let mut states = vec![false; graph.frame_states.len()];
    let mut work = Vec::<NodeId>::new();
    for &block in layout {
        let block = graph.block(block);
        work.extend(block.phis.iter().chain(&block.control).copied());
        work.extend(block.body.iter().copied().filter(|&id| {
            let node = graph.node(id);
            let props = node.kind.properties();
            node.repr == Repr::None
                || props.effectful
                || props.eager_deopt
                || props.lazy_deopt
                || props.call
        }));
    }

    while let Some(id) = work.pop() {
        if std::mem::replace(&mut used[id.0 as usize], true) {
            continue;
        }
        let node = graph.node(id);
        work.extend(node.inputs.iter().copied());
        for state in [node.eager, node.lazy].into_iter().flatten() {
            if !std::mem::replace(&mut states[state.0 as usize], true) {
                work.extend(graph.state_values(state));
            }
        }
    }

    for &block in layout {
        graph
            .block_mut(block)
            .body
            .retain(|node| used[node.0 as usize]);
    }
}
