//! Authoritative native VM/JIT ABI crate map.
//!
//! The focused modules below are the single source of machine-observed frame,
//! dispatch, runtime-stub, safepoint, code metadata, and dependency layouts.
//! This module intentionally contains only shared ids, sentinels, re-exports,
//! and compile-time glue.
//!
//! # Contents
//! - [`call_trampoline`] — native-stack frame creation and call continuations.
//! - [`call_thread`] — the shared execution context and callable entry ABI.
//! - [`code_entry`] — stable per-generation native entry cells.
//! - [`frame`] — VM thread and activation layouts.
//! - [`dispatch`] — tier and runtime-stub result/status layouts.
//! - [`runtime_stubs`] — classified descriptor inventory and call packets.
//! - [`safepoints`] — frame/spill maps and safepoint entries.
//! - [`metadata`] — code-object metadata and dependencies.
//! - [`source_work`] — canonical saturating source-opcode work storage.
//!
//! # Invariants
//! - There is one native ABI. No tier owns a private frame, status, or
//!   safepoint representation.
//! - Machine-observed records are C-layout and compile-time asserted.
//! - Addresses are fixed-width `u64` values and are never Rust layout handles.
//!
//! # See also
//! - [`crate::active_frame`] for tier-neutral semantic frame access.
//! - [`crate::jit`] for the compiler service boundary.

mod call_thread;
mod call_trampoline;
mod code_entry;
mod dispatch;
mod frame;
mod metadata;
mod return_pc;
mod runtime_stubs;
mod safepoints;
mod source_work;

pub use call_thread::*;
pub use call_trampoline::*;
pub use code_entry::*;
pub use dispatch::*;
pub use frame::*;
pub use metadata::*;
pub(crate) use return_pc::*;
pub use runtime_stubs::*;
pub use safepoints::*;
pub use source_work::*;

// Reentrant runtime-call surface re-exported beside the stub descriptors, so
// generated-code entry points import one ABI namespace.
pub use crate::runtime_activation::{
    BinaryOperator, ClassRuntimeOp, CommittedValueError, IteratorRuntimeOutcome,
    ObjectProtocolValueOp, RuntimeCall, ScalarValueOp, ValueLoadRuntimeOp,
};
pub use crate::{ActiveFrameMut, ActiveFrameRef};

/// Dense identifier for one deopt/side-exit frame state.
pub type FrameStateId = u32;
/// Dense identifier for one code-object-owned safepoint.
pub type SafepointId = u32;
/// Dense identifier for one runtime-stub descriptor.
pub type RuntimeStubId = u32;

/// Sentinel for calls that cannot allocate and have no safepoint.
pub const NO_SAFEPOINT: SafepointId = u32::MAX;
/// Sentinel for guards/calls with no frame state.
pub const NO_FRAME_STATE: FrameStateId = u32::MAX;

const _: [(); 4] = [(); std::mem::size_of::<FrameStateId>()];
const _: [(); 4] = [(); std::mem::size_of::<SafepointId>()];
const _: [(); 4] = [(); std::mem::size_of::<RuntimeStubId>()];

/// Private engine terminal constructor entry; the existing compiled pair is
/// retained by the emitted caller's aligned packet until this leaf returns.
#[doc(hidden)]
pub use crate::constructor_layout::{constructor_receiver_commit, constructor_terminal};
