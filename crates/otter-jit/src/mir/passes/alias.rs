//! Alias analysis — category-based effect tracking for MIR operations.
//!
//! Each MIR operation declares which heap categories it reads/writes/none.
//! This enables:
//! - **GVN**: two loads from the same source are CSE-able if no aliasing store between them
//! - **LICM**: an instruction is loop-invariant if no aliasing store in the loop body
//!
//! ## Categories (from SpiderMonkey's AliasSet)
//!
//! - `Element`: array elements (dense storage)
//! - `FixedSlot`: inline object properties (first 8 slots)
//! - `DynamicSlot`: overflow/dictionary properties
//! - `ObjectFields`: object metadata (shape, prototype, flags)
//! - `GlobalVar`: global variable bindings
//! - `StackSlot`: local variables / registers
//!
//! SM reference: `AliasSet` flags, `getAliasSet()` per MIR instruction.
//!
//! Spec: Phase 6.2 of JIT_INCREMENTAL_PLAN.md

use crate::mir::nodes::MirOp;

/// Alias categories — a bitset of heap regions an operation may access.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct AliasSet(pub u8);

impl AliasSet {
    /// No heap access (pure computation).
    pub const NONE: Self = Self(0);
    /// Array element storage.
    pub const ELEMENT: Self = Self(1 << 0);
    /// Inline (fixed) object property slots.
    pub const FIXED_SLOT: Self = Self(1 << 1);
    /// Overflow (dynamic) object property slots.
    pub const DYNAMIC_SLOT: Self = Self(1 << 2);
    /// Object metadata (shape, prototype, flags).
    pub const OBJECT_FIELDS: Self = Self(1 << 3);
    /// Global variable bindings.
    pub const GLOBAL_VAR: Self = Self(1 << 4);
    /// Local variable / register stack slots.
    pub const STACK_SLOT: Self = Self(1 << 5);
    /// Everything (conservative).
    pub const ALL: Self = Self(0x3F);

    /// Union of two alias sets.
    #[must_use]
    pub fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// Whether two alias sets may overlap.
    #[must_use]
    pub fn may_alias(self, other: Self) -> bool {
        (self.0 & other.0) != 0
    }

    /// Whether this is empty (no heap access).
    #[must_use]
    pub fn is_none(self) -> bool {
        self.0 == 0
    }
}

/// Effect declaration for a MIR operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Effects {
    /// Categories this operation may read from.
    pub reads: AliasSet,
    /// Categories this operation may write to.
    pub writes: AliasSet,
}

impl Effects {
    /// Pure: no reads or writes.
    pub const PURE: Self = Self {
        reads: AliasSet::NONE,
        writes: AliasSet::NONE,
    };

    /// Whether the operation is pure (no heap effects).
    #[must_use]
    pub fn is_pure(self) -> bool {
        self.reads.is_none() && self.writes.is_none()
    }

    /// Whether this operation may read from a category that another writes.
    #[must_use]
    pub fn may_depend_on(self, other: Self) -> bool {
        self.reads.may_alias(other.writes)
    }

    /// Whether this operation may write to a category that another reads or writes.
    #[must_use]
    pub fn may_conflict_with(self, other: Self) -> bool {
        self.writes.may_alias(other.reads) || self.writes.may_alias(other.writes)
    }
}

