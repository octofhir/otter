//! Guard elimination — local + dominator-based.
//!
//! ## Local (within a block)
//! If `GuardInt32(v)` has already been checked in the current block,
//! subsequent `GuardInt32(v)` → `Move(prev_result)`.
//!
//! ## Dominator-based (cross-block)
//! If `GuardInt32(v)` is proven in block A, and A dominates block B,
//! then `GuardInt32(v)` in B can be eliminated.
//!
//! ## Type-analysis-based
//! If type analysis proves a value is always Int32 (e.g., result of AddI32),
//! then `GuardInt32` on that value is redundant.
//!
//! V8 Maglev: "Known Node Aspects" + "Map Inference".
//! JSC DFG: "Type Check Hoisting".
//!
//! Spec: Phases 1.1 + 4.4 of JIT_INCREMENTAL_PLAN.md

use std::collections::HashMap;

use crate::mir::graph::{BlockId, MirGraph, ValueId};
use crate::mir::nodes::MirOp;

use super::dominators::DominatorTree;
use super::type_analysis::{self, AbstractType, TypeMap};

/// Guard kind for deduplication.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum GuardKind {
    Int32,
    Float64,
    Object,
    String,
    Function,
    Bool,
}

type GuardKey = (ValueId, GuardKind);

/// Run guard elimination on the MIR graph.
///
/// Phase 1: local guard elimination (within blocks).
/// Phase 2: dominator-based guard elimination (across blocks, if edges computed).
/// Phase 3: type-proof-based elimination (using abstract type analysis).
pub fn run(graph: &mut MirGraph) {
    // Run type analysis for proof-based elimination.
    let type_map = type_analysis::run(graph);

    // Try to compute dominator tree (needs edges).
    graph.recompute_edges();
    let dom_tree = DominatorTree::compute(graph);

    // Collect proven guards per block (from local analysis).
    let mut proven_per_block: HashMap<BlockId, HashMap<GuardKey, ValueId>> = HashMap::new();

    // Process blocks in RPO (dominator tree order) so parent info is ready.
    for &block_id in dom_tree.rpo_order() {
        // Start with guards inherited from the immediate dominator.
        let mut proven: HashMap<GuardKey, ValueId> = dom_tree
            .idom(block_id)
            .and_then(|idom| {
                if idom != block_id {
                    proven_per_block.get(&idom).cloned()
                } else {
                    None
                }
            })
            .unwrap_or_default();

        let block_idx = block_id.0 as usize;
        if block_idx >= graph.blocks.len() {
            continue;
        }

        let mut i = 0;
        while i < graph.blocks[block_idx].instrs.len() {
            let instr = &graph.blocks[block_idx].instrs[i];
            let value = instr.value;

            let replacement = match &instr.op {
                MirOp::GuardInt32 { val, .. } => {
                    try_eliminate_guard(*val, value, GuardKind::Int32, &mut proven, &type_map)
                }
                MirOp::GuardFloat64 { val, .. } => {
                    try_eliminate_guard(*val, value, GuardKind::Float64, &mut proven, &type_map)
                }
                MirOp::GuardObject { val, .. } => {
                    try_eliminate_guard(*val, value, GuardKind::Object, &mut proven, &type_map)
                }
                MirOp::GuardString { val, .. } => {
                    try_eliminate_guard(*val, value, GuardKind::String, &mut proven, &type_map)
                }
                MirOp::GuardFunction { val, .. } => {
                    try_eliminate_guard(*val, value, GuardKind::Function, &mut proven, &type_map)
                }
                MirOp::GuardBool { val, .. } => {
                    try_eliminate_guard(*val, value, GuardKind::Bool, &mut proven, &type_map)
                }
                _ => None,
            };

            if let Some(new_op) = replacement {
                graph.blocks[block_idx].instrs[i].op = new_op;
            }
            i += 1;
        }

        // Save proven guards for dominated children.
        proven_per_block.insert(block_id, proven);
    }
}

/// Try to eliminate a guard. Returns Some(Move) if redundant, None if needed.
fn try_eliminate_guard(
    guarded_val: ValueId,
    result_val: ValueId,
    kind: GuardKind,
    proven: &mut HashMap<GuardKey, ValueId>,
    type_map: &TypeMap,
) -> Option<MirOp> {
    let key = (guarded_val, kind);

    // Check 1: Already proven by a dominating guard.
    if let Some(&prev_result) = proven.get(&key) {
        return Some(MirOp::Move(prev_result));
    }

    // Check 2: Type analysis proves the guard always succeeds.
    let abstract_ty = type_map
        .get(&guarded_val)
        .copied()
        .unwrap_or(AbstractType::ANY);
    let proved_by_type = match kind {
        GuardKind::Int32 => abstract_ty.proves_int32(),
        GuardKind::Float64 => abstract_ty.proves_float64(),
        GuardKind::Object => abstract_ty.proves_object(),
        GuardKind::Bool => abstract_ty.proves_bool(),
        _ => false,
    };

    if proved_by_type {
        // Guard always succeeds. Replace with unbox (the guard's semantic output).
        let unbox = match kind {
            GuardKind::Int32 => MirOp::UnboxInt32(guarded_val),
            GuardKind::Float64 => MirOp::UnboxFloat64(guarded_val),
            _ => MirOp::Move(guarded_val),
        };
        proven.insert(key, result_val);
        return Some(unbox);
    }

    // Guard is needed. Record it for downstream blocks.
    proven.insert(key, result_val);
    None
}
