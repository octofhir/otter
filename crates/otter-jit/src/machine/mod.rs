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
//! - [`MachineOpcode`] — scalar operations, guarded element accesses, control
//!   flow, and descriptor-backed calls.
//! - [`CallDescriptor`], [`DirectCallCandidate`], and [`DirectCallKind`] —
//!   complete semantic target chains, guards, ABI, effects, and
//!   normal/exceptional exits.
//! - [`TargetRegisterFile`] — complete allocatable target register inventory.
//! - [`AllocatedSequence`] — allocator edits, per-operand locations, and exact
//!   safepoint/deoptimization locations.
//! - [`MachineFrameLayout`] — aligned post-allocation spill-frame contract.
//! - [`MachineSafepointTable`] — allocator roots and VM spill-slot records.
//!
//! # Invariants
//! - Virtual values are dense and have one machine representation.
//! - Every block owns a non-empty contiguous instruction range ending in one
//!   branch or return instruction.
//! - Metadata values are ordinary late uses. Calls therefore cannot leave a
//!   live GC/deopt value in a clobbered register.
//! - Direct methods own one complete dense one-to-four-candidate chain; plain
//!   and constructor targets remain monomorphic. A cold call exit owns no
//!   inputs, effects, clobbers, roots, or safepoint and must carry an exact
//!   pre-call deoptimization state.
//! - Root and deopt maps are built from the same per-operand allocation table
//!   consumed by the emitter; there is no pre-allocation location fallback.
//! - OSR sources are immutable entry metadata aligned with ordinary late-use
//!   operands; their target locations come from that same allocation table.
//! - Guarded element operands are late location uses, so target emission may
//!   materialize stack or register homes without overwriting a live input.
//! - Prepared global reads define exactly one tagged register value, carry one
//!   exact pre-operation deopt state, and neither allocate nor own a safepoint.
//! - Tagged nullish loose equality deoptimizes before its Boolean definition
//!   for every non-nullish cell so canonical HTMLDDA semantics remain visible.
//! - Target register files enumerate physical registers explicitly. There is
//!   no synthetic constant register budget.
//!
//! # See also
//! - [`crate::optimizing`] — current semantic lowering being replaced.

mod deopt;
mod frame;
#[cfg(target_arch = "aarch64")]
pub(crate) mod numeric;
mod regalloc;
mod safepoint;
mod target;

pub use deopt::{
    MachineDeoptError, MachineFrameSlot, MachineFrameState, lower_deopt_table, undefined_slot,
};
pub use frame::{FrameLayoutError, MachineFrameLayout};
pub use regalloc::{
    AllocatedLocation, AllocatedMetadata, AllocatedSequence, AllocationEdit, AllocationError,
    AllocationPoint,
};
pub use safepoint::{
    MachineSafepointError, MachineSafepointRoot, MachineSafepointSite, MachineSafepointTable,
    lower_safepoints,
};
pub use target::{PhysicalRegister, TargetArchitecture, TargetRegisterFile};

use std::fmt::Write as _;

/// Dense identity of a target-selected virtual value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MachineValue(pub u32);

/// Maximum number of loop-scoped packed-double view caches in one body.
///
/// Each cache owns two untraced native-stack words, so the bound also caps
/// persistent raw frame growth at 512 bytes on 64-bit targets.
pub const MAX_PACKED_DOUBLE_VIEW_CACHES: usize = 32;

/// Number of untraced native-stack words owned by one packed-double view
/// cache: the non-null element base followed by the live dense length.
pub const PACKED_DOUBLE_VIEW_CACHE_RAW_WORDS: usize = 2;

/// Dense identity of one loop-scoped packed-double element-view cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PackedDoubleViewCacheId(u8);

impl PackedDoubleViewCacheId {
    /// Construct a bounded cache identity.
    #[must_use]
    pub const fn new(index: usize) -> Option<Self> {
        if index < MAX_PACKED_DOUBLE_VIEW_CACHES {
            Some(Self(index as u8))
        } else {
            None
        }
    }

    /// Zero-based cache index.
    #[must_use]
    pub const fn index(self) -> usize {
        self.0 as usize
    }

    /// First raw frame word owned by this cache.
    #[must_use]
    pub const fn raw_word(self) -> usize {
        self.index() * PACKED_DOUBLE_VIEW_CACHE_RAW_WORDS
    }
}

