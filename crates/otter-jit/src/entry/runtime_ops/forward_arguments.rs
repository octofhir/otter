//! Native argument-forwarding probes, copies and committed completion.
//!
//! # Contents
//! - Intrinsic-apply/live-window count and runtime target-plan probes.
//! - Incoming/captured argument copy before generated register-alias stores.
//! - Admitted boxed-value completion and pre-effect source readiness.
//!
//! # Invariants
//! - Probes and copies are leaf/no-allocation with no observable JS effects.
//! - Committed completion copies its packet and returns a pure value/exception.
//! - Count/plan/copy misses return `u64::MAX`; success returns actual count in the
//!   engine-owned descriptors. Probe/copy failures never park an exception.
//! - Committed completion uses the shared NativeResultPair status domain.
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

/// Complete an admitted forwarding source from explicit values. The packet is
/// copied before binding runtime services; its words never become extra roots.
pub(crate) extern "C" fn jit_call_forward_arguments_stub(
    ctx: *mut JitCtx,
    packet: *const otter_vm::Value,
    count: u32,
) -> otter_vm::native_abi::NativeResultPair {
    let copied = if count < 3
        || packet.is_null()
        || !(packet as usize).is_multiple_of(std::mem::align_of::<otter_vm::Value>())
    {
        Err(otter_vm::VmError::InvalidOperand)
    } else {
        // SAFETY: the caller provides count initialized, aligned Value words.
        // Copying ends the native-memory borrow before collection or reentry.
        let values = unsafe { std::slice::from_raw_parts(packet, count as usize) };
        Ok(smallvec::SmallVec::<[otter_vm::Value; 8]>::from_slice(
            values,
        ))
    };
    // SAFETY: generated code supplies its live entry-lifetime context.
    let ctx = unsafe { &mut *ctx };
    let result = copied.and_then(|values| {
        ctx.runtime_call()
            .and_then(|mut runtime| runtime.call_forward_values(&values))
    });
    super::committed_vm_result(ctx, result)
}

/// Probe whether caller materialization must precede the committed boundary.
pub(crate) extern "C" fn jit_forward_source_ready_stub(ctx: *mut JitCtx, method: u64) -> u64 {
    // SAFETY: generated code supplies its live entry-lifetime context.
    let ctx = unsafe { &mut *ctx };
    u64::from(
        ctx.runtime_call().is_ok_and(|runtime| {
            runtime.forward_call_can_complete(otter_vm::Value::from_bits(method))
        }),
    )
}
