//! Exhaustive optimizer-visible effects for Machine IR.
//!
//! # Contents
//! - [`MachineAliasClass`] and [`MachineAliasSet`] partition mutable runtime
//!   state conservatively.
//! - [`MachineEffects`] classifies every Machine opcode and descriptor-backed
//!   call for GVN, guard elimination, and later loop transforms.
//!
//! # Invariants
//! - Every opcode is classified by one exhaustive match; a new opcode cannot
//!   compile until its effect row is chosen.
//! - Unknown or reentrant calls invalidate every heap proof.
//! - Committed calls, allocations, stores, barriers, throwing operations,
//!   safepoints, and control nodes are never commoned.
//! - Alias sets are conservative: false overlap costs optimization but cannot
//!   make a heap proof survive an invalidating operation.
//! - SELF, closure-context and context-parent words are fixed for their
//!   activation or object and are pure values. Context slots form their own
//!   class, written only by slot stores, generated binding writes and calls,
//!   so object and element stores leave captured-binding reads available.
//!
//! # See also
//! - `super::gvn` consumes this table with dominator-scoped value numbers.

use otter_bytecode::opcode_schema::BindingSemantics;

use super::{CallDescriptor, CallEffects, ContextField, MachineOpcode, SafepointKind};

/// Mutable runtime state that may invalidate a Machine value or proof.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum MachineAliasClass {
    /// Hidden-class and fast-object state.
    Shape,
    /// Atom-to-slot descriptor and extensibility state.
    PropertyMetadata,
    /// Direct prototype links.
    Prototype,
    /// String-keyed value-slab contents.
    PropertyField,
    /// Indexed-view kind, extent, detachment, and backing metadata.
    ElementMetadata,
    /// Indexed element payloads.
    ElementField,
    /// Declarative/global binding cells and derived-this state.
    Binding,
    /// Per-scope context slot words. Parent and closure-context words are
    /// immutable and belong to no alias class.
    ContextSlot,
    /// Address-stable traced constant cells rewritten by moving collection.
    ConstantCell,
    /// Nursery frontier and unpublished receiver state.
    Allocation,
    /// Remembered-set and incremental-marking metadata.
    GcBarrier,
}

impl MachineAliasClass {
    pub(super) const COUNT: usize = 11;

    const fn bit(self) -> u16 {
        1 << self as u8
    }
}

/// Compact deterministic set of [`MachineAliasClass`] values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MachineAliasSet(u16);

impl MachineAliasSet {
    /// Empty alias set.
    pub const NONE: Self = Self(0);
    /// Every currently declared alias class.
    pub const ALL: Self = Self((1 << MachineAliasClass::COUNT) - 1);
    /// All JavaScript heap state.
    pub const HEAP: Self = Self::ALL;

    /// Construct a singleton alias set.
    #[must_use]
    pub const fn one(alias: MachineAliasClass) -> Self {
        Self(alias.bit())
    }

    /// Combine two alias sets.
    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// Whether the set is empty.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Whether the set contains one alias class.
    #[must_use]
    pub const fn contains(self, alias: MachineAliasClass) -> bool {
        self.0 & alias.bit() != 0
    }

    /// Whether two sets overlap.
    #[must_use]
    pub const fn intersects(self, other: Self) -> bool {
        self.0 & other.0 != 0
    }
}

/// Whether GVN may reuse the result or successful proof of an instruction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MachineCommoning {
    /// The instruction is observable, source-identity-bearing, or control.
    Never,
    /// Ordinary output values may be replaced by a dominating equivalent.
    Value,
    /// Outputs or a no-output successful proof may be reused.
    Guard,
}

/// Complete effect row used by Machine graph optimizations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MachineEffects {
    /// Alias classes read by the operation.
    pub reads: MachineAliasSet,
    /// Alias classes written by the operation.
    pub writes: MachineAliasSet,
    /// The operation may reserve or publish GC-managed storage.
    pub allocates: bool,
    /// The operation can complete by throwing rather than by a pre-effect exit.
    pub throws: bool,
    /// The operation can stop for moving collection.
    pub safepoint: bool,
    /// The operation may execute arbitrary JavaScript.
    pub reentrant: bool,
    /// Legal value/proof commoning class.
    pub commoning: MachineCommoning,
}