/// Get the alias effects for a MIR operation.
///
/// Each operation declares what heap categories it reads and writes.
/// Pure computations have `Effects::PURE`.
#[must_use]
pub fn get_effects(op: &MirOp) -> Effects {
    match op {
        // ---- Pure computations ----
        MirOp::Const(_) | MirOp::ConstInt32(_) | MirOp::ConstFloat64(_)
        | MirOp::True | MirOp::False | MirOp::Undefined | MirOp::Null => Effects::PURE,

        MirOp::BoxInt32(_) | MirOp::BoxFloat64(_) | MirOp::BoxBool(_)
        | MirOp::UnboxInt32(_) | MirOp::UnboxFloat64(_)
        | MirOp::Int32ToFloat64(_) => Effects::PURE,

        MirOp::AddI32 { .. } | MirOp::SubI32 { .. } | MirOp::MulI32 { .. }
        | MirOp::DivI32 { .. } | MirOp::ModI32 { .. }
        | MirOp::IncI32 { .. } | MirOp::DecI32 { .. } | MirOp::NegI32 { .. } => Effects::PURE,

        MirOp::AddF64 { .. } | MirOp::SubF64 { .. } | MirOp::MulF64 { .. }
        | MirOp::DivF64 { .. } | MirOp::ModF64 { .. } | MirOp::NegF64(_) => Effects::PURE,

        MirOp::BitAnd { .. } | MirOp::BitOr { .. } | MirOp::BitXor { .. }
        | MirOp::Shl { .. } | MirOp::Shr { .. } | MirOp::Ushr { .. }
        | MirOp::BitNot(_) => Effects::PURE,

        MirOp::CmpI32 { .. } | MirOp::CmpF64 { .. }
        | MirOp::CmpStrictEq { .. } | MirOp::CmpStrictNe { .. }
        | MirOp::LogicalNot(_) => Effects::PURE,

        MirOp::Move(_) | MirOp::Phi(_) => Effects::PURE,

        // ---- Stack slot access ----
        MirOp::LoadLocal(_) | MirOp::LoadRegister(_) | MirOp::LoadThis => Effects {
            reads: AliasSet::STACK_SLOT,
            writes: AliasSet::NONE,
        },
        MirOp::StoreLocal { .. } | MirOp::StoreRegister { .. } => Effects {
            reads: AliasSet::NONE,
            writes: AliasSet::STACK_SLOT,
        },

        // ---- Property access ----
        MirOp::GetPropShaped { inline: true, .. } => Effects {
            reads: AliasSet::FIXED_SLOT,
            writes: AliasSet::NONE,
        },
        MirOp::GetPropShaped { inline: false, .. } => Effects {
            reads: AliasSet::DYNAMIC_SLOT,
            writes: AliasSet::NONE,
        },
        MirOp::SetPropShaped { inline: true, .. } => Effects {
            reads: AliasSet::NONE,
            writes: AliasSet::FIXED_SLOT,
        },
        MirOp::SetPropShaped { inline: false, .. } => Effects {
            reads: AliasSet::NONE,
            writes: AliasSet::DYNAMIC_SLOT,
        },

        // Generic property access: conservative (may touch any slot).
        MirOp::GetPropGeneric { .. } | MirOp::GetPropConstGeneric { .. } => Effects {
            reads: AliasSet::FIXED_SLOT.union(AliasSet::DYNAMIC_SLOT).union(AliasSet::OBJECT_FIELDS),
            writes: AliasSet::NONE,
        },
        MirOp::SetPropGeneric { .. } | MirOp::SetPropConstGeneric { .. } => Effects {
            reads: AliasSet::NONE,
            writes: AliasSet::FIXED_SLOT.union(AliasSet::DYNAMIC_SLOT).union(AliasSet::OBJECT_FIELDS),
        },

        // ---- Array access ----
        MirOp::GetElemDense { .. } => Effects {
            reads: AliasSet::ELEMENT,
            writes: AliasSet::NONE,
        },
        MirOp::SetElemDense { .. } => Effects {
            reads: AliasSet::NONE,
            writes: AliasSet::ELEMENT,
        },

        // ---- Global access ----
        MirOp::GetGlobal { .. } => Effects {
            reads: AliasSet::GLOBAL_VAR,
            writes: AliasSet::NONE,
        },
        MirOp::SetGlobal { .. } => Effects {
            reads: AliasSet::NONE,
            writes: AliasSet::GLOBAL_VAR,
        },

        // ---- Guards: read object fields (shape checks) but don't write ----
        MirOp::GuardShape { .. } | MirOp::GuardProtoEpoch { .. }
        | MirOp::GuardArrayDense { .. } => Effects {
            reads: AliasSet::OBJECT_FIELDS,
            writes: AliasSet::NONE,
        },
        MirOp::GuardInt32 { .. } | MirOp::GuardFloat64 { .. }
        | MirOp::GuardObject { .. } | MirOp::GuardString { .. }
        | MirOp::GuardFunction { .. } | MirOp::GuardBool { .. }
        | MirOp::GuardNotHole { .. } | MirOp::GuardBoundsCheck { .. } => Effects::PURE,

        // ---- Everything else: conservative ----
        _ => Effects {
            reads: AliasSet::ALL,
            writes: AliasSet::ALL,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pure_ops() {
        assert!(get_effects(&MirOp::ConstInt32(42)).is_pure());
        assert!(get_effects(&MirOp::True).is_pure());
        assert!(get_effects(&MirOp::AddF64 {
            lhs: crate::mir::graph::ValueId(0),
            rhs: crate::mir::graph::ValueId(1),
        }).is_pure());
    }

    #[test]
    fn test_property_effects() {
        let load_fixed = get_effects(&MirOp::GetPropShaped {
            obj: crate::mir::graph::ValueId(0),
            shape_id: 1,
            offset: 0,
            inline: true,
        });
        assert!(!load_fixed.is_pure());
        assert!(load_fixed.reads.may_alias(AliasSet::FIXED_SLOT));
        assert!(!load_fixed.reads.may_alias(AliasSet::ELEMENT));

        let store_fixed = get_effects(&MirOp::SetPropShaped {
            obj: crate::mir::graph::ValueId(0),
            shape_id: 1,
            offset: 0,
            val: crate::mir::graph::ValueId(1),
            inline: true,
        });
        assert!(store_fixed.writes.may_alias(AliasSet::FIXED_SLOT));
    }

    #[test]
    fn test_alias_conflict() {
        let load = get_effects(&MirOp::GetPropShaped {
            obj: crate::mir::graph::ValueId(0),
            shape_id: 1,
            offset: 0,
            inline: true,
        });
        let store = get_effects(&MirOp::SetPropShaped {
            obj: crate::mir::graph::ValueId(0),
            shape_id: 1,
            offset: 0,
            val: crate::mir::graph::ValueId(1),
            inline: true,
        });
        // Store to fixed slot conflicts with load from fixed slot.
        assert!(store.may_conflict_with(load));
        // Load depends on store.
        assert!(load.may_depend_on(store));
    }

    #[test]
    fn test_no_cross_category_alias() {
        let load_elem = get_effects(&MirOp::GetElemDense {
            arr: crate::mir::graph::ValueId(0),
            idx: crate::mir::graph::ValueId(1),
        });
        let store_slot = get_effects(&MirOp::SetPropShaped {
            obj: crate::mir::graph::ValueId(0),
            shape_id: 1,
            offset: 0,
            val: crate::mir::graph::ValueId(1),
            inline: true,
        });
        // Element load doesn't alias with fixed slot store.
        assert!(!load_elem.may_depend_on(store_slot));
    }
}
