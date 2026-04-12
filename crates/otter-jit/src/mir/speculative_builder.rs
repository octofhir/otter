//! Speculative MIR builder — feedback-driven graph construction.
//!
//! Unlike the baseline builder which emits generic operations (guard every time,
//! box/unbox every result), the speculative builder consumes runtime feedback
//! (from `FeedbackVector`) and emits specialized nodes directly.
//!
//! ## Maglev's Key Insight
//!
//! "Specialize during graph building, not in later passes."
//!
//! When the builder sees `Add` with feedback `ArithmeticFeedback::Int32`:
//! - Baseline: `LoadLocal → GuardInt32 → LoadLocal → GuardInt32 → AddI32 → BoxInt32 → StoreLocal`
//! - Speculative: `LoadLocal → GuardInt32 → LoadLocal → GuardInt32 → AddI32 → StoreLocal`
//!   (skip boxing if the store target is also consumed as Int32)
//!
//! When feedback says a property access is monomorphic:
//! - Baseline: `GetPropGeneric` (cold helper call)
//! - Speculative: `GuardShape → LoadFixedSlot` (inline fast path)
//!
//! ## Speculate only when confident
//!
//! JSC rule: only speculate when success probability p ~ 0.9994.
//! We speculate based on feedback lattice state, not counters.
//!
//! Spec: Phase 4.1 of JIT_INCREMENTAL_PLAN.md

use otter_vm::feedback::{
    ArithmeticFeedback, ComparisonFeedback, FeedbackSlotData, FeedbackSlotId, FeedbackVector,
};

use crate::mir::graph::{DeoptInfo, MirGraph, ResumeMode, ValueId};
use crate::mir::nodes::MirOp;
use crate::mir::types::CmpOp;

/// Speculation decision for an arithmetic site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArithSpeculation {
    /// Emit typed Int32 path with overflow deopt.
    Int32,
    /// Emit typed Float64 path.
    Float64,
    /// No speculation — emit generic helper call.
    Generic,
}

/// Speculation decision for a comparison site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmpSpeculation {
    Int32,
    Float64,
    Generic,
}

/// Speculation decision for a property access site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PropSpeculation {
    /// Monomorphic: emit shape guard + fixed slot load.
    Monomorphic { shape_id: u64, slot_index: u16 },
    /// Too polymorphic — emit generic access.
    Generic,
}

/// Decide arithmetic speculation from feedback.
#[must_use]
pub fn decide_arithmetic(feedback: &FeedbackVector, slot: FeedbackSlotId) -> ArithSpeculation {
    match feedback.get(slot) {
        Some(FeedbackSlotData::Arithmetic(ArithmeticFeedback::Int32)) => ArithSpeculation::Int32,
        Some(FeedbackSlotData::Arithmetic(ArithmeticFeedback::Number)) => ArithSpeculation::Float64,
        _ => ArithSpeculation::Generic,
    }
}

/// Decide comparison speculation from feedback.
#[must_use]
pub fn decide_comparison(feedback: &FeedbackVector, slot: FeedbackSlotId) -> CmpSpeculation {
    match feedback.get(slot) {
        Some(FeedbackSlotData::Comparison(ComparisonFeedback::Int32)) => CmpSpeculation::Int32,
        Some(FeedbackSlotData::Comparison(ComparisonFeedback::Number)) => CmpSpeculation::Float64,
        _ => CmpSpeculation::Generic,
    }
}

/// Decide property access speculation from feedback.
#[must_use]
pub fn decide_property(feedback: &FeedbackVector, slot: FeedbackSlotId) -> PropSpeculation {
    match feedback.get(slot) {
        Some(FeedbackSlotData::Property(prop_fb)) => {
            if let Some(cache) = prop_fb.as_monomorphic() {
                PropSpeculation::Monomorphic {
                    shape_id: cache.shape_id().0,
                    slot_index: cache.slot_index(),
                }
            } else {
                PropSpeculation::Generic
            }
        }
        _ => PropSpeculation::Generic,
    }
}

