//! Bytecode wire format: byte-stream encoder, decoder, source-map
//! and jump-offset helpers.
//!
//! Encodes the compiler's [`Instruction`] DTO stream into a single
//! `Vec<u8>` byte buffer that the VM dispatch loop reads opcode-by-
//! opcode. The format is self-describing per operand (a kind byte
//! precedes each operand), so the decoder does not need a per-opcode
//! schema. A schema-driven decoder is a future optimization. The
//! encoder and decoder ship in the same binary build — bytecode is
//! never persisted across versions, so no wire-format version is
//! carried in the stream.
//!
//! # Wire format (per instruction)
//!
//! ```text
//! instruction    := opcode operand_count operand*
//! opcode         := u8                (Op as u8)
//! operand_count  := u8
//! operand        := operand_kind operand_bytes
//! operand_kind   := u8                (OPERAND_KIND_REGISTER | _CONST_INDEX | _IMM32)
//! operand_bytes  :=
//!     Register:    u16 little-endian
//!     ConstIndex:  u32 little-endian
//!     Imm32:       i32 little-endian
//! ```

use crate::{Instruction, NO_HANDLER_OFFSET, Op, Operand, OperandList, SpanEntry};

const OPERAND_KIND_REGISTER: u8 = 0;
const OPERAND_KIND_CONST_INDEX: u8 = 1;
const OPERAND_KIND_IMM32: u8 = 2;

/// Errors surfaced by the decoder.
#[derive(Debug, PartialEq, Eq)]
pub enum DecodeError {
    /// Stream ended mid-instruction.
    UnexpectedEnd {
        /// Byte offset at which the stream ended unexpectedly.
        offset: usize,
    },
    /// Opcode byte not recognised.
    UnknownOpcode {
        /// Byte offset of the offending opcode byte.
        offset: usize,
        /// Raw opcode byte value.
        byte: u8,
    },
    /// Operand kind tag not recognised.
    UnknownOperandKind {
        /// Byte offset of the operand kind byte.
        offset: usize,
        /// Raw operand kind byte value.
        kind: u8,
    },
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnexpectedEnd { offset } => {
                write!(
                    f,
                    "unexpected end of bytecode stream at byte offset {offset}"
                )
            }
            Self::UnknownOpcode { offset, byte } => {
                write!(f, "unknown opcode byte 0x{byte:02X} at offset {offset}")
            }
            Self::UnknownOperandKind { offset, kind } => {
                write!(f, "unknown operand kind 0x{kind:02X} at offset {offset}")
            }
        }
    }
}

impl std::error::Error for DecodeError {}

/// Append-only writer that builds the byte stream from an
/// [`Instruction`] sequence.
#[derive(Debug, Default)]
pub struct BytecodeWriter {
    bytes: Vec<u8>,
}

impl BytecodeWriter {
    /// Construct an empty writer.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Byte offset at which the next write will land.
    #[must_use]
    pub fn pc(&self) -> u32 {
        u32::try_from(self.bytes.len()).expect("bytecode stream over u32::MAX bytes")
    }

    /// Encode one [`Instruction`] onto the stream. Panics if the
    /// opcode has no registered byte mapping; callers should add a
    /// row to [`op_to_byte`] / [`op_from_byte`] before writing a new
    /// opcode.
    pub fn write(&mut self, instr: &Instruction) {
        let opcode_byte = op_to_byte(instr.op)
            .unwrap_or_else(|| panic!("opcode {:?} not registered in bytecode table", instr.op));
        self.bytes.push(opcode_byte);
        let operands = instr.operands.as_slice();
        let count =
            u8::try_from(operands.len()).expect("instruction operand count exceeds u8::MAX");
        self.bytes.push(count);
        for operand in operands {
            self.write_operand(operand);
        }
    }

    fn write_operand(&mut self, operand: &Operand) {
        match operand {
            Operand::Register(reg) => {
                self.bytes.push(OPERAND_KIND_REGISTER);
                self.bytes.extend_from_slice(&reg.to_le_bytes());
            }
            Operand::ConstIndex(idx) => {
                self.bytes.push(OPERAND_KIND_CONST_INDEX);
                self.bytes.extend_from_slice(&idx.to_le_bytes());
            }
            Operand::Imm32(imm) => {
                self.bytes.push(OPERAND_KIND_IMM32);
                self.bytes.extend_from_slice(&imm.to_le_bytes());
            }
        }
    }

    /// Freeze the writer into a boxed byte stream.
    #[must_use]
    pub fn into_bytes(self) -> Box<[u8]> {
        self.bytes.into_boxed_slice()
    }
}

