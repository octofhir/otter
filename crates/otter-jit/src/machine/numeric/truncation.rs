//! Word32 truncation of Number and int32 addition and subtraction.
//!
//! # Contents
//! - [`optimize`] rewrites `ToInt32(t)` where `t` is a tree of Number `+` / `-`
//!   over int32 leaves (widened int32 values and integral int32 constants)
//!   into the same tree of wrapping int32 operations, the rewrite V8's
//!   representation selection performs for a Word32-truncating use.
//! - [`wrap_truncated_integer_arithmetic`] turns an overflow-checked int32
//!   `+` / `-` whose every reader truncates to Word32 (a bitwise operator, a
//!   shift, another such `+` / `-`) into its wrapping form, V8's lowering of a
//!   `SpeculativeSafeIntegerAdd` whose uses all truncate.
//!
//! # Invariants
//! - A tree has at most [`MAX_DEPTH`] levels, so every intermediate double is
//!   an exact integer below 2^53; ToInt32 of an exact sum of int32 values is
//!   the sum modulo 2^32, which the wrapping operations compute.
//! - Only the ToInt32 node is replaced. The Number tree stays for any other
//!   reader (a frame state reconstructs the exact double); Machine dead-code
//!   elimination drops it once nothing reads it.
//! - New nodes are inserted immediately before the replaced node in its block,
//!   so every operand is defined before its use.
//! - A checked `+` / `-` is rewritten only when no frame state names its
//!   value (deoptimization would otherwise materialize the wrapped value) and
//!   its wrapping tree stays within [`MAX_DEPTH`] levels, so the interpreter's
//!   exact double arithmetic after any deoptimization computes the same
//!   Word32.
//!
//! # See also
//! - `super::hir` builds the `FloatToInt32` sites for bitwise operators.
//! - `super::super::dce` removes the abandoned Number tree.

use super::hir::{NumericFunction, NumericNode, NumericValue};

/// Tree depth bound: 2^MAX_DEPTH int32 leaves sum below 2^(31 + MAX_DEPTH),
/// inside the 2^53 exact-integer range of a double.
const MAX_DEPTH: u32 = 16;

#[derive(Debug, Clone)]
enum Term {
    Int32(NumericValue),
    Constant(i32),
    Add(Box<Term>, Box<Term>),
    Sub(Box<Term>, Box<Term>),
}

pub(super) fn optimize(hir: &mut NumericFunction) -> usize {
    let mut rewritten = 0;
    for block in 0..hir.blocks.len() {
        let mut position = 0;
        while position < hir.blocks[block].nodes.len() {
            let value = hir.blocks[block].nodes[position];
            let NumericNode::FloatToInt32(source) = hir.nodes[value.0] else {
                position += 1;
                continue;
            };
            let Some(term) = number_term(&hir.nodes, source, 0) else {
                position += 1;
                continue;
            };
            let (Term::Add(left, right) | Term::Sub(left, right)) = &term else {
                position += 1;
                continue;
            };
            let mut inserted = Vec::new();
            let left = materialize(hir, left, &mut inserted);
            let right = materialize(hir, right, &mut inserted);
            hir.nodes[value.0] = match term {
                Term::Add(..) => NumericNode::IntegerAddWrapping(left, right),
                _ => NumericNode::IntegerSubWrapping(left, right),
            };
            let count = inserted.len();
            hir.blocks[block].nodes.splice(position..position, inserted);
            position += count + 1;
            rewritten += 1;
        }
    }
    rewritten
}

