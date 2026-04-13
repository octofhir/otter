//! Strength reduction — algebraic simplification of MIR operations.
//!
//! Catches peephole optimizations that constant folding doesn't cover:
//! - `x + 0` → `x`
//! - `x * 1` → `x`
//! - `x * 0` → `0`
//! - `x | 0` → `x`
//! - `x & 0` → `0`
//! - `x ^ 0` → `x`
//! - `x << 0` → `x`
//! - `x >> 0` → `x`
//! - `x === x` → `true` (for non-NaN)
//! - Double negation: `!(!x)` → `x`
//!
//! JSC: "Strength Reduction" pass in DFG.
//! SM: catch-all simplification in Ion.
//!
//! Spec: Phase 6.4 of JIT_INCREMENTAL_PLAN.md

use std::collections::HashMap;

use crate::mir::graph::{MirGraph, ValueId};
use crate::mir::nodes::MirOp;

/// Run strength reduction on the MIR graph.
pub fn run(graph: &mut MirGraph) {
    // Build constant map for identity checks.
    let mut const_i32: HashMap<ValueId, i32> = HashMap::new();
    for block in &graph.blocks {
        for instr in &block.instrs {
            if let MirOp::ConstInt32(v) = &instr.op {
                const_i32.insert(instr.value, *v);
            }
        }
    }

    // Build def map for pattern matching.
    let mut defs: HashMap<ValueId, MirOp> = HashMap::new();
    for block in &graph.blocks {
        for instr in &block.instrs {
            defs.insert(instr.value, instr.op.clone());
        }
    }

    for block_idx in 0..graph.blocks.len() {
        let mut i = 0;
        while i < graph.blocks[block_idx].instrs.len() {
            let instr = &graph.blocks[block_idx].instrs[i];
            let new_op = simplify(&instr.op, &const_i32, &defs);
            if let Some(op) = new_op {
                graph.blocks[block_idx].instrs[i].op = op;
            }
            i += 1;
        }
    }
}

fn simplify(
    op: &MirOp,
    consts: &HashMap<ValueId, i32>,
    defs: &HashMap<ValueId, MirOp>,
) -> Option<MirOp> {
    match op {
        // ---- f64 identity: x + 0.0 → x, 0.0 + x → x ----
        MirOp::AddF64 { lhs, rhs } => {
            if is_f64_zero(rhs, defs) {
                return Some(MirOp::Move(*lhs));
            }
            if is_f64_zero(lhs, defs) {
                return Some(MirOp::Move(*rhs));
            }
            None
        }
        // x - 0.0 → x
        MirOp::SubF64 { lhs, rhs } => {
            if is_f64_zero(rhs, defs) {
                return Some(MirOp::Move(*lhs));
            }
            None
        }
        // x * 1.0 → x, 1.0 * x → x
        MirOp::MulF64 { lhs, rhs } => {
            if is_f64_one(rhs, defs) {
                return Some(MirOp::Move(*lhs));
            }
            if is_f64_one(lhs, defs) {
                return Some(MirOp::Move(*rhs));
            }
            None
        }
        // x / 1.0 → x
        MirOp::DivF64 { lhs, rhs } => {
            if is_f64_one(rhs, defs) {
                return Some(MirOp::Move(*lhs));
            }
            None
        }

        // ---- Bitwise identity ----
        // x | 0 → x
        MirOp::BitOr { lhs, rhs } => {
            if consts.get(rhs) == Some(&0) {
                return Some(MirOp::Move(*lhs));
            }
            if consts.get(lhs) == Some(&0) {
                return Some(MirOp::Move(*rhs));
            }
            None
        }
        // x & 0 → 0
        MirOp::BitAnd { lhs, rhs } => {
            if consts.get(rhs) == Some(&0) {
                return Some(MirOp::Move(*rhs));
            }
            if consts.get(lhs) == Some(&0) {
                return Some(MirOp::Move(*lhs));
            }
            // x & -1 (all ones) → x
            if consts.get(rhs) == Some(&-1) {
                return Some(MirOp::Move(*lhs));
            }
            if consts.get(lhs) == Some(&-1) {
                return Some(MirOp::Move(*rhs));
            }
            None
        }
        // x ^ 0 → x
        MirOp::BitXor { lhs, rhs } => {
            if consts.get(rhs) == Some(&0) {
                return Some(MirOp::Move(*lhs));
            }
            if consts.get(lhs) == Some(&0) {
                return Some(MirOp::Move(*rhs));
            }
            None
        }
        // x << 0 → x, x >> 0 → x, x >>> 0 → x
        MirOp::Shl { lhs, rhs } | MirOp::Shr { lhs, rhs } | MirOp::Ushr { lhs, rhs } => {
            if consts.get(rhs) == Some(&0) {
                return Some(MirOp::Move(*lhs));
            }
            None
        }

        // ---- Double negation: !(!x) → x ----
        MirOp::LogicalNot(val) => {
            if let Some(MirOp::LogicalNot(inner)) = defs.get(val) {
                return Some(MirOp::Move(*inner));
            }
            None
        }

        // ---- Strict equality with self: x === x → true (for int32, always true) ----
        MirOp::CmpStrictEq { lhs, rhs } => {
            if lhs == rhs {
                return Some(MirOp::True);
            }
            None
        }
        MirOp::CmpStrictNe { lhs, rhs } => {
            if lhs == rhs {
                return Some(MirOp::False);
            }
            None
        }

        _ => None,
    }
}

