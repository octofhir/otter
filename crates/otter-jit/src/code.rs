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
//! - Logical instruction bytes and retained mapping capacity are distinct:
//!   artifacts and entry bounds use the former, resource admission the latter.
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

    /// Raw pointer to an arbitrary byte `offset` within the compiled code.
    ///
    /// Used for alternate (OSR) entry points: a loop-header trampoline emitted
    /// after the main body. `offset` must be an assembler offset into this
    /// buffer.
    ///
    /// # Safety
    /// Same contract as [`Self::entry_ptr`]: the caller transmutes the pointer
    /// to the emitted calling convention and invokes it only while `self` is
    /// alive. `offset` must be in bounds of the finalized mapping.
    #[must_use]
    pub unsafe fn ptr_at(&self, offset: usize) -> *const u8 {
        self.buf.ptr(AssemblyOffset(offset))
    }

    /// Number of finalized instruction bytes, excluding unused mapping capacity.
    #[must_use]
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// Backing capacity retained by the executable mapping, including its
    /// unused tail. This is the owner's requested mapping size, independent
    /// of the instruction length and of process resident-memory measurements.
    #[must_use]
    pub fn retained_mapping_bytes(&self) -> usize {
        self.buf.size()
    }

    /// `true` when the compiled code mapping is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.buf.len() == 0
    }

    /// Borrow the exact finalized machine-code bytes.
    ///
    /// Artifact capture clones this slice only when explicitly requested.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.buf
    }

    /// Main entry offset within the finalized mapping.
    #[must_use]
    pub const fn entry_offset(&self) -> usize {
        self.entry
    }
}

#[cfg(test)]
mod tests {
    use dynasmrt::DynasmApi;

    use super::CompiledCode;

    #[test]
    fn retained_mapping_counts_reserved_tail_without_changing_code_bytes() {
        // Emission is target independent; this byte is inspected, never run.
        let mut assembler = dynasmrt::x64::Assembler::new_with_capacity(32_768).unwrap();
        let entry = assembler.offset();
        assembler.push(0x90);
        let buffer = assembler.finalize().unwrap();
        let reserved = buffer.size();
        assert!(reserved >= 32_768);
        let code = CompiledCode::new(buffer, entry);
        assert_eq!(code.len(), 1);
        assert_eq!(code.bytes(), &[0x90]);
        assert_eq!(code.retained_mapping_bytes(), reserved);
        assert!(code.retained_mapping_bytes() > code.len());
    }
}
