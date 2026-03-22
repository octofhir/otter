//! Shared execution ABI definitions for the new VM.

/// Version tag for the execution ABI shared by the interpreter and the future JIT.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VmAbiVersion {
    /// Initial ABI for the new `otter-vm` crate.
    V1,
}
