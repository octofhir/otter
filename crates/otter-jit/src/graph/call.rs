//! Typed operands of Graph's committed collecting runtime boundary.
//!
//! # Contents
//! - [`CommittedArgument`] distinguishes live values, scalars and addresses.
//!
//! # Invariants
//! - Each encoder stages value operands before overwriting call ABI words.
//! - Baked addresses retain symbolic relocation ownership.
//! - Stack addresses are relative to the currently staged argument span;
//!   canonical homes remain the collector's sole long-lived value storage.
//! - This recipe owns no frame, runtime entry or second result carrier.
//!
//! # See also
//! - [`super::frame`] for canonical home geometry.
//! - [`super::arm64`] for the AArch64 committed boundary encoder.

use crate::artifact::relocation::RelocationTarget;

pub(crate) enum CommittedArgument {
    Value(u8),
    Scalar(u64),
    Address(u64, RelocationTarget),
    StackAddress(u32),
}
