//! Back-edge countdowns carried in a register through leaf loops.
//!
//! Every loop back edge polls for interrupts and charges the work budget by
//! counting down the isolate's shared countdown cell. A load, decrement and
//! store of that cell on every iteration puts a memory round trip on the
//! loop's critical path. A loop that cannot reach the cell any other way
//! reads it once on entry, counts down in a register, lets its back edge poll
//! test the register, and writes the count back where it leaves.
//!
//! # Contents
//! - [`carry_in_registers`] — rewrite every eligible loop.
//!
//! # Invariants
//! - Only an innermost loop whose nodes cannot call, collect, throw or leave
//!   through a lazy exit carries its countdown: nothing inside it reads or
//!   writes the cell, so the register is the countdown for as long as the
//!   loop runs. An eager exit leaves without writing it back, which can only
//!   undercount the batch the next poll charges.
//! - Every entry into the loop (the OSR entry included) reads the cell; the
//!   header's phi merges those reads with the decremented back-edge values.
//!   A loop entered somewhere other than its header keeps the shared cell.
//! - The poll tests the decremented count; after the poll re-armed the cell,
//!   the register reloads it. An interrupt zeroes the cell, so it is seen at
//!   the latest when the register's batch runs out.
//! - Every edge out of the loop writes the header's count back, on a block of
//!   its own when its target is also entered from outside the loop.
//!
//! # See also
//! - [`super::licm::loop_body`] — the blocks of a loop.
//! - `otter_vm::Interpreter::jit_backedge_poll` — charges and re-arms.

use rustc_hash::FxHashSet;
use smallvec::SmallVec;

use super::builder::LoopHeader;
use super::ir::{BlockId, Graph, Kind, NodeId, Repr};
use super::licm::loop_body;

/// Rewrite every eligible loop of `loop_headers` to carry its countdown in a
/// register.
pub(crate) fn carry_in_registers(
    graph: &mut Graph,
    layout: &mut Vec<BlockId>,
    loop_headers: &[LoopHeader],
) {
    for header in loop_headers {
        let header = header.block;
        let body = loop_body(graph, header);
        if let Some(plan) = plan(graph, header, &body) {
            rewrite(graph, layout, header, &body, &plan);
        }
    }
}

/// The edges of one eligible loop.
struct Plan {
    /// Header predecessors outside the loop.
    entries: SmallVec<[BlockId; 2]>,
    /// Blocks whose control is the loop's back edge.
    latches: SmallVec<[BlockId; 2]>,
    /// `(source, target)` edges leaving the loop.
    exits: SmallVec<[(BlockId, BlockId); 4]>,
}

fn plan(graph: &Graph, header: BlockId, body: &FxHashSet<BlockId>) -> Option<Plan> {
    let mut latches = SmallVec::new();
    let mut exits = SmallVec::new();
    for &block in body {
        let data = graph.block(block);
        if block != header
            && data
                .predecessors
                .iter()
                .any(|predecessor| !body.contains(predecessor))
        {
            return None;
        }
        for &node in &data.body {
            let properties = graph.node(node).kind.properties();
            if properties.call || properties.may_collect || properties.can_throw {
                return None;
            }
            if properties.lazy_deopt || graph.node(node).lazy.is_some() {
                return None;
            }
        }
        let control = data.control?;
        let targets: SmallVec<[BlockId; 2]> = match graph.node(control).kind {
            Kind::JumpLoop(target) => {
                if target != header || !graph.node(control).inputs.is_empty() {
                    return None;
                }
                latches.push(block);
                continue;
            }
            Kind::Jump(target) => smallvec::smallvec![target],
            Kind::Branch {
                if_true, if_false, ..
            } => {
                if if_true == if_false {
                    return None;
                }
                smallvec::smallvec![if_true, if_false]
            }
            Kind::Return | Kind::Deopt(_) => SmallVec::new(),
            _ => return None,
        };
        for target in targets {
            if target == header {
                return None;
            }
            if !body.contains(&target) {
                exits.push((block, target));
            }
        }
    }
    let entries: SmallVec<[BlockId; 2]> = graph
        .block(header)
        .predecessors
        .iter()
        .copied()
        .filter(|predecessor| !body.contains(predecessor))
        .collect();
    if latches.is_empty() || entries.is_empty() {
        return None;
    }
    latches.sort_unstable();
    exits.sort_unstable();
    Some(Plan {
        entries,
        latches,
        exits,
    })
}

fn place(graph: &mut Graph, block: BlockId, node: NodeId, at_start: bool) {
    graph.node_mut(node).block = Some(block);
    let body = &mut graph.block_mut(block).body;
    if at_start {
        body.insert(0, node);
    } else {
        body.push(node);
    }
}

fn rewrite(
    graph: &mut Graph,
    layout: &mut Vec<BlockId>,
    header: BlockId,
    body: &FxHashSet<BlockId>,
    plan: &Plan,
) {
    let control = graph.block(header).control.expect("a loop header ends");
    graph.position = graph.node(control).pc;
    graph.origin = 0;
    let phi = graph.add_node(Kind::Phi, &[], Repr::Word);
    graph.node_mut(phi).block = Some(header);
    graph.block_mut(header).phis.push(phi);
    let mut inputs: SmallVec<[NodeId; 3]> = SmallVec::new();
    for predecessor in graph.block(header).predecessors.clone() {
        let value = if plan.entries.contains(&predecessor) {
            let load = graph.add_node(Kind::LoadBackedgeFuel, &[], Repr::Word);
            place(graph, predecessor, load, false);
            load
        } else {
            debug_assert!(plan.latches.contains(&predecessor));
            let decrement = graph.add_node(Kind::DecrementBackedgeFuel, &[phi], Repr::Word);
            place(graph, predecessor, decrement, false);
            let back_edge = graph
                .block(predecessor)
                .control
                .expect("a latch ends in its back edge");
            graph.node_mut(back_edge).inputs = [decrement].into_iter().collect();
            decrement
        };
        inputs.push(value);
    }
    graph.node_mut(phi).inputs = inputs;
    for &(source, target) in &plan.exits {
        let store = graph.add_node(Kind::StoreBackedgeFuel, &[phi], Repr::None);
        let shared = graph
            .block(target)
            .predecessors
            .iter()
            .any(|predecessor| !body.contains(predecessor));
        let shared = shared
            || plan
                .exits
                .iter()
                .filter(|&&(other, exit)| exit == target && other != source)
                .count()
                > 0;
        if !shared {
            place(graph, target, store, true);
            continue;
        }
        // The target is also entered from elsewhere: the write gets a block
        // of its own on this edge.
        let landing = graph.new_block();
        place(graph, landing, store, false);
        let jump = graph.add_node(Kind::Jump(target), &[], Repr::None);
        graph.node_mut(jump).block = Some(landing);
        graph.block_mut(landing).control = Some(jump);
        graph.block_mut(landing).predecessors = [source].into_iter().collect();
        let control = graph.block(source).control.expect("an exit ends its block");
        match &mut graph.node_mut(control).kind {
            Kind::Jump(next) if *next == target => *next = landing,
            Kind::Branch {
                if_true, if_false, ..
            } => {
                if *if_true == target {
                    *if_true = landing;
                } else if *if_false == target {
                    *if_false = landing;
                }
            }
            other => unreachable!("an exit edge ends in {other:?}"),
        }
        for predecessor in &mut graph.block_mut(target).predecessors {
            if *predecessor == source {
                *predecessor = landing;
                break;
            }
        }
        let position = layout
            .iter()
            .position(|&block| block == target)
            .expect("an exit target is laid out");
        layout.insert(position, landing);
    }
}
