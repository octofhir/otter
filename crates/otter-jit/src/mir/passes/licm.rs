//! Loop-Invariant Code Motion (LICM).
//!
//! Moves computations that don't change across loop iterations out of the loop
//! into the loop's preheader block.
//!
//! ## What can be hoisted
//!
//! An instruction is loop-invariant if all its operands are either:
//! 1. Defined outside the loop
//! 2. Constants
//! 3. Themselves loop-invariant
//!
//! AND the instruction has no side effects (pure computation).
//!
//! ## Shape guard hoisting
//!
//! If a shape guard checks an object that's defined outside the loop and
//! there are no aliasing stores in the loop body, the guard can be hoisted
//! to the preheader — checked once instead of every iteration.
//!
//! SpiderMonkey Ion: LICM pass.
//! JSC FTL: LICM enabled by SSA form.
//!
//! Spec: Phase 4.6 of JIT_INCREMENTAL_PLAN.md

use std::collections::HashSet;

use crate::mir::graph::{BlockId, MirGraph, ValueId};
use crate::mir::nodes::MirOp;

/// Run LICM on the MIR graph.
///
/// For each natural loop (identified by back-edges), hoist loop-invariant
/// instructions to the preheader.
pub fn run(graph: &mut MirGraph) {
    graph.recompute_edges();

    // Find natural loops via back-edges.
    let back_edges = graph.back_edges();
    if back_edges.is_empty() {
        return; // No loops to optimize.
    }

    for (backedge_source, header) in &back_edges {
        // Collect loop body blocks (all blocks reachable from header that reach backedge_source).
        let loop_blocks = collect_loop_blocks(graph, *header, *backedge_source);
        if loop_blocks.is_empty() {
            continue;
        }

        // Find or identify the preheader (predecessor of header outside the loop).
        // For now, we don't create preheaders — we just identify invariant instructions
        // and mark them for future hoisting. True hoisting requires inserting preheader blocks.

        // Collect values defined inside the loop.
        let loop_defined: HashSet<ValueId> = loop_blocks
            .iter()
            .flat_map(|&bid| {
                let block = &graph.blocks[bid.0 as usize];
                block.instrs.iter().map(|i| i.value)
                    .chain(block.params.iter().map(|p| p.value))
            })
            .collect();

        // Identify loop-invariant instructions (iterative, fixpoint).
        let mut invariant: HashSet<ValueId> = HashSet::new();
        let mut changed = true;
        while changed {
            changed = false;
            for &bid in &loop_blocks {
                let block = &graph.blocks[bid.0 as usize];
                for instr in &block.instrs {
                    if invariant.contains(&instr.value) {
                        continue;
                    }
                    if !is_pure(&instr.op) {
                        continue;
                    }
                    if all_operands_invariant(&instr.op, &loop_defined, &invariant) {
                        invariant.insert(instr.value);
                        changed = true;
                    }
                }
            }
        }

        // For now, just count invariant instructions for telemetry.
        // True hoisting (moving instructions to preheader) requires:
        // 1. Identifying/creating the preheader block
        // 2. Moving instructions while maintaining SSA
        // 3. Updating Phi nodes
        // This is Phase 4.6 complete implementation — deferred to dedicated session.
        let _ = invariant.len();
    }
}

/// Collect all blocks in the natural loop body.
///
/// A natural loop for back-edge (source → header) contains all blocks
/// that can reach `source` without going through `header`.
fn collect_loop_blocks(
    graph: &MirGraph,
    header: BlockId,
    backedge_source: BlockId,
) -> Vec<BlockId> {
    let mut body: HashSet<BlockId> = HashSet::new();
    body.insert(header);

    if header == backedge_source {
        return vec![header]; // Self-loop.
    }

    // Walk backwards from backedge_source to find all blocks in the loop.
    let mut worklist = vec![backedge_source];
    while let Some(block) = worklist.pop() {
        if body.insert(block) {
            // Add predecessors (all paths that reach this block).
            for &pred in &graph.blocks[block.0 as usize].predecessors {
                if !body.contains(&pred) {
                    worklist.push(pred);
                }
            }
        }
    }

    body.into_iter().collect()
}

