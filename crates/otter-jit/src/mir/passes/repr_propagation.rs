//! Representation propagation — choose optimal representation per value.
//!
//! For each SSA value, decide whether it should be stored as:
//! - `Int32`: unboxed 32-bit integer in a GP register
//! - `Float64`: unboxed 64-bit float in an FP register
//! - `Tagged`: NaN-boxed value in a GP register
//!
//! Box/Unbox instructions are inserted only at representation mismatches.
//! Phi nodes are assigned the representation that matches the majority of
//! their inputs (like V8 Maglev's "Phi Representation Selection").
//!
//! ## Algorithm
//!
//! 1. Run type analysis to get `AbstractType` per value.
//! 2. For each value, choose the "natural" representation:
//!    - Constants, arithmetic results, guard outputs → their concrete type
//!    - Phi → union of input representations
//!    - Loads → Tagged (no type info without speculation)
//! 3. At each use site, if the input's repr doesn't match the expected repr,
//!    insert a conversion (Box/Unbox/Int32ToFloat64).
//!
//! Spec: Phase 4.3 of JIT_INCREMENTAL_PLAN.md

use std::collections::HashMap;

use crate::mir::graph::{MirGraph, ValueId};
use crate::mir::nodes::MirOp;
use crate::mir::types::MirType;

use super::type_analysis::{self, AbstractType, TypeMap};

/// Representation assigned to a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Repr {
    /// Unboxed Int32 (GP register).
    Int32,
    /// Unboxed Float64 (FP register).
    Float64,
    /// NaN-boxed tagged value (GP register, 64-bit).
    Tagged,
}

/// Representation map: ValueId → chosen representation.
pub type ReprMap = HashMap<ValueId, Repr>;

/// Run representation propagation on the MIR graph.
///
/// This computes the optimal representation for each value and updates
/// the graph's type information accordingly.
pub fn run(graph: &mut MirGraph) -> ReprMap {
    // Step 1: Get abstract types from type analysis.
    let type_map = type_analysis::run(graph);

    // Step 2: Assign representations based on abstract types.
    let mut repr_map = ReprMap::new();

    for block in &graph.blocks {
        // Block params: use the abstract type to choose repr.
        for param in &block.params {
            let repr = abstract_type_to_repr(
                type_map
                    .get(&param.value)
                    .copied()
                    .unwrap_or(AbstractType::ANY),
            );
            repr_map.insert(param.value, repr);
        }

        // Instructions.
        for instr in &block.instrs {
            let repr = choose_repr(&instr.op, &type_map, instr.value);
            repr_map.insert(instr.value, repr);
        }
    }

    // Step 3: Refine Phi representations.
    // If all Phi inputs have the same concrete repr, use that repr.
    // Otherwise, use Tagged (boxing happens at the inputs).
    let mut changed = true;
    while changed {
        changed = false;
        for block in &graph.blocks {
            for instr in &block.instrs {
                if let MirOp::Phi(inputs) = &instr.op {
                    let input_reprs: Vec<Repr> = inputs
                        .iter()
                        .map(|(_, v)| repr_map.get(v).copied().unwrap_or(Repr::Tagged))
                        .collect();

                    let new_repr = if input_reprs.iter().all(|r| *r == Repr::Int32) {
                        Repr::Int32
                    } else if input_reprs.iter().all(|r| *r == Repr::Float64) {
                        Repr::Float64
                    } else if input_reprs
                        .iter()
                        .all(|r| *r == Repr::Int32 || *r == Repr::Float64)
                    {
                        // Mixed numeric: promote to Float64.
                        Repr::Float64
                    } else {
                        Repr::Tagged
                    };

                    if repr_map.get(&instr.value) != Some(&new_repr) {
                        repr_map.insert(instr.value, new_repr);
                        changed = true;
                    }
                }
            }
        }
    }

    // Step 4: Update graph type cache with representation info.
    for (&val, &repr) in &repr_map {
        let mir_type = match repr {
            Repr::Int32 => MirType::Int32,
            Repr::Float64 => MirType::Float64,
            Repr::Tagged => MirType::Boxed,
        };
        // Only update if more specific than current type.
        if repr != Repr::Tagged {
            graph.set_value_type(val, mir_type);
        }
    }

    repr_map
}

