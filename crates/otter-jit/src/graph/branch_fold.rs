//! Reduce truthiness branches to the predicate their condition computed.
//!
//! A branch on `!x` or on `ToBoolean(x)` takes the same edges as a branch on
//! `x` itself, the first with its successors swapped: `Branch(Truthy)`
//! already applies ToBoolean. Rewriting the branch to read `x` leaves the
//! conversion without the branch as a user, and the allocator drops a pure
//! node no one reads, so `if (!(a instanceof B))` tests the instanceof answer
//! directly instead of materializing and negating it first. Typed numeric
//! comparisons feed their operands and condition directly to the control
//! node. Only the other consumers of the comparison need a tagged answer.
//!
//! # Contents
//! - [`fold_branch_negations`] — the rewrite over every block's branch.
//!
//! # Invariants
//! - Only `Branch { kind: Truthy }` changes. Predicate selection and
//!   successor swaps preserve every CFG edge and phi assignment.
//! - A conversion other nodes still read stays in place for them.
//!
//! # See also
//! - `regalloc::Allocator::allocate_node` — skips unused pure nodes.

use super::ir::{BranchKind, Graph, Kind};

/// Make every truthiness branch read the value under its chain of `!` and
/// ToBoolean conversions, swapping its successors once per `!`.
pub(crate) fn fold_branch_negations(graph: &mut Graph) {
    for index in 0..graph.blocks.len() {
        let Some(control) = graph.blocks[index].control else {
            continue;
        };
        let Kind::Branch {
            kind: BranchKind::Truthy,
            ..
        } = graph.node(control).kind
        else {
            continue;
        };
        let mut condition = graph.node(control).inputs[0];
        let mut negated = false;
        loop {
            match graph.node(condition).kind {
                Kind::LogicalNot => negated = !negated,
                Kind::ToBoolean => {}
                _ => break,
            }
            condition = graph.node(condition).inputs[0];
        }
        let predicate = graph.node(condition);
        let comparison = match predicate.kind {
            Kind::Int32Compare(condition) => Some(BranchKind::Int32(condition)),
            Kind::Float64Compare(condition) => Some(BranchKind::Float64(condition)),
            _ => None,
        };
        let inputs = if comparison.is_some() {
            predicate.inputs.clone()
        } else {
            smallvec::smallvec![condition]
        };
        let node = graph.node_mut(control);
        node.inputs = inputs;
        if let Kind::Branch {
            kind,
            if_true,
            if_false,
        } = &mut node.kind
        {
            if let Some(comparison) = comparison {
                *kind = comparison;
            }
            if negated {
                std::mem::swap(if_true, if_false);
            }
        }
    }
}
