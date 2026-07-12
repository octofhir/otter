//! Backend-neutral baseline lowering rules and operand decoding.
//!
//! # Contents
//! - Whole-function fallback reasons shared by baseline backends.
//! - Call-site argument packing policy.
//! - A linear typed instruction stream with canonical branch targets.
//! - Bytecode operand decoding and branch/register validation.
//!
//! # Invariants
//! - Invalid bytecode shapes reject the whole baseline compilation.
//! - Decoding never invents defaults for missing or mismatched operands.
//! - Register offsets fit the baseline backend's unsigned scaled addressing.
//!
//! # See also
//! - [`super::arm64`], the first machine-code consumer of these rules.

use otter_bytecode::{Op, Operand};
use otter_vm::{JitCompileSnapshot, NO_FRAME_STATE, SafepointId, SafepointRecord};
use std::collections::{BTreeMap, BTreeSet};

/// Largest argument count the `Call` emitter inlines.
pub(crate) const MAX_INLINE_ARGS: usize = 4;

/// Largest argument count a `CallMethodValue` site passes inline.
///
/// Argument register indices occupy one 16-bit lane each in a single word.
pub(crate) const MAX_METHOD_ARGS: usize = 4;

/// Why a function could not be baseline-compiled.
///
/// Every variant maps to a silent interpreter fallback and is never exposed as
/// a JavaScript error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unsupported {
    /// An opcode outside the supported subset.
    Opcode(Op),
    /// An operand whose kind/shape baseline lowering does not handle.
    OperandShape(&'static str),
    /// A branch whose logical target does not name an instruction boundary.
    BranchTarget(i64),
    /// A register index whose byte offset exceeds inline load/store addressing.
    RegisterRange(u16),
    /// A call with more arguments than baseline lowering inlines.
    ArgCount(usize),
}

/// Typed destination register for fixed-operand loads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DestinationOperands {
    pub(crate) dst: u16,
}

/// Typed source register for return-like operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SourceOperands {
    pub(crate) src: u16,
}

/// Typed destination plus constant-pool index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ConstantOperands {
    pub(crate) dst: u16,
    pub(crate) constant: u32,
}

/// Typed immediate load operands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LoadInt32Operands {
    pub(crate) dst: u16,
    pub(crate) value: i32,
}

/// Typed local-window transfer operands. `value` is the loaded destination or
/// stored source according to the opcode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LocalOperands {
    pub(crate) value: u16,
    pub(crate) local: u16,
}

/// Typed two-register operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct UnaryOperands {
    pub(crate) dst: u16,
    pub(crate) src: u16,
}

/// Typed three-register operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BinaryOperands {
    pub(crate) dst: u16,
    pub(crate) lhs: u16,
    pub(crate) rhs: u16,
}

/// Typed element-load operands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ElementLoadOperands {
    pub(crate) dst: u16,
    pub(crate) receiver: u16,
    pub(crate) index: u16,
}

/// Typed element-store operands including the bytecode scratch slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ElementStoreOperands {
    pub(crate) receiver: u16,
    pub(crate) index: u16,
    pub(crate) value: u16,
    pub(crate) scratch: u16,
}

/// Typed update-expression operands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct IncrementOperands {
    pub(crate) dst: u16,
    pub(crate) src: u16,
    pub(crate) delta: i32,
}

/// Typed named-property load operands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PropertyLoadOperands {
    pub(crate) dst: u16,
    pub(crate) object: u16,
    pub(crate) name: u32,
}

/// Typed named-property store operands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PropertyStoreOperands {
    pub(crate) object: u16,
    pub(crate) name: u32,
    pub(crate) value: u16,
    pub(crate) scratch: u16,
}

/// Typed captured-binding transfer operands. `value` is the load destination
/// or store source according to the opcode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct UpvalueOperands {
    pub(crate) value: u16,
    pub(crate) index: i32,
}

/// Canonical control-flow target resolved to a verified logical PC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BranchOperands {
    pub(crate) target: u32,
}

/// Conditional control-flow operands with a verified canonical target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ConditionalBranchOperands {
    pub(crate) target: u32,
    pub(crate) condition: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LoweredOperands {
    Raw,
    Destination(DestinationOperands),
    Source(SourceOperands),
    Constant(ConstantOperands),
    LoadInt32(LoadInt32Operands),
    Local(LocalOperands),
    Unary(UnaryOperands),
    Binary(BinaryOperands),
    ElementLoad(ElementLoadOperands),
    ElementStore(ElementStoreOperands),
    Increment(IncrementOperands),
    PropertyLoad(PropertyLoadOperands),
    PropertyStore(PropertyStoreOperands),
    Upvalue(UpvalueOperands),
    Branch(BranchOperands),
    ConditionalBranch(ConditionalBranchOperands),
}

