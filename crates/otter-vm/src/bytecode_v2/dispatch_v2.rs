//! Minimal v2 bytecode interpreter harness (Phase 3a).
//!
//! This is the **smallest possible** v2 dispatch loop — just enough to
//! prove that v1 bytecode, transpiled through [`super::transpile`], runs
//! and produces the same result as the v1 interpreter. Scope covers only
//! the int32-arithmetic-loop subset:
//!
//! - accumulator + register file + pc
//! - `Lda*` / `Star` / `Mov` / `LdaSmi`
//! - all 12 binary arithmetic opcodes
//! - all unary arithmetic opcodes (no-op for `ToNumber` when already int32)
//! - the 8 comparison opcodes
//! - `JumpIf*Boolean*` / `Jump` / `Return`
//!
//! No RuntimeState, no heap, no property access, no calls, no
//! generators. Those all come in Phase 3b — the full interpreter
//! integration. The point of 3a is end-to-end round-trip validation of
//! the ISA on pure-arithmetic workloads.

use crate::value::{RegisterValue, TAG_NAN, ValueError};

use super::decoding::{DecodeError, InstructionIter};
use super::opcodes::OpcodeV2;
use super::operand::Operand;
use super::Bytecode;

/// Harness-level execution errors.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ExecError {
    #[error(transparent)]
    Decode(#[from] DecodeError),
    #[error("opcode {0:?} not supported by the Phase 3a harness")]
    Unsupported(OpcodeV2),
    #[error("register {index} out of bounds (have {len})")]
    RegisterOutOfBounds { index: u32, len: usize },
    #[error("arithmetic error: {0}")]
    Arithmetic(ValueError),
    #[error("unexpected accumulator kind at pc {pc}")]
    AccumulatorKindMismatch { pc: u32 },
    #[error("jump target out of range: pc={pc} + off={offset}")]
    JumpOutOfRange { pc: u32, offset: i32 },
    #[error("bytecode ended without Return")]
    UnterminatedBytecode,
}

impl From<ValueError> for ExecError {
    fn from(e: ValueError) -> Self {
        ExecError::Arithmetic(e)
    }
}

/// Interpreter frame for Phase 3a. One frame = one function call; no
/// nested calls yet.
pub struct Frame {
    registers: Vec<RegisterValue>,
    accumulator: RegisterValue,
    pc: u32,
}

impl Frame {
    /// Create a frame with `register_count` undefined slots and an empty
    /// accumulator.
    #[must_use]
    pub fn new(register_count: usize) -> Self {
        Self {
            registers: vec![RegisterValue::undefined(); register_count],
            accumulator: RegisterValue::undefined(),
            pc: 0,
        }
    }

    /// Install a value into a specific register slot. Used to set up
    /// function parameters before calling [`execute`].
    pub fn set_register(&mut self, index: usize, value: RegisterValue) {
        self.registers[index] = value;
    }

    #[must_use]
    pub fn accumulator(&self) -> RegisterValue {
        self.accumulator
    }
}