impl MachineEffects {
    const PURE_VALUE: Self = Self::pure(MachineCommoning::Value);
    const PURE_GUARD: Self = Self::pure(MachineCommoning::Guard);
    const NEVER: Self = Self::pure(MachineCommoning::Never);

    const fn pure(commoning: MachineCommoning) -> Self {
        Self {
            reads: MachineAliasSet::NONE,
            writes: MachineAliasSet::NONE,
            allocates: false,
            throws: false,
            safepoint: false,
            reentrant: false,
            commoning,
        }
    }

    const fn read(reads: MachineAliasSet, commoning: MachineCommoning) -> Self {
        Self {
            reads,
            commoning,
            ..Self::NEVER
        }
    }

    const fn write(writes: MachineAliasSet) -> Self {
        Self {
            writes,
            ..Self::NEVER
        }
    }

    const fn allocation(reads: MachineAliasSet) -> Self {
        Self {
            reads,
            writes: MachineAliasSet::one(MachineAliasClass::Allocation),
            allocates: true,
            ..Self::NEVER
        }
    }

    /// A boundary invalidates all dependency epochs even when its alias set is
    /// empty, because success cannot prove that arbitrary runtime state stayed
    /// unchanged.
    #[must_use]
    pub const fn invalidates_dependency_epoch(self) -> bool {
        self.allocates || self.throws || self.safepoint || self.reentrant
    }
}

const SHAPE: MachineAliasSet = MachineAliasSet::one(MachineAliasClass::Shape);
const PROPERTY_METADATA: MachineAliasSet =
    MachineAliasSet::one(MachineAliasClass::PropertyMetadata);
const PROTOTYPE: MachineAliasSet = MachineAliasSet::one(MachineAliasClass::Prototype);
const PROPERTY_FIELD: MachineAliasSet = MachineAliasSet::one(MachineAliasClass::PropertyField);
const ELEMENT_METADATA: MachineAliasSet = MachineAliasSet::one(MachineAliasClass::ElementMetadata);
const ELEMENT_FIELD: MachineAliasSet = MachineAliasSet::one(MachineAliasClass::ElementField);
const BINDING: MachineAliasSet = MachineAliasSet::one(MachineAliasClass::Binding);
const CONTEXT_SLOT: MachineAliasSet = MachineAliasSet::one(MachineAliasClass::ContextSlot);
const CONSTANT_CELL: MachineAliasSet = MachineAliasSet::one(MachineAliasClass::ConstantCell);
const ALLOCATION: MachineAliasSet = MachineAliasSet::one(MachineAliasClass::Allocation);
const GC_BARRIER: MachineAliasSet = MachineAliasSet::one(MachineAliasClass::GcBarrier);

impl MachineOpcode {
    /// Whether this is an element view whose base is a typed array's off-heap
    /// storage. A moving collection never changes such a base or its length;
    /// only JavaScript reentry (detach, resize, transfer) or a write of the
    /// receiver's shape or element metadata can, so the view's proof outlives
    /// safepoints and allocations that end every in-heap address.
    #[must_use]
    pub fn is_off_heap_element_view(&self) -> bool {
        matches!(
            self,
            Self::ElementView { access, .. }
                if matches!(access.base, otter_vm::JitElementBase::ThroughLocalBuffer { .. })
        )
    }