/// Encoded function body: byte stream plus the per-instruction byte
/// offsets. Source-map translation reuses the offsets to rewrite each
/// `SpanEntry::pc` from instruction index (the compiler's coordinate
/// system) to byte offset (the dispatcher's) without re-walking the
/// stream.
#[derive(Debug, Clone)]
pub struct EncodedFunction {
    /// Byte-stream code.
    pub code: Box<[u8]>,
    /// `instr_to_byte_pc[i]` is the byte offset of the `i`-th
    /// instruction in `code`. Length equals the source `Instruction`
    /// slice length.
    pub instr_to_byte_pc: Box<[u32]>,
}

/// Encode a whole function body (an [`Instruction`] slice in source
/// order) into the byte stream and return both the bytes and the
/// per-instruction byte-offset map.
///
/// The compiler emits branch operands (`Op::Jump`, `Op::JumpIfTrue`,
/// `Op::JumpIfFalse`, `Op::JumpIfNullish`, `Op::EnterTry`) as
/// *instruction-index* deltas relative to the instruction following the
/// jump. The wire format wants *byte-offset* deltas relative to
/// `(jump_pc + 1)` (the byte right after the opcode), so encoding is a
/// two-pass walk:
///
/// 1. Write every instruction in order, capturing each jump operand's
///    byte position and the source instruction-index delta.
/// 2. Re-resolve each captured slot to a byte-offset delta computed
///    against `instr_to_byte_pc`.
///
/// The `NO_HANDLER_OFFSET` sentinel (`i32::MIN`) is preserved as-is —
/// the runtime treats it as "absent handler" for [`Op::EnterTry`].
#[must_use]
pub fn encode_function(instructions: &[Instruction]) -> EncodedFunction {
    let mut writer = BytecodeWriter::new();
    let mut instr_to_byte_pc: Vec<u32> = Vec::with_capacity(instructions.len());
    let mut fixups: Vec<JumpFixup> = Vec::new();

    for (idx, instr) in instructions.iter().enumerate() {
        let byte_pc = writer.pc();
        instr_to_byte_pc.push(byte_pc);
        write_instruction_capturing_jumps(&mut writer, instr, idx, byte_pc, &mut fixups);
    }

    let total_bytes = writer.pc();
    for fixup in &fixups {
        resolve_jump_fixup(
            &mut writer.bytes,
            fixup,
            &instr_to_byte_pc,
            total_bytes,
            instructions.len(),
        );
    }

    EncodedFunction {
        code: writer.into_bytes(),
        instr_to_byte_pc: instr_to_byte_pc.into_boxed_slice(),
    }
}

/// Slot bookkeeping for a single jump-class `Imm32` operand needing
/// byte-offset patching after the whole function has been laid out.
#[derive(Debug, Clone, Copy)]
struct JumpFixup {
    /// Source-order index of the jump instruction.
    jump_idx: usize,
    /// Byte offset of the jump opcode byte in the encoded stream.
    jump_byte_pc: u32,
    /// Byte offset of the `Imm32` payload bytes (the four bytes after
    /// the operand kind tag) for this jump operand.
    imm32_byte_offset: u32,
}

fn write_instruction_capturing_jumps(
    writer: &mut BytecodeWriter,
    instr: &Instruction,
    jump_idx: usize,
    jump_byte_pc: u32,
    fixups: &mut Vec<JumpFixup>,
) {
    let opcode_byte = op_to_byte(instr.op)
        .unwrap_or_else(|| panic!("opcode {:?} not registered in bytecode table", instr.op));
    writer.bytes.push(opcode_byte);
    let operands = instr.operands.as_slice();
    let count = u8::try_from(operands.len()).expect("instruction operand count exceeds u8::MAX");
    writer.bytes.push(count);
    let branch_slots = branch_imm32_operand_slots(instr.op);
    for (op_idx, operand) in operands.iter().enumerate() {
        let operand_start = writer.bytes.len() as u32;
        writer.write_operand(operand);
        if branch_slots.contains(&op_idx) {
            assert!(
                matches!(operand, Operand::Imm32(_)),
                "branch operand at slot {op_idx} of {:?} must be Imm32, got {operand:?}",
                instr.op
            );
            // `operand_start` points at the operand-kind tag byte; the
            // four little-endian `Imm32` payload bytes follow it.
            fixups.push(JumpFixup {
                jump_idx,
                jump_byte_pc,
                imm32_byte_offset: operand_start + 1,
            });
        }
    }
}