/// Execute a v2 bytecode stream on the given frame. Returns the value
/// left in the accumulator when `Return` is reached.
pub fn execute(bc: &Bytecode, frame: &mut Frame) -> Result<RegisterValue, ExecError> {
    let bytes = bc.bytes();
    let mut iter = InstructionIter::new(bytes);

    loop {
        iter.seek(frame.pc);
        let Some(decoded) = iter.next() else {
            return Err(ExecError::UnterminatedBytecode);
        };
        let instr = decoded?;
        let next_pc = instr.end_pc;

        match instr.opcode {
            // --- loads / stores / move ---
            OpcodeV2::Ldar => {
                let r = reg_operand(&instr.operands)?;
                frame.accumulator = read_reg(&frame.registers, r)?;
            }
            OpcodeV2::Star => {
                let r = reg_operand(&instr.operands)?;
                write_reg(&mut frame.registers, r, frame.accumulator)?;
            }
            OpcodeV2::Mov => {
                let (src, dst) = two_regs(&instr.operands)?;
                let v = read_reg(&frame.registers, src)?;
                write_reg(&mut frame.registers, dst, v)?;
            }
            OpcodeV2::LdaSmi => {
                let imm = imm_operand(&instr.operands)?;
                frame.accumulator = RegisterValue::from_i32(imm);
            }
            OpcodeV2::LdaUndefined => frame.accumulator = RegisterValue::undefined(),
            OpcodeV2::LdaTrue => frame.accumulator = RegisterValue::from_bool(true),
            OpcodeV2::LdaFalse => frame.accumulator = RegisterValue::from_bool(false),
            OpcodeV2::LdaNull => frame.accumulator = RegisterValue::null(),
            OpcodeV2::LdaNaN => {
                frame.accumulator = RegisterValue::from_raw_bits(TAG_NAN)
                    .expect("TAG_NAN is always a valid RegisterValue bit pattern");
            }
            OpcodeV2::LdaTheHole => frame.accumulator = RegisterValue::hole(),

            // --- binary arithmetic (int32-only harness) ---
            OpcodeV2::Add => bin_op(frame, &instr.operands, RegisterValue::add_i32)?,
            OpcodeV2::Sub => bin_op(frame, &instr.operands, RegisterValue::sub_i32)?,
            OpcodeV2::Mul => bin_op(frame, &instr.operands, RegisterValue::mul_i32)?,
            OpcodeV2::BitwiseAnd => bin_op(frame, &instr.operands, int_bitand)?,
            OpcodeV2::BitwiseOr => bin_op(frame, &instr.operands, int_bitor)?,
            OpcodeV2::BitwiseXor => bin_op(frame, &instr.operands, int_bitxor)?,
            OpcodeV2::Shl => bin_op(frame, &instr.operands, int_shl)?,
            OpcodeV2::Shr => bin_op(frame, &instr.operands, int_shr)?,
            OpcodeV2::UShr => bin_op(frame, &instr.operands, int_ushr)?,

            // --- Smi-immediate fast paths ---
            OpcodeV2::AddSmi => smi_op(frame, &instr.operands, |a, b| {
                Ok(RegisterValue::from_i32(a.wrapping_add(b)))
            })?,
            OpcodeV2::BitwiseOrSmi => smi_op(frame, &instr.operands, |a, b| {
                Ok(RegisterValue::from_i32(a | b))
            })?,
            OpcodeV2::BitwiseAndSmi => smi_op(frame, &instr.operands, |a, b| {
                Ok(RegisterValue::from_i32(a & b))
            })?,

            // --- comparisons (strict eq / ordered) ---
            OpcodeV2::TestEqualStrict => cmp_op(frame, &instr.operands, |a, b| a == b)?,
            OpcodeV2::TestLessThan => cmp_op(frame, &instr.operands, |a, b| a < b)?,
            OpcodeV2::TestGreaterThan => cmp_op(frame, &instr.operands, |a, b| a > b)?,
            OpcodeV2::TestLessThanOrEqual => cmp_op(frame, &instr.operands, |a, b| a <= b)?,
            OpcodeV2::TestGreaterThanOrEqual => {
                cmp_op(frame, &instr.operands, |a, b| a >= b)?
            }

            // --- jumps ---
            OpcodeV2::Jump => {
                let off = jump_offset(&instr.operands)?;
                let target = resolve_jump(next_pc, off)?;
                frame.pc = target;
                continue;
            }
            OpcodeV2::JumpIfToBooleanTrue => {
                let off = jump_offset(&instr.operands)?;
                if frame.accumulator.is_truthy() {
                    frame.pc = resolve_jump(next_pc, off)?;
                    continue;
                }
            }
            OpcodeV2::JumpIfToBooleanFalse => {
                let off = jump_offset(&instr.operands)?;
                if !frame.accumulator.is_truthy() {
                    frame.pc = resolve_jump(next_pc, off)?;
                    continue;
                }
            }
            OpcodeV2::JumpIfTrue => {
                let off = jump_offset(&instr.operands)?;
                // Strict truthiness requires the acc to be literally the
                // boolean `true` — matches V8's JumpIfTrue semantics.
                if frame.accumulator.as_bool() == Some(true) {
                    frame.pc = resolve_jump(next_pc, off)?;
                    continue;
                }
            }
            OpcodeV2::JumpIfFalse => {
                let off = jump_offset(&instr.operands)?;
                if frame.accumulator.as_bool() == Some(false) {
                    frame.pc = resolve_jump(next_pc, off)?;
                    continue;
                }
            }

            // --- terminator ---
            OpcodeV2::Return => return Ok(frame.accumulator),
            OpcodeV2::Nop => {}

            other => return Err(ExecError::Unsupported(other)),
        }

        frame.pc = next_pc;
    }
}

// ---------------- operand helpers ----------------

fn reg_operand(ops: &[Operand]) -> Result<u32, ExecError> {
    match ops.first() {
        Some(Operand::Reg(r)) => Ok(*r),
        _ => Err(ExecError::Unsupported(OpcodeV2::Abort)),
    }
}

fn two_regs(ops: &[Operand]) -> Result<(u32, u32), ExecError> {
    match (ops.first(), ops.get(1)) {
        (Some(Operand::Reg(a)), Some(Operand::Reg(b))) => Ok((*a, *b)),
        _ => Err(ExecError::Unsupported(OpcodeV2::Abort)),
    }
}

fn imm_operand(ops: &[Operand]) -> Result<i32, ExecError> {
    match ops.first() {
        Some(Operand::Imm(v)) => Ok(*v),
        _ => Err(ExecError::Unsupported(OpcodeV2::Abort)),
    }
}

