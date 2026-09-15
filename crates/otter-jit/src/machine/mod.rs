//! Target-selected Machine IR and exact post-allocation metadata.
//!
//! This module is the final boundary between JavaScript-semantic lowering and
//! target code emission. Instructions carry only machine operations, virtual
//! registers, physical constraints, clobbers, and metadata operands. They do
//! not contain bytecode opcodes or emitter-local access plans; typed call
//! descriptors own any VM-baked semantic guards they require. The explicit
//! OSR entry marker is the sole operation allowed to map live interpreter-frame
//! registers into allocator operands.
//!
//! # Contents
//! - [`InstructionSequence`] — verified block, value, and instruction storage.
//! - [`MachineInstruction`] — one selected operation and its allocator inputs.
//! - [`MachineCacheIrSite`] — source-owned immutable CacheIR programs.
//! - [`MachineOpcode`] — scalar operations, guarded element accesses, control
//!   flow, and descriptor-backed calls.
//! - [`CallDescriptor`], [`CallTarget`], [`DirectCallCandidate`], and
//!   [`DirectCallKind`] — complete semantic targets, guards, ABI, effects, and
//!   normal/exceptional exits.
//! - [`TargetSpec`] — complete target ABI, frame, legalization, and register
//!   contract.
//! - [`AllocatedSequence`] — allocator edits, per-operand locations, and exact
//!   safepoint/deoptimization locations.
//! - [`MachineFrameLayout`] — aligned post-allocation spill-frame contract.
//! - [`MachineSafepointTable`] — allocator roots and VM spill-slot records.
//! - [`MachineEffects`] — the exhaustive alias, reentry, allocation, throw,
//!   safepoint, and commoning contract consumed by graph optimization.
//!
//! # Invariants
//! - Virtual values are dense and have one machine representation.
//! - Every block owns a non-empty contiguous instruction range ending in one
//!   branch or return instruction.
//! - Metadata values are ordinary late uses. Calls therefore cannot leave a
//!   live GC/deopt value in a clobbered register.
//! - Every call, allocation, and side exit names one logical frame state.
//!   Compiler completion derives the exact `TaggedRoot` set from completed CFG
//!   liveness plus that state's tagged reconstruction recipes. Verification
//!   rejects missing and surplus roots before allocation.
//! - Named loads expose a no-call probe and a committed cold/status CFG.
//!   Only the cold call roots tagged state; its raw IC pointer must come from
//!   a property probe. Local catches consume the pure exception payload.
//!   Inlined property/global-read cold sites publish descendants through the
//!   same state-owned recipes without copying the caller.
//! - Inline method guards own their source's receiver/prototype/slot program,
//!   produce the current callable, and exact-deopt before any lookup effects.
//! - Direct methods own one complete dense one-to-four-candidate chain; plain
//!   and constructor targets remain monomorphic; an explicit-receiver call
//!   and a base construct own at most one candidate and otherwise the generic
//!   value call. A cold call exit owns no
//!   inputs, effects, clobbers, roots, or safepoint and must carry an exact
//!   pre-call deoptimization state.
//! - Committed runtime calls own zero to two explicit inputs and one GC
//!   safepoint. Ordinary committed descriptors expose the tagged completion and
//!   one exceptional edge; binding and named-load descriptors expose the physical
//!   tagged payload plus descriptor-domain status to explicit Machine control.
//!   Neither form can report a guard miss or request deoptimization/replay.
//!   Trailing `TaggedRoot` metadata is derived from the state and completed CFG;
//!   selection does not maintain an independent ordinary-root list.
//! - The schema-owned binding family expands before allocation into an explicit
//!   guard, generated hit, committed `NativeResultPair` cold call, three-way
//!   Success/Throw/Fatal branch, and join. Guard-produced owner/storage
//!   addresses are untraced and verifier-confined to the generated hit block;
//!   no safepoint, block argument, or backedge may retain them.
//! - Root and deopt maps are built from the same per-operand allocation table
//!   consumed by the emitter; there is no pre-allocation location fallback.
//! - OSR sources are immutable entry metadata aligned with ordinary late-use
//!   operands; their target locations come from that same allocation table.
//! - Guarded element operands are late location uses, so target emission may
//!   materialize stack or register homes without overwriting a live input.
//! - Tagged nullish loose equality deoptimizes before its Boolean definition
//!   only for a native-function cell, the sole HTMLDDA carrier; every other
//!   cell completes as not nullish in generated code.
//! - Target register files enumerate physical registers explicitly. There is
//!   no synthetic constant register budget.
//! - GVN equivalence includes representation, canonical inputs, dependency
//!   epoch, alias class, and the dominating memory version. No committed call,
//!   allocation, barrier, throwing operation, or control node is commoned.
//!
//! # See also
//! - [`crate::optimizing`] — the production Machine compilation entry.

mod committed_probe;
mod deopt;
mod derived_this;
mod effects;
mod frame;
mod gvn;
mod inline_frames;
mod licm;
mod native_leaf;
#[cfg(target_arch = "aarch64")]
pub(crate) mod numeric;
mod property;
mod regalloc;
mod safepoint;
mod target;
mod truthiness;

pub use deopt::{
    MachineDeoptError, MachineFrameSlot, MachineFrameState, lower_deopt_table, undefined_slot,
};
pub use effects::{MachineAliasClass, MachineAliasSet, MachineCommoning, MachineEffects};
pub use frame::{FrameLayoutError, MachineFrameLayout};
pub use property::MachineCacheIrSite;
pub use regalloc::{
    AllocatedLocation, AllocatedMetadata, AllocatedSequence, AllocationEdit, AllocationError,
    AllocationPoint,
};
pub use safepoint::{
    MachineSafepointError, MachineSafepointRoot, MachineSafepointSite, MachineSafepointTable,
    lower_safepoints,
};
pub use target::{
    PhysicalRegister, TargetArchitecture, TargetCapability, TargetClobberSet, TargetRegisterFile,
    TargetSpec,
};

use otter_vm::native_abi::{ExitAction, ExitReason, FrameStateId};
use std::fmt::Write as _;

/// Dense identity of a target-selected virtual value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MachineValue(pub u32);

/// Dense identity of a machine basic block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MachineBlock(pub u32);

/// Dense identity of a target-selected instruction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MachineInstructionId(pub u32);

/// Dense identity of a safepoint metadata record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SafepointId(pub u32);

/// Dense identity of a deoptimization metadata record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DeoptId(pub u32);

/// Machine representation assigned before instruction selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MachineRepresentation {
    /// Ordinary 64-bit JavaScript value.
    Tagged,
    /// Precisely traced 64-bit heap-cell pointer.
    Cell,
    /// Unboxed signed 32-bit integer.
    Int32,
    /// Unboxed canonical Boolean stored as integer zero or one.
    Boolean,
    /// Unboxed unsigned 32-bit integer.
    Uint32,
    /// Unboxed 64-bit integer or address-sized scalar.
    Int64,
    /// Whole-word status produced by the sole native result-pair ABI.
    ///
    /// This is deliberately not an integer value: only the descriptor-owned
    /// native-status terminator may consume it, so ordinary arithmetic cannot
    /// accidentally reinterpret an ABI state word.
    NativeStatus,
    /// Unboxed IEEE-754 binary64 value.
    Float64,
}

impl MachineRepresentation {
    fn register_class(self) -> regalloc2::RegClass {
        match self {
            Self::Tagged
            | Self::Cell
            | Self::Int32
            | Self::Boolean
            | Self::Uint32
            | Self::Int64
            | Self::NativeStatus => regalloc2::RegClass::Int,
            Self::Float64 => regalloc2::RegClass::Float,
        }
    }
}

/// Constraint on one selected instruction operand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperandConstraint {
    /// Register or stack home is accepted.
    Any,
    /// Operand must be in a register.
    Register,
    /// Operand must be in a stack spill slot.
    Stack,
    /// Operand must occupy one exact physical register.
    Fixed(PhysicalRegister),
    /// A definition reuses the register assigned to an earlier operand.
    Reuse(u8),
}

/// Whether an operand reads or defines its virtual value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperandRole {
    /// Read an existing value.
    Use,
    /// Define a new value.
    Definition,
}

/// Lifetime position of an operand within its instruction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperandTiming {
    /// Read or write occurs before the instruction's main effect.
    Early,
    /// Read or write remains live through the instruction's main effect.
    Late,
}

/// Why an instruction names an operand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperandPurpose {
    /// Ordinary machine-code input.
    Input,
    /// Ordinary machine-code result.
    Output,
    /// Tagged GC root at the instruction's safepoint.
    TaggedRoot,
    /// Runtime-only tagged root absent from interpreter frame state.
    RuntimeRoot,
    /// Cell GC root at the instruction's safepoint.
    CellRoot,
    /// Value required by the instruction's logical frame state.
    FrameState,
}

impl OperandPurpose {
    const fn is_metadata(self) -> bool {
        matches!(
            self,
            Self::TaggedRoot | Self::RuntimeRoot | Self::CellRoot | Self::FrameState
        )
    }
}

/// One virtual-register occurrence on a selected instruction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MachineOperand {
    /// Virtual value read or defined.
    pub value: MachineValue,
    /// Location constraint imposed by target selection.
    pub constraint: OperandConstraint,
    /// Read or definition role.
    pub role: OperandRole,
    /// Early or late position within the instruction.
    pub timing: OperandTiming,
    /// Machine-code or metadata purpose.
    pub purpose: OperandPurpose,
}

impl MachineOperand {
    /// Keep an ordinary input in its allocator-selected register or spill home
    /// through the instruction's main effect.
    #[must_use]
    pub const fn location_input(value: MachineValue) -> Self {
        Self {
            value,
            constraint: OperandConstraint::Any,
            role: OperandRole::Use,
            timing: OperandTiming::Late,
            purpose: OperandPurpose::Input,
        }
    }

    /// Construct an ordinary early register input.
    #[must_use]
    pub const fn register_input(value: MachineValue) -> Self {
        Self {
            value,
            constraint: OperandConstraint::Register,
            role: OperandRole::Use,
            timing: OperandTiming::Early,
            purpose: OperandPurpose::Input,
        }
    }

    /// Construct an ordinary early input in one ABI-mandated register.
    #[must_use]
    pub const fn fixed_register_input(value: MachineValue, register: PhysicalRegister) -> Self {
        Self {
            value,
            constraint: OperandConstraint::Fixed(register),
            role: OperandRole::Use,
            timing: OperandTiming::Early,
            purpose: OperandPurpose::Input,
        }
    }

    /// Construct an ordinary late register definition.
    #[must_use]
    pub const fn register_output(value: MachineValue) -> Self {
        Self {
            value,
            constraint: OperandConstraint::Register,
            role: OperandRole::Definition,
            timing: OperandTiming::Late,
            purpose: OperandPurpose::Output,
        }
    }

    /// Construct an ordinary late definition in one ABI-mandated register.
    #[must_use]
    pub const fn fixed_register_output(value: MachineValue, register: PhysicalRegister) -> Self {
        Self {
            value,
            constraint: OperandConstraint::Fixed(register),
            role: OperandRole::Definition,
            timing: OperandTiming::Late,
            purpose: OperandPurpose::Output,
        }
    }

    /// Construct a late definition tied to an earlier register input.
    #[must_use]
    pub const fn register_reuse_output(value: MachineValue, input: u8) -> Self {
        Self {
            value,
            constraint: OperandConstraint::Reuse(input),
            role: OperandRole::Definition,
            timing: OperandTiming::Late,
            purpose: OperandPurpose::Output,
        }
    }

    /// Keep a tagged GC root live through a safepoint instruction.
    #[must_use]
    pub const fn tagged_root(value: MachineValue) -> Self {
        Self {
            value,
            constraint: OperandConstraint::Any,
            role: OperandRole::Use,
            timing: OperandTiming::Late,
            purpose: OperandPurpose::TaggedRoot,
        }
    }

    /// Keep a runtime-only tagged temporary live through a safepoint.
    #[must_use]
    pub const fn runtime_root(value: MachineValue) -> Self {
        Self {
            value,
            constraint: OperandConstraint::Any,
            role: OperandRole::Use,
            timing: OperandTiming::Late,
            purpose: OperandPurpose::RuntimeRoot,
        }
    }

    /// Keep a cell GC root live through a safepoint instruction.
    #[must_use]
    pub const fn cell_root(value: MachineValue) -> Self {
        Self {
            value,
            constraint: OperandConstraint::Any,
            role: OperandRole::Use,
            timing: OperandTiming::Late,
            purpose: OperandPurpose::CellRoot,
        }
    }

    /// Keep a value available for the instruction's logical frame state.
    #[must_use]
    pub const fn frame_value(value: MachineValue) -> Self {
        Self {
            value,
            constraint: OperandConstraint::Any,
            role: OperandRole::Use,
            timing: OperandTiming::Late,
            purpose: OperandPurpose::FrameState,
        }
    }
}

/// Scalar contract for one interpreter-frame value entering Machine IR via OSR.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MachineOsrType {
    /// Any ordinary tagged JavaScript value copied without interpretation.
    Tagged,
    /// Tagged Number proven to fit a signed 32-bit integer.
    Int32,
    /// Tagged Number proven to fit an unsigned 32-bit integer.
    Uint32,
    /// Any tagged JavaScript Number decoded to binary64.
    Float64,
    /// Canonical tagged Boolean decoded to integer zero or one.
    Boolean,
}

/// One live loop-header value materialized by an OSR entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MachineOsrInput {
    /// Interpreter frame register containing the tagged source value.
    pub frame_register: u16,
    /// Unboxed scalar contract expected by the loop header.
    pub value_type: MachineOsrType,
}

/// Memory and dependency effects declared by a machine call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CallEffects(u8);

impl CallEffects {
    /// Pure call with no observable memory or shape effect.
    pub const PURE: Self = Self(0);
    /// Call may read heap memory.
    pub const READS_HEAP: Self = Self(1 << 0);
    /// Call may write heap memory.
    pub const WRITES_HEAP: Self = Self(1 << 1);
    /// Call may invalidate shape-dependent facts.
    pub const INVALIDATES_SHAPES: Self = Self(1 << 2);
    /// Call may invoke arbitrary JavaScript.
    pub const REENTRANT: Self = Self(1 << 3);

    /// Combine two effect sets.
    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// Whether every bit in `other` is present.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

/// Exceptional control edge of a machine call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExceptionalEdge {
    /// Call cannot throw.
    None,
    /// A throw leaves the compiled function through its shared abrupt exit.
    Propagate,
    /// A thrown exception transfers to this landing-pad block.
    LandingPad(MachineBlock),
}

/// Collector interaction declared by a machine call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SafepointKind {
    /// Leaf call cannot allocate or stop for GC.
    None,
    /// Call requires a return-PC stack map.
    Gc,
}

/// JavaScript entry semantics for one compiler-generated call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirectCallKind {
    /// Ordinary call whose dynamic callable is an allocator-owned operand.
    Plain,
    /// Call whose receiver is argument word zero: at most one identity-guarded
    /// candidate, and every guard miss or absent candidate completes through
    /// the generic explicit-receiver value call.
    CallWithThis,
    /// Intrinsic apply forwarding with explicit method/callee/receiver and live
    /// register bindings; runtime selection follows the permanent entry cell.
    Forward,
    /// Method call whose receiver/prototype chain and current callable must be
    /// revalidated immediately before native entry.
    Method,
    /// Base construction with receiver creation and `new.target` publication
    /// owned by the generated call boundary.
    Construct,
    /// Derived construction with `this = hole` and `new.target = callee`.
    DerivedConstruct,
    /// Base superclass construction with the caller's `new.target`.
    SuperConstruct,
    /// Derived superclass construction with the caller's `new.target`.
    DerivedSuperConstruct,
}

/// One reusable generated callable-identity proof.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MachineCallGuard {
    /// Exact ordinary closure identity.
    Plain {
        /// Canonical bytecode function identity.
        function_id: u32,
        /// Already-profiled ordinary `this` policy to prove before entry.
        this_mode: otter_vm::JitDirectCallThisMode,
    },
    /// Class-wrapper unwrapping followed by exact constructor identity.
    Construct {
        /// Canonical bytecode constructor identity.
        function_id: u32,
    },
    /// Exact receiver/prototype/slot method program.
    Method(Box<otter_vm::jit::JitMethodGuard>),
}

/// Maximum complete guarded method chain accepted by the Machine backend.
///
/// The VM may retain a larger bounded feedback chain for other tiers. Machine
/// lowering never truncates one: a site is selected only when its entire dense
/// chain fits this limit.
pub const MAX_MACHINE_DIRECT_METHOD_TARGETS: usize = 4;

/// One member of a complete compiler-generated call target chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectCallCandidate {
    /// Zero-based dense position in the captured chain.
    pub target_index: u32,
    /// Total captured target count, repeated on every candidate.
    pub target_count: u32,
    /// Exact receiver/prototype/method-slot guard for method candidates.
    pub guard: Option<otter_vm::jit::JitMethodGuard>,
    /// Exact callee generation plan and stable function entry cell.
    pub callee: otter_vm::JitDirectCallee,
}