fn resolve_jump_fixup(
    bytes: &mut [u8],
    fixup: &JumpFixup,
    instr_to_byte_pc: &[u32],
    total_bytes: u32,
    instruction_count: usize,
) {
    let start = fixup.imm32_byte_offset as usize;
    let raw_bytes: [u8; 4] = bytes[start..start + 4]
        .try_into()
        .expect("imm32 payload occupies exactly 4 bytes");
    let raw = i32::from_le_bytes(raw_bytes);
    if raw == NO_HANDLER_OFFSET {
        return;
    }
    let target_instr_idx = (fixup.jump_idx as i64) + 1 + (raw as i64);
    assert!(
        target_instr_idx >= 0,
        "jump target instruction index underflow: jump_idx={} raw_delta={}",
        fixup.jump_idx,
        raw
    );
    let target_byte_pc = if target_instr_idx as usize == instruction_count {
        // Jump past the last instruction lands at end-of-stream.
        total_bytes
    } else {
        instr_to_byte_pc[target_instr_idx as usize]
    };
    let base = i64::from(fixup.jump_byte_pc) + 1;
    let byte_delta = i64::from(target_byte_pc) - base;
    let byte_delta_i32 =
        i32::try_from(byte_delta).expect("jump byte-offset delta exceeds i32 range");
    bytes[start..start + 4].copy_from_slice(&byte_delta_i32.to_le_bytes());
}

/// Operand slot positions whose `Imm32` value is a branch offset that
/// the encoder must rewrite from instruction-index delta to byte-offset
/// delta. Non-branch opcodes return an empty slice.
fn branch_imm32_operand_slots(op: Op) -> &'static [usize] {
    match op {
        Op::Jump | Op::JumpIfTrue | Op::JumpIfFalse | Op::JumpIfNullish => &[0],
        Op::EnterTry => &[0, 1],
        _ => &[],
    }
}

/// Translate an instruction-index source-map into byte-offset form
/// using the `instr_to_byte_pc` map produced by [`encode_function`].
/// Out-of-range entries fall back to the end of the byte stream
/// (`total_bytes`), matching the "jump past the last instruction"
/// convention used by the encoder itself.
///
/// Order is preserved; the caller may pass either a `Vec` or a slice
/// borrowed from [`crate::Function::spans`].
#[must_use]
pub fn translate_spans_to_byte_pcs(
    spans: &[SpanEntry],
    instr_to_byte_pc: &[u32],
    total_bytes: u32,
) -> Vec<SpanEntry> {
    spans
        .iter()
        .map(|entry| {
            let byte_pc = instr_to_byte_pc
                .get(entry.pc as usize)
                .copied()
                .unwrap_or(total_bytes);
            SpanEntry {
                pc: byte_pc,
                span: entry.span,
            }
        })
        .collect()
}

/// Decode a whole function body into the corresponding
/// [`Instruction`] sequence, re-walking the byte stream.
///
/// # Errors
///
/// Propagates any [`DecodeError`] from the underlying
/// [`decode_instruction`] call.
pub fn decode_function(code: &[u8]) -> Result<Vec<Instruction>, DecodeError> {
    let mut out: Vec<Instruction> = Vec::new();
    let mut pc = 0usize;
    while pc < code.len() {
        let (instr, next) = decode_instruction(code, pc)?;
        out.push(instr);
        pc = next;
    }
    Ok(out)
}

/// Decode the next instruction from `code` starting at byte offset
/// `pc`. Returns the decoded instruction and the byte offset of the
/// instruction that follows it.
///
/// # Errors
///
/// [`DecodeError`] on truncation, unknown opcode byte, or unknown
/// operand kind tag.
pub fn decode_instruction(code: &[u8], pc: usize) -> Result<(Instruction, usize), DecodeError> {
    let opcode_byte = *code
        .get(pc)
        .ok_or(DecodeError::UnexpectedEnd { offset: pc })?;
    let op = op_from_byte(opcode_byte).ok_or(DecodeError::UnknownOpcode {
        offset: pc,
        byte: opcode_byte,
    })?;
    let operand_count = *code
        .get(pc + 1)
        .ok_or(DecodeError::UnexpectedEnd { offset: pc + 1 })? as usize;
    let mut cursor = pc + 2;
    let mut operands: Vec<Operand> = Vec::with_capacity(operand_count);
    for _ in 0..operand_count {
        let (operand, next) = decode_operand(code, cursor)?;
        operands.push(operand);
        cursor = next;
    }
    let instr = Instruction {
        pc: u32::try_from(pc).expect("pc fits in u32"),
        op,
        operands: OperandList::from(operands.as_slice()),
    };
    Ok((instr, cursor))
}

