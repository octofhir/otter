//! Admission for byte buffers retained by native `node:zlib` handles.
//!
//! # Contents
//! - [`ZlibRetainedBudget`] combines the isolate ledger with a finite module ledger.
//! - [`RetainedBytes`] keeps one owned buffer inseparable from both charges.
//! - [`PreparedAppend`] admits output before a native operation can produce it.
//!
//! # Invariants
//! - One dictionary retains at most 16 MiB and all live handles together retain
//!   at most 64 MiB of dictionary/carry bytes, even with an unlimited isolate.
//! - Both ledgers grow before allocation and roll back together on rejection.
//! - Mutable carry storage keeps its admitted capacity charged across partial
//!   drains and failed producers; emptying or dropping it releases the charge.
//!
//! # See also
//! - [`super`] owns handle lifetime and codec dispatch.

use std::collections::TryReserveError;

use otter_runtime::{ResourceAccount, ResourceClass, ResourceError, ResourceLease, ResourceLimits};

/// Largest dictionary copied into one native handle.
pub(super) const MAX_DICTIONARY_BYTES: u64 = 16 * 1024 * 1024;
/// Largest output fragment retained between zlib calls.
pub(super) const MAX_CARRY_BYTES: u64 = 64 * 1024;
/// Aggregate dictionary/carry bytes retained by one installed zlib binding.
const MAX_MODULE_RETAINED_BYTES: u64 = 64 * 1024 * 1024;

/// Shared admission state for all native handles in one zlib binding.
#[derive(Clone)]
pub(super) struct ZlibRetainedBudget {
    runtime: ResourceAccount,
    module: ResourceAccount,
}

impl ZlibRetainedBudget {
    pub(super) fn standard(runtime: ResourceAccount) -> Self {
        Self::with_module_limit(runtime, MAX_MODULE_RETAINED_BYTES)
    }

    fn with_module_limit(runtime: ResourceAccount, module_limit: u64) -> Self {
        Self {
            runtime,
            module: ResourceAccount::new(
                ResourceLimits::builder()
                    .limit(ResourceClass::ExternalBytes, module_limit)
                    .build(),
            ),
        }
    }

    fn reserve(&self, amount: u64) -> Result<RetainedCharge, RetainedBytesError> {
        let module = self
            .module
            .reserve_exact(ResourceClass::ExternalBytes, amount)
            .map_err(RetainedBytesError::ModuleBudget)?;
        let runtime = self
            .runtime
            .reserve_exact(ResourceClass::ExternalBytes, amount)
            .map_err(RetainedBytesError::RuntimeBudget)?;
        Ok(RetainedCharge { module, runtime })
    }

    #[cfg(test)]
    fn for_test(runtime: ResourceAccount, module_limit: u64) -> Self {
        Self::with_module_limit(runtime, module_limit)
    }
}

/// Paired exact charge for one retained buffer.
struct RetainedCharge {
    module: ResourceLease,
    runtime: ResourceLease,
}

impl RetainedCharge {
    fn resize(&mut self, amount: u64) -> Result<(), RetainedBytesError> {
        let previous = self.module.amount();
        self.module
            .resize(amount)
            .map_err(RetainedBytesError::ModuleBudget)?;
        if let Err(error) = self.runtime.resize(amount) {
            self.module
                .resize(previous)
                .expect("rolling a zlib module charge back cannot fail");
            return Err(RetainedBytesError::RuntimeBudget(error));
        }
        Ok(())
    }

    fn amount(&self) -> u64 {
        self.runtime.amount()
    }
}

/// One native byte buffer and its isolate/module charges.
pub(super) struct RetainedBytes {
    bytes: Vec<u8>,
    charge: RetainedCharge,
    max_bytes: u64,
}

impl RetainedBytes {
    pub(super) fn new(
        budget: &ZlibRetainedBudget,
        max_bytes: u64,
    ) -> Result<Self, RetainedBytesError> {
        Ok(Self {
            bytes: Vec::new(),
            charge: budget.reserve(0)?,
            max_bytes,
        })
    }

    pub(super) fn as_slice(&self) -> &[u8] {
        &self.bytes
    }

    pub(super) fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    pub(super) fn remaining_capacity(&self) -> usize {
        usize::try_from(self.max_bytes)
            .unwrap_or(usize::MAX)
            .saturating_sub(self.bytes.len())
    }

