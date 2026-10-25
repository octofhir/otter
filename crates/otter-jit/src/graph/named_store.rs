//! Consume numeric SSA at a geometry-certified static append.
//!
//! # Contents
//! - [`refine`] upgrades a selected Tagged append after phi representation and
//!   constant folding expose its pure numeric input.
//!
//! # Invariants
//! - Selection and geometry stay in the one source program.
//! - Every ordinary field accepts a Value-valid encoded numeric word.
//! - No checked conversion, heap effect or new speculation is introduced.
//! - The original source position and eager recovery state remain unchanged.
//!
//! # See also
//! - [`super::phi_repr`] selects representations before this consumer runs.
//! - [`super::builder_field_type`] selects the actual physical store summary.

use std::sync::Arc;

use otter_vm::JitCompileSnapshot;
use rustc_hash::FxHashMap;

use super::ir::{BlockId, Graph, Kind, NamedStorePlan, NodeId, Repr};

/// Return an existing Float64 source or a pure numeric constant. Int32 inputs
/// retain their canonical tagged representation.
fn numeric_input(graph: &mut Graph, value: NodeId) -> Option<NodeId> {
    let data = graph.node(value);
    match (&data.kind, data.repr) {
        (_, Repr::Float64) => Some(value),
        (Kind::Float64ToTagged, Repr::Tagged) => Some(data.inputs[0]),
        (Kind::ConstTagged(bits), Repr::Tagged) => {
            let value = otter_vm::Value::from_bits(*bits);
            if value.is_int32() {
                return None;
            }
            let number = value.as_number()?.as_f64();
            Some(graph.constant(Kind::ConstFloat64(number.to_bits())))
        }
        _ => None,
    }
}

/// Refine existing selected plans, returning whether unused numeric boxes may
/// now be removed. This never selects a different source program or proof.
pub(crate) fn refine(
    graph: &mut Graph,
    layout: &[BlockId],
    view: &JitCompileSnapshot,
    inline_views: &[Arc<JitCompileSnapshot>],
) -> bool {
    let mut changed = false;
    let mut recovery_sources = FxHashMap::default();
    for &block in layout {
        for node in graph.block(block).body.clone() {
            let Kind::StoreNamedProperty {
                byte_pc,
                plan: NamedStorePlan::TransitionTagged(index),
                receiver_shape,
                proved,
                ..
            } = graph.node(node).kind
            else {
                continue;
            };
            let source = super::metadata::source_view(view, inline_views, graph, node);
            let Some(_program) =
                super::feedback::store_node_programs(source, byte_pc, receiver_shape, proved)
                    .and_then(|programs| programs.get(usize::from(index)))
                    .filter(|program| super::feedback::inline_store_transition(program))
            else {
                continue;
            };
            let original = graph.node(node).inputs[1];
            let Some(value) = numeric_input(graph, original) else {
                continue;
            };
            let recovered = if graph.node(original).kind == Kind::Float64ToTagged {
                graph.node(original).inputs[0]
            } else {
                original
            };
            recovery_sources.insert(original, recovered);
            let data = graph.node_mut(node);
            let Kind::StoreNamedProperty { plan, .. } = &mut data.kind else {
                unreachable!()
            };
            *plan = NamedStorePlan::TransitionDouble(index);
            data.inputs[1] = value;
            changed = true;
        }
    }
    if !changed {
        return false;
    }
    // A purely numeric RHS cannot create a heap edge. This also removes a
    // barrier introduced while its Phi was still Tagged during graph building.
    for &block in layout {
        let mut kept = Vec::new();
        for node in graph.block(block).body.clone() {
            let data = graph.node(node);
            if data.kind == Kind::WriteBarrier && recovery_sources.contains_key(&data.inputs[1]) {
                graph.node_mut(node).block = None;
            } else {
                kept.push(node);
            }
        }
        graph.block_mut(block).body = kept;
    }
    // Recovery canonically boxes numeric SSA already. Keep ordinary Tagged
    // consumers untouched, while a recovery-only box leaves the hot path.
    for state in &mut graph.frame_states {
        state.for_each_value_mut(|value| {
            if let Some(&source) = recovery_sources.get(value) {
                *value = source;
            }
        });
    }
    changed
}
