//! Ownership wrapper for finalized JIT machine code.
//!
//! # Contents
//! - [`CompiledCode`] — owns a finalized W^X executable mapping plus its entry
//!   offset, and hands out a raw entry pointer for the caller to transmute and
//!   call.
//!
//! # Invariants
//! - The executable mapping lives exactly as long as the [`CompiledCode`]; the
//!   entry pointer is invalid after drop. Callers must keep the value alive for
//!   the duration of any call into the code.
//!
//! # See also
//! - [`crate`] — the JIT tier and its rooting/`unsafe` contract.

use dynasmrt::{AssemblyOffset, ExecutableBuffer};

/// A finalized block of JIT-emitted machine code plus the byte offset of its
/// entry point. Owns the underlying executable mapping; dropping it frees the
/// mapping and invalidates any entry pointer handed out by [`Self::entry_ptr`].
pub struct CompiledCode {
    buf: ExecutableBuffer,
    entry: usize,
}

impl CompiledCode {
    /// Wrap a finalized assembler buffer and the entry offset within it.
    // Constructed by the bytecode→machine-code compiler (landing next) and by the
    // in-crate toolchain tests; allow until the compiler entry point lands.
    #[allow(dead_code)]
    #[must_use]
    pub(crate) fn new(buf: ExecutableBuffer, entry: AssemblyOffset) -> Self {
        Self {
            buf,
            entry: entry.0,
        }
    }

    /// Raw pointer to the entry instruction of the compiled code.
    ///
    /// # Safety
    /// The caller must transmute the returned pointer to a function signature
    /// matching the emitted code's calling convention, and must only invoke it
    /// while `self` is alive (the mapping is freed on drop).
    #[must_use]
    pub unsafe fn entry_ptr(&self) -> *const u8 {
        // `entry` is an offset produced by the assembler for this buffer, so it
        // is in bounds of the finalized mapping. `ExecutableBuffer::ptr` is a
        // safe accessor; the unsafety this method documents is the caller's
        // transmute + call-while-alive contract.
        self.buf.ptr(AssemblyOffset(self.entry))
    }

    /// Size in bytes of the finalized code mapping.
    #[must_use]
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// `true` when the compiled code mapping is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.buf.len() == 0
    }
}
