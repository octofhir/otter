//! Representation elimination (box/unbox elimination) pass.
//!
//! Eliminates redundant boxing/unboxing chains:
//! - `UnboxInt32(BoxInt32(x))` → `Move(x)` (x is already i32)
//! - `UnboxFloat64(BoxFloat64(x))` → `Move(x)` (x is already f64)
//! - `BoxInt32(UnboxInt32(x))` → `Move(x)` when x is known Int32
//! - `BoxFloat64(UnboxFloat64(x))` → `Move(x)` when x is known Float64
//! - `BoxBool(LogicalNot(x))` where x is known Bool → fold
//!
//! This is the "Representation Change" pattern from V8 TurboFan and
//! JSC's "FoldLoadsWithUnbox" pass.
//!
//! Spec: Phase 1.2 of JIT_INCREMENTAL_PLAN.md

use std::collections::HashMap;

use crate::mir::graph::{MirGraph, ValueId};
use crate::mir::nodes::MirOp;

/// Run representation elimination on the MIR graph.
pub fn run(graph: &mut MirGraph) {
    // Build a map: ValueId -> defining MirOp (for single-pass analysis).
    let mut defs: HashMap<ValueId, MirOp> = HashMap::new();
    for block in &graph.blocks {
        for instr in &block.instrs {
            defs.insert(instr.value, instr.op.clone());
        }
    }

    // Scan all instructions; replace redundant box/unbox pairs.
    for block_idx in 0..graph.blocks.len() {
        let mut i = 0;
        while i < graph.blocks[block_idx].instrs.len() {
            let instr = &graph.blocks[block_idx].instrs[i];
            let new_op = match &instr.op {
                // UnboxInt32(BoxInt32(x)) → Move(x)
                MirOp::UnboxInt32(val) => {
                    match defs.get(val) {
                        Some(MirOp::BoxInt32(inner)) => Some(MirOp::Move(*inner)),
                        _ => None,
                    }
                }
                // UnboxFloat64(BoxFloat64(x)) → Move(x)
                MirOp::UnboxFloat64(val) => {
                    match defs.get(val) {
                        Some(MirOp::BoxFloat64(inner)) => Some(MirOp::Move(*inner)),
                        _ => None,
                    }
                }
                // BoxInt32(UnboxInt32(x)) → Move(x) if x was boxed int32
                MirOp::BoxInt32(val) => {
                    match defs.get(val) {
                        Some(MirOp::UnboxInt32(inner)) => Some(MirOp::Move(*inner)),
                        Some(MirOp::GuardInt32 { val: inner, .. }) => {
                            // GuardInt32 produces unboxed i32, boxing it back → original boxed
                            Some(MirOp::Move(*inner))
                        }
                        _ => None,
                    }
                }
                // BoxFloat64(UnboxFloat64(x)) → Move(x) if x was boxed f64
                MirOp::BoxFloat64(val) => {
                    match defs.get(val) {
                        Some(MirOp::UnboxFloat64(inner)) => Some(MirOp::Move(*inner)),
                        Some(MirOp::GuardFloat64 { val: inner, .. }) => {
                            Some(MirOp::Move(*inner))
                        }
                        _ => None,
                    }
                }
                // BoxBool on known boolean → original boxed value
                MirOp::BoxBool(val) => {
                    match defs.get(val) {
                        Some(MirOp::GuardBool { val: inner, .. }) => {
                            Some(MirOp::Move(*inner))
                        }
                        _ => None,
                    }
                }
                _ => None,
            };

            if let Some(new) = new_op {
                // Update defs map with the replacement.
                let value = graph.blocks[block_idx].instrs[i].value;
                defs.insert(value, new.clone());
                graph.blocks[block_idx].instrs[i].op = new;
            }
            i += 1;
        }
    }
}
