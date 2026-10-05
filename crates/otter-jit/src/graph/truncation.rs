//! Truncated-use selection of wrapping int32 additions.
//!
//! # Contents
//! - [`wrap_truncated_arithmetic`] — turn a checked `Int32Add`/`Int32Sub`
//!   whose every use truncates its result into the two's-complement form.
//!
//! # Invariants
//! - Both operands are int32, so the exact sum or difference is an integer
//!   below 2^33 in magnitude and is exact as a double. `ToInt32` and
//!   `ToUint32` are additive modulo 2^32, so the wrapped word is what every
//!   truncating use computes from the exact result, also through a chain of
//!   wrapping additions (TurboFan's word32 truncation of `NumberAdd`).
//! - A use truncates when it is a bitwise or shift operand, or a wrapping
//!   addition itself selected by this pass. Comparisons, phis, boxes,
//!   conversions, element indices and stores observe the exact value.
//! - No frame state may record a wrapped value: a deopt resumes the
//!   interpreter with exact values, so a node any frame state captures keeps
//!   its overflow check.
//! - A selected node no longer deopts; it drops its eager frame state.
//!
//! # See also
//! - [`super::phi_repr`] — unboxes the phis these additions feed.
//! - [`super::builder`] — builds the checked forms from int32 feedback.

use rustc_hash::FxHashSet;

use super::ir::{BlockId, Graph, Kind, NodeId};

/// Whether `user` reads its input `index` through `ToInt32`/`ToUint32`.
fn truncates(kind: &Kind, wrapping: bool) -> bool {
    match kind {
        Kind::Int32BitAnd
        | Kind::Int32BitOr
        | Kind::Int32BitXor
        | Kind::Int32BitNot
        | Kind::Int32ShiftLeft
        | Kind::Int32ShiftRight
        | Kind::Int32ShiftRightLogical
        | Kind::Uint32ShiftRightToFloat64 => true,
        Kind::Int32Add | Kind::Int32Sub => wrapping,
        _ => false,
    }
}

/// Select every checked int32 addition whose uses all truncate it.
pub(crate) fn wrap_truncated_arithmetic(graph: &mut Graph, layout: &[BlockId]) {
    let mut observed: FxHashSet<NodeId> = FxHashSet::default();
    for state in &mut graph.frame_states {
        state.for_each_value_mut(|value| {
            observed.insert(*value);
        });
    }
    let mut candidates: FxHashSet<NodeId> = FxHashSet::default();
    let mut uses: Vec<(NodeId, NodeId)> = Vec::new();
    for &block in layout {
        let data = graph.block(block);
        for &node in data.phis.iter().chain(&data.body).chain(data.control.iter()) {
            let node_data = graph.node(node);
            if matches!(node_data.kind, Kind::Int32Add | Kind::Int32Sub) && !observed.contains(&node)
            {
                candidates.insert(node);
            }
            for &input in &node_data.inputs {
                uses.push((input, node));
            }
        }
    }
    if candidates.is_empty() {
        return;
    }
    // Drop candidates with an exact use until the selection is closed under
    // its own wrapping uses.
    loop {
        let mut dropped = false;
        for &(value, user) in &uses {
            if !candidates.contains(&value) {
                continue;
            }
            let wrapping = candidates.contains(&user);
            if !truncates(&graph.node(user).kind, wrapping) {
                candidates.remove(&value);
                dropped = true;
            }
        }
        if !dropped {
            break;
        }
    }
    for node in candidates {
        let data = graph.node_mut(node);
        data.kind = match data.kind {
            Kind::Int32Add => Kind::Int32AddWrapping,
            _ => Kind::Int32SubWrapping,
        };
        data.eager = None;
    }
}
