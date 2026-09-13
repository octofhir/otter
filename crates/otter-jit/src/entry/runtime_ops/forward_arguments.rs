//! Leaf ABI entries for native argument forwarding.
//!
//! # Contents
//! - Intrinsic-apply/live-window count probe.
//! - Complete live argument copy before generated callee publication.
//!
//! # Invariants
//! - Both operations are leaf/no-allocation and have no observable JS effects.
//! - Count misses return `u64::MAX`; copy misses return one, matching the
//!   engine-owned descriptors. Neither failure parks an exception.
//!
//! # See also
//! - `otter_vm::runtime_activation` — checked compiled activation operations.

use super::JitCtx;

pub(crate) extern "C" fn jit_forward_argument_count_stub(ctx: *mut JitCtx, method: u64) -> u64 {
    // SAFETY: generated code supplies its live entry-lifetime context.
    let ctx = unsafe { &mut *ctx };
    ctx.runtime_call()
        .ok()
        .and_then(|runtime| runtime.forward_argument_count(otter_vm::Value::from_bits(method)))
        .map_or(u64::MAX, u64::from)
}

pub(crate) extern "C" fn jit_copy_forwarded_arguments_stub(
    ctx: *mut JitCtx,
    destination: *mut otter_vm::native_abi::NativeFrame,
    parameter_count: u64,
) -> u64 {
    let Ok(parameter_count) = u16::try_from(parameter_count) else {
        return 1;
    };
    // SAFETY: generated code supplies its live entry-lifetime context.
    let ctx = unsafe { &mut *ctx };
    let Ok(runtime) = ctx.runtime_call() else {
        return 1;
    };
    // SAFETY: shared generated linkage owns a complete initialized private
    // destination frame and keeps it disjoint from the published caller.
    u64::from(!unsafe { runtime.copy_forwarded_arguments(destination, parameter_count) })
}