/// Boundary that invalidates every persistent packed-double view cache.
///
/// The target emitter consumes this semantic reason when placing zeroing
/// operations. Cached words contain raw host addresses rather than GC roots
/// and may survive only across a generated loop backedge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackedDoubleViewCacheClearReason {
    /// Ordinary function activation before any generated instruction.
    FunctionEntry,
    /// Interpreter-to-native loop-header entry.
    OsrEntry,
    /// Non-backedge control entering the owning natural-loop header.
    LoopEntry,
    /// Generated control leaves through a cold or exact-deoptimization path.
    ColdExit,
    /// Generated control may allocate, collect, or invoke JavaScript.
    Reentry,
}

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
            | Self::Int64 => regalloc2::RegClass::Int,
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
    /// Cell GC root at the instruction's safepoint.
    CellRoot,
    /// Value required by deoptimization reconstruction.
    Deopt,
}

impl OperandPurpose {
    const fn is_metadata(self) -> bool {
        matches!(self, Self::TaggedRoot | Self::CellRoot | Self::Deopt)
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

    /// Keep a value available for deoptimization at this instruction.
    #[must_use]
    pub const fn deopt(value: MachineValue) -> Self {
        Self {
            value,
            constraint: OperandConstraint::Any,
            role: OperandRole::Use,
            timing: OperandTiming::Late,
            purpose: OperandPurpose::Deopt,
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

/// How a generated call obtains the callee's declared parameter prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirectCallArgumentMode {
    /// Each argument is an explicit machine operand.
    Fixed,
    /// One machine operand names the compiler-created dense argument array.
    Spread,
}

/// Semantic destination selected before target emission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallTarget {
    /// VM-owned runtime entry with one statically declared ABI.
    RuntimeStub(otter_vm::native_abi::RuntimeStubDescriptor),
    /// VM-planned JavaScript callee chain entered through generated stack-owned
    /// linkage. Plain and constructor calls remain monomorphic; guarded methods
    /// may carry one complete dense bounded chain.
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

/// Complete target-neutral call contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallDescriptor {
    /// Semantic entry selected above target emission.
    pub target: CallTarget,
    /// Ordered argument representations.
    pub arguments: Vec<MachineRepresentation>,
    /// Result representation, or no result.
    pub result: Option<MachineRepresentation>,
    /// Memory, shape, and reentrancy effects.
    pub effects: CallEffects,
    /// Physical registers destroyed by the selected target ABI.
    pub clobbers: Vec<PhysicalRegister>,
    /// Exceptional control transfer.
    pub exceptional: ExceptionalEdge,
    /// GC interaction.
    pub safepoint: SafepointKind,
}

/// Target-neutral name of a selected machine operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MachineOpcode {
    /// Materialize an incoming ABI value.
    EntryValue(u16),
    /// Materialize the current frame's tagged `this` binding.
    EntryThis,
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
    /// Prove that one Float64 is an exact unsigned dense-element index.
    ///
    /// The conversion deoptimizes at the owning element operation for a
    /// fraction, non-finite value, negative value, or value outside Uint32.
    CheckedFloat64ToElementIndex(u32),
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
    /// Invert canonical Boolean bits.
    BooleanNot,
    /// Compare one tagged value with a statically known `null` or `undefined`.
    /// Non-nullish cells deopt before defining the result so the canonical
    /// equality path can observe HTMLDDA objects.
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
    /// Read one captured binding from the current native frame's stable cell
    /// spine, deoptimizing at the source operation on a TDZ or invalid layout.
    LoadUpvalue {
        /// Zero-based cell handle in the upvalue spine.
        index: i32,
        /// Source bytecode offset used by artifacts and exact deoptimization.
        byte_pc: u32,
    },
    /// Read one VM-baked permanent global-declarative cell, deoptimizing at
    /// the source operation if the live value is still in TDZ.
    GlobalLexicalLoad {
        /// Source bytecode offset used by artifacts and exact deoptimization.
        byte_pc: u32,
        /// Stable GC-cage cell identity copied from the compile snapshot.
        target: otter_vm::jit::JitGlobalLexicalLoad,
    },
    /// Guard the live global-declarative epoch, global-object shape, and own
    /// data slot before reading one VM-baked global-object property.
    GlobalObjectLoad {
        /// Source bytecode offset used by artifacts and exact deoptimization.
        byte_pc: u32,
        /// Complete epoch, shape, dictionary, and slot guard program.
        target: otter_vm::jit::JitGlobalObjectLoad,
    },
    /// Guard and load one VM-baked indexed element, deoptimizing on any miss.
    ElementLoad(u32),
    /// Guard and store one VM-baked indexed element, deoptimizing before the
    /// first effect on any miss.
    ElementStore(u32),
    /// Guard and directly load one ordinary Array PackedDouble element.
    PackedDoubleElementLoad {
        /// Source bytecode offset used by artifacts and exact deoptimization.
        byte_pc: u32,
        /// Optional loop-scoped raw view cache.
        cache: Option<PackedDoubleViewCacheId>,
    },
    /// Guard and directly store one ordinary Array PackedDouble element.
    PackedDoubleElementStore {
        /// Source bytecode offset used by artifacts and exact deoptimization.
        byte_pc: u32,
        /// Optional loop-scoped raw view cache.
        cache: Option<PackedDoubleViewCacheId>,
    },
    /// Clear every persistent packed-double view word at one semantic boundary.
    ClearPackedDoubleViewCaches(PackedDoubleViewCacheClearReason),
    /// Guard and load one settled own-data property or exotic array/string
    /// length, deoptimizing at the source operation before effects on a miss.
    PropertyLoad {
        /// Source bytecode offset used for settled metadata and artifacts.
        byte_pc: u32,
        /// Whether the property name is `length` and should first try the
        /// dense-array/primitive-string layout program.
        exotic_length: bool,
    },
    /// Guard and store one settled existing own-data property, deoptimizing
    /// before the first effect on any metadata, receiver, shape, or slot miss.
    PropertyStore {
        /// Source bytecode offset used for settled metadata and artifacts.
        byte_pc: u32,
        /// Whether scalar typing proves the boxed value cannot be a GC cell,
        /// allowing emission to omit the conditional generational barrier.
        value_is_non_cell: bool,
    },
    /// Apply one VM-baked constructor-owned add-property transition.
    ConstructorFieldStore(u32),
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
    /// Unconditional control transfer.
    Jump,
    /// Conditional control transfer; branch when the integer condition equals
    /// the encoded polarity.
    BranchIf(bool),
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
    /// Deopt exit described by metadata operands, when present.
    pub deopt: Option<DeoptId>,
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
            deopt: None,
            control: ControlFlow::None,
        }
    }
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
    blocks: Vec<MachineBlockData>,
    instructions: Vec<MachineInstruction>,
    packed_double_view_cache_count: u8,
}

