//! Bytecode v2 decoder. Turns a byte stream back into structured
//! [`Instruction`] records via [`InstructionIter`].
//!
//! The decoder is an iterator over instruction boundaries: on each
//! `next()` it parses the optional prefix byte, reads the opcode, then
//! reads each operand slot at the active width. This is the shape
//! Phase 3's interpreter dispatch will walk; the analyzer and
//! disassembler also use it.

use super::opcodes::{OpcodeV2, OperandShape};
use super::operand::{Operand, OperandKind, OperandWidth};
use super::{PREFIX_EXTRA_WIDE, PREFIX_WIDE};

/// One decoded instruction: opcode, operand width used to encode it, and
/// the list of operands in source order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Instruction {
    /// Byte offset where this instruction's prefix (or opcode, if no
    /// prefix) starts.
    pub start_pc: u32,
    /// Byte offset of the first byte *after* this instruction. Jump
    /// offsets are measured from here.
    pub end_pc: u32,
    /// The opcode byte.
    pub opcode: OpcodeV2,
    /// Width used for this instruction's operands.
    pub width: OperandWidth,
    /// Operands in positional order. `RegList` operands appear as a
    /// single `Operand::RegList { base, count }` even though they occupy
    /// two byte-slots on the wire.
    pub operands: Vec<Operand>,
}

impl Instruction {
    /// Raw opcode byte + operand bytes length in the stream (excludes
    /// the prefix byte if any).
    #[must_use]
    pub fn body_len(&self) -> usize {
        1 + (self.width.bytes_per_operand()
            * self.operands.iter().map(Operand::slot_count).sum::<usize>())
    }
}

/// Decoder errors surfaced from a bytecode stream. The byte stream is
/// normally produced by the trusted encoder; any error here implies
/// corruption or a bug in a producer.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
    /// Encountered a byte that is neither a valid opcode nor a prefix.
    #[error("byte 0x{byte:02x} at pc {pc} is not a valid opcode or prefix")]
    UnknownOpcode { pc: u32, byte: u8 },
    /// Stream ended mid-instruction.
    #[error("truncated instruction at pc {pc}: need {needed} more bytes, have {have}")]
    Truncated { pc: u32, needed: usize, have: usize },
    /// Two prefix bytes in a row.
    #[error("repeated prefix byte at pc {pc}")]
    DoublePrefix { pc: u32 },
}

/// Iterator over a decoded bytecode stream. Call `.next()` to get the
/// next [`Instruction`] or a [`DecodeError`]; `None` means normal end.
pub struct InstructionIter<'a> {
    bytes: &'a [u8],
    pc: usize,
}

impl<'a> InstructionIter<'a> {
    #[must_use]
    pub fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pc: 0 }
    }

    /// Current byte cursor.
    #[must_use]
    pub fn pc(&self) -> u32 {
        self.pc as u32
    }

    /// Reset the cursor to `pc`. Used by the interpreter for jumps.
    pub fn seek(&mut self, pc: u32) {
        self.pc = pc as usize;
    }

    /// Remaining bytes from the current cursor.
    #[must_use]
    pub fn remaining(&self) -> &[u8] {
        &self.bytes[self.pc..]
    }
}

impl<'a> Iterator for InstructionIter<'a> {
    type Item = Result<Instruction, DecodeError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.pc >= self.bytes.len() {
            return None;
        }
        let start_pc = self.pc as u32;
        let (width, after_prefix) = match decode_prefix(self.bytes, self.pc) {
            Ok(result) => result,
            Err(e) => return Some(Err(e)),
        };
        self.pc = after_prefix;
        if self.pc >= self.bytes.len() {
            return Some(Err(DecodeError::Truncated {
                pc: start_pc,
                needed: 1,
                have: 0,
            }));
        }
        let opcode_byte = self.bytes[self.pc];
        let Some(opcode) = OpcodeV2::from_byte(opcode_byte) else {
            return Some(Err(DecodeError::UnknownOpcode {
                pc: self.pc as u32,
                byte: opcode_byte,
            }));
        };
        self.pc += 1;
        let shape = opcode.shape();
        match decode_operands(self.bytes, self.pc, shape, width) {
            Ok((operands, after)) => {
                self.pc = after;
                Some(Ok(Instruction {
                    start_pc,
                    end_pc: self.pc as u32,
                    opcode,
                    width,
                    operands,
                }))
            }
            Err(e) => Some(Err(e)),
        }
    }
}

