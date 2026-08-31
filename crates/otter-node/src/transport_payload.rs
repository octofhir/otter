//! Admission for byte payloads retained by native network queues.
//!
//! # Contents
//! - [`TransportPayloadBudget`] combines the isolate ledger with a finite binding ledger.
//! - [`QueuedPayload`] keeps owned bytes inseparable from their queue charges.
//! - [`TransportPayloadLease`] preserves accounting while bytes move into VM ownership.
//!
//! # Invariants
//! - One installed transport binding retains at most 4,096 payloads and 64 MiB.
//! - Both ledgers admit a payload before its `Vec` allocates and roll back together on failure.
//! - Queued message count and byte charges live exactly as long as the native-owned payload.
//!
//! # See also
//! - [`crate::net`] owns stream read/write queues.
//! - [`crate::dgram`] owns datagram receive delivery.

use std::collections::TryReserveError;

use otter_runtime::{
    ResourceAccount, ResourceClass, ResourceError, ResourceLeaseSet, ResourceLimits,
};

const MAX_BINDING_QUEUED_MESSAGES: u64 = 4_096;
const MAX_BINDING_QUEUED_MESSAGE_BYTES: u64 = 64 * 1024 * 1024;

/// Shared admission state for all native payloads in one transport binding.
#[derive(Clone)]
pub(crate) struct TransportPayloadBudget {
    runtime: ResourceAccount,
    binding: ResourceAccount,
}

impl TransportPayloadBudget {
    pub(crate) fn standard(runtime: ResourceAccount) -> Self {
        Self::with_limits(
            runtime,
            MAX_BINDING_QUEUED_MESSAGES,
            MAX_BINDING_QUEUED_MESSAGE_BYTES,
        )
    }

    fn with_limits(runtime: ResourceAccount, messages: u64, bytes: u64) -> Self {
        Self {
            runtime,
            binding: ResourceAccount::new(
                ResourceLimits::builder()
                    .limit(ResourceClass::QueuedMessages, messages)
                    .limit(ResourceClass::QueuedMessageBytes, bytes)
                    .build(),
            ),
        }
    }

    /// Admit and copy one payload before it crosses a call or task boundary.
    pub(crate) fn copy_from(&self, source: &[u8]) -> Result<QueuedPayload, TransportPayloadError> {
        let bytes = u64::try_from(source.len()).map_err(|_| TransportPayloadError::TooLarge)?;
        let requests = [
            (ResourceClass::QueuedMessages, 1),
            (ResourceClass::QueuedMessageBytes, bytes),
        ];
        let binding = self
            .binding
            .reserve_exact_many(&requests)
            .map_err(TransportPayloadError::BindingBudget)?;
        let runtime = self
            .runtime
            .reserve_exact_many(&requests)
            .map_err(TransportPayloadError::RuntimeBudget)?;

        let mut owned = Vec::new();
        owned
            .try_reserve_exact(source.len())
            .map_err(TransportPayloadError::Allocation)?;
        owned.extend_from_slice(source);
        Ok(QueuedPayload {
            bytes: owned,
            lease: TransportPayloadLease {
                _binding: binding,
                _runtime: runtime,
            },
        })
    }

    #[cfg(test)]
    pub(crate) fn for_test(runtime: ResourceAccount, messages: u64, bytes: u64) -> Self {
        Self::with_limits(runtime, messages, bytes)
    }

    #[cfg(test)]
    fn binding_snapshot(&self) -> otter_runtime::ResourceSnapshot {
        self.binding.snapshot()
    }
}

/// Owned bytes waiting in a native transport queue.
pub(crate) struct QueuedPayload {
    bytes: Vec<u8>,
    lease: TransportPayloadLease,
}

impl QueuedPayload {
    pub(crate) fn as_slice(&self) -> &[u8] {
        &self.bytes
    }

    /// Split ownership so a VM backing-store transfer can overlap the queue
    /// lease until the VM has admitted and adopted the bytes.
    pub(crate) fn into_parts(self) -> (Vec<u8>, TransportPayloadLease) {
        (self.bytes, self.lease)
    }
}

