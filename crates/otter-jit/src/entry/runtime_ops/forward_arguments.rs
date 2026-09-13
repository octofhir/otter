//! Leaf ABI entries for native argument forwarding.
//!
//! # Contents
//! - Intrinsic-apply/live-window count and runtime target-plan probes.
//! - Incoming/captured argument copy before generated register-alias stores.
//!
//! # Invariants
//! - All probes and copies are leaf/no-allocation with no observable JS effects.
//! - Count/plan/copy misses return `u64::MAX`; success returns actual count in the
//!   engine-owned descriptors. No failure parks an exception.
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
        return u64::MAX;
    };
    // SAFETY: generated code supplies its live entry-lifetime context.
    let ctx = unsafe { &mut *ctx };
    let Ok(runtime) = ctx.runtime_call() else {
        return u64::MAX;
    };
    // SAFETY: shared generated linkage owns a complete initialized private
    // destination frame and keeps it disjoint from the published caller.
    unsafe { runtime.copy_forwarded_argument_window(destination, parameter_count) }
        .map_or(u64::MAX, u64::from)
}

/// Write the existing engine plan into caller-owned native scratch. The metadata
/// contains no moving value; a miss leaves scratch unread and has no JS effect.
pub(crate) extern "C" fn jit_forward_call_plan_stub(
    ctx: *mut JitCtx,
    method: u64,
    callee: u64,
    output: *mut otter_vm::jit::JitDirectCallPlan,
) -> u64 {
    // SAFETY: generated linkage owns the live context and an aligned, disjoint
    // plan-sized scratch reservation. This leaf never allocates or reenters.
    let ctx = unsafe { &mut *ctx };
    let Ok(runtime) = ctx.runtime_call() else {
        return u64::MAX;
    };
    let Some(count) = runtime.forward_argument_count(otter_vm::Value::from_bits(method)) else {
        return u64::MAX;
    };
    let Some(plan) = runtime.forwarded_call_plan(otter_vm::Value::from_bits(callee)) else {
        return u64::MAX;
    };
    // SAFETY: the private scratch is initialized exactly once before any read.
    unsafe { output.write(plan) };
    u64::from(count)
}