    /// Return the opcode-local effect row. Descriptor-backed call effects are
    /// completed by [`effects_for_instruction`].
    #[must_use]
    pub const fn effects(&self) -> MachineEffects {
        use MachineCommoning::{Guard, Never, Value};
        match self {
            Self::EntryValue(_)
            | Self::EntryThis
            // SELF, a closure's context word and a context's parent word are
            // fixed for the activation and the object's life respectively.
            | Self::EntryCallee
            | Self::ContextLoad {
                field: ContextField::ClosureContext | ContextField::Parent,
            }
            | Self::TaggedIsNotHole
            | Self::AllocationHit
            | Self::BaseConstructResult
            | Self::TaggedConstant(_)
            | Self::IntegerConstant(_)
            | Self::FloatConstant(_)
            | Self::IntegerAdd
            | Self::IntegerSub
            | Self::IntegerMul
            | Self::IntegerNeg
            | Self::IntegerAddImmediate(_)
            | Self::IntegerSubImmediate(_)
            | Self::IntegerAddWrapping
            | Self::IntegerSubWrapping
            | Self::IntegerAnd
            | Self::IntegerOr
            | Self::IntegerXor
            | Self::IntegerShiftLeft
            | Self::IntegerShiftRight
            | Self::IntegerShiftRightLogical
            | Self::IntegerNot
            | Self::IntegerAndImmediate(_)
            | Self::IntegerLessThanImmediate(_)
            | Self::IntegerEqualImmediate(_)
            | Self::IntegerNotEqualImmediate(_)
            | Self::IntegerEqual
            | Self::IntegerNotEqual
            | Self::IntegerLessThan
            | Self::IntegerLessEqual
            | Self::IntegerGreaterThan
            | Self::IntegerGreaterEqual
            | Self::Int32ToFloat64
            | Self::Uint32ToFloat64
            | Self::Float64ToInt32
            | Self::BooleanToInt32
            | Self::FloatAdd
            | Self::FloatSub
            | Self::FloatMul
            | Self::FloatDiv
            | Self::FloatNeg
            | Self::IntegerToBoolean
            | Self::FloatToBoolean
            | Self::BooleanNot
            | Self::FloatLessThan
            | Self::FloatEqual
            | Self::FloatNotEqual
            | Self::FloatLessEqual
            | Self::FloatGreaterThan
            | Self::FloatGreaterEqual
            | Self::BoxNumber
            | Self::BoxInt32
            | Self::BoxUint32
            | Self::BoxBoolean
            | Self::BindingJoin { .. }
            | Self::ElementAddress { .. }
            | Self::BooleanConstant(_)
            | Self::BooleanOr
            | Self::TaggedSelect
            | Self::NativeInt32Math { .. }
            | Self::CacheIrJoin { .. }
            // Decodes value bits only: a cell operand misses without a load.
            | Self::BinaryNumberProbe { .. } => MachineEffects::PURE_VALUE,

            Self::DecodeNumber | Self::DecodeInt32 | Self::GuardCondition => {
                MachineEffects::PURE_GUARD
            }

            Self::GuardCallTarget { .. } => {
                MachineEffects::read(SHAPE.union(PROTOTYPE).union(PROPERTY_METADATA), Guard)
            }
            Self::ResolveCallThis { .. } => MachineEffects::read(BINDING, Value),
            // The live callable's kind and native identity are read only after
            // its active guard. Heap invalidation must also invalidate this
            // proof; method lookup itself remains an explicit CacheIR load.
            Self::NativeLeafIdentity { .. } => MachineEffects::read(MachineAliasSet::HEAP, Guard),
            // A declared leaf reads its operands' bodies; an in-place entry
            // also writes them behind its own barrier. Neither allocates,
            // throws, reenters nor collects.
            Self::NativeLeafProbe { stub, .. } => {
                if super::native_leaf::leaf_probe_mutates(*stub) {
                    MachineEffects {
                        reads: MachineAliasSet::HEAP,
                        writes: MachineAliasSet::HEAP,
                        allocates: false,
                        reentrant: false,
                        throws: false,
                        safepoint: false,
                        commoning: Never,
                    }
                } else {
                    MachineEffects::read(MachineAliasSet::HEAP, Value)
                }
            }
            Self::ArgumentsReadProbe { .. } => MachineEffects {
                reads: BINDING,
                writes: MachineAliasSet::NONE,
                allocates: false,
                reentrant: false,
                throws: false,
                safepoint: false,
                commoning: Never,
            },
            Self::TruthinessProbe
            | Self::LooseEqualityProbe { .. }
            | Self::TaggedNullishEqual { .. } => MachineEffects::read(SHAPE, Guard),
            // Reads the target's bag slot and the value's prototype chain.
            Self::InstanceofProbe { .. } => MachineEffects::read(
                SHAPE
                    .union(PROPERTY_METADATA)
                    .union(PROPERTY_FIELD)
                    .union(PROTOTYPE),
                Guard,
            ),

            Self::AllocateObject { .. } => {
                MachineEffects::allocation(SHAPE.union(PROTOTYPE).union(ALLOCATION))
            }
            Self::PublishObject { .. } => MachineEffects::write(ALLOCATION),
            Self::TryBindDerivedThis { .. } => MachineEffects::write(BINDING),

            // These lower to helper calls. They remain explicit even though
            // their VM declarations are non-allocating leaves.
            Self::FloatRem | Self::FloatPow => MachineEffects::NEVER,

            // GVN reuses a binding guard only inside its own block: its raw
            // cell addresses must not outlive a safepoint. A context-slot
            // guard reads the slot for its hole test.
            Self::BindingGuard { .. } => MachineEffects::read(
                BINDING
                    .union(CONTEXT_SLOT)
                    .union(SHAPE)
                    .union(PROPERTY_METADATA),
                Guard,
            ),
            Self::BindingHit { semantics, .. } => match semantics {
                BindingSemantics::Read(_) => {
                    MachineEffects::read(BINDING.union(CONTEXT_SLOT).union(PROPERTY_FIELD), Value)
                }
                BindingSemantics::Write(_) | BindingSemantics::Delete(_) => {
                    MachineEffects::write(BINDING.union(CONTEXT_SLOT).union(PROPERTY_FIELD))
                }
            },
            Self::BindingWriteBarrier | Self::ContextWriteBarrier => {
                MachineEffects::write(GC_BARRIER)
            }
            Self::ContextLoad {
                field: ContextField::Slot(_),
            } => MachineEffects::read(CONTEXT_SLOT, Value),
            Self::ContextStore { .. } => MachineEffects::write(CONTEXT_SLOT),
            Self::StringConstantCellLoad { .. } => MachineEffects::read(CONSTANT_CELL, Value),

            Self::ElementView { .. } => MachineEffects::read(SHAPE.union(ELEMENT_METADATA), Guard),
            Self::ElementValueLoad { .. } => MachineEffects::read(ELEMENT_FIELD, Value),
            Self::ElementValueGuard { .. } => MachineEffects::read(ELEMENT_FIELD, Guard),
            Self::ElementValueStore { .. } => MachineEffects::write(ELEMENT_FIELD),
            Self::PropertySource { .. } => MachineEffects::NEVER,
            Self::CacheIrGuardShape { .. }
            | Self::CacheIrGuardDictionaryLayout { .. }
            | Self::CacheIrGuardOrdinaryState { .. }
            | Self::CacheIrGuardAtomSlot { .. }
            | Self::CacheIrGuardExtensible { .. }
            | Self::PropertyShapeProof { .. } => {
                MachineEffects::read(SHAPE.union(PROPERTY_METADATA).union(PROTOTYPE), Guard)
            }
            Self::CacheIrLoadPrototype { .. } => MachineEffects::read(PROTOTYPE, Value),
            Self::CacheIrLoadIntrinsicPrototype { .. } => {
                MachineEffects::read(PROPERTY_METADATA.union(PROTOTYPE), Guard)
            }
            Self::CacheIrGuardPrototypeNull { .. } => MachineEffects::read(PROTOTYPE, Guard),
            // The latch flips only when some object gains an indexed accessor,
            // a prototype-contents change that every reentrant or writing
            // boundary already invalidates.
            Self::CacheIrGuardArrayIndexProtector { .. } => {
                MachineEffects::read(PROTOTYPE.union(PROPERTY_METADATA), Guard)
            }
            Self::CacheIrLoadField { .. } | Self::PropertySlotLoad { .. } => {
                MachineEffects::read(PROPERTY_FIELD, Value)
            }
            Self::PropertyPolymorphicLoad { .. } | Self::PropertyMegamorphicLoad { .. } => {
                MachineEffects::read(
                SHAPE
                    .union(PROPERTY_METADATA)
                    .union(PROPERTY_FIELD)
                    .union(PROTOTYPE),
                Value,
                )
            }
            Self::PropertyMegamorphicStore { .. } => MachineEffects {
                reads: SHAPE.union(PROPERTY_METADATA).union(PROPERTY_FIELD),
                writes: PROPERTY_FIELD,
                allocates: false,
                reentrant: false,
                throws: false,
                safepoint: false,
                commoning: Never,
            },
            Self::CacheIrStoreField { .. } => MachineEffects::write(PROPERTY_FIELD),
            Self::PropertyStoreDispatch { .. } => MachineEffects {
                reads: SHAPE
                    .union(PROPERTY_METADATA)
                    .union(PROPERTY_FIELD)
                    .union(PROTOTYPE),
                writes: PROPERTY_FIELD.union(SHAPE).union(PROPERTY_METADATA),
                allocates: false,
                reentrant: false,
                throws: false,
                safepoint: false,
                commoning: Never,
            },
            Self::CacheIrPublishShape { .. } => {
                MachineEffects::write(SHAPE.union(PROPERTY_METADATA))
            }
            Self::CacheIrWriteBarrier { .. } => MachineEffects::write(GC_BARRIER),
            Self::ExoticLength { .. } => {
                MachineEffects::read(SHAPE.union(ELEMENT_METADATA).union(PROPERTY_FIELD), Value)
            }

            // The batched slow edge is a LeafNoAlloc budget/interrupt check.
            // It may stop execution, but a successful return cannot collect,
            // reenter JavaScript, or mutate JavaScript heap state.
            Self::BackedgePoll => MachineEffects {
                throws: true,
                ..MachineEffects::NEVER
            },
            // An OSR frame read leaves the function before any effect when the
            // interpreter's value is outside the header's representation.
            Self::OsrValue { .. } => MachineEffects {
                throws: true,
                ..MachineEffects::NEVER
            },
            Self::Call(_)
            | Self::OsrDispatch { .. }
            | Self::LoopPreheader
            | Self::Jump
            | Self::BranchIf(_)
            | Self::BranchNativeStatus
            | Self::Throw
            | Self::Fatal
            | Self::Return => MachineEffects::pure(Never),
        }
    }
}

