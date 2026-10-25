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
/// proof that selects the prototype, the layout proof of the prototype, the
/// slot that layout fixes, and the native the slot must hold.
#[derive(Debug, Clone, Copy)]
pub(crate) struct IntrinsicMethod {
    pub(crate) receiver: JitIntrinsicPrototype,
    pub(crate) holder: IntrinsicHolder,
    pub(crate) field: FieldLocation,
    pub(crate) native_ref: u32,
}

/// The builtin method a guarded call on an intrinsic receiver at `byte_pc`
/// reads, with the site and entry its prototype reference names.
pub(crate) fn guarded_intrinsic_method(
    view: &otter_vm::JitCompileSnapshot,
    byte_pc: u32,
) -> Result<(IntrinsicMethod, (u32, u32)), crate::Unsupported> {
    let call = view
        .guarded_method_calls
        .get(&byte_pc)
        .ok_or(crate::Unsupported::OperandShape(
            "graph guarded method site",
        ))?;
    let (otter_vm::jit::JitGuardedReceiver::Exotic(receiver), Some(holder)) =
        (call.receiver, IntrinsicHolder::of(call.holder))
    else {
        return Err(crate::Unsupported::OperandShape(
            "graph guarded method proof",
        ));
    };
    Ok((
        IntrinsicMethod {
            receiver,
            holder,
            field: call.method_field,
            native_ref: call.builtin_native_ref,
        },
        (byte_pc, call.entry_stub_id),
    ))
}

/// How a pinned intrinsic prototype's slot layout is proved.
#[derive(Debug, Clone, Copy)]
pub(crate) enum IntrinsicHolder {
    /// A fast-mode prototype with this hidden-class handle offset.
    Shape(u32),
    /// A dictionary-mode prototype with this slot-layout epoch.
    Dictionary(u64),
}

impl IntrinsicHolder {
    /// The layout proof a guarded method call names for its holder, if it is
    /// one this record can carry.
    pub(crate) const fn of(holder: otter_vm::jit::JitMethodHolder) -> Option<Self> {
        match holder {
            otter_vm::jit::JitMethodHolder::Shape(shape) => Some(Self::Shape(shape)),
            otter_vm::jit::JitMethodHolder::Dictionary(layout) => Some(Self::Dictionary(layout)),
            _ => None,
        }
    }
}