/// Source call family represented by an always-deoptimizing cold exit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColdCallKind {
    /// Ordinary `Call` whose site had never executed at snapshot time.
    Plain,
    /// Receiver-bound `CallMethodValue` whose site had never executed.
    Method,
}

/// How a call descriptor represents its explicit input operands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirectCallArgumentMode {
    /// Each input is an explicit machine operand. Forward calls carry method,
    /// callee, receiver and register bindings here; the forwarded actual count
    /// is dynamic and belongs to the source activation.
    Fixed,
    /// One machine operand names the compiler-created dense argument array.
    Spread,
}

/// Semantic destination selected before target emission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallTarget {
    /// VM-owned runtime entry with one statically declared ABI.
    RuntimeStub(otter_vm::native_abi::RuntimeStubDescriptor),
    /// Canonical non-reentrant literal allocation from a boxed-value span.
    /// Arguments and unrelated live roots remain allocator-visible; the result
    /// commits once without an interpreter destination or pre-effect replay.
    LiteralAllocation {
        /// Shared VM-owned allocation descriptor.
        target: otter_vm::native_abi::RuntimeStubDescriptor,
        /// Source instruction index published before collection.
        logical_pc: u32,
        /// Source byte offset for allocation attribution.
        byte_pc: u32,
    },
    /// Exact bootstrap-identity guarded leaf with allocator-visible ABI operands.
    NativeLeaf {
        /// VM-owned declaration and isolate-local bootstrap identity.
        target: otter_vm::JitStaticNativeCall,
        /// Source byte offset for artifacts and exact pre-call deoptimization.
        byte_pc: u32,
    },
    /// Effect-once completion through a VM-declared fixed ABI. Boxed-value
    /// entries pad omitted inputs with `undefined`; named loads instead take
    /// one boxed receiver and a stable, untraced code-owned IC-cell address.
    /// Trailing `TaggedRoot` operands independently publish the complete live
    /// moving state, including unrelated values at a zero-arity operation. A
    /// descriptor may either collapse the committed status into an exceptional
    /// edge or expose the physical payload/status pair to an explicit
    /// [`MachineOpcode::BranchNativeStatus`]; both use this one target shape.
    CommittedRuntime {
        /// Typed VM-owned completion entry.
        target: otter_vm::native_abi::RuntimeStubDescriptor,
        /// Canonical source instruction index published before reentry.
        logical_pc: u32,
        /// Source byte offset used by artifacts.
        byte_pc: u32,
        /// Number of explicit target inputs, in the range zero through two.
        /// Named loads count the receiver and IC-cell address.
        semantic_arity: u8,
    },
    /// VM-planned JavaScript callee chain entered through generated stack-owned
    /// linkage. Plain and constructor calls remain monomorphic; guarded methods
    /// may carry one complete dense bounded chain. A zero-candidate method is
    /// an attempted site whose canonical runtime miss owns lookup and call.
    Direct {
        /// Plain or receiver-bound method entry through the shared linkage.
        kind: DirectCallKind,
        /// Fixed operands or one compiler-created dense spread array.
        argument_mode: DirectCallArgumentMode,
        /// Complete target chain in guard order.
        candidates: Vec<DirectCallCandidate>,
        /// Calling function identity used by started-call deoptimization.
        caller_function_id: u32,
        /// Canonical caller instruction index.
        logical_pc: u32,
        /// Caller byte offset used by diagnostics and relocations.
        byte_pc: u32,
    },
    /// A proven-never-attempted ordinary call. Generated code exits at the
    /// exact pre-call state before boxing, rooting, or any observable effect.
    ColdCallExit {
        /// Plain or receiver-bound source call family.
        kind: ColdCallKind,
        /// Calling function identity used by exact deoptimization.
        caller_function_id: u32,
        /// Canonical caller instruction index.
        logical_pc: u32,
        /// Caller byte offset used by diagnostics.
        byte_pc: u32,
    },
}

/// Structurally safe generated target for one typed binding operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MachineBindingTarget {
    /// No generated target is proven; guard transfers directly to cold.
    Cold,
    /// Realm-global object reached through the snapshot's global-this cell.
    GlobalThis,
    /// One stable captured cell in the current native-frame spine.
    Upvalue {
        /// Zero-based stable cell index.
        index: u32,
    },
    /// VM-baked global lexical/object proof.
    Global(otter_vm::jit::BindingHitProof),
}

fn binding_target_matches_semantics(
    semantics: otter_bytecode::opcode_schema::BindingSemantics,
    target: MachineBindingTarget,
) -> bool {
    use otter_bytecode::opcode_schema::{BindingRead, BindingSemantics, BindingWrite};

    matches!(
        (semantics, target),
        (
            BindingSemantics::Read(BindingRead::GlobalThis { .. }),
            MachineBindingTarget::GlobalThis,
        ) | (
            BindingSemantics::Read(BindingRead::Upvalue { .. })
                | BindingSemantics::Write(BindingWrite::Upvalue { .. }),
            MachineBindingTarget::Upvalue { .. },
        ) | (
            BindingSemantics::Read(BindingRead::Global { .. } | BindingRead::Exists { .. })
                | BindingSemantics::Write(
                    BindingWrite::Global { .. } | BindingWrite::GlobalChecked { .. },
                ),
            MachineBindingTarget::Global(_),
        ) | (_, MachineBindingTarget::Cold)
    )
}

fn binding_guard_clobbers(target: &TargetSpec) -> Vec<PhysicalRegister> {
    target.clobbers(TargetClobberSet::BindingGuard).to_vec()
}

fn binding_hit_clobbers(target: &TargetSpec) -> Vec<PhysicalRegister> {
    target.clobbers(TargetClobberSet::BindingHit).to_vec()
}

fn binding_write_barrier_clobbers(target: &TargetSpec) -> Vec<PhysicalRegister> {
    target
        .clobbers(TargetClobberSet::BindingWriteBarrier)
        .to_vec()
}

/// Complete target-neutral call contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallDescriptor {
    /// Semantic entry selected above target emission.
    pub target: CallTarget,
    /// Ordered argument representations.
    pub arguments: Vec<MachineRepresentation>,
    /// Ordered result representations.
    ///
    /// Ordinary calls expose zero or one result. A committed descriptor that
    /// keeps its physical `NativeResultPair` explicit has two: tagged payload
    /// followed by [`MachineRepresentation::NativeStatus`]. The result domain
    /// remains owned exclusively by the runtime-stub descriptor.
    pub results: Vec<MachineRepresentation>,
    /// Memory, shape, and reentrancy effects.
    pub effects: CallEffects,
    /// Physical registers destroyed by the selected target ABI.
    pub clobbers: Vec<PhysicalRegister>,
    /// Exceptional control transfer.
    pub exceptional: ExceptionalEdge,
    /// GC interaction.
    pub safepoint: SafepointKind,
}

fn is_explicit_committed_runtime_call(descriptor: &CallDescriptor) -> bool {
    matches!(
        &descriptor.target,
        CallTarget::CommittedRuntime { target, .. }
            if (matches!(
                target.signature,
                otter_vm::native_abi::RuntimeStubSignature::CommittedValue2
                    | otter_vm::native_abi::RuntimeStubSignature::ReentrantValue2
                    | otter_vm::native_abi::RuntimeStubSignature::ReentrantValue3
            ) || *target == otter_vm::native_abi::STUB_JIT_LOAD_PROPERTY || *target == otter_vm::native_abi::STUB_JIT_STORE_PROPERTY)
                && target.result_abi
                    == otter_vm::native_abi::RuntimeStubResultAbi::NativePair
                && target.result_domain
                    == otter_vm::native_abi::NativeResultDomain::Committed
                && descriptor.results
                    == [
                        MachineRepresentation::Tagged,
                        MachineRepresentation::NativeStatus,
                    ]
                && descriptor.exceptional == ExceptionalEdge::None
    )
}

fn is_caught_throw_acknowledgement_target(descriptor: &CallDescriptor) -> bool {
    matches!(
        descriptor.target,
        CallTarget::RuntimeStub(target)
            if target.id == otter_vm::native_abi::STUB_JIT_ACKNOWLEDGE_CAUGHT_THROW.id
    )
}

/// Target-neutral name of a selected machine operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MachineOpcode {
    /// Materialize an incoming ABI value.
    EntryValue(u16),
    /// Materialize the current frame's tagged `this` binding.
    EntryThis,
    /// Prove one callable identity without resolving its `this` binding.
    GuardCallTarget {
        /// Immutable identity proof selected from source-owned feedback.
        guard: MachineCallGuard,
    },
    /// Resolve the `this` value from an already-proven ordinary callable.
    ResolveCallThis {
        /// Ordinary call binding policy already accepted by its guard.
        this_mode: otter_vm::JitDirectCallThisMode,
    },
    /// Reserve and fully initialize one unpublished receiver candidate from
    /// the shared nursery; undefined means no candidate was available.
    AllocateObject {
        /// Source byte PC for generated allocation attribution.
        byte_pc: u32,
        /// One immutable live-prototype/shape allocation program.
        plan: otter_vm::jit::JitReceiverAllocationPlan,
    },
    /// Test the successful receiver result of a nursery probe.
    AllocationHit,
    /// Publish an initialized allocation by advancing the nursery frontier and
    /// committing its accounting and weak observation. A false hit is a no-op.
    PublishObject {
        /// Source bytecode offset for effect attribution.
        byte_pc: u32,
    },
    /// Return an object result, or substitute the allocated base receiver.
    BaseConstructResult,
    /// Commit an unbound stack-owned derived this, returning whether it hit.
    TryBindDerivedThis {
        /// Exact source byte PC for generated/cold attribution.
        byte_pc: u32,
    },
    /// Materialize one exact immediate tagged JavaScript value.
    TaggedConstant(u64),
    /// Guard and decode one tagged JavaScript Number.
    DecodeNumber,
    /// Guard and decode one tagged JavaScript Int32.
    DecodeInt32,
    /// Materialize an integer constant.
    IntegerConstant(i64),
    /// Materialize a floating-point constant by exact bit pattern.
    FloatConstant(u64),
    /// Integer addition.
    IntegerAdd,
    /// Integer subtraction with an overflow exit.
    IntegerSub,
    /// Integer multiplication with overflow and negative-zero exits.
    IntegerMul,
    /// Integer negation with overflow and negative-zero exits.
    IntegerNeg,
    /// Integer addition with a baked right operand and overflow exit.
    IntegerAddImmediate(i32),
    /// Integer subtraction with a baked right operand and overflow exit.
    IntegerSubImmediate(i32),
    /// Integer bitwise AND.
    IntegerAnd,
    /// Integer bitwise OR.
    IntegerOr,
    /// Integer bitwise XOR.
    IntegerXor,
    /// Integer left shift with JavaScript's masked count.
    IntegerShiftLeft,
    /// Signed integer right shift with JavaScript's masked count.
    IntegerShiftRight,
    /// Unsigned integer right shift with JavaScript's masked count.
    IntegerShiftRightLogical,
    /// Integer bitwise complement.
    IntegerNot,
    /// Integer bitwise AND with a baked right operand.
    IntegerAndImmediate(i32),
    /// Signed integer less-than with a baked right operand.
    IntegerLessThanImmediate(i32),
    /// Integer equality with a baked right operand.
    IntegerEqualImmediate(i32),
    /// Integer inequality with a baked right operand.
    IntegerNotEqualImmediate(i32),
    /// Signed integer equality producing 0 or 1.
    IntegerEqual,
    /// Signed integer inequality producing 0 or 1.
    IntegerNotEqual,
    /// Signed integer less-than producing 0 or 1.
    IntegerLessThan,
    /// Signed integer less-than-or-equal producing 0 or 1.
    IntegerLessEqual,
    /// Signed integer greater-than producing 0 or 1.
    IntegerGreaterThan,
    /// Signed integer greater-than-or-equal producing 0 or 1.
    IntegerGreaterEqual,
    /// Losslessly widen an integer Number into floating-point representation.
    Int32ToFloat64,
    /// Losslessly widen an unsigned integer Number into floating-point representation.
    Uint32ToFloat64,
    /// Convert an unboxed Float64 through ECMAScript ToInt32.
    Float64ToInt32,
    /// Reinterpret canonical Boolean bits as Int32.
    BooleanToInt32,
    /// Floating-point addition.
    FloatAdd,
    /// Floating-point subtraction.
    FloatSub,
    /// Floating-point multiplication.
    FloatMul,
    /// Floating-point division.
    FloatDiv,
    /// Floating-point remainder through the canonical no-allocation leaf.
    FloatRem,
    /// Floating-point exponentiation through the canonical no-allocation leaf.
    FloatPow,
    /// Floating-point negation.
    FloatNeg,
    /// Convert an Int32 or Uint32 value to canonical Boolean bits.
    IntegerToBoolean,
    /// Convert a Float64 value to canonical Boolean bits.
    FloatToBoolean,
    /// No-call truthiness probe: tagged input, Boolean result, Boolean hit.
    /// Uncertain cells miss to an explicit canonical leaf sibling.
    TruthinessProbe,
    /// No-call loose equality proof: tagged pair, tagged Boolean, Boolean hit.
    /// Coercive operands use the explicit canonical committed cold sibling.
    LooseEqualityProbe {
        /// Source bytecode offset for structural attribution.
        byte_pc: u32,
        /// True for ==, false for !=.
        equal: bool,
    },
    /// Invert canonical Boolean bits.
    BooleanNot,
    /// Compare one tagged value with a statically known `null` or `undefined`.
    /// A native-function cell deopts before defining the result so the
    /// canonical equality path can observe HTMLDDA; every other cell completes
    /// as not nullish.
    TaggedNullishEqual {
        /// Source bytecode offset used by artifacts and exact deoptimization.
        byte_pc: u32,
        /// `true` for loose equality and `false` for loose inequality.
        equal: bool,
    },
    /// Ordered floating-point less-than comparison producing 0 or 1.
    FloatLessThan,
    /// Floating-point equality comparison producing 0 or 1.
    FloatEqual,
    /// Floating-point inequality comparison producing 0 or 1.
    FloatNotEqual,
    /// Ordered floating-point less-than-or-equal comparison producing 0 or 1.
    FloatLessEqual,
    /// Ordered floating-point greater-than comparison producing 0 or 1.
    FloatGreaterThan,
    /// Ordered floating-point greater-than-or-equal comparison producing 0 or 1.
    FloatGreaterEqual,
    /// Canonically box one floating-point JavaScript Number.
    BoxNumber,
    /// Canonically box one integer JavaScript Number.
    BoxInt32,
    /// Canonically box one unsigned integer JavaScript Number.
    BoxUint32,
    /// Canonically box one Boolean represented as integer 0 or 1.
    BoxBoolean,
    /// Prove one generated binding target without performing the semantic
    /// read or write. Outputs are hit Boolean, owner address, and storage
    /// address; every miss transfers to the committed cold sibling.
    BindingGuard {
        /// Source byte offset used by artifacts.
        byte_pc: u32,
        /// Schema-owned read/write/delete semantics.
        semantics: otter_bytecode::opcode_schema::BindingSemantics,
        /// Structurally safe target, or an explicit always-cold guard.
        target: MachineBindingTarget,
    },
    /// Perform the non-allocating read/write after its guard has succeeded.
    BindingHit {
        /// Source byte offset used by artifacts.
        byte_pc: u32,
        /// Schema-owned read/write/delete semantics.
        semantics: otter_bytecode::opcode_schema::BindingSemantics,
        /// Proven generated target.
        target: MachineBindingTarget,
    },
    /// Explicit post-store generational/incremental barrier. This is a
    /// separate Machine effect, never hidden in [`MachineOpcode::BindingHit`].
    BindingWriteBarrier,
    /// Structural join marker for a completed binding operation.
    BindingJoin {
        /// Source byte offset used by artifacts.
        byte_pc: u32,
        /// Schema-owned operation identity.
        semantics: otter_bytecode::opcode_schema::BindingSemantics,
    },
    /// Read one eagerly prepared primitive string from its address-stable,
    /// GC-traced constant cell. The relocation contains no moving handle.
    StringConstantCellLoad {
        /// Source bytecode offset used by artifacts.
        byte_pc: u32,
        /// Stable traced-cell target copied from the compile snapshot.
        target: otter_vm::jit::JitStringConstantCell,
    },
    /// Prove receiver identity and representation and materialize its live raw
    /// base/length pair. A miss only clears the Boolean result.
    ElementView {
        /// Source bytecode offset selecting immutable layout metadata.
        byte_pc: u32,
    },
    /// Prove an exact integer index is in bounds and derive one raw address
    /// from a prior view. This operation has no heap effect.
    ElementAddress {
        /// Source bytecode offset selecting element stride and length width.
        byte_pc: u32,
    },
    /// Read one addressed element and independently prove slot presence.
    ElementValueLoad {
        /// Source bytecode offset selecting the physical representation.
        byte_pc: u32,
    },
    /// Prove that a value can be written directly to one addressed element.
    /// No store is performed by this guard.
    ElementValueGuard {
        /// Source bytecode offset selecting the physical representation.
        byte_pc: u32,
    },
    /// Commit one already-proven element store. This no-fail effect has no
    /// guard, exit, call, or hidden control transfer.
    ElementValueStore {
        /// Source bytecode offset selecting the physical representation.
        byte_pc: u32,
    },
    /// Require a composed Boolean proof. Failure is an exact pre-effect exit.
    GuardCondition,
    /// Materialize a Boolean constant for explicit guard composition.
    BooleanConstant(bool),
    /// Combine two canonical Boolean guard results.
    BooleanOr,
    /// Select one tagged SSA value without reentry or memory effects.
    TaggedSelect,
    /// Materialize the stable source cell used only by the committed property
    /// boundary to recover function/logical-PC identity.
    PropertySource {
        /// Bytecode function owning the site.
        function_id: u32,
        /// Canonical instruction index in that function.
        logical_pc: u32,
        /// Source byte offset used by artifacts.
        byte_pc: u32,
        /// Whether this is a store rather than a load.
        store: bool,
    },
    /// Prove an ordinary fast object has one expected hidden class. A false
    /// incoming condition keeps the result false without touching the object.
    CacheIrGuardShape {
        /// Source byte offset used by artifacts.
        byte_pc: u32,
        /// Stable compressed hidden-class token.
        shape: u32,
    },
    /// Prove that an atom's immutable shape slot is not overridden by
    /// object-local descriptor or exotic state.
    CacheIrGuardAtomSlot {
        /// Source byte offset used by artifacts.
        byte_pc: u32,
        /// Isolate-global atom identity captured in the CacheIR program.
        atom: u32,
        /// Byte offset of the guarded value slot.
        value_byte: u32,
        /// Whether the terminal operation requires a writable data slot.
        writable: bool,
    },
    /// Read a direct prototype under a prior CacheIR condition.
    CacheIrLoadPrototype {
        /// Source byte offset used by artifacts.
        byte_pc: u32,
    },
    /// Prove that an object's direct prototype is null under a prior CacheIR
    /// condition.
    CacheIrGuardPrototypeNull {
        /// Source byte offset used by artifacts.
        byte_pc: u32,
    },
    /// Read one own data field under a complete CacheIR guard chain.
    CacheIrLoadField {
        /// Source byte offset used by artifacts.
        byte_pc: u32,
        /// Byte offset inside the object's value slab.
        value_byte: u32,
    },
    /// Commit one existing own-data field store under a complete CacheIR guard
    /// chain. The parent address output is valid only for the immediately
    /// following write barrier and never crosses a safepoint.
    CacheIrStoreField {
        /// Source byte offset used by artifacts.
        byte_pc: u32,
        /// Byte offset inside the receiver's value slab.
        value_byte: u32,
    },
    /// Prove that an append targets the exact next slot, the receiver remains
    /// extensible, and existing storage has capacity for the write.
    CacheIrGuardExtensible {
        /// Source byte offset used by artifacts.
        byte_pc: u32,
        /// Byte offset of the exact next slot.
        value_byte: u32,
    },
    /// Publish an add-transition's child shape and logical slot length. Every
    /// miss-capable guard and the value store precedes this no-fail effect.
    CacheIrPublishShape {
        /// Source byte offset used by artifacts.
        byte_pc: u32,
        /// Stable compressed child hidden-class token.
        shape: u32,
        /// Logical slot length after publication.
        new_len: u16,
        /// Whether to initialize the inline values pointer for slot zero.
        initialize_inline: bool,
    },
    /// Apply the post-store generational/incremental barrier. A false incoming
    /// condition is a no-op; no miss or exit is possible after the store.
    CacheIrWriteBarrier {
        /// Source byte offset used by artifacts.
        byte_pc: u32,
        /// Scalar typing proves the stored value cannot be a GC cell.
        value_is_non_cell: bool,
    },
    /// Try the non-CacheIR dense-array/primitive-string `.length` program.
    ExoticLength {
        /// Source byte offset used by artifacts.
        byte_pc: u32,
    },
    /// Copy the completed CacheIR payload/hit pair into the property CFG's
    /// canonical values before branching to hit or committed cold control.
    CacheIrJoin {
        /// Source byte offset used by artifacts.
        byte_pc: u32,
        /// Whether the source operation is a store.
        store: bool,
    },
    /// Target ABI call through a call descriptor.
    Call(u32),
    /// Interpreter-to-native loop-header entry marker. Its late-use operands
    /// name the exact allocator locations populated by the cold trampoline.
    OsrEntry {
        /// Canonical instruction index of the loop header.
        logical_pc: u32,
        /// Interpreter sources aligned one-for-one with instruction operands.
        inputs: Vec<MachineOsrInput>,
    },
    /// Loop backedge poll with an exact interpreter reconstruction state.
    BackedgePoll,
    /// Explicit external entry into a natural loop. Ordinary entry and OSR
    /// share this target before any loop-invariant proof is consumed.
    LoopPreheader,
    /// Unconditional control transfer.
    Jump,
    /// Conditional control transfer; branch when the integer condition equals
    /// the encoded polarity.
    BranchIf(bool),
    /// Three-way descriptor-domain branch. Success, Throw, and Fatal are
    /// successors zero, one, and two respectively; corrupt statuses take the
    /// Fatal edge.
    BranchNativeStatus,
    /// Propagate one pure exception SSA value through the compiled abrupt exit.
    Throw,
    /// Leave through the shared fatal runtime exit.
    Fatal,
    /// Function return.
    Return,
}

