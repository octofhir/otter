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
//! - Every linked function selects a live interpreter or compiled destination.
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
//! - Template call entries count native entries for tiering; Graph entries
//!   omit that hot-path accounting and record only their cold deopts. These
//!   cells therefore cannot prove ordinary or OSR entry into Graph code.
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
    /// Address of the current interpreter or compiled [`CodeEntryCell`].
    pub generation_cell: AtomicU64,
    /// Immutable bytecode function identity.
    pub function_id: u32,
    /// Formal parameter count shared by every generation of this function.
    pub param_count: u16,
    /// Tagged register-window length shared by every generation.
    pub register_count: u16,
    /// Immutable OrdinaryCallBindThis and construction semantics; see the
    /// `FUNCTION_CALL_*` bits.
    pub call_flags: u32,
    /// Realm that linked this function. The call trampoline binds a sloppy
    /// `undefined`/`null` receiver to the active global only while the active
    /// realm matches; any other realm's global is bound by activation
    /// preparation.
    pub realm_id: u32,
    /// Permanent destination used before compilation and after invalidation.
    interpreter: CodeEntryCell,
}

/// The receiver is passed unconverted (strict, arrow, or `this`-free body).
pub const FUNCTION_CALL_NO_RECEIVER_CONVERSION: u32 = 1 << 0;
/// The body is a derived class constructor.
pub const FUNCTION_CALL_DERIVED_CONSTRUCTOR: u32 = 1 << 1;
/// The body has a `[[Construct]]` internal method.
pub const FUNCTION_CALL_CONSTRUCTIBLE: u32 = 1 << 2;
/// Async, generator or async-generator body: the interpreter entry performs
/// its promise or generator prologue, so dispatch always selects that entry.
pub const FUNCTION_CALL_SUSPENDABLE: u32 = 1 << 3;
/// Arrow function: `this` and `new.target` come from the closure, never from
/// the call.
pub const FUNCTION_CALL_LEXICAL_THIS: u32 = 1 << 4;

/// Byte offset of the immutable call flags in [`FunctionEntryCell`].
pub const FUNCTION_ENTRY_CALL_FLAGS_OFFSET: usize =
    std::mem::offset_of!(FunctionEntryCell, call_flags);
/// Byte offset of the formal parameter count in [`FunctionEntryCell`].
pub const FUNCTION_ENTRY_PARAM_COUNT_OFFSET: usize =
    std::mem::offset_of!(FunctionEntryCell, param_count);
/// Byte offset of the linking realm in [`FunctionEntryCell`].
pub const FUNCTION_ENTRY_REALM_OFFSET: usize = std::mem::offset_of!(FunctionEntryCell, realm_id);
/// Byte offset of the permanent interpreter destination in [`FunctionEntryCell`].
pub const FUNCTION_ENTRY_INTERPRETER_OFFSET: usize =
    std::mem::offset_of!(FunctionEntryCell, interpreter);

impl FunctionEntryCell {
    /// Allocate the permanent cell and publish its interpreter destination.
    #[must_use]
    pub fn new(
        function_id: u32,
        param_count: u16,
        register_count: u16,
        call_flags: u32,
        realm_id: u32,
    ) -> Box<Self> {
        let cell = Box::new(Self {
            generation_cell: AtomicU64::new(0),
            function_id,
            param_count,
            register_count,
            call_flags,
            realm_id,
            interpreter: CodeEntryCell::interpreter(function_id, register_count),
        });
        cell.restore_interpreter();
        cell
    }

    /// Publish one current generation cell.
    pub fn publish(&self, generation_cell: u64) {
        debug_assert_ne!(generation_cell, 0);
        self.generation_cell
            .store(generation_cell, Ordering::Release);
    }

    /// Publish the owned interpreter destination after compiled invalidation.
    pub fn restore_interpreter(&self) {
        self.publish(std::ptr::from_ref(&self.interpreter) as u64);
    }

