//! Bytecode wire format: byte-stream encoder, decoder, source-map
//! and jump-offset helpers.
//!
//! # Contents
//! - panic-free validation for logical DTOs and authoritative wordcode
//! - [`verify_exception_handlers`] — the handler table against its code
//! - cold byte-stream encoding, decoding, and branch fixups
//! - source-map and serialized-layout helpers
//!
//! # Invariants
//! - Authoritative wordcode is validated before executable consumers inspect it.
//! - No path from the entry or from a handler target runs off the end of the
//!   code.
//! - Handler ranges nest or are disjoint, an inner range precedes the ranges
//!   enclosing it, and no handler covers its own target.
//!
//! Encodes the compiler's [`Instruction`] DTO stream into a self-describing
//! byte buffer retained for cold metadata, diagnostics, and serialization.
//! Active VM dispatch executes schema-typed CodeBlock words instead. Logical
//! instruction-index DTOs are verified directly before encoding; decoding a
//! serialized stream independently validates byte-boundary branch and handler
//! targets. Negative compiler/backend fixtures may measure canonical wordcode
//! size without validating their deliberately malformed targets. Bytecode is
//! never persisted across incompatible versions, so no wire-format version is
//! carried in the stream.
//!
//! # See also
//! - [`crate::opcode_schema`] for operand and successor authority.
//! - `otter-vm`'s `code_block_cfg` and frame handler operations for consumers.
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


use crate::{
    ExceptionHandler, FunctionCode, Instruction, Op, Operand, SpanEntry,
    opcode_schema::{
        OperandKind, OperandShapeError, RelativeTargetBase, SuccessorSpec,
        decode_operand_word, opcode_schema, operand_kind_at, verify_operand_shape,
    },
    wordcode::{INLINE_OPERAND_WORDS, Instruction as WordInstruction},
};

/// Verify authoritative execution wordcode directly in logical-PC space.
///
/// # Errors
/// Returns [`VerifyError`] for malformed/noncanonical operand storage,
/// operand-shape or branch-target violations, or a reachable function end.
pub fn verify_wordcode_function(code: &FunctionCode) -> Result<(), VerifyError> {
    let len = i64::try_from(code.len()).map_err(|_| VerifyError::FunctionTooLarge)?;
    let (instructions, overflow_operand_words) = code.raw_parts();
    let mut next_overflow_offset = 0;
    let mut decoded_operands = DecodedOperands::with_capacity(instructions.len());
    for (instruction_index, instr) in instructions.iter().enumerate() {
        let start = decoded_operands.operands.len();
        decode_wordcode_operands(
            instr,
            overflow_operand_words,
            &mut next_overflow_offset,
            instruction_index,
            &mut decoded_operands.operands,
        )?;
        let operands = &decoded_operands.operands[start..];
        verify_operand_shape(instr.op, operands).map_err(|error| {
            VerifyError::InvalidOperandShape {
                instruction_index,
                error,
            }
        })?;
        let schema = opcode_schema(instr.op);
        for successor in schema.successor_shape.exact() {
            if let SuccessorSpec::RelativeTarget { operand_index, .. } = successor {
                verify_wordcode_target(operands, instruction_index, *operand_index, None, len)?;
            }
        }
        decoded_operands.ends.push(decoded_operands.operands.len());
    }
    verify_wordcode_overflow_consumed(next_overflow_offset, overflow_operand_words.len())?;
    verify_reachable_end(instructions, &decoded_operands, [0])?;
    Ok(())
}

/// Verify a function's exception handler table against its verified code.
///
/// # Errors
/// Returns [`VerifyError`] for a range outside the code, a target outside the
/// code or inside its own range, ranges that overlap without nesting or list
/// an enclosing range first, or a path from a target that runs off the end.
pub fn verify_exception_handlers(
    code: &FunctionCode,
    handlers: &[ExceptionHandler],
) -> Result<(), VerifyError> {
    let len = u32::try_from(code.len()).map_err(|_| VerifyError::FunctionTooLarge)?;
    for (handler_index, handler) in handlers.iter().enumerate() {
        if handler.start >= handler.end || handler.end > len {
            return Err(VerifyError::InvalidHandlerRange {
                handler_index,
                start: handler.start,
                end: handler.end,
            });
        }
        if handler.target >= len || handler.covers(handler.target) {
            return Err(VerifyError::InvalidHandlerTarget {
                handler_index,
                target: handler.target,
            });
        }
    }
    // Sweep ranges by start, outer before inner: every range must sit inside
    // the innermost range still open at its start, and come before it in
    // the table.
    let mut order: Vec<usize> = (0..handlers.len()).collect();
    order.sort_by_key(|&index| {
        let handler = handlers[index];
        (
            handler.start,
            std::cmp::Reverse(handler.end),
            std::cmp::Reverse(index),
        )
    });
    let mut open: Vec<usize> = Vec::new();
    for handler_index in order {
        let handler = handlers[handler_index];
        while open
            .last()
            .is_some_and(|&outer| handlers[outer].end <= handler.start)
        {
            open.pop();
        }
        if let Some(&outer) = open.last()
            && (handler.end > handlers[outer].end || outer < handler_index)
        {
            return Err(VerifyError::HandlerNesting {
                handler_index: handler_index.max(outer),
                earlier_index: handler_index.min(outer),
            });
        }
        open.push(handler_index);
    }
    if handlers.is_empty() {
        return Ok(());
    }
    let (instructions, overflow_operand_words) = code.raw_parts();
    let mut next_overflow_offset = 0;
    let mut decoded_operands = DecodedOperands::with_capacity(instructions.len());
    for (instruction_index, instr) in instructions.iter().enumerate() {
        decode_wordcode_operands(
            instr,
            overflow_operand_words,
            &mut next_overflow_offset,
            instruction_index,
            &mut decoded_operands.operands,
        )?;
        decoded_operands.ends.push(decoded_operands.operands.len());
    }
    verify_reachable_end(
        instructions,
        &decoded_operands,
        handlers.iter().map(|handler| handler.target as usize),
    )
}

