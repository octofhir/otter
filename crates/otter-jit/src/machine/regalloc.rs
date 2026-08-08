//! regalloc2 integration and post-allocation metadata finalization.
//!
//! # Contents
//! - [`allocate`] — validates SSA, runs the Ion allocator, and converts output.
//! - [`AllocatedSequence`] — target locations for operands and inserted moves.
//! - [`AllocatedMetadata`] — the single root/deopt location table.
//!
//! # Invariants
//! - Every allocator operand corresponds one-for-one with a stored Machine IR
//!   operand; metadata never uses a reconstructed or pre-allocation location.
//! - A metadata operand is a late use, so its allocation is valid through the
//!   safepoint/deopt instruction and cannot overlap a declared clobber.
//! - Allocation edits are retained in program-point order for final emission.

use std::fmt::Write as _;

use regalloc2::{
    Allocation, Block, Edit, Function, Inst, InstRange, Operand, OperandConstraint as RaConstraint,
    OperandKind, OperandPos, PRegSet, RegAllocError, RegallocOptions, VReg,
};

use super::{
    ControlFlow, DeoptId, InstructionSequence, MachineInstructionId, MachineOperand, MachineValue,
    OperandConstraint, OperandPurpose, OperandRole, OperandTiming, PhysicalRegister, SafepointId,
    TargetRegisterFile, VerificationError,
};

/// Final register or stack location assigned by regalloc2.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum AllocatedLocation {
    /// Physical register.
    Register(PhysicalRegister),
    /// Word-sized stack spill slot.
    Stack(u32),
}

/// Before/after position for one inserted allocation edit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum AllocationPoint {
    /// Immediately before the instruction.
    Before(MachineInstructionId),
    /// Immediately after the instruction.
    After(MachineInstructionId),
}

/// Register/spill move inserted by allocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AllocationEdit {
    /// Program point where the move executes.
    pub point: AllocationPoint,
    /// Source location.
    pub from: AllocatedLocation,
    /// Destination location.
    pub to: AllocatedLocation,
}

/// Exact location of one root or deoptimization value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AllocatedMetadata {
    /// Instruction carrying the metadata operand.
    pub instruction: MachineInstructionId,
    /// Safepoint identity for a root.
    pub safepoint: Option<SafepointId>,
    /// Deopt identity for a reconstruction value.
    pub deopt: Option<DeoptId>,
    /// Virtual value whose bits occupy the location.
    pub value: MachineValue,
    /// Root/deopt kind.
    pub purpose: OperandPurpose,
    /// Exact post-allocation location.
    pub location: AllocatedLocation,
}

/// Complete allocator output consumed by metadata and target emission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AllocatedSequence {
    architecture: super::TargetArchitecture,
    spill_slots: u32,
    operand_locations: Vec<Vec<AllocatedLocation>>,
    edits: Vec<AllocationEdit>,
    metadata: Vec<AllocatedMetadata>,
}

impl AllocatedSequence {
    /// Number of word-sized spill slots reserved by allocation.
    #[must_use]
    pub const fn spill_slots(&self) -> u32 {
        self.spill_slots
    }

    /// Per-operand locations for one instruction.
    #[must_use]
    pub fn instruction_locations(
        &self,
        instruction: MachineInstructionId,
    ) -> Option<&[AllocatedLocation]> {
        self.operand_locations
            .get(instruction.0 as usize)
            .map(Vec::as_slice)
    }

    /// Allocation edits in program-point order.
    #[must_use]
    pub fn edits(&self) -> &[AllocationEdit] {
        &self.edits
    }

    /// Shared exact root/deopt location table.
    #[must_use]
    pub fn metadata(&self) -> &[AllocatedMetadata] {
        &self.metadata
    }