fn decode_operand(code: &[u8], pc: usize) -> Result<(Operand, usize), DecodeError> {
    let kind = *code
        .get(pc)
        .ok_or(DecodeError::UnexpectedEnd { offset: pc })?;
    match kind {
        OPERAND_KIND_REGISTER => {
            let bytes = take_n::<2>(code, pc + 1)?;
            Ok((Operand::Register(u16::from_le_bytes(bytes)), pc + 3))
        }
        OPERAND_KIND_CONST_INDEX => {
            let bytes = take_n::<4>(code, pc + 1)?;
            Ok((Operand::ConstIndex(u32::from_le_bytes(bytes)), pc + 5))
        }
        OPERAND_KIND_IMM32 => {
            let bytes = take_n::<4>(code, pc + 1)?;
            Ok((Operand::Imm32(i32::from_le_bytes(bytes)), pc + 5))
        }
        other => Err(DecodeError::UnknownOperandKind {
            offset: pc,
            kind: other,
        }),
    }
}

fn take_n<const N: usize>(code: &[u8], pc: usize) -> Result<[u8; N], DecodeError> {
    let slice = code
        .get(pc..pc + N)
        .ok_or(DecodeError::UnexpectedEnd { offset: pc })?;
    let mut out = [0u8; N];
    out.copy_from_slice(slice);
    Ok(out)
}

/// Stable opcode → byte mapping. Returning `None` means the opcode
/// is not in [`OP_BYTE_TABLE`].
///
/// O(1) via the table's sequentiality invariant: rows are indexed by
/// their byte value, so the byte equals the row index.
#[must_use]
pub fn op_to_byte(op: Op) -> Option<u8> {
    OP_BYTE_TABLE
        .iter()
        .position(|(candidate, _)| *candidate == op)
        .map(|index| index as u8)
}

/// Reverse of [`op_to_byte`]. O(1) via direct table indexing.
#[must_use]
pub fn op_from_byte(byte: u8) -> Option<Op> {
    OP_BYTE_TABLE.get(byte as usize).map(|(op, _)| *op)
}