/// Every instruction's decoded operands in one buffer, delimited by the end
/// offset of each instruction's run.
struct DecodedOperands {
    operands: Vec<Operand>,
    ends: Vec<usize>,
}

impl DecodedOperands {
    fn with_capacity(instructions: usize) -> Self {
        Self {
            operands: Vec::with_capacity(instructions.saturating_mul(3)),
            ends: Vec::with_capacity(instructions),
        }
    }

    /// Operands of instruction `index`.
    fn of(&self, index: usize) -> &[Operand] {
        let start = index
            .checked_sub(1)
            .map_or(0, |previous| self.ends[previous]);
        &self.operands[start..self.ends[index]]
    }
}

fn verify_wordcode_target(
    operands: &[Operand],
    instruction_index: usize,
    operand_index: usize,
    absent_value: Option<i32>,
    instruction_count: i64,
) -> Result<(), VerifyError> {
    resolve_wordcode_target(
        operands,
        instruction_index,
        operand_index,
        absent_value,
        instruction_count,
    )?;
    Ok(())
}

fn resolve_wordcode_target(
    operands: &[Operand],
    instruction_index: usize,
    operand_index: usize,
    absent_value: Option<i32>,
    instruction_count: i64,
) -> Result<Option<usize>, VerifyError> {
    let delta = checked_wordcode_imm32_operand(operands, instruction_index, operand_index)?;
    if absent_value == Some(delta) {
        return Ok(None);
    }
    let base = i64::try_from(instruction_index)
        .map_err(|_| VerifyError::FunctionTooLarge)?
        .checked_add(1)
        .ok_or(VerifyError::FunctionTooLarge)?;
    let target = base
        .checked_add(i64::from(delta))
        .ok_or(VerifyError::FunctionTooLarge)?;
    if !(0..=instruction_count).contains(&target) {
        return Err(VerifyError::InvalidControlFlowTarget {
            instruction_index,
            target,
        });
    }
    let target = usize::try_from(target).map_err(|_| VerifyError::FunctionTooLarge)?;
    Ok(Some(target))
}

fn checked_wordcode_imm32_operand(
    operands: &[Operand],
    instruction_index: usize,
    operand_index: usize,
) -> Result<i32, VerifyError> {
    match operands.get(operand_index).copied() {
        Some(Operand::Imm32(value)) => Ok(value),
        Some(operand) => Err(VerifyError::InvalidOperandShape {
            instruction_index,
            error: OperandShapeError::Kind {
                index: operand_index,
                expected: OperandKind::Imm32,
                actual: OperandKind::of(&operand),
            },
        }),
        None => Err(VerifyError::InvalidOperandShape {
            instruction_index,
            error: OperandShapeError::Count {
                expected: operand_index.saturating_add(1),
                actual: operands.len(),
            },
        }),
    }
}

fn decode_wordcode_operands(
    instruction: &WordInstruction,
    overflow_operand_words: &[u32],
    next_overflow_offset: &mut usize,
    instruction_index: usize,
    operands: &mut Vec<Operand>,
) -> Result<(), VerifyError> {
    let (op, raw_operand_count, inline_operand_words, raw_overflow_offset) =
        instruction.raw_parts();
    let operand_count = usize::from(raw_operand_count);
    let operand_words = if operand_count <= INLINE_OPERAND_WORDS {
        if raw_overflow_offset != u32::MAX {
            return Err(VerifyError::InvalidOperandStorage {
                instruction_index,
                error: OperandStorageError::InlineUsesOverflow {
                    operand_count: raw_operand_count,
                    overflow_offset: raw_overflow_offset,
                },
            });
        }
        for (word_index, word) in inline_operand_words
            .iter()
            .copied()
            .enumerate()
            .skip(operand_count)
        {
            if word != 0 {
                return Err(VerifyError::InvalidOperandStorage {
                    instruction_index,
                    error: OperandStorageError::NonZeroInlinePadding { word_index, word },
                });
            }
        }
        &inline_operand_words[..operand_count]
    } else {
        if raw_overflow_offset == u32::MAX {
            return Err(VerifyError::InvalidOperandStorage {
                instruction_index,
                error: OperandStorageError::MissingOverflow {
                    operand_count: raw_operand_count,
                },
            });
        }
        for (word_index, word) in inline_operand_words.iter().copied().enumerate() {
            if word != 0 {
                return Err(VerifyError::InvalidOperandStorage {
                    instruction_index,
                    error: OperandStorageError::NonZeroInlinePadding { word_index, word },
                });
            }
        }
        let overflow_offset = usize::try_from(raw_overflow_offset).map_err(|_| {
            VerifyError::InvalidOperandStorage {
                instruction_index,
                error: OperandStorageError::OverflowOutOfBounds {
                    overflow_offset: raw_overflow_offset,
                    operand_count: raw_operand_count,
                    available_words: overflow_operand_words.len(),
                },
            }
        })?;
        let overflow_end = overflow_offset.checked_add(operand_count).ok_or(
            VerifyError::InvalidOperandStorage {
                instruction_index,
                error: OperandStorageError::OverflowOutOfBounds {
                    overflow_offset: raw_overflow_offset,
                    operand_count: raw_operand_count,
                    available_words: overflow_operand_words.len(),
                },
            },
        )?;
        if overflow_end > overflow_operand_words.len() {
            return Err(VerifyError::InvalidOperandStorage {
                instruction_index,
                error: OperandStorageError::OverflowOutOfBounds {
                    overflow_offset: raw_overflow_offset,
                    operand_count: raw_operand_count,
                    available_words: overflow_operand_words.len(),
                },
            });
        }
        if overflow_offset != *next_overflow_offset {
            return Err(VerifyError::InvalidOperandStorage {
                instruction_index,
                error: OperandStorageError::NonCanonicalOverflowOffset {
                    expected: *next_overflow_offset,
                    actual: raw_overflow_offset,
                },
            });
        }
        *next_overflow_offset = overflow_end;
        &overflow_operand_words[overflow_offset..overflow_end]
    };

    if let Some(expected) = opcode_schema(op).operand_shape.fixed()
        && operand_count != expected.len()
    {
        return Err(VerifyError::InvalidOperandShape {
            instruction_index,
            error: OperandShapeError::Count {
                expected: expected.len(),
                actual: operand_count,
            },
        });
    }

    operands.reserve(operand_count);
    for (operand_index, word) in operand_words.iter().copied().enumerate() {
        let Some(kind) = operand_kind_at(op, operand_index) else {
            let expected = opcode_schema(op)
                .operand_shape
                .prefix()
                .map_or(0, |prefix| prefix.len());
            return Err(VerifyError::InvalidOperandShape {
                instruction_index,
                error: OperandShapeError::Count {
                    expected,
                    actual: operand_count,
                },
            });
        };
        let Some(operand) = decode_operand_word(kind, word) else {
            return Err(VerifyError::InvalidOperandStorage {
                instruction_index,
                error: OperandStorageError::InvalidOperandWord {
                    operand_index,
                    expected: kind,
                    word,
                },
            });
        };
        operands.push(operand);
    }
    Ok(())
}

