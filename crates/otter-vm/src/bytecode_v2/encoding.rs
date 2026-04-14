//! Bytecode v2 encoder. Produces a byte stream + feedback map ready for
//! [`super::Bytecode`] / [`super::InstructionIter`] consumption.
//!
//! The encoder's job is threefold:
//!
//! 1. **Validate** that every emitted operand matches the opcode's static
//!    [`OperandShape`](super::OperandShape).
//! 2. **Pick the narrowest operand width** that fits all operands of a
//!    single instruction (narrow / wide / extra-wide), auto-prepending
//!    the appropriate prefix byte.
//! 3. **Serialize** operands into the chosen width, little-endian, signed
//!    or unsigned depending on kind.
//!
//! Labels and forward-reference jumps are resolved by the builder. A
//! `Label` handle starts as `Unresolved`; when the target PC is known
//! (`bind_label`), all pending references to the label are back-patched.

use super::feedback_map::{FeedbackMap, FeedbackSlot};
use super::opcodes::{OpcodeV2, OperandShape};
use super::operand::{Operand, OperandKind, OperandWidth};
use super::{Bytecode, PREFIX_EXTRA_WIDE, PREFIX_WIDE};

/// Errors the encoder surfaces. All are compile-time bugs — the source
/// compiler should never hit them in production.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EncodeError {
    /// Operand kind doesn't match the opcode's declared shape at this
    /// position.
    #[error(
        "opcode {opcode} operand #{position} expected kind {expected:?}, got {actual:?}"
    )]
    OperandKindMismatch {
        opcode: &'static str,
        position: usize,
        expected: OperandKind,
        actual: OperandKind,
    },
    /// Too few / too many operands for this opcode.
    #[error("opcode {opcode} expected {expected} operand slots, got {actual}")]
    ArityMismatch {
        opcode: &'static str,
        expected: usize,
        actual: usize,
    },
    /// Operand exceeds the ExtraWide range (u32 / i32). This shouldn't
    /// happen with well-formed programs.
    #[error(
        "opcode {opcode} operand #{position} out of representable range: {value}"
    )]
    OperandOutOfRange {
        opcode: &'static str,
        position: usize,
        value: i64,
    },
    /// `bind_label` called twice for the same `Label`.
    #[error("label {0} bound more than once")]
    LabelAlreadyBound(u32),
    /// Label reference cannot be encoded in ExtraWide (jump offset >±2GiB
    /// from the site). Practically unreachable — JS functions fit.
    #[error("label {label} jump offset {offset} overflows i32")]
    LabelOffsetOverflow { label: u32, offset: i64 },
    /// Bytecode stream grew past u32::MAX — would wrap feedback offsets.
    #[error("bytecode stream length exceeded u32::MAX")]
    StreamTooLong,
}

/// A forward-reference label. Created by [`BytecodeBuilder::new_label`],
/// pointed at by [`BytecodeBuilder::emit_jump_to`], and bound to a PC by
/// [`BytecodeBuilder::bind_label`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Label(u32);

struct LabelState {
    // Either Some(target_pc) or None (unbound). Pending references live
    // in `pending_references` until the label is bound.
    target_pc: Option<u32>,
    pending: Vec<PendingJump>,
}

/// A jump emitted before its target was known. Back-patched on
/// `bind_label` with the actual `target_pc - ref_pc_after_insn` offset.
struct PendingJump {
    /// Absolute byte position of the first operand byte in the stream.
    operand_start: u32,
    /// Width of the operand, so the back-patcher knows how many bytes to
    /// overwrite.
    operand_width: OperandWidth,
    /// Byte position of the next instruction after this jump (the base
    /// from which the jump offset is measured).
    ref_pc_after: u32,
}

/// Primary encoder. Accumulates bytes into an internal `Vec<u8>`,
/// resolves labels at finalization, and emits a sparse feedback map.
pub struct BytecodeBuilder {
    bytes: Vec<u8>,
    labels: Vec<LabelState>,
    feedback_entries: Vec<(u32, FeedbackSlot)>,
}

impl Default for BytecodeBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl BytecodeBuilder {
    #[must_use]
    pub fn new() -> Self {
        Self {
            bytes: Vec::new(),
            labels: Vec::new(),
            feedback_entries: Vec::new(),
        }
    }

    /// Current bytecode PC (byte offset). Useful for peephole / jump-back
    /// emission. PCs are stable across label back-patching.
    #[must_use]
    pub fn pc(&self) -> u32 {
        self.bytes.len() as u32
    }

    /// Allocate a new label. Not yet bound to any PC.
    pub fn new_label(&mut self) -> Label {
        let id = self.labels.len() as u32;
        self.labels.push(LabelState {
            target_pc: None,
            pending: Vec::new(),
        });
        Label(id)
    }