    /// Distinct physical registers named by operands or allocator edits.
    ///
    /// Target frame builders use this exact post-allocation set to derive
    /// callee-saved state; metadata locations are already a subset of operand
    /// locations and therefore need no parallel scan.
    pub fn used_registers(&self) -> impl Iterator<Item = PhysicalRegister> + '_ {
        self.operand_locations
            .iter()
            .flatten()
            .chain(self.edits.iter().flat_map(|edit| [&edit.from, &edit.to]))
            .filter_map(|location| match location {
                AllocatedLocation::Register(register) => Some(*register),
                AllocatedLocation::Stack(_) => None,
            })
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
    }

    /// Number of distinct physical registers named by operands or edits.
    #[must_use]
    pub fn used_register_count(&self) -> u32 {
        self.used_registers().count() as u32
    }

    /// Deterministic post-allocation identity used by artifacts and tests.
    #[must_use]
    pub fn normalized(&self) -> String {
        let mut output = format!(
            "allocation target={:?} spills={}\n",
            self.architecture, self.spill_slots
        );
        for (index, locations) in self.operand_locations.iter().enumerate() {
            writeln!(output, "i{index} {locations:?}").expect("writing to String cannot fail");
        }
        for edit in &self.edits {
            writeln!(output, "edit {edit:?}").expect("writing to String cannot fail");
        }
        for metadata in &self.metadata {
            writeln!(output, "metadata {metadata:?}").expect("writing to String cannot fail");
        }
        output
    }
}

/// Failure to verify or allocate target-selected Machine IR.
#[derive(Debug, Clone)]
pub enum AllocationError {
    /// The sequence failed its structural verifier.
    Verification(VerificationError),
    /// regalloc2 rejected CFG, SSA, or register pressure.
    RegisterAllocation(RegAllocError),
    /// A physical allocation used an unsupported register class.
    UnsupportedRegisterClass,
    /// Allocator spill-slot count does not fit the current metadata schema.
    SpillSlotOverflow,
    /// Allocator result did not contain one location per Machine IR operand.
    OperandAllocationMismatch(MachineInstructionId),
}

impl std::fmt::Display for AllocationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Machine IR allocation failed: {self:?}")
    }
}

impl std::error::Error for AllocationError {}

impl From<VerificationError> for AllocationError {
    fn from(error: VerificationError) -> Self {
        Self::Verification(error)
    }
}

struct RegallocFunction<'a> {
    sequence: &'a InstructionSequence,
    operands: Vec<Vec<Operand>>,
    predecessors: Vec<Vec<Block>>,
    successors: Vec<Vec<Block>>,
    parameters: Vec<Vec<VReg>>,
    successor_arguments: Vec<Vec<Vec<VReg>>>,
}

impl Function for RegallocFunction<'_> {
    fn num_insts(&self) -> usize {
        self.sequence.instructions().len()
    }

    fn num_blocks(&self) -> usize {
        self.sequence.blocks().len()
    }

    fn entry_block(&self) -> Block {
        Block::new(self.sequence.entry.0 as usize)
    }

    fn block_insns(&self, block: Block) -> InstRange {
        let block = &self.sequence.blocks()[block.index()];
        InstRange::new(
            Inst::new(block.first.0 as usize),
            Inst::new(block.end.0 as usize),
        )
    }

    fn block_succs(&self, block: Block) -> &[Block] {
        &self.successors[block.index()]
    }

    fn block_preds(&self, block: Block) -> &[Block] {
        &self.predecessors[block.index()]
    }

    fn block_params(&self, block: Block) -> &[VReg] {
        &self.parameters[block.index()]
    }

    fn is_ret(&self, instruction: Inst) -> bool {
        self.sequence.instructions()[instruction.index()].control == ControlFlow::Return
    }

    fn is_branch(&self, instruction: Inst) -> bool {
        self.sequence.instructions()[instruction.index()].control == ControlFlow::Branch
    }

    fn branch_blockparams(&self, block: Block, _: Inst, successor: usize) -> &[VReg] {
        &self.successor_arguments[block.index()][successor]
    }

    fn inst_operands(&self, instruction: Inst) -> &[Operand] {
        &self.operands[instruction.index()]
    }

    fn inst_clobbers(&self, instruction: Inst) -> PRegSet {
        let mut clobbers = PRegSet::empty();
        for &register in &self.sequence.instructions()[instruction.index()].clobbers {
            clobbers.add(register.as_regalloc());
        }
        clobbers
    }

    fn num_vregs(&self) -> usize {
        self.sequence.representations().len()
    }

    fn spillslot_size(&self, _: regalloc2::RegClass) -> usize {
        1
    }
}

