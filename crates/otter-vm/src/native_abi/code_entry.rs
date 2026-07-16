//! Stable machine-visible code-entry cells.
//!
//! A cell separates the address selected by a new compiled call from ownership
//! of the executable mapping. Invalidation unlinks the address first; active
//! entrants keep the generation alive until their lease count reaches zero.
//! The cell itself is never reused for another code generation.
//!
//! # Contents
//! - [`CodeEntryCell`] — fixed-layout per-generation entry and frame metadata.
//! - [`CodeEntryLease`] — Rust-side implementation of the acquire/recheck/
//!   release protocol native call trampolines must mirror.
//!
//! # Invariants
//! - `entry_addr == 0` means unlinked and permanently rejects new entries.
//! - An entrant loads the address, increments `active_count`, then rechecks the
//!   address before branching. Invalidation stores zero before code retirement.
//! - A generation's cell address and immutable identity/layout fields never
//!   change, and cells are never repurposed for newer native code.
//! - Executable ownership may retire only after the cell is unlinked and
//!   `active_count == 0` with acquire ordering.
//!
//! # See also
//! - [`super::CodeObjectMetadata`] — immutable compiled-object identity.
//! - [`super::NativeFrame`] — publishes the active code-object id.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// The compiled generation owns precise safepoint metadata.
pub const CODE_ENTRY_HAS_SAFEPOINTS: u32 = 1 << 0;

/// Stable per-generation entry cell consumed by native call linkage.
#[repr(C, align(8))]
#[derive(Debug)]
pub struct CodeEntryCell {
    /// Current native entry address; zero after unlinking.
    pub entry_addr: AtomicU64,
    /// Immutable isolate-local code-object identity.
    pub code_object_id: u64,
    /// Immutable bytecode function identity.
    pub function_id: u32,
    /// Formal parameter count used by frame construction.
    pub param_count: u16,
    /// Full initialized register-window length.
    pub register_count: u16,
    /// Stable machine-readable feedback-vector base, or zero while absent.
    pub feedback_base: u64,
    /// Dense feedback-vector identity, or zero while absent.
    pub feedback_id: u32,
    /// [`CODE_ENTRY_HAS_SAFEPOINTS`] and future versioned entry flags.
    pub flags: u32,
    /// Native activations that acquired this generation and have not returned.
    pub active_count: AtomicU32,
    /// Reserved; zero in version 1.
    pub reserved0: u32,
}

impl CodeEntryCell {
    /// Construct one linked code generation.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        entry_addr: usize,
        code_object_id: u64,
        function_id: u32,
        param_count: u16,
        register_count: u16,
        feedback_base: u64,
        feedback_id: u32,
        flags: u32,
    ) -> Self {
        debug_assert_ne!(entry_addr, 0);
        debug_assert_ne!(code_object_id, 0);
        Self {
            entry_addr: AtomicU64::new(entry_addr as u64),
            code_object_id,
            function_id,
            param_count,
            register_count,
            feedback_base,
            feedback_id,
            flags,
            active_count: AtomicU32::new(0),
            reserved0: 0,
        }
    }

    /// Try to acquire this exact generation for a new native activation.
    ///
    /// Native code must implement the same load/increment/recheck order before
    /// branching through [`Self::entry_addr`]. A concurrent unlink between the
    /// first load and the recheck is observed and the provisional count is
    /// released without entering code.
    #[must_use]
    pub fn try_acquire(&self) -> Option<CodeEntryLease<'_>> {
        let entry_addr = self.entry_addr.load(Ordering::Acquire);
        if entry_addr == 0 {
            return None;
        }
        if self
            .active_count
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                count.checked_add(1)
            })
            .is_err()
        {
            return None;
        }
        let confirmed = self.entry_addr.load(Ordering::Acquire);
        if confirmed == 0 {
            self.active_count.fetch_sub(1, Ordering::AcqRel);
            return None;
        }
        debug_assert_eq!(confirmed, entry_addr, "entry cells are never relinked");
        Some(CodeEntryLease {
            cell: self,
            entry_addr: confirmed,
        })
    }

    /// Permanently reject new entries and return the previously linked address.
    pub fn unlink(&self) -> Option<usize> {
        let previous = self.entry_addr.swap(0, Ordering::AcqRel);
        (previous != 0).then_some(previous as usize)
    }

    /// Whether executable ownership can be retired safely.
    #[must_use]
    pub fn can_retire(&self) -> bool {
        self.entry_addr.load(Ordering::Acquire) == 0
            && self.active_count.load(Ordering::Acquire) == 0
    }

    /// Current number of acquired native activations.
    #[must_use]
    pub fn active_count(&self) -> u32 {
        self.active_count.load(Ordering::Acquire)
    }
}

/// One acquired native entry generation.
#[derive(Debug)]
pub struct CodeEntryLease<'a> {
    cell: &'a CodeEntryCell,
    entry_addr: u64,
}

impl CodeEntryLease<'_> {
    /// Stable native entry address validated by the acquire/recheck protocol.
    #[must_use]
    pub fn entry_addr(&self) -> usize {
        self.entry_addr as usize
    }

    /// Immutable code-object identity to publish in the callee frame.
    #[must_use]
    pub fn code_object_id(&self) -> u64 {
        self.cell.code_object_id
    }
}

impl Drop for CodeEntryLease<'_> {
    fn drop(&mut self) {
        let previous = self.cell.active_count.fetch_sub(1, Ordering::AcqRel);
        debug_assert_ne!(previous, 0, "entry lease count cannot underflow");
    }
}

const _: [(); 48] = [(); std::mem::size_of::<CodeEntryCell>()];
const _: [(); 8] = [(); std::mem::align_of::<CodeEntryCell>()];
const _: [(); 0] = [(); std::mem::offset_of!(CodeEntryCell, entry_addr)];
const _: [(); 8] = [(); std::mem::offset_of!(CodeEntryCell, code_object_id)];
const _: [(); 24] = [(); std::mem::offset_of!(CodeEntryCell, feedback_base)];
const _: [(); 40] = [(); std::mem::offset_of!(CodeEntryCell, active_count)];

#[cfg(test)]
mod tests {
    use super::*;

    fn cell() -> CodeEntryCell {
        CodeEntryCell::new(0x1234, 7, 9, 2, 12, 0, 0, CODE_ENTRY_HAS_SAFEPOINTS)
    }

    #[test]
    fn unlink_rejects_new_entries_but_active_lease_delays_retirement() {
        let cell = cell();
        let lease = cell.try_acquire().expect("linked generation acquires");
        assert_eq!(lease.entry_addr(), 0x1234);
        assert_eq!(lease.code_object_id(), 7);
        assert_eq!(cell.active_count(), 1);

        assert_eq!(cell.unlink(), Some(0x1234));
        assert!(cell.try_acquire().is_none());
        assert!(!cell.can_retire());

        drop(lease);
        assert_eq!(cell.active_count(), 0);
        assert!(cell.can_retire());
        assert_eq!(cell.unlink(), None, "unlink is idempotent");
    }

    #[test]
    fn lease_release_does_not_unlink_a_live_generation() {
        let cell = cell();
        drop(cell.try_acquire().unwrap());
        assert_eq!(cell.active_count(), 0);
        assert!(!cell.can_retire());
        assert_eq!(cell.entry_addr.load(Ordering::Acquire), 0x1234);
    }

    #[test]
    fn saturated_activation_count_rejects_entry_without_wrapping() {
        let cell = cell();
        cell.active_count.store(u32::MAX, Ordering::Release);
        assert!(cell.try_acquire().is_none());
        assert_eq!(cell.active_count(), u32::MAX);
    }
}