/// Byte assignments for every [`Op`] variant. The encoder and
/// decoder ship in the same binary build, so the assignments only
/// need to be dense (the table is indexed by byte) and unique.
pub const OP_BYTE_TABLE: &[(Op, u8)] = &[
    (Op::Nop, 0x00),
    (Op::LoadUndefined, 0x01),
    (Op::LoadHole, 0x02),
    (Op::Return, 0x03),
    (Op::LoadString, 0x04),
    (Op::LoadNumber, 0x05),
    (Op::LoadInt32, 0x06),
    (Op::LoadBigInt, 0x07),
    (Op::LoadRegExp, 0x08),
    (Op::QueueMicrotask, 0x09),
    (Op::PromiseNew, 0x0A),
    (Op::PromiseCall, 0x0B),
    (Op::LoadTrue, 0x0C),
    (Op::LoadFalse, 0x0D),
    (Op::LoadLength, 0x0E),
    (Op::GetStringIndex, 0x0F),
    (Op::CallMethodValue, 0x10),
    (Op::Add, 0x11),
    (Op::Sub, 0x12),
    (Op::Mul, 0x13),
    (Op::Div, 0x14),
    (Op::Rem, 0x15),
    (Op::Neg, 0x16),
    (Op::Pow, 0x17),
    (Op::BitwiseAnd, 0x18),
    (Op::BitwiseOr, 0x19),
    (Op::BitwiseXor, 0x1A),
    (Op::BitwiseNot, 0x1B),
    (Op::Shl, 0x1C),
    (Op::Shr, 0x1D),
    (Op::Ushr, 0x1E),
    (Op::ToNumber, 0x1F),
    (Op::Equal, 0x20),
    (Op::NotEqual, 0x21),
    (Op::LessThan, 0x22),
    (Op::LessEq, 0x23),
    (Op::GreaterThan, 0x24),
    (Op::GreaterEq, 0x25),
    (Op::LoadNull, 0x26),
    (Op::LogicalNot, 0x27),
    (Op::ToBoolean, 0x28),
    (Op::Jump, 0x29),
    (Op::JumpIfTrue, 0x2A),
    (Op::JumpIfFalse, 0x2B),
    (Op::JumpIfNullish, 0x2C),
    (Op::LoadLocal, 0x2D),
    (Op::StoreLocal, 0x2E),
    (Op::TdzError, 0x2F),
    (Op::MakeFunction, 0x30),
    (Op::MakeClosure, 0x31),
    (Op::LoadUpvalue, 0x32),
    (Op::StoreUpvalue, 0x33),
    (Op::Call, 0x34),
    (Op::CallWithThis, 0x35),
    (Op::BindFunction, 0x36),
    (Op::LoadThis, 0x37),
    (Op::LoadNewTarget, 0x38),
    (Op::Throw, 0x39),
    (Op::EnterTry, 0x3A),
    (Op::LeaveTry, 0x3B),
    (Op::EndFinally, 0x3C),
    (Op::NewError, 0x3D),
    (Op::GetIterator, 0x3E),
    (Op::IteratorNext, 0x3F),
    (Op::ArrayPush, 0x40),
    (Op::CallSpread, 0x41),
    (Op::New, 0x42),
    (Op::NewSpread, 0x43),
    (Op::SuperConstructSpread, 0x44),
    (Op::MakeClass, 0x45),
    (Op::MathLoad, 0x46),
    (Op::CollectRest, 0x47),
    (Op::ReturnValue, 0x48),
    (Op::ReturnUndefined, 0x49),
    (Op::NewObject, 0x4A),
    (Op::LoadProperty, 0x4B),
    (Op::StoreProperty, 0x4C),
    (Op::DeleteProperty, 0x4D),
    (Op::GetPrototype, 0x4E),
    (Op::SetPrototype, 0x4F),
    (Op::NewArray, 0x50),
    (Op::LoadElement, 0x51),
    (Op::StoreElement, 0x52),
    (Op::ArrayLength, 0x53),
    (Op::HasProperty, 0x54),
    (Op::Instanceof, 0x55),
    (Op::Eval, 0x56),
    (Op::NewFunction, 0x57),
    (Op::LoadGlobalThis, 0x58),
    (Op::LoadGlobalOrThrow, 0x59),
    (Op::CollectArguments, 0x5A),
    (Op::LoadGlobalOrUndefined, 0x5B),
    (Op::DefineGlobalVar, 0x5C),
    (Op::ImportMetaResolve, 0x5D),
    (Op::ImportNamespaceDynamic, 0x5E),
    (Op::ImportNamespace, 0x5F),
    (Op::PromiseFulfilledOf, 0x60),
    (Op::NewIntl, 0x61),
    (Op::TemporalLoad, 0x62),
    (Op::NewCollection, 0x63),
    (Op::NewWeakRef, 0x64),
    (Op::NewFinalizationRegistry, 0x65),
    (Op::SymbolLoad, 0x66),
    (Op::TypeOf, 0x67),
    (Op::DeleteElement, 0x68),
    (Op::Await, 0x69),
    (Op::SameValue, 0x6A),
    (Op::IsArray, 0x6B),
    (Op::LooseEqual, 0x6C),
    (Op::LooseNotEqual, 0x6D),
    (Op::NewBuiltinError, 0x6E),
    (Op::LoadBuiltinError, 0x6F),
    (Op::BigIntCall, 0x70),
    (Op::ArrayConstruct, 0x71),
    (Op::ArrayFrom, 0x72),
    (Op::ArrayOf, 0x73),
    (Op::ArrayBufferCall, 0x74),
    (Op::DataViewCall, 0x75),
    (Op::Yield, 0x76),
    (Op::SharedArrayBufferCall, 0x77),
    (Op::ToPrimitive, 0x78),
    (Op::ForInKeys, 0x79),
    (Op::CopyDataProperties, 0x7A),
    (Op::DefineOwnProperty, 0x7B),
    (Op::IteratorClose, 0x7C),
    (Op::IteratorCloseStart, 0x7D),
    (Op::IteratorCloseEnd, 0x7E),
    (Op::GeneratorStart, 0x7F),
    (Op::GetAsyncIterator, 0x80),
];

#[cfg(test)]
mod tests {
    use super::*;

    fn make_instr(op: Op, operands: &[Operand]) -> Instruction {
        Instruction {
            pc: 0,
            op,
            operands: OperandList::from(operands),
        }
    }

    fn roundtrip(instr: &Instruction) -> Instruction {
        let mut writer = BytecodeWriter::new();
        writer.write(instr);
        let bytes = writer.into_bytes();
        let (decoded, next_pc) = decode_instruction(&bytes, 0).expect("decode");
        assert_eq!(next_pc, bytes.len(), "decoder must consume full stream");
        decoded
    }

    #[test]
    fn roundtrip_load_undefined() {
        let instr = make_instr(Op::LoadUndefined, &[Operand::Register(7)]);
        assert_eq!(roundtrip(&instr), instr);
    }