    /// Replace the buffer after both ledgers admit its exact length.
    pub(super) fn replace_from_slice(&mut self, source: &[u8]) -> Result<(), RetainedBytesError> {
        let amount = retained_amount(source.len())?;
        self.check_limit(amount)?;
        let previous = self.charge.amount();
        self.charge.resize(amount)?;

        let mut replacement = Vec::new();
        if let Err(error) = replacement.try_reserve_exact(source.len()) {
            self.charge
                .resize(previous)
                .expect("rolling zlib allocation admission back cannot fail");
            return Err(RetainedBytesError::Allocation(error));
        }
        replacement.extend_from_slice(source);
        self.bytes = replacement;
        Ok(())
    }

    /// Admit and expose zeroed append space before a native producer runs.
    pub(super) fn prepare_append(
        &mut self,
        max_additional: usize,
    ) -> Result<PreparedAppend<'_>, RetainedBytesError> {
        let previous_len = self.bytes.len();
        let target_len = previous_len
            .checked_add(max_additional)
            .ok_or(RetainedBytesError::LengthOverflow)?;
        let target_amount = retained_amount(target_len)?;
        self.check_limit(target_amount)?;
        let previous_amount = self.charge.amount();
        self.charge.resize(target_amount)?;
        if let Err(error) = self.bytes.try_reserve_exact(max_additional) {
            self.charge
                .resize(previous_amount)
                .expect("rolling zlib producer admission back cannot fail");
            return Err(RetainedBytesError::Allocation(error));
        }
        self.bytes.resize(target_len, 0);
        Ok(PreparedAppend {
            retained: self,
            previous_len,
            committed: false,
        })
    }

    /// Copy and remove the oldest retained bytes.
    pub(super) fn drain_into(&mut self, output: &mut [u8]) -> usize {
        let copied = self.bytes.len().min(output.len());
        output[..copied].copy_from_slice(&self.bytes[..copied]);
        self.bytes.drain(..copied);
        if self.bytes.is_empty() {
            self.bytes = Vec::new();
            self.charge
                .resize(0)
                .expect("releasing an empty zlib retained-byte charge cannot fail");
        }
        copied
    }

    fn check_limit(&self, amount: u64) -> Result<(), RetainedBytesError> {
        if amount > self.max_bytes {
            return Err(RetainedBytesError::TooLarge {
                requested: amount,
                limit: self.max_bytes,
            });
        }
        Ok(())
    }
}

/// Pre-admitted append storage that rolls back unless its producer commits.
pub(super) struct PreparedAppend<'a> {
    retained: &'a mut RetainedBytes,
    previous_len: usize,
    committed: bool,
}

impl PreparedAppend<'_> {
    pub(super) fn output_mut(&mut self) -> &mut [u8] {
        &mut self.retained.bytes[self.previous_len..]
    }

    pub(super) fn commit(mut self, produced: usize) {
        assert!(
            produced <= self.retained.bytes.len() - self.previous_len,
            "native zlib producer exceeded its admitted output"
        );
        let final_len = self.previous_len + produced;
        self.retained.bytes.truncate(final_len);
        if final_len == 0 {
            self.retained.bytes = Vec::new();
            self.retained
                .charge
                .resize(0)
                .expect("releasing an empty prepared zlib append cannot fail");
        }
        self.committed = true;
    }
}

impl Drop for PreparedAppend<'_> {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        self.retained.bytes.truncate(self.previous_len);
        if self.previous_len == 0 {
            self.retained.bytes = Vec::new();
            self.retained
                .charge
                .resize(0)
                .expect("rolling an empty zlib append back cannot fail");
        }
    }
}

fn retained_amount(length: usize) -> Result<u64, RetainedBytesError> {
    u64::try_from(length).map_err(|_| RetainedBytesError::LengthOverflow)
}

