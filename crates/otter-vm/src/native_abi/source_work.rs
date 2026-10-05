//! Address-stable source-opcode work shared by the VM and Template code.
//!
//! # Contents
//! - [`SourceWork`] stores one function's monotone entered-opcode attempts.
//! - The compiled ABI exposes its aligned word while retaining the same Arc.
//!
//! # Invariants
//! - One isolate mutator writes; compiler and diagnostic readers may read.
//! - Rust relaxed atomic loads/stores and native aligned LDR/STR or MOV accesses
//!   implement the same relaxed word operations. No locked RMW is needed.
//! - Addition saturates before the single final store, never publishing wrap.
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
    _lease: otter_resource::ResourceLease,
}

impl SourceWork {
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
            _lease: lease,
        })
    }

    pub(crate) fn retained_bytes(&self) -> u64 {
        self._lease.amount()
    }

    /// Exact attempts visible at the latest dispatched/compiled boundary.
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

    /// Aligned atomic-word address consumed only by the compiled-code ABI.
    /// The emitted code must retain this allocation and obey the single-writer
    /// and saturating-store contract above.
    #[must_use]
    pub fn native_address(&self) -> usize {
        std::ptr::from_ref(&self.total) as usize
    }
}

const _: [(); 0] = [(); std::mem::offset_of!(SourceWork, total)];
const _: [(); 8] = [(); std::mem::align_of::<SourceWork>()];

#[cfg(test)]
mod tests {
    use super::*;

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
