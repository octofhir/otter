//! Word32 truncation of Number addition and subtraction.
//!
//! # Contents
//! - [`optimize`] rewrites `ToInt32(t)` where `t` is a tree of Number `+` / `-`
//!   over int32 leaves (widened int32 values and integral int32 constants)
//!   into the same tree of wrapping int32 operations, the rewrite V8's
//!   representation selection performs for a Word32-truncating use.
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
