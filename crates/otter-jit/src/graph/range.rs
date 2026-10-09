//! Int32 value ranges and the arithmetic checks they rule out.
//!
//! # Contents
//! - [`narrow_checked_arithmetic`] — a forward range analysis of the int32
//!   nodes (the integer subset of TurboFan's typer) and the rewrite of
//!   checked additions, subtractions and multiplications that cannot
//!   overflow, or produce `-0`, into their unchecked forms.
//!
//! # Invariants
//! - A node's range bounds every value it produces on any execution that
//!   reaches it. A checked operation deopts instead of producing an
//!   out-of-range value, so its range is its exact result's range clamped to
//!   int32; an unchecked one keeps the exact range only when that fits.
//! - Phis merge the ranges of their inputs. A loop phi starts from its
//!   entry inputs; a back edge that widens it moves the widened bound
//!   straight to the int32 limit, so the analysis settles in a few passes.
//! - A rewritten node computes the exact result its checked form would have
//!   produced on every execution, up to the sign of a zero no use tells
//!   apart, so frame states may record it; one that no longer deopts drops
//!   its eager state.
//!
//! # See also
//! - [`super::truncation`] — wraps the checked additions whose uses only
//!   truncate, which ranges cannot prove exact.

use rustc_hash::FxHashMap;

use super::ir::{BlockId, Graph, Kind, NodeId, Repr};
use otter_vm::jit::JitElementRepr;

/// An inclusive interval of int32 values, held wide so arithmetic on two
/// bounds is exact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Range {
    lo: i64,
    hi: i64,
}

const INT32: Range = Range {
    lo: i32::MIN as i64,
    hi: i32::MAX as i64,
};

impl Range {
    const fn new(lo: i64, hi: i64) -> Self {
        Self { lo, hi }
    }

    fn fits(self) -> bool {
        self.lo >= INT32.lo && self.hi <= INT32.hi
    }

    /// The range of a checked operation whose exact result is `self`.
    fn clamped(self) -> Self {
        Self::new(self.lo.max(INT32.lo), self.hi.min(INT32.hi))
    }

    /// The range of an unchecked operation whose exact result is `self`.
    fn wrapped(self) -> Self {
        if self.fits() { self } else { INT32 }
    }

    fn union(self, other: Self) -> Self {
        Self::new(self.lo.min(other.lo), self.hi.max(other.hi))
    }

    fn contains(self, value: i64) -> bool {
        self.lo <= value && value <= self.hi
    }

    fn add(self, other: Self) -> Self {
        Self::new(self.lo + other.lo, self.hi + other.hi)
    }

    fn sub(self, other: Self) -> Self {
        Self::new(self.lo - other.hi, self.hi - other.lo)
    }

    fn mul(self, other: Self) -> Self {
        let products = [
            self.lo * other.lo,
            self.lo * other.hi,
            self.hi * other.lo,
            self.hi * other.hi,
        ];
        Self::new(
            *products.iter().min().expect("four products"),
            *products.iter().max().expect("four products"),
        )
    }

    /// Whether a product of values in `self` and `other` can be `-0`: one
    /// factor zero and the other negative.
    fn product_may_be_minus_zero(self, other: Self) -> bool {
        (self.contains(0) && other.lo < 0) || (other.contains(0) && self.lo < 0)
    }

    /// `[0, 2^k - 1]` for the least `k` that covers `hi`.
    fn bits_up_to(hi: i64) -> Self {
        let bits = 64 - (hi.max(0) as u64).leading_zeros();
        Self::new(0, (1i64 << bits) - 1)
    }
}