/// Control-flow role of a selected instruction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlFlow {
    /// Ordinary non-terminal instruction.
    None,
    /// Final branch instruction of a block.
    Branch,
    /// Final return instruction of a block.
    Return,
}

/// One target-selected instruction before register allocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineInstruction {
    /// Target-neutral operation identity.
    pub opcode: MachineOpcode,
    /// Ordered instruction and metadata operands.
    pub operands: Vec<MachineOperand>,
    /// Physical registers overwritten outside explicit definitions.
    pub clobbers: Vec<PhysicalRegister>,
    /// Safepoint described by metadata operands, when present.
    pub safepoint: Option<SafepointId>,
    /// Logical interpreter state shared by root and deopt lowering.
    pub frame_state: Option<FrameStateId>,
    /// Exact generated exits and their policies. Multiple entries distinguish
    /// different failed proofs that share one logical frame state.
    pub exits: Box<[MachineExit]>,
    /// Boxed inline activation recipes at committed cold reentry. The first
    /// frame names the suspended caller and has no copied register window.
    pub inline_frames: Box<[otter_vm::deopt::DeoptFrame<Option<MachineValue>>]>,
    /// Control-flow role.
    pub control: ControlFlow,
}

impl MachineInstruction {
    /// Construct a non-terminal instruction without metadata or clobbers.
    #[must_use]
    pub fn plain(opcode: MachineOpcode, operands: Vec<MachineOperand>) -> Self {
        Self {
            opcode,
            operands,
            clobbers: Vec::new(),
            safepoint: None,
            frame_state: None,
            exits: Box::default(),
            inline_frames: Box::default(),
            control: ControlFlow::None,
        }
    }

    #[cfg(test)]
    pub(super) fn set_test_exit(&mut self, exit: DeoptId, state: FrameStateId) {
        self.frame_state = Some(state);
        self.exits = Box::new([MachineExit {
            id: exit,
            reason: ExitReason::UnsupportedOperation,
            action: ExitAction::Recompile,
        }]);
    }

    /// The sole exit id for operations with one failed proof.
    #[must_use]
    pub fn deopt_id(&self) -> Option<DeoptId> {
        (self.exits.len() == 1).then(|| self.exits[0].id)
    }

    /// Exit id for one exact failed proof.
    #[must_use]
    pub fn exit_id(&self, reason: ExitReason) -> Option<DeoptId> {
        self.exits
            .iter()
            .find(|exit| exit.reason == reason)
            .map(|exit| exit.id)
    }
}

/// One typed pre-effect exit attached to a Machine instruction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MachineExit {
    /// Dense emitted exit-site identity.
    pub id: DeoptId,
    /// Stable reason used by exit profiling.
    pub reason: ExitReason,
    /// Cold policy requested by this site.
    pub action: ExitAction,
}

/// One contiguous target-selected basic block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineBlockData {
    /// First instruction in the block.
    pub first: MachineInstructionId,
    /// Instruction after the block's final instruction.
    pub end: MachineInstructionId,
    /// Predecessors in deterministic order.
    pub predecessors: Vec<MachineBlock>,
    /// Successors in deterministic order.
    pub successors: Vec<MachineBlock>,
    /// SSA block parameters defined at block entry.
    pub parameters: Vec<MachineValue>,
    /// One value list per successor, corresponding to its parameters.
    pub successor_arguments: Vec<Vec<MachineValue>>,
}

/// Verified target-selected instruction sequence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstructionSequence {
    pub(super) entry: MachineBlock,
    representations: Vec<MachineRepresentation>,
    call_descriptors: Vec<CallDescriptor>,
    frame_states: Vec<MachineFrameState>,
    blocks: Vec<MachineBlockData>,
    instructions: Vec<MachineInstruction>,
}

/// Structural failure in a target-selected instruction sequence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerificationError {
    /// Entry block does not exist.
    InvalidEntry,
    /// A block owns an empty or invalid instruction range.
    InvalidBlockRange(MachineBlock),
    /// Blocks do not partition instructions contiguously in dense order.
    NonContiguousBlocks(MachineBlock),
    /// A block does not end in branch or return.
    MissingTerminator(MachineBlock),
    /// A non-final instruction is marked as a terminator.
    EarlyTerminator(MachineInstructionId),
    /// A CFG block identity is outside dense block storage.
    InvalidBlock(MachineBlock),
    /// Successor arguments do not match successor count or parameters.
    InvalidSuccessorArguments(MachineBlock),
    /// Stored predecessors are not the exact reverse of successor edges.
    PredecessorMismatch(MachineBlock),
    /// An edge argument representation differs from its block parameter.
    BlockParameterRepresentation(MachineBlock, MachineValue),
    /// A selected branch/return has the wrong successor count.
    TerminatorSuccessors(MachineBlock),
    /// An operand references a missing value.
    InvalidValue(MachineValue),
    /// A fixed register has the wrong class for its virtual value.
    FixedRegisterClass(MachineInstructionId, MachineValue),
    /// A selected opcode's ordinary operands or effect metadata violate its
    /// target-neutral signature.
    OpcodeSignatureMismatch(MachineInstructionId),
    /// Metadata operands must be late uses.
    InvalidMetadataOperand(MachineInstructionId, MachineValue),
    /// Root metadata does not match the value representation.
    InvalidRootRepresentation(MachineInstructionId, MachineValue),
    /// A tagged SSA value live across a GC safepoint is not rooted there.
    MissingLiveTaggedRoot(MachineInstructionId, MachineValue),
    /// One tagged value appears more than once in a safepoint root set.
    DuplicateTaggedRoot(MachineInstructionId, MachineValue),
    /// A tagged root is neither live nor named by the logical frame state.
    UnexpectedTaggedRoot(MachineInstructionId, MachineValue),
    /// Logical frame-state storage is not dense in id order.
    NonDenseFrameStateId {
        /// Required id at this storage position.
        expected: FrameStateId,
        /// Stored id.
        actual: FrameStateId,
    },
    /// An instruction references no valid logical frame state.
    InvalidFrameState(MachineInstructionId, Option<FrameStateId>),
    /// One instruction repeats a typed exit reason.
    DuplicateExitReason(MachineInstructionId, ExitReason),
    /// Exit identities are not dense after whole-sequence selection.
    NonDenseExitId {
        /// Required id at this sorted exit position.
        expected: u32,
        /// Stored id.
        actual: u32,
    },
    /// Safepoint metadata appears without a safepoint identity.
    RootWithoutSafepoint(MachineInstructionId),
    /// Deopt metadata appears without a deopt identity.
    DeoptWithoutExit(MachineInstructionId),
    /// A safepoint identity is duplicated.
    DuplicateSafepoint(SafepointId),
    /// A deopt identity is duplicated.
    DuplicateDeopt(DeoptId),
    /// A call references a missing descriptor.
    InvalidCallDescriptor(MachineInstructionId, u32),
    /// A direct or cold call target violates its bounded semantic contract.
    InvalidCallTarget(MachineInstructionId),
    /// A call's ordinary inputs/results disagree with its descriptor.
    CallSignatureMismatch(MachineInstructionId),
    /// A call's safepoint marker disagrees with its descriptor.
    CallSafepointMismatch(MachineInstructionId),
    /// A call's exceptional target does not exist or is not a CFG successor.
    InvalidExceptionalEdge(MachineInstructionId, MachineBlock),
    /// A local exception edge does not enter a dedicated acknowledgement
    /// block exactly once before reaching its catch body.
    InvalidCaughtThrowLanding(MachineBlock),
    /// The caught-throw acknowledgement leaf appears outside its dedicated
    /// local-exception edge block or violates its no-result/no-throw contract.
    InvalidCaughtThrowAcknowledgement(MachineInstructionId),
    /// Physical clobber is repeated on one instruction.
    DuplicateClobber(MachineInstructionId, PhysicalRegister),
}

impl std::fmt::Display for VerificationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid Machine IR: {self:?}")
    }
}

impl std::error::Error for VerificationError {}

impl InstructionSequence {
    /// Construct and verify one target-selected instruction sequence.
    pub fn new(
        target: &TargetSpec,
        entry: MachineBlock,
        representations: Vec<MachineRepresentation>,
        call_descriptors: Vec<CallDescriptor>,
        blocks: Vec<MachineBlockData>,
        instructions: Vec<MachineInstruction>,
    ) -> Result<Self, VerificationError> {
        let sequence = Self {
            entry,
            representations,
            call_descriptors,
            frame_states: Vec::new(),
            blocks,
            instructions,
        };
        sequence.verify(target)?;
        Ok(sequence)
    }

    /// Construct and verify a sequence with explicit logical frame states.
    pub fn new_with_frame_states(
        target: &TargetSpec,
        entry: MachineBlock,
        representations: Vec<MachineRepresentation>,
        call_descriptors: Vec<CallDescriptor>,
        frame_states: Vec<MachineFrameState>,
        blocks: Vec<MachineBlockData>,
        instructions: Vec<MachineInstruction>,
    ) -> Result<Self, VerificationError> {
        let sequence = Self {
            entry,
            representations,
            call_descriptors,
            frame_states,
            blocks,
            instructions,
        };
        sequence.verify(target)?;
        Ok(sequence)
    }

    /// Finalize one compiler-selected sequence from its complete CFG.
    ///
    /// Individual instruction selection cannot know which tagged values remain
    /// live through a later call or around a loop backedge. The complete CFG is
    /// therefore the sole authority for adding missing late root uses. Public
    /// construction still requires callers to provide exact root metadata and
    /// is verified without repair.
    pub(super) fn new_selected(
        target: &TargetSpec,
        entry: MachineBlock,
        representations: Vec<MachineRepresentation>,
        call_descriptors: Vec<CallDescriptor>,
        frame_states: Vec<MachineFrameState>,
        blocks: Vec<MachineBlockData>,
        instructions: Vec<MachineInstruction>,
    ) -> Result<Self, VerificationError> {
        let mut sequence = Self {
            entry,
            representations,
            call_descriptors,
            frame_states,
            blocks,
            instructions,
        };
        sequence.complete_gc_root_liveness();
        sequence.verify_structure(target)?;
        sequence.verify_gc_root_liveness()?;
        Ok(sequence)
    }

    /// Allocate through regalloc2's Ion allocator and finalize exact metadata.
    pub fn allocate(&self, target: &TargetSpec) -> Result<AllocatedSequence, AllocationError> {
        regalloc::allocate(self, target)
    }

    /// Run effect-aware GVN and LICM, then reverify the rewritten graph before
    /// register allocation.
    pub(crate) fn optimize(
        self,
        target: &TargetSpec,
    ) -> Result<(Self, gvn::MachineOptimizationStats), VerificationError> {
        let (sequence, mut stats) = gvn::optimize(self, target)?;
        let (sequence, licm) = licm::optimize(sequence, target)?;
        let (sequence, exposed) = gvn::optimize(sequence, target)?;
        stats.eliminated_instructions = stats
            .eliminated_instructions
            .saturating_add(exposed.eliminated_instructions);
        stats.eliminated_guards = stats
            .eliminated_guards
            .saturating_add(exposed.eliminated_guards);
        stats.eliminated_loads = stats
            .eliminated_loads
            .saturating_add(exposed.eliminated_loads);
        stats.hoisted_instructions = licm.hoisted_instructions;
        stats.versioned_loops = licm.versioned_loops;
        Ok((sequence, stats))
    }

    /// Dense machine representations indexed by [`MachineValue`].
    #[must_use]
    pub fn representations(&self) -> &[MachineRepresentation] {
        &self.representations
    }

    /// Interned call descriptors referenced by [`MachineOpcode::Call`].
    #[must_use]
    pub fn call_descriptors(&self) -> &[CallDescriptor] {
        &self.call_descriptors
    }