fn verify_wordcode_overflow_consumed(
    consumed_words: usize,
    total_words: usize,
) -> Result<(), VerifyError> {
    if consumed_words == total_words {
        return Ok(());
    }
    Err(VerifyError::TrailingOverflowOperandWords {
        first_unused_word: consumed_words,
        total_words,
    })
}

/// Walk normal successors from `roots`; no reachable path may run off the
/// end of the code.
fn verify_reachable_end(
    instructions: &[WordInstruction],
    operands: &DecodedOperands,
    roots: impl IntoIterator<Item = usize>,
) -> Result<(), VerifyError> {
    let len = instructions.len();
    let count = i64::try_from(len).map_err(|_| VerifyError::FunctionTooLarge)?;
    let mut seen = vec![false; len];
    let mut worklist = Vec::new();
    for root in roots {
        if root >= len {
            return Err(VerifyError::ReachableFunctionEnd { predecessor: None });
        }
        if !seen[root] {
            seen[root] = true;
            worklist.push(root);
        }
    }
    while let Some(index) = worklist.pop() {
        for successor in opcode_schema(instructions[index].op).successor_shape.exact() {
            let target = match successor {
                SuccessorSpec::Fallthrough => index + 1,
                SuccessorSpec::RelativeTarget { operand_index, .. } => {
                    resolve_wordcode_target(operands.of(index), index, *operand_index, None, count)?
                        .ok_or(VerifyError::FunctionTooLarge)?
                }
                SuccessorSpec::FrameReturn => continue,
            };
            if target >= len {
                return Err(VerifyError::ReachableFunctionEnd {
                    predecessor: Some(index),
                });
            }
            if !seen[target] {
                seen[target] = true;
                worklist.push(target);
            }
        }
    }
    Ok(())
}

const OPERAND_KIND_REGISTER: u8 = 0;
const OPERAND_KIND_CONST_INDEX: u8 = 1;
const OPERAND_KIND_IMM32: u8 = 2;

/// Errors surfaced while decoding the cold serialized representation.
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
    /// A schema-authoritative fixed instruction has an invalid shape.
    InvalidOperandShape {
        /// Byte offset of the instruction opcode.
        offset: usize,
        /// Exact count or wire-kind mismatch.
        error: OperandShapeError,
    },
    /// A schema-authoritative branch targets an invalid byte position.
    InvalidControlFlowTarget {
        /// Byte offset of the branch instruction.
        offset: usize,
        /// Resolved signed target byte position.
        target: i64,
    },
    /// A schema-authoritative control-flow operand has an invalid value.
    InvalidControlFlowOperand {
        /// Byte offset of the instruction.
        offset: usize,
        /// Operand position.
        operand_index: usize,
        /// Invalid signed immediate.
        value: i32,
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
            Self::InvalidOperandShape { offset, error } => {
                write!(f, "invalid operand shape at byte offset {offset}: {error}")
            }
            Self::InvalidControlFlowTarget { offset, target } => write!(
                f,
                "invalid control-flow target {target} at byte offset {offset}"
            ),
            Self::InvalidControlFlowOperand {
                offset,
                operand_index,
                value,
            } => write!(
                f,
                "invalid control-flow operand {operand_index} value {value} at byte offset {offset}"
            ),
        }
    }
}

impl std::error::Error for DecodeError {}

/// Malformed or noncanonical operand storage in authoritative wordcode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperandStorageError {
    /// An inline instruction points into the overflow table.
    InlineUsesOverflow {
        /// Raw operand count.
        operand_count: u8,
        /// Unexpected overflow-table offset.
        overflow_offset: u32,
    },
    /// An instruction too large for inline storage has no overflow range.
    MissingOverflow {
        /// Raw operand count.
        operand_count: u8,
    },
    /// An overflow range does not fit wholly in the shared table.
    OverflowOutOfBounds {
        /// Raw overflow-table offset.
        overflow_offset: u32,
        /// Raw operand count.
        operand_count: u8,
        /// Number of words available in the table.
        available_words: usize,
    },
    /// An overflow range is valid in isolation but is not the next dense range.
    NonCanonicalOverflowOffset {
        /// Required next dense word offset.
        expected: usize,
        /// Raw offset stored by the instruction.
        actual: u32,
    },
    /// An inline slot that is not active in this storage form is nonzero.
    NonZeroInlinePadding {
        /// Inline word position.
        word_index: usize,
        /// Noncanonical word value.
        word: u32,
    },
    /// A raw word cannot be represented by its schema-declared operand kind.
    InvalidOperandWord {
        /// Operand position.
        operand_index: usize,
        /// Schema-declared kind.
        expected: OperandKind,
        /// Invalid raw word.
        word: u32,
    },
}

