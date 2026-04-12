//! Global Value Numbering (GVN) — cross-block common subexpression elimination.
//!
//! For each instruction, compute a hash of its operation + operands.
//! If a dominating instruction with the same hash already exists, replace
//! the current instruction with a `Move` to the dominating value.
//!
//! ## Alias-aware load elimination
//!
//! Two loads from the same source can be deduplicated only if there's no
//! aliasing store between them (SM's `dependency()` check). For now, we
//! only GVN pure computations (no loads/stores).
//!
//! SM reference: GVN checks `congruentTo()` AND matching `dependency()`.
//! V8 reference: Load Elimination pass in TurboFan.
//!
//! Spec: Phase 6.1 of JIT_INCREMENTAL_PLAN.md

use std::collections::HashMap;

use crate::mir::graph::{BlockId, MirGraph, ValueId};
use crate::mir::nodes::MirOp;

use super::dominators::DominatorTree;

/// A hash key representing the "shape" of a pure computation.
/// Two instructions with the same GvnKey produce the same value.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum GvnKey {
    /// Constant: same constant always has the same value.
    ConstI32(i32),
    ConstF64(u64), // f64 bits for exact comparison.
    True,
    False,
    Undefined,
    Null,

    /// Binary op: (opcode_tag, lhs_value_number, rhs_value_number).
    Binary(u8, ValueId, ValueId),
    /// Unary op: (opcode_tag, operand_value_number).
    Unary(u8, ValueId),
}

/// Opcode tags for GVN hashing (distinct u8 per operation).
const TAG_ADD_I32: u8 = 1;
const TAG_SUB_I32: u8 = 2;
const TAG_MUL_I32: u8 = 3;
const TAG_ADD_F64: u8 = 4;
const TAG_SUB_F64: u8 = 5;
const TAG_MUL_F64: u8 = 6;
const TAG_DIV_F64: u8 = 7;
const TAG_BIT_AND: u8 = 8;
const TAG_BIT_OR: u8 = 9;
const TAG_BIT_XOR: u8 = 10;
const TAG_SHL: u8 = 11;
const TAG_SHR: u8 = 12;
const TAG_USHR: u8 = 13;
const TAG_CMP_I32_EQ: u8 = 14;
const TAG_CMP_I32_NE: u8 = 15;
const TAG_CMP_I32_LT: u8 = 16;
const TAG_CMP_I32_LE: u8 = 17;
const TAG_CMP_I32_GT: u8 = 18;
const TAG_CMP_I32_GE: u8 = 19;
const TAG_STRICT_EQ: u8 = 20;
const TAG_STRICT_NE: u8 = 21;
const TAG_NEG_F64: u8 = 22;
const TAG_BIT_NOT: u8 = 23;
const TAG_NOT: u8 = 24;
const TAG_BOX_I32: u8 = 25;
const TAG_BOX_F64: u8 = 26;
const TAG_BOX_BOOL: u8 = 27;
const TAG_UNBOX_I32: u8 = 28;
const TAG_UNBOX_F64: u8 = 29;
const TAG_I32_TO_F64: u8 = 30;
const TAG_MOVE: u8 = 31;

/// Run Global Value Numbering on the MIR graph.
pub fn run(graph: &mut MirGraph) {
    graph.recompute_edges();
    let dom_tree = DominatorTree::compute(graph);

    // Map from GvnKey → (defining ValueId, defining BlockId).
    // We process blocks in RPO order so dominating defs are seen first.
    let mut value_table: HashMap<GvnKey, (ValueId, BlockId)> = HashMap::new();

    for &block_id in dom_tree.rpo_order() {
        let block_idx = block_id.0 as usize;
        if block_idx >= graph.blocks.len() {
            continue;
        }

        let mut i = 0;
        while i < graph.blocks[block_idx].instrs.len() {
            let instr = &graph.blocks[block_idx].instrs[i];

            if let Some(key) = compute_gvn_key(&instr.op) {
                if let Some(&(existing_val, existing_block)) = value_table.get(&key) {
                    // Check that the existing definition dominates this use.
                    if dom_tree.dominates(existing_block, block_id) {
                        // Replace with Move to existing value.
                        graph.blocks[block_idx].instrs[i].op = MirOp::Move(existing_val);
                        i += 1;
                        continue;
                    }
                }
                // First occurrence or non-dominating — register it.
                value_table.insert(key, (instr.value, block_id));
            }

            i += 1;
        }
    }
}

