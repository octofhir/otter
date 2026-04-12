//! Dead code elimination pass.
//!
//! Removes instructions whose values are never used, preserving those
//! with side effects (stores, calls, guards, terminators, safepoints).
//!
//! Algorithm:
//! 1. Build use-count for every ValueId.
//! 2. Mark instructions with side effects as live.
//! 3. Mark all operands of live instructions as live (transitive).
//! 4. Remove instructions that are not live.
//!
//! Spec: Phase 1.4 of JIT_INCREMENTAL_PLAN.md

use std::collections::HashSet;

use crate::mir::graph::{MirGraph, ValueId};
use crate::mir::nodes::MirOp;

/// Run dead code elimination on the MIR graph.
pub fn run(graph: &mut MirGraph) {
    // Step 1: Collect all used ValueIds.
    let mut used: HashSet<ValueId> = HashSet::new();
    for block in &graph.blocks {
        for instr in &block.instrs {
            collect_operands(&instr.op, &mut used);
        }
        // Block params are used by Phi — their uses are in jump args.
    }

    // Step 2: Mark instructions that are inherently live (side effects + terminators).
    let mut live: HashSet<ValueId> = HashSet::new();
    for block in &graph.blocks {
        for instr in &block.instrs {
            if has_side_effects(&instr.op) || used.contains(&instr.value) {
                live.insert(instr.value);
            }
        }
        // Block params are always live (they are Phis).
        for param in &block.params {
            live.insert(param.value);
        }
    }

    // Step 3: Propagate liveness — if a live instruction uses V, V is live.
    // Iterate to fixpoint.
    let mut changed = true;
    while changed {
        changed = false;
        for block in &graph.blocks {
            for instr in &block.instrs {
                if !live.contains(&instr.value) {
                    continue;
                }
                let mut operands = Vec::new();
                collect_operands(&instr.op, &mut operands);
                for val in operands {
                    if live.insert(val) {
                        changed = true;
                    }
                }
            }
        }
    }

    // Step 4: Remove dead instructions (keep side-effectful ones even if unused).
    for block in &mut graph.blocks {
        block.instrs.retain(|instr| {
            live.contains(&instr.value) || has_side_effects(&instr.op)
        });
    }
}

/// Collect ValueId operands from a MirOp.
fn collect_operands(op: &MirOp, out: &mut impl Extend<ValueId>) {
    let mut vals = Vec::new();

    match op {
        MirOp::Const(_) | MirOp::Undefined | MirOp::Null | MirOp::True | MirOp::False
        | MirOp::ConstInt32(_) | MirOp::ConstFloat64(_) => {}

        MirOp::GuardInt32 { val, .. } | MirOp::GuardFloat64 { val, .. }
        | MirOp::GuardObject { val, .. } | MirOp::GuardString { val, .. }
        | MirOp::GuardFunction { val, .. } | MirOp::GuardBool { val, .. }
        | MirOp::GuardNotHole { val, .. } => { vals.push(*val); }

        MirOp::GuardShape { obj, .. } | MirOp::GuardArrayDense { obj, .. } => { vals.push(*obj); }
        MirOp::GuardProtoEpoch { .. } => {}
        MirOp::GuardBoundsCheck { arr, idx, .. } => { vals.push(*arr); vals.push(*idx); }

        MirOp::BoxInt32(v) | MirOp::BoxFloat64(v) | MirOp::BoxBool(v)
        | MirOp::UnboxInt32(v) | MirOp::UnboxFloat64(v)
        | MirOp::Int32ToFloat64(v) => { vals.push(*v); }

        MirOp::AddI32 { lhs, rhs, .. } | MirOp::SubI32 { lhs, rhs, .. }
        | MirOp::MulI32 { lhs, rhs, .. } | MirOp::DivI32 { lhs, rhs, .. }
        | MirOp::ModI32 { lhs, rhs, .. } => { vals.push(*lhs); vals.push(*rhs); }

        MirOp::IncI32 { val, .. } | MirOp::DecI32 { val, .. }
        | MirOp::NegI32 { val, .. } => { vals.push(*val); }

        MirOp::AddF64 { lhs, rhs } | MirOp::SubF64 { lhs, rhs }
        | MirOp::MulF64 { lhs, rhs } | MirOp::DivF64 { lhs, rhs }
        | MirOp::ModF64 { lhs, rhs } => { vals.push(*lhs); vals.push(*rhs); }
        MirOp::NegF64(v) => { vals.push(*v); }

        MirOp::BitAnd { lhs, rhs } | MirOp::BitOr { lhs, rhs }
        | MirOp::BitXor { lhs, rhs } | MirOp::Shl { lhs, rhs }
        | MirOp::Shr { lhs, rhs } | MirOp::Ushr { lhs, rhs } => { vals.push(*lhs); vals.push(*rhs); }
        MirOp::BitNot(v) => { vals.push(*v); }

        MirOp::CmpI32 { lhs, rhs, .. } | MirOp::CmpF64 { lhs, rhs, .. }
        | MirOp::CmpStrictEq { lhs, rhs } | MirOp::CmpStrictNe { lhs, rhs } => {
            vals.push(*lhs); vals.push(*rhs);
        }
        MirOp::LogicalNot(v) => { vals.push(*v); }

        MirOp::LoadLocal(_) | MirOp::LoadRegister(_) | MirOp::LoadThis => {}
        MirOp::StoreLocal { val, .. } | MirOp::StoreRegister { val, .. } => { vals.push(*val); }
        MirOp::LoadUpvalue { .. } | MirOp::CloseUpvalue { .. } => {}
        MirOp::StoreUpvalue { val, .. } => { vals.push(*val); }

        MirOp::Jump(_, args) => { vals.extend_from_slice(args); }
        MirOp::Branch { cond, true_args, false_args, .. } => {
            vals.push(*cond);
            vals.extend_from_slice(true_args);
            vals.extend_from_slice(false_args);
        }
        MirOp::Return(v) => { vals.push(*v); }
        MirOp::ReturnUndefined | MirOp::Deopt(_) => {}

        MirOp::Move(v) => { vals.push(*v); }
        MirOp::Phi(vs) => { vals.extend(vs.iter().map(|(_, v)| *v)); }

        // Catch-all: conservatively keep alive (has_side_effects handles liveness).
        _ => {}
    }

    out.extend(vals);
}

