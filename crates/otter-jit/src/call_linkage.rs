//! Architecture-neutral contract of generated JavaScript calls.
//!
//! # Contents
//! - [`CallTarget`] — where a generated call goes.
//! - [`EntryShape`] — static call semantics of one compiled function and its
//!   record's second header word.
//! - [`pushed_argument_bytes`] — the caller-stack bytes of one actual span.
//!
//! # Invariants
//! - One call ABI enters every bytecode function generation: the context,
//!   the callee, the receiver as given, `new.target` (`undefined` exactly for
//!   `[[Call]]`), the actual count and the actual span on the caller's stack,
//!   padded with `undefined` to the callee's formal count. A compiled
//!   generation's entry builds, publishes and retires its one native frame;
//!   the caller pops the span.
//! - A caller that proved its callee enters the current generation through
//!   the target's permanent `FunctionEntryCell`; any other callee enters the
//!   generic entry, which classifies it in the call trampoline.
//! - Receiver binding belongs to the callee: an arrow's lexical `this`, a
//!   strict or unobserved receiver as given, a sloppy object as given and any
//!   other sloppy receiver through activation preparation; a base
//!   constructor creates a missing receiver, a derived constructor binds the
//!   hole.
//! - The completion is `Success`, `Throw` or `Fatal`; side exits never cross
//!   a call.
//!
//! # See also
//! - `crate::arm64::js_call` — the AArch64 caller.
//! - `crate::arm64::activation` — the AArch64 callee entry pieces.
//! - `otter_vm::native_abi::call_trampoline` — classification.

use otter_vm::{
    JitCompileSnapshot,
    native_abi::{
        FUNCTION_CALL_CONSTRUCTIBLE, FUNCTION_CALL_DERIVED_CONSTRUCTOR, FUNCTION_CALL_LEXICAL_THIS,
        FUNCTION_CALL_NO_RECEIVER_CONVERSION, NativeFrameFlags, NativeFrameKind,
    },
};

use crate::entry::Unsupported;

/// Where a generated call goes.
#[derive(Debug, Clone, Copy)]
pub(crate) enum CallTarget {
    /// The current generation of a proven bytecode target, entered through
    /// its permanent `FunctionEntryCell`.
    Known {
        /// Address of the target's `FunctionEntryCell`.
        entry_cell: u64,
        /// The target's function id, the cell's relocation identity.
        function_id: u32,
    },
    /// Classification of any callee.
    Generic,
}

/// Largest caller-stack actual span one call pushes.
pub(crate) const MAX_PUSHED_ARGUMENT_BYTES: u32 = 4_080;

/// 16-aligned caller-stack bytes for `count` pushed actuals.
pub(crate) fn pushed_argument_bytes(count: usize) -> Result<u32, Unsupported> {
    u32::try_from(count)
        .ok()
        .and_then(|count| count.checked_mul(8))
        .map(|bytes| (bytes + 15) & !15)
        .filter(|&bytes| bytes <= MAX_PUSHED_ARGUMENT_BYTES)
        .ok_or(Unsupported::OperandShape("call actual span"))
}

/// Static call semantics of one compiled function.
#[derive(Debug, Clone, Copy)]
pub(crate) struct EntryShape {
    pub(crate) function_id: u32,
    pub(crate) code_object_id: u32,
    pub(crate) register_count: u16,
    pub(crate) param_count: u16,
    pub(crate) kind: NativeFrameKind,
    pub(crate) has_safepoints: bool,
    pub(crate) constructible: bool,
    pub(crate) derived: bool,
    pub(crate) lexical_this: bool,
    pub(crate) converts_receiver: bool,
}

impl EntryShape {
    pub(crate) fn of(
        view: &JitCompileSnapshot,
        code_object_id: u64,
        kind: NativeFrameKind,
        has_safepoints: bool,
    ) -> Result<Self, Unsupported> {
        let function = &view.code_block;
        let flags = function.call_flags();
        Ok(Self {
            function_id: function.id,
            code_object_id: u32::try_from(code_object_id)
                .map_err(|_| Unsupported::OperandShape("code object id"))?,
            register_count: function.register_count,
            param_count: function.param_count,
            kind,
            has_safepoints,
            constructible: flags & FUNCTION_CALL_CONSTRUCTIBLE != 0,
            derived: flags & FUNCTION_CALL_DERIVED_CONSTRUCTOR != 0,
            lexical_this: flags & FUNCTION_CALL_LEXICAL_THIS != 0,
            converts_receiver: flags & FUNCTION_CALL_NO_RECEIVER_CONVERSION == 0,
        })
    }

    /// A base constructor creates its receiver when constructed.
    pub(crate) fn base_constructor(self) -> bool {
        self.constructible && !self.derived
    }

    /// A sloppy body binding its receiver: objects as is, anything else
    /// through activation preparation.
    pub(crate) fn sloppy_receiver(self) -> bool {
        self.converts_receiver && !self.lexical_this && !self.derived
    }

    /// Whether a return can owe constructor completion.
    pub(crate) fn completes_construct(self) -> bool {
        self.constructible || self.derived
    }

    /// The record's second header word: register count, tier, flags and
    /// generation.
    pub(crate) fn header_word(self, window_slots: u16) -> u64 {
        let mut flags = 0u8;
        if self.has_safepoints {
            flags |= NativeFrameFlags::HAS_SAFEPOINTS;
        }
        if self.derived {
            flags |= NativeFrameFlags::DERIVED_CONSTRUCTOR | NativeFrameFlags::CONSTRUCT;
        }
        u64::from(window_slots)
            | (u64::from(self.kind as u8) << 16)
            | (u64::from(flags) << 24)
            | (u64::from(self.code_object_id) << 32)
    }
}