impl std::fmt::Display for OperandStorageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InlineUsesOverflow {
                operand_count,
                overflow_offset,
            } => write!(
                f,
                "inline operand count {operand_count} uses overflow offset {overflow_offset}"
            ),
            Self::MissingOverflow { operand_count } => write!(
                f,
                "operand count {operand_count} exceeds inline storage without an overflow range"
            ),
            Self::OverflowOutOfBounds {
                overflow_offset,
                operand_count,
                available_words,
            } => write!(
                f,
                "overflow range {overflow_offset}..+{operand_count} exceeds {available_words} words"
            ),
            Self::NonCanonicalOverflowOffset { expected, actual } => write!(
                f,
                "overflow offset {actual} is noncanonical; expected dense offset {expected}"
            ),
            Self::NonZeroInlinePadding { word_index, word } => write!(
                f,
                "inactive inline operand word {word_index} is nonzero ({word})"
            ),
            Self::InvalidOperandWord {
                operand_index,
                expected,
                word,
            } => write!(
                f,
                "operand {operand_index} word {word} cannot be decoded as {expected:?}"
            ),
        }
    }
}

impl std::error::Error for OperandStorageError {}

/// Structural error in logical instructions or authoritative wordcode.
#[derive(Debug, PartialEq, Eq)]
pub enum VerifyError {
    /// Cold serialized metadata would exceed the u32 PC coordinate space.
    FunctionTooLarge,
    /// An instruction violates its authoritative operand shape.
    InvalidOperandShape {
        /// Dense source-order instruction index.
        instruction_index: usize,
        /// Exact count or kind mismatch.
        error: OperandShapeError,
    },
    /// One instruction has malformed or noncanonical raw operand storage.
    InvalidOperandStorage {
        /// Dense source-order instruction index.
        instruction_index: usize,
        /// Exact storage violation.
        error: OperandStorageError,
    },
    /// The shared overflow table contains words not owned by an instruction.
    TrailingOverflowOperandWords {
        /// First word not consumed by canonical dense ranges.
        first_unused_word: usize,
        /// Total overflow-table word count.
        total_words: usize,
    },
    /// A relative branch or handler target falls outside `0..=len`.
    InvalidControlFlowTarget {
        /// Dense source-order instruction index.
        instruction_index: usize,
        /// Resolved signed target instruction index.
        target: i64,
    },
    /// A schema-declared control-flow operand contains an invalid value.
    InvalidControlFlowOperand {
        /// Dense source-order instruction index.
        instruction_index: usize,
        /// Operand position.
        operand_index: usize,
        /// Invalid signed immediate.
        value: i32,
    },
    /// A handler range is empty or extends past the code.
    InvalidHandlerRange {
        /// Index into the handler table.
        handler_index: usize,
        /// First covered instruction.
        start: u32,
        /// One past the last covered instruction.
        end: u32,
    },
    /// A handler target lies outside the code or inside its own range.
    InvalidHandlerTarget {
        /// Index into the handler table.
        handler_index: usize,
        /// Target instruction.
        target: u32,
    },
    /// Two handler ranges overlap without nesting, or an enclosing range
    /// precedes a range inside it.
    HandlerNesting {
        /// Later entry of the conflicting pair.
        handler_index: usize,
        /// Earlier entry of the conflicting pair.
        earlier_index: usize,
    },
    /// Normal control flow reaches the non-executable function-end boundary.
    ReachableFunctionEnd {
        /// Instruction whose successor reaches the end; `None` for an empty
        /// body or a handler target at the end.
        predecessor: Option<usize>,
    },
}

impl std::fmt::Display for VerifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::FunctionTooLarge => write!(f, "function byte metadata exceeds u32::MAX"),
            Self::InvalidOperandShape {
                instruction_index,
                error,
            } => write!(
                f,
                "invalid operand shape at instruction {instruction_index}: {error}"
            ),
            Self::InvalidOperandStorage {
                instruction_index,
                error,
            } => write!(
                f,
                "invalid operand storage at instruction {instruction_index}: {error}"
            ),
            Self::TrailingOverflowOperandWords {
                first_unused_word,
                total_words,
            } => write!(
                f,
                "overflow operand words {first_unused_word}..{total_words} are unreferenced"
            ),
            Self::InvalidControlFlowTarget {
                instruction_index,
                target,
            } => write!(
                f,
                "invalid control-flow target {target} at instruction {instruction_index}"
            ),
            Self::InvalidControlFlowOperand {
                instruction_index,
                operand_index,
                value,
            } => write!(
                f,
                "invalid control-flow operand {operand_index} value {value} at instruction {instruction_index}"
            ),
            Self::InvalidHandlerRange {
                handler_index,
                start,
                end,
            } => write!(
                f,
                "exception handler {handler_index} covers invalid range {start}..{end}"
            ),
            Self::InvalidHandlerTarget {
                handler_index,
                target,
            } => write!(
                f,
                "exception handler {handler_index} targets invalid instruction {target}"
            ),
            Self::HandlerNesting {
                handler_index,
                earlier_index,
            } => write!(
                f,
                "exception handlers {earlier_index} and {handler_index} overlap without nesting inner-first"
            ),
            Self::ReachableFunctionEnd { predecessor } => write!(
                f,
                "normal control flow from {predecessor:?} reaches function end"
            ),
        }
    }
}

impl std::error::Error for VerifyError {}