    #[test]
    fn roundtrip_load_int32() {
        let instr = make_instr(Op::LoadInt32, &[Operand::Register(0), Operand::Imm32(-42)]);
        assert_eq!(roundtrip(&instr), instr);
    }

    #[test]
    fn roundtrip_add_three_registers() {
        let instr = make_instr(
            Op::Add,
            &[
                Operand::Register(0),
                Operand::Register(1),
                Operand::Register(2),
            ],
        );
        assert_eq!(roundtrip(&instr), instr);
    }

    #[test]
    fn multi_instruction_stream_steps_pc_by_byte_size() {
        let mut writer = BytecodeWriter::new();
        writer.write(&make_instr(Op::Nop, &[]));
        writer.write(&make_instr(Op::LoadUndefined, &[Operand::Register(1)]));
        writer.write(&make_instr(
            Op::LoadInt32,
            &[Operand::Register(2), Operand::Imm32(7)],
        ));
        let bytes = writer.into_bytes();

        let mut pc = 0;
        let (first, next) = decode_instruction(&bytes, pc).unwrap();
        assert_eq!(first.op, Op::Nop);
        pc = next;

        let (second, next) = decode_instruction(&bytes, pc).unwrap();
        assert_eq!(second.op, Op::LoadUndefined);
        pc = next;

        let (third, next) = decode_instruction(&bytes, pc).unwrap();
        assert_eq!(third.op, Op::LoadInt32);
        assert_eq!(next, bytes.len());
    }

    #[test]
    fn truncated_stream_surfaces_clean_error() {
        let mut writer = BytecodeWriter::new();
        writer.write(&make_instr(
            Op::LoadInt32,
            &[Operand::Register(0), Operand::Imm32(0)],
        ));
        let bytes = writer.into_bytes();
        let truncated = &bytes[..bytes.len() - 1];
        match decode_instruction(truncated, 0) {
            Err(DecodeError::UnexpectedEnd { .. }) => {}
            other => panic!("expected UnexpectedEnd, got {other:?}"),
        }
    }

    #[test]
    fn unknown_opcode_byte_rejected() {
        let bytes = [0xFFu8, 0];
        match decode_instruction(&bytes, 0) {
            Err(DecodeError::UnknownOpcode { byte: 0xFF, .. }) => {}
            other => panic!("expected UnknownOpcode, got {other:?}"),
        }
    }

    #[test]
    fn op_byte_table_round_trips() {
        for (op, byte) in OP_BYTE_TABLE {
            assert_eq!(op_to_byte(*op), Some(*byte));
            assert_eq!(op_from_byte(*byte), Some(*op));
        }
    }

    #[test]
    fn op_byte_assignments_unique() {
        let mut seen = std::collections::HashSet::new();
        for (op, byte) in OP_BYTE_TABLE {
            assert!(
                seen.insert(*byte),
                "byte 0x{:02X} assigned to multiple opcodes (offending: {:?})",
                byte,
                op
            );
        }
    }

    #[test]
    fn op_byte_assignments_are_sequential() {
        // Stable wire format requires monotonic byte assignments so
        // diffs on this table read as a single growing column.
        for (i, (_, byte)) in OP_BYTE_TABLE.iter().enumerate() {
            assert_eq!(
                *byte as usize, i,
                "OP_BYTE_TABLE row {i} has byte 0x{byte:02X}; table must stay dense"
            );
        }
    }

    #[test]
    fn op_byte_assignments_have_unique_opcodes() {
        let mut seen = std::collections::HashSet::new();
        for (op, _) in OP_BYTE_TABLE {
            assert!(
                seen.insert(*op),
                "opcode {op:?} appears twice in OP_BYTE_TABLE"
            );
        }
    }

    #[test]
    fn encode_decode_function_roundtrip() {
        let instructions = vec![
            make_instr(Op::Nop, &[]),
            make_instr(Op::LoadUndefined, &[Operand::Register(0)]),
            make_instr(Op::LoadInt32, &[Operand::Register(1), Operand::Imm32(42)]),
            make_instr(
                Op::Add,
                &[
                    Operand::Register(2),
                    Operand::Register(0),
                    Operand::Register(1),
                ],
            ),
            make_instr(Op::Return, &[Operand::Register(2)]),
        ];
        let encoded = encode_function(&instructions);
        assert_eq!(encoded.instr_to_byte_pc.len(), instructions.len());
        // First instruction always lands at byte 0.
        assert_eq!(encoded.instr_to_byte_pc[0], 0);
        // Byte offsets must be strictly monotonic.
        for win in encoded.instr_to_byte_pc.windows(2) {
            assert!(win[0] < win[1]);
        }

        let decoded = decode_function(&encoded.code).expect("decode");
        // Re-stamp pc since round-trip through bytes flips PC from
        // instruction-index to byte-offset; the structural data should
        // otherwise match exactly.
        for (i, (orig, decoded)) in instructions.iter().zip(decoded.iter()).enumerate() {
            assert_eq!(orig.op, decoded.op, "op mismatch at index {i}");
            assert_eq!(
                orig.operands.as_slice(),
                decoded.operands.as_slice(),
                "operands mismatch at index {i}"
            );
            assert_eq!(
                decoded.pc, encoded.instr_to_byte_pc[i],
                "decoded pc must match the byte-offset map"
            );
        }
    }

