//! Typed operands of Graph's committed collecting runtime boundary.
//!
//! # Contents
//! - [`CommittedArgument`] distinguishes live values, scalars and addresses.
//! - [`IntrinsicMethod`] names a builtin a receiver reads from its pinned
//!   intrinsic prototype, as each encoder's identity guard proves it.
//!
//! # Invariants
//! - Each encoder stages value operands before overwriting call ABI words.
//! - Baked addresses retain symbolic relocation ownership.
//! - Stack addresses are relative to the currently staged argument span;
//!   canonical homes remain the collector's sole long-lived value storage.
//! - This recipe owns no frame, runtime entry or second result carrier.
//! - An intrinsic method's prototype is pinned; its hidden class fixes the
//!   method slot, whose value the guard still compares by native identity.
//!
//! # See also
//! - [`super::frame`] for canonical home geometry.
//! - [`super::arm64`] for the AArch64 committed boundary encoder.

use crate::artifact::relocation::RelocationTarget;
use otter_vm::jit::JitIntrinsicPrototype;
use otter_vm::object::FieldLocation;

pub(crate) enum CommittedArgument {
    Value(u8),
    Scalar(u64),
    Address(u64, RelocationTarget),
    StackAddress(u32),
}

/// A builtin method read from a pinned intrinsic prototype: the receiver
/// proof that selects the prototype, the prototype's hidden class, the slot
/// that class fixes, and the native the slot must hold.
#[derive(Debug, Clone, Copy)]
pub(crate) struct IntrinsicMethod {
    pub(crate) receiver: JitIntrinsicPrototype,
    pub(crate) holder_shape: u32,
    pub(crate) field: FieldLocation,
    pub(crate) native_ref: u32,
}