    /// Registry-owned interpreter destination, including its generated feedback.
    pub(crate) fn interpreter_destination(&self) -> &CodeEntryCell {
        &self.interpreter
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
pub const CODE_ENTRY_OPTIMIZING_TIER: u32 = 1 << 1;

/// Stable per-generation entry cell consumed by native call linkage.
#[repr(C, align(8))]
#[derive(Debug)]
pub struct CodeEntryCell {
    /// Entry of this generation in the JavaScript call ABI; zero after
    /// unlinking. A compiled generation's entry builds, publishes and
    /// retires its own frame. The interpreter destination's entry classifies
    /// the call through the trampoline, which builds the interpreter frame.
    pub entry_addr: AtomicU64,
    /// Immutable isolate-local code-object identity.
    pub code_object_id: u64,
    /// Static properties of this compiled generation.
    pub flags: u32,
    /// Explicit leases that may outlive a native-activation retirement epoch.
    pub active_count: AtomicU32,
    /// Ready-to-copy frame header for stack-owned generated calls.
    ///
    /// Function identity, register shape, tier, and safepoint capability are
    /// immutable for this generation. Packing them once removes per-entry
    /// metadata decoding from generated call linkage.
    pub native_frame_header: VmFrameHeader,
    /// This generation's id as the frame record's 32-bit `code_object_id`.
    /// It completes the header's second word, so linkage copies register
    /// shape, tier, flags and generation with one load/store pair.
    pub native_frame_code_object_id: u32,
    /// Absolute canonical source-work target at which a Template entry asks
    /// cold policy for promotion. The policy owns the feedback-epoch origin;
    /// deferral preserves already observed work.
    pub generated_tiering_work_target: Cell<u64>,
    /// Whether this baseline generation may request optimizing compilation.
    /// Cold policy clears it after a cached compile outcome; a replacement
    /// generation starts fresh. Optimizing generations never request promotion.
    pub generated_tiering_enabled: Cell<u32>,
    /// Template generated native entries observed for tiering/introspection.
    /// The Graph backend omits entry accounting. Template normal completions
    /// are derived as `entries - deopts` during cold reconciliation, so the hot
    /// path owns no redundant return counter.
    ///
    /// The isolate has one mutator and reconciles feedback only after native
    /// activation returns, so these counters deliberately use ordinary
    /// single-mutator cells instead of exclusive atomic loops on every call.
    pub generated_entries: Cell<u64>,
    /// Generated entries that bailed and resumed through cold deoptimization.
    pub generated_deopts: Cell<u64>,
}

impl CodeEntryCell {
    fn interpreter(function_id: u32, register_count: u16) -> Self {
        Self {
            entry_addr: AtomicU64::new(
                super::call_trampoline::call_generic_entry as *const () as u64,
            ),
            code_object_id: 0,
            flags: 0,
            active_count: AtomicU32::new(0),
            native_frame_header: VmFrameHeader::interpreter(function_id, register_count),
            native_frame_code_object_id: 0,
            generated_tiering_work_target: Cell::new(u64::MAX),
            generated_tiering_enabled: Cell::new(0),
            generated_entries: Cell::new(0),
            generated_deopts: Cell::new(0),
        }
    }

    /// Construct one linked code generation.
    #[must_use]
    pub fn new(
        entry_addr: usize,
        code_object_id: u64,
        function_id: u32,
        register_count: u16,
        flags: u32,
        generated_tiering_work_target: Option<u64>,
    ) -> Self {
        debug_assert_ne!(entry_addr, 0);
        debug_assert_ne!(code_object_id, 0);
        let kind = if flags & CODE_ENTRY_OPTIMIZING_TIER != 0 {
            NativeFrameKind::Optimizing
        } else {
            NativeFrameKind::Baseline
        };
        let mut frame_flag_bits = 0;
        if flags & CODE_ENTRY_HAS_SAFEPOINTS != 0 {
            frame_flag_bits |= NativeFrameFlags::HAS_SAFEPOINTS;
        }
        let frame_flags = NativeFrameFlags::from_bits(frame_flag_bits);
        Self {
            entry_addr: AtomicU64::new(entry_addr as u64),
            code_object_id,
            flags,
            active_count: AtomicU32::new(0),
            native_frame_header: VmFrameHeader {
                function_id,
                pc: 0,
                register_count,
                kind,
                flags: frame_flags,
            },
            native_frame_code_object_id: u32::try_from(code_object_id)
                .expect("code object ids fit the frame record's generation field"),
            generated_tiering_work_target: Cell::new(
                generated_tiering_work_target.unwrap_or(u64::MAX),
            ),
            generated_entries: Cell::new(0),
            generated_deopts: Cell::new(0),
            generated_tiering_enabled: Cell::new(u32::from(
                flags & CODE_ENTRY_OPTIMIZING_TIER == 0 && generated_tiering_work_target.is_some(),
            )),
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
    /// Graph generations report cold deopts but zero entries and returns.
    #[must_use]
    pub fn generated_feedback(&self) -> (u64, u64, u64) {
        let entries = self.generated_entries.get();
        let deopts = self.generated_deopts.get();
        (entries, entries.saturating_sub(deopts), deopts)
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

const _: [(); 96] = [(); std::mem::size_of::<FunctionEntryCell>()];
const _: [(); 8] = [(); std::mem::align_of::<FunctionEntryCell>()];
const _: [(); 0] = [(); std::mem::offset_of!(FunctionEntryCell, generation_cell)];
const _: [(); 8] = [(); std::mem::offset_of!(FunctionEntryCell, function_id)];
const _: [(); 12] = [(); FUNCTION_ENTRY_PARAM_COUNT_OFFSET];
const _: [(); 14] = [(); std::mem::offset_of!(FunctionEntryCell, register_count)];
const _: [(); 16] = [(); FUNCTION_ENTRY_CALL_FLAGS_OFFSET];
const _: [(); 20] = [(); FUNCTION_ENTRY_REALM_OFFSET];
const _: [(); 24] = [(); FUNCTION_ENTRY_INTERPRETER_OFFSET];

const _: [(); 72] = [(); std::mem::size_of::<CodeEntryCell>()];
const _: [(); 40] = [(); std::mem::offset_of!(CodeEntryCell, generated_tiering_work_target)];
const _: [(); 48] = [(); std::mem::offset_of!(CodeEntryCell, generated_tiering_enabled)];
const _: [(); 8] = [(); std::mem::align_of::<CodeEntryCell>()];
const _: [(); 0] = [(); std::mem::offset_of!(CodeEntryCell, entry_addr)];
const _: [(); 8] = [(); std::mem::offset_of!(CodeEntryCell, code_object_id)];
const _: [(); 16] = [(); std::mem::offset_of!(CodeEntryCell, flags)];
const _: [(); 20] = [(); std::mem::offset_of!(CodeEntryCell, active_count)];
const _: [(); 24] = [(); std::mem::offset_of!(CodeEntryCell, native_frame_header)];
const _: [(); 36] = [(); std::mem::offset_of!(CodeEntryCell, native_frame_code_object_id)];
const _: [(); 56] = [(); std::mem::offset_of!(CodeEntryCell, generated_entries)];
const _: [(); 64] = [(); std::mem::offset_of!(CodeEntryCell, generated_deopts)];

#[cfg(test)]
mod tests {
    use super::*;

    fn cell() -> CodeEntryCell {
        CodeEntryCell::new(0x1234, 7, 9, 12, CODE_ENTRY_HAS_SAFEPOINTS, Some(321))
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

    #[test]
    fn generation_cell_keeps_one_function_identity_in_a_compact_layout() {
        let cell = cell();
        assert_eq!(std::mem::size_of::<CodeEntryCell>(), 72);
        assert_eq!(cell.native_frame_header.function_id, 9);
        assert_eq!(std::mem::offset_of!(CodeEntryCell, native_frame_header), 24);
    }
}

/// Bit zero of the private JS generation input identifies a trampoline-staged
/// request. The code cell itself remains the sole aligned generation owner;
/// entries clear this bit before any generation-cell dereference and consume
/// the request's genuine caller return address before publishing their frame.
pub const CODE_ENTRY_STAGED_REQUEST_MASK: u64 = 1;
const _: () = assert!(std::mem::align_of::<CodeEntryCell>() > 1);
