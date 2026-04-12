//! Constant folding pass.
//!
//! Evaluates operations on constant operands at compile time:
//! - `ConstInt32(a) + ConstInt32(b)` → `ConstInt32(a + b)`
//! - `GuardInt32(ConstInt32(x))` → nop (guard always succeeds)
//! - `Branch(True/False, ...)` → `Jump` to the known-taken target
//!
//! Spec: Phase 1.3 of JIT_INCREMENTAL_PLAN.md

use crate::mir::graph::MirGraph;
use crate::mir::nodes::MirOp;

/// Run constant folding on the MIR graph.
pub fn run(graph: &mut MirGraph) {
    // Build a map: ValueId -> constant value (if the defining op is a constant).
    let mut const_i32 = std::collections::HashMap::new();
    let mut const_f64 = std::collections::HashMap::new();
    let mut const_bool = std::collections::HashMap::new(); // true=1, false=0

    // First pass: collect all constants.
    for block in &graph.blocks {
        for instr in &block.instrs {
            match &instr.op {
                MirOp::ConstInt32(v) => { const_i32.insert(instr.value, *v); }
                MirOp::ConstFloat64(v) => { const_f64.insert(instr.value, *v); }
                MirOp::True => { const_bool.insert(instr.value, true); }
                MirOp::False => { const_bool.insert(instr.value, false); }
                _ => {}
            }
        }
    }

    // Second pass: fold operations with constant operands.
    for block_idx in 0..graph.blocks.len() {
        let mut i = 0;
        while i < graph.blocks[block_idx].instrs.len() {
            let instr = &graph.blocks[block_idx].instrs[i];
            let value = instr.value;
            let new_op = match &instr.op {
                // ---- i32 arithmetic ----
                MirOp::AddI32 { lhs, rhs, .. } => {
                    match (const_i32.get(lhs), const_i32.get(rhs)) {
                        (Some(&a), Some(&b)) => a.checked_add(b).map(|r| {
                            const_i32.insert(value, r);
                            MirOp::ConstInt32(r)
                        }),
                        _ => None,
                    }
                }
                MirOp::SubI32 { lhs, rhs, .. } => {
                    match (const_i32.get(lhs), const_i32.get(rhs)) {
                        (Some(&a), Some(&b)) => a.checked_sub(b).map(|r| {
                            const_i32.insert(value, r);
                            MirOp::ConstInt32(r)
                        }),
                        _ => None,
                    }
                }
                MirOp::MulI32 { lhs, rhs, .. } => {
                    match (const_i32.get(lhs), const_i32.get(rhs)) {
                        (Some(&a), Some(&b)) => a.checked_mul(b).map(|r| {
                            const_i32.insert(value, r);
                            MirOp::ConstInt32(r)
                        }),
                        _ => None,
                    }
                }
                MirOp::IncI32 { val, .. } => {
                    const_i32.get(val).and_then(|&a| a.checked_add(1)).map(|r| {
                        const_i32.insert(value, r);
                        MirOp::ConstInt32(r)
                    })
                }
                MirOp::DecI32 { val, .. } => {
                    const_i32.get(val).and_then(|&a| a.checked_sub(1)).map(|r| {
                        const_i32.insert(value, r);
                        MirOp::ConstInt32(r)
                    })
                }
                MirOp::NegI32 { val, .. } => {
                    const_i32.get(val).and_then(|&a| a.checked_neg()).map(|r| {
                        const_i32.insert(value, r);
                        MirOp::ConstInt32(r)
                    })
                }

                // ---- f64 arithmetic ----
                MirOp::AddF64 { lhs, rhs } => {
                    let pair = (const_f64.get(lhs).copied(), const_f64.get(rhs).copied());
                    match pair {
                        (Some(a), Some(b)) => {
                            let r = a + b;
                            const_f64.insert(value, r);
                            Some(MirOp::ConstFloat64(r))
                        }
                        _ => None,
                    }
                }
                MirOp::SubF64 { lhs, rhs } => {
                    let pair = (const_f64.get(lhs).copied(), const_f64.get(rhs).copied());
                    match pair {
                        (Some(a), Some(b)) => {
                            let r = a - b;
                            const_f64.insert(value, r);
                            Some(MirOp::ConstFloat64(r))
                        }
                        _ => None,
                    }
                }
                MirOp::MulF64 { lhs, rhs } => {
                    let pair = (const_f64.get(lhs).copied(), const_f64.get(rhs).copied());
                    match pair {
                        (Some(a), Some(b)) => {
                            let r = a * b;
                            const_f64.insert(value, r);
                            Some(MirOp::ConstFloat64(r))
                        }
                        _ => None,
                    }
                }
                MirOp::NegF64(val) => {
                    const_f64.get(val).copied().map(|a| {
                        let r = -a;
                        const_f64.insert(value, r);
                        MirOp::ConstFloat64(r)
                    })
                }

                // ---- Logical NOT ----
                MirOp::LogicalNot(val) => {
                    const_bool.get(val).copied().map(|b| {
                        const_bool.insert(value, !b);
                        if !b { MirOp::True } else { MirOp::False }
                    })
                }

                // ---- Guards on known constants: always succeed → nop (Move) ----
                MirOp::GuardInt32 { val, .. } => {
                    if const_i32.contains_key(val) {
                        // Guard always succeeds. Replace with UnboxInt32 (identity for const).
                        Some(MirOp::UnboxInt32(*val))
                    } else {
                        None
                    }
                }
                MirOp::GuardFloat64 { val, .. } => {
                    if const_f64.contains_key(val) {
                        Some(MirOp::UnboxFloat64(*val))
                    } else {
                        None
                    }
                }
                MirOp::GuardBool { val, .. } => {
                    if const_bool.contains_key(val) {
                        Some(MirOp::Move(*val))
                    } else {
                        None
                    }
                }

                // ---- Branch on constant condition → unconditional jump ----
                MirOp::Branch { cond, true_block, true_args, false_block, false_args, .. } => {
                    const_bool.get(cond).map(|&b| {
                        if b {
                            MirOp::Jump(*true_block, true_args.clone())
                        } else {
                            MirOp::Jump(*false_block, false_args.clone())
                        }
                    })
                }

                // ---- BoxInt32(ConstInt32) → Const (known NaN-boxed value) ----
                MirOp::BoxInt32(val) => {
                    const_i32.get(val).map(|&v| {
                        let boxed = 0x7FF8_0001_0000_0000u64 | (v as u32 as u64);
                        MirOp::Const(boxed)
                    })
                }

                _ => None,
            };

            if let Some(new) = new_op {
                graph.blocks[block_idx].instrs[i].op = new;
            }
            i += 1;
        }
    }
}