/// The range a node produces from the ranges of its inputs.
fn transfer(graph: &Graph, node: NodeId, ranges: &FxHashMap<NodeId, Range>) -> Option<Range> {
    let data = graph.node(node);
    if data.repr != Repr::Int32 {
        return None;
    }
    let input = |index: usize| -> Range {
        data.inputs
            .get(index)
            .and_then(|&input| range_of(graph, input, ranges))
            .unwrap_or(INT32)
    };
    let constant = |index: usize| -> Option<i32> {
        match graph.node(*data.inputs.get(index)?).kind {
            Kind::ConstInt32(value) => Some(value),
            _ => None,
        }
    };
    Some(match &data.kind {
        Kind::ConstInt32(value) => Range::new(i64::from(*value), i64::from(*value)),
        Kind::Phi => {
            let mut merged: Option<Range> = None;
            for &input in &data.inputs {
                if let Some(range) = range_of(graph, input, ranges) {
                    merged = Some(merged.map_or(range, |merged| merged.union(range)));
                }
            }
            merged?
        }
        Kind::Int32Add => input(0).add(input(1)).clamped(),
        Kind::Int32Sub => input(0).sub(input(1)).clamped(),
        Kind::Int32Mul | Kind::Int32MulIdentifyZeros => input(0).mul(input(1)).clamped(),
        Kind::Int32AddWrapping => input(0).add(input(1)).wrapped(),
        Kind::Int32SubWrapping => input(0).sub(input(1)).wrapped(),
        Kind::Int32MulExact => input(0).mul(input(1)),
        Kind::Int32Negate => Range::new(-input(0).hi, -input(0).lo).clamped(),
        Kind::Int32Div => {
            let magnitude = input(0).lo.abs().max(input(0).hi.abs());
            Range::new(-magnitude, magnitude).clamped()
        }
        Kind::Int32Mod => {
            let divisor = input(1).lo.abs().max(input(1).hi.abs()).max(1) - 1;
            let dividend = input(0);
            Range::new(
                if dividend.lo >= 0 {
                    0
                } else {
                    -divisor.min(-dividend.lo)
                },
                if dividend.hi <= 0 {
                    0
                } else {
                    divisor.min(dividend.hi)
                },
            )
        }
        Kind::Int32BitAnd => {
            let (a, b) = (input(0), input(1));
            match (a.lo >= 0, b.lo >= 0) {
                (true, true) => Range::new(0, a.hi.min(b.hi)),
                (true, false) => Range::new(0, a.hi),
                (false, true) => Range::new(0, b.hi),
                (false, false) => INT32,
            }
        }
        Kind::Int32BitOr | Kind::Int32BitXor => {
            let (a, b) = (input(0), input(1));
            if a.lo >= 0 && b.lo >= 0 {
                let bits = Range::bits_up_to(a.hi.max(b.hi));
                if data.kind == Kind::Int32BitOr {
                    Range::new(a.lo.max(b.lo), bits.hi)
                } else {
                    bits
                }
            } else {
                INT32
            }
        }
        Kind::Int32BitNot => Range::new(-input(0).hi - 1, -input(0).lo - 1),
        Kind::Int32ShiftLeft => match constant(1) {
            Some(count) => {
                let count = count & 31;
                Range::new(input(0).lo << count, input(0).hi << count).wrapped()
            }
            None => INT32,
        },
        Kind::Int32ShiftRight => {
            let value = input(0);
            match constant(1) {
                Some(count) => {
                    let count = count & 31;
                    Range::new(value.lo >> count, value.hi >> count)
                }
                None => Range::new(value.lo.min(0), value.hi.max(0)),
            }
        }
        Kind::Int32ShiftRightLogical => {
            // The result is checked to fit int32.
            let value = input(0);
            match constant(1) {
                Some(count) if count & 31 != 0 => {
                    let count = count & 31;
                    if value.lo >= 0 {
                        Range::new(value.lo >> count, value.hi >> count)
                    } else {
                        Range::new(0, i64::from(u32::MAX >> count))
                    }
                }
                _ if value.lo >= 0 => value,
                _ => Range::new(0, INT32.hi),
            }
        }
        Kind::LoadElement(repr) => match repr {
            JitElementRepr::Int8 => Range::new(i64::from(i8::MIN), i64::from(i8::MAX)),
            JitElementRepr::Uint8 | JitElementRepr::Uint8Clamped => {
                Range::new(0, i64::from(u8::MAX))
            }
            JitElementRepr::Int16 => Range::new(i64::from(i16::MIN), i64::from(i16::MAX)),
            JitElementRepr::Uint16 => Range::new(0, i64::from(u16::MAX)),
            JitElementRepr::Uint32 => Range::new(0, INT32.hi),
            _ => INT32,
        },
        Kind::BooleanToInt32 => Range::new(0, 1),
        _ => INT32,
    })
}