/// One backend-neutral instruction in canonical emission order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LoweredInstr {
    pub(crate) byte_pc: u32,
    pub(crate) instruction_pc: u32,
    pub(crate) op: Op,
    operands: LoweredOperands,
}

impl LoweredInstr {
    pub(crate) fn destination_operands(self) -> Result<DestinationOperands, Unsupported> {
        match self.operands {
            LoweredOperands::Destination(operands) => Ok(operands),
            _ => Err(Unsupported::OperandShape("lowered destination operands")),
        }
    }

    pub(crate) fn source_operands(self) -> Result<SourceOperands, Unsupported> {
        match self.operands {
            LoweredOperands::Source(operands) => Ok(operands),
            _ => Err(Unsupported::OperandShape("lowered source operands")),
        }
    }

    pub(crate) fn constant_operands(self) -> Result<ConstantOperands, Unsupported> {
        match self.operands {
            LoweredOperands::Constant(operands) => Ok(operands),
            _ => Err(Unsupported::OperandShape("lowered constant operands")),
        }
    }

    pub(crate) fn load_int32_operands(self) -> Result<LoadInt32Operands, Unsupported> {
        match self.operands {
            LoweredOperands::LoadInt32(operands) => Ok(operands),
            _ => Err(Unsupported::OperandShape("lowered LoadInt32 operands")),
        }
    }

    pub(crate) fn local_operands(self) -> Result<LocalOperands, Unsupported> {
        match self.operands {
            LoweredOperands::Local(operands) => Ok(operands),
            _ => Err(Unsupported::OperandShape("lowered local operands")),
        }
    }

    pub(crate) fn unary_operands(self) -> Result<UnaryOperands, Unsupported> {
        match self.operands {
            LoweredOperands::Unary(operands) => Ok(operands),
            _ => Err(Unsupported::OperandShape("lowered unary operands")),
        }
    }

    pub(crate) fn binary_operands(self) -> Result<BinaryOperands, Unsupported> {
        match self.operands {
            LoweredOperands::Binary(operands) => Ok(operands),
            _ => Err(Unsupported::OperandShape("lowered binary operands")),
        }
    }

    pub(crate) fn element_load_operands(self) -> Result<ElementLoadOperands, Unsupported> {
        match self.operands {
            LoweredOperands::ElementLoad(operands) => Ok(operands),
            _ => Err(Unsupported::OperandShape("lowered element-load operands")),
        }
    }

    pub(crate) fn element_store_operands(self) -> Result<ElementStoreOperands, Unsupported> {
        match self.operands {
            LoweredOperands::ElementStore(operands) => Ok(operands),
            _ => Err(Unsupported::OperandShape("lowered element-store operands")),
        }
    }

    pub(crate) fn increment_operands(self) -> Result<IncrementOperands, Unsupported> {
        match self.operands {
            LoweredOperands::Increment(operands) => Ok(operands),
            _ => Err(Unsupported::OperandShape("lowered increment operands")),
        }
    }

    pub(crate) fn property_load_operands(self) -> Result<PropertyLoadOperands, Unsupported> {
        match self.operands {
            LoweredOperands::PropertyLoad(operands) => Ok(operands),
            _ => Err(Unsupported::OperandShape("lowered property-load operands")),
        }
    }

    pub(crate) fn property_store_operands(self) -> Result<PropertyStoreOperands, Unsupported> {
        match self.operands {
            LoweredOperands::PropertyStore(operands) => Ok(operands),
            _ => Err(Unsupported::OperandShape("lowered property-store operands")),
        }
    }

    pub(crate) fn upvalue_operands(self) -> Result<UpvalueOperands, Unsupported> {
        match self.operands {
            LoweredOperands::Upvalue(operands) => Ok(operands),
            _ => Err(Unsupported::OperandShape("lowered upvalue operands")),
        }
    }

    pub(crate) fn branch_operands(self) -> Result<BranchOperands, Unsupported> {
        match self.operands {
            LoweredOperands::Branch(operands) => Ok(operands),
            _ => Err(Unsupported::OperandShape("lowered branch operands")),
        }
    }

