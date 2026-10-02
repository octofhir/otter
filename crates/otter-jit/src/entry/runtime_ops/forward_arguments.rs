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

/// Stage the complete pending call request of an admitted forwarding site.
///
/// The packet is `[method, callee, receiver, register bindings…, formals
/// context]`. Resolution may materialize the arguments object; the request is
/// written only after that, and the generated caller enters the trampoline.
pub(crate) extern "C" fn jit_stage_forward_stub(
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
    let result = copied
        .and_then(|values| {
            ctx.runtime_call()
                .and_then(|mut runtime| runtime.stage_forward_values(&values))
        })
        .and_then(|(callee, receiver, arguments)| {
            ctx.stage_call_request(callee, receiver, arguments)
        })
        .map(|()| otter_vm::Value::undefined());
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