/// Verify compiler instructions directly in their logical instruction-index
/// coordinate system.
///
/// # Errors
/// Returns [`VerifyError`] for operand-shape or branch-target violations.
pub fn verify_logical_function(instructions: &[Instruction]) -> Result<(), VerifyError> {
    let len = instructions.len() as i64;
    for (instruction_index, instr) in instructions.iter().enumerate() {
        verify_operand_shape(instr.op, instr.operands.as_slice()).map_err(|error| {
            VerifyError::InvalidOperandShape {
                instruction_index,
                error,
            }
        })?;
        let schema = opcode_schema(instr.op);
        for successor in schema.successor_shape.exact() {
            if let SuccessorSpec::RelativeTarget { operand_index, .. } = successor {
                verify_logical_target(instructions, instruction_index, *operand_index, None, len)?;
            }
        }
    }
    Ok(())
}

/// Cold serialized byte-PC layout derived without materialising a byte stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionLayout {
    /// Total serialized length used as the byte-PC end boundary.
    pub total_bytes: u32,
    /// Byte PC for every logical instruction index.
    pub instr_to_byte_pc: Box<[u32]>,
}

/// Verify a logical function and calculate its cold serialized byte-PC layout
/// without encoding or decoding a byte stream.
///
/// # Errors
/// Returns [`VerifyError`] for an invalid logical function or u32 metadata
/// overflow.
pub fn layout_function(instructions: &[Instruction]) -> Result<FunctionLayout, VerifyError> {
    verify_logical_function(instructions)?;
    let mut byte_pc = 0_u32;
    let mut instr_to_byte_pc = Vec::with_capacity(instructions.len());
    for instr in instructions {
        instr_to_byte_pc.push(byte_pc);
        let mut byte_len = 2_u32;
        for operand in instr.operands.iter() {
            let operand_len = match operand {
                Operand::Register(_) => 3,
                Operand::ConstIndex(_) | Operand::Imm32(_) => 5,
            };
            byte_len = byte_len
                .checked_add(operand_len)
                .ok_or(VerifyError::FunctionTooLarge)?;
        }
        byte_pc = byte_pc
            .checked_add(byte_len)
            .ok_or(VerifyError::FunctionTooLarge)?;
    }
    Ok(FunctionLayout {
        total_bytes: byte_pc,
        instr_to_byte_pc: instr_to_byte_pc.into_boxed_slice(),
    })
}

/// Verify authoritative wordcode and calculate its cold serialized byte-PC
/// layout without creating a byte stream.
///
/// # Errors
/// Returns [`VerifyError`] for invalid wordcode or u32 metadata overflow.
pub fn layout_wordcode_function(code: &FunctionCode) -> Result<FunctionLayout, VerifyError> {
    verify_wordcode_function(code)?;
    measure_wordcode_function(code)
}

/// Calculate a wordcode function's serialized byte-PC layout without
/// validating control-flow targets.
///
/// Production loading should use [`layout_wordcode_function`]. This narrower
/// operation exists for compiler/backend negative-test fixtures that
/// deliberately carry malformed targets but still need the canonical encoded
/// size before the intended validation stage.
///
/// # Errors
/// Returns [`VerifyError`] for malformed/noncanonical operand storage,
/// operand-shape violations, or u32 metadata overflow. Control-flow targets
/// are intentionally not checked here.
pub fn measure_wordcode_function(code: &FunctionCode) -> Result<FunctionLayout, VerifyError> {
    let mut byte_pc = 0_u32;
    let mut instr_to_byte_pc = Vec::with_capacity(code.len());
    let (instructions, overflow_operand_words) = code.raw_parts();
    let mut next_overflow_offset = 0;
    let mut operands = Vec::new();
    for (instruction_index, instruction) in instructions.iter().enumerate() {
        operands.clear();
        decode_wordcode_operands(
            instruction,
            overflow_operand_words,
            &mut next_overflow_offset,
            instruction_index,
            &mut operands,
        )?;
        verify_operand_shape(instruction.op, &operands).map_err(|error| {
            VerifyError::InvalidOperandShape {
                instruction_index,
                error,
            }
        })?;
        instr_to_byte_pc.push(byte_pc);
        let mut byte_len = 2_u32;
        for operand in &operands {
            let operand_len = match operand {
                Operand::Register(_) => 3,
                Operand::ConstIndex(_) | Operand::Imm32(_) => 5,
            };
            byte_len = byte_len
                .checked_add(operand_len)
                .ok_or(VerifyError::FunctionTooLarge)?;
        }
        byte_pc = byte_pc
            .checked_add(byte_len)
            .ok_or(VerifyError::FunctionTooLarge)?;
    }
    verify_wordcode_overflow_consumed(next_overflow_offset, overflow_operand_words.len())?;
    Ok(FunctionLayout {
        total_bytes: byte_pc,
        instr_to_byte_pc: instr_to_byte_pc.into_boxed_slice(),
    })
}

fn verify_logical_target(
    instructions: &[Instruction],
    instruction_index: usize,
    operand_index: usize,
    absent_value: Option<i32>,
    instruction_count: i64,
) -> Result<(), VerifyError> {
    let Operand::Imm32(delta) = instructions[instruction_index].operands.as_slice()[operand_index]
    else {
        unreachable!("operand shape verified before control-flow metadata")
    };
    if absent_value == Some(delta) {
        return Ok(());
    }
    let target = instruction_index as i64 + 1 + i64::from(delta);
    if !(0..=instruction_count).contains(&target) {
        return Err(VerifyError::InvalidControlFlowTarget {
            instruction_index,
            target,
        });
    }
    Ok(())
}

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
/// `Op::JumpIfFalse`, `Op::JumpIfNullish`) as
/// *instruction-index* deltas relative to the instruction following the
/// jump. The wire format wants *byte-offset* deltas relative to
/// `(jump_pc + 1)` (the byte right after the opcode), so encoding is a
/// two-pass walk:
///
/// 1. Write every instruction in order, capturing each jump operand's
///    byte position and the source instruction-index delta.
/// 2. Re-resolve each captured slot to a byte-offset delta computed
///    against `instr_to_byte_pc`.
#[must_use]
pub fn encode_function(instructions: &[Instruction]) -> EncodedFunction {
    try_encode_function(instructions)
        .unwrap_or_else(|error| panic!("invalid logical bytecode function: {error}"))
}

