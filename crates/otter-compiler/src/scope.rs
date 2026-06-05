//! Lexical binding and control-flow frame data used by function lowering.
//!
//! # Contents
//! - binding storage metadata
//! - scope binding records
//! - loop and switch patch lists
//!
//! # Invariants
//! - Register and upvalue storage decisions are compile-time only.
//!
//! # See also
//! - `function_context` for operations over these records

use crate::*;

/// One lexical scope's binding table. The compiler keeps a stack
/// of these so block-scoped `let`/`const` shadow correctly.
#[derive(Debug, Default)]
pub(crate) struct Scope {
    /// Map from binding name → register index (locals + scratch
    /// share one window in the foundation slice; locals occupy the
    /// low end).
    pub(crate) bindings: HashMap<String, BindingInfo>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct BindingInfo {
    /// Backing storage. Foundation uses register-only locals for
    /// non-captured names and an own-upvalue cell for names some
    /// inner function references (see [`capture`]).
    pub(crate) storage: BindingStorage,
    /// `true` for `const` declarations.
    pub(crate) is_const: bool,
    /// Whether the binding has been definitely initialized at the
    /// current compile point. `let x;` and `let x = init` start at
    /// `false` and flip to `true` after the initializer's
    /// `StoreLocal` / `StoreUpvalue`. Reads before that emit
    /// `Op::TdzError`.
    pub(crate) initialized: bool,
}

/// Where a binding lives in the running frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BindingStorage {
    /// Plain register. Read with `LoadLocal`, written with
    /// `StoreLocal`.
    Register { reg: u16 },
    /// Own-upvalue cell at index `idx` in `frame.upvalues`. Used
    /// for any binding some inner function captures. Read /
    /// written with `LoadUpvalue` / `StoreUpvalue`.
    Upvalue { idx: u16 },
}

impl BindingStorage {
    pub(crate) fn to_argument_storage(self) -> ArgumentBindingStorage {
        match self {
            Self::Register { reg } => ArgumentBindingStorage::Register { reg },
            Self::Upvalue { idx } => ArgumentBindingStorage::Upvalue { idx },
        }
    }
}

/// One pending control-flow target so `break` / `continue` can patch
/// their offsets at scope close.
///
/// Tracks both real loops (`for` / `while` / `do-while` / `for-of` /
/// `for-in`) and pseudo-loops (`switch` body — only `break` is
/// legal, `continue` skips switch frames per spec §13.10.1).
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-iteration-statements>
/// - <https://tc39.es/ecma262/#sec-switch-statement>
/// - <https://tc39.es/ecma262/#sec-labelled-statements>
#[derive(Debug, Default)]
pub(crate) struct LoopFrame {
    /// Instruction PCs where `continue` emitted a placeholder
    /// JUMP. Patched to point at the loop's continue target (the
    /// update / test).
    pub(crate) continue_patches: Vec<u32>,
    /// Instruction PCs where `break` emitted a placeholder JUMP.
    /// Patched to point at the instruction after the loop body.
    pub(crate) break_patches: Vec<u32>,
    /// Optional label attached to this frame by an enclosing
    /// `LabeledStatement`. `break label;` matches against this
    /// field walking outward; `continue label;` only matches
    /// when [`LoopFrame::is_real_loop`] is true.
    pub(crate) label: Option<String>,
    /// `true` when the frame represents an iteration statement.
    /// `false` for a `switch` body, where `continue` must skip the
    /// frame and target the enclosing loop instead.
    pub(crate) is_real_loop: bool,
    /// For a `for…of` frame, the register holding the iterator that
    /// must be closed (§7.4.9 IteratorClose) when an abrupt
    /// completion (`break` / labelled `continue` / `return`) exits the
    /// loop. `None` for every other loop / switch frame.
    pub(crate) iterator_close_reg: Option<u16>,
    /// Runtime try-handler-stack depth in effect when this frame was
    /// entered. A `break`/`continue` targeting this frame must run
    /// every `finally` pushed since (handlers above this floor).
    pub(crate) handler_floor: u32,
    /// `finally`-handler count in effect at entry; a target whose
    /// `active_finally` exceeds this needs the finally-routing
    /// `break`/`continue` opcode.
    pub(crate) finally_floor: u32,
}

impl LoopFrame {
    pub(crate) fn iteration() -> Self {
        Self {
            continue_patches: Vec::new(),
            break_patches: Vec::new(),
            label: None,
            is_real_loop: true,
            iterator_close_reg: None,
            handler_floor: 0,
            finally_floor: 0,
        }
    }

    pub(crate) fn switch_body() -> Self {
        Self {
            continue_patches: Vec::new(),
            break_patches: Vec::new(),
            label: None,
            is_real_loop: false,
            iterator_close_reg: None,
            handler_floor: 0,
            finally_floor: 0,
        }
    }
}