pub(super) fn allocate(
    sequence: &InstructionSequence,
    target: &TargetRegisterFile,
) -> Result<AllocatedSequence, AllocationError> {
    sequence.verify()?;
    let function = build_regalloc_function(sequence);
    let options = RegallocOptions {
        verbose_log: false,
        validate_ssa: true,
        algorithm: regalloc2::Algorithm::Ion,
    };
    let output = regalloc2::run(&function, &target.environment(), &options)
        .map_err(AllocationError::RegisterAllocation)?;
    let spill_slots =
        u32::try_from(output.num_spillslots).map_err(|_| AllocationError::SpillSlotOverflow)?;

    let mut operand_locations = Vec::with_capacity(sequence.instructions().len());
    let mut metadata = Vec::new();
    for (instruction_index, instruction) in sequence.instructions().iter().enumerate() {
        let id = MachineInstructionId(instruction_index as u32);
        let allocations = output.inst_allocs(Inst::new(instruction_index));
        if allocations.len() != instruction.operands.len() {
            return Err(AllocationError::OperandAllocationMismatch(id));
        }
        let locations = allocations
            .iter()
            .copied()
            .map(convert_location)
            .collect::<Result<Vec<_>, _>>()?;
        for (operand, &location) in instruction.operands.iter().zip(&locations) {
            if operand.purpose.is_metadata() {
                metadata.push(AllocatedMetadata {
                    instruction: id,
                    safepoint: matches!(
                        operand.purpose,
                        OperandPurpose::TaggedRoot | OperandPurpose::CellRoot
                    )
                    .then_some(instruction.safepoint)
                    .flatten(),
                    deopt: (operand.purpose == OperandPurpose::Deopt)
                        .then_some(instruction.deopt)
                        .flatten(),
                    value: operand.value,
                    purpose: operand.purpose,
                    location,
                });
            }
        }
        operand_locations.push(locations);
    }

    let edits = output
        .edits
        .iter()
        .map(|(point, edit)| {
            let point = match point.pos() {
                regalloc2::InstPosition::Before => {
                    AllocationPoint::Before(MachineInstructionId(point.inst().index() as u32))
                }
                regalloc2::InstPosition::After => {
                    AllocationPoint::After(MachineInstructionId(point.inst().index() as u32))
                }
            };
            let Edit::Move { from, to } = edit;
            Ok(AllocationEdit {
                point,
                from: convert_location(*from)?,
                to: convert_location(*to)?,
            })
        })
        .collect::<Result<Vec<_>, AllocationError>>()?;

    Ok(AllocatedSequence {
        architecture: target.architecture(),
        spill_slots,
        operand_locations,
        edits,
        metadata,
    })
}

