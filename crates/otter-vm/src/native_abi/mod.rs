//! Authoritative native VM/JIT ABI crate map.
//!
//! The focused modules below are the single source of machine-observed frame,
//! dispatch, runtime-stub, safepoint, code metadata, dependency, and version
//! layouts. This module intentionally contains only shared ids, sentinels,
//! re-exports, and compile-time glue.
//!
//! # Contents
//! - [`code_entry`] — stable per-generation native entry cells.
//! - [`frame`] — VM thread and activation layouts.
//! - [`dispatch`] — tier and runtime-stub result/status layouts.
//! - [`runtime_stubs`] — classified descriptor inventory and table header.
//! - [`safepoints`] — frame/spill maps and safepoint entries.
//! - [`metadata`] — code-object metadata, dependencies, and versions.
//!
//! # Invariants
//! - There is one native ABI. No tier owns a private frame, status, or
//!   safepoint representation.
//! - Machine-observed records are C-layout and compile-time asserted.
//! - Addresses are fixed-width `u64` values and are never Rust layout handles.
//!
//! # See also
//! - [`crate::jit`] for the compiler service boundary.
//! - `JIT_REFACTOR_PLAN.md` for phase gates.

mod code_entry;
mod dispatch;
mod frame;
mod metadata;
mod runtime_stubs;
mod safepoints;

pub use code_entry::*;
pub use dispatch::*;
pub use frame::*;
pub use metadata::*;
pub use runtime_stubs::*;
pub use safepoints::*;

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
