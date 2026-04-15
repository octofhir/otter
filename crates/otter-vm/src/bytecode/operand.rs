//! Operand kinds, widths, and the in-memory `Operand` representation.
//!
//! Each instruction has a fixed-arity list of operand *kinds*, and an
//! *active width* chosen by the `Wide` / `ExtraWide` prefix. See
//! `docs/bytecode-v2.md` §4.2 for the rationale.

/// What a single operand slot means logically. Width is picked at
/// encode/decode time based on the active prefix.
#[allow(missing_docs)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OperandKind {
    /// `BytecodeRegister` index into the current frame. Unsigned.
    Reg,
    /// Signed immediate. i8 / i16 / i32 depending on width.
    Imm,
    /// Index into a side table (constant pool, string pool, closure
    /// template, etc.). Unsigned.
    Idx,
    /// Signed jump offset. Base is the first byte *after* this instruction.
    JumpOff,
    /// `(base: Reg, count: Reg-unsigned-width)` pair describing a
    /// contiguous outgoing argument window. Encoded as two operand slots.
    RegList,
}

/// Width of operand slots for the next instruction. Set by the absence
/// or presence of a prefix byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperandWidth {
    /// Default: operands are 1 byte each.
    Narrow,
    /// After a `Wide` prefix: operands are 2 bytes each (little-endian).
    Wide,
    /// After an `ExtraWide` prefix: operands are 4 bytes each.
    ExtraWide,
}

impl OperandWidth {
    /// Number of bytes one operand slot consumes under this width.
    #[must_use]
    pub const fn bytes_per_operand(self) -> usize {
        match self {
            OperandWidth::Narrow => 1,
            OperandWidth::Wide => 2,
            OperandWidth::ExtraWide => 4,
        }
    }

    /// Upgrade to the narrowest width that fits `value` as an unsigned
    /// operand.
    #[must_use]
    pub const fn min_for_unsigned(value: u32) -> Self {
        if value <= u8::MAX as u32 {
            OperandWidth::Narrow
        } else if value <= u16::MAX as u32 {
            OperandWidth::Wide
        } else {
            OperandWidth::ExtraWide
        }
    }

    /// Upgrade to the narrowest width that fits `value` as a signed
    /// operand.
    #[must_use]
    pub const fn min_for_signed(value: i32) -> Self {
        if value >= i8::MIN as i32 && value <= i8::MAX as i32 {
            OperandWidth::Narrow
        } else if value >= i16::MIN as i32 && value <= i16::MAX as i32 {
            OperandWidth::Wide
        } else {
            OperandWidth::ExtraWide
        }
    }

    /// Widest of two widths — used to pick the single width that covers
    /// *all* operands of an instruction.
    #[must_use]
    pub const fn max(self, other: Self) -> Self {
        match (self, other) {
            (OperandWidth::ExtraWide, _) | (_, OperandWidth::ExtraWide) => OperandWidth::ExtraWide,
            (OperandWidth::Wide, _) | (_, OperandWidth::Wide) => OperandWidth::Wide,
            _ => OperandWidth::Narrow,
        }
    }
}

/// A single operand in its encoder-input form. The encoder decides what
/// width the whole instruction gets, then serializes each operand into
/// that width. Over-wide values are an [`EncodeError::OperandOutOfRange`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operand {
    /// A frame register, by user-visible index.
    Reg(u32),
    /// A signed immediate. Width picked by encoder based on magnitude.
    Imm(i32),
    /// An index into a side table (constants, strings, etc.).
    Idx(u32),
    /// A jump offset, relative to the first byte after the jump instruction.
    JumpOff(i32),
    /// A register window — `{base, count}`.
    RegList { base: u32, count: u32 },
}

impl Operand {
    /// Kind this operand satisfies. Must match the opcode's static shape
    /// at encode time.
    #[must_use]
    pub const fn kind(&self) -> OperandKind {
        match self {
            Operand::Reg(_) => OperandKind::Reg,
            Operand::Imm(_) => OperandKind::Imm,
            Operand::Idx(_) => OperandKind::Idx,
            Operand::JumpOff(_) => OperandKind::JumpOff,
            Operand::RegList { .. } => OperandKind::RegList,
        }
    }

    /// Narrowest width that encodes this operand without loss. `RegList`
    /// picks the width that fits both fields.
    #[must_use]
    pub const fn min_width(&self) -> OperandWidth {
        match self {
            Operand::Reg(v) | Operand::Idx(v) => OperandWidth::min_for_unsigned(*v),
            Operand::Imm(v) | Operand::JumpOff(v) => OperandWidth::min_for_signed(*v),
            Operand::RegList { base, count } => {
                OperandWidth::min_for_unsigned(*base).max(OperandWidth::min_for_unsigned(*count))
            }
        }
    }

    /// Number of operand *slots* this operand consumes in the encoded
    /// stream. `RegList` consumes two slots (base + count), everything
    /// else consumes one.
    #[must_use]
    pub const fn slot_count(&self) -> usize {
        match self {
            Operand::RegList { .. } => 2,
            _ => 1,
        }
    }
}