fn build_regalloc_function(sequence: &InstructionSequence) -> RegallocFunction<'_> {
    let vreg = |value: MachineValue| {
        VReg::new(
            value.0 as usize,
            sequence.representations()[value.0 as usize].register_class(),
        )
    };
    RegallocFunction {
        sequence,
        operands: sequence
            .instructions()
            .iter()
            .map(|instruction| {
                instruction
                    .operands
                    .iter()
                    .map(|operand| convert_operand(operand, vreg(operand.value)))
                    .collect()
            })
            .collect(),
        predecessors: sequence
            .blocks()
            .iter()
            .map(|block| {
                block
                    .predecessors
                    .iter()
                    .map(|block| Block::new(block.0 as usize))
                    .collect()
            })
            .collect(),
        successors: sequence
            .blocks()
            .iter()
            .map(|block| {
                block
                    .successors
                    .iter()
                    .map(|block| Block::new(block.0 as usize))
                    .collect()
            })
            .collect(),
        parameters: sequence
            .blocks()
            .iter()
            .map(|block| block.parameters.iter().copied().map(vreg).collect())
            .collect(),
        successor_arguments: sequence
            .blocks()
            .iter()
            .map(|block| {
                block
                    .successor_arguments
                    .iter()
                    .map(|arguments| arguments.iter().copied().map(vreg).collect())
                    .collect()
            })
            .collect(),
    }
}

fn convert_operand(operand: &MachineOperand, vreg: VReg) -> Operand {
    let constraint = match operand.constraint {
        OperandConstraint::Any => RaConstraint::Any,
        OperandConstraint::Register => RaConstraint::Reg,
        OperandConstraint::Stack => RaConstraint::Stack,
        OperandConstraint::Fixed(register) => RaConstraint::FixedReg(register.as_regalloc()),
        OperandConstraint::Reuse(index) => RaConstraint::Reuse(usize::from(index)),
    };
    let kind = match operand.role {
        OperandRole::Use => OperandKind::Use,
        OperandRole::Definition => OperandKind::Def,
    };
    let timing = match operand.timing {
        OperandTiming::Early => OperandPos::Early,
        OperandTiming::Late => OperandPos::Late,
    };
    Operand::new(vreg, constraint, kind, timing)
}

