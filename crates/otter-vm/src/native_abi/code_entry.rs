//! Stable machine-visible function and generation entry cells.
//!
//! A function cell gives generated callers one permanent linkage address. It
//! points at the best current generation cell, while each generation cell owns
//! its exact entry address, frame contract, feedback, and activation lease.
//! Promotion patches the function cell instead of recompiling dependent callers.
//!
//! # Contents
//! - [`FunctionEntryCell`] — fixed-layout stable dispatch cell per function.
//! - [`CodeEntryCell`] — fixed-layout per-generation entry and frame metadata.
//! - [`CodeEntryLease`] — optional Rust-side ownership for users that outlive a
//!   native-activation retirement epoch.
//!
//! # Invariants
//! - A function cell is allocated once and never reused for another function.
//! - `FunctionEntryCell::generation_cell == 0` selects the cold resolver.
//! - Publication switches the function cell before the old generation unlinks.
//! - `entry_addr == 0` means unlinked and permanently rejects new entries.
//! - Generated callers run on the isolate's single mutator and load the current
//!   address before any possible VM transition. Their published outer native
//!   activation defers executable retirement through reentrant invalidation.
//!   Users that can outlive that epoch must instead acquire `active_count`.
//! - A generation's cell address and immutable identity/layout fields never
//!   change, and cells are never repurposed for newer native code.
//! - At a native-activation epoch boundary, executable ownership may retire
//!   only after the cell is unlinked and `active_count == 0`.
//!
//! # See also
//! - [`super::CodeObjectMetadata`] — immutable compiled-object identity.
//! - [`super::CodeRegistryView`] — safepoint metadata selected by this id.

use std::{
    cell::Cell,
    sync::atomic::{AtomicU32, AtomicU64, Ordering},
};

use super::{NativeFrameFlags, NativeFrameKind, VmFrameHeader};

/// Stable per-function dispatch cell consumed by generated call linkage.
#[repr(C, align(8))]
#[derive(Debug)]
pub struct FunctionEntryCell {
    /// Address of the current [`CodeEntryCell`], or zero for the cold resolver.
    pub generation_cell: AtomicU64,
    /// Immutable bytecode function identity.
    pub function_id: u32,
    /// Formal parameter count shared by every generation of this function.
    pub param_count: u16,
    /// Tagged register-window length shared by every generation.
    pub register_count: u16,
}

impl FunctionEntryCell {
    /// Construct one unresolved stable function entry.
    #[must_use]
    pub const fn new(function_id: u32, param_count: u16, register_count: u16) -> Self {
        Self {
            generation_cell: AtomicU64::new(0),
            function_id,
            param_count,
            register_count,
        }
    }

    /// Publish one current generation cell.
    pub fn publish(&self, generation_cell: u64) {
        debug_assert_ne!(generation_cell, 0);
        self.generation_cell
            .store(generation_cell, Ordering::Release);
    }

    /// Select the cold resolver until another generation is published.
    pub fn clear(&self) {
        self.generation_cell.store(0, Ordering::Release);
    }

    /// Current generation-cell address.
    #[must_use]
    pub fn current_generation(&self) -> u64 {
        self.generation_cell.load(Ordering::Acquire)
    }
}

/// The compiled generation owns precise safepoint metadata.
pub const CODE_ENTRY_HAS_SAFEPOINTS: u32 = 1 << 0;
/// The compiled generation belongs to the optimizing tier.
pub const CODE_ENTRY_OPTIMIZING_TIER: u32 = 1 << 2;

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
    /// Static properties of this compiled generation.
    pub flags: u32,
    /// Persistent native-stack bytes reserved by the target after entry, or
    /// zero when this generation cannot accept stack-owned generated calls.
    pub generated_stack_frame_bytes: u32,
    /// Ready-to-copy frame header for stack-owned generated calls.
    ///
    /// Function identity, register shape, tier, and safepoint capability are
    /// immutable for this generation. Packing them once removes per-entry
    /// metadata decoding from generated call linkage.
    pub native_frame_header: VmFrameHeader,
    /// Explicit leases that may outlive a native-activation retirement epoch.
    pub active_count: AtomicU32,
    /// Generated native entries observed for tiering/introspection. Normal
    /// returns are derived as `entries - deopts - throws` during cold
    /// reconciliation, so the hot path owns no redundant return counter.
    ///
    /// The isolate has one mutator and reconciles feedback only after native
    /// activation returns, so these counters deliberately use ordinary
    /// single-mutator cells instead of exclusive atomic loops on every call.
    pub generated_entries: Cell<u64>,
    /// Generated entries that bailed and resumed through cold deoptimization.
    pub generated_deopts: Cell<u64>,
    /// Generated entries that propagated a throw status.
    pub generated_throws: Cell<u64>,
    /// Consecutive generated native bails since the last return/throw.
    pub generated_bail_streak: Cell<u32>,
}

