//! Ownership wrapper for finalized JIT machine code.
//!
//! # Contents
//! - [`CompiledCode`] — owns one code object placed in its compiler's
//!   [`CodeMemory`] plus its entry offset, and hands out a raw entry pointer
//!   for the caller to transmute and call.
//!
//! # Invariants
//! - The placed bytes live exactly as long as the [`CompiledCode`]; the entry
//!   pointer is invalid after drop. Callers must keep the value alive for the
//!   duration of any call into the code.
//! - Emitted code is position independent: it names its own bytes only
//!   through PC-relative forms and everything else through absolute
//!   addresses, so the assembler's finalized bytes run unchanged wherever
//!   they are placed.
//! - Logical instruction bytes and retained placement are distinct: artifacts
//!   and entry bounds use the former, resource admission the latter.
//!
//! # See also
//! - [`crate`] — the JIT tier and its rooting/`unsafe` contract.
//! - [`crate::code_memory`] — placement and the write protocol.

use std::sync::Arc;

use dynasmrt::{AssemblyOffset, ExecutableBuffer};

use crate::code_memory::{CodeAllocation, CodeMemory};
use crate::{BackendFailure, Unsupported};

/// A finalized block of JIT-emitted machine code plus the byte offset of its
/// entry point. Owns its placement; dropping it returns the bytes to the
/// compiler's memory and invalidates any entry pointer handed out by
/// [`Self::entry_ptr`].
#[derive(Debug)]
pub struct CompiledCode {
    code: CodeAllocation,
    entry: usize,
    /// The routine set this code calls, kept alive as long as the code.
    routines: Option<Arc<crate::routines::Routines>>,
}

impl CompiledCode {
    /// Place a finalized assembler buffer in `memory`, with the entry offset
    /// within it. The assembler's own mapping is released here.
    pub(crate) fn new(
        memory: &Arc<CodeMemory>,
        buf: ExecutableBuffer,
        entry: AssemblyOffset,
    ) -> Result<Self, Unsupported> {
        let code = memory
            .place(&buf)
            .ok_or(Unsupported::Backend(BackendFailure::Finalization))?;
        Ok(Self {
            code,
            entry: entry.0,
            routines: None,
        })
    }

    /// `(address, length, name)` of the placed routines this code calls.
    pub(crate) fn routine_symbols(&self) -> Vec<(u64, usize, &'static str)> {
        self.routines
            .as_ref()
            .map(|routines| routines.symbols())
            .unwrap_or_default()
    }

    /// Keep `routines` alive for as long as this code, which calls them.
    pub(crate) fn retain_routines(&mut self, routines: Option<Arc<crate::routines::Routines>>) {
        self.routines = routines;
    }

    /// Raw pointer to the entry instruction of the compiled code.
    ///
    /// # Safety
    /// The caller must transmute the returned pointer to a function signature
    /// matching the emitted code's calling convention, and must only invoke it
    /// while `self` is alive (the placement is returned on drop).
    #[must_use]
    pub unsafe fn entry_ptr(&self) -> *const u8 {
        self.code.ptr().wrapping_add(self.entry)
    }

    /// Raw pointer to an arbitrary byte `offset` within the compiled code.
    ///
    /// Used for alternate (OSR) entry points: a loop-header trampoline emitted
    /// after the main body. `offset` must be an assembler offset into this
    /// code.
    ///
    /// # Safety
    /// Same contract as [`Self::entry_ptr`]: the caller transmutes the pointer
    /// to the emitted calling convention and invokes it only while `self` is
    /// alive. `offset` must be in bounds of the placed bytes.
    #[must_use]
    pub unsafe fn ptr_at(&self, offset: usize) -> *const u8 {
        self.code.ptr().wrapping_add(offset)
    }

    /// Discard instructions the calling thread may have fetched before this
    /// code was placed on another thread. Placement already made the bytes
    /// visible to every core's instruction cache.
    pub fn prepare_for_execution(&self) {
        dynasmrt::cache_control::prepare_for_execution(self.code.bytes());
    }

    /// Number of finalized instruction bytes, excluding unused mapping capacity.
    #[must_use]
    pub fn len(&self) -> usize {
        self.code.len()
    }

    /// Executable bytes this code keeps from its memory: its length rounded
    /// up to the placement granule, independent of process resident-memory
    /// measurements.
    #[must_use]
    pub fn retained_mapping_bytes(&self) -> usize {
        self.code.reserved()
    }

    /// `true` when the compiled code is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.code.is_empty()
    }

    /// Borrow the exact finalized machine-code bytes.
    ///
    /// Artifact capture clones this slice only when explicitly requested.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        self.code.bytes()
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
    use crate::code_memory::{CodeMemory, GRANULE};

    #[test]
    fn retained_placement_rounds_code_bytes_to_the_granule() {
        // Emission is target independent; this byte is inspected, never run.
        let mut assembler = dynasmrt::x64::Assembler::new_with_capacity(32_768).unwrap();
        let entry = assembler.offset();
        assembler.push(0x90);
        let code =
            CompiledCode::new(&CodeMemory::new(), assembler.finalize().unwrap(), entry).unwrap();
        assert_eq!(code.len(), 1);
        assert_eq!(code.bytes(), &[0x90]);
        assert_eq!(code.retained_mapping_bytes(), GRANULE);
    }
}