/// Failure to retain native zlib bytes.
#[derive(Debug, thiserror::Error)]
pub(super) enum RetainedBytesError {
    #[error("zlib retained buffer of {requested} bytes exceeds its {limit}-byte limit")]
    TooLarge { requested: u64, limit: u64 },
    #[error("zlib module retained-byte limit: {0}")]
    ModuleBudget(ResourceError),
    #[error("runtime external-memory budget: {0}")]
    RuntimeBudget(ResourceError),
    #[error("failed to allocate zlib retained bytes: {0}")]
    Allocation(TryReserveError),
    #[error("zlib retained-byte length overflow")]
    LengthOverflow,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn current(account: &ResourceAccount) -> u64 {
        account
            .snapshot()
            .get(ResourceClass::ExternalBytes)
            .current()
    }

    #[test]
    fn retained_bytes_charge_replace_drain_and_drop() {
        let runtime = ResourceAccount::default();
        let budget = ZlibRetainedBudget::for_test(runtime.clone(), 32);
        {
            let mut bytes = RetainedBytes::new(&budget, 24).expect("empty buffer");
            bytes.replace_from_slice(b"dictionary").expect("replace");
            assert_eq!(current(&runtime), 10);
            let mut append = bytes.prepare_append(5).expect("append admission");
            append.output_mut().copy_from_slice(b"-tail");
            append.commit(5);
            assert_eq!(current(&runtime), 15);

            let mut output = [0; 12];
            assert_eq!(bytes.drain_into(&mut output), 12);
            assert_eq!(&output, b"dictionary-t");
            assert_eq!(current(&runtime), 15);
        }
        assert_eq!(current(&runtime), 0);
    }

    #[test]
    fn rejection_preserves_existing_bytes_and_charge() {
        let runtime = ResourceAccount::default();
        let budget = ZlibRetainedBudget::for_test(runtime.clone(), 8);
        let mut bytes = RetainedBytes::new(&budget, 16).expect("empty buffer");
        bytes.replace_from_slice(b"123456").expect("initial bytes");

        let error = bytes.prepare_append(3).err().expect("module limit");
        assert!(matches!(error, RetainedBytesError::ModuleBudget(_)));
        assert_eq!(bytes.as_slice(), b"123456");
        assert_eq!(current(&runtime), 6);
    }

    #[test]
    fn runtime_rejection_rolls_module_charge_back() {
        let runtime = ResourceAccount::new(
            ResourceLimits::builder()
                .limit(ResourceClass::ExternalBytes, 8)
                .build(),
        );
        let budget = ZlibRetainedBudget::for_test(runtime.clone(), 64);
        let mut bytes = RetainedBytes::new(&budget, 16).expect("empty buffer");
        bytes.replace_from_slice(b"123456").expect("initial bytes");

        let error = bytes.prepare_append(3).err().expect("runtime limit");
        assert!(matches!(error, RetainedBytesError::RuntimeBudget(_)));
        assert_eq!(bytes.as_slice(), b"123456");
        assert_eq!(current(&runtime), 6);
        assert_eq!(current(&budget.module), 6);
    }

    #[test]
    fn prepared_append_charges_before_effect_and_rolls_back_or_commits() {
        let runtime = ResourceAccount::default();
        let budget = ZlibRetainedBudget::for_test(runtime.clone(), 64);
        let mut bytes = RetainedBytes::new(&budget, 32).expect("empty buffer");
        bytes.replace_from_slice(b"old").expect("initial bytes");

        {
            let mut append = bytes.prepare_append(8).expect("prepared output");
            assert_eq!(current(&runtime), 11);
            append.output_mut()[..4].copy_from_slice(b"lost");
        }
        assert_eq!(bytes.as_slice(), b"old");
        assert_eq!(current(&runtime), 11);

        let mut append = bytes.prepare_append(8).expect("prepared output");
        append.output_mut()[..4].copy_from_slice(b"kept");
        append.commit(4);
        assert_eq!(bytes.as_slice(), b"oldkept");
        assert_eq!(current(&runtime), 11);
    }

    #[test]
    fn per_buffer_limit_precedes_any_ledger_mutation() {
        let runtime = ResourceAccount::default();
        let budget = ZlibRetainedBudget::for_test(runtime.clone(), 64);
        let mut bytes = RetainedBytes::new(&budget, 4).expect("empty buffer");

        let error = bytes
            .replace_from_slice(b"12345")
            .expect_err("buffer limit");
        assert!(matches!(error, RetainedBytesError::TooLarge { .. }));
        assert!(bytes.is_empty());
        assert_eq!(current(&runtime), 0);
    }
}
