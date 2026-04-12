//! Abstract interpretation — forward dataflow type analysis.
//!
//! Tracks the abstract type of every SSA value through the MIR graph.
//! When the abstract type proves a guard always succeeds, marks it as
//! `IsProved` so later passes (guard_elim) can eliminate it.
//!
//! ## Abstract Types
//!
//! Each value has an `AbstractType` which is a bitset of possible types:
//! `Int32 | Float64 | String | Object | Bool | Undefined | Null | Any`
//!
//! The analysis propagates types forward through the graph:
//! - `ConstInt32(n)` → type = Int32
//! - `GuardInt32(v)` → output type = Int32, input type narrowed
//! - `AddI32(a, b)` → type = Int32
//! - `BoxInt32(v)` → type = Tagged (could be any boxed value)
//! - `Phi(a, b)` → type = union(type(a), type(b))
//!
//! Inspired by JSC's CFA (Control Flow Analysis) and V8 Maglev's "Known Node Aspects".
//!
//! Spec: Phase 4.2 of JIT_INCREMENTAL_PLAN.md

use std::collections::HashMap;

use crate::mir::graph::{MirGraph, ValueId};
use crate::mir::nodes::MirOp;

/// Abstract type — a bitset of possible runtime types for a value.
///
/// Uses a compact u8 bitfield. Multiple bits set = union of types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct AbstractType(pub u8);

impl AbstractType {
    pub const BOTTOM: Self = Self(0);       // Unreachable / no info.
    pub const INT32: Self = Self(1 << 0);
    pub const FLOAT64: Self = Self(1 << 1);
    pub const STRING: Self = Self(1 << 2);
    pub const OBJECT: Self = Self(1 << 3);
    pub const BOOL: Self = Self(1 << 4);
    pub const UNDEFINED: Self = Self(1 << 5);
    pub const NULL: Self = Self(1 << 6);
    pub const ANY: Self = Self(0x7F);        // All bits set.

    /// Number = Int32 | Float64
    pub const NUMBER: Self = Self(Self::INT32.0 | Self::FLOAT64.0);

    /// Union of two abstract types.
    #[must_use]
    pub fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// Intersection of two abstract types.
    #[must_use]
    pub fn intersect(self, other: Self) -> Self {
        Self(self.0 & other.0)
    }

    /// Whether this type is a subset of another.
    #[must_use]
    pub fn is_subset_of(self, other: Self) -> bool {
        (self.0 & other.0) == self.0
    }

    /// Whether this is exactly one type (power of 2).
    #[must_use]
    pub fn is_concrete(self) -> bool {
        self.0 != 0 && (self.0 & (self.0 - 1)) == 0
    }

    /// Whether a GuardInt32 would always succeed for this type.
    #[must_use]
    pub fn proves_int32(self) -> bool {
        self.is_subset_of(Self::INT32)
    }

    /// Whether a GuardFloat64 would always succeed.
    #[must_use]
    pub fn proves_float64(self) -> bool {
        self.is_subset_of(Self::FLOAT64)
    }

    /// Whether a GuardObject would always succeed.
    #[must_use]
    pub fn proves_object(self) -> bool {
        self.is_subset_of(Self::OBJECT)
    }

    /// Whether a GuardBool would always succeed.
    #[must_use]
    pub fn proves_bool(self) -> bool {
        self.is_subset_of(Self::BOOL)
    }
}

impl std::fmt::Display for AbstractType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if *self == Self::BOTTOM { return write!(f, "bottom"); }
        if *self == Self::ANY { return write!(f, "any"); }
        let mut parts = Vec::new();
        if self.0 & Self::INT32.0 != 0 { parts.push("i32"); }
        if self.0 & Self::FLOAT64.0 != 0 { parts.push("f64"); }
        if self.0 & Self::STRING.0 != 0 { parts.push("str"); }
        if self.0 & Self::OBJECT.0 != 0 { parts.push("obj"); }
        if self.0 & Self::BOOL.0 != 0 { parts.push("bool"); }
        if self.0 & Self::UNDEFINED.0 != 0 { parts.push("undef"); }
        if self.0 & Self::NULL.0 != 0 { parts.push("null"); }
        write!(f, "{}", parts.join("|"))
    }
}