/// Whether an operation has side effects and must not be eliminated.
fn has_side_effects(op: &MirOp) -> bool {
    match op {
        // Pure computations: no side effects.
        MirOp::Const(_) | MirOp::Undefined | MirOp::Null | MirOp::True | MirOp::False
        | MirOp::ConstInt32(_) | MirOp::ConstFloat64(_) => false,

        // Guards have side effects (can deopt) — must not be eliminated.
        MirOp::GuardInt32 { .. } | MirOp::GuardFloat64 { .. }
        | MirOp::GuardObject { .. } | MirOp::GuardString { .. }
        | MirOp::GuardFunction { .. } | MirOp::GuardBool { .. }
        | MirOp::GuardShape { .. } | MirOp::GuardProtoEpoch { .. }
        | MirOp::GuardArrayDense { .. } | MirOp::GuardBoundsCheck { .. }
        | MirOp::GuardNotHole { .. } => true,

        MirOp::BoxInt32(_) | MirOp::BoxFloat64(_) | MirOp::BoxBool(_)
        | MirOp::UnboxInt32(_) | MirOp::UnboxFloat64(_)
        | MirOp::Int32ToFloat64(_) => false,

        // i32 arithmetic has overflow side effects (can deopt).
        MirOp::AddI32 { .. } | MirOp::SubI32 { .. } | MirOp::MulI32 { .. }
        | MirOp::DivI32 { .. } | MirOp::ModI32 { .. }
        | MirOp::IncI32 { .. } | MirOp::DecI32 { .. } | MirOp::NegI32 { .. } => true,

        MirOp::AddF64 { .. } | MirOp::SubF64 { .. } | MirOp::MulF64 { .. }
        | MirOp::DivF64 { .. } | MirOp::ModF64 { .. } | MirOp::NegF64(_) => false,

        MirOp::BitAnd { .. } | MirOp::BitOr { .. } | MirOp::BitXor { .. }
        | MirOp::Shl { .. } | MirOp::Shr { .. } | MirOp::Ushr { .. }
        | MirOp::BitNot(_) => false,

        MirOp::CmpI32 { .. } | MirOp::CmpF64 { .. }
        | MirOp::CmpStrictEq { .. } | MirOp::CmpStrictNe { .. }
        | MirOp::LogicalNot(_) => false,

        MirOp::LoadLocal(_) | MirOp::LoadRegister(_) | MirOp::LoadThis
        | MirOp::LoadUpvalue { .. } => false,

        // Stores have side effects.
        MirOp::StoreLocal { .. } | MirOp::StoreRegister { .. }
        | MirOp::StoreUpvalue { .. } | MirOp::CloseUpvalue { .. } => true,

        // Terminators have side effects.
        MirOp::Jump(_, _) | MirOp::Branch { .. } | MirOp::Return(_)
        | MirOp::ReturnUndefined | MirOp::Deopt(_) => true,

        MirOp::Move(_) | MirOp::Phi(_) => false,

        // Everything else: conservatively treat as side-effectful.
        _ => true,
    }
}