    /// Logical frame states shared by root and deopt lowering.
    #[must_use]
    pub fn frame_states(&self) -> &[MachineFrameState] {
        &self.frame_states
    }

    /// Dense blocks indexed by [`MachineBlock`].
    #[must_use]
    pub fn blocks(&self) -> &[MachineBlockData] {
        &self.blocks
    }

    /// Dense instructions indexed by [`MachineInstructionId`].
    #[must_use]
    pub fn instructions(&self) -> &[MachineInstruction] {
        &self.instructions
    }

    /// Deterministic machine instruction and source-recipe dump.
    #[must_use]
    pub fn normalized(&self) -> String {
        let mut output = String::from("machine-ir explicit-loop-preheaders\n");
        for (index, representation) in self.representations.iter().enumerate() {
            writeln!(output, "v{index}:{representation:?}").expect("writing to String cannot fail");
        }
        for (index, descriptor) in self.call_descriptors.iter().enumerate() {
            writeln!(output, "call{index}:{descriptor:?}").expect("writing to String cannot fail");
        }
        for (index, block) in self.blocks.iter().enumerate() {
            writeln!(
                output,
                "b{index} i{}..i{} params={:?} succ={:?} args={:?}",
                block.first.0,
                block.end.0,
                block.parameters,
                block.successors,
                block.successor_arguments
            )
            .expect("writing to String cannot fail");
            for instruction_index in block.first.0..block.end.0 {
                let instruction = &self.instructions[instruction_index as usize];
                writeln!(
                    output,
                    "  i{instruction_index} {:?} {:?} clobbers={:?} sp={:?} state={:?} exits={:?}",
                    instruction.opcode,
                    instruction.operands,
                    instruction.clobbers,
                    instruction.safepoint,
                    instruction.frame_state,
                    instruction.exits
                )
                .expect("writing to String cannot fail");
                if !instruction.inline_frames.is_empty() {
                    writeln!(output, "    inline-frames={:?}", instruction.inline_frames)
                        .expect("writing to String cannot fail");
                }
            }
        }
        output
    }

    fn exceptional_target(&self, instruction: &MachineInstruction) -> Option<MachineBlock> {
        let MachineOpcode::Call(descriptor_index) = instruction.opcode else {
            return None;
        };
        match self
            .call_descriptors
            .get(descriptor_index as usize)?
            .exceptional
        {
            ExceptionalEdge::LandingPad(target) => Some(target),
            ExceptionalEdge::None | ExceptionalEdge::Propagate => None,
        }
    }

    fn live_values_on_edge(
        &self,
        predecessor: usize,
        successor_index: usize,
        block_live_in: &[Vec<bool>],
    ) -> Vec<bool> {
        let block = &self.blocks[predecessor];
        let successor = block.successors[successor_index];
        let successor_block = &self.blocks[successor.0 as usize];
        let mut live = block_live_in[successor.0 as usize].clone();
        for (&parameter, &argument) in successor_block
            .parameters
            .iter()
            .zip(&block.successor_arguments[successor_index])
        {
            let parameter_is_live = live[parameter.0 as usize];
            live[parameter.0 as usize] = false;
            if parameter_is_live {
                live[argument.0 as usize] = true;
            }
        }
        live
    }

    fn normal_live_out(&self, block_index: usize, block_live_in: &[Vec<bool>]) -> Vec<bool> {
        let block = &self.blocks[block_index];
        let exceptional_targets = (block.first.0..block.end.0)
            .filter_map(|instruction_index| {
                self.exceptional_target(&self.instructions[instruction_index as usize])
            })
            .collect::<std::collections::BTreeSet<_>>();
        let mut live = vec![false; self.representations.len()];
        for (successor_index, successor) in block.successors.iter().enumerate() {
            if exceptional_targets.contains(successor) {
                continue;
            }
            for (destination, source) in live.iter_mut().zip(self.live_values_on_edge(
                block_index,
                successor_index,
                block_live_in,
            )) {
                *destination |= source;
            }
        }
        live
    }

    fn live_before_instruction(
        &self,
        block_index: usize,
        instruction: &MachineInstruction,
        block_live_in: &[Vec<bool>],
        mut live: Vec<bool>,
    ) -> Vec<bool> {
        if let Some(target) = self.exceptional_target(instruction) {
            let successor_index = self.blocks[block_index]
                .successors
                .iter()
                .position(|successor| *successor == target)
                .expect("verified exceptional edge must name a stored successor");
            for (destination, source) in live.iter_mut().zip(self.live_values_on_edge(
                block_index,
                successor_index,
                block_live_in,
            )) {
                *destination |= source;
            }
        }
        for operand in &instruction.operands {
            if operand.role == OperandRole::Definition && operand.purpose == OperandPurpose::Output
            {
                live[operand.value.0 as usize] = false;
            }
        }
        for operand in &instruction.operands {
            if operand.role == OperandRole::Use
                && matches!(
                    operand.purpose,
                    OperandPurpose::Input | OperandPurpose::FrameState
                )
            {
                live[operand.value.0 as usize] = true;
            }
        }
        live
    }