impl CodeEntryCell {
    /// Construct one linked code generation.
    #[must_use]
    pub fn new(
        entry_addr: usize,
        code_object_id: u64,
        function_id: u32,
        param_count: u16,
        register_count: u16,
        flags: u32,
        generated_stack_frame_bytes: u32,
    ) -> Self {
        debug_assert_ne!(entry_addr, 0);
        debug_assert_ne!(code_object_id, 0);
        let kind = if flags & CODE_ENTRY_OPTIMIZING_TIER != 0 {
            NativeFrameKind::Optimizing
        } else {
            NativeFrameKind::Baseline
        };
        let mut frame_flag_bits = NativeFrameFlags::STACK_REGISTERS;
        if flags & CODE_ENTRY_HAS_SAFEPOINTS != 0 {
            frame_flag_bits |= NativeFrameFlags::HAS_SAFEPOINTS;
        }
        let frame_flags = NativeFrameFlags::from_bits(frame_flag_bits);
        Self {
            entry_addr: AtomicU64::new(entry_addr as u64),
            code_object_id,
            function_id,
            param_count,
            register_count,
            flags,
            generated_stack_frame_bytes,
            native_frame_header: VmFrameHeader {
                function_id,
                code_block_id: function_id,
                pc: 0,
                register_count,
                kind,
                flags: frame_flags,
            },
            active_count: AtomicU32::new(0),
            generated_entries: Cell::new(0),
            generated_deopts: Cell::new(0),
            generated_throws: Cell::new(0),
            generated_bail_streak: Cell::new(0),
        }
    }

    /// Try to acquire this exact generation for a new native activation.
    ///
    /// Use this only when ownership can outlive the isolate's published
    /// native-activation retirement epoch. Generated callers execute on the
    /// single mutator and are pinned by that epoch instead.
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

    /// Cumulative generated-call feedback for this exact code generation.
    #[must_use]
    pub fn generated_feedback(&self) -> (u64, u64, u64, u64, u32) {
        let entries = self.generated_entries.get();
        let deopts = self.generated_deopts.get();
        let throws = self.generated_throws.get();
        let returns = entries.saturating_sub(deopts.saturating_add(throws));
        (
            entries,
            returns,
            deopts,
            throws,
            self.generated_bail_streak.get(),
        )
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

const _: [(); 16] = [(); std::mem::size_of::<FunctionEntryCell>()];
const _: [(); 8] = [(); std::mem::align_of::<FunctionEntryCell>()];
const _: [(); 0] = [(); std::mem::offset_of!(FunctionEntryCell, generation_cell)];
const _: [(); 8] = [(); std::mem::offset_of!(FunctionEntryCell, function_id)];

const _: [(); 88] = [(); std::mem::size_of::<CodeEntryCell>()];
const _: [(); 8] = [(); std::mem::align_of::<CodeEntryCell>()];
const _: [(); 0] = [(); std::mem::offset_of!(CodeEntryCell, entry_addr)];
const _: [(); 8] = [(); std::mem::offset_of!(CodeEntryCell, code_object_id)];
const _: [(); 24] = [(); std::mem::offset_of!(CodeEntryCell, flags)];
const _: [(); 28] = [(); std::mem::offset_of!(CodeEntryCell, generated_stack_frame_bytes)];
const _: [(); 32] = [(); std::mem::offset_of!(CodeEntryCell, native_frame_header)];
const _: [(); 48] = [(); std::mem::offset_of!(CodeEntryCell, active_count)];
const _: [(); 56] = [(); std::mem::offset_of!(CodeEntryCell, generated_entries)];
const _: [(); 64] = [(); std::mem::offset_of!(CodeEntryCell, generated_deopts)];
const _: [(); 72] = [(); std::mem::offset_of!(CodeEntryCell, generated_throws)];
const _: [(); 80] = [(); std::mem::offset_of!(CodeEntryCell, generated_bail_streak)];

#[cfg(test)]
mod tests {
    use super::*;

    fn cell() -> CodeEntryCell {
        CodeEntryCell::new(0x1234, 7, 9, 2, 12, CODE_ENTRY_HAS_SAFEPOINTS, 64)
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
