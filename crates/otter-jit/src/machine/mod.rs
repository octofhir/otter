//! Target-selected Machine IR and exact post-allocation metadata.
//!
//! This module is the final boundary between JavaScript-semantic lowering and
//! target code emission. Instructions carry only machine operations, virtual
//! registers, physical constraints, clobbers, and metadata operands. They do
//! not contain bytecode opcodes, shapes, access plans, or interpreter slots.
//!
//! # Contents
//! - [`InstructionSequence`] — verified block, value, and instruction storage.
//! - [`MachineInstruction`] — one selected operation and its allocator inputs.
//! - [`TargetRegisterFile`] — complete allocatable target register inventory.
//! - [`AllocatedSequence`] — allocator edits, per-operand locations, and exact
//!   safepoint/deoptimization locations.
//! - [`MachineFrameLayout`] — aligned post-allocation spill-frame contract.
//!
//! # Invariants
//! - Virtual values are dense and have one machine representation.
//! - Every block owns a non-empty contiguous instruction range ending in one
//!   branch or return instruction.
//! - Metadata values are ordinary late uses. Calls therefore cannot leave a
//!   live GC/deopt value in a clobbered register.
//! - Root and deopt maps are built from the same per-operand allocation table
//!   consumed by the emitter; there is no pre-allocation location fallback.
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
mod target;

pub use deopt::{
    MachineDeoptError, MachineFrameSlot, MachineFrameState, lower_deopt_table, undefined_slot,
};
pub use frame::{FrameLayoutError, MachineFrameLayout};
pub use regalloc::{
    AllocatedLocation, AllocatedMetadata, AllocatedSequence, AllocationEdit, AllocationError,
    AllocationPoint,
};
pub use target::{PhysicalRegister, TargetArchitecture, TargetRegisterFile};

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
            Self::Tagged | Self::Cell | Self::Int32 | Self::Uint32 | Self::Int64 => {
                regalloc2::RegClass::Int
            }
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

/// Complete target-neutral call contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallDescriptor {
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
    /// Guard and decode one tagged JavaScript Number.
    DecodeNumber,
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
    /// Load the Int32 result deposited by the preceding numeric leaf call.
    IntegerLeafResult,
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
    /// Load the Float64 result deposited by the preceding numeric leaf call.
    FloatLeafResult,
    /// Floating-point negation.
    FloatNeg,
    /// Convert an Int32 or Uint32 value to canonical Boolean bits.
    IntegerToBoolean,
    /// Convert a Float64 value to canonical Boolean bits.
    FloatToBoolean,
    /// Invert canonical Boolean bits.
    BooleanNot,
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
    /// Target ABI call through a call descriptor.
    Call(u32),
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
        let sequence = Self {
            entry,
            representations,
            call_descriptors,
            blocks,
            instructions,
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

    /// Deterministic identity excluding source/module/function identities.
    #[must_use]
    pub fn normalized(&self) -> String {
        let mut output = String::from("machine-ir\n");
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

    pub(super) fn verify(&self) -> Result<(), VerificationError> {
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
                        MachineOpcode::Jump if block.successors.len() != 1 => {
                            return Err(VerificationError::TerminatorSuccessors(block_id));
                        }
                        MachineOpcode::BranchIf(_) if block.successors.len() != 2 => {
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
                if let MachineOpcode::Call(descriptor_index) = instruction.opcode {
                    let Some(descriptor) = self.call_descriptors.get(descriptor_index as usize)
                    else {
                        return Err(VerificationError::InvalidCallDescriptor(
                            id,
                            descriptor_index,
                        ));
                    };
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