/// Result of type analysis: a map from ValueId to AbstractType.
pub type TypeMap = HashMap<ValueId, AbstractType>;

/// Run forward type analysis on the MIR graph.
///
/// Returns a type map that can be used by guard_elim and repr_propagation.
/// Also annotates the graph's type cache with refined types.
pub fn run(graph: &mut MirGraph) -> TypeMap {
    let mut types: TypeMap = HashMap::new();

    // Initialize block params as ANY (join points).
    for block in &graph.blocks {
        for param in &block.params {
            types.insert(param.value, AbstractType::ANY);
        }
    }

    // Forward pass: compute types for all instructions.
    // Single pass is sufficient for acyclic regions. For loops, we'd need fixpoint.
    // For now, single pass with loop-back defaults to ANY.
    for block in &graph.blocks {
        for instr in &block.instrs {
            let ty = infer_type(&instr.op, &types);
            types.insert(instr.value, ty);
        }
    }

    // Update the graph's type cache with the analysis results.
    for (&val, &ty) in &types {
        if ty.is_concrete() {
            // Map abstract type to MIR type.
            let mir_ty = if ty == AbstractType::INT32 {
                crate::mir::types::MirType::Int32
            } else if ty == AbstractType::FLOAT64 {
                crate::mir::types::MirType::Float64
            } else if ty == AbstractType::BOOL {
                crate::mir::types::MirType::Bool
            } else {
                continue; // Don't override with Boxed.
            };
            graph.set_value_type(val, mir_ty);
        }
    }

    types
}

