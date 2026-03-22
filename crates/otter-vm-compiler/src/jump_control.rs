//! Jump control flow tracking for break/continue/return across finally boundaries.
//!
//! This module implements a system modeled after Boa's `JumpRecord`/`JumpControlInfo`
//! architecture, which correctly handles ES2023 §14.12 (switch), §14.15 (try),
//! §14.7 (iteration), and §14.13 (labelled) statements.
//!
//! The key insight: when `break`, `continue`, or `return` must cross a `finally`
//! boundary, the control transfer is deferred. The compiler emits code to record
//! the intended action in a register and jump to the finally entry. After the
//! finally block executes, a `JumpTable` instruction dispatches to the correct
//! post-finally target.
//!
//! ## Architecture
//!
//! - [`JumpControlInfo`] — one per loop/switch/try-finally/labelled scope, pushed
//!   onto `Compiler::jump_info`.
//! - [`JumpRecord`] — created by break/continue/return, carries a chain of
//!   [`JumpRecordAction`]s that describe how to reach the final target.
//! - [`JumpRecordAction`] — individual steps: close iterator, handle finally, transfer.

use otter_vm_bytecode::operand::Register;

// ─── JumpRecordAction ──────────────────────────────────────────────────────

/// A single action to perform while processing a break/continue/return
/// as it "bubbles up" through nested control flow scopes.
#[derive(Debug, Clone, Copy)]
pub(crate) enum JumpRecordAction {
    /// Emit `IteratorClose` for the given iterator register.
    /// Needed when break/continue/return exits through a for-of loop.
    CloseIterator { iter: Register, is_async: bool },

    /// Record the finally-dispatch index into `finally_index_reg` and clear
    /// the rethrow flag, then Transfer to the enclosing TryWithFinally info.
    ///
    /// Per ES2023 §14.15.3: abrupt completions from break/continue/return
    /// must execute the finally block before performing the actual transfer.
    HandleFinally {
        /// Which slot in the JumpTable this jump will occupy.
        /// Equals `info.pending_jumps.len()` at the time the action is built.
        table_index: u32,
        /// Register holding the "should rethrow?" boolean flag.
        finally_rethrow_reg: Register,
        /// Register holding the jump-table dispatch index.
        finally_index_reg: Register,
    },

    /// Emit a Jump placeholder and delegate the remaining actions to
    /// the `JumpControlInfo` at `jump_info[info_index]`.
    Transfer { info_index: u32 },
}

// ─── JumpRecordKind ────────────────────────────────────────────────────────

/// Whether this is a break, continue, or return.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JumpRecordKind {
    Break,
    Continue,
    Return,
}

// ─── JumpRecord ────────────────────────────────────────────────────────────

/// One pending control-flow transfer, carrying the actions still needed
/// to reach its final destination.
///
/// Actions are processed right-to-left (popped from the end). When a
/// `Transfer` action is encountered, the record is delegated to the
/// target `JumpControlInfo` and processing stops.
#[derive(Debug, Clone)]
pub(crate) struct JumpRecord {
    pub(crate) kind: JumpRecordKind,
    /// Instruction index of the Jump placeholder emitted by Transfer.
    /// Starts as `usize::MAX` (sentinel) until a Transfer action emits it.
    pub(crate) jump_index: usize,
    /// Actions remaining, processed right-to-left (pop from end).
    pub(crate) actions: Vec<JumpRecordAction>,
    /// For Return kind: the register holding the saved return value.
    pub(crate) return_value_reg: Option<Register>,
}

impl JumpRecord {
    pub(crate) fn new(kind: JumpRecordKind, actions: Vec<JumpRecordAction>) -> Self {
        Self {
            kind,
            jump_index: usize::MAX,
            actions,
            return_value_reg: None,
        }
    }

    pub(crate) fn new_return(actions: Vec<JumpRecordAction>, return_value_reg: Register) -> Self {
        Self {
            kind: JumpRecordKind::Return,
            jump_index: usize::MAX,
            actions,
            return_value_reg: Some(return_value_reg),
        }
    }
}

// ─── JumpControlFlags ──────────────────────────────────────────────────────

/// Bitflags describing what kind of control scope a `JumpControlInfo` represents.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct JumpControlFlags(u8);

impl JumpControlFlags {
    pub const LOOP: u8 = 0b0000_0001;
    pub const SWITCH: u8 = 0b0000_0010;
    pub const TRY_FINALLY: u8 = 0b0000_0100;
    pub const IN_FINALLY: u8 = 0b0000_1000;
    pub const LABELLED: u8 = 0b0001_0000;
    pub const ITERATOR_LOOP: u8 = 0b0010_0000;
    pub const FOR_AWAIT: u8 = 0b0100_0000;

    pub fn contains(self, flag: u8) -> bool {
        self.0 & flag != 0
    }
    pub fn set(&mut self, flag: u8) {
        self.0 |= flag;
    }
}

// ─── JumpControlInfo ───────────────────────────────────────────────────────

/// Tracks control flow context for one loop/switch/try-finally/labelled scope.
///
/// Pushed onto `Compiler::jump_info` when entering a new control scope.
/// When popped, all `pending_jumps` are finalized (patches emitted).
#[derive(Debug)]
pub(crate) struct JumpControlInfo {
    /// Optional label (for labelled statements).
    pub(crate) label: Option<String>,
    /// Instruction index of the loop's continue target (start-of-iteration).
    /// Only meaningful for loops; 0 for switch/try/labelled.
    pub(crate) start_index: usize,
    /// Type flags.
    pub(crate) flags: JumpControlFlags,
    /// JumpRecords delegated here via Transfer, waiting to be finalized.
    pub(crate) pending_jumps: Vec<JumpRecord>,
    /// For TRY_FINALLY: `(finally_rethrow_reg, finally_index_reg)`.
    pub(crate) finally_regs: Option<(Register, Register)>,
    /// For ITERATOR_LOOP: the iterator register to close on break/return.
    pub(crate) iterator_reg: Option<Register>,
}

impl JumpControlInfo {
    pub(crate) fn new() -> Self {
        Self {
            label: None,
            start_index: 0,
            flags: JumpControlFlags::default(),
            pending_jumps: Vec::new(),
            finally_regs: None,
            iterator_reg: None,
        }
    }

    pub(crate) fn is_loop(&self) -> bool {
        self.flags.contains(JumpControlFlags::LOOP)
    }
    pub(crate) fn is_switch(&self) -> bool {
        self.flags.contains(JumpControlFlags::SWITCH)
    }
    pub(crate) fn is_try_finally(&self) -> bool {
        self.flags.contains(JumpControlFlags::TRY_FINALLY)
    }
    pub(crate) fn in_finally(&self) -> bool {
        self.flags.contains(JumpControlFlags::IN_FINALLY)
    }
    pub(crate) fn is_iterator_loop(&self) -> bool {
        self.flags.contains(JumpControlFlags::ITERATOR_LOOP)
    }
    pub(crate) fn is_for_await(&self) -> bool {
        self.flags.contains(JumpControlFlags::FOR_AWAIT)
    }
}