/// The int32 arithmetic tree whose exact double value `value` holds.
fn number_term(nodes: &[NumericNode], value: NumericValue, depth: u32) -> Option<Term> {
    match nodes.get(value.0)? {
        NumericNode::WidenInt32(source) => Some(Term::Int32(*source)),
        NumericNode::Constant(constant) => {
            let integral = *constant as i32;
            (f64::from(integral) == *constant && !(integral == 0 && constant.is_sign_negative()))
                .then_some(Term::Constant(integral))
        }
        NumericNode::Add(left, right) if depth < MAX_DEPTH => Some(Term::Add(
            Box::new(number_term(nodes, *left, depth + 1)?),
            Box::new(number_term(nodes, *right, depth + 1)?),
        )),
        NumericNode::Sub(left, right) if depth < MAX_DEPTH => Some(Term::Sub(
            Box::new(number_term(nodes, *left, depth + 1)?),
            Box::new(number_term(nodes, *right, depth + 1)?),
        )),
        _ => None,
    }
}

fn materialize(
    hir: &mut NumericFunction,
    term: &Term,
    inserted: &mut Vec<NumericValue>,
) -> NumericValue {
    let node = match term {
        Term::Int32(value) => return *value,
        Term::Constant(constant) => NumericNode::IntegerConstant(*constant),
        Term::Add(left, right) => {
            let left = materialize(hir, left, inserted);
            let right = materialize(hir, right, inserted);
            NumericNode::IntegerAddWrapping(left, right)
        }
        Term::Sub(left, right) => {
            let left = materialize(hir, left, inserted);
            let right = materialize(hir, right, inserted);
            NumericNode::IntegerSubWrapping(left, right)
        }
    };
    let value = NumericValue(hir.nodes.len());
    hir.nodes.push(node);
    inserted.push(value);
    value
}

/// Rewrite every overflow-checked int32 `+` / `-` whose readers all truncate
/// to Word32 into the wrapping operation, and drop its overflow frame state.
pub(super) fn wrap_truncated_integer_arithmetic(hir: &mut NumericFunction) -> usize {
    use super::frame_state::{NumericFramePoint, NumericFrameSlot};
    use super::hir::NumericTerminator;
    let count = hir.nodes.len();
    let mut users = vec![Vec::new(); count];
    for (index, &node) in hir.nodes.iter().enumerate() {
        if super::boxed_arithmetic::visit_inputs(hir, node, |value, _| {
            if let Some(list) = users.get_mut(value.0) {
                list.push(index);
            }
        })
        .is_none()
        {
            return 0;
        }
    }
    for block in &hir.blocks {
        for value in block.successor_arguments.iter().flatten() {
            users[value.0].push(usize::MAX);
        }
        match block.terminator {
            NumericTerminator::Branch { condition, .. } => users[condition.0].push(usize::MAX),
            NumericTerminator::Return(value) | NumericTerminator::Throw(value) => {
                users[value.0].push(usize::MAX);
            }
            NumericTerminator::Jump => {}
        }
    }
    for state in &hir.frame_states {
        for slot in state.frame_slots() {
            if let NumericFrameSlot::Value(value) = slot {
                users[value.0].push(usize::MAX);
            }
        }
    }
    let mut convertible = hir
        .nodes
        .iter()
        .map(|node| {
            matches!(
                node,
                NumericNode::IntegerAdd(..)
                    | NumericNode::IntegerSub(..)
                    | NumericNode::IntegerAddImmediate(..)
                    | NumericNode::IntegerSubImmediate(..)
            )
        })
        .collect::<Vec<_>>();
    // A node leaving the tree (a non-truncating reader, or the depth bound)
    // makes its operands' readers non-truncating in turn: iterate both
    // prunings to one common fixed point.
    loop {
        prune_non_truncated(hir, &users, &mut convertible);
        if !prune_deep(hir, &mut convertible) {
            break;
        }
    }
    let mut rewritten = 0;
    for block in 0..hir.blocks.len() {
        let mut position = 0;
        while position < hir.blocks[block].nodes.len() {
            let value = hir.blocks[block].nodes[position];
            if !convertible[value.0] {
                position += 1;
                continue;
            }
            let immediate = match hir.nodes[value.0] {
                NumericNode::IntegerAddImmediate(_, constant)
                | NumericNode::IntegerSubImmediate(_, constant) => {
                    let node = NumericValue(hir.nodes.len());
                    hir.nodes.push(NumericNode::IntegerConstant(constant));
                    hir.blocks[block].nodes.insert(position, node);
                    position += 1;
                    Some(node)
                }
                _ => None,
            };
            hir.nodes[value.0] = match (hir.nodes[value.0], immediate) {
                (NumericNode::IntegerAdd(left, right), _) => {
                    NumericNode::IntegerAddWrapping(left, right)
                }
                (NumericNode::IntegerSub(left, right), _) => {
                    NumericNode::IntegerSubWrapping(left, right)
                }
                (NumericNode::IntegerAddImmediate(source, _), Some(constant)) => {
                    NumericNode::IntegerAddWrapping(source, constant)
                }
                (NumericNode::IntegerSubImmediate(source, _), Some(constant)) => {
                    NumericNode::IntegerSubWrapping(source, constant)
                }
                (node, _) => node,
            };
            position += 1;
            rewritten += 1;
        }
    }
    if rewritten != 0 {
        let nodes = &hir.nodes;
        hir.frame_states.retain(|state| match state.point {
            NumericFramePoint::Node(value) => nodes[value.0].frame_state_purpose().is_some(),
            NumericFramePoint::Backedge { .. } => true,
        });
    }
    rewritten
}

