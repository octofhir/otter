//! Native frame geometry and labels shared by every machine backend.
//!
//! # Contents
//! - [`SpillArea`] describes initialized canonical homes and their root map.
//! - [`ActivationExits`] names constructor completion and interpreter exits.
//! - [`CallEntryCold`] allocates labels for the entry's semantic continuations.
//!
//! # Invariants
//! - Tagged homes precede untagged homes; the area's last 16 bytes preserve
//!   the interpreter's previous root words during a tier entry.
//! - Labels describe the same VM frame protocol on every architecture.
//! - Only baseline entries account source work toward promotion.
//!
//! # See also
//! - [`crate::call_linkage::EntryShape`] owns static call semantics.
//! - `crate::arm64::frame` and `crate::x86_64::frame` emit the machine layout.

use dynasmrt::{DynamicLabel, relocations::Relocation};
use otter_vm::native_abi as abi;

use crate::call_linkage::EntryShape;

/// Source work one entered Template activation charges on entry: its opcode
/// count, the interrupt-budget weight of one pass. Loops charge at polls.
pub(crate) fn entry_work(view: &otter_vm::JitCompileSnapshot) -> u64 {
    view.instructions.len().max(1) as u64
}

/// Tier-owned canonical homes below the register window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SpillArea {
    /// Aligned reservation including 16 bytes for saved interpreter root words.
    pub(crate) bytes: u32,
    /// The exception scratch: the one tagged home the entry record roots,
    /// zeroed before the frame is published. Every other home is rooted only
    /// by records of boundaries that wrote it.
    pub(crate) scratch_slot: Option<u32>,
    /// The entry record, rooting only the scratch.
    pub(crate) safepoint: abi::SafepointId,
    /// AArch64 callee-saved pairs the body allocates, bit `i` for
    /// `x(22 + 2i)`/`x(23 + 2i)`; saved below the frame pointer at entry and
    /// restored when the frame is released.
    pub(crate) saved_pairs: u8,
}

impl SpillArea {
    /// No tier-owned homes: the record roots only its register window.
    pub(crate) const NONE: Self = Self {
        bytes: 0,
        scratch_slot: None,
        safepoint: abi::NO_SAFEPOINT,
        saved_pairs: 0,
    };

    /// The callee-saved pairs covering the general registers in `used`.
    pub(crate) fn saved_pairs_for(used: u32) -> u8 {
        (0..4u8)
            .filter(|&pair| used & (0b11 << (22 + 2 * pair)) != 0)
            .fold(0, |mask, pair| mask | 1 << pair)
    }
}

/// Shared completion labels for one native body.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ActivationExits {
    /// Constructor result completion, then return to the caller.
    pub(crate) construct: DynamicLabel,
    /// Interpreter continuation with the encoded semantic exit.
    pub(crate) side_exit: DynamicLabel,
}

/// Cold entry continuations, each paired with its hot resumption label.
#[derive(Debug, Clone, Copy)]
pub(crate) struct CallEntryCold {
    pub(crate) overflow: DynamicLabel,
    pub(crate) break_even: Option<(DynamicLabel, DynamicLabel)>,
    pub(crate) promote: Option<(DynamicLabel, DynamicLabel)>,
    pub(crate) construct: Option<(DynamicLabel, DynamicLabel)>,
    pub(crate) prepare: Option<(DynamicLabel, DynamicLabel)>,
    pub(crate) underarity: Option<(DynamicLabel, DynamicLabel)>,
}

impl CallEntryCold {
    /// Allocate exactly the continuations required by this entry shape.
    pub(crate) fn new<R: Relocation>(ops: &mut dynasmrt::Assembler<R>, shape: EntryShape) -> Self {
        let mut pair = || (ops.new_dynamic_label(), ops.new_dynamic_label());
        let break_even = shape.counts_entries().then(&mut pair);
        let promote = shape.counts_entries().then(&mut pair);
        let construct = shape.base_constructor().then(&mut pair);
        let prepare = shape.sloppy_receiver().then(&mut pair);
        let underarity = (shape.lazy_window && shape.param_count != 0).then(&mut pair);
        Self {
            overflow: ops.new_dynamic_label(),
            break_even,
            promote,
            construct,
            prepare,
            underarity,
        }
    }
}

#[cfg(test)]
#[path = "frame/constructor_terminal_tests.rs"]
mod constructor_terminal_tests;