    pub(crate) fn conditional_branch_operands(
        self,
    ) -> Result<ConditionalBranchOperands, Unsupported> {
        match self.operands {
            LoweredOperands::ConditionalBranch(operands) => Ok(operands),
            _ => Err(Unsupported::OperandShape(
                "lowered conditional branch operands",
            )),
        }
    }

    pub(crate) fn written_register(self) -> Option<u16> {
        match self.operands {
            LoweredOperands::Destination(operands) => Some(operands.dst),
            LoweredOperands::Constant(operands) => Some(operands.dst),
            LoweredOperands::LoadInt32(operands) => Some(operands.dst),
            LoweredOperands::Local(operands) if self.op == Op::LoadLocal => Some(operands.value),
            LoweredOperands::Unary(operands) => Some(operands.dst),
            LoweredOperands::Binary(operands) => Some(operands.dst),
            LoweredOperands::ElementLoad(operands) => Some(operands.dst),
            LoweredOperands::Increment(operands) => Some(operands.dst),
            LoweredOperands::PropertyLoad(operands) => Some(operands.dst),
            LoweredOperands::Upvalue(operands) if self.op == Op::LoadUpvalue => {
                Some(operands.value)
            }
            _ => None,
        }
    }
}

/// Backend-neutral facts established before machine-code emission starts.
///
/// Keeping this pass allocation-light makes it suitable for every future
/// baseline backend while ensuring the assembler never discovers malformed
/// control flow after it has already emitted part of a function.
pub(crate) struct BaselinePlan {
    pub(crate) instructions: Vec<LoweredInstr>,
    pub(crate) enable_float_residency: bool,
    pub(crate) load_property_count: usize,
    pub(crate) store_property_count: usize,
    pub(crate) safepoint_records: Vec<SafepointRecord>,
    pub(crate) add_alloc_safepoints: BTreeMap<u32, SafepointId>,
    pub(crate) method_alloc_safepoints: BTreeMap<u32, SafepointId>,
}