/// Verify and encode a compiler function DTO without decoding the serialized
/// bytes back into an execution representation.
///
/// # Errors
/// Returns [`VerifyError`] before encoding when the logical instruction stream
/// violates the opcode schema or its instruction-index CFG.
pub fn try_encode_function(instructions: &[Instruction]) -> Result<EncodedFunction, VerifyError> {
    verify_logical_function(instructions)?;
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

    Ok(EncodedFunction {
        code: writer.into_bytes(),
        instr_to_byte_pc: instr_to_byte_pc.into_boxed_slice(),
    })
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
/// Propagates any [`DecodeError`] from instruction decoding or schema-derived
/// control-flow target verification.
pub fn decode_function(code: &[u8]) -> Result<Vec<Instruction>, DecodeError> {
    let mut out: Vec<Instruction> = Vec::new();
    let mut pc = 0usize;
    while pc < code.len() {
        let (instr, next) = decode_instruction(code, pc)?;
        out.push(instr);
        pc = next;
    }
    verify_control_flow_targets(&out, code.len())?;
    Ok(out)
}

fn verify_control_flow_targets(
    instructions: &[Instruction],
    code_len: usize,
) -> Result<(), DecodeError> {
    let boundaries: Vec<u32> = instructions.iter().map(|instr| instr.pc).collect();
    for instr in instructions {
        let schema = opcode_schema(instr.op);
        for successor in schema.successor_shape.exact() {
            let SuccessorSpec::RelativeTarget {
                operand_index,
                base,
            } = successor
            else {
                continue;
            };
            verify_relative_target(instr, *operand_index, *base, None, &boundaries, code_len)?;
        }
    }
    Ok(())
}

fn verify_relative_target(
    instr: &Instruction,
    operand_index: usize,
    base: RelativeTargetBase,
    absent_value: Option<i32>,
    boundaries: &[u32],
    code_len: usize,
) -> Result<(), DecodeError> {
    let Operand::Imm32(delta) = instr.operands.as_slice()[operand_index] else {
        unreachable!("schema operand verification precedes successor verification")
    };
    if absent_value == Some(delta) {
        return Ok(());
    }
    let base = match base {
        RelativeTargetBase::AfterOpcode => i64::from(instr.pc) + 1,
    };
    let target = base + i64::from(delta);
    let valid = target == code_len as i64
        || (target >= 0
            && target < code_len as i64
            && boundaries.binary_search(&(target as u32)).is_ok());
    if !valid {
        return Err(DecodeError::InvalidControlFlowTarget {
            offset: instr.pc as usize,
            target,
        });
    }
    Ok(())
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
        operands,
    };
    verify_operand_shape(instr.op, instr.operands.as_slice())
        .map_err(|error| DecodeError::InvalidOperandShape { offset: pc, error })?;
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

/// Generated byte assignments for every [`Op`] variant.
///
/// The declarative schema owns the rows; this re-export preserves the current
/// encoder/decoder API while preventing a second byte-assignment table.
pub use crate::opcode_schema::OP_BYTE_TABLE;

#[cfg(test)]
mod tests {
    use super::*;

    const NO_OVERFLOW: u32 = u32::MAX;

    fn make_instr(op: Op, operands: &[Operand]) -> Instruction {
        Instruction {
            pc: 0,
            op,
            operands: operands.to_vec(),
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

    fn raw_wordcode(
        instruction: WordInstruction,
        overflow_operand_words: Vec<u32>,
    ) -> FunctionCode {
        FunctionCode::from_raw_parts(vec![instruction], overflow_operand_words)
    }

    fn build_wordcode(instructions: Vec<(Op, Vec<Operand>)>) -> FunctionCode {
        let mut builder = crate::FunctionCodeBuilder::new();
        for (op, operands) in instructions {
            builder.push(op, &operands);
        }
        builder.finish()
    }

    #[test]
    fn wordcode_verifier_accepts_canonical_dense_overflow_storage() {
        let mut builder = crate::FunctionCodeBuilder::new();
        for destination in [0, 5] {
            builder.push(
                Op::MakeClass,
                &[
                    Operand::Register(destination),
                    Operand::Register(destination + 1),
                    Operand::Register(destination + 2),
                    Operand::Register(destination + 3),
                    Operand::Register(destination + 4),
                ],
            );
        }
        builder.push(Op::ReturnUndefined, &[]);
        let code = builder.finish();

        assert_eq!(verify_wordcode_function(&code), Ok(()));
        assert!(layout_wordcode_function(&code).is_ok());
    }

    #[test]
    fn wordcode_verifier_rejects_each_noncanonical_storage_form() {
        let inline_uses_overflow = raw_wordcode(
            WordInstruction::from_raw_parts(Op::LoadUndefined, 1, [0; 4], 0),
            vec![0],
        );
        assert!(matches!(
            verify_wordcode_function(&inline_uses_overflow),
            Err(VerifyError::InvalidOperandStorage {
                instruction_index: 0,
                error: OperandStorageError::InlineUsesOverflow {
                    operand_count: 1,
                    overflow_offset: 0,
                },
            })
        ));

        let missing_overflow = raw_wordcode(
            WordInstruction::from_raw_parts(Op::MakeClass, 5, [0; 4], NO_OVERFLOW),
            vec![0; 5],
        );
        assert!(matches!(
            verify_wordcode_function(&missing_overflow),
            Err(VerifyError::InvalidOperandStorage {
                instruction_index: 0,
                error: OperandStorageError::MissingOverflow { operand_count: 5 },
            })
        ));

        let out_of_bounds = raw_wordcode(
            WordInstruction::from_raw_parts(Op::MakeClass, 5, [0; 4], 0),
            vec![0; 4],
        );
        assert!(matches!(
            verify_wordcode_function(&out_of_bounds),
            Err(VerifyError::InvalidOperandStorage {
                instruction_index: 0,
                error: OperandStorageError::OverflowOutOfBounds {
                    overflow_offset: 0,
                    operand_count: 5,
                    available_words: 4,
                },
            })
        ));

        let noncanonical_offset = raw_wordcode(
            WordInstruction::from_raw_parts(Op::MakeClass, 5, [0; 4], 1),
            vec![0; 6],
        );
        assert!(matches!(
            verify_wordcode_function(&noncanonical_offset),
            Err(VerifyError::InvalidOperandStorage {
                instruction_index: 0,
                error: OperandStorageError::NonCanonicalOverflowOffset {
                    expected: 0,
                    actual: 1,
                },
            })
        ));

        let nonzero_padding = raw_wordcode(
            WordInstruction::from_raw_parts(Op::Nop, 0, [1, 0, 0, 0], NO_OVERFLOW),
            Vec::new(),
        );
        assert!(matches!(
            verify_wordcode_function(&nonzero_padding),
            Err(VerifyError::InvalidOperandStorage {
                instruction_index: 0,
                error: OperandStorageError::NonZeroInlinePadding {
                    word_index: 0,
                    word: 1,
                },
            })
        ));

        let invalid_register_word = raw_wordcode(
            WordInstruction::from_raw_parts(Op::LoadUndefined, 1, [u32::MAX, 0, 0, 0], NO_OVERFLOW),
            Vec::new(),
        );
        assert!(matches!(
            verify_wordcode_function(&invalid_register_word),
            Err(VerifyError::InvalidOperandStorage {
                instruction_index: 0,
                error: OperandStorageError::InvalidOperandWord {
                    operand_index: 0,
                    expected: OperandKind::Register,
                    word: u32::MAX,
                },
            })
        ));

        let trailing_overflow = raw_wordcode(
            WordInstruction::from_raw_parts(Op::Nop, 0, [0; 4], NO_OVERFLOW),
            vec![0],
        );
        assert!(matches!(
            verify_wordcode_function(&trailing_overflow),
            Err(VerifyError::TrailingOverflowOperandWords {
                first_unused_word: 0,
                total_words: 1,
            })
        ));
    }

    #[test]
    fn wordcode_verifier_rejects_hostile_raw_fields_without_panicking() {
        let hostile_bodies = [
            raw_wordcode(
                WordInstruction::from_raw_parts(Op::MakeClass, u8::MAX, [0; 4], u32::MAX - 1),
                Vec::new(),
            ),
            raw_wordcode(
                WordInstruction::from_raw_parts(
                    Op::LoadUndefined,
                    1,
                    [u32::MAX, 0, 0, 0],
                    NO_OVERFLOW,
                ),
                Vec::new(),
            ),
            raw_wordcode(
                WordInstruction::from_raw_parts(Op::Nop, 4, [0; 4], NO_OVERFLOW),
                Vec::new(),
            ),
        ];

        for code in hostile_bodies {
            let result = std::panic::catch_unwind(|| verify_wordcode_function(&code));
            assert!(
                matches!(result, Ok(Err(_))),
                "hostile wordcode must return a typed error: {result:?}"
            );
        }
    }

    #[test]
    fn wordcode_measurement_rejects_malformed_storage_without_operand_view() {
        let code = raw_wordcode(
            WordInstruction::from_raw_parts(Op::MakeClass, 5, [0; 4], u32::MAX - 1),
            Vec::new(),
        );

        assert!(matches!(
            measure_wordcode_function(&code),
            Err(VerifyError::InvalidOperandStorage {
                error: OperandStorageError::OverflowOutOfBounds { .. },
                ..
            })
        ));
    }

    #[test]
    fn wordcode_layout_always_verifies_control_flow() {
        let mut builder = crate::FunctionCodeBuilder::new();
        builder.push(Op::Jump, &[Operand::Imm32(-2)]);
        let code = builder.finish();

        assert!(matches!(
            layout_wordcode_function(&code),
            Err(VerifyError::InvalidControlFlowTarget {
                instruction_index: 0,
                target: -1,
            })
        ));
    }

    #[test]
    fn handler_table_rejects_bad_ranges_targets_and_nesting() {
        let code = build_wordcode(vec![
            (Op::Nop, vec![]),
            (Op::Nop, vec![]),
            (Op::ReturnUndefined, vec![]),
            (Op::ReturnUndefined, vec![]),
        ]);
        let handler = |start, end, target| ExceptionHandler {
            start,
            end,
            target,
            exception: 0,
        };
        assert!(verify_exception_handlers(&code, &[handler(0, 2, 3)]).is_ok());
        assert!(matches!(
            verify_exception_handlers(&code, &[handler(2, 2, 3)]),
            Err(VerifyError::InvalidHandlerRange { .. })
        ));
        assert!(matches!(
            verify_exception_handlers(&code, &[handler(0, 5, 3)]),
            Err(VerifyError::InvalidHandlerRange { .. })
        ));
        assert!(matches!(
            verify_exception_handlers(&code, &[handler(0, 2, 1)]),
            Err(VerifyError::InvalidHandlerTarget { .. })
        ));
        assert!(matches!(
            verify_exception_handlers(&code, &[handler(0, 2, 4)]),
            Err(VerifyError::InvalidHandlerTarget { .. })
        ));
        // Inner before outer is accepted; outer before inner or a partial
        // overlap is not.
        assert!(verify_exception_handlers(&code, &[handler(1, 2, 3), handler(0, 2, 3)]).is_ok());
        assert!(matches!(
            verify_exception_handlers(&code, &[handler(0, 2, 3), handler(1, 2, 3)]),
            Err(VerifyError::HandlerNesting { .. })
        ));
        assert!(matches!(
            verify_exception_handlers(&code, &[handler(0, 2, 3), handler(1, 3, 3)]),
            Err(VerifyError::HandlerNesting { .. })
        ));
    }

    #[test]
    fn handler_target_must_not_run_off_the_end() {
        let code = build_wordcode(vec![(Op::ReturnUndefined, vec![]), (Op::Nop, vec![])]);
        let handler = ExceptionHandler {
            start: 0,
            end: 1,
            target: 1,
            exception: 0,
        };
        assert!(matches!(
            verify_exception_handlers(&code, &[handler]),
            Err(VerifyError::ReachableFunctionEnd {
                predecessor: Some(1)
            })
        ));
    }

    #[test]
    fn wordcode_verifier_rejects_reachable_function_end() {
        let code = build_wordcode(vec![(Op::Nop, vec![])]);
        assert!(matches!(
            verify_wordcode_function(&code),
            Err(VerifyError::ReachableFunctionEnd {
                predecessor: Some(0)
            })
        ));
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
    fn direct_layout_matches_serialized_encoder_boundaries() {
        let instructions = vec![
            make_instr(Op::LoadUndefined, &[Operand::Register(1)]),
            make_instr(
                Op::LoadInt32,
                &[Operand::Register(2), Operand::Imm32(i32::MIN)],
            ),
            make_instr(
                Op::Call,
                &[
                    Operand::Register(0),
                    Operand::Register(1),
                    Operand::ConstIndex(1),
                    Operand::Register(2),
                ],
            ),
        ];
        let layout = layout_function(&instructions).unwrap();
        let encoded = try_encode_function(&instructions).unwrap();

        assert_eq!(layout.instr_to_byte_pc, encoded.instr_to_byte_pc);
        assert_eq!(layout.total_bytes as usize, encoded.code.len());
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
    fn authoritative_shape_rejects_wrong_operand_count() {
        let bytes = [0x01u8, 0]; // LoadUndefined requires one write register.
        assert!(matches!(
            decode_instruction(&bytes, 0),
            Err(DecodeError::InvalidOperandShape {
                error: OperandShapeError::Count {
                    expected: 1,
                    actual: 0
                },
                ..
            })
        ));
    }

    #[test]
    fn authoritative_shape_rejects_wrong_operand_kind() {
        let bytes = [0x06u8, 2, 0, 0, 0, 1, 0, 0, 0, 0];
        assert!(matches!(
            decode_instruction(&bytes, 0),
            Err(DecodeError::InvalidOperandShape {
                error: OperandShapeError::Kind {
                    index: 1,
                    expected: crate::opcode_schema::OperandKind::Imm32,
                    actual: crate::opcode_schema::OperandKind::ConstIndex,
                },
                ..
            })
        ));
    }

    #[test]
    fn authoritative_variadic_shape_rejects_count_mismatch() {
        let mut writer = BytecodeWriter::new();
        writer.write(&make_instr(
            Op::Call,
            &[
                Operand::Register(0),
                Operand::Register(1),
                Operand::ConstIndex(2),
                Operand::Register(2),
            ],
        ));
        let bytes = writer.into_bytes();
        assert!(matches!(
            decode_instruction(&bytes, 0),
            Err(DecodeError::InvalidOperandShape {
                error: OperandShapeError::Count {
                    expected: 5,
                    actual: 4
                },
                ..
            })
        ));
    }

    #[test]
    fn authoritative_variadic_shape_rejects_wrong_tail_kind() {
        let mut writer = BytecodeWriter::new();
        writer.write(&make_instr(
            Op::TailCall,
            &[
                Operand::Register(0),
                Operand::Register(1),
                Operand::ConstIndex(1),
                Operand::Imm32(2),
            ],
        ));
        let bytes = writer.into_bytes();
        assert!(matches!(
            decode_instruction(&bytes, 0),
            Err(DecodeError::InvalidOperandShape {
                error: OperandShapeError::Kind {
                    index: 3,
                    expected: crate::opcode_schema::OperandKind::Register,
                    actual: crate::opcode_schema::OperandKind::Imm32,
                },
                ..
            })
        ));
    }

    #[test]
    fn authoritative_variadic_shape_round_trips() {
        let instr = make_instr(
            Op::CallWithThis,
            &[
                Operand::Register(0),
                Operand::Register(1),
                Operand::Register(2),
                Operand::ConstIndex(2),
                Operand::Register(3),
                Operand::Register(4),
            ],
        );
        assert_eq!(roundtrip(&instr), instr);
    }

    #[test]
    fn authoritative_jump_rejects_target_inside_instruction() {
        let mut writer = BytecodeWriter::new();
        // Jump byte PC is 0 and relative deltas use base PC+1. Nop starts at
        // byte 7, so delta 7 resolves to byte 8: the Nop operand-count byte.
        writer.write(&make_instr(Op::Jump, &[Operand::Imm32(7)]));
        writer.write(&make_instr(Op::Nop, &[]));
        let bytes = writer.into_bytes();
        assert!(matches!(
            decode_function(&bytes),
            Err(DecodeError::InvalidControlFlowTarget {
                offset: 0,
                target: 8
            })
        ));
    }

    #[test]
    fn authoritative_jump_accepts_instruction_and_end_boundaries() {
        let instructions = [
            make_instr(Op::Jump, &[Operand::Imm32(0)]),
            make_instr(Op::Nop, &[]),
        ];
        let encoded = encode_function(&instructions);
        assert!(decode_function(&encoded.code).is_ok());

        let end_jump = encode_function(&[make_instr(Op::Jump, &[Operand::Imm32(0)])]);
        assert!(decode_function(&end_jump.code).is_ok());
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
}