    fn live_tagged_values_at_gc_safepoints(
        &self,
    ) -> Vec<(MachineInstructionId, Vec<MachineValue>)> {
        if self
            .instructions
            .iter()
            .all(|instruction| instruction.safepoint.is_none())
        {
            return Vec::new();
        }
        let mut block_live_in = vec![vec![false; self.representations.len()]; self.blocks.len()];
        loop {
            let mut changed = false;
            for block_index in (0..self.blocks.len()).rev() {
                let block = &self.blocks[block_index];
                let mut live = self.normal_live_out(block_index, &block_live_in);
                for instruction_index in (block.first.0..block.end.0).rev() {
                    live = self.live_before_instruction(
                        block_index,
                        &self.instructions[instruction_index as usize],
                        &block_live_in,
                        live,
                    );
                }
                if live != block_live_in[block_index] {
                    block_live_in[block_index] = live;
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }

        let mut safepoints = Vec::new();
        for (block_index, block) in self.blocks.iter().enumerate() {
            let mut live = self.normal_live_out(block_index, &block_live_in);
            for instruction_index in (block.first.0..block.end.0).rev() {
                let id = MachineInstructionId(instruction_index);
                let instruction = &self.instructions[instruction_index as usize];
                live = self.live_before_instruction(block_index, instruction, &block_live_in, live);
                if instruction.safepoint.is_none() {
                    continue;
                }
                let mut live_tagged = self
                    .representations
                    .iter()
                    .zip(&live)
                    .enumerate()
                    .filter_map(|(value, (&representation, &is_live))| {
                        (representation == MachineRepresentation::Tagged && is_live)
                            .then_some(MachineValue(value as u32))
                    })
                    .collect::<std::collections::BTreeSet<_>>();
                if let Some(state_id) = instruction.frame_state
                    && let Some(state) = self.frame_states.get(state_id as usize)
                {
                    for frame in &state.frames {
                        let slots = frame
                            .entry
                            .iter()
                            .flat_map(|entry| [&entry.new_target, &entry.this, &entry.closure])
                            .chain(frame.slots.iter());
                        for slot in slots {
                            let MachineFrameSlot::Value(value) = *slot else {
                                continue;
                            };
                            if self.representations.get(value.0 as usize)
                                == Some(&MachineRepresentation::Tagged)
                            {
                                live_tagged.insert(value);
                            }
                        }
                    }
                }
                for frame in &instruction.inline_frames {
                    let slots = frame
                        .entry
                        .iter()
                        .flat_map(|entry| [&entry.new_target, &entry.this, &entry.closure])
                        .chain(frame.slots.iter());
                    for &value in slots.flatten() {
                        if self.representations.get(value.0 as usize)
                            == Some(&MachineRepresentation::Tagged)
                        {
                            live_tagged.insert(value);
                        }
                    }
                }
                safepoints.push((id, live_tagged.into_iter().collect()));
            }
        }
        safepoints
    }

    fn complete_gc_root_liveness(&mut self) {
        for (id, live_tagged) in self.live_tagged_values_at_gc_safepoints() {
            let instruction = &mut self.instructions[id.0 as usize];
            let mut roots = instruction
                .operands
                .iter()
                .filter(|operand| operand.purpose == OperandPurpose::TaggedRoot)
                .map(|operand| operand.value)
                .collect::<std::collections::BTreeSet<_>>();
            for value in live_tagged {
                if roots.insert(value) {
                    instruction
                        .operands
                        .push(MachineOperand::tagged_root(value));
                }
            }
        }
    }

    fn verify_gc_root_liveness(&self) -> Result<(), VerificationError> {
        for (id, live_tagged) in self.live_tagged_values_at_gc_safepoints() {
            let instruction = &self.instructions[id.0 as usize];
            let mut roots = std::collections::BTreeSet::new();
            for root in instruction
                .operands
                .iter()
                .filter(|operand| operand.purpose == OperandPurpose::TaggedRoot)
            {
                if !roots.insert(root.value) {
                    return Err(VerificationError::DuplicateTaggedRoot(id, root.value));
                }
            }
            for &value in &live_tagged {
                if !roots.contains(&value) {
                    return Err(VerificationError::MissingLiveTaggedRoot(id, value));
                }
            }
            let expected = live_tagged
                .into_iter()
                .collect::<std::collections::BTreeSet<_>>();
            for value in roots {
                if !expected.contains(&value) {
                    return Err(VerificationError::UnexpectedTaggedRoot(id, value));
                }
            }
        }
        Ok(())
    }

    fn verify_structure(&self, target_spec: &TargetSpec) -> Result<(), VerificationError> {
        if self.entry.0 as usize >= self.blocks.len() {
            return Err(VerificationError::InvalidEntry);
        }
        for (expected, state) in self.frame_states.iter().enumerate() {
            let expected = expected as FrameStateId;
            if state.id != expected {
                return Err(VerificationError::NonDenseFrameStateId {
                    expected,
                    actual: state.id,
                });
            }
            for frame in &state.frames {
                let slots = frame
                    .entry
                    .iter()
                    .flat_map(|entry| [&entry.new_target, &entry.this, &entry.closure])
                    .chain(frame.slots.iter());
                for slot in slots {
                    if let MachineFrameSlot::Value(value) = *slot
                        && value.0 as usize >= self.representations.len()
                    {
                        return Err(VerificationError::InvalidValue(value));
                    }
                }
            }
        }
        let mut expected_first = 0u32;
        let mut safepoints = std::collections::BTreeSet::new();
        let mut deopts = std::collections::BTreeSet::new();
        let mut expected_predecessors = vec![Vec::new(); self.blocks.len()];
        for (predecessor, block) in self.blocks.iter().enumerate() {
            for successor in &block.successors {
                let Some(predecessors) = expected_predecessors.get_mut(successor.0 as usize) else {
                    return Err(VerificationError::InvalidBlock(*successor));
                };
                predecessors.push(MachineBlock(predecessor as u32));
            }
        }
        let mut landing_pad_uses = vec![Vec::new(); self.blocks.len()];
        for (source_index, block) in self.blocks.iter().enumerate() {
            if block.first.0 >= block.end.0 || block.end.0 as usize > self.instructions.len() {
                continue;
            }
            for instruction_index in block.first.0..block.end.0 {
                let instruction = &self.instructions[instruction_index as usize];
                if instruction.opcode == MachineOpcode::BranchNativeStatus {
                    if let Some(&target) = block.successors.get(1)
                        && self
                            .blocks
                            .get(target.0 as usize)
                            .is_some_and(|target_block| {
                                self.instructions
                                    .get(target_block.first.0 as usize)
                                    .and_then(|instruction| {
                                        let MachineOpcode::Call(descriptor) = instruction.opcode
                                        else {
                                            return None;
                                        };
                                        self.call_descriptors.get(descriptor as usize)
                                    })
                                    .is_some_and(is_caught_throw_acknowledgement_target)
                            })
                        && let Some(uses) = landing_pad_uses.get_mut(target.0 as usize)
                    {
                        uses.push((
                            MachineInstructionId(instruction_index),
                            MachineBlock(source_index as u32),
                        ));
                    }
                    continue;
                }
                let MachineOpcode::Call(descriptor_index) = instruction.opcode else {
                    continue;
                };
                let Some(descriptor) = self.call_descriptors.get(descriptor_index as usize) else {
                    continue;
                };
                let ExceptionalEdge::LandingPad(target) = descriptor.exceptional else {
                    continue;
                };
                if let Some(uses) = landing_pad_uses.get_mut(target.0 as usize) {
                    uses.push((
                        MachineInstructionId(instruction_index),
                        MachineBlock(source_index as u32),
                    ));
                }
            }
        }
        for (block_index, block) in self.blocks.iter().enumerate() {
            let block_id = MachineBlock(block_index as u32);
            if block.predecessors != expected_predecessors[block_index] {
                return Err(VerificationError::PredecessorMismatch(block_id));
            }
            if block.first.0 != expected_first {
                return Err(VerificationError::NonContiguousBlocks(block_id));
            }
            if block.first.0 >= block.end.0 || block.end.0 as usize > self.instructions.len() {
                return Err(VerificationError::InvalidBlockRange(block_id));
            }
            let acknowledgements = (block.first.0..block.end.0)
                .filter_map(|instruction_index| {
                    let instruction = &self.instructions[instruction_index as usize];
                    let MachineOpcode::Call(descriptor_index) = instruction.opcode else {
                        return None;
                    };
                    self.call_descriptors
                        .get(descriptor_index as usize)
                        .is_some_and(is_caught_throw_acknowledgement_target)
                        .then_some(MachineInstructionId(instruction_index))
                })
                .collect::<Vec<_>>();
            if let Some(&(_, source)) = landing_pad_uses[block_index].first() {
                if landing_pad_uses[block_index].len() != 1
                    || acknowledgements.as_slice() != [MachineInstructionId(block.first.0)]
                    || block.predecessors.as_slice() != [source]
                    || block.successors.len() != 1
                {
                    return Err(VerificationError::InvalidCaughtThrowLanding(block_id));
                }
            } else if let Some(&acknowledgement) = acknowledgements.first() {
                return Err(VerificationError::InvalidCaughtThrowAcknowledgement(
                    acknowledgement,
                ));
            }
            expected_first = block.end.0;
            for &other in block.predecessors.iter().chain(&block.successors) {
                if other.0 as usize >= self.blocks.len() {
                    return Err(VerificationError::InvalidBlock(other));
                }
            }
            if block.successors.len() != block.successor_arguments.len() {
                return Err(VerificationError::InvalidSuccessorArguments(block_id));
            }
            for (&successor, arguments) in block.successors.iter().zip(&block.successor_arguments) {
                let parameters = &self.blocks[successor.0 as usize].parameters;
                if arguments.len() != parameters.len() {
                    return Err(VerificationError::InvalidSuccessorArguments(block_id));
                }
                for (&argument, &parameter) in arguments.iter().zip(parameters) {
                    let Some(&argument_representation) =
                        self.representations.get(argument.0 as usize)
                    else {
                        return Err(VerificationError::InvalidValue(argument));
                    };
                    let Some(&parameter_representation) =
                        self.representations.get(parameter.0 as usize)
                    else {
                        return Err(VerificationError::InvalidValue(parameter));
                    };
                    if argument_representation != parameter_representation {
                        return Err(VerificationError::BlockParameterRepresentation(
                            successor, parameter,
                        ));
                    }
                }
            }
            for &parameter in &block.parameters {
                if parameter.0 as usize >= self.representations.len() {
                    return Err(VerificationError::InvalidValue(parameter));
                }
            }
            let exceptional_successors = (block.first.0..block.end.0)
                .filter_map(|instruction_index| {
                    let instruction = &self.instructions[instruction_index as usize];
                    let MachineOpcode::Call(descriptor_index) = instruction.opcode else {
                        return None;
                    };
                    match self
                        .call_descriptors
                        .get(descriptor_index as usize)?
                        .exceptional
                    {
                        ExceptionalEdge::LandingPad(target) => Some(target),
                        ExceptionalEdge::None | ExceptionalEdge::Propagate => None,
                    }
                })
                .collect::<std::collections::BTreeSet<_>>();
            let normal_successor_count = block
                .successors
                .iter()
                .filter(|successor| !exceptional_successors.contains(successor))
                .count();
            for instruction_index in block.first.0..block.end.0 {
                let id = MachineInstructionId(instruction_index);
                let instruction = &self.instructions[instruction_index as usize];
                let is_last = instruction_index + 1 == block.end.0;
                if is_last {
                    if !matches!(
                        instruction.control,
                        ControlFlow::Branch | ControlFlow::Return
                    ) {
                        return Err(VerificationError::MissingTerminator(block_id));
                    }
                    match instruction.opcode {
                        MachineOpcode::Jump if normal_successor_count != 1 => {
                            return Err(VerificationError::TerminatorSuccessors(block_id));
                        }
                        MachineOpcode::BranchIf(_) if normal_successor_count != 2 => {
                            return Err(VerificationError::TerminatorSuccessors(block_id));
                        }
                        MachineOpcode::BranchNativeStatus if normal_successor_count != 3 => {
                            return Err(VerificationError::TerminatorSuccessors(block_id));
                        }
                        MachineOpcode::Return | MachineOpcode::Throw | MachineOpcode::Fatal
                            if !block.successors.is_empty() =>
                        {
                            return Err(VerificationError::TerminatorSuccessors(block_id));
                        }
                        _ => {}
                    }
                } else if instruction.control != ControlFlow::None {
                    return Err(VerificationError::EarlyTerminator(id));
                }
                if let Some(safepoint) = instruction.safepoint
                    && !safepoints.insert(safepoint)
                {
                    return Err(VerificationError::DuplicateSafepoint(safepoint));
                }
                let state_valid = instruction
                    .frame_state
                    .is_some_and(|state| self.frame_states.get(state as usize).is_some());
                let owns_state = instruction.safepoint.is_some()
                    || !instruction.exits.is_empty()
                    || instruction
                        .operands
                        .iter()
                        .any(|operand| operand.purpose == OperandPurpose::FrameState);
                if owns_state && !state_valid {
                    return Err(VerificationError::InvalidFrameState(
                        id,
                        instruction.frame_state,
                    ));
                }
                if instruction.frame_state.is_some() && !state_valid {
                    return Err(VerificationError::InvalidFrameState(
                        id,
                        instruction.frame_state,
                    ));
                }
                let mut exit_reasons = std::collections::BTreeSet::new();
                for exit in &instruction.exits {
                    if !exit_reasons.insert(exit.reason) {
                        return Err(VerificationError::DuplicateExitReason(id, exit.reason));
                    }
                    if !deopts.insert(exit.id) {
                        return Err(VerificationError::DuplicateDeopt(exit.id));
                    }
                }
                let mut clobbers = std::collections::BTreeSet::new();
                for &clobber in &instruction.clobbers {
                    if !clobbers.insert(clobber) {
                        return Err(VerificationError::DuplicateClobber(id, clobber));
                    }
                }
                for operand in &instruction.operands {
                    if operand.value.0 as usize >= self.representations.len() {
                        return Err(VerificationError::InvalidValue(operand.value));
                    }
                }
                if let MachineOpcode::StringConstantCellLoad { target, .. } = instruction.opcode {
                    let [output] = instruction.operands.as_slice() else {
                        return Err(VerificationError::OpcodeSignatureMismatch(id));
                    };
                    let output_is_tagged_register = *output
                        == MachineOperand::register_output(output.value)
                        && self.representations[output.value.0 as usize]
                            == MachineRepresentation::Tagged;
                    if !output_is_tagged_register
                        || target.cell_addr == 0
                        || target.cell_addr % std::mem::align_of::<otter_vm::Value>() != 0
                        || instruction.clobbers
                            != target_spec.clobbers(TargetClobberSet::StringConstantLoad)
                        || !instruction.exits.is_empty()
                        || instruction.safepoint.is_some()
                    {
                        return Err(VerificationError::OpcodeSignatureMismatch(id));
                    }
                }
                match &instruction.opcode {
                    MachineOpcode::BooleanConstant(_) => {
                        let [output] = instruction.operands.as_slice() else {
                            return Err(VerificationError::OpcodeSignatureMismatch(id));
                        };
                        if *output != MachineOperand::register_output(output.value)
                            || self.representations[output.value.0 as usize]
                                != MachineRepresentation::Boolean
                            || !instruction.clobbers.is_empty()
                            || !instruction.exits.is_empty()
                            || instruction.safepoint.is_some()
                        {
                            return Err(VerificationError::OpcodeSignatureMismatch(id));
                        }
                    }
                    MachineOpcode::BooleanOr => {
                        let [left, right, output] = instruction.operands.as_slice() else {
                            return Err(VerificationError::OpcodeSignatureMismatch(id));
                        };
                        if [left, right].iter().any(|operand| {
                            **operand != MachineOperand::register_input(operand.value)
                                || self.representations[operand.value.0 as usize]
                                    != MachineRepresentation::Boolean
                        }) || *output != MachineOperand::register_output(output.value)
                            || self.representations[output.value.0 as usize]
                                != MachineRepresentation::Boolean
                            || !instruction.clobbers.is_empty()
                            || !instruction.exits.is_empty()
                            || instruction.safepoint.is_some()
                        {
                            return Err(VerificationError::OpcodeSignatureMismatch(id));
                        }
                    }
                    MachineOpcode::TaggedSelect => {
                        let [condition, if_true, if_false, output] =
                            instruction.operands.as_slice()
                        else {
                            return Err(VerificationError::OpcodeSignatureMismatch(id));
                        };
                        if *condition != MachineOperand::register_input(condition.value)
                            || self.representations[condition.value.0 as usize]
                                != MachineRepresentation::Boolean
                            || [if_true, if_false].iter().any(|operand| {
                                **operand != MachineOperand::register_input(operand.value)
                                    || self.representations[operand.value.0 as usize]
                                        != MachineRepresentation::Tagged
                            })
                            || *output != MachineOperand::register_output(output.value)
                            || self.representations[output.value.0 as usize]
                                != MachineRepresentation::Tagged
                            || !instruction.clobbers.is_empty()
                            || !instruction.exits.is_empty()
                            || instruction.safepoint.is_some()
                        {
                            return Err(VerificationError::OpcodeSignatureMismatch(id));
                        }
                    }
                    MachineOpcode::PropertySource { .. } => {
                        let [output] = instruction.operands.as_slice() else {
                            return Err(VerificationError::OpcodeSignatureMismatch(id));
                        };
                        if *output != MachineOperand::register_output(output.value)
                            || self.representations[output.value.0 as usize]
                                != MachineRepresentation::Int64
                            || !instruction.clobbers.is_empty()
                            || !instruction.exits.is_empty()
                            || instruction.safepoint.is_some()
                        {
                            return Err(VerificationError::OpcodeSignatureMismatch(id));
                        }
                    }
                    MachineOpcode::CacheIrGuardShape { shape, .. } => {
                        let [object, active, output] = instruction.operands.as_slice() else {
                            return Err(VerificationError::OpcodeSignatureMismatch(id));
                        };
                        if *shape == 0
                            || *object != MachineOperand::location_input(object.value)
                            || self.representations[object.value.0 as usize]
                                != MachineRepresentation::Tagged
                            || *active != MachineOperand::register_input(active.value)
                            || self.representations[active.value.0 as usize]
                                != MachineRepresentation::Boolean
                            || *output != MachineOperand::register_output(output.value)
                            || self.representations[output.value.0 as usize]
                                != MachineRepresentation::Boolean
                            || instruction.clobbers
                                != target_spec.clobbers(TargetClobberSet::PropertyLoad)
                            || !instruction.exits.is_empty()
                            || instruction.safepoint.is_some()
                        {
                            return Err(VerificationError::OpcodeSignatureMismatch(id));
                        }
                    }
                    MachineOpcode::CacheIrGuardAtomSlot {
                        atom, value_byte, ..
                    } => {
                        let [object, active, output] = instruction.operands.as_slice() else {
                            return Err(VerificationError::OpcodeSignatureMismatch(id));
                        };
                        if *atom == u32::MAX
                            || *value_byte % 8 != 0
                            || *object != MachineOperand::location_input(object.value)
                            || self.representations[object.value.0 as usize]
                                != MachineRepresentation::Tagged
                            || *active != MachineOperand::register_input(active.value)
                            || self.representations[active.value.0 as usize]
                                != MachineRepresentation::Boolean
                            || *output != MachineOperand::register_output(output.value)
                            || self.representations[output.value.0 as usize]
                                != MachineRepresentation::Boolean
                            || instruction.clobbers
                                != target_spec.clobbers(TargetClobberSet::PropertyLoad)
                            || !instruction.exits.is_empty()
                            || instruction.safepoint.is_some()
                        {
                            return Err(VerificationError::OpcodeSignatureMismatch(id));
                        }
                    }
                    MachineOpcode::CacheIrGuardPrototypeNull { .. }
                    | MachineOpcode::CacheIrGuardExtensible { .. } => {
                        let [object, active, output] = instruction.operands.as_slice() else {
                            return Err(VerificationError::OpcodeSignatureMismatch(id));
                        };
                        if matches!(
                            &instruction.opcode,
                            MachineOpcode::CacheIrGuardExtensible { value_byte, .. }
                                if *value_byte % 8 != 0
                        ) || *object != MachineOperand::location_input(object.value)
                            || self.representations[object.value.0 as usize]
                                != MachineRepresentation::Tagged
                            || *active != MachineOperand::register_input(active.value)
                            || self.representations[active.value.0 as usize]
                                != MachineRepresentation::Boolean
                            || *output != MachineOperand::register_output(output.value)
                            || self.representations[output.value.0 as usize]
                                != MachineRepresentation::Boolean
                            || instruction.clobbers
                                != target_spec.clobbers(TargetClobberSet::PropertyLoad)
                            || !instruction.exits.is_empty()
                            || instruction.safepoint.is_some()
                        {
                            return Err(VerificationError::OpcodeSignatureMismatch(id));
                        }
                    }
                    MachineOpcode::CacheIrLoadPrototype { .. }
                    | MachineOpcode::CacheIrLoadField { .. } => {
                        let [object, active, payload, hit] = instruction.operands.as_slice() else {
                            return Err(VerificationError::OpcodeSignatureMismatch(id));
                        };
                        if *object != MachineOperand::location_input(object.value)
                            || self.representations[object.value.0 as usize]
                                != MachineRepresentation::Tagged
                            || *active != MachineOperand::register_input(active.value)
                            || self.representations[active.value.0 as usize]
                                != MachineRepresentation::Boolean
                            || *payload != MachineOperand::register_output(payload.value)
                            || self.representations[payload.value.0 as usize]
                                != MachineRepresentation::Tagged
                            || *hit != MachineOperand::register_output(hit.value)
                            || self.representations[hit.value.0 as usize]
                                != MachineRepresentation::Boolean
                            || instruction.clobbers
                                != target_spec.clobbers(TargetClobberSet::PropertyLoad)
                            || !instruction.exits.is_empty()
                            || instruction.safepoint.is_some()
                        {
                            return Err(VerificationError::OpcodeSignatureMismatch(id));
                        }
                    }
                    MachineOpcode::CacheIrStoreField { .. } => {
                        let [object, value, active, owner, hit] = instruction.operands.as_slice()
                        else {
                            return Err(VerificationError::OpcodeSignatureMismatch(id));
                        };
                        if [object, value].iter().any(|operand| {
                            **operand != MachineOperand::location_input(operand.value)
                                || self.representations[operand.value.0 as usize]
                                    != MachineRepresentation::Tagged
                        }) || *active != MachineOperand::register_input(active.value)
                            || self.representations[active.value.0 as usize]
                                != MachineRepresentation::Boolean
                            || *owner != MachineOperand::register_output(owner.value)
                            || self.representations[owner.value.0 as usize]
                                != MachineRepresentation::Int64
                            || *hit != MachineOperand::register_output(hit.value)
                            || self.representations[hit.value.0 as usize]
                                != MachineRepresentation::Boolean
                            || instruction.clobbers
                                != target_spec.clobbers(TargetClobberSet::PropertyStore)
                            || !instruction.exits.is_empty()
                            || instruction.safepoint.is_some()
                        {
                            return Err(VerificationError::OpcodeSignatureMismatch(id));
                        }
                    }
                    MachineOpcode::CacheIrPublishShape { shape, new_len, .. } => {
                        let [owner, active] = instruction.operands.as_slice() else {
                            return Err(VerificationError::OpcodeSignatureMismatch(id));
                        };
                        if *shape == 0
                            || *new_len == 0
                            || *owner != MachineOperand::location_input(owner.value)
                            || self.representations[owner.value.0 as usize]
                                != MachineRepresentation::Int64
                            || *active != MachineOperand::register_input(active.value)
                            || self.representations[active.value.0 as usize]
                                != MachineRepresentation::Boolean
                            || instruction.clobbers
                                != target_spec.clobbers(TargetClobberSet::PropertyStore)
                            || !instruction.exits.is_empty()
                            || instruction.safepoint.is_some()
                        {
                            return Err(VerificationError::OpcodeSignatureMismatch(id));
                        }
                    }
                    MachineOpcode::CacheIrWriteBarrier { .. } => {
                        let [owner, value, active] = instruction.operands.as_slice() else {
                            return Err(VerificationError::OpcodeSignatureMismatch(id));
                        };
                        if *owner != MachineOperand::location_input(owner.value)
                            || self.representations[owner.value.0 as usize]
                                != MachineRepresentation::Int64
                            || *value != MachineOperand::location_input(value.value)
                            || self.representations[value.value.0 as usize]
                                != MachineRepresentation::Tagged
                            || *active != MachineOperand::register_input(active.value)
                            || self.representations[active.value.0 as usize]
                                != MachineRepresentation::Boolean
                            || instruction.clobbers
                                != target_spec.clobbers(TargetClobberSet::PropertyStore)
                            || !instruction.exits.is_empty()
                            || instruction.safepoint.is_some()
                        {
                            return Err(VerificationError::OpcodeSignatureMismatch(id));
                        }
                    }
                    MachineOpcode::ExoticLength { .. } => {
                        let [receiver, payload, hit] = instruction.operands.as_slice() else {
                            return Err(VerificationError::OpcodeSignatureMismatch(id));
                        };
                        if *receiver != MachineOperand::location_input(receiver.value)
                            || self.representations[receiver.value.0 as usize]
                                != MachineRepresentation::Tagged
                            || *payload != MachineOperand::register_output(payload.value)
                            || self.representations[payload.value.0 as usize]
                                != MachineRepresentation::Tagged
                            || *hit != MachineOperand::register_output(hit.value)
                            || self.representations[hit.value.0 as usize]
                                != MachineRepresentation::Boolean
                            || instruction.clobbers
                                != target_spec.clobbers(TargetClobberSet::PropertyLoad)
                            || !instruction.exits.is_empty()
                            || instruction.safepoint.is_some()
                        {
                            return Err(VerificationError::OpcodeSignatureMismatch(id));
                        }
                    }
                    MachineOpcode::CacheIrJoin { store, .. } => {
                        let valid = if *store {
                            let [input, output] = instruction.operands.as_slice() else {
                                return Err(VerificationError::OpcodeSignatureMismatch(id));
                            };
                            *input == MachineOperand::register_input(input.value)
                                && *output == MachineOperand::register_output(output.value)
                                && [input, output].iter().all(|operand| {
                                    self.representations[operand.value.0 as usize]
                                        == MachineRepresentation::Boolean
                                })
                        } else {
                            let [payload, hit, output_payload, output_hit] =
                                instruction.operands.as_slice()
                            else {
                                return Err(VerificationError::OpcodeSignatureMismatch(id));
                            };
                            *payload == MachineOperand::register_input(payload.value)
                                && *hit == MachineOperand::register_input(hit.value)
                                && *output_payload
                                    == MachineOperand::register_output(output_payload.value)
                                && *output_hit == MachineOperand::register_output(output_hit.value)
                                && self.representations[payload.value.0 as usize]
                                    == MachineRepresentation::Tagged
                                && self.representations[output_payload.value.0 as usize]
                                    == MachineRepresentation::Tagged
                                && self.representations[hit.value.0 as usize]
                                    == MachineRepresentation::Boolean
                                && self.representations[output_hit.value.0 as usize]
                                    == MachineRepresentation::Boolean
                        };
                        if !valid
                            || !instruction.clobbers.is_empty()
                            || !instruction.exits.is_empty()
                            || instruction.safepoint.is_some()
                        {
                            return Err(VerificationError::OpcodeSignatureMismatch(id));
                        }
                    }
                    MachineOpcode::AllocateObject { .. }
                    | MachineOpcode::AllocationHit
                    | MachineOpcode::BaseConstructResult => {
                        let (inputs, outputs, result, clobbers) = match instruction.opcode {
                            MachineOpcode::AllocateObject { .. } => (
                                1,
                                2,
                                MachineRepresentation::Tagged,
                                TargetClobberSet::ConstructReceiver,
                            ),
                            MachineOpcode::AllocationHit => (
                                1,
                                1,
                                MachineRepresentation::Boolean,
                                TargetClobberSet::ConstructReceiverHit,
                            ),
                            _ => (
                                2,
                                1,
                                MachineRepresentation::Tagged,
                                TargetClobberSet::BaseConstructResult,
                            ),
                        };
                        if instruction.operands.len() != inputs + outputs
                            || !instruction
                                .operands
                                .iter()
                                .enumerate()
                                .all(|(index, operand)| {
                                    *operand
                                        == if index < inputs {
                                            MachineOperand::register_input(operand.value)
                                        } else {
                                            MachineOperand::register_output(operand.value)
                                        }
                                        && self.representations[operand.value.0 as usize]
                                            == if matches!(
                                                instruction.opcode,
                                                MachineOpcode::AllocateObject { .. }
                                            ) && index == inputs + 1
                                            {
                                                MachineRepresentation::Int64
                                            } else if index < inputs {
                                                MachineRepresentation::Tagged
                                            } else {
                                                result
                                            }
                                })
                            || instruction.clobbers != target_spec.clobbers(clobbers)
                            || !instruction.exits.is_empty()
                            || instruction.safepoint.is_some()
                        {
                            return Err(VerificationError::OpcodeSignatureMismatch(id));
                        }
                    }
                    MachineOpcode::PublishObject { .. } => {
                        let [new_target, input, page, hit, output] =
                            instruction.operands.as_slice()
                        else {
                            return Err(VerificationError::OpcodeSignatureMismatch(id));
                        };
                        if *new_target != MachineOperand::register_input(new_target.value)
                            || *input != MachineOperand::register_input(input.value)
                            || *page != MachineOperand::register_input(page.value)
                            || *hit != MachineOperand::register_input(hit.value)
                            || *output != MachineOperand::register_output(output.value)
                            || self.representations[new_target.value.0 as usize]
                                != MachineRepresentation::Tagged
                            || self.representations[input.value.0 as usize]
                                != MachineRepresentation::Tagged
                            || self.representations[page.value.0 as usize]
                                != MachineRepresentation::Int64
                            || self.representations[hit.value.0 as usize]
                                != MachineRepresentation::Boolean
                            || self.representations[output.value.0 as usize]
                                != MachineRepresentation::Tagged
                            || instruction.clobbers
                                != target_spec.clobbers(TargetClobberSet::ConstructReceiver)
                            || instruction.safepoint.is_some()
                            || !instruction.exits.is_empty()
                        {
                            return Err(VerificationError::OpcodeSignatureMismatch(id));
                        }
                    }
                    MachineOpcode::GuardCallTarget { .. } => {
                        let [input, output, late @ ..] = instruction.operands.as_slice() else {
                            return Err(VerificationError::OpcodeSignatureMismatch(id));
                        };
                        let clobbers = if matches!(
                            instruction.opcode,
                            MachineOpcode::GuardCallTarget {
                                guard: MachineCallGuard::Method(_),
                            }
                        ) {
                            TargetClobberSet::InlineMethodGuard
                        } else {
                            TargetClobberSet::InlineCallGuard
                        };
                        if late
                            .iter()
                            .any(|operand| *operand != MachineOperand::frame_value(operand.value))
                            || *input != MachineOperand::register_input(input.value)
                            || *output != MachineOperand::register_output(output.value)
                            || [input, output].iter().any(|operand| {
                                self.representations[operand.value.0 as usize]
                                    != MachineRepresentation::Tagged
                            })
                            || instruction.clobbers != target_spec.clobbers(clobbers)
                            || instruction.exits.is_empty()
                            || instruction.safepoint.is_some()
                        {
                            return Err(VerificationError::OpcodeSignatureMismatch(id));
                        }
                    }
                    MachineOpcode::ResolveCallThis { .. } => {
                        let [input, output] = instruction.operands.as_slice() else {
                            return Err(VerificationError::OpcodeSignatureMismatch(id));
                        };
                        if *input != MachineOperand::register_input(input.value)
                            || *output != MachineOperand::register_output(output.value)
                            || self.representations[input.value.0 as usize]
                                != MachineRepresentation::Tagged
                            || self.representations[output.value.0 as usize]
                                != MachineRepresentation::Tagged
                            || instruction.clobbers
                                != target_spec.clobbers(TargetClobberSet::InlineCallGuard)
                            || !instruction.exits.is_empty()
                            || instruction.safepoint.is_some()
                        {
                            return Err(VerificationError::OpcodeSignatureMismatch(id));
                        }
                    }
                    MachineOpcode::TruthinessProbe | MachineOpcode::LooseEqualityProbe { .. } => {
                        let (input_count, result_type) =
                            if matches!(instruction.opcode, MachineOpcode::TruthinessProbe) {
                                (1, MachineRepresentation::Boolean)
                            } else {
                                (2, MachineRepresentation::Tagged)
                            };
                        let valid =
                            instruction.operands.len() == input_count + 2
                                && instruction.operands.iter().enumerate().all(
                                    |(index, operand)| {
                                        let (expected, representation) = if index < input_count {
                                            (
                                                MachineOperand::register_input(operand.value),
                                                MachineRepresentation::Tagged,
                                            )
                                        } else {
                                            (
                                                MachineOperand::register_output(operand.value),
                                                if index == input_count {
                                                    result_type
                                                } else {
                                                    MachineRepresentation::Boolean
                                                },
                                            )
                                        };
                                        *operand == expected
                                            && self.representations[operand.value.0 as usize]
                                                == representation
                                    },
                                );
                        if !valid
                            || !instruction.clobbers.is_empty()
                            || !instruction.exits.is_empty()
                            || instruction.safepoint.is_some()
                        {
                            return Err(VerificationError::OpcodeSignatureMismatch(id));
                        }
                    }
                    MachineOpcode::BindingGuard {
                        semantics, target, ..
                    } => {
                        let Some((outputs, inputs)) = instruction.operands.split_at_checked(3)
                        else {
                            return Err(VerificationError::OpcodeSignatureMismatch(id));
                        };
                        let expected_inputs =
                            semantics.value_operands().into_iter().flatten().count();
                        let valid_outputs = outputs
                            .iter()
                            .zip([
                                MachineRepresentation::Boolean,
                                MachineRepresentation::Int64,
                                MachineRepresentation::Int64,
                            ])
                            .all(|(operand, representation)| {
                                *operand == MachineOperand::register_output(operand.value)
                                    && self.representations[operand.value.0 as usize]
                                        == representation
                            });
                        let valid_inputs = inputs.len() == expected_inputs
                            && inputs.iter().all(|operand| {
                                *operand == MachineOperand::location_input(operand.value)
                                    && self.representations[operand.value.0 as usize]
                                        == MachineRepresentation::Tagged
                            });
                        let raw_addresses = [outputs[1].value, outputs[2].value];
                        let hit_block = (!matches!(target, MachineBindingTarget::Cold))
                            .then(|| block.successors.first().copied())
                            .flatten();
                        let raw_addresses_are_hit_local = self.blocks.iter().enumerate().all(
                            |(candidate_block_index, candidate_block)| {
                                if candidate_block
                                    .parameters
                                    .iter()
                                    .chain(candidate_block.successor_arguments.iter().flatten())
                                    .any(|value| raw_addresses.contains(value))
                                {
                                    return false;
                                }
                                for candidate_index in
                                    candidate_block.first.0..candidate_block.end.0
                                {
                                    let Some(candidate) =
                                        self.instructions.get(candidate_index as usize)
                                    else {
                                        return false;
                                    };
                                    for (operand_index, operand) in
                                        candidate.operands.iter().enumerate()
                                    {
                                        if operand.role != OperandRole::Use
                                            || !raw_addresses.contains(&operand.value)
                                        {
                                            continue;
                                        }
                                        let in_hit_block = hit_block
                                            == Some(MachineBlock(candidate_block_index as u32));
                                        let allowed = in_hit_block
                                            && operand.purpose == OperandPurpose::Input
                                            && match candidate.opcode {
                                                MachineOpcode::BindingHit { .. } => {
                                                    (operand_index == 0
                                                        && operand.value == raw_addresses[0])
                                                        || (operand_index == 1
                                                            && operand.value == raw_addresses[1])
                                                }
                                                MachineOpcode::BindingWriteBarrier => {
                                                    operand_index == 0
                                                        && operand.value == raw_addresses[0]
                                                }
                                                _ => false,
                                            };
                                        if !allowed {
                                            return false;
                                        }
                                    }
                                }
                                true
                            },
                        );
                        let writable = !matches!(
                            semantics,
                            otter_bytecode::opcode_schema::BindingSemantics::Write(_)
                        ) || match target {
                            MachineBindingTarget::Global(
                                otter_vm::jit::BindingHitProof::GlobalLexical { writable, .. }
                                | otter_vm::jit::BindingHitProof::GlobalObject { writable, .. },
                            ) => *writable,
                            MachineBindingTarget::Cold
                            | MachineBindingTarget::GlobalThis
                            | MachineBindingTarget::Upvalue { .. } => true,
                        };
                        if !valid_outputs
                            || !valid_inputs
                            || !binding_target_matches_semantics(*semantics, *target)
                            || !raw_addresses_are_hit_local
                            || !writable
                            || instruction.clobbers != binding_guard_clobbers(target_spec)
                            || !instruction.exits.is_empty()
                            || instruction.safepoint.is_some()
                        {
                            return Err(VerificationError::OpcodeSignatureMismatch(id));
                        }
                    }
                    MachineOpcode::BindingHit {
                        semantics, target, ..
                    } => {
                        if matches!(target, MachineBindingTarget::Cold) {
                            return Err(VerificationError::OpcodeSignatureMismatch(id));
                        }
                        let expected_output = usize::from(semantics.result_operand().is_some());
                        let expected_inputs = 2 + usize::from(matches!(
                            semantics,
                            otter_bytecode::opcode_schema::BindingSemantics::Write(_)
                        ));
                        let (inputs, outputs) = instruction
                            .operands
                            .split_at_checked(expected_inputs)
                            .ok_or(VerificationError::OpcodeSignatureMismatch(id))?;
                        let valid_addresses = inputs.get(..2).is_some_and(|addresses| {
                            addresses.iter().all(|operand| {
                                *operand == MachineOperand::location_input(operand.value)
                                    && self.representations[operand.value.0 as usize]
                                        == MachineRepresentation::Int64
                            })
                        });
                        let valid_value = inputs.get(2).is_none_or(|operand| {
                            *operand == MachineOperand::location_input(operand.value)
                                && self.representations[operand.value.0 as usize]
                                    == MachineRepresentation::Tagged
                        });
                        let valid_output = outputs.len() == expected_output
                            && outputs.iter().all(|operand| {
                                *operand == MachineOperand::register_output(operand.value)
                                    && self.representations[operand.value.0 as usize]
                                        == MachineRepresentation::Tagged
                            });
                        if !valid_addresses
                            || !valid_value
                            || !valid_output
                            || !binding_target_matches_semantics(*semantics, *target)
                            || instruction.clobbers != binding_hit_clobbers(target_spec)
                            || !instruction.exits.is_empty()
                            || instruction.safepoint.is_some()
                        {
                            return Err(VerificationError::OpcodeSignatureMismatch(id));
                        }
                    }
                    MachineOpcode::BindingWriteBarrier => {
                        let [owner, value] = instruction.operands.as_slice() else {
                            return Err(VerificationError::OpcodeSignatureMismatch(id));
                        };
                        if *owner != MachineOperand::location_input(owner.value)
                            || self.representations[owner.value.0 as usize]
                                != MachineRepresentation::Int64
                            || *value != MachineOperand::location_input(value.value)
                            || self.representations[value.value.0 as usize]
                                != MachineRepresentation::Tagged
                            || instruction.clobbers != binding_write_barrier_clobbers(target_spec)
                            || !instruction.exits.is_empty()
                            || instruction.safepoint.is_some()
                        {
                            return Err(VerificationError::OpcodeSignatureMismatch(id));
                        }
                    }
                    MachineOpcode::BindingJoin { .. } => {
                        if !instruction.operands.is_empty()
                            || !instruction.clobbers.is_empty()
                            || !instruction.exits.is_empty()
                            || instruction.safepoint.is_some()
                        {
                            return Err(VerificationError::OpcodeSignatureMismatch(id));
                        }
                    }
                    MachineOpcode::BranchNativeStatus => {
                        let [status] = instruction.operands.as_slice() else {
                            return Err(VerificationError::OpcodeSignatureMismatch(id));
                        };
                        let producers = self
                            .instructions
                            .iter()
                            .filter(|candidate| {
                                candidate.operands.iter().any(|operand| {
                                    operand.purpose == OperandPurpose::Output
                                        && operand.value == status.value
                                })
                            })
                            .collect::<Vec<_>>();
                        let committed_pair_status = if let [producer] = producers.as_slice() {
                            let MachineOpcode::Call(descriptor) = producer.opcode else {
                                return Err(VerificationError::OpcodeSignatureMismatch(id));
                            };
                            self.call_descriptors
                                .get(descriptor as usize)
                                .is_some_and(is_explicit_committed_runtime_call)
                        } else {
                            false
                        };
                        if *status != MachineOperand::register_input(status.value)
                            || self.representations[status.value.0 as usize]
                                != MachineRepresentation::NativeStatus
                            || !committed_pair_status
                            || !instruction.exits.is_empty()
                            || instruction.safepoint.is_some()
                            || instruction.clobbers
                                != target_spec.clobbers(TargetClobberSet::StatusScratch)
                        {
                            return Err(VerificationError::OpcodeSignatureMismatch(id));
                        }
                    }
                    MachineOpcode::Throw => {
                        let [exception] = instruction.operands.as_slice() else {
                            return Err(VerificationError::OpcodeSignatureMismatch(id));
                        };
                        if *exception != MachineOperand::register_input(exception.value)
                            || self.representations[exception.value.0 as usize]
                                != MachineRepresentation::Tagged
                            || !instruction.clobbers.is_empty()
                            || !instruction.exits.is_empty()
                            || instruction.safepoint.is_some()
                        {
                            return Err(VerificationError::OpcodeSignatureMismatch(id));
                        }
                    }
                    MachineOpcode::Fatal
                        if !instruction.operands.is_empty()
                            || !instruction.clobbers.is_empty()
                            || !instruction.exits.is_empty()
                            || instruction.safepoint.is_some() =>
                    {
                        return Err(VerificationError::OpcodeSignatureMismatch(id));
                    }
                    MachineOpcode::LoopPreheader
                        if !instruction.operands.is_empty()
                            || !instruction.clobbers.is_empty()
                            || !instruction.exits.is_empty()
                            || instruction.safepoint.is_some()
                            || instruction.frame_state.is_some()
                            || instruction.control != ControlFlow::None =>
                    {
                        return Err(VerificationError::OpcodeSignatureMismatch(id));
                    }
                    _ => {}
                }
                if matches!(instruction.opcode, MachineOpcode::TaggedNullishEqual { .. }) {
                    let Some((ordinary, metadata)) = instruction.operands.split_at_checked(2)
                    else {
                        return Err(VerificationError::OpcodeSignatureMismatch(id));
                    };
                    let input = ordinary[0];
                    let output = ordinary[1];
                    let ordinary_signature = input == MachineOperand::register_input(input.value)
                        && self.representations[input.value.0 as usize]
                            == MachineRepresentation::Tagged
                        && output == MachineOperand::register_output(output.value)
                        && self.representations[output.value.0 as usize]
                            == MachineRepresentation::Boolean;
                    let mut deopt_values = std::collections::BTreeSet::new();
                    let metadata_is_exact_deopt = metadata.iter().all(|operand| {
                        *operand == MachineOperand::frame_value(operand.value)
                            && operand.value != output.value
                            && deopt_values.insert(operand.value)
                    });
                    if !ordinary_signature
                        || !metadata_is_exact_deopt
                        || !deopt_values.contains(&input.value)
                        || instruction.exits.is_empty()
                        || instruction.safepoint.is_some()
                        || instruction.clobbers
                            != target_spec.clobbers(TargetClobberSet::StatusScratch)
                    {
                        return Err(VerificationError::OpcodeSignatureMismatch(id));
                    }
                }
                if matches!(instruction.opcode, MachineOpcode::ElementView { .. }) {
                    let [receiver, base, length, hit] = instruction.operands.as_slice() else {
                        return Err(VerificationError::OpcodeSignatureMismatch(id));
                    };
                    let valid = *receiver == MachineOperand::location_input(receiver.value)
                        && *base == MachineOperand::register_output(base.value)
                        && *length == MachineOperand::register_output(length.value)
                        && *hit == MachineOperand::register_output(hit.value)
                        && self.representations[receiver.value.0 as usize]
                            == MachineRepresentation::Tagged
                        && self.representations[base.value.0 as usize]
                            == MachineRepresentation::Int64
                        && self.representations[length.value.0 as usize]
                            == MachineRepresentation::Int64
                        && self.representations[hit.value.0 as usize]
                            == MachineRepresentation::Boolean;
                    if !valid {
                        return Err(VerificationError::OpcodeSignatureMismatch(id));
                    }
                }
                if matches!(instruction.opcode, MachineOpcode::ElementAddress { .. }) {
                    let [base, length, index, active, address, hit] =
                        instruction.operands.as_slice()
                    else {
                        return Err(VerificationError::OpcodeSignatureMismatch(id));
                    };
                    let valid = [base, length, index]
                        .into_iter()
                        .all(|operand| *operand == MachineOperand::location_input(operand.value))
                        && *active == MachineOperand::register_input(active.value)
                        && *address == MachineOperand::register_output(address.value)
                        && *hit == MachineOperand::register_output(hit.value)
                        && self.representations[base.value.0 as usize]
                            == MachineRepresentation::Int64
                        && self.representations[length.value.0 as usize]
                            == MachineRepresentation::Int64
                        && self.representations[index.value.0 as usize]
                            == MachineRepresentation::Tagged
                        && self.representations[active.value.0 as usize]
                            == MachineRepresentation::Boolean
                        && self.representations[address.value.0 as usize]
                            == MachineRepresentation::Int64
                        && self.representations[hit.value.0 as usize]
                            == MachineRepresentation::Boolean;
                    if !valid {
                        return Err(VerificationError::OpcodeSignatureMismatch(id));
                    }
                }
                if matches!(instruction.opcode, MachineOpcode::ElementValueLoad { .. }) {
                    let [address, active, value, hit] = instruction.operands.as_slice() else {
                        return Err(VerificationError::OpcodeSignatureMismatch(id));
                    };
                    let valid = *address == MachineOperand::location_input(address.value)
                        && *active == MachineOperand::register_input(active.value)
                        && *value == MachineOperand::register_output(value.value)
                        && *hit == MachineOperand::register_output(hit.value)
                        && self.representations[address.value.0 as usize]
                            == MachineRepresentation::Int64
                        && self.representations[active.value.0 as usize]
                            == MachineRepresentation::Boolean
                        && self.representations[value.value.0 as usize]
                            == MachineRepresentation::Tagged
                        && self.representations[hit.value.0 as usize]
                            == MachineRepresentation::Boolean;
                    if !valid {
                        return Err(VerificationError::OpcodeSignatureMismatch(id));
                    }
                }
                if matches!(instruction.opcode, MachineOpcode::ElementValueGuard { .. }) {
                    let [address, value, active, hit] = instruction.operands.as_slice() else {
                        return Err(VerificationError::OpcodeSignatureMismatch(id));
                    };
                    let value_representation = self.representations[value.value.0 as usize];
                    let valid = *address == MachineOperand::location_input(address.value)
                        && *value == MachineOperand::location_input(value.value)
                        && *active == MachineOperand::register_input(active.value)
                        && *hit == MachineOperand::register_output(hit.value)
                        && self.representations[address.value.0 as usize]
                            == MachineRepresentation::Int64
                        && matches!(
                            value_representation,
                            MachineRepresentation::Tagged | MachineRepresentation::Float64
                        )
                        && self.representations[active.value.0 as usize]
                            == MachineRepresentation::Boolean
                        && self.representations[hit.value.0 as usize]
                            == MachineRepresentation::Boolean;
                    if !valid {
                        return Err(VerificationError::OpcodeSignatureMismatch(id));
                    }
                }
                if matches!(instruction.opcode, MachineOpcode::ElementValueStore { .. }) {
                    let [address, value] = instruction.operands.as_slice() else {
                        return Err(VerificationError::OpcodeSignatureMismatch(id));
                    };
                    let valid = *address == MachineOperand::location_input(address.value)
                        && *value == MachineOperand::location_input(value.value)
                        && self.representations[address.value.0 as usize]
                            == MachineRepresentation::Int64
                        && matches!(
                            self.representations[value.value.0 as usize],
                            MachineRepresentation::Tagged | MachineRepresentation::Float64
                        );
                    if !valid {
                        return Err(VerificationError::OpcodeSignatureMismatch(id));
                    }
                }
                if matches!(
                    instruction.opcode,
                    MachineOpcode::ElementView { .. }
                        | MachineOpcode::ElementAddress { .. }
                        | MachineOpcode::ElementValueLoad { .. }
                        | MachineOpcode::ElementValueGuard { .. }
                        | MachineOpcode::ElementValueStore { .. }
                ) && (instruction.clobbers != target_spec.clobbers(TargetClobberSet::Element)
                    || instruction.safepoint.is_some()
                    || !instruction.exits.is_empty()
                    || instruction.control != ControlFlow::None)
                {
                    return Err(VerificationError::OpcodeSignatureMismatch(id));
                }
                if matches!(instruction.opcode, MachineOpcode::GuardCondition) {
                    let Some((condition, state)) = instruction.operands.split_first() else {
                        return Err(VerificationError::OpcodeSignatureMismatch(id));
                    };
                    let mut values = std::collections::BTreeSet::new();
                    let valid = *condition == MachineOperand::register_input(condition.value)
                        && self.representations[condition.value.0 as usize]
                            == MachineRepresentation::Boolean
                        && state.iter().all(|operand| {
                            *operand == MachineOperand::frame_value(operand.value)
                                && values.insert(operand.value)
                        })
                        && !instruction.exits.is_empty()
                        && instruction.clobbers
                            == target_spec.clobbers(TargetClobberSet::StatusScratch)
                        && instruction.safepoint.is_none()
                        && instruction.control == ControlFlow::None;
                    if !valid {
                        return Err(VerificationError::OpcodeSignatureMismatch(id));
                    }
                }
                if let MachineOpcode::TryBindDerivedThis { byte_pc } = instruction.opcode {
                    let valid = instruction.operands.len() == 2
                        && instruction.operands[0]
                            == MachineOperand::fixed_register_input(
                                instruction.operands[0].value,
                                target_spec
                                    .integer_argument(1)
                                    .ok_or(VerificationError::OpcodeSignatureMismatch(id))?,
                            )
                        && instruction.operands[1]
                            == MachineOperand::fixed_register_output(
                                instruction.operands[1].value,
                                target_spec.integer_result(),
                            )
                        && self
                            .representations
                            .get(instruction.operands[0].value.0 as usize)
                            == Some(&MachineRepresentation::Tagged)
                        && self
                            .representations
                            .get(instruction.operands[1].value.0 as usize)
                            == Some(&MachineRepresentation::Boolean)
                        && instruction.clobbers.is_empty()
                        && instruction.safepoint.is_none()
                        && instruction.exits.is_empty()
                        && instruction.control == ControlFlow::None
                        && derived_this::probe_cfg_is_valid(
                            self,
                            block_index,
                            id,
                            byte_pc,
                            instruction.operands[1].value,
                        );
                    if !valid {
                        return Err(VerificationError::OpcodeSignatureMismatch(id));
                    }
                }
                if let MachineOpcode::Call(descriptor_index) = instruction.opcode {
                    let Some(descriptor) = self.call_descriptors.get(descriptor_index as usize)
                    else {
                        return Err(VerificationError::InvalidCallDescriptor(
                            id,
                            descriptor_index,
                        ));
                    };
                    let valid_target = match &descriptor.target {
                        CallTarget::RuntimeStub(target)
                            if target.id
                                == otter_vm::native_abi::STUB_JIT_ACKNOWLEDGE_CAUGHT_THROW.id =>
                        {
                            *target == otter_vm::native_abi::STUB_JIT_ACKNOWLEDGE_CAUGHT_THROW
                                && descriptor.arguments.is_empty()
                                && descriptor.results.is_empty()
                                && descriptor.effects == CallEffects::WRITES_HEAP
                                && descriptor.clobbers
                                    == target_spec.clobbers(TargetClobberSet::ScalarCall)
                                && descriptor.exceptional == ExceptionalEdge::None
                                && descriptor.safepoint == SafepointKind::None
                                && instruction.operands.is_empty()
                                && instruction.safepoint.is_none()
                                && instruction.exits.is_empty()
                        }
                        CallTarget::RuntimeStub(_) => true,
                        CallTarget::LiteralAllocation { target, .. } => {
                            let arguments = descriptor.arguments.len();
                            matches!(
                                *target,
                                otter_vm::native_abi::STUB_JIT_NEW_OBJECT
                                    | otter_vm::native_abi::STUB_JIT_NEW_ARRAY
                            ) && (*target != otter_vm::native_abi::STUB_JIT_NEW_OBJECT
                                || arguments == 0)
                                && arguments <= usize::from(u8::MAX) - 2
                                && descriptor
                                    .arguments
                                    .iter()
                                    .all(|rep| *rep == MachineRepresentation::Tagged)
                                && descriptor.results == [MachineRepresentation::Tagged]
                                && descriptor.effects
                                    == CallEffects::READS_HEAP.union(CallEffects::WRITES_HEAP)
                                && descriptor.clobbers
                                    == target_spec.clobbers(TargetClobberSet::ScalarCall)
                                && descriptor.exceptional == ExceptionalEdge::None
                                && descriptor.safepoint == SafepointKind::Gc
                                && instruction.safepoint.is_some()
                                && instruction.exits.is_empty()
                        }
                        CallTarget::NativeLeaf { .. } => {
                            native_leaf::is_valid(target_spec, descriptor, instruction)
                        }
                        CallTarget::CommittedRuntime {
                            target,
                            semantic_arity,
                            ..
                        } => {
                            let semantic_arity = usize::from(*semantic_arity);
                            let named_load =
                                *target == otter_vm::native_abi::STUB_JIT_LOAD_PROPERTY;
                            let named_store =
                                *target == otter_vm::native_abi::STUB_JIT_STORE_PROPERTY;
                            let named_property = named_load || named_store;
                            let element_load =
                                *target == otter_vm::native_abi::STUB_JIT_LOAD_ELEMENT;
                            let element_store =
                                *target == otter_vm::native_abi::STUB_JIT_STORE_ELEMENT;
                            let element = element_load || element_store;
                            let cell_index = if named_store { 2 } else { 1 };
                            let complete_effects = CallEffects::READS_HEAP
                                .union(CallEffects::WRITES_HEAP)
                                .union(CallEffects::INVALIDATES_SHAPES)
                                .union(CallEffects::REENTRANT);
                            let inputs = instruction
                                .operands
                                .iter()
                                .filter(|operand| operand.purpose == OperandPurpose::Input)
                                .collect::<Vec<_>>();
                            let outputs = instruction
                                .operands
                                .iter()
                                .filter(|operand| operand.purpose == OperandPurpose::Output)
                                .collect::<Vec<_>>();
                            let roots = instruction
                                .operands
                                .iter()
                                .filter(|operand| operand.purpose == OperandPurpose::TaggedRoot)
                                .map(|operand| operand.value)
                                .collect::<std::collections::BTreeSet<_>>();
                            let collapses_status = !named_property
                                && !element
                                && descriptor.results == [MachineRepresentation::Tagged]
                                && descriptor.exceptional != ExceptionalEdge::None
                                && *target != otter_vm::native_abi::STUB_JIT_BINDING_VALUE;
                            let exposes_committed_status =
                                is_explicit_committed_runtime_call(descriptor);
                            semantic_arity <= if named_store || element_store { 3 } else { 2 }
                                && (named_property || element || target.signature == otter_vm::native_abi::RuntimeStubSignature::CommittedValue2)
                                && target.argument_count == if named_load {
                                    1
                                } else if named_store {
                                    2
                                } else if element {
                                    u8::try_from(semantic_arity).unwrap_or(u8::MAX)
                                } else {
                                    2
                                }
                                && target.result_abi
                                    == otter_vm::native_abi::RuntimeStubResultAbi::NativePair
                                && target.result_domain
                                    == otter_vm::native_abi::NativeResultDomain::Committed
                                && target.safepoint
                                    == otter_vm::native_abi::RuntimeStubSafepoint::Required
                                && target.exception
                                    == otter_vm::native_abi::RuntimeStubException::Status
                                && descriptor.arguments.len() == semantic_arity
                                && (if named_property { descriptor.arguments.len() == cell_index + 1 && descriptor.arguments[cell_index] == MachineRepresentation::Int64 && descriptor.arguments[..cell_index].iter().all(|r| *r == MachineRepresentation::Tagged) } else { descriptor.arguments.iter().all(|representation| *representation == MachineRepresentation::Tagged) })
                                && (collapses_status || exposes_committed_status)
                                && descriptor.effects == complete_effects
                                && descriptor.clobbers
                                    == target_spec.clobbers(TargetClobberSet::ScalarCall)
                                && descriptor.safepoint == SafepointKind::Gc
                                && instruction.exits.is_empty()
                                && inputs.len() == semantic_arity
                                && (!named_property || self.instructions[..id.0 as usize].iter().any(|producer|
                                    matches!(producer.opcode, MachineOpcode::PropertySource { store, .. } if store == named_store)
                                    && producer.operands.first().is_some_and(|cell| cell.value == inputs[cell_index].value)))
                                && inputs.iter().all(|operand| {
                                    **operand == MachineOperand::location_input(operand.value)
                                        && (roots.contains(&operand.value) || (named_property && operand.value == inputs[cell_index].value && self.representations[operand.value.0 as usize] == MachineRepresentation::Int64))
                                })
                                && outputs.iter().all(|operand| {
                                    operand.role == OperandRole::Definition
                                        && operand.timing == OperandTiming::Late
                                        && matches!(
                                            operand.constraint,
                                            OperandConstraint::Register
                                                | OperandConstraint::Fixed(_)
                                        )
                                })
                        }
                        CallTarget::Direct {
                            kind,
                            candidates,
                            argument_mode,
                            ..
                        } => {
                            let method = *kind == DirectCallKind::Method;
                            let count = candidates.len();
                            let valid_count = if method {
                                !descriptor.arguments.is_empty()
                                    && count <= MAX_MACHINE_DIRECT_METHOD_TARGETS
                            } else if *kind == DirectCallKind::Forward {
                                descriptor.arguments.len() >= 3
                                    && count == 0
                                    && *argument_mode == DirectCallArgumentMode::Fixed
                            } else if *kind == DirectCallKind::CallWithThis {
                                descriptor.arguments.len() >= 2 && count <= 1
                            } else if *kind == DirectCallKind::Construct {
                                count <= 1
                            } else {
                                count == 1
                            };
                            let target_count = u32::try_from(count).ok();
                            valid_count
                                && candidates.iter().enumerate().all(|(index, candidate)| {
                                    candidate.target_index == index as u32
                                        && Some(candidate.target_count) == target_count
                                        && if method {
                                            candidate.guard.as_ref().is_some_and(|guard| {
                                                guard.method_fid
                                                    == candidate.callee.plan.function_id
                                            })
                                        } else {
                                            candidate.guard.is_none()
                                        }
                                })
                        }
                        CallTarget::ColdCallExit { .. } => {
                            descriptor.arguments.is_empty()
                                && descriptor.results == [MachineRepresentation::Tagged]
                                && descriptor.effects == CallEffects::PURE
                                && descriptor.clobbers.is_empty()
                                && descriptor.safepoint == SafepointKind::None
                                && !instruction.exits.is_empty()
                        }
                    };
                    if !valid_target {
                        return Err(VerificationError::InvalidCallTarget(id));
                    }
                    if !state_valid {
                        return Err(VerificationError::InvalidFrameState(
                            id,
                            instruction.frame_state,
                        ));
                    }
                    let inputs = instruction
                        .operands
                        .iter()
                        .filter(|operand| operand.purpose == OperandPurpose::Input)
                        .map(|operand| self.representations[operand.value.0 as usize]);
                    let outputs = instruction
                        .operands
                        .iter()
                        .filter(|operand| operand.purpose == OperandPurpose::Output)
                        .map(|operand| self.representations[operand.value.0 as usize])
                        .collect::<Vec<_>>();
                    if !inputs.eq(descriptor.arguments.iter().copied())
                        || outputs.as_slice() != descriptor.results.as_slice()
                    {
                        return Err(VerificationError::CallSignatureMismatch(id));
                    }
                    if matches!(descriptor.safepoint, SafepointKind::Gc)
                        != instruction.safepoint.is_some()
                    {
                        return Err(VerificationError::CallSafepointMismatch(id));
                    }
                    if let CallTarget::RuntimeStub(target) = descriptor.target {
                        if usize::from(target.argument_count) != descriptor.arguments.len() {
                            return Err(VerificationError::CallSignatureMismatch(id));
                        }
                        if matches!(
                            target.safepoint,
                            otter_vm::native_abi::RuntimeStubSafepoint::Required
                        ) != matches!(descriptor.safepoint, SafepointKind::Gc)
                        {
                            return Err(VerificationError::CallSafepointMismatch(id));
                        }
                    }
                    if instruction.clobbers != descriptor.clobbers {
                        return Err(VerificationError::CallSignatureMismatch(id));
                    }
                    if let ExceptionalEdge::LandingPad(target) = descriptor.exceptional
                        && (target.0 as usize >= self.blocks.len()
                            || !block.successors.contains(&target))
                    {
                        return Err(VerificationError::InvalidExceptionalEdge(id, target));
                    }
                }
                for operand in &instruction.operands {
                    let Some(&representation) = self.representations.get(operand.value.0 as usize)
                    else {
                        return Err(VerificationError::InvalidValue(operand.value));
                    };
                    if let OperandConstraint::Fixed(register) = operand.constraint
                        && register.class() != representation.register_class()
                    {
                        return Err(VerificationError::FixedRegisterClass(id, operand.value));
                    }
                    if operand.purpose.is_metadata()
                        && (operand.role != OperandRole::Use
                            || operand.timing != OperandTiming::Late)
                    {
                        return Err(VerificationError::InvalidMetadataOperand(id, operand.value));
                    }
                    match operand.purpose {
                        OperandPurpose::TaggedRoot | OperandPurpose::RuntimeRoot
                            if representation != MachineRepresentation::Tagged =>
                        {
                            return Err(VerificationError::InvalidRootRepresentation(
                                id,
                                operand.value,
                            ));
                        }
                        OperandPurpose::CellRoot
                            if representation != MachineRepresentation::Cell =>
                        {
                            return Err(VerificationError::InvalidRootRepresentation(
                                id,
                                operand.value,
                            ));
                        }
                        OperandPurpose::TaggedRoot
                        | OperandPurpose::RuntimeRoot
                        | OperandPurpose::CellRoot
                            if instruction.safepoint.is_none() =>
                        {
                            return Err(VerificationError::RootWithoutSafepoint(id));
                        }
                        OperandPurpose::FrameState if instruction.exits.is_empty() => {
                            return Err(VerificationError::DeoptWithoutExit(id));
                        }
                        _ => {}
                    }
                }
            }
        }
        if expected_first as usize != self.instructions.len() {
            return Err(VerificationError::NonContiguousBlocks(MachineBlock(
                self.blocks.len() as u32,
            )));
        }
        for (expected, exit) in deopts.into_iter().enumerate() {
            if exit.0 != expected as u32 {
                return Err(VerificationError::NonDenseExitId {
                    expected: expected as u32,
                    actual: exit.0,
                });
            }
        }
        Ok(())
    }

    pub(super) fn verify(&self, target: &TargetSpec) -> Result<(), VerificationError> {
        self.verify_structure(target)?;
        inline_frames::verify(self)?;
        self.verify_gc_root_liveness()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selectors_do_not_maintain_independent_tagged_root_lists() {
        for (name, source) in [
            ("numeric/mod.rs", include_str!("numeric/mod.rs")),
            ("numeric/arm64.rs", include_str!("numeric/arm64.rs")),
            (
                "numeric/property_cfg.rs",
                include_str!("numeric/property_cfg.rs"),
            ),
            (
                "numeric/inline_reentry.rs",
                include_str!("numeric/inline_reentry.rs"),
            ),
        ] {
            let production = source
                .rsplit_once("#[cfg(test)]\nmod tests")
                .map_or(source, |(production, _)| production);
            assert!(
                !production.contains("MachineOperand::tagged_root"),
                "{name} must leave ordinary root derivation to InstructionSequence completion"
            );
        }
    }

    fn committed_runtime_sequence(semantic_arity: u8) -> InstructionSequence {
        assert!(semantic_arity <= 2);
        let inputs = (0..u32::from(semantic_arity))
            .map(MachineValue)
            .collect::<Vec<_>>();
        let result = MachineValue(u32::from(semantic_arity));
        let mut instructions = inputs
            .iter()
            .enumerate()
            .map(|(index, &value)| {
                MachineInstruction::plain(
                    MachineOpcode::EntryValue(index as u16),
                    vec![MachineOperand::register_output(value)],
                )
            })
            .collect::<Vec<_>>();
        let mut operands = inputs
            .iter()
            .copied()
            .map(MachineOperand::location_input)
            .collect::<Vec<_>>();
        operands.push(MachineOperand::register_output(result));
        operands.extend(inputs.iter().copied().map(MachineOperand::tagged_root));
        let mut call = MachineInstruction::plain(MachineOpcode::Call(0), operands);
        call.clobbers = TargetSpec::aarch64()
            .clobbers(TargetClobberSet::ScalarCall)
            .to_vec();
        call.safepoint = Some(SafepointId(0));
        call.frame_state = Some(0);
        call.frame_state = Some(0);
        call.frame_state = Some(0);
        instructions.push(call);
        let mut ret = MachineInstruction::plain(
            MachineOpcode::Return,
            vec![MachineOperand::register_input(result)],
        );
        ret.control = ControlFlow::Return;
        instructions.push(ret);
        let complete_effects = CallEffects::READS_HEAP
            .union(CallEffects::WRITES_HEAP)
            .union(CallEffects::INVALIDATES_SHAPES)
            .union(CallEffects::REENTRANT);
        let instruction_count = instructions.len() as u32;

        InstructionSequence::new_with_frame_states(
            &TargetSpec::aarch64(),
            MachineBlock(0),
            vec![MachineRepresentation::Tagged; usize::from(semantic_arity) + 1],
            vec![CallDescriptor {
                target: CallTarget::CommittedRuntime {
                    target: otter_vm::native_abi::STUB_JIT_SCALAR_VALUE,
                    logical_pc: 19,
                    byte_pc: 41,
                    semantic_arity,
                },
                arguments: vec![MachineRepresentation::Tagged; usize::from(semantic_arity)],
                results: vec![MachineRepresentation::Tagged],
                effects: complete_effects,
                clobbers: TargetSpec::aarch64()
                    .clobbers(TargetClobberSet::ScalarCall)
                    .to_vec(),
                exceptional: ExceptionalEdge::Propagate,
                safepoint: SafepointKind::Gc,
            }],
            vec![MachineFrameState {
                id: 0,
                frames: Box::new([otter_vm::deopt::DeoptFrame {
                    function_id: 0,
                    byte_pc: 41,
                    entry: None,
                    slots: inputs
                        .iter()
                        .copied()
                        .map(MachineFrameSlot::Value)
                        .collect(),
                }]),
            }],
            vec![MachineBlockData {
                first: MachineInstructionId(0),
                end: MachineInstructionId(instruction_count),
                predecessors: Vec::new(),
                successors: Vec::new(),
                parameters: Vec::new(),
                successor_arguments: Vec::new(),
            }],
            instructions,
        )
        .expect("valid committed-runtime sequence")
    }

    #[test]
    fn verifier_rejects_direct_method_without_receiver_argument() {
        let result = MachineValue(0);
        let descriptor = CallDescriptor {
            target: CallTarget::Direct {
                kind: DirectCallKind::Method,
                argument_mode: DirectCallArgumentMode::Fixed,
                candidates: Vec::new(),
                caller_function_id: 0,
                logical_pc: 0,
                byte_pc: 0,
            },
            arguments: Vec::new(),
            results: vec![MachineRepresentation::Tagged],
            effects: CallEffects::REENTRANT,
            clobbers: Vec::new(),
            exceptional: ExceptionalEdge::Propagate,
            safepoint: SafepointKind::None,
        };
        let call = MachineInstruction::plain(
            MachineOpcode::Call(0),
            vec![MachineOperand::register_output(result)],
        );
        let mut ret = MachineInstruction::plain(
            MachineOpcode::Return,
            vec![MachineOperand::register_input(result)],
        );
        ret.control = ControlFlow::Return;

        assert_eq!(
            InstructionSequence::new(
                &TargetSpec::aarch64(),
                MachineBlock(0),
                vec![MachineRepresentation::Tagged],
                vec![descriptor],
                vec![MachineBlockData {
                    first: MachineInstructionId(0),
                    end: MachineInstructionId(2),
                    predecessors: Vec::new(),
                    successors: Vec::new(),
                    parameters: Vec::new(),
                    successor_arguments: Vec::new(),
                }],
                vec![call, ret],
            ),
            Err(VerificationError::InvalidCallTarget(MachineInstructionId(
                0
            )))
        );
    }

    #[test]
    fn verifier_accepts_zero_to_two_true_committed_runtime_inputs() {
        for semantic_arity in 0..=2 {
            let sequence = committed_runtime_sequence(semantic_arity);
            assert!(sequence.verify(&TargetSpec::aarch64()).is_ok());
            assert!(
                sequence
                    .normalized()
                    .contains(&format!("semantic_arity: {semantic_arity}"))
            );
        }
    }

    #[test]
    fn zero_arity_committed_runtime_keeps_unrelated_live_tagged_roots() {
        let unrelated = MachineValue(0);
        let result = MachineValue(1);
        let mut call = MachineInstruction::plain(
            MachineOpcode::Call(0),
            vec![
                MachineOperand::register_output(result),
                MachineOperand::tagged_root(unrelated),
            ],
        );
        call.clobbers = TargetSpec::aarch64()
            .clobbers(TargetClobberSet::ScalarCall)
            .to_vec();
        call.safepoint = Some(SafepointId(0));
        call.frame_state = Some(0);
        let mut ret = MachineInstruction::plain(
            MachineOpcode::Return,
            vec![MachineOperand::register_input(unrelated)],
        );
        ret.control = ControlFlow::Return;
        let descriptor = committed_runtime_sequence(0).call_descriptors[0].clone();
        let sequence = InstructionSequence::new_with_frame_states(
            &TargetSpec::aarch64(),
            MachineBlock(0),
            vec![MachineRepresentation::Tagged; 2],
            vec![descriptor],
            vec![MachineFrameState {
                id: 0,
                frames: Box::new([otter_vm::deopt::DeoptFrame {
                    function_id: 0,
                    byte_pc: 41,
                    entry: None,
                    slots: Box::new([MachineFrameSlot::Value(unrelated)]),
                }]),
            }],
            vec![MachineBlockData {
                first: MachineInstructionId(0),
                end: MachineInstructionId(3),
                predecessors: Vec::new(),
                successors: Vec::new(),
                parameters: Vec::new(),
                successor_arguments: Vec::new(),
            }],
            vec![
                MachineInstruction::plain(
                    MachineOpcode::EntryValue(0),
                    vec![MachineOperand::register_output(unrelated)],
                ),
                call,
                ret,
            ],
        )
        .expect("zero-arity committed call with unrelated root");
        let allocation = sequence
            .allocate(&TargetSpec::aarch64())
            .expect("zero-arity committed allocation");
        let safepoints =
            lower_safepoints(&sequence, &allocation).expect("zero-arity committed safepoints");
        assert_eq!(
            safepoints
                .site(MachineInstructionId(1))
                .expect("zero-arity committed site")
                .roots
                .iter()
                .map(|root| root.value)
                .collect::<Vec<_>>(),
            [unrelated]
        );

        let mut duplicate = sequence.clone();
        duplicate.instructions[1]
            .operands
            .push(MachineOperand::tagged_root(unrelated));
        assert_eq!(
            duplicate.verify(&TargetSpec::aarch64()),
            Err(VerificationError::DuplicateTaggedRoot(
                MachineInstructionId(1),
                unrelated,
            ))
        );

        let mut missing = sequence;
        missing.instructions[1]
            .operands
            .retain(|operand| operand.purpose != OperandPurpose::TaggedRoot);
        assert_eq!(
            missing.verify(&TargetSpec::aarch64()),
            Err(VerificationError::MissingLiveTaggedRoot(
                MachineInstructionId(1),
                unrelated,
            ))
        );
    }

    #[test]
    fn completed_selection_derives_only_the_unrelated_live_tagged_root() {
        let unrelated = MachineValue(0);
        let result = MachineValue(1);
        let mut call = MachineInstruction::plain(
            MachineOpcode::Call(0),
            vec![MachineOperand::register_output(result)],
        );
        call.clobbers = TargetSpec::aarch64()
            .clobbers(TargetClobberSet::ScalarCall)
            .to_vec();
        call.safepoint = Some(SafepointId(0));
        call.frame_state = Some(0);
        let mut ret = MachineInstruction::plain(
            MachineOpcode::Return,
            vec![MachineOperand::register_input(unrelated)],
        );
        ret.control = ControlFlow::Return;

        let sequence = InstructionSequence::new_selected(
            &TargetSpec::aarch64(),
            MachineBlock(0),
            vec![MachineRepresentation::Tagged; 2],
            vec![committed_runtime_sequence(0).call_descriptors[0].clone()],
            vec![MachineFrameState {
                id: 0,
                frames: Box::new([otter_vm::deopt::DeoptFrame {
                    function_id: 0,
                    byte_pc: 41,
                    entry: None,
                    slots: Box::new([MachineFrameSlot::Value(unrelated)]),
                }]),
            }],
            vec![MachineBlockData {
                first: MachineInstructionId(0),
                end: MachineInstructionId(3),
                predecessors: Vec::new(),
                successors: Vec::new(),
                parameters: Vec::new(),
                successor_arguments: Vec::new(),
            }],
            vec![
                MachineInstruction::plain(
                    MachineOpcode::EntryValue(0),
                    vec![MachineOperand::register_output(unrelated)],
                ),
                call,
                ret,
            ],
        )
        .expect("completed selection roots");

        assert_eq!(
            sequence.instructions[1]
                .operands
                .iter()
                .filter(|operand| operand.purpose == OperandPurpose::TaggedRoot)
                .map(|operand| operand.value)
                .collect::<Vec<_>>(),
            [unrelated]
        );
        sequence
            .verify(&TargetSpec::aarch64())
            .expect("completed roots remain verifiable");
    }

    #[test]
    fn verifier_rejects_probe_or_replay_contract_on_committed_runtime_call() {
        let call_id = MachineInstructionId(2);
        let mut wrong_signature = committed_runtime_sequence(2);
        let CallTarget::CommittedRuntime { target, .. } =
            &mut wrong_signature.call_descriptors[0].target
        else {
            panic!("committed target fixture")
        };
        *target = otter_vm::native_abi::STUB_JIT_LOAD_ELEMENT;
        assert_eq!(
            wrong_signature.verify(&TargetSpec::aarch64()),
            Err(VerificationError::InvalidCallTarget(call_id))
        );

        let mut hidden_binding_status = committed_runtime_sequence(2);
        let CallTarget::CommittedRuntime { target, .. } =
            &mut hidden_binding_status.call_descriptors[0].target
        else {
            panic!("committed target fixture")
        };
        *target = otter_vm::native_abi::STUB_JIT_BINDING_VALUE;
        assert_eq!(
            hidden_binding_status.verify(&TargetSpec::aarch64()),
            Err(VerificationError::InvalidCallTarget(call_id)),
            "binding status must remain explicit Machine SSA"
        );

        let mut too_wide = committed_runtime_sequence(2);
        let CallTarget::CommittedRuntime { semantic_arity, .. } =
            &mut too_wide.call_descriptors[0].target
        else {
            panic!("committed target fixture")
        };
        *semantic_arity = 3;
        assert_eq!(
            too_wide.verify(&TargetSpec::aarch64()),
            Err(VerificationError::InvalidCallTarget(call_id))
        );

        let mut missing_root = committed_runtime_sequence(2);
        missing_root.instructions[call_id.0 as usize]
            .operands
            .retain(|operand| operand != &MachineOperand::tagged_root(MachineValue(1)));
        assert_eq!(
            missing_root.verify(&TargetSpec::aarch64()),
            Err(VerificationError::InvalidCallTarget(call_id))
        );

        let mut local_replay = committed_runtime_sequence(2);
        local_replay.instructions[call_id.0 as usize].set_test_exit(DeoptId(0), 0);
        assert_eq!(
            local_replay.verify(&TargetSpec::aarch64()),
            Err(VerificationError::InvalidCallTarget(call_id))
        );

        let mut no_throw_edge = committed_runtime_sequence(2);
        no_throw_edge.call_descriptors[0].exceptional = ExceptionalEdge::None;
        assert_eq!(
            no_throw_edge.verify(&TargetSpec::aarch64()),
            Err(VerificationError::InvalidCallTarget(call_id))
        );
    }

    fn checked_instruction_sequence(
        result_representation: MachineRepresentation,
        mut checked: MachineInstruction,
    ) -> InstructionSequence {
        let input = MachineValue(0);
        let result = MachineValue(1);
        checked.set_test_exit(DeoptId(0), 0);
        let mut ret = MachineInstruction::plain(
            MachineOpcode::Return,
            vec![MachineOperand::register_input(result)],
        );
        ret.control = ControlFlow::Return;
        InstructionSequence::new_with_frame_states(
            &TargetSpec::aarch64(),
            MachineBlock(0),
            vec![MachineRepresentation::Tagged, result_representation],
            Vec::new(),
            vec![MachineFrameState {
                id: 0,
                frames: Box::new([otter_vm::deopt::DeoptFrame {
                    function_id: 0,
                    byte_pc: 0,
                    entry: None,
                    slots: Box::new([MachineFrameSlot::Value(input)]),
                }]),
            }],
            vec![MachineBlockData {
                first: MachineInstructionId(0),
                end: MachineInstructionId(3),
                predecessors: Vec::new(),
                successors: Vec::new(),
                parameters: Vec::new(),
                successor_arguments: Vec::new(),
            }],
            vec![
                MachineInstruction::plain(
                    MachineOpcode::EntryValue(0),
                    vec![MachineOperand::register_output(input)],
                ),
                checked,
                ret,
            ],
        )
        .expect("valid checked-opcode sequence")
    }

    #[test]
    fn verifier_requires_nullish_input_in_exact_deopt_metadata() {
        let input = MachineValue(0);
        let result = MachineValue(1);
        let mut compare = MachineInstruction::plain(
            MachineOpcode::TaggedNullishEqual {
                byte_pc: 8,
                equal: true,
            },
            vec![
                MachineOperand::register_input(input),
                MachineOperand::register_output(result),
                MachineOperand::frame_value(input),
            ],
        );
        compare.clobbers = TargetSpec::aarch64()
            .clobbers(TargetClobberSet::StatusScratch)
            .to_vec();
        let mut sequence = checked_instruction_sequence(MachineRepresentation::Boolean, compare);
        sequence.instructions[1].operands.pop();
        assert_eq!(
            sequence.verify(&TargetSpec::aarch64()),
            Err(VerificationError::OpcodeSignatureMismatch(
                MachineInstructionId(1)
            ))
        );
    }
}