/// Emit a speculative Int32 binary operation (guard + typed op + overflow deopt).
///
/// Returns the unboxed Int32 result value.
pub fn emit_speculative_i32_binary(
    graph: &mut MirGraph,
    block: crate::mir::graph::BlockId,
    pc: u32,
    lhs_boxed: ValueId,
    rhs_boxed: ValueId,
    make_op: fn(ValueId, ValueId, crate::mir::graph::DeoptId) -> MirOp,
) -> ValueId {
    let deopt = graph.create_deopt(DeoptInfo {
        bytecode_pc: pc,
        live_state: Vec::new(),
        resume_mode: ResumeMode::ResumeAtPc,
    });

    // Guard both operands are Int32.
    let lhs_i32 = graph.push_instr(block, MirOp::GuardInt32 { val: lhs_boxed, deopt }, pc);
    let rhs_i32 = graph.push_instr(block, MirOp::GuardInt32 { val: rhs_boxed, deopt }, pc);

    // Typed operation with overflow deopt.
    graph.push_instr(block, make_op(lhs_i32, rhs_i32, deopt), pc)
}

/// Emit a speculative Float64 binary operation.
pub fn emit_speculative_f64_binary(
    graph: &mut MirGraph,
    block: crate::mir::graph::BlockId,
    pc: u32,
    lhs_boxed: ValueId,
    rhs_boxed: ValueId,
    make_op: fn(ValueId, ValueId) -> MirOp,
) -> ValueId {
    let deopt = graph.create_deopt(DeoptInfo {
        bytecode_pc: pc,
        live_state: Vec::new(),
        resume_mode: ResumeMode::ResumeAtPc,
    });

    let lhs_f64 = graph.push_instr(block, MirOp::GuardFloat64 { val: lhs_boxed, deopt }, pc);
    let rhs_f64 = graph.push_instr(block, MirOp::GuardFloat64 { val: rhs_boxed, deopt }, pc);

    graph.push_instr(block, make_op(lhs_f64, rhs_f64), pc)
}

/// Emit a speculative monomorphic property load (shape guard + slot load).
pub fn emit_speculative_prop_load(
    graph: &mut MirGraph,
    block: crate::mir::graph::BlockId,
    pc: u32,
    obj_boxed: ValueId,
    shape_id: u64,
    slot_offset: u32,
) -> ValueId {
    let deopt = graph.create_deopt(DeoptInfo {
        bytecode_pc: pc,
        live_state: Vec::new(),
        resume_mode: ResumeMode::ResumeAtPc,
    });

    // Guard that the receiver is an object.
    let obj = graph.push_instr(block, MirOp::GuardObject { val: obj_boxed, deopt }, pc);

    // Guard the shape matches.
    graph.push_instr(block, MirOp::GuardShape { obj, shape_id, deopt }, pc);

    // Load from the known slot offset.
    graph.push_instr(
        block,
        MirOp::GetPropShaped {
            obj,
            shape_id,
            offset: slot_offset,
            inline: slot_offset < 64, // 8 inline slots * 8 bytes
        },
        pc,
    )
}