/// Choose the natural representation for a MIR operation.
fn choose_repr(op: &MirOp, type_map: &TypeMap, value: ValueId) -> Repr {
    let abstract_ty = type_map.get(&value).copied().unwrap_or(AbstractType::ANY);

    match op {
        // ---- Constants: use their natural representation ----
        MirOp::ConstInt32(_) => Repr::Int32,
        MirOp::ConstFloat64(_) => Repr::Float64,
        MirOp::True | MirOp::False => Repr::Tagged, // Booleans stay boxed.
        MirOp::Undefined | MirOp::Null => Repr::Tagged,
        MirOp::Const(_) => Repr::Tagged,

        // ---- Guards: output is the unboxed type ----
        MirOp::GuardInt32 { .. } | MirOp::UnboxInt32(_) => Repr::Int32,
        MirOp::GuardFloat64 { .. } | MirOp::UnboxFloat64(_) => Repr::Float64,

        // ---- Boxing: output is tagged ----
        MirOp::BoxInt32(_) | MirOp::BoxFloat64(_) | MirOp::BoxBool(_) => Repr::Tagged,

        // ---- Type conversion ----
        MirOp::Int32ToFloat64(_) => Repr::Float64,

        // ---- i32 arithmetic ----
        MirOp::AddI32 { .. }
        | MirOp::SubI32 { .. }
        | MirOp::MulI32 { .. }
        | MirOp::DivI32 { .. }
        | MirOp::ModI32 { .. }
        | MirOp::IncI32 { .. }
        | MirOp::DecI32 { .. }
        | MirOp::NegI32 { .. } => Repr::Int32,

        // ---- f64 arithmetic ----
        MirOp::AddF64 { .. }
        | MirOp::SubF64 { .. }
        | MirOp::MulF64 { .. }
        | MirOp::DivF64 { .. }
        | MirOp::ModF64 { .. }
        | MirOp::NegF64(_) => Repr::Float64,

        // ---- Bitwise: always i32 ----
        MirOp::BitAnd { .. }
        | MirOp::BitOr { .. }
        | MirOp::BitXor { .. }
        | MirOp::Shl { .. }
        | MirOp::Shr { .. }
        | MirOp::Ushr { .. }
        | MirOp::BitNot(_) => Repr::Int32,

        // ---- Comparisons: result is boolean (tagged) ----
        MirOp::CmpI32 { .. }
        | MirOp::CmpF64 { .. }
        | MirOp::CmpStrictEq { .. }
        | MirOp::CmpStrictNe { .. }
        | MirOp::LogicalNot(_) => Repr::Tagged,

        // ---- Move: inherit source repr ----
        MirOp::Move(_) => abstract_type_to_repr(abstract_ty),

        // ---- Everything else: tagged ----
        _ => Repr::Tagged,
    }
}

/// Map an abstract type to its preferred representation.
fn abstract_type_to_repr(ty: AbstractType) -> Repr {
    if ty == AbstractType::INT32 {
        Repr::Int32
    } else if ty == AbstractType::FLOAT64 {
        Repr::Float64
    } else {
        Repr::Tagged
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mir::graph::{DeoptInfo, MirGraph, ResumeMode};
    use crate::mir::nodes::MirOp;

    #[test]
    fn test_repr_constants() {
        let mut graph = MirGraph::new("test".into(), 2, 2, 0);
        let bb = graph.entry_block;

        let v_i32 = graph.push_instr(bb, MirOp::ConstInt32(42), 0);
        let v_f64 = graph.push_instr(bb, MirOp::ConstFloat64(std::f64::consts::PI), 1);
        let v_true = graph.push_instr(bb, MirOp::True, 2);
        graph.push_instr(bb, MirOp::Return(v_i32), 3);

        let repr_map = run(&mut graph);

        assert_eq!(repr_map[&v_i32], Repr::Int32);
        assert_eq!(repr_map[&v_f64], Repr::Float64);
        assert_eq!(repr_map[&v_true], Repr::Tagged);
    }

    #[test]
    fn test_repr_arithmetic_chain() {
        let mut graph = MirGraph::new("test".into(), 2, 2, 0);
        let bb = graph.entry_block;
        let deopt = graph.create_deopt(DeoptInfo {
            bytecode_pc: 0,
            live_state: vec![],
            resume_mode: ResumeMode::ResumeAtPc,
        });

        // v0 = LoadLocal(0) → Tagged
        let v0 = graph.push_instr(bb, MirOp::LoadLocal(0), 0);
        // v1 = GuardInt32(v0) → Int32
        let v1 = graph.push_instr(bb, MirOp::GuardInt32 { val: v0, deopt }, 1);
        // v2 = ConstInt32(1)
        let v2 = graph.push_instr(bb, MirOp::ConstInt32(1), 2);
        // v3 = AddI32(v1, v2) → Int32
        let v3 = graph.push_instr(
            bb,
            MirOp::AddI32 {
                lhs: v1,
                rhs: v2,
                deopt,
            },
            3,
        );
        // v4 = BoxInt32(v3) → Tagged
        let v4 = graph.push_instr(bb, MirOp::BoxInt32(v3), 4);
        graph.push_instr(bb, MirOp::Return(v4), 5);

        let repr_map = run(&mut graph);

        assert_eq!(repr_map[&v0], Repr::Tagged);
        assert_eq!(repr_map[&v1], Repr::Int32);
        assert_eq!(repr_map[&v2], Repr::Int32);
        assert_eq!(repr_map[&v3], Repr::Int32);
        assert_eq!(repr_map[&v4], Repr::Tagged);
    }

    #[test]
    fn test_repr_move_inherits() {
        let mut graph = MirGraph::new("test".into(), 2, 2, 0);
        let bb = graph.entry_block;

        let v0 = graph.push_instr(bb, MirOp::ConstInt32(42), 0);
        let v1 = graph.push_instr(bb, MirOp::Move(v0), 1);
        graph.push_instr(bb, MirOp::Return(v1), 2);

        let repr_map = run(&mut graph);

        // Move should inherit Int32 from ConstInt32.
        assert_eq!(repr_map[&v0], Repr::Int32);
        assert_eq!(repr_map[&v1], Repr::Int32);
    }
}