fn decode_prefix(
    bytes: &[u8],
    pc: usize,
) -> Result<(OperandWidth, usize), DecodeError> {
    match bytes[pc] {
        PREFIX_WIDE => {
            let next_pc = pc + 1;
            if next_pc < bytes.len()
                && (bytes[next_pc] == PREFIX_WIDE || bytes[next_pc] == PREFIX_EXTRA_WIDE)
            {
                return Err(DecodeError::DoublePrefix { pc: pc as u32 });
            }
            Ok((OperandWidth::Wide, next_pc))
        }
        PREFIX_EXTRA_WIDE => {
            let next_pc = pc + 1;
            if next_pc < bytes.len()
                && (bytes[next_pc] == PREFIX_WIDE || bytes[next_pc] == PREFIX_EXTRA_WIDE)
            {
                return Err(DecodeError::DoublePrefix { pc: pc as u32 });
            }
            Ok((OperandWidth::ExtraWide, next_pc))
        }
        _ => Ok((OperandWidth::Narrow, pc)),
    }
}

fn decode_operands(
    bytes: &[u8],
    start: usize,
    shape: OperandShape,
    width: OperandWidth,
) -> Result<(Vec<Operand>, usize), DecodeError> {
    let per = width.bytes_per_operand();
    // Wire slot count: RegList occupies two slots, every other kind one.
    let wire_slots: usize = shape
        .operands()
        .iter()
        .map(|k| if matches!(k, OperandKind::RegList) { 2 } else { 1 })
        .sum();
    let needed = per * wire_slots;
    let available = bytes.len().saturating_sub(start);
    if available < needed {
        return Err(DecodeError::Truncated {
            pc: start as u32,
            needed,
            have: available,
        });
    }
    let mut operands = Vec::with_capacity(shape.operands().len());
    let mut cursor = start;
    for kind in shape.operands() {
        match kind {
            OperandKind::Reg => {
                operands.push(Operand::Reg(read_unsigned(bytes, cursor, width)));
                cursor += per;
            }
            OperandKind::Idx => {
                operands.push(Operand::Idx(read_unsigned(bytes, cursor, width)));
                cursor += per;
            }
            OperandKind::Imm => {
                operands.push(Operand::Imm(read_signed(bytes, cursor, width)));
                cursor += per;
            }
            OperandKind::JumpOff => {
                operands.push(Operand::JumpOff(read_signed(bytes, cursor, width)));
                cursor += per;
            }
            OperandKind::RegList => {
                let base = read_unsigned(bytes, cursor, width);
                cursor += per;
                let count = read_unsigned(bytes, cursor, width);
                cursor += per;
                operands.push(Operand::RegList { base, count });
            }
        }
    }
    Ok((operands, cursor))
}

fn read_unsigned(bytes: &[u8], at: usize, width: OperandWidth) -> u32 {
    match width {
        OperandWidth::Narrow => bytes[at] as u32,
        OperandWidth::Wide => u16::from_le_bytes([bytes[at], bytes[at + 1]]) as u32,
        OperandWidth::ExtraWide => u32::from_le_bytes([
            bytes[at],
            bytes[at + 1],
            bytes[at + 2],
            bytes[at + 3],
        ]),
    }
}

fn read_signed(bytes: &[u8], at: usize, width: OperandWidth) -> i32 {
    match width {
        OperandWidth::Narrow => i32::from(bytes[at] as i8),
        OperandWidth::Wide => i32::from(i16::from_le_bytes([bytes[at], bytes[at + 1]])),
        OperandWidth::ExtraWide => i32::from_le_bytes([
            bytes[at],
            bytes[at + 1],
            bytes[at + 2],
            bytes[at + 3],
        ]),
    }
}