fn range_of(graph: &Graph, node: NodeId, ranges: &FxHashMap<NodeId, Range>) -> Option<Range> {
    match graph.node(node).kind {
        Kind::ConstInt32(value) => Some(Range::new(i64::from(value), i64::from(value))),
        _ => ranges.get(&node).copied(),
    }
}

/// Compute the int32 ranges of the graph, then drop the overflow and
/// minus-zero checks they prove impossible. A multiplication in
/// `zero_insensitive` needs no minus-zero check at all.
pub(crate) fn narrow_checked_arithmetic(
    graph: &mut Graph,
    layout: &[BlockId],
    zero_insensitive: &rustc_hash::FxHashSet<NodeId>,
) {
    let mut ranges: FxHashMap<NodeId, Range> = FxHashMap::default();
    // Every pass visits the blocks in order, so a forward edge's value is
    // current and only back edges lag. A phi bound a back edge widens again
    // goes to the int32 limit, so each bound changes at most twice.
    let mut widened: FxHashMap<NodeId, Range> = FxHashMap::default();
    loop {
        let mut changed = false;
        for &block in layout {
            let data = graph.block(block);
            for &node in data.phis.iter().chain(&data.body) {
                let Some(mut range) = transfer(graph, node, &ranges) else {
                    continue;
                };
                let previous = ranges.get(&node).copied();
                if let Some(previous) = previous
                    && graph.node(node).kind == Kind::Phi
                    && range != previous
                {
                    let floor = widened.entry(node).or_insert(previous);
                    if range.lo < floor.lo {
                        range.lo = INT32.lo;
                    }
                    if range.hi > floor.hi {
                        range.hi = INT32.hi;
                    }
                    *floor = range;
                }
                if previous != Some(range) {
                    ranges.insert(node, range);
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }
    for &block in layout {
        let body = graph.block(block).body.clone();
        for node in body {
            let data = graph.node(node);
            let operands = |index: usize| {
                data.inputs
                    .get(index)
                    .and_then(|&input| range_of(graph, input, &ranges))
                    .unwrap_or(INT32)
            };
            let (a, b) = (operands(0), operands(1));
            let identifies_zeros = zero_insensitive.contains(&node);
            let narrowed = match data.kind {
                Kind::Int32Add if a.add(b).fits() => Kind::Int32AddWrapping,
                Kind::Int32Sub if a.sub(b).fits() => Kind::Int32SubWrapping,
                Kind::Int32Mul
                    if a.mul(b).fits() && (identifies_zeros || !a.product_may_be_minus_zero(b)) =>
                {
                    Kind::Int32MulExact
                }
                Kind::Int32Mul if identifies_zeros => Kind::Int32MulIdentifyZeros,
                _ => continue,
            };
            let data = graph.node_mut(node);
            data.kind = narrowed;
            if data.kind != Kind::Int32MulIdentifyZeros {
                data.eager = None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn products_of_masked_halves_fit_and_cannot_be_minus_zero() {
        let low = Range::new(0, 0x3fff);
        let high = Range::new(-(1 << 17), (1 << 17) - 1);
        assert!(low.mul(high).fits());
        assert!(!low.mul(low).product_may_be_minus_zero(low));
        assert!(low.product_may_be_minus_zero(high));
        assert!(!high.mul(high).fits());
    }

    #[test]
    fn unsigned_masks_bound_bitwise_results() {
        assert_eq!(Range::bits_up_to(0x3fff), Range::new(0, 0x3fff));
        assert_eq!(Range::bits_up_to(0x4000), Range::new(0, 0x7fff));
        assert_eq!(Range::bits_up_to(0), Range::new(0, 0));
    }
}