pub(super) fn effects_for_instruction(
    opcode: &MachineOpcode,
    descriptors: &[CallDescriptor],
) -> MachineEffects {
    let MachineOpcode::Call(index) = opcode else {
        return opcode.effects();
    };
    let Some(descriptor) = descriptors.get(*index as usize) else {
        return MachineEffects {
            reads: MachineAliasSet::ALL,
            writes: MachineAliasSet::ALL,
            allocates: true,
            throws: true,
            safepoint: true,
            reentrant: true,
            commoning: MachineCommoning::Never,
        };
    };

    let reads = if descriptor.effects.contains(CallEffects::READS_HEAP) {
        MachineAliasSet::HEAP
    } else {
        MachineAliasSet::NONE
    };
    let mut writes = if descriptor.effects.contains(CallEffects::WRITES_HEAP) {
        MachineAliasSet::HEAP
    } else {
        MachineAliasSet::NONE
    };
    if descriptor.effects.contains(CallEffects::INVALIDATES_SHAPES) {
        writes = writes
            .union(SHAPE)
            .union(PROPERTY_METADATA)
            .union(PROTOTYPE);
    }
    let reentrant = descriptor.effects.contains(CallEffects::REENTRANT);
    let safepoint = descriptor.safepoint == SafepointKind::Gc;
    MachineEffects {
        reads,
        writes,
        allocates: safepoint,
        throws: descriptor.exceptional != super::ExceptionalEdge::None,
        safepoint,
        reentrant,
        commoning: MachineCommoning::Never,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alias_sets_preserve_independent_memory_versions() {
        assert!(!SHAPE.intersects(PROPERTY_FIELD));
        assert!(!PROPERTY_FIELD.intersects(ELEMENT_FIELD));
        assert!(MachineAliasSet::HEAP.intersects(CONSTANT_CELL));
    }

    #[test]
    fn cache_ir_rows_are_explicit_and_effectful_stores_are_not_commonable() {
        // Both emitters read fast-shape/opaque state; ordinary slot and
        // extensibility guards also read descriptor/storage metadata.
        // Prototype changes can change opacity without changing the shape.
        let ordinary_state_reads = SHAPE.union(PROPERTY_METADATA).union(PROTOTYPE);
        for opcode in [
            MachineOpcode::CacheIrGuardShape {
                byte_pc: 0,
                shape: 1,
            },
            MachineOpcode::CacheIrGuardOrdinaryState { byte_pc: 0 },
            MachineOpcode::CacheIrGuardAtomSlot {
                byte_pc: 0,
                atom: 1,
                value_byte: 8,
                writable: false,
            },
            MachineOpcode::CacheIrGuardAtomSlot {
                byte_pc: 0,
                atom: 1,
                value_byte: 8,
                writable: true,
            },
            MachineOpcode::CacheIrGuardExtensible {
                byte_pc: 0,
                value_byte: 8,
            },
        ] {
            let guard = opcode.effects();
            assert_eq!(guard.reads, ordinary_state_reads, "{opcode:?}");
            assert_eq!(guard.commoning, MachineCommoning::Guard, "{opcode:?}");
            assert!(guard.writes.is_empty(), "{opcode:?}");
            assert!(!guard.allocates, "{opcode:?}");
            assert!(!guard.throws, "{opcode:?}");
            assert!(!guard.safepoint, "{opcode:?}");
            assert!(!guard.reentrant, "{opcode:?}");
        }

        let load = MachineOpcode::CacheIrLoadField {
            byte_pc: 0,
            value_byte: 8,
        }
        .effects();
        assert_eq!(load.reads, PROPERTY_FIELD);
        assert_eq!(load.commoning, MachineCommoning::Value);

        for opcode in [
            MachineOpcode::CacheIrStoreField {
                byte_pc: 0,
                value_byte: 8,
            },
            MachineOpcode::CacheIrWriteBarrier {
                byte_pc: 0,
                value_is_non_cell: false,
            },
            MachineOpcode::ElementValueStore {
                byte_pc: 0,
                access: otter_vm::JitElementAccess::default(),
            },
        ] {
            let effects = opcode.effects();
            assert!(!effects.writes.is_empty(), "{opcode:?}");
            assert_eq!(effects.commoning, MachineCommoning::Never, "{opcode:?}");
        }
    }

    #[test]
    fn context_words_split_immutable_chain_reads_from_mutable_slots() {
        for field in [ContextField::ClosureContext, ContextField::Parent] {
            assert_eq!(
                MachineOpcode::ContextLoad { field }.effects(),
                MachineEffects::PURE_VALUE,
                "{field:?}"
            );
        }
        assert_eq!(
            MachineOpcode::EntryCallee.effects(),
            MachineEffects::PURE_VALUE
        );
        assert_eq!(
            MachineOpcode::TaggedIsNotHole.effects(),
            MachineEffects::PURE_VALUE
        );
        let slot = MachineOpcode::ContextLoad {
            field: ContextField::Slot(2),
        }
        .effects();
        assert_eq!(slot.reads, CONTEXT_SLOT);
        assert_eq!(slot.commoning, MachineCommoning::Value);
        let store = MachineOpcode::ContextStore { slot: 2 }.effects();
        assert!(store.writes.intersects(slot.reads));
        assert_eq!(store.commoning, MachineCommoning::Never);
        let barrier = MachineOpcode::ContextWriteBarrier.effects();
        assert_eq!(barrier.writes, GC_BARRIER);
        assert!(!barrier.writes.intersects(slot.reads));
        // Unrelated object and element stores keep slot loads available.
        for unrelated in [
            MachineOpcode::CacheIrStoreField {
                byte_pc: 0,
                value_byte: 8,
            },
            MachineOpcode::ElementValueStore {
                byte_pc: 0,
                access: otter_vm::JitElementAccess::default(),
            },
            MachineOpcode::CacheIrPublishShape {
                byte_pc: 0,
                shape: 1,
                new_len: 1,
            },
        ] {
            let effects = unrelated.effects();
            assert!(!effects.writes.intersects(slot.reads), "{unrelated:?}");
            assert!(!effects.invalidates_dependency_epoch(), "{unrelated:?}");
        }
        // A generated binding write can target a context slot.
        let binding_write = MachineOpcode::BindingHit {
            byte_pc: 0,
            semantics: BindingSemantics::Write(
                otter_bytecode::opcode_schema::BindingWrite::ContextSlot {
                    value: 0,
                    context: 1,
                    coord: 2,
                },
            ),
            target: super::super::MachineBindingTarget::ContextSlot { depth: 0, slot: 2 },
        }
        .effects();
        assert!(binding_write.writes.intersects(slot.reads));
        // Every call that may run JavaScript or collect invalidates slots.
        let reentrant = effects_for_instruction(&MachineOpcode::Call(7), &[]);
        assert!(reentrant.writes.intersects(slot.reads));
        assert!(reentrant.invalidates_dependency_epoch());
    }

    #[test]
    fn backedge_poll_is_a_throwing_leaf_without_heap_invalidation() {
        let poll = MachineOpcode::BackedgePoll.effects();
        assert!(poll.throws);
        assert!(!poll.allocates);
        assert!(!poll.safepoint);
        assert!(!poll.reentrant);
        assert!(poll.reads.is_empty());
        assert!(poll.writes.is_empty());
        assert_eq!(poll.commoning, MachineCommoning::Never);
    }
}