impl BaselinePlan {
    pub(crate) fn build(view: &JitCompileSnapshot) -> Result<Self, Unsupported> {
        let code_block = view.code_block.as_ref();
        let mut instructions = Vec::with_capacity(view.instructions.len());
        let mut boundaries = BTreeSet::new();
        let mut enable_float_residency = false;
        let mut load_property_count = 0;
        let mut store_property_count = 0;
        for instr in &view.instructions {
            let op = instr.op(code_block);
            let pc = instr.instruction_pc(code_block);
            if !boundaries.insert(pc) {
                return Err(Unsupported::OperandShape("duplicate instruction pc"));
            }

            match op {
                Op::EnterTry | Op::LeaveTry | Op::Throw | Op::EndFinally => {
                    return Err(Unsupported::Opcode(op));
                }
                Op::Div => enable_float_residency = true,
                Op::LoadProperty => load_property_count += 1,
                Op::StoreProperty => store_property_count += 1,
                Op::CallMethodValue => {
                    if let Some(argc) = instr.const_index(code_block, 3)
                        && argc as usize > MAX_METHOD_ARGS
                    {
                        return Err(Unsupported::ArgCount(argc as usize));
                    }
                }
                _ => {}
            }

            let operands = instr.operand_view(code_block);
            let operands = match op {
                Op::MakeFunction | Op::MakeClosure if instr.make_self => {
                    LoweredOperands::Destination(DestinationOperands {
                        dst: reg(operands, 0)?,
                    })
                }
                Op::NewObject | Op::LoadThis => LoweredOperands::Destination(DestinationOperands {
                    dst: reg(operands, 0)?,
                }),
                Op::MakeFunction
                | Op::LoadString
                | Op::LoadGlobalOrThrow
                | Op::LoadBuiltinError => LoweredOperands::Constant(ConstantOperands {
                    dst: reg(operands, 0)?,
                    constant: const_index(operands, 1)?,
                }),
                Op::Return | Op::ReturnValue => LoweredOperands::Source(SourceOperands {
                    src: reg(operands, 0)?,
                }),
                Op::LoadInt32 => LoweredOperands::LoadInt32(LoadInt32Operands {
                    dst: reg(operands, 0)?,
                    value: imm32(operands, 1)?,
                }),
                Op::LoadNumber
                | Op::LoadUndefined
                | Op::LoadNull
                | Op::LoadHole
                | Op::LoadTrue
                | Op::LoadFalse => LoweredOperands::Destination(DestinationOperands {
                    dst: reg(operands, 0)?,
                }),
                Op::LoadLocal | Op::StoreLocal => LoweredOperands::Local(LocalOperands {
                    value: reg(operands, 0)?,
                    local: local_index(operands, 1)?,
                }),
                Op::ToPrimitive | Op::ToNumeric | Op::Neg => {
                    LoweredOperands::Unary(UnaryOperands {
                        dst: reg(operands, 0)?,
                        src: reg(operands, 1)?,
                    })
                }
                Op::Increment => LoweredOperands::Increment(IncrementOperands {
                    dst: reg(operands, 0)?,
                    src: reg(operands, 1)?,
                    delta: imm32(operands, 2)?,
                }),
                Op::LoadElement => LoweredOperands::ElementLoad(ElementLoadOperands {
                    dst: reg(operands, 0)?,
                    receiver: reg(operands, 1)?,
                    index: reg(operands, 2)?,
                }),
                Op::StoreElement => LoweredOperands::ElementStore(ElementStoreOperands {
                    receiver: reg(operands, 0)?,
                    index: reg(operands, 1)?,
                    value: reg(operands, 2)?,
                    scratch: reg(operands, 3)?,
                }),
                Op::LoadProperty => LoweredOperands::PropertyLoad(PropertyLoadOperands {
                    dst: reg(operands, 0)?,
                    object: reg(operands, 1)?,
                    name: const_index(operands, 2)?,
                }),
                Op::StoreProperty => LoweredOperands::PropertyStore(PropertyStoreOperands {
                    object: reg(operands, 0)?,
                    name: const_index(operands, 1)?,
                    value: reg(operands, 2)?,
                    scratch: reg(operands, 3)?,
                }),
                Op::LoadUpvalue | Op::StoreUpvalue | Op::StoreUpvalueChecked => {
                    LoweredOperands::Upvalue(UpvalueOperands {
                        value: reg(operands, 0)?,
                        index: imm32(operands, 1)?,
                    })
                }
                Op::Add
                | Op::Sub
                | Op::Mul
                | Op::Div
                | Op::Rem
                | Op::LessThan
                | Op::LessEq
                | Op::GreaterThan
                | Op::GreaterEq
                | Op::Equal
                | Op::NotEqual
                | Op::LooseEqual
                | Op::LooseNotEqual
                | Op::BitwiseOr
                | Op::BitwiseAnd
                | Op::BitwiseXor
                | Op::Shl
                | Op::Shr
                | Op::Ushr => {
                    let (dst, lhs, rhs) = reg3(operands)?;
                    LoweredOperands::Binary(BinaryOperands { dst, lhs, rhs })
                }
                _ => LoweredOperands::Raw,
            };
            instructions.push(LoweredInstr {
                byte_pc: instr.byte_pc,
                instruction_pc: pc,
                op,
                operands,
            });
        }

        for (instr, lowered) in view.instructions.iter().zip(&mut instructions) {
            if matches!(lowered.op, Op::Jump | Op::JumpIfFalse | Op::JumpIfTrue) {
                let rel = imm32(instr.operand_view(code_block), 0)?;
                let target = branch_target(code_block, instr, rel);
                let target_pc = u32::try_from(target)
                    .ok()
                    .filter(|pc| boundaries.contains(pc))
                    .ok_or(Unsupported::BranchTarget(target))?;
                lowered.operands = match lowered.op {
                    Op::Jump => LoweredOperands::Branch(BranchOperands { target: target_pc }),
                    Op::JumpIfFalse | Op::JumpIfTrue => {
                        LoweredOperands::ConditionalBranch(ConditionalBranchOperands {
                            target: target_pc,
                            condition: reg(instr.operand_view(code_block), 1)?,
                        })
                    }
                    _ => unreachable!("branch opcode was filtered above"),
                };
            }
        }

        let mut safepoint_records: Vec<_> = view.safepoints.values().cloned().collect();
        let mut next_safepoint = safepoint_records
            .iter()
            .map(|record| record.id)
            .max()
            .map_or(1, |id| id.saturating_add(1))
            .max(1);
        let mut add_alloc_safepoints = BTreeMap::new();
        let mut method_alloc_safepoints = BTreeMap::new();
        for lowered in &instructions {
            match lowered.op {
                Op::CallMethodValue => {
                    let safepoint = view
                        .collection_alloc_methods
                        .get(&lowered.byte_pc)
                        .map(|alloc| alloc.safepoint_id)
                        .unwrap_or_else(|| {
                            let id = next_safepoint;
                            next_safepoint = next_safepoint.saturating_add(1);
                            safepoint_records.push(SafepointRecord::frame_slot_window(
                                id,
                                NO_FRAME_STATE,
                                view.code_block.register_count,
                            ));
                            id
                        });
                    method_alloc_safepoints.insert(lowered.byte_pc, safepoint);
                }
                Op::Add => {
                    let safepoint = next_safepoint;
                    next_safepoint = next_safepoint.saturating_add(1);
                    add_alloc_safepoints.insert(lowered.byte_pc, safepoint);
                    safepoint_records.push(SafepointRecord::frame_slot_window(
                        safepoint,
                        NO_FRAME_STATE,
                        view.code_block.register_count,
                    ));
                }
                _ => {}
            }
        }

        Ok(Self {
            instructions,
            enable_float_residency,
            load_property_count,
            store_property_count,
            safepoint_records,
            add_alloc_safepoints,
            method_alloc_safepoints,
        })
    }
}

