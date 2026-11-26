//! Address-stable compilation budgets shared by the VM and generated code.
//!
//! # Contents
//! - [`SourceWork`] stores one function's monotone tiering work
//!   and its entries, the denominator of its call sites' frequencies.
//! - Compact Graph generations debit one source-owned budget at eligible call
//!   bytecodes. Reaching zero signals the isolate; active frames keep running.
//! - The compiled ABI exposes its aligned words while retaining the same Arc.
//!
//! # Invariants
//! - One isolate mutator writes; compiler and diagnostic readers may read.
//! - Rust relaxed atomic loads/stores and native aligned LDR/STR or MOV accesses
//!   implement the same relaxed word operations. No locked RMW is needed.
//! - Source work saturates. The inline budget counts down to zero. Only the
//!   mutator claims or re-arms a wakeup; successive compact generations share
//!   the count. Neither word contains a GC reference.
//! - Code objects retain the exact allocation for every baked source address.
//!   Its one SourceModuleBytes lease survives until the last policy/native Arc
//!   drops, independently of the originating CodeBlock or chunk.
//!
//! # See also
//! - [`crate::executable::CodeBlock`] owns the source allocation.
//! - [`crate::tier_policy`] owns its interpretation and compile admission.

use std::sync::atomic::{AtomicU64, Ordering};

/// Private engine ABI scalar; this is not an extension or embedding counter.
#[repr(C)]
#[derive(Debug)]
pub struct SourceWork {
    total: AtomicU64,
    /// Entries of the function by the interpreter and by baseline code.
    entries: AtomicU64,
    /// Visits to deferred inline sites until the source earns body expansion.
    inline_calls_remaining: AtomicU64,
    _lease: otter_resource::ResourceLease,
}

/// Byte offset of [`SourceWork`]'s entry count from its native address.
pub const SOURCE_WORK_ENTRIES_OFFSET: usize = std::mem::offset_of!(SourceWork, entries);

/// Offset of the single-mutator call budget from the native source address.
pub const INLINE_CALL_BUDGET_OFFSET: usize =
    std::mem::offset_of!(SourceWork, inline_calls_remaining);

/// Actual eligible site visits that purchase a full entry-Graph inline tree.
const INLINE_CALL_BUDGET: u64 = 16_384;
/// A claimed wakeup has the sign bit set so both native encoders skip it.
const CLAIMED: u64 = u64::MAX;

impl SourceWork {
    /// Whether any compact generation has observed enough inlineable calls.
    pub(crate) fn inline_calls_funded(&self) -> bool {
        matches!(
            self.inline_calls_remaining.load(Ordering::Relaxed),
            0 | CLAIMED
        )
    }

    /// Claim one wakeup on the isolate mutator. Other generations share this word.
    pub(crate) fn claim_inline_calls(&self) -> bool {
        if self.inline_calls_remaining.load(Ordering::Relaxed) != 0 {
            return false;
        }
        self.inline_calls_remaining
            .store(CLAIMED, Ordering::Relaxed);
        true
    }

    /// A transient refusal lets the next executed candidate signal again.
    pub(crate) fn retry_inline_calls(&self) {
        if self.inline_calls_funded() {
            self.inline_calls_remaining.store(1, Ordering::Relaxed);
        }
    }

    /// Back off after code-memory pressure. A still-hot source can request
    /// expansion again when enough headroom has become available.
    pub(crate) fn defer_inline_calls_for_pressure(&self) {
        self.inline_calls_remaining
            .store(INLINE_CALL_BUDGET, Ordering::Relaxed);
    }

    /// Admit the one physical source-work cell before retaining its address.
    pub(crate) fn new(
        account: &otter_resource::ResourceAccount,
    ) -> Result<Self, otter_resource::ResourceError> {
        let lease = account.reserve_exact(
            otter_resource::ResourceClass::SourceModuleBytes,
            std::mem::size_of::<Self>() as u64,
        )?;
        Ok(Self {
            total: AtomicU64::new(0),
            entries: AtomicU64::new(0),
            inline_calls_remaining: AtomicU64::new(INLINE_CALL_BUDGET),
            _lease: lease,
        })
    }

    pub(crate) fn retained_bytes(&self) -> u64 {
        self._lease.amount()
    }

    /// Interpreted opcode attempts plus Template entry and loop work charges.
    #[must_use]
    pub fn total(&self) -> u64 {
        self.total.load(Ordering::Relaxed)
    }

    /// Charge entered source opcodes on the exclusive isolate mutator.
    #[inline]
    pub(crate) fn charge(&self, attempts: u64) {
        self.total
            .store(self.total().saturating_add(attempts), Ordering::Relaxed);
    }

    /// Entries of the function so far.
    #[must_use]
    pub fn entries(&self) -> u64 {
        self.entries.load(Ordering::Relaxed)
    }

    /// Count one entry on the exclusive isolate mutator.
    #[inline]
    pub(crate) fn note_entry(&self) {
        self.entries
            .store(self.entries().saturating_add(1), Ordering::Relaxed);
    }

    /// Aligned atomic-word address consumed only by the compiled-code ABI.
    /// The emitted code must retain this allocation and obey the single-writer
    /// and saturating-store contract above.
    #[must_use]
    pub fn native_address(&self) -> usize {
        std::ptr::from_ref(&self.total) as usize
    }
}

const _: [(); 0] = [(); std::mem::offset_of!(SourceWork, total)];
const _: [(); 8] = [(); SOURCE_WORK_ENTRIES_OFFSET];
const _: [(); 16] = [(); INLINE_CALL_BUDGET_OFFSET];
const _: [(); 8] = [(); std::mem::align_of::<SourceWork>()];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_budget_is_shared_and_claimed_once() {
        let work = std::sync::Arc::new(SourceWork::new(&Default::default()).unwrap());
        let previous = work.clone();
        assert!(!work.inline_calls_funded());
        assert!(!work.claim_inline_calls());
        // Last debit in the old generation, then a poll in the new one.
        previous.inline_calls_remaining.store(0, Ordering::Relaxed);
        assert!(work.inline_calls_funded());
        assert!(work.claim_inline_calls());
        assert!(!previous.claim_inline_calls());
        assert!(previous.inline_calls_funded());
        work.retry_inline_calls();
        assert_eq!(previous.inline_calls_remaining.load(Ordering::Relaxed), 1);
        previous.inline_calls_remaining.store(0, Ordering::Relaxed);
        assert!(work.claim_inline_calls());
        work.defer_inline_calls_for_pressure();
        assert!(!previous.inline_calls_funded());
        work.retry_inline_calls();
        assert_eq!(
            previous.inline_calls_remaining.load(Ordering::Relaxed),
            INLINE_CALL_BUDGET
        );
    }

    #[test]
    fn source_work_never_wraps() {
        let work = SourceWork::new(&otter_resource::ResourceAccount::default()).unwrap();
        work.charge(u64::MAX - 2);
        work.charge(1);
        assert_eq!(work.total(), u64::MAX - 1);
        work.charge(9);
        assert_eq!(work.total(), u64::MAX);
        work.charge(1);
        assert_eq!(work.total(), u64::MAX);
    }
}