fn jump_offset(ops: &[Operand]) -> Result<i32, ExecError> {
    match ops.first() {
        Some(Operand::JumpOff(v)) => Ok(*v),
        _ => Err(ExecError::Unsupported(OpcodeV2::Abort)),
    }
}

fn read_reg(regs: &[RegisterValue], index: u32) -> Result<RegisterValue, ExecError> {
    regs.get(index as usize)
        .copied()
        .ok_or(ExecError::RegisterOutOfBounds {
            index,
            len: regs.len(),
        })
}

fn write_reg(regs: &mut [RegisterValue], index: u32, v: RegisterValue) -> Result<(), ExecError> {
    regs.get_mut(index as usize)
        .map(|slot| *slot = v)
        .ok_or(ExecError::RegisterOutOfBounds {
            index,
            len: regs.len(),
        })
}

fn resolve_jump(base: u32, offset: i32) -> Result<u32, ExecError> {
    let target = i64::from(base) + i64::from(offset);
    u32::try_from(target).map_err(|_| ExecError::JumpOutOfRange {
        pc: base,
        offset,
    })
}

// ---------------- op-specific helpers ----------------

fn bin_op(
    frame: &mut Frame,
    operands: &[Operand],
    op: fn(RegisterValue, RegisterValue) -> Result<RegisterValue, ValueError>,
) -> Result<(), ExecError> {
    let r = reg_operand(operands)?;
    let rhs = read_reg(&frame.registers, r)?;
    let lhs = frame.accumulator;
    frame.accumulator = op(lhs, rhs)?;
    Ok(())
}

fn smi_op(
    frame: &mut Frame,
    operands: &[Operand],
    op: fn(i32, i32) -> Result<RegisterValue, ValueError>,
) -> Result<(), ExecError> {
    let imm = imm_operand(operands)?;
    let a = frame.accumulator.as_i32().ok_or(ValueError::ExpectedI32)?;
    frame.accumulator = op(a, imm)?;
    Ok(())
}

fn cmp_op(
    frame: &mut Frame,
    operands: &[Operand],
    cmp: fn(i32, i32) -> bool,
) -> Result<(), ExecError> {
    let r = reg_operand(operands)?;
    let rhs = read_reg(&frame.registers, r)?;
    let l = frame.accumulator.as_i32().ok_or(ValueError::ExpectedI32)?;
    let r = rhs.as_i32().ok_or(ValueError::ExpectedI32)?;
    frame.accumulator = RegisterValue::from_bool(cmp(l, r));
    Ok(())
}

fn int_bitand(a: RegisterValue, b: RegisterValue) -> Result<RegisterValue, ValueError> {
    let l = a.as_i32().ok_or(ValueError::ExpectedI32)?;
    let r = b.as_i32().ok_or(ValueError::ExpectedI32)?;
    Ok(RegisterValue::from_i32(l & r))
}

fn int_bitor(a: RegisterValue, b: RegisterValue) -> Result<RegisterValue, ValueError> {
    let l = a.as_i32().ok_or(ValueError::ExpectedI32)?;
    let r = b.as_i32().ok_or(ValueError::ExpectedI32)?;
    Ok(RegisterValue::from_i32(l | r))
}

fn int_bitxor(a: RegisterValue, b: RegisterValue) -> Result<RegisterValue, ValueError> {
    let l = a.as_i32().ok_or(ValueError::ExpectedI32)?;
    let r = b.as_i32().ok_or(ValueError::ExpectedI32)?;
    Ok(RegisterValue::from_i32(l ^ r))
}

fn int_shl(a: RegisterValue, b: RegisterValue) -> Result<RegisterValue, ValueError> {
    let l = a.as_i32().ok_or(ValueError::ExpectedI32)?;
    let r = b.as_i32().ok_or(ValueError::ExpectedI32)?;
    // JS §13.9.2: shift count masked to low 5 bits.
    Ok(RegisterValue::from_i32(l.wrapping_shl((r as u32) & 0x1F)))
}

fn int_shr(a: RegisterValue, b: RegisterValue) -> Result<RegisterValue, ValueError> {
    let l = a.as_i32().ok_or(ValueError::ExpectedI32)?;
    let r = b.as_i32().ok_or(ValueError::ExpectedI32)?;
    Ok(RegisterValue::from_i32(l.wrapping_shr((r as u32) & 0x1F)))
}

fn int_ushr(a: RegisterValue, b: RegisterValue) -> Result<RegisterValue, ValueError> {
    let l = a.as_i32().ok_or(ValueError::ExpectedI32)? as u32;
    let r = b.as_i32().ok_or(ValueError::ExpectedI32)? as u32;
    Ok(RegisterValue::from_i32((l.wrapping_shr(r & 0x1F)) as i32))
}