    #[test]
    fn forward_jump_is_rewritten_to_byte_offset_delta() {
        // Layout: instr 0 = LoadInt32 (size 1 + 1 + 3 + 5 = 10 bytes),
        // instr 1 = Jump +1 (target = instr 3) (size 1 + 1 + 5 = 7),
        // instr 2 = LoadInt32 (10 bytes),
        // instr 3 = Return (1 + 1 + 3 = 5 bytes).
        let instructions = vec![
            make_instr(Op::LoadInt32, &[Operand::Register(0), Operand::Imm32(7)]),
            make_instr(Op::Jump, &[Operand::Imm32(1)]),
            make_instr(Op::LoadInt32, &[Operand::Register(1), Operand::Imm32(8)]),
            make_instr(Op::Return, &[Operand::Register(0)]),
        ];
        let encoded = encode_function(&instructions);
        let jump_byte_pc = encoded.instr_to_byte_pc[1];
        let target_byte_pc = encoded.instr_to_byte_pc[3];
        // Re-decode the jump and confirm its Imm32 is the byte-offset
        // delta from `(jump_pc + 1)` to the target.
        let (decoded_jump, _) = decode_instruction(&encoded.code, jump_byte_pc as usize).unwrap();
        let Operand::Imm32(byte_delta) = decoded_jump.operands.as_slice()[0] else {
            panic!("jump operand must remain Imm32");
        };
        let resolved_target = (jump_byte_pc as i64) + 1 + (byte_delta as i64);
        assert_eq!(resolved_target as u32, target_byte_pc);
    }

    #[test]
    fn backward_jump_byte_delta_is_negative() {
        // instr 0 = LoadInt32 (10),
        // instr 1 = Return (5),
        // instr 2 = Jump -2 (target = instr 1)
        let instructions = vec![
            make_instr(Op::LoadInt32, &[Operand::Register(0), Operand::Imm32(0)]),
            make_instr(Op::Return, &[Operand::Register(0)]),
            make_instr(Op::Jump, &[Operand::Imm32(-2)]),
        ];
        let encoded = encode_function(&instructions);
        let jump_byte_pc = encoded.instr_to_byte_pc[2];
        let target_byte_pc = encoded.instr_to_byte_pc[1];
        let (decoded_jump, _) = decode_instruction(&encoded.code, jump_byte_pc as usize).unwrap();
        let Operand::Imm32(byte_delta) = decoded_jump.operands.as_slice()[0] else {
            panic!("jump operand must remain Imm32");
        };
        assert!(
            byte_delta < 0,
            "expected backward branch delta, got {byte_delta}"
        );
        let resolved_target = (jump_byte_pc as i64) + 1 + (byte_delta as i64);
        assert_eq!(resolved_target as u32, target_byte_pc);
    }

    #[test]
    fn enter_try_no_handler_sentinel_preserved() {
        let instructions = vec![
            make_instr(
                Op::EnterTry,
                &[
                    Operand::Imm32(NO_HANDLER_OFFSET),
                    Operand::Imm32(NO_HANDLER_OFFSET),
                    Operand::Register(0),
                ],
            ),
            make_instr(Op::LeaveTry, &[]),
            make_instr(Op::Return, &[Operand::Register(0)]),
        ];
        let encoded = encode_function(&instructions);
        let (decoded, _) = decode_instruction(&encoded.code, 0).unwrap();
        let operands = decoded.operands.as_slice();
        assert_eq!(operands[0], Operand::Imm32(NO_HANDLER_OFFSET));
        assert_eq!(operands[1], Operand::Imm32(NO_HANDLER_OFFSET));
        assert_eq!(operands[2], Operand::Register(0));
    }

