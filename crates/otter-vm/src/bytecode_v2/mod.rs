//! Ignition-style accumulator bytecode (Phase C, v2).
//!
//! This is the **in-progress** second-generation bytecode ISA. Gated behind
//! the `bytecode_v2` Cargo feature; when the feature is off, this module
//! is entirely unreachable and has no effect on shipping artifacts.
//!
//! The full design lives in [`docs/bytecode-v2.md`](../../../../docs/bytecode-v2.md).
//! This module is Phase 1: self-contained ISA library (opcodes, encoder,
//! decoder, PC→feedback-slot side table) with unit tests. It does **not**
//! yet wire into the AST lowering, interpreter, or JIT — those are Phase 2,
//! 3, and 4 respectively.
//!
//! ## Contract for callers
//!
//! The encoder (`BytecodeBuilder`) and the decoder (`Bytecode::iter()`)
//! together guarantee:
//!
//! 1. **Round-trip**: every instruction emitted via the builder decodes
//!    back to the same logical instruction, regardless of operand width.
//! 2. **Variable-width discipline**: the builder picks the narrowest
//!    operand width that fits every operand of an instruction, auto-
//!    prepending `Wide` / `ExtraWide` prefixes as needed.
//! 3. **Sparse feedback slots**: [`FeedbackMap`] carries `pc → FeedbackSlotId`
//!    in a compact, binary-searchable `Vec<(u32, u16)>`. Instructions
//!    without a feedback slot do not pay for a map entry.
//!
//! ## V8 Ignition alignment
//!
//! - Prefix bytes `Wide = 0xFE`, `ExtraWide = 0xFF` apply to *all* operands
//!   of the immediately following instruction ([Ignition spec](https://chromium.googlesource.com/external/github.com/v8/v8.wiki/+/69cdcc46450fe609426180fbc5524ea0ecba76d5/Ignition-Bytecode-Format.md)).
//! - `JumpOff` is relative to the first byte after the jump instruction.
//! - `RegList(base, count)` addresses a contiguous outgoing argument window.

mod decoding;
mod dispatch_v2;
mod encoding;
mod feedback_map;
mod opcodes;
mod operand;
mod transpile;

#[cfg(test)]
mod tests;

pub use decoding::{DecodeError, Instruction, InstructionIter};
pub use dispatch_v2::{ExecError, Frame, execute};
pub use encoding::{BytecodeBuilder, EncodeError, Label};
pub use feedback_map::{FeedbackMap, FeedbackSlot};
pub use opcodes::{OpcodeV2, OperandShape};
pub use operand::{Operand, OperandKind, OperandWidth};
pub use transpile::{TranspileError, transpile, transpile_with_function};

/// Prefix byte values. V8 Ignition convention: `Wide` promotes operands of
/// the next instruction to 2 bytes, `ExtraWide` to 4.
pub const PREFIX_WIDE: u8 = 0xFE;
pub const PREFIX_EXTRA_WIDE: u8 = 0xFF;

/// Compiled function bytecode — byte stream plus a sparse PC → feedback
/// slot table. No other side data lives here; constant pools, closure
/// templates, deopt descriptors, etc. are owned by the `Function`
/// metadata (same as v1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bytecode {
    bytes: Box<[u8]>,
    feedback: FeedbackMap,
}

impl Bytecode {
    /// Raw byte stream. Valid starting PC is 0; valid ending PC is
    /// `bytes().len()`. Callers iterate via [`Self::iter`] or index
    /// directly through an [`InstructionIter`].
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Sparse PC → [`FeedbackSlot`] table. Returns `None` when no feedback
    /// slot is associated with `pc`.
    #[must_use]
    pub fn feedback(&self) -> &FeedbackMap {
        &self.feedback
    }

    /// Iterate instructions from the start.
    #[must_use]
    pub fn iter(&self) -> InstructionIter<'_> {
        InstructionIter::new(&self.bytes)
    }

    /// Internal constructor used by [`BytecodeBuilder::finish`].
    pub(crate) fn new(bytes: Box<[u8]>, feedback: FeedbackMap) -> Self {
        Self { bytes, feedback }
    }
}
