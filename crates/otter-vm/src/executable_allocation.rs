//! Fallible preparation of admitted executable metadata buffers.
//!
//! # Contents
//! - Exact array/string allocation over the caller's physical resource lease.
//! - Deep scope copies preserving independent isolate-owned context metadata.
//!
//! # Invariants
//! - Callers admit the requested final metadata before preparing buffers.
//! - Actual Vec/String allocator failures retain their original TryReserveError.
//! - Capacity above the requested amount is admitted before any publication;
//!   the caller settles the final boxed/retained geometry before returning it.
//! - These helpers create no resource account, parallel ledger or owner carrier.
//! - Standard Arc/Box owner headers use stable infallible std constructors;
//!   their physical bytes are admitted before publication, without a custom
//!   allocator abstraction or a claim of complete system-OOM recoverability.
//!
//! # See also
//! - `crate::executable` owns body/table leases and final retained geometry.
//! - `crate::code_space` owns linked compiler and directory allocations.

use otter_resource::{ResourceClass, ResourceError, ResourceLease};

pub(crate) fn array_bytes<T>(capacity: usize) -> u64 {
    (capacity as u64).saturating_mul(std::mem::size_of::<T>() as u64)
}

pub(crate) fn try_vec<T>(
    capacity: usize,
    lease: &mut ResourceLease,
) -> Result<Vec<T>, ResourceError> {
    let mut buffer = Vec::new();
    buffer
        .try_reserve_exact(capacity)
        .map_err(|cause| ResourceError::Allocation {
            class: ResourceClass::SourceModuleBytes,
            capacity,
            requested: array_bytes::<T>(capacity),
            cause,
        })?;
    let extra = array_bytes::<T>(buffer.capacity().saturating_sub(capacity));
    if extra != 0 {
        lease.resize(lease.amount().saturating_add(extra))?;
    }
    Ok(buffer)
}

pub(crate) fn try_copy<T: Clone>(
    source: &[T],
    lease: &mut ResourceLease,
) -> Result<Box<[T]>, ResourceError> {
    let mut buffer = try_vec(source.len(), lease)?;
    buffer.extend_from_slice(source);
    Ok(buffer.into_boxed_slice())
}

pub(crate) fn try_string(source: &str, lease: &mut ResourceLease) -> Result<String, ResourceError> {
    let mut buffer = String::new();
    buffer
        .try_reserve_exact(source.len())
        .map_err(|cause| ResourceError::Allocation {
            class: ResourceClass::SourceModuleBytes,
            capacity: source.len(),
            requested: source.len() as u64,
            cause,
        })?;
    let extra = buffer.capacity().saturating_sub(source.len()) as u64;
    if extra != 0 {
        lease.resize(lease.amount().saturating_add(extra))?;
    }
    buffer.push_str(source);
    Ok(buffer)
}

pub(crate) fn try_scopes<'a>(
    source: impl ExactSizeIterator<Item = &'a otter_bytecode::ScopeDescriptor>,
    lease: &mut ResourceLease,
) -> Result<Box<[std::sync::Arc<otter_bytecode::SharedScope>]>, ResourceError> {
    let mut scopes = try_vec(source.len(), lease)?;
    for scope in source {
        let mut slots = try_vec(scope.slots.len(), lease)?;
        for slot in &scope.slots {
            slots.push(otter_bytecode::SlotDescriptor {
                name: try_string(&slot.name, lease)?,
                kind: slot.kind,
                exported: slot.exported,
            });
        }
        scopes.push(std::sync::Arc::new(otter_bytecode::SharedScope::new(
            otter_bytecode::ScopeDescriptor {
                kind: scope.kind,
                flags: scope.flags,
                slots,
            },
        )));
    }
    Ok(scopes.into_boxed_slice())
}

/// UTF-8 bytes of the property-key spelling of `source`.
pub(crate) fn utf16_bytes(source: &[u16]) -> u64 {
    crate::wtf16_escape::encoded_len(source) as u64
}

/// The property-key spelling of `source` (see [`crate::wtf16_escape`]),
/// allocated against `lease`.
pub(crate) fn try_utf16(
    source: &[u16],
    lease: &mut ResourceLease,
) -> Result<String, ResourceError> {
    let requested = utf16_bytes(source);
    let capacity = requested as usize;
    let mut buffer = String::new();
    buffer
        .try_reserve_exact(capacity)
        .map_err(|cause| ResourceError::Allocation {
            class: ResourceClass::SourceModuleBytes,
            capacity,
            requested,
            cause,
        })?;
    let extra = buffer.capacity().saturating_sub(capacity) as u64;
    if extra != 0 {
        lease.resize(lease.amount().saturating_add(extra))?;
    }
    buffer.extend(crate::wtf16_escape::escaped_chars(source));
    Ok(buffer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use otter_resource::ResourceAccount;
    use std::error::Error;

    #[test]
    fn actual_capacity_overflow_preserves_allocator_cause_and_unpublished_lease_rolls_back() {
        let account = ResourceAccount::default();
        let requested = array_bytes::<u64>(usize::MAX);
        let mut lease = account
            .reserve_exact(ResourceClass::SourceModuleBytes, requested)
            .unwrap();
        let error = try_vec::<u64>(usize::MAX, &mut lease).unwrap_err();
        let ResourceError::Allocation {
            class,
            capacity,
            requested: actual_bytes,
            cause,
        } = &error
        else {
            panic!("actual Vec capacity overflow")
        };
        assert_eq!(*class, ResourceClass::SourceModuleBytes);
        assert_eq!(*capacity, usize::MAX);
        assert_eq!(*actual_bytes, requested);
        assert_eq!(
            error
                .source()
                .unwrap()
                .downcast_ref::<std::collections::TryReserveError>(),
            Some(cause)
        );
        assert_eq!(
            account
                .snapshot()
                .get(ResourceClass::SourceModuleBytes)
                .current(),
            requested
        );
        drop(lease);
        assert_eq!(
            account
                .snapshot()
                .get(ResourceClass::SourceModuleBytes)
                .current(),
            0
        );
    }
}