/// Emit a speculative Int32 comparison.
pub fn emit_speculative_i32_cmp(
    graph: &mut MirGraph,
    block: crate::mir::graph::BlockId,
    pc: u32,
    lhs_boxed: ValueId,
    rhs_boxed: ValueId,
    cmp_op: CmpOp,
) -> ValueId {
    let deopt = graph.create_deopt(DeoptInfo {
        bytecode_pc: pc,
        live_state: Vec::new(),
        resume_mode: ResumeMode::ResumeAtPc,
    });

    let lhs_i32 = graph.push_instr(block, MirOp::GuardInt32 { val: lhs_boxed, deopt }, pc);
    let rhs_i32 = graph.push_instr(block, MirOp::GuardInt32 { val: rhs_boxed, deopt }, pc);

    graph.push_instr(
        block,
        MirOp::CmpI32 {
            op: cmp_op,
            lhs: lhs_i32,
            rhs: rhs_i32,
        },
        pc,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mir::graph::MirGraph;
    use crate::mir::nodes::MirOp;
    use otter_vm::feedback::*;

    fn make_feedback_int32_add() -> FeedbackVector {
        let layout = FeedbackTableLayout::new(vec![
            FeedbackSlotLayout::new(FeedbackSlotId(0), FeedbackKind::Arithmetic),
        ]);
        let mut fv = FeedbackVector::from_layout(&layout);
        fv.record_arithmetic(FeedbackSlotId(0), ArithmeticFeedback::Int32);
        fv
    }

    #[test]
    fn test_decide_arithmetic_int32() {
        let fv = make_feedback_int32_add();
        assert_eq!(decide_arithmetic(&fv, FeedbackSlotId(0)), ArithSpeculation::Int32);
    }

    #[test]
    fn test_decide_arithmetic_no_feedback() {
        let fv = FeedbackVector::empty();
        assert_eq!(decide_arithmetic(&fv, FeedbackSlotId(0)), ArithSpeculation::Generic);
    }

    #[test]
    fn test_emit_speculative_i32_add() {
        let mut graph = MirGraph::new("test".into(), 2, 4, 0);
        let bb = graph.entry_block;

        // Simulate: LoadLocal(0) + LoadLocal(1) with Int32 feedback.
        let lhs = graph.push_instr(bb, MirOp::LoadLocal(0), 0);
        let rhs = graph.push_instr(bb, MirOp::LoadLocal(1), 1);

        let result = emit_speculative_i32_binary(
            &mut graph, bb, 2, lhs, rhs,
            |l, r, d| MirOp::AddI32 { lhs: l, rhs: r, deopt: d },
        );

        // Should emit: GuardInt32(lhs), GuardInt32(rhs), AddI32
        let instrs = &graph.block(bb).instrs;
        assert!(matches!(instrs[2].op, MirOp::GuardInt32 { .. })); // Guard lhs
        assert!(matches!(instrs[3].op, MirOp::GuardInt32 { .. })); // Guard rhs
        assert!(matches!(instrs[4].op, MirOp::AddI32 { .. }));     // Typed add
        assert_eq!(instrs[4].value, result);
    }

    #[test]
    fn test_emit_speculative_prop_load() {
        let mut graph = MirGraph::new("test".into(), 2, 4, 0);
        let bb = graph.entry_block;

        let obj = graph.push_instr(bb, MirOp::LoadLocal(0), 0);
        let result = emit_speculative_prop_load(&mut graph, bb, 1, obj, 42, 16);

        let instrs = &graph.block(bb).instrs;
        assert!(matches!(instrs[1].op, MirOp::GuardObject { .. }));
        assert!(matches!(instrs[2].op, MirOp::GuardShape { shape_id: 42, .. }));
        assert!(matches!(instrs[3].op, MirOp::GetPropShaped { shape_id: 42, offset: 16, .. }));
        assert_eq!(instrs[3].value, result);
    }

    #[test]
    fn test_emit_speculative_i32_cmp() {
        let mut graph = MirGraph::new("test".into(), 2, 4, 0);
        let bb = graph.entry_block;

        let lhs = graph.push_instr(bb, MirOp::LoadLocal(0), 0);
        let rhs = graph.push_instr(bb, MirOp::LoadLocal(1), 1);
        let result = emit_speculative_i32_cmp(&mut graph, bb, 2, lhs, rhs, CmpOp::Lt);

        let instrs = &graph.block(bb).instrs;
        assert!(matches!(instrs[4].op, MirOp::CmpI32 { op: CmpOp::Lt, .. }));
        assert_eq!(instrs[4].value, result);
    }

    #[test]
    fn test_decide_property_monomorphic() {
        use otter_vm::object::ObjectShapeId;

        let layout = FeedbackTableLayout::new(vec![
            FeedbackSlotLayout::new(FeedbackSlotId(0), FeedbackKind::Property),
        ]);
        let mut fv = FeedbackVector::from_layout(&layout);
        fv.record_property(FeedbackSlotId(0), ObjectShapeId(42), 3);

        match decide_property(&fv, FeedbackSlotId(0)) {
            PropSpeculation::Monomorphic { shape_id, slot_index } => {
                assert_eq!(shape_id, 42);
                assert_eq!(slot_index, 3);
            }
            other => panic!("expected Monomorphic, got {other:?}"),
        }
    }
}
