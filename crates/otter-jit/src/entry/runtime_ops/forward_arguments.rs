//! Committed forwarding requests from compiled argument windows.
//!
//! # Contents
//! - Staging from intrinsic apply, callee, receiver and current mapped bindings.
//!
//! # Invariants
//! - The boxed packet is copied before collection or JavaScript reentry.
//! - Staged requests contain only actual arguments, including unmapped extras.
//! - The selected callee entry builds its frame and initializes missing formals.
//! - Completion uses the shared NativeResultPair value/exception domain.
//!
//! # See also
//! - `otter_vm::runtime_activation` — checked compiled activation operations.

use super::JitCtx;
use otter_vm::native_abi::CommittedValueError;

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
        Err(CommittedValueError::Fatal(
            otter_vm::VmError::InvalidOperand,
        ))
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
                .map_err(CommittedValueError::Fatal)
                .and_then(|mut runtime| runtime.stage_forward_values(&values))
        })
        .and_then(|(callee, receiver, arguments)| {
            ctx.stage_call_request(callee, receiver, arguments)
                .map_err(CommittedValueError::JavaScript)
        })
        .map(|()| otter_vm::Value::undefined());
    super::committed_value_result(ctx, result)
}