    /// Bind an allocated label to the current PC. All pending forward
    /// references get their operand back-patched.
    pub fn bind_label(&mut self, label: Label) -> Result<(), EncodeError> {
        let target_pc = self.pc();
        let state = self
            .labels
            .get_mut(label.0 as usize)
            .expect("label from a different builder");
        if state.target_pc.is_some() {
            return Err(EncodeError::LabelAlreadyBound(label.0));
        }
        state.target_pc = Some(target_pc);
        let pending = std::mem::take(&mut state.pending);
        for p in pending {
            let offset = i64::from(target_pc) - i64::from(p.ref_pc_after);
            let offset_i32 = i32::try_from(offset).map_err(|_| EncodeError::LabelOffsetOverflow {
                label: label.0,
                offset,
            })?;
            Self::patch_signed(
                &mut self.bytes,
                p.operand_start as usize,
                p.operand_width,
                offset_i32,
            )?;
        }
        Ok(())
    }

    /// Emit an instruction with already-resolved operands.
    ///
    /// The builder picks the narrowest uniform operand width that fits
    /// every operand in `operands`, prepends a prefix byte if needed,
    /// emits the opcode, then emits each operand slot in little-endian.
    pub fn emit(
        &mut self,
        opcode: OpcodeV2,
        operands: &[Operand],
    ) -> Result<u32, EncodeError> {
        let shape = opcode.shape();
        Self::validate_shape(opcode, shape, operands)?;

        let width = chosen_width(operands);
        let pc = self.pc();
        self.emit_prefix(width);
        self.bytes.push(opcode.as_byte());
        for (i, operand) in operands.iter().enumerate() {
            self.emit_operand(opcode, i, *operand, width)?;
        }
        Ok(pc)
    }

    /// Emit a jump-family instruction referencing a `Label`. The operand
    /// width is pessimistically sized to cover any future jump distance —
    /// we pick based on the current best guess (Wide if the label is
    /// bound and fits in i16; else ExtraWide). If the label is unbound,
    /// we default to `Wide` — enough for every realistic JS function —
    /// and fix up at `bind_label` time. A mismatch between the reserved
    /// width and the resolved offset raises `LabelOffsetOverflow`.
    pub fn emit_jump_to(
        &mut self,
        opcode: OpcodeV2,
        label: Label,
    ) -> Result<u32, EncodeError> {
        let shape = opcode.shape();
        assert_eq!(
            shape.arity(),
            1,
            "emit_jump_to expects an instruction with exactly one JumpOff operand"
        );
        assert!(matches!(shape.operands()[0], OperandKind::JumpOff));

        let state = self
            .labels
            .get(label.0 as usize)
            .expect("label from a different builder");

        // Pick width: if the label is already bound, size exactly;
        // otherwise reserve Wide (16-bit) which covers every realistic
        // JS function (±32KiB). If a function ever overflows Wide, Phase
        // 2 will promote this path to speculative ExtraWide.
        let width = match state.target_pc {
            Some(target) => {
                let after = self.pc() + 1 + 1; // prefix byte? unknown yet; use 2 (no prefix + opcode) as lower bound
                let rough = i64::from(target) - i64::from(after);
                OperandWidth::min_for_signed(rough as i32)
            }
            None => OperandWidth::Wide,
        };

        let pc = self.pc();
        self.emit_prefix(width);
        self.bytes.push(opcode.as_byte());
        let operand_start = self.pc();
        // Reserve operand bytes (zero-filled placeholder).
        for _ in 0..width.bytes_per_operand() {
            self.bytes.push(0);
        }
        let ref_pc_after = self.pc();

        let state = &mut self.labels[label.0 as usize];
        if let Some(target_pc) = state.target_pc {
            // Backward jump — patch immediately.
            let offset = i64::from(target_pc) - i64::from(ref_pc_after);
            let offset_i32 =
                i32::try_from(offset).map_err(|_| EncodeError::LabelOffsetOverflow {
                    label: label.0,
                    offset,
                })?;
            Self::patch_signed(&mut self.bytes, operand_start as usize, width, offset_i32)?;
        } else {
            state.pending.push(PendingJump {
                operand_start,
                operand_width: width,
                ref_pc_after,
            });
        }
        Ok(pc)
    }

    /// Attach a [`FeedbackSlot`] to the instruction that starts at `pc`.
    /// Must be called at most once per PC; the builder does not verify
    /// that the PC is an instruction boundary (callers already know it).
    pub fn attach_feedback(&mut self, pc: u32, slot: FeedbackSlot) {
        // Keep entries sorted by ascending PC for binary search at read time.
        self.feedback_entries.push((pc, slot));
    }

    /// Finalize into an immutable [`Bytecode`]. Errors if any label
    /// remained unbound — that would leak placeholder zero operands.
    pub fn finish(mut self) -> Result<Bytecode, EncodeError> {
        for (idx, state) in self.labels.iter().enumerate() {
            if state.target_pc.is_none() && !state.pending.is_empty() {
                return Err(EncodeError::LabelOffsetOverflow {
                    label: idx as u32,
                    offset: 0,
                });
            }
        }
        if self.bytes.len() > u32::MAX as usize {
            return Err(EncodeError::StreamTooLong);
        }
        self.feedback_entries.sort_by_key(|(pc, _)| *pc);
        let feedback = FeedbackMap::from_sorted(self.feedback_entries);
        Ok(Bytecode::new(self.bytes.into_boxed_slice(), feedback))
    }

