//! Guard elimination pass (local value numbering).
//!
//! Within a basic block, if `GuardInt32(v)` has already been checked,
//! subsequent `GuardInt32(v)` can be replaced with `Move(v_unboxed)`.
//!
//! This is the "Known Node Aspects" pattern from V8 Maglev.
//!
//! Spec: Phase 1.1 of JIT_INCREMENTAL_PLAN.md

use std::collections::HashMap;

use crate::mir::graph::{MirGraph, ValueId};
use crate::mir::nodes::MirOp;

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

/// Run guard elimination on the MIR graph.
pub fn run(graph: &mut MirGraph) {
    for block_idx in 0..graph.blocks.len() {
        // Track: (guarded_value, guard_kind) -> result_value (the unboxed value).
        // Within a block, if we've already guarded a value, reuse the result.
        let mut proven: HashMap<(ValueId, GuardKind), ValueId> = HashMap::new();

        let mut i = 0;
        while i < graph.blocks[block_idx].instrs.len() {
            let instr = &graph.blocks[block_idx].instrs[i];
            let value = instr.value;

            let replacement = match &instr.op {
                MirOp::GuardInt32 { val, .. } => {
                    let key = (*val, GuardKind::Int32);
                    if let Some(&prev_result) = proven.get(&key) {
                        // Already guarded — reuse the previous result.
                        Some(MirOp::Move(prev_result))
                    } else {
                        // First guard — record it.
                        proven.insert(key, value);
                        None
                    }
                }
                MirOp::GuardFloat64 { val, .. } => {
                    let key = (*val, GuardKind::Float64);
                    if let Some(&prev_result) = proven.get(&key) {
                        Some(MirOp::Move(prev_result))
                    } else {
                        proven.insert(key, value);
                        None
                    }
                }
                MirOp::GuardObject { val, .. } => {
                    let key = (*val, GuardKind::Object);
                    if let Some(&prev_result) = proven.get(&key) {
                        Some(MirOp::Move(prev_result))
                    } else {
                        proven.insert(key, value);
                        None
                    }
                }
                MirOp::GuardString { val, .. } => {
                    let key = (*val, GuardKind::String);
                    if let Some(&prev_result) = proven.get(&key) {
                        Some(MirOp::Move(prev_result))
                    } else {
                        proven.insert(key, value);
                        None
                    }
                }
                MirOp::GuardFunction { val, .. } => {
                    let key = (*val, GuardKind::Function);
                    if let Some(&prev_result) = proven.get(&key) {
                        Some(MirOp::Move(prev_result))
                    } else {
                        proven.insert(key, value);
                        None
                    }
                }
                MirOp::GuardBool { val, .. } => {
                    let key = (*val, GuardKind::Bool);
                    if let Some(&prev_result) = proven.get(&key) {
                        Some(MirOp::Move(prev_result))
                    } else {
                        proven.insert(key, value);
                        None
                    }
                }
                _ => None,
            };

            if let Some(new_op) = replacement {
                graph.blocks[block_idx].instrs[i].op = new_op;
            }
            i += 1;
        }
    }
}