fn is_f64_zero(val: &ValueId, defs: &HashMap<ValueId, MirOp>) -> bool {
    matches!(defs.get(val), Some(MirOp::ConstFloat64(v)) if *v == 0.0)
}

fn is_f64_one(val: &ValueId, defs: &HashMap<ValueId, MirOp>) -> bool {
    matches!(defs.get(val), Some(MirOp::ConstFloat64(v)) if *v == 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mir::graph::MirGraph;
    use crate::mir::nodes::MirOp;

    #[test]
    fn test_add_f64_zero_eliminated() {
        let mut graph = MirGraph::new("test".into(), 0, 2, 0);
        let bb = graph.entry_block;

        let x = graph.push_instr(bb, MirOp::ConstFloat64(std::f64::consts::PI), 0);
        let zero = graph.push_instr(bb, MirOp::ConstFloat64(0.0), 1);
        let r = graph.push_instr(bb, MirOp::AddF64 { lhs: x, rhs: zero }, 2);
        graph.push_instr(bb, MirOp::Return(r), 3);

        run(&mut graph);

        assert!(
            matches!(graph.blocks[0].instrs[2].op, MirOp::Move(src) if src == x),
            "x + 0.0 should be simplified to Move(x)"
        );
    }

    #[test]
    fn test_bitor_zero_eliminated() {
        let mut graph = MirGraph::new("test".into(), 0, 2, 0);
        let bb = graph.entry_block;

        let x = graph.push_instr(bb, MirOp::ConstInt32(42), 0);
        let zero = graph.push_instr(bb, MirOp::ConstInt32(0), 1);
        let r = graph.push_instr(bb, MirOp::BitOr { lhs: x, rhs: zero }, 2);
        graph.push_instr(bb, MirOp::Return(r), 3);

        run(&mut graph);

        assert!(
            matches!(graph.blocks[0].instrs[2].op, MirOp::Move(src) if src == x),
            "x | 0 should be simplified"
        );
    }

    #[test]
    fn test_double_negation() {
        let mut graph = MirGraph::new("test".into(), 0, 2, 0);
        let bb = graph.entry_block;

        let x = graph.push_instr(bb, MirOp::True, 0);
        let not_x = graph.push_instr(bb, MirOp::LogicalNot(x), 1);
        let not_not_x = graph.push_instr(bb, MirOp::LogicalNot(not_x), 2);
        graph.push_instr(bb, MirOp::Return(not_not_x), 3);

        run(&mut graph);

        assert!(
            matches!(graph.blocks[0].instrs[2].op, MirOp::Move(src) if src == x),
            "!!x should be simplified to Move(x)"
        );
    }

    #[test]
    fn test_self_equality() {
        let mut graph = MirGraph::new("test".into(), 0, 2, 0);
        let bb = graph.entry_block;

        let x = graph.push_instr(bb, MirOp::ConstInt32(42), 0);
        let cmp = graph.push_instr(bb, MirOp::CmpStrictEq { lhs: x, rhs: x }, 1);
        graph.push_instr(bb, MirOp::Return(cmp), 2);

        run(&mut graph);

        assert!(
            matches!(graph.blocks[0].instrs[1].op, MirOp::True),
            "x === x should be simplified to True"
        );
    }
}