/// Pack method-call argument register indices into one word.
pub(crate) fn pack_method_arg_regs(arg_regs: &[u16]) -> u64 {
    let mut packed = 0u64;
    for (slot, &areg) in arg_regs.iter().take(MAX_METHOD_ARGS).enumerate() {
        packed |= u64::from(areg) << (16 * slot);
    }
    packed
}

/// Unpack method-call argument register indices from one word.
pub(crate) fn unpack_method_arg_regs(packed: u64) -> [u16; MAX_METHOD_ARGS] {
    [
        (packed & 0xffff) as u16,
        ((packed >> 16) & 0xffff) as u16,
        ((packed >> 32) & 0xffff) as u16,
        ((packed >> 48) & 0xffff) as u16,
    ]
}

/// Byte offset of register `idx` within the register array.
pub(crate) fn reg_offset(idx: u16) -> Result<u32, Unsupported> {
    let off = u32::from(idx) * 8;
    if off > 32760 {
        return Err(Unsupported::RegisterRange(idx));
    }
    Ok(off)
}

/// Target canonical instruction PC of a relative branch.
pub(crate) fn branch_target(
    code_block: &otter_vm::CodeBlock,
    instr: &otter_vm::JitInstructionMetadata,
    rel: i32,
) -> i64 {
    i64::from(instr.instruction_pc(code_block)) + 1 + i64::from(rel)
}

pub(crate) trait WordOperands: Copy {
    fn get(self, index: usize) -> Option<Operand>;
}

impl WordOperands for otter_vm::OperandView<'_> {
    fn get(self, index: usize) -> Option<Operand> {
        self.get(index)
    }
}

impl WordOperands for &[Operand] {
    fn get(self, index: usize) -> Option<Operand> {
        <[Operand]>::get(self, index).copied()
    }
}

impl<const N: usize> WordOperands for &[Operand; N] {
    fn get(self, index: usize) -> Option<Operand> {
        self.as_slice().get(index).copied()
    }
}

impl WordOperands for BinaryOperands {
    fn get(self, index: usize) -> Option<Operand> {
        match index {
            0 => Some(Operand::Register(self.dst)),
            1 => Some(Operand::Register(self.lhs)),
            2 => Some(Operand::Register(self.rhs)),
            _ => None,
        }
    }
}

pub(crate) fn reg(operands: impl WordOperands, i: usize) -> Result<u16, Unsupported> {
    match operands.get(i) {
        Some(Operand::Register(r)) => Ok(r),
        _ => Err(Unsupported::OperandShape("expected register")),
    }
}

pub(crate) fn imm32(operands: impl WordOperands, i: usize) -> Result<i32, Unsupported> {
    match operands.get(i) {
        Some(Operand::Imm32(v)) => Ok(v),
        _ => Err(Unsupported::OperandShape("expected imm32")),
    }
}

/// Decode a local index carried by an inline immediate.
pub(crate) fn local_index(operands: impl WordOperands, i: usize) -> Result<u16, Unsupported> {
    u16::try_from(imm32(operands, i)?).map_err(|_| Unsupported::OperandShape("local index"))
}

/// Decode a constant-pool index operand.
pub(crate) fn const_index(operands: impl WordOperands, i: usize) -> Result<u32, Unsupported> {
    match operands.get(i) {
        Some(Operand::ConstIndex(n)) => Ok(n),
        _ => Err(Unsupported::OperandShape("expected const index")),
    }
}

pub(crate) fn reg3(operands: impl WordOperands) -> Result<(u16, u16, u16), Unsupported> {
    Ok((reg(operands, 0)?, reg(operands, 1)?, reg(operands, 2)?))
}