/// Structural failure in a target-selected instruction sequence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerificationError {
    /// The sequence requests more persistent raw view caches than the frame
    /// contract permits.
    TooManyPackedDoubleViewCaches(u8),
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
    /// A packed-double operation references a cache outside this sequence.
    InvalidPackedDoubleViewCache(MachineInstructionId, PackedDoubleViewCacheId),
    /// Metadata operands must be late uses.
    InvalidMetadataOperand(MachineInstructionId, MachineValue),
    /// Root metadata does not match the value representation.
    InvalidRootRepresentation(MachineInstructionId, MachineValue),
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
        entry: MachineBlock,
        representations: Vec<MachineRepresentation>,
        call_descriptors: Vec<CallDescriptor>,
        blocks: Vec<MachineBlockData>,
        instructions: Vec<MachineInstruction>,
    ) -> Result<Self, VerificationError> {
        Self::new_with_packed_double_view_caches(
            entry,
            representations,
            call_descriptors,
            blocks,
            instructions,
            0,
        )
    }

    /// Construct and verify a sequence with bounded raw packed-double caches.
    pub fn new_with_packed_double_view_caches(
        entry: MachineBlock,
        representations: Vec<MachineRepresentation>,
        call_descriptors: Vec<CallDescriptor>,
        blocks: Vec<MachineBlockData>,
        instructions: Vec<MachineInstruction>,
        packed_double_view_cache_count: u8,
    ) -> Result<Self, VerificationError> {
        let sequence = Self {
            entry,
            representations,
            call_descriptors,
            blocks,
            instructions,
            packed_double_view_cache_count,
        };
        sequence.verify()?;
        Ok(sequence)
    }

    /// Allocate through regalloc2's Ion allocator and finalize exact metadata.
    pub fn allocate(
        &self,
        target: &TargetRegisterFile,
    ) -> Result<AllocatedSequence, AllocationError> {
        regalloc::allocate(self, target)
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

    /// Number of two-word raw packed-double view caches owned by the frame.
    #[must_use]
    pub const fn packed_double_view_cache_count(&self) -> u8 {
        self.packed_double_view_cache_count
    }

    /// Deterministic identity excluding source/module/function identities.
    #[must_use]
    pub fn normalized(&self) -> String {
        let mut output = format!(
            "machine-ir packed-double-view-caches={}\n",
            self.packed_double_view_cache_count
        );
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
                    "  i{instruction_index} {:?} {:?} clobbers={:?} sp={:?} deopt={:?}",
                    instruction.opcode,
                    instruction.operands,
                    instruction.clobbers,
                    instruction.safepoint,
                    instruction.deopt
                )
                .expect("writing to String cannot fail");
            }
        }
        output
    }

    /// Recover the VM-semantic value behind selection-only exact conversions.
    ///
    /// Packed element instructions consume checked indices and widened Number
    /// temporaries that intentionally do not appear in the pre-operation VM
    /// state. Their source does. Parameter-entry decodes are not unwrapped:
    /// that HIR parameter itself is the deopt value and its guard has no deopt
    /// identity. This bounded walk lets the verifier prove that every semantic
    /// operand, rather than merely some metadata, is reconstructible.
    fn semantic_deopt_source_before(
        &self,
        before: MachineInstructionId,
        mut value: MachineValue,
    ) -> MachineValue {
        for _ in 0..self.representations.len() {
            let Some(definition) =
                self.instructions[..before.0 as usize]
                    .iter()
                    .rev()
                    .find(|instruction| {
                        instruction.operands.iter().any(|operand| {
                            operand.value == value
                                && operand.role == OperandRole::Definition
                                && operand.purpose == OperandPurpose::Output
                        })
                    })
            else {
                break;
            };
            let unwrap = matches!(
                definition.opcode,
                MachineOpcode::CheckedFloat64ToElementIndex(..)
                    | MachineOpcode::Int32ToFloat64
                    | MachineOpcode::Uint32ToFloat64
            ) || (definition.opcode == MachineOpcode::DecodeNumber
                && definition.deopt.is_some());
            if !unwrap {
                break;
            }
            let Some(source) = definition.operands.iter().find(|operand| {
                operand.role == OperandRole::Use && operand.purpose == OperandPurpose::Input
            }) else {
                break;
            };
            if source.value == value {
                break;
            }
            value = source.value;
        }
        value
    }

    pub(super) fn verify(&self) -> Result<(), VerificationError> {
        if usize::from(self.packed_double_view_cache_count) > MAX_PACKED_DOUBLE_VIEW_CACHES {
            return Err(VerificationError::TooManyPackedDoubleViewCaches(
                self.packed_double_view_cache_count,
            ));
        }
        if self.entry.0 as usize >= self.blocks.len() {
            return Err(VerificationError::InvalidEntry);
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
                        MachineOpcode::Return if !block.successors.is_empty() => {
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
                if let Some(deopt) = instruction.deopt
                    && !deopts.insert(deopt)
                {
                    return Err(VerificationError::DuplicateDeopt(deopt));
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
                if matches!(
                    instruction.opcode,
                    MachineOpcode::GlobalLexicalLoad { .. }
                        | MachineOpcode::GlobalObjectLoad { .. }
                ) {
                    let Some((output, metadata)) = instruction.operands.split_first() else {
                        return Err(VerificationError::OpcodeSignatureMismatch(id));
                    };
                    let output_is_tagged_register = *output
                        == MachineOperand::register_output(output.value)
                        && self.representations[output.value.0 as usize]
                            == MachineRepresentation::Tagged;
                    let mut deopt_values = std::collections::BTreeSet::new();
                    let metadata_is_exact_deopt = metadata.iter().all(|operand| {
                        *operand == MachineOperand::deopt(operand.value)
                            && operand.value != output.value
                            && deopt_values.insert(operand.value)
                    });
                    let clobbers_are_exact = match &instruction.opcode {
                        MachineOpcode::GlobalLexicalLoad { .. } => {
                            instruction.clobbers
                                == [
                                    PhysicalRegister::integer(9),
                                    PhysicalRegister::integer(11),
                                    PhysicalRegister::integer(13),
                                ]
                        }
                        MachineOpcode::GlobalObjectLoad { .. } => {
                            instruction.clobbers
                                == [9, 11, 12, 13, 14, 15]
                                    .map(PhysicalRegister::integer)
                                    .as_slice()
                        }
                        _ => false,
                    };
                    if !output_is_tagged_register
                        || !metadata_is_exact_deopt
                        || !clobbers_are_exact
                        || instruction.deopt.is_none()
                        || instruction.safepoint.is_some()
                    {
                        return Err(VerificationError::OpcodeSignatureMismatch(id));
                    }
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
                        *operand == MachineOperand::deopt(operand.value)
                            && operand.value != output.value
                            && deopt_values.insert(operand.value)
                    });
                    if !ordinary_signature
                        || !metadata_is_exact_deopt
                        || !deopt_values.contains(&input.value)
                        || instruction.deopt.is_none()
                        || instruction.safepoint.is_some()
                        || instruction.clobbers != [PhysicalRegister::integer(16)]
                    {
                        return Err(VerificationError::OpcodeSignatureMismatch(id));
                    }
                }
                if matches!(
                    instruction.opcode,
                    MachineOpcode::PropertyLoad { .. } | MachineOpcode::PropertyStore { .. }
                ) {
                    let Some((ordinary, metadata)) = instruction.operands.split_at_checked(2)
                    else {
                        return Err(VerificationError::OpcodeSignatureMismatch(id));
                    };
                    let receiver = ordinary[0];
                    let receiver_is_tagged_location = receiver
                        == MachineOperand::location_input(receiver.value)
                        && self.representations[receiver.value.0 as usize]
                            == MachineRepresentation::Tagged;
                    let ordinary_signature = match &instruction.opcode {
                        MachineOpcode::PropertyLoad { .. } => {
                            let output = ordinary[1];
                            output == MachineOperand::register_output(output.value)
                                && self.representations[output.value.0 as usize]
                                    == MachineRepresentation::Tagged
                        }
                        MachineOpcode::PropertyStore { .. } => {
                            let value = ordinary[1];
                            value == MachineOperand::location_input(value.value)
                                && self.representations[value.value.0 as usize]
                                    == MachineRepresentation::Tagged
                        }
                        _ => false,
                    };
                    let metadata_shape = metadata.iter().all(|operand| {
                        *operand == MachineOperand::tagged_root(operand.value)
                            || *operand == MachineOperand::deopt(operand.value)
                    });
                    let root_values = metadata
                        .iter()
                        .filter(|operand| operand.purpose == OperandPurpose::TaggedRoot)
                        .map(|operand| operand.value)
                        .collect::<std::collections::BTreeSet<_>>();
                    let required_roots = match &instruction.opcode {
                        MachineOpcode::PropertyLoad { .. } => root_values.contains(&receiver.value),
                        MachineOpcode::PropertyStore { .. } => {
                            root_values.contains(&receiver.value)
                                && root_values.contains(&ordinary[1].value)
                        }
                        _ => false,
                    };
                    if !receiver_is_tagged_location
                        || !ordinary_signature
                        || !metadata_shape
                        || !required_roots
                        || instruction.clobbers
                            != TargetRegisterFile::aarch64_scalar_call_clobbers()
                        || instruction.deopt.is_none()
                        || instruction.safepoint.is_none()
                        || instruction.control != ControlFlow::None
                    {
                        return Err(VerificationError::OpcodeSignatureMismatch(id));
                    }
                }
                if matches!(
                    instruction.opcode,
                    MachineOpcode::ClearPackedDoubleViewCaches(..)
                ) && (!instruction.operands.is_empty()
                    || !instruction.clobbers.is_empty()
                    || instruction.safepoint.is_some()
                    || instruction.deopt.is_some()
                    || instruction.control != ControlFlow::None)
                {
                    return Err(VerificationError::OpcodeSignatureMismatch(id));
                }
                if matches!(
                    instruction.opcode,
                    MachineOpcode::CheckedFloat64ToElementIndex(..)
                ) {
                    let Some((ordinary, metadata)) = instruction.operands.split_at_checked(2)
                    else {
                        return Err(VerificationError::OpcodeSignatureMismatch(id));
                    };
                    let input = ordinary[0];
                    let output = ordinary[1];
                    let ordinary_signature = input == MachineOperand::register_input(input.value)
                        && self.representations[input.value.0 as usize]
                            == MachineRepresentation::Float64
                        && output == MachineOperand::register_output(output.value)
                        && self.representations[output.value.0 as usize]
                            == MachineRepresentation::Uint32;
                    let mut deopt_values = std::collections::BTreeSet::new();
                    let metadata_is_exact_deopt = metadata.iter().all(|operand| {
                        *operand == MachineOperand::deopt(operand.value)
                            && operand.value != output.value
                            && deopt_values.insert(operand.value)
                    });
                    if !ordinary_signature
                        || !metadata_is_exact_deopt
                        || !deopt_values.contains(&input.value)
                        || instruction.deopt.is_none()
                        || instruction.safepoint.is_some()
                        || instruction.clobbers
                            != [PhysicalRegister::integer(16), PhysicalRegister::float(31)]
                    {
                        return Err(VerificationError::OpcodeSignatureMismatch(id));
                    }
                }
                if matches!(
                    instruction.opcode,
                    MachineOpcode::PackedDoubleElementLoad { .. }
                        | MachineOpcode::PackedDoubleElementStore { .. }
                ) {
                    let cache = match instruction.opcode {
                        MachineOpcode::PackedDoubleElementLoad { cache, .. }
                        | MachineOpcode::PackedDoubleElementStore { cache, .. } => cache,
                        _ => unreachable!("matched packed-double operation"),
                    };
                    if let Some(cache) = cache
                        && cache.index() >= usize::from(self.packed_double_view_cache_count)
                    {
                        return Err(VerificationError::InvalidPackedDoubleViewCache(id, cache));
                    }
                    let Some((ordinary, metadata)) = instruction.operands.split_at_checked(3)
                    else {
                        return Err(VerificationError::OpcodeSignatureMismatch(id));
                    };
                    let receiver = ordinary[0];
                    let index = ordinary[1];
                    let payload = ordinary[2];
                    let receiver_is_tagged_location = receiver
                        == MachineOperand::location_input(receiver.value)
                        && self.representations[receiver.value.0 as usize]
                            == MachineRepresentation::Tagged;
                    let index_representation = self.representations[index.value.0 as usize];
                    let index_is_scalar_location = index
                        == MachineOperand::location_input(index.value)
                        && matches!(
                            index_representation,
                            MachineRepresentation::Tagged
                                | MachineRepresentation::Int32
                                | MachineRepresentation::Uint32
                        );
                    let payload_signature = match instruction.opcode {
                        MachineOpcode::PackedDoubleElementLoad { .. } => {
                            payload == MachineOperand::register_output(payload.value)
                                && self.representations[payload.value.0 as usize]
                                    == MachineRepresentation::Float64
                        }
                        MachineOpcode::PackedDoubleElementStore { .. } => {
                            payload == MachineOperand::register_input(payload.value)
                                && self.representations[payload.value.0 as usize]
                                    == MachineRepresentation::Float64
                        }
                        _ => false,
                    };
                    let output = matches!(
                        instruction.opcode,
                        MachineOpcode::PackedDoubleElementLoad { .. }
                    )
                    .then_some(payload.value);
                    let mut deopt_values = std::collections::BTreeSet::new();
                    let metadata_is_exact_deopt = metadata.iter().all(|operand| {
                        *operand == MachineOperand::deopt(operand.value)
                            && Some(operand.value) != output
                            && deopt_values.insert(operand.value)
                    });
                    let semantic_receiver = self.semantic_deopt_source_before(id, receiver.value);
                    let semantic_index = self.semantic_deopt_source_before(id, index.value);
                    let semantic_payload = self.semantic_deopt_source_before(id, payload.value);
                    let expected_clobbers = std::iter::once(PhysicalRegister::integer(9))
                        .chain((11..=16).map(PhysicalRegister::integer))
                        .collect::<Vec<_>>();
                    if !receiver_is_tagged_location
                        || !index_is_scalar_location
                        || !payload_signature
                        || !metadata_is_exact_deopt
                        || !deopt_values.contains(&semantic_receiver)
                        || !deopt_values.contains(&semantic_index)
                        || (matches!(
                            instruction.opcode,
                            MachineOpcode::PackedDoubleElementStore { .. }
                        ) && !deopt_values.contains(&semantic_payload))
                        || instruction.deopt.is_none()
                        || instruction.safepoint.is_some()
                        || instruction.clobbers != expected_clobbers
                    {
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
                        CallTarget::RuntimeStub(_) => true,
                        CallTarget::Direct {
                            kind, candidates, ..
                        } => {
                            let method = *kind == DirectCallKind::Method;
                            let count = candidates.len();
                            let valid_count = if method {
                                (1..=MAX_MACHINE_DIRECT_METHOD_TARGETS).contains(&count)
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
                                && descriptor.result == Some(MachineRepresentation::Tagged)
                                && descriptor.effects == CallEffects::PURE
                                && descriptor.clobbers.is_empty()
                                && descriptor.safepoint == SafepointKind::None
                                && instruction.deopt.is_some()
                        }
                    };
                    if !valid_target {
                        return Err(VerificationError::InvalidCallTarget(id));
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
                        || outputs.as_slice() != descriptor.result.as_slice()
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
                        OperandPurpose::TaggedRoot
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
                        OperandPurpose::TaggedRoot | OperandPurpose::CellRoot
                            if instruction.safepoint.is_none() =>
                        {
                            return Err(VerificationError::RootWithoutSafepoint(id));
                        }
                        OperandPurpose::Deopt if instruction.deopt.is_none() => {
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
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packed_double_view_cache_ids_bound_raw_frame_words() {
        let first = PackedDoubleViewCacheId::new(0).expect("first cache");
        let last = PackedDoubleViewCacheId::new(MAX_PACKED_DOUBLE_VIEW_CACHES - 1)
            .expect("last bounded cache");
        assert_eq!(first.raw_word(), 0);
        assert_eq!(last.index(), 31);
        assert_eq!(last.raw_word(), 62);
        assert!(PackedDoubleViewCacheId::new(MAX_PACKED_DOUBLE_VIEW_CACHES).is_none());
        assert_eq!(
            MAX_PACKED_DOUBLE_VIEW_CACHES * PACKED_DOUBLE_VIEW_CACHE_RAW_WORDS,
            64
        );
    }

    #[test]
    fn verifier_bounds_packed_double_view_caches_and_clear_signature() {
        let input = MachineValue(0);
        let mut clear = MachineInstruction::plain(
            MachineOpcode::ClearPackedDoubleViewCaches(PackedDoubleViewCacheClearReason::LoopEntry),
            Vec::new(),
        );
        let mut ret = MachineInstruction::plain(
            MachineOpcode::Return,
            vec![MachineOperand::register_input(input)],
        );
        ret.control = ControlFlow::Return;
        let mut sequence = InstructionSequence::new_with_packed_double_view_caches(
            MachineBlock(0),
            vec![MachineRepresentation::Tagged],
            Vec::new(),
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
                clear.clone(),
                ret,
            ],
            1,
        )
        .expect("one-cache sequence");
        assert!(
            sequence
                .normalized()
                .starts_with("machine-ir packed-double-view-caches=1\n")
        );

        sequence.packed_double_view_cache_count = 33;
        assert_eq!(
            sequence.verify(),
            Err(VerificationError::TooManyPackedDoubleViewCaches(33))
        );
        sequence.packed_double_view_cache_count = 1;
        clear.clobbers.push(PhysicalRegister::integer(9));
        sequence.instructions[1] = clear;
        assert_eq!(
            sequence.verify(),
            Err(VerificationError::OpcodeSignatureMismatch(
                MachineInstructionId(1)
            ))
        );
    }

    #[test]
    fn verifier_rejects_packed_double_cache_outside_sequence() {
        let receiver = MachineValue(0);
        let index = MachineValue(1);
        let result = MachineValue(2);
        let mut load = MachineInstruction::plain(
            MachineOpcode::PackedDoubleElementLoad {
                byte_pc: 24,
                cache: PackedDoubleViewCacheId::new(0),
            },
            vec![
                MachineOperand::location_input(receiver),
                MachineOperand::location_input(index),
                MachineOperand::register_output(result),
                MachineOperand::deopt(receiver),
                MachineOperand::deopt(index),
            ],
        );
        load.clobbers = std::iter::once(PhysicalRegister::integer(9))
            .chain((11..=16).map(PhysicalRegister::integer))
            .collect();
        load.deopt = Some(DeoptId(0));
        let mut ret = MachineInstruction::plain(
            MachineOpcode::Return,
            vec![MachineOperand::register_input(result)],
        );
        ret.control = ControlFlow::Return;
        let mut sequence = InstructionSequence::new_with_packed_double_view_caches(
            MachineBlock(0),
            vec![
                MachineRepresentation::Tagged,
                MachineRepresentation::Uint32,
                MachineRepresentation::Float64,
            ],
            Vec::new(),
            vec![MachineBlockData {
                first: MachineInstructionId(0),
                end: MachineInstructionId(4),
                predecessors: Vec::new(),
                successors: Vec::new(),
                parameters: Vec::new(),
                successor_arguments: Vec::new(),
            }],
            vec![
                MachineInstruction::plain(
                    MachineOpcode::EntryValue(0),
                    vec![MachineOperand::register_output(receiver)],
                ),
                MachineInstruction::plain(
                    MachineOpcode::IntegerConstant(0),
                    vec![MachineOperand::register_output(index)],
                ),
                load,
                ret,
            ],
            1,
        )
        .expect("cached packed load");
        sequence.instructions[2].opcode = MachineOpcode::PackedDoubleElementLoad {
            byte_pc: 24,
            cache: PackedDoubleViewCacheId::new(1),
        };
        assert_eq!(
            sequence.verify(),
            Err(VerificationError::InvalidPackedDoubleViewCache(
                MachineInstructionId(2),
                PackedDoubleViewCacheId::new(1).expect("bounded id")
            ))
        );
    }

    fn checked_instruction_sequence(
        result_representation: MachineRepresentation,
        mut checked: MachineInstruction,
    ) -> InstructionSequence {
        let input = MachineValue(0);
        let result = MachineValue(1);
        checked.deopt = Some(DeoptId(0));
        let mut ret = MachineInstruction::plain(
            MachineOpcode::Return,
            vec![MachineOperand::register_input(result)],
        );
        ret.control = ControlFlow::Return;
        InstructionSequence::new(
            MachineBlock(0),
            vec![MachineRepresentation::Tagged, result_representation],
            Vec::new(),
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
    fn verifier_rejects_missing_global_load_scratch_clobbers() {
        let input = MachineValue(0);
        let result = MachineValue(1);
        let cases = [
            (
                MachineOpcode::GlobalLexicalLoad {
                    byte_pc: 8,
                    target: otter_vm::jit::JitGlobalLexicalLoad { cell_offset: 32 },
                },
                [9, 11, 13].as_slice(),
            ),
            (
                MachineOpcode::GlobalObjectLoad {
                    byte_pc: 8,
                    target: otter_vm::jit::JitGlobalObjectLoad {
                        shape: 7,
                        dictionary: false,
                        value_byte: 16,
                        global_lexical_epoch: 3,
                    },
                },
                [9, 11, 12, 13, 14, 15].as_slice(),
            ),
        ];
        for (opcode, clobbers) in cases {
            let mut load = MachineInstruction::plain(
                opcode,
                vec![
                    MachineOperand::register_output(result),
                    MachineOperand::deopt(input),
                ],
            );
            load.clobbers = clobbers
                .iter()
                .copied()
                .map(PhysicalRegister::integer)
                .collect();
            let mut sequence = checked_instruction_sequence(MachineRepresentation::Tagged, load);
            sequence.instructions[1].clobbers.clear();
            assert_eq!(
                sequence.verify(),
                Err(VerificationError::OpcodeSignatureMismatch(
                    MachineInstructionId(1)
                ))
            );
        }
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
                MachineOperand::deopt(input),
            ],
        );
        compare.clobbers = vec![PhysicalRegister::integer(16)];
        let mut sequence = checked_instruction_sequence(MachineRepresentation::Boolean, compare);
        sequence.instructions[1].operands.pop();
        assert_eq!(
            sequence.verify(),
            Err(VerificationError::OpcodeSignatureMismatch(
                MachineInstructionId(1)
            ))
        );
    }
}