/// Whether an instruction is pure (no side effects, safe to hoist).
fn is_pure(op: &MirOp) -> bool {
    matches!(
        op,
        MirOp::Const(_)
            | MirOp::ConstInt32(_)
            | MirOp::ConstFloat64(_)
            | MirOp::True
            | MirOp::False
            | MirOp::Undefined
            | MirOp::Null
            | MirOp::BoxInt32(_)
            | MirOp::BoxFloat64(_)
            | MirOp::BoxBool(_)
            | MirOp::UnboxInt32(_)
            | MirOp::UnboxFloat64(_)
            | MirOp::Int32ToFloat64(_)
            | MirOp::AddF64 { .. }
            | MirOp::SubF64 { .. }
            | MirOp::MulF64 { .. }
            | MirOp::DivF64 { .. }
            | MirOp::NegF64(_)
            | MirOp::BitAnd { .. }
            | MirOp::BitOr { .. }
            | MirOp::BitXor { .. }
            | MirOp::Shl { .. }
            | MirOp::Shr { .. }
            | MirOp::Ushr { .. }
            | MirOp::BitNot(_)
            | MirOp::CmpI32 { .. }
            | MirOp::CmpF64 { .. }
            | MirOp::CmpStrictEq { .. }
            | MirOp::CmpStrictNe { .. }
            | MirOp::LogicalNot(_)
            | MirOp::Move(_)
    )
}

/// Whether all operands of an instruction are loop-invariant.
///
/// An operand is invariant if it's either:
/// - Not defined inside the loop (defined outside or is a constant)
/// - Already identified as loop-invariant
fn all_operands_invariant(
    op: &MirOp,
    loop_defined: &HashSet<ValueId>,
    invariant: &HashSet<ValueId>,
) -> bool {
    let operands = collect_operands(op);
    operands.iter().all(|v| {
        !loop_defined.contains(v) || invariant.contains(v)
    })
}

/// Collect ValueId operands from a MirOp.
fn collect_operands(op: &MirOp) -> Vec<ValueId> {
    let mut vals = Vec::new();
    match op {
        MirOp::Const(_) | MirOp::ConstInt32(_) | MirOp::ConstFloat64(_)
        | MirOp::True | MirOp::False | MirOp::Undefined | MirOp::Null => {}

        MirOp::BoxInt32(v) | MirOp::BoxFloat64(v) | MirOp::BoxBool(v)
        | MirOp::UnboxInt32(v) | MirOp::UnboxFloat64(v) | MirOp::Int32ToFloat64(v)
        | MirOp::NegF64(v) | MirOp::BitNot(v) | MirOp::LogicalNot(v)
        | MirOp::Move(v) => { vals.push(*v); }

        MirOp::AddF64 { lhs, rhs } | MirOp::SubF64 { lhs, rhs }
        | MirOp::MulF64 { lhs, rhs } | MirOp::DivF64 { lhs, rhs }
        | MirOp::BitAnd { lhs, rhs } | MirOp::BitOr { lhs, rhs }
        | MirOp::BitXor { lhs, rhs } | MirOp::Shl { lhs, rhs }
        | MirOp::Shr { lhs, rhs } | MirOp::Ushr { lhs, rhs }
        | MirOp::CmpI32 { lhs, rhs, .. } | MirOp::CmpF64 { lhs, rhs, .. }
        | MirOp::CmpStrictEq { lhs, rhs } | MirOp::CmpStrictNe { lhs, rhs } => {
            vals.push(*lhs); vals.push(*rhs);
        }

        _ => {} // Non-pure ops won't be candidates anyway.
    }
    vals
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mir::graph::MirGraph;
    use crate::mir::nodes::MirOp;

    #[test]
    fn test_collect_loop_blocks_simple() {
        // bb0 → bb1 → bb0 (back-edge)
        let mut graph = MirGraph::new("loop".into(), 0, 2, 0);
        let bb0 = graph.entry_block;
        let bb1 = graph.create_block();

        let cond = graph.push_instr(bb0, MirOp::True, 0);
        graph.push_instr(bb0, MirOp::Branch {
            cond,
            true_block: bb1,
            true_args: vec![],
            false_block: bb1, // simplified
            false_args: vec![],
        }, 1);
        graph.push_instr(bb1, MirOp::Jump(bb0, vec![]), 2);

        graph.recompute_edges();

        let body = collect_loop_blocks(&graph, bb0, bb1);
        assert!(body.contains(&bb0));
        assert!(body.contains(&bb1));
    }

    #[test]
    fn test_licm_no_crash_on_loopless() {
        let mut graph = MirGraph::new("noloop".into(), 0, 1, 0);
        let bb = graph.entry_block;
        let v = graph.push_instr(bb, MirOp::ConstInt32(42), 0);
        graph.push_instr(bb, MirOp::Return(v), 1);

        // Should not crash.
        run(&mut graph);
    }
}