fn convert_location(allocation: Allocation) -> Result<AllocatedLocation, AllocationError> {
    if let Some(register) = allocation.as_reg() {
        let register = match register.class() {
            regalloc2::RegClass::Int => PhysicalRegister::integer(register.hw_enc() as u8),
            regalloc2::RegClass::Float => PhysicalRegister::float(register.hw_enc() as u8),
            regalloc2::RegClass::Vector => {
                return Err(AllocationError::UnsupportedRegisterClass);
            }
        };
        Ok(AllocatedLocation::Register(register))
    } else if let Some(stack) = allocation.as_stack() {
        Ok(AllocatedLocation::Stack(stack.index() as u32))
    } else {
        Err(AllocationError::UnsupportedRegisterClass)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::machine::{
        CallDescriptor, CallEffects, ControlFlow, DeoptId, ExceptionalEdge, InstructionSequence,
        MachineBlock, MachineBlockData, MachineInstruction, MachineOpcode, MachineOperand,
        MachineRepresentation, OperandConstraint, OperandPurpose, OperandRole, OperandTiming,
        SafepointId, SafepointKind,
    };

    fn sequence() -> InstructionSequence {
        let tagged = MachineValue(0);
        let integer = MachineValue(1);
        let result = MachineValue(2);
        let mut entry_tagged = MachineInstruction::plain(
            MachineOpcode::EntryValue(0),
            vec![MachineOperand::register_output(tagged)],
        );
        entry_tagged.operands[0].constraint =
            OperandConstraint::Fixed(PhysicalRegister::integer(0));
        let mut entry_integer = MachineInstruction::plain(
            MachineOpcode::IntegerConstant(7),
            vec![MachineOperand::register_output(integer)],
        );
        entry_integer.operands[0].constraint =
            OperandConstraint::Fixed(PhysicalRegister::integer(1));
        let mut call = MachineInstruction::plain(
            MachineOpcode::Call(0),
            vec![MachineOperand::tagged_root(tagged)],
        );
        call.safepoint = Some(SafepointId(0));
        call.clobbers = (0..=18).map(PhysicalRegister::integer).collect();
        let call_clobbers = call.clobbers.clone();
        let mut add = MachineInstruction::plain(
            MachineOpcode::IntegerAdd,
            vec![
                MachineOperand::register_input(tagged),
                MachineOperand::register_input(integer),
                MachineOperand::register_output(result),
                MachineOperand::deopt(tagged),
                MachineOperand::deopt(integer),
            ],
        );
        add.deopt = Some(DeoptId(0));
        let mut ret = MachineInstruction::plain(
            MachineOpcode::Return,
            vec![MachineOperand::register_input(result)],
        );
        ret.control = ControlFlow::Return;
        InstructionSequence::new(
            MachineBlock(0),
            vec![
                MachineRepresentation::Tagged,
                MachineRepresentation::Int64,
                MachineRepresentation::Int64,
            ],
            vec![CallDescriptor {
                arguments: vec![],
                result: None,
                effects: CallEffects::READS_HEAP,
                clobbers: call_clobbers,
                exceptional: ExceptionalEdge::None,
                safepoint: SafepointKind::Gc,
            }],
            vec![MachineBlockData {
                first: MachineInstructionId(0),
                end: MachineInstructionId(5),
                predecessors: vec![],
                successors: vec![],
                parameters: vec![],
                successor_arguments: vec![],
            }],
            vec![entry_tagged, entry_integer, call, add, ret],
        )
        .expect("valid selected function")
    }

    #[test]
    fn both_targets_allocate_identical_metadata_contract() {
        let sequence = sequence();
        for target in [TargetRegisterFile::aarch64(), TargetRegisterFile::x86_64()] {
            let allocated = sequence.allocate(&target).expect("allocation succeeds");
            assert_eq!(allocated.metadata().len(), 3);
            assert_eq!(allocated.metadata()[0].safepoint, Some(SafepointId(0)));
            assert_eq!(allocated.metadata()[1].deopt, Some(DeoptId(0)));
            assert_eq!(allocated.metadata()[2].deopt, Some(DeoptId(0)));
            assert_ne!(
                allocated.metadata()[1].location,
                allocated.metadata()[2].location
            );
        }
    }

    #[test]
    fn call_clobbers_force_the_tagged_root_out_of_caller_saved_registers() {
        let allocated = sequence()
            .allocate(&TargetRegisterFile::aarch64())
            .expect("allocation succeeds");
        let root = allocated.metadata()[0];
        assert_eq!(root.purpose, OperandPurpose::TaggedRoot);
        match root.location {
            AllocatedLocation::Register(register) => {
                assert!(!(0..=18).contains(&register.encoding()));
            }
            AllocatedLocation::Stack(_) => {}
        }
    }

    #[test]
    fn normalized_ir_and_allocation_are_deterministic() {
        let first = sequence();
        let second = sequence();
        assert_eq!(first.normalized(), second.normalized());
        let target = TargetRegisterFile::aarch64();
        assert_eq!(
            first.allocate(&target).expect("first").normalized(),
            second.allocate(&target).expect("second").normalized()
        );
    }

    #[test]
    fn metadata_must_be_a_late_use_at_a_matching_exit() {
        let mut invalid = MachineOperand::deopt(MachineValue(0));
        invalid.timing = OperandTiming::Early;
        invalid.role = OperandRole::Definition;
        invalid.purpose = OperandPurpose::TaggedRoot;
        let mut ret = MachineInstruction::plain(MachineOpcode::Return, vec![invalid]);
        ret.control = ControlFlow::Return;
        let error = InstructionSequence::new(
            MachineBlock(0),
            vec![MachineRepresentation::Tagged],
            vec![],
            vec![MachineBlockData {
                first: MachineInstructionId(0),
                end: MachineInstructionId(1),
                predecessors: vec![],
                successors: vec![],
                parameters: vec![],
                successor_arguments: vec![],
            }],
            vec![ret],
        )
        .expect_err("invalid metadata rejected");
        assert!(matches!(
            error,
            VerificationError::InvalidMetadataOperand(..)
        ));
    }
}