/// Paired queue charges retained while payload ownership is handed off.
pub(crate) struct TransportPayloadLease {
    _binding: ResourceLeaseSet,
    _runtime: ResourceLeaseSet,
}

/// Failure to retain a native transport payload.
#[derive(Debug, thiserror::Error)]
pub(crate) enum TransportPayloadError {
    #[error("transport payload length does not fit the resource ledger")]
    TooLarge,
    #[error("transport binding queue limit: {0}")]
    BindingBudget(ResourceError),
    #[error("runtime transport queue budget: {0}")]
    RuntimeBudget(ResourceError),
    #[error("failed to allocate transport payload: {0}")]
    Allocation(TryReserveError),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn current(account: &ResourceAccount, class: ResourceClass) -> u64 {
        account.snapshot().get(class).current()
    }

    #[test]
    fn payload_charges_both_ledgers_until_drop() {
        let runtime = ResourceAccount::default();
        let budget = TransportPayloadBudget::for_test(runtime.clone(), 2, 8);

        let payload = budget.copy_from(b"abc").expect("payload admitted");
        assert_eq!(payload.as_slice(), b"abc");
        assert_eq!(current(&runtime, ResourceClass::QueuedMessages), 1);
        assert_eq!(current(&runtime, ResourceClass::QueuedMessageBytes), 3);
        assert_eq!(
            budget
                .binding_snapshot()
                .get(ResourceClass::QueuedMessageBytes)
                .current(),
            3
        );

        drop(payload);
        assert_eq!(current(&runtime, ResourceClass::QueuedMessages), 0);
        assert_eq!(current(&runtime, ResourceClass::QueuedMessageBytes), 0);
        assert_eq!(
            budget
                .binding_snapshot()
                .get(ResourceClass::QueuedMessageBytes)
                .current(),
            0
        );
    }

    #[test]
    fn runtime_rejection_rolls_binding_charge_back() {
        let runtime = ResourceAccount::new(
            ResourceLimits::builder()
                .limit(ResourceClass::QueuedMessageBytes, 2)
                .build(),
        );
        let budget = TransportPayloadBudget::for_test(runtime.clone(), 2, 8);

        assert!(matches!(
            budget.copy_from(b"abc"),
            Err(TransportPayloadError::RuntimeBudget(_))
        ));
        assert_eq!(current(&runtime, ResourceClass::QueuedMessages), 0);
        assert_eq!(current(&runtime, ResourceClass::QueuedMessageBytes), 0);
        assert_eq!(
            budget
                .binding_snapshot()
                .get(ResourceClass::QueuedMessages)
                .current(),
            0
        );
        assert_eq!(
            budget
                .binding_snapshot()
                .get(ResourceClass::QueuedMessageBytes)
                .current(),
            0
        );
    }

    #[test]
    fn finite_binding_rejects_before_runtime_charge() {
        let runtime = ResourceAccount::default();
        let budget = TransportPayloadBudget::for_test(runtime.clone(), 1, 2);

        assert!(matches!(
            budget.copy_from(b"abc"),
            Err(TransportPayloadError::BindingBudget(_))
        ));
        assert_eq!(current(&runtime, ResourceClass::QueuedMessages), 0);
        assert_eq!(current(&runtime, ResourceClass::QueuedMessageBytes), 0);
    }

    #[test]
    fn message_limit_bounds_empty_payloads_and_recovers() {
        let runtime = ResourceAccount::default();
        let budget = TransportPayloadBudget::for_test(runtime.clone(), 1, 8);

        let first = budget.copy_from(b"").expect("first empty payload");
        assert!(matches!(
            budget.copy_from(b""),
            Err(TransportPayloadError::BindingBudget(_))
        ));
        assert_eq!(current(&runtime, ResourceClass::QueuedMessages), 1);
        drop(first);
        assert_eq!(current(&runtime, ResourceClass::QueuedMessages), 0);

        drop(budget.copy_from(b"").expect("capacity recovered"));
        assert_eq!(current(&runtime, ResourceClass::QueuedMessages), 0);
    }
}
