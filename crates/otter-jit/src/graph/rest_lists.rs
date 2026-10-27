//! Rest lists that only feed calls are not allocated.
//!
//! `function f(a, ...args) { return g.apply(this, args); }` builds `args`
//! from its actual arguments only to have a call spread it again. When the
//! list never leaves the function — every use is the list input of a
//! [`Kind::CallApplyArray`], besides frame states — each such call forwards
//! the activation's own actuals past the formals ([`Kind::CallForward`] with
//! a `skip`), and the list itself becomes a [`Kind::VirtualRestArray`] a
//! deopt rebuilds from those actuals.
//!
//! # Contents
//! - [`elide_rest_lists`] — select the lists that do not escape and rewrite
//!   their calls and frame states.
//!
//! # Invariants
//! - A rest-parameter function's actuals never change: its `arguments` is
//!   unmapped, so assignments to formals leave them alone. Each forward
//!   still checks that no arguments object took them over.
//! - A list named by an inlined body's frame state, used by any other node
//!   or merged in a phi, keeps its allocation: only a single-frame deopt
//!   rebuilds it.
//!
//! # See also
//! - [`super::frame::deopt_frames`] turns the virtual list into its recipe.
//! - `crate::entry::runtime_ops::deopt` expands it from the frame's actuals.

use smallvec::SmallVec;

use super::ir::{BlockId, Graph, Kind, NodeId, Repr};

/// Rewrite every rest list of the compiled function whose only uses are
/// spreading calls and single-frame states.
pub(crate) fn elide_rest_lists(graph: &mut Graph, layout: &[BlockId]) {
    let lists: SmallVec<[(NodeId, u16, BlockId); 2]> = layout
        .iter()
        .flat_map(|&block| {
            graph
                .block(block)
                .body
                .iter()
                .map(move |&node| (node, block))
        })
        .filter_map(|(node, block)| match graph.node(node).kind {
            Kind::NewRestArray(formals) => Some((node, formals, block)),
            _ => None,
        })
        .collect();
    for (list, formals, home) in lists {
        let Some(calls) = spreading_calls(graph, layout, list) else {
            continue;
        };
        let rest = graph.add_node(Kind::VirtualRestArray(formals), &[], Repr::Tagged);
        for state in &mut graph.frame_states {
            state.for_each_value_mut(|value| {
                if *value == list {
                    *value = rest;
                }
            });
        }
        for (call, block) in calls {
            let data = graph.node(call);
            let Kind::CallApplyArray { pc } = data.kind else {
                unreachable!("a spreading call");
            };
            let (callee, receiver, eager) = (data.inputs[0], data.inputs[1], data.eager);
            let (source_pc, origin) = (data.pc, data.origin);
            let check = graph.add_node(Kind::CheckArgumentsElided, &[], Repr::None);
            {
                let node = graph.node_mut(check);
                node.eager = eager;
                node.block = Some(block);
                node.pc = source_pc;
                node.origin = origin;
            }
            let node = graph.node_mut(call);
            node.kind = Kind::CallForward {
                pc,
                plan: crate::call_linkage::CallPlan::Generic,
                bindings: Box::default(),
                skip: formals,
            };
            node.inputs = smallvec::smallvec![callee, receiver];
            let body = &mut graph.block_mut(block).body;
            let index = body
                .iter()
                .position(|&node| node == call)
                .expect("a placed call");
            body.insert(index, check);
        }
        graph.block_mut(home).body.retain(|&node| node != list);
    }
}

/// The calls spreading `list`, each with its block, when nothing else reads
/// it and no inlined body's frame state names it.
fn spreading_calls(
    graph: &Graph,
    layout: &[BlockId],
    list: NodeId,
) -> Option<SmallVec<[(NodeId, BlockId); 4]>> {
    let mut calls = SmallVec::new();
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
                if input != list {
                    continue;
                }
                if !matches!(node_data.kind, Kind::CallApplyArray { .. }) || index != 2 {
                    return None;
                }
                calls.push((node, block));
            }
            for state in [node_data.eager, node_data.lazy].into_iter().flatten() {
                if graph.frame_state(state).caller.is_some()
                    && graph.state_values(state).contains(&list)
                {
                    return None;
                }
            }
        }
    }
    (!calls.is_empty()).then_some(calls)
}
