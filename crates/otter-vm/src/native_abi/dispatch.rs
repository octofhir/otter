//! Fixed status/result contracts for tier and runtime-stub dispatch.
//!
//! # Contents
//! - [`DispatchStatus`] and [`DispatchResult`] cross compiled-entry boundaries.
//! - [`RuntimeStubStatus`] and result records cross runtime-stub boundaries.
//!
//! # Invariants
//! - JavaScript exceptions never unwind through native frames; `Throw` means a
//!   rooted exception is published on [`super::VmThread`].
//! - Every non-success condition has an explicit discriminant and payload.
//! - Records are fixed-width C-layout values suitable for two-register returns.
//! - A `SideExit` payload is the exact instruction-index PC of an uncommitted
//!   instruction. All earlier instructions are committed, frame registers are
//!   materialized, and the interpreter must continue at (not after) that PC.
//! - A runtime operation that has started observable work either completes its
//!   opcode or reports `Throw`; it cannot return a resumable side exit that
//!   would repeat the operation.
//!
//! # See also
//! - [`super::runtime_stubs`] for descriptor-side status classification.

use super::FrameStateId;

/// Result of interpreter or compiled dispatch.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchStatus {
    /// Function returned; `value_bits` is the completion value.
    Return = 0,
    /// Resume the interpreter at `payload` logical PC.
    SideExit = 1,
    /// A rooted pending exception is stored on [`super::VmThread`].
    Throw = 2,
    /// Interrupt/budget handling is required at `payload` logical PC.
    Interrupt = 3,
    /// Fatal allocation failure.
    OutOfMemory = 4,
}

/// Fixed two-word result shared by interpreter and compiled entries.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DispatchResult {
    /// Dispatch action.
    pub status: DispatchStatus,
    /// Logical PC, reason id, or zero depending on `status`.
    pub payload: u32,
    /// Boxed completion value bits for [`DispatchStatus::Return`].
    pub value_bits: u64,
}

impl DispatchResult {
    /// Normal return.
    #[must_use]
    pub const fn returned(value_bits: u64) -> Self {
        Self {
            status: DispatchStatus::Return,
            payload: 0,
            value_bits,
        }
    }

    /// Exact logical-PC side exit.
    #[must_use]
    pub const fn side_exit(logical_pc: u32) -> Self {
        Self {
            status: DispatchStatus::SideExit,
            payload: logical_pc,
            value_bits: 0,
        }
    }

    /// Throw with the exception rooted in the VM thread.
    #[must_use]
    pub const fn thrown() -> Self {
        Self {
            status: DispatchStatus::Throw,
            payload: 0,
            value_bits: 0,
        }
    }
}

/// Status code returned by a runtime stub.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeStubStatus {
    /// Stub completed and `value_bits` carries the JS result.
    Ok = 0,
    /// Guarded fast path was not applicable.
    Miss = 1,
    /// Stub threw and published a rooted pending exception.
    Throw = 2,
    /// Stub requests an exact frame-state exit.
    Deopt = 3,
    /// Allocation failed.
    OutOfMemory = 4,
    /// Runtime interrupt or budget stop.
    Interrupt = 5,
}

/// Rust-facing fixed-width runtime-stub result.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeStubResult {
    /// Result status.
    pub status: RuntimeStubStatus,
    /// Raw boxed value bits when status is `Ok`.
    pub value_bits: u64,
    /// Status-specific payload.
    pub payload: u64,
}

impl RuntimeStubResult {
    /// Successful result from boxed value bits.
    #[must_use]
    pub const fn ok_bits(value_bits: u64) -> Self {
        Self {
            status: RuntimeStubStatus::Ok,
            value_bits,
            payload: 0,
        }
    }

    /// Successful result from a VM value.
    #[must_use]
    pub(crate) const fn ok_value(value: crate::Value) -> Self {
        Self::ok_bits(value.to_abi_bits())
    }

    /// Guard miss.
    #[must_use]
    pub const fn miss() -> Self {
        Self {
            status: RuntimeStubStatus::Miss,
            value_bits: 0,
            payload: 0,
        }
    }

    /// Exact frame-state exit.
    #[must_use]
    pub const fn deopt(frame_state: FrameStateId) -> Self {
        Self {
            status: RuntimeStubStatus::Deopt,
            value_bits: 0,
            payload: frame_state as u64,
        }
    }

    /// Allocation failure.
    #[must_use]
    pub const fn out_of_memory() -> Self {
        Self {
            status: RuntimeStubStatus::OutOfMemory,
            value_bits: 0,
            payload: 0,
        }
    }

    /// Extract a successful VM value.
    #[must_use]
    pub(crate) const fn into_value(self) -> Option<crate::Value> {
        match self.status {
            RuntimeStubStatus::Ok => Some(crate::Value::from_abi_bits(self.value_bits)),
            _ => None,
        }
    }
}

/// Two-register machine runtime-stub result.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeStubResultPair {
    /// Raw boxed value bits when status is `Ok`.
    pub value_bits: u64,
    /// Low byte is status; high 56 bits are payload.
    pub status_payload: u64,
}

impl RuntimeStubResultPair {
    /// Pack a Rust-facing result.
    #[must_use]
    pub const fn from_result(result: RuntimeStubResult) -> Self {
        Self {
            value_bits: result.value_bits,
            status_payload: ((result.payload & 0x00ff_ffff_ffff_ffff) << 8) | result.status as u64,
        }
    }

    /// Decode status.
    #[must_use]
    pub const fn status(self) -> RuntimeStubStatus {
        match (self.status_payload & 0xff) as u8 {
            0 => RuntimeStubStatus::Ok,
            1 => RuntimeStubStatus::Miss,
            2 => RuntimeStubStatus::Throw,
            3 => RuntimeStubStatus::Deopt,
            4 => RuntimeStubStatus::OutOfMemory,
            _ => RuntimeStubStatus::Interrupt,
        }
    }

    /// Decode payload.
    #[must_use]
    pub const fn payload(self) -> u64 {
        self.status_payload >> 8
    }

    /// Convert to the Rust-facing record.
    #[must_use]
    pub const fn into_result(self) -> RuntimeStubResult {
        RuntimeStubResult {
            status: self.status(),
            value_bits: self.value_bits,
            payload: self.payload(),
        }
    }
}

const _: [(); 16] = [(); std::mem::size_of::<DispatchResult>()];
const _: [(); 8] = [(); std::mem::align_of::<DispatchResult>()];
const _: [(); 24] = [(); std::mem::size_of::<RuntimeStubResult>()];
const _: [(); 16] = [(); std::mem::size_of::<RuntimeStubResultPair>()];
const _: [(); 8] = [(); std::mem::align_of::<RuntimeStubResultPair>()];
const _: [(); 8] = [(); std::mem::offset_of!(DispatchResult, value_bits)];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pair_round_trips_status_and_payload() {
        let result = RuntimeStubResult::deopt(17);
        assert_eq!(
            RuntimeStubResultPair::from_result(result).into_result(),
            result
        );
    }

    #[test]
    fn successful_value_round_trips() {
        let value = crate::Value::number_i32(42);
        assert_eq!(RuntimeStubResult::ok_value(value).into_value(), Some(value));
    }
}
