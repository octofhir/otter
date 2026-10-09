//! Truthiness branches test the value their condition converts.
//!
//! A branch on `!x` or on `ToBoolean(x)` takes the same edges as a branch on
//! `x` itself, the first with its successors swapped: `Branch(Truthy)`
//! already applies ToBoolean. Rewriting the branch to read `x` leaves the
//! conversion without the branch as a user, and the allocator drops a pure
//! node no one reads, so `if (!(a instanceof B))` tests the instanceof answer
//! directly instead of materializing and negating it first (Maglev's
//! `BranchIfToBooleanTrue` over a `LogicalNot` input).
//!
//! # Contents
//! - [`fold_branch_negations`] — the rewrite over every block's branch.
//!
//! # Invariants
//! - Only `Branch { kind: Truthy }` changes, and only its input and the order
//!   of its two successors. The CFG's edges, and so every phi, stay as they
//!   were.
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
        if condition == graph.node(control).inputs[0] {
            continue;
        }
        let node = graph.node_mut(control);
        node.inputs[0] = condition;
        if negated
            && let Kind::Branch {
                if_true, if_false, ..
            } = &mut node.kind
        {
            std::mem::swap(if_true, if_false);
        }
    }
}