    // ---------------- internals ----------------

    fn validate_shape(
        opcode: OpcodeV2,
        shape: OperandShape,
        operands: &[Operand],
    ) -> Result<(), EncodeError> {
        // `shape.arity()` is the logical operand count — one entry per
        // `Operand` the caller passes in. `RegList` is one logical operand
        // even though it occupies two byte slots on the wire.
        if operands.len() != shape.arity() {
            return Err(EncodeError::ArityMismatch {
                opcode: opcode.name(),
                expected: shape.arity(),
                actual: operands.len(),
            });
        }
        for (pos, (operand, expected)) in operands.iter().zip(shape.operands()).enumerate() {
            if operand.kind() != *expected {
                return Err(EncodeError::OperandKindMismatch {
                    opcode: opcode.name(),
                    position: pos,
                    expected: *expected,
                    actual: operand.kind(),
                });
            }
        }
        Ok(())
    }

    fn emit_prefix(&mut self, width: OperandWidth) {
        match width {
            OperandWidth::Narrow => {}
            OperandWidth::Wide => self.bytes.push(PREFIX_WIDE),
            OperandWidth::ExtraWide => self.bytes.push(PREFIX_EXTRA_WIDE),
        }
    }

    fn emit_operand(
        &mut self,
        opcode: OpcodeV2,
        position: usize,
        operand: Operand,
        width: OperandWidth,
    ) -> Result<(), EncodeError> {
        match operand {
            Operand::Reg(v) | Operand::Idx(v) => {
                self.emit_unsigned(opcode, position, v, width)
            }
            Operand::Imm(v) | Operand::JumpOff(v) => {
                self.emit_signed(opcode, position, v, width)
            }
            Operand::RegList { base, count } => {
                self.emit_unsigned(opcode, position, base, width)?;
                self.emit_unsigned(opcode, position + 1, count, width)
            }
        }
    }

    fn emit_unsigned(
        &mut self,
        opcode: OpcodeV2,
        position: usize,
        value: u32,
        width: OperandWidth,
    ) -> Result<(), EncodeError> {
        match width {
            OperandWidth::Narrow => {
                let v = u8::try_from(value).map_err(|_| EncodeError::OperandOutOfRange {
                    opcode: opcode.name(),
                    position,
                    value: i64::from(value),
                })?;
                self.bytes.push(v);
            }
            OperandWidth::Wide => {
                let v = u16::try_from(value).map_err(|_| EncodeError::OperandOutOfRange {
                    opcode: opcode.name(),
                    position,
                    value: i64::from(value),
                })?;
                self.bytes.extend_from_slice(&v.to_le_bytes());
            }
            OperandWidth::ExtraWide => {
                self.bytes.extend_from_slice(&value.to_le_bytes());
            }
        }
        Ok(())
    }

    fn emit_signed(
        &mut self,
        opcode: OpcodeV2,
        position: usize,
        value: i32,
        width: OperandWidth,
    ) -> Result<(), EncodeError> {
        match width {
            OperandWidth::Narrow => {
                let v = i8::try_from(value).map_err(|_| EncodeError::OperandOutOfRange {
                    opcode: opcode.name(),
                    position,
                    value: i64::from(value),
                })?;
                self.bytes.push(v as u8);
            }
            OperandWidth::Wide => {
                let v = i16::try_from(value).map_err(|_| EncodeError::OperandOutOfRange {
                    opcode: opcode.name(),
                    position,
                    value: i64::from(value),
                })?;
                self.bytes.extend_from_slice(&v.to_le_bytes());
            }
            OperandWidth::ExtraWide => {
                self.bytes.extend_from_slice(&value.to_le_bytes());
            }
        }
        Ok(())
    }

    fn patch_signed(
        bytes: &mut [u8],
        offset: usize,
        width: OperandWidth,
        value: i32,
    ) -> Result<(), EncodeError> {
        match width {
            OperandWidth::Narrow => {
                let v = i8::try_from(value).map_err(|_| EncodeError::LabelOffsetOverflow {
                    label: u32::MAX,
                    offset: i64::from(value),
                })?;
                bytes[offset] = v as u8;
            }
            OperandWidth::Wide => {
                let v = i16::try_from(value).map_err(|_| EncodeError::LabelOffsetOverflow {
                    label: u32::MAX,
                    offset: i64::from(value),
                })?;
                let encoded = v.to_le_bytes();
                bytes[offset..offset + 2].copy_from_slice(&encoded);
            }
            OperandWidth::ExtraWide => {
                let encoded = value.to_le_bytes();
                bytes[offset..offset + 4].copy_from_slice(&encoded);
            }
        }
        Ok(())
    }
}

/// Pick the single operand width that covers every operand of one
/// instruction.
fn chosen_width(operands: &[Operand]) -> OperandWidth {
    let mut w = OperandWidth::Narrow;
    for op in operands {
        w = w.max(op.min_width());
    }
    w
}
