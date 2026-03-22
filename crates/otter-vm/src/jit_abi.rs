//! JIT-facing ABI placeholders for the new VM.

use crate::abi::VmAbiVersion;

/// Reports the ABI version expected by the future JIT integration.
#[must_use]
pub const fn jit_abi_version() -> VmAbiVersion {
    VmAbiVersion::V1
}