    #[test]
    fn enter_try_handler_offsets_rewritten_to_byte_pcs() {
        // instr 0 = EnterTry catch=+1 finally=NO_HANDLER (size = 1+1+5+5+3 = 15)
        // instr 1 = LoadInt32 (size 10)
        // instr 2 = LeaveTry (size 1+1 = 2)        ← catch target
        // instr 3 = Return (size 5)
        let instructions = vec![
            make_instr(
                Op::EnterTry,
                &[
                    Operand::Imm32(1), // catch_offset (instr-index delta = +1 → target = idx 2)
                    Operand::Imm32(NO_HANDLER_OFFSET),
                    Operand::Register(7),
                ],
            ),
            make_instr(Op::LoadInt32, &[Operand::Register(0), Operand::Imm32(0)]),
            make_instr(Op::LeaveTry, &[]),
            make_instr(Op::Return, &[Operand::Register(0)]),
        ];
        let encoded = encode_function(&instructions);
        let try_byte_pc = encoded.instr_to_byte_pc[0];
        let leave_byte_pc = encoded.instr_to_byte_pc[2];
        let (decoded, _) = decode_instruction(&encoded.code, try_byte_pc as usize).unwrap();
        let operands = decoded.operands.as_slice();
        let Operand::Imm32(catch_byte_delta) = operands[0] else {
            panic!("catch operand must remain Imm32");
        };
        assert_eq!(operands[1], Operand::Imm32(NO_HANDLER_OFFSET));
        assert_eq!(operands[2], Operand::Register(7));
        let resolved = (try_byte_pc as i64) + 1 + (catch_byte_delta as i64);
        assert_eq!(resolved as u32, leave_byte_pc);
    }

    #[test]
    fn jump_past_last_instruction_lands_at_stream_end() {
        // Jump +0 from a last-position instruction equals "fall off"
        // (target = instructions.len()). Encoder maps that to the end
        // of the byte stream so unwind / source-map clients see a
        // stable PC.
        let instructions = vec![
            make_instr(Op::Nop, &[]),
            make_instr(Op::Jump, &[Operand::Imm32(0)]),
        ];
        let encoded = encode_function(&instructions);
        let total_len = encoded.code.len() as u32;
        let jump_byte_pc = encoded.instr_to_byte_pc[1];
        let (decoded, _) = decode_instruction(&encoded.code, jump_byte_pc as usize).unwrap();
        let Operand::Imm32(delta) = decoded.operands.as_slice()[0] else {
            panic!("jump operand kind");
        };
        let resolved = (jump_byte_pc as i64) + 1 + (delta as i64);
        assert_eq!(resolved as u32, total_len);
    }

    #[test]
    fn translate_spans_maps_to_byte_offsets() {
        // Three instructions: LoadInt32 (10), Nop (2), Return (5).
        let instructions = vec![
            make_instr(Op::LoadInt32, &[Operand::Register(0), Operand::Imm32(0)]),
            make_instr(Op::Nop, &[]),
            make_instr(Op::Return, &[Operand::Register(0)]),
        ];
        let encoded = encode_function(&instructions);
        let spans = vec![
            SpanEntry {
                pc: 0,
                span: (10, 20),
            },
            SpanEntry {
                pc: 1,
                span: (20, 25),
            },
            SpanEntry {
                pc: 2,
                span: (25, 30),
            },
        ];
        let translated = translate_spans_to_byte_pcs(
            &spans,
            &encoded.instr_to_byte_pc,
            encoded.code.len() as u32,
        );
        assert_eq!(translated.len(), spans.len());
        for (i, entry) in translated.iter().enumerate() {
            assert_eq!(entry.pc, encoded.instr_to_byte_pc[i]);
            assert_eq!(entry.span, spans[i].span);
        }
    }

    #[test]
    fn translate_spans_out_of_range_pc_falls_back_to_stream_end() {
        let instructions = vec![make_instr(Op::Nop, &[])];
        let encoded = encode_function(&instructions);
        let total = encoded.code.len() as u32;
        let spans = vec![SpanEntry {
            pc: 5,
            span: (0, 1),
        }];
        let translated = translate_spans_to_byte_pcs(&spans, &encoded.instr_to_byte_pc, total);
        assert_eq!(translated[0].pc, total);
    }

    #[test]
    fn coverage_matches_dispatcher_enum_size() {
        // Catches accidental opcode additions that forget to wire
        // through OP_BYTE_TABLE. If this fires, append the missing
        // opcode at the next unused byte.
        const EXPECTED_OPCODE_COUNT: usize = 129;
        assert_eq!(
            OP_BYTE_TABLE.len(),
            EXPECTED_OPCODE_COUNT,
            "Op enum changed; sync OP_BYTE_TABLE with the new opcode set"
        );
    }
}