/// Readers of an overflow-checked `+` / `-` that consume only its Word32.
fn truncates(node: NumericNode) -> bool {
    matches!(
        node,
        NumericNode::IntegerAnd(..)
            | NumericNode::IntegerOr(..)
            | NumericNode::IntegerXor(..)
            | NumericNode::IntegerNot(..)
            | NumericNode::IntegerAndImmediate(..)
            | NumericNode::IntegerShiftLeft(..)
            | NumericNode::IntegerShiftRight(..)
            | NumericNode::IntegerShiftRightLogical(..)
            | NumericNode::IntegerAddWrapping(..)
            | NumericNode::IntegerSubWrapping(..)
    )
}

/// Drop every candidate with a reader that is neither truncating nor a
/// candidate itself, to a fixed point. `users` holds `usize::MAX` for a
/// terminator, block argument or frame-state read.
fn prune_non_truncated(hir: &NumericFunction, users: &[Vec<usize>], convertible: &mut [bool]) {
    loop {
        let mut changed = false;
        for index in 0..convertible.len() {
            if !convertible[index] {
                continue;
            }
            let truncated = !users[index].is_empty()
                && users[index].iter().all(|&user| {
                    user != usize::MAX && (convertible[user] || truncates(hir.nodes[user]))
                });
            if !truncated {
                convertible[index] = false;
                changed = true;
            }
        }
        if !changed {
            return;
        }
    }
}

/// Drop every candidate whose wrapping tree exceeds [`MAX_DEPTH`] levels.
/// Returns whether any candidate was dropped.
fn prune_deep(hir: &NumericFunction, convertible: &mut [bool]) -> bool {
    let mut depth = vec![0u32; convertible.len()];
    let mut dropped = false;
    // Depths only grow, and a node past the bound leaves the tree, so the
    // fixed point is reached within `MAX_DEPTH + 1` sweeps.
    for _ in 0..=MAX_DEPTH {
        let mut changed = false;
        for index in 0..convertible.len() {
            if !convertible[index] {
                continue;
            }
            let operands = match hir.nodes[index] {
                NumericNode::IntegerAdd(left, right) | NumericNode::IntegerSub(left, right) => {
                    [Some(left), Some(right)]
                }
                NumericNode::IntegerAddImmediate(source, _)
                | NumericNode::IntegerSubImmediate(source, _) => [Some(source), None],
                _ => [None, None],
            };
            let level = 1 + operands
                .into_iter()
                .flatten()
                .filter(|operand| convertible[operand.0])
                .map(|operand| depth[operand.0])
                .max()
                .unwrap_or(0);
            if level > MAX_DEPTH {
                convertible[index] = false;
                dropped = true;
                changed = true;
            } else if level != depth[index] {
                depth[index] = level;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    dropped
}
