//! New VM backend for Otter.
//!
//! This crate is the fresh execution backend that will replace the current VM
//! architecture incrementally. It starts as a small scaffold with a strict
//! module split and a minimal public API.

#![deny(clippy::all)]
#![forbid(unsafe_code)]

/// Shared execution ABI.
pub mod abi;
/// Engine/runtime integration boundary.
pub mod bridge;
/// Runtime bytecode model.
pub mod bytecode;
/// Deoptimization metadata and handoff types.
pub mod deopt;
/// Feedback and profiling side-table layout.
pub mod feedback;
/// Frame and register-window layout.
pub mod frame;
/// Bytecode interpreter entry points.
pub mod interpreter;
/// JIT-facing ABI surface.
pub mod jit_abi;
/// Executable module and function containers.
pub mod module;

pub use abi::VmAbiVersion;
pub use frame::FrameLayout;
pub use interpreter::Interpreter;
pub use module::Module;

/// Returns the current execution ABI version of the new VM.
#[must_use]
pub const fn abi_version() -> VmAbiVersion {
    VmAbiVersion::V1
}