/// Infer the abstract type of a MIR operation from its operands' types.
fn infer_type(op: &MirOp, types: &TypeMap) -> AbstractType {
    match op {
        // ---- Constants ----
        MirOp::ConstInt32(_) => AbstractType::INT32,
        MirOp::ConstFloat64(_) => AbstractType::FLOAT64,
        MirOp::True | MirOp::False => AbstractType::BOOL,
        MirOp::Undefined => AbstractType::UNDEFINED,
        MirOp::Null => AbstractType::NULL,
        MirOp::Const(_) => AbstractType::ANY, // Could be anything.

        // ---- Guards narrow type ----
        MirOp::GuardInt32 { .. } => AbstractType::INT32,
        MirOp::GuardFloat64 { .. } => AbstractType::FLOAT64,
        MirOp::GuardObject { .. } => AbstractType::OBJECT,
        MirOp::GuardString { .. } => AbstractType::STRING,
        MirOp::GuardFunction { .. } => AbstractType::OBJECT, // Functions are objects.
        MirOp::GuardBool { .. } => AbstractType::BOOL,

        // ---- Boxing: output is tagged (ANY for boxed representation) ----
        MirOp::BoxInt32(_) | MirOp::BoxFloat64(_) | MirOp::BoxBool(_) => AbstractType::ANY,

        // ---- Unboxing: output is the unboxed type ----
        MirOp::UnboxInt32(_) => AbstractType::INT32,
        MirOp::UnboxFloat64(_) => AbstractType::FLOAT64,
        MirOp::Int32ToFloat64(_) => AbstractType::FLOAT64,

        // ---- i32 arithmetic: output is i32 ----
        MirOp::AddI32 { .. } | MirOp::SubI32 { .. } | MirOp::MulI32 { .. }
        | MirOp::DivI32 { .. } | MirOp::ModI32 { .. }
        | MirOp::IncI32 { .. } | MirOp::DecI32 { .. } | MirOp::NegI32 { .. } => {
            AbstractType::INT32
        }

        // ---- f64 arithmetic: output is f64 ----
        MirOp::AddF64 { .. } | MirOp::SubF64 { .. } | MirOp::MulF64 { .. }
        | MirOp::DivF64 { .. } | MirOp::ModF64 { .. } | MirOp::NegF64(_) => {
            AbstractType::FLOAT64
        }

        // ---- Bitwise: always i32 ----
        MirOp::BitAnd { .. } | MirOp::BitOr { .. } | MirOp::BitXor { .. }
        | MirOp::Shl { .. } | MirOp::Shr { .. } | MirOp::Ushr { .. }
        | MirOp::BitNot(_) => AbstractType::INT32,

        // ---- Comparisons: output is bool ----
        MirOp::CmpI32 { .. } | MirOp::CmpF64 { .. }
        | MirOp::CmpStrictEq { .. } | MirOp::CmpStrictNe { .. }
        | MirOp::LogicalNot(_) => AbstractType::BOOL,

        // ---- Move: inherits source type ----
        MirOp::Move(src) => types.get(src).copied().unwrap_or(AbstractType::ANY),

        // ---- Phi: union of all input types ----
        MirOp::Phi(inputs) => {
            let mut result = AbstractType::BOTTOM;
            for (_, val) in inputs {
                let input_ty = types.get(val).copied().unwrap_or(AbstractType::ANY);
                result = result.union(input_ty);
            }
            result
        }

        // ---- Loads: tagged (unknown type without further analysis) ----
        MirOp::LoadLocal(_) | MirOp::LoadRegister(_) | MirOp::LoadThis
        | MirOp::LoadUpvalue { .. } => AbstractType::ANY,

        // ---- Property access: tagged result ----
        MirOp::GetPropShaped { .. } | MirOp::GetPropGeneric { .. }
        | MirOp::GetPropConstGeneric { .. } => AbstractType::ANY,

        // ---- Everything else: conservatively ANY ----
        _ => AbstractType::ANY,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_abstract_type_basics() {
        assert!(AbstractType::INT32.proves_int32());
        assert!(!AbstractType::FLOAT64.proves_int32());
        assert!(!AbstractType::NUMBER.proves_int32()); // NUMBER = i32 | f64
        assert!(AbstractType::INT32.is_concrete());
        assert!(!AbstractType::NUMBER.is_concrete());
        assert!(AbstractType::INT32.is_subset_of(AbstractType::NUMBER));
    }

    #[test]
    fn test_abstract_type_union() {
        let a = AbstractType::INT32;
        let b = AbstractType::FLOAT64;
        let u = a.union(b);
        assert_eq!(u, AbstractType::NUMBER);
        assert!(!u.is_concrete());
    }

    #[test]
    fn test_abstract_type_display() {
        assert_eq!(format!("{}", AbstractType::INT32), "i32");
        assert_eq!(format!("{}", AbstractType::NUMBER), "i32|f64");
        assert_eq!(format!("{}", AbstractType::ANY), "any");
        assert_eq!(format!("{}", AbstractType::BOTTOM), "bottom");
    }

    #[test]
    fn test_type_analysis_simple() {
        use crate::mir::graph::{MirGraph, DeoptId, DeoptInfo, ResumeMode};

        let mut graph = MirGraph::new("test".into(), 2, 2, 0);
        let bb = graph.entry_block;

        // v0 = ConstInt32(42)
        let v0 = graph.push_instr(bb, MirOp::ConstInt32(42), 0);
        // v1 = ConstFloat64(3.14)
        let v1 = graph.push_instr(bb, MirOp::ConstFloat64(3.14), 1);
        // v2 = True
        let v2 = graph.push_instr(bb, MirOp::True, 2);
        // v3 = BoxInt32(v0)
        let v3 = graph.push_instr(bb, MirOp::BoxInt32(v0), 3);
        // v4 = GuardInt32(v3)
        let deopt = graph.create_deopt(DeoptInfo {
            bytecode_pc: 0,
            live_state: vec![],
            resume_mode: ResumeMode::ResumeAtPc,
        });
        let v4 = graph.push_instr(bb, MirOp::GuardInt32 { val: v3, deopt }, 4);
        // v5 = AddI32(v0, v4)
        let v5 = graph.push_instr(bb, MirOp::AddI32 { lhs: v0, rhs: v4, deopt }, 5);
        // terminator
        graph.push_instr(bb, MirOp::Return(v5), 6);

        let type_map = run(&mut graph);

        assert_eq!(type_map[&v0], AbstractType::INT32);
        assert_eq!(type_map[&v1], AbstractType::FLOAT64);
        assert_eq!(type_map[&v2], AbstractType::BOOL);
        assert_eq!(type_map[&v3], AbstractType::ANY); // BoxInt32 → tagged
        assert_eq!(type_map[&v4], AbstractType::INT32); // GuardInt32 → i32
        assert_eq!(type_map[&v5], AbstractType::INT32); // AddI32 → i32
    }
}