/// Compute a GVN key for a pure instruction, or None for side-effectful ops.
fn compute_gvn_key(op: &MirOp) -> Option<GvnKey> {
    match op {
        // Constants.
        MirOp::ConstInt32(v) => Some(GvnKey::ConstI32(*v)),
        MirOp::ConstFloat64(v) => Some(GvnKey::ConstF64(v.to_bits())),
        MirOp::True => Some(GvnKey::True),
        MirOp::False => Some(GvnKey::False),
        MirOp::Undefined => Some(GvnKey::Undefined),
        MirOp::Null => Some(GvnKey::Null),

        // Binary pure ops (no deopt = pure; with deopt = side-effectful, skip).
        MirOp::AddF64 { lhs, rhs } => Some(GvnKey::Binary(TAG_ADD_F64, *lhs, *rhs)),
        MirOp::SubF64 { lhs, rhs } => Some(GvnKey::Binary(TAG_SUB_F64, *lhs, *rhs)),
        MirOp::MulF64 { lhs, rhs } => Some(GvnKey::Binary(TAG_MUL_F64, *lhs, *rhs)),
        MirOp::DivF64 { lhs, rhs } => Some(GvnKey::Binary(TAG_DIV_F64, *lhs, *rhs)),

        MirOp::BitAnd { lhs, rhs } => Some(GvnKey::Binary(TAG_BIT_AND, *lhs, *rhs)),
        MirOp::BitOr { lhs, rhs } => Some(GvnKey::Binary(TAG_BIT_OR, *lhs, *rhs)),
        MirOp::BitXor { lhs, rhs } => Some(GvnKey::Binary(TAG_BIT_XOR, *lhs, *rhs)),
        MirOp::Shl { lhs, rhs } => Some(GvnKey::Binary(TAG_SHL, *lhs, *rhs)),
        MirOp::Shr { lhs, rhs } => Some(GvnKey::Binary(TAG_SHR, *lhs, *rhs)),
        MirOp::Ushr { lhs, rhs } => Some(GvnKey::Binary(TAG_USHR, *lhs, *rhs)),

        MirOp::CmpStrictEq { lhs, rhs } => Some(GvnKey::Binary(TAG_STRICT_EQ, *lhs, *rhs)),
        MirOp::CmpStrictNe { lhs, rhs } => Some(GvnKey::Binary(TAG_STRICT_NE, *lhs, *rhs)),

        MirOp::CmpI32 { op, lhs, rhs } => {
            use crate::mir::types::CmpOp;
            let tag = match op {
                CmpOp::Eq => TAG_CMP_I32_EQ,
                CmpOp::Ne => TAG_CMP_I32_NE,
                CmpOp::Lt => TAG_CMP_I32_LT,
                CmpOp::Le => TAG_CMP_I32_LE,
                CmpOp::Gt => TAG_CMP_I32_GT,
                CmpOp::Ge => TAG_CMP_I32_GE,
            };
            Some(GvnKey::Binary(tag, *lhs, *rhs))
        }

        // Unary pure ops.
        MirOp::NegF64(v) => Some(GvnKey::Unary(TAG_NEG_F64, *v)),
        MirOp::BitNot(v) => Some(GvnKey::Unary(TAG_BIT_NOT, *v)),
        MirOp::LogicalNot(v) => Some(GvnKey::Unary(TAG_NOT, *v)),
        MirOp::BoxInt32(v) => Some(GvnKey::Unary(TAG_BOX_I32, *v)),
        MirOp::BoxFloat64(v) => Some(GvnKey::Unary(TAG_BOX_F64, *v)),
        MirOp::BoxBool(v) => Some(GvnKey::Unary(TAG_BOX_BOOL, *v)),
        MirOp::UnboxInt32(v) => Some(GvnKey::Unary(TAG_UNBOX_I32, *v)),
        MirOp::UnboxFloat64(v) => Some(GvnKey::Unary(TAG_UNBOX_F64, *v)),
        MirOp::Int32ToFloat64(v) => Some(GvnKey::Unary(TAG_I32_TO_F64, *v)),
        MirOp::Move(v) => Some(GvnKey::Unary(TAG_MOVE, *v)),

        // i32 arithmetic has deopt (overflow) → side-effectful, don't GVN.
        // Loads, stores, calls, guards → not pure, don't GVN.
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mir::graph::MirGraph;
    use crate::mir::nodes::MirOp;

    #[test]
    fn test_gvn_eliminates_duplicate_constants() {
        let mut graph = MirGraph::new("test".into(), 0, 2, 0);
        let bb = graph.entry_block;

        let v0 = graph.push_instr(bb, MirOp::ConstInt32(42), 0);
        let v1 = graph.push_instr(bb, MirOp::ConstInt32(42), 1); // duplicate
        graph.push_instr(bb, MirOp::Return(v1), 2);

        graph.recompute_edges();
        run(&mut graph);

        // v1 should now be Move(v0).
        let instr = &graph.blocks[0].instrs[1];
        assert!(
            matches!(instr.op, MirOp::Move(src) if src == v0),
            "duplicate constant should be replaced with Move, got {:?}",
            instr.op
        );
    }

    #[test]
    fn test_gvn_eliminates_duplicate_f64_add() {
        let mut graph = MirGraph::new("test".into(), 0, 3, 0);
        let bb = graph.entry_block;

        let a = graph.push_instr(bb, MirOp::ConstFloat64(1.0), 0);
        let b = graph.push_instr(bb, MirOp::ConstFloat64(2.0), 1);
        let v0 = graph.push_instr(bb, MirOp::AddF64 { lhs: a, rhs: b }, 2);
        let v1 = graph.push_instr(bb, MirOp::AddF64 { lhs: a, rhs: b }, 3); // duplicate
        graph.push_instr(bb, MirOp::Return(v1), 4);

        graph.recompute_edges();
        run(&mut graph);

        let instr = &graph.blocks[0].instrs[3];
        assert!(
            matches!(instr.op, MirOp::Move(src) if src == v0),
            "duplicate AddF64 should be eliminated, got {:?}",
            instr.op
        );
    }

    #[test]
    fn test_gvn_cross_block_diamond() {
        // bb0: v0 = ConstInt32(42)
        // bb0 → bb1, bb2
        // bb1: v1 = ConstInt32(42) ← should be eliminated (bb0 dominates bb1)
        // bb2: v2 = ConstInt32(42) ← should be eliminated
        let mut graph = MirGraph::new("diamond".into(), 0, 2, 0);
        let bb0 = graph.entry_block;
        let bb1 = graph.create_block();
        let bb2 = graph.create_block();

        let v0 = graph.push_instr(bb0, MirOp::ConstInt32(42), 0);
        let cond = graph.push_instr(bb0, MirOp::True, 1);
        graph.push_instr(bb0, MirOp::Branch {
            cond,
            true_block: bb1,
            true_args: vec![],
            false_block: bb2,
            false_args: vec![],
        }, 2);

        let v1 = graph.push_instr(bb1, MirOp::ConstInt32(42), 3);
        graph.push_instr(bb1, MirOp::Return(v1), 4);

        let v2 = graph.push_instr(bb2, MirOp::ConstInt32(42), 5);
        graph.push_instr(bb2, MirOp::Return(v2), 6);

        graph.recompute_edges();
        run(&mut graph);

        // Both bb1 and bb2 constants should be eliminated.
        assert!(
            matches!(graph.blocks[1].instrs[0].op, MirOp::Move(src) if src == v0),
            "bb1 constant should be Move(v0), got {:?}",
            graph.blocks[1].instrs[0].op
        );
        assert!(
            matches!(graph.blocks[2].instrs[0].op, MirOp::Move(src) if src == v0),
            "bb2 constant should be Move(v0), got {:?}",
            graph.blocks[2].instrs[0].op
        );
    }

    #[test]
    fn test_gvn_does_not_eliminate_side_effectful() {
        let mut graph = MirGraph::new("test".into(), 0, 2, 0);
        let bb = graph.entry_block;

        // LoadLocal is not pure → should NOT be GVN'd.
        let v0 = graph.push_instr(bb, MirOp::LoadLocal(0), 0);
        let v1 = graph.push_instr(bb, MirOp::LoadLocal(0), 1);
        graph.push_instr(bb, MirOp::Return(v1), 2);

        graph.recompute_edges();
        run(&mut graph);

        // v1 should NOT be replaced (loads might have different values).
        assert!(
            matches!(graph.blocks[0].instrs[1].op, MirOp::LoadLocal(0)),
            "LoadLocal should not be GVN'd"
        );
    }
}
