//! Native activation and generated-call deoptimization entries.
//!
//! # Contents
//! - Native activation publication and release for nested compiled calls.
//! - Closure validation for scratch-frame inline calls.
//! - Generated-call cold deoptimization.
//!
//! # Invariants
//! - Every entry receives a live [`JitCtx`] whose frame/register roots remain
//!   published across allocating or reentrant VM work.
//! - JavaScript exceptions return as pure values in the compiled
//!   `NativeResultPair` domain and never unwind through generated machine frames.
//!
//! # See also
//! - `super::reentry` — shared throw resumption and error parking.
//! - `otter_vm::interp::jit_call` — VM-owned generated-call accounting.

use otter_vm::{
    VmError,
    native_abi::{NativeResultPair, NativeResultStatus},
};

use super::super::JitCtx;
use super::reentry::{compiled_error, compiled_fatal, park_jit_error};

/// Publish one machine-constructed [`JitCtx`] before its compiled entry can
/// reach an allocating/reentrant safepoint. Structural failure is parked in
/// the shared slot and reported as [`NativeResultStatus::Fatal`].
pub(crate) extern "C" fn jit_push_native_activation_stub(ctx: *mut JitCtx) -> u64 {
    // SAFETY: the caller has fully initialized `ctx` on its native stack and
    // keeps it live until the matching pop stub.
    let ctx = unsafe { &mut *ctx };
    let vm = unsafe { &mut *ctx.activation().vm_ptr() };
    let Some(frame) = (unsafe { ctx.native_frame.as_mut() }) else {
        park_jit_error(ctx, VmError::InvalidOperand);
        return NativeResultStatus::Fatal as u64;
    };
    // SAFETY: the complete canonical frame remains live on the native stack
    // until the matching pop; the VM publishes and traces it as one unit.
    match unsafe { vm.jit_push_native_frame(frame) } {
        Ok(()) => NativeResultStatus::Success as u64,
        Err(err) => {
            park_jit_error(ctx, err);
            NativeResultStatus::Fatal as u64
        }
    }
}

/// Release the topmost native JIT activation before its `JitCtx` stack record
/// is discarded.
pub(crate) extern "C" fn jit_pop_native_activation_stub(ctx: *mut JitCtx) -> u64 {
    // SAFETY: the active context and its interpreter pointer are live by ABI.
    let ctx = unsafe { &mut *ctx };
    let vm = unsafe { &mut *ctx.activation().vm_ptr() };
    vm.jit_pop_native_activation();
    NativeResultStatus::Success as u64
}

/// Validate a closure callee for scratch-frame inlining and return its captured
/// upvalue-spine base, or `0` when the site must take the normal call path.
pub(crate) extern "C" fn jit_inline_closure_upvalues_stub(
    ctx: *mut JitCtx,
    callee_reg: u64,
    expected_fid: u64,
) -> usize {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let callee = match ctx
        .active_frame_mut()
        .and_then(|frame| frame.read(callee_reg as u16))
    {
        Ok(callee) => callee,
        Err(_) => return 0,
    };
    let vm = unsafe { &mut *ctx.activation().vm_ptr() };
    vm.jit_inline_closure_upvalues(callee, expected_fid as u32)
        .unwrap_or(0)
}

/// Cold deoptimization for one already-started generated stack call.
///
/// The native frame stays published for this entire transition. The VM copies
/// its tagged window into interpreter-owned storage, validates that the Bail
/// payload names the same exact published PC, resumes there, and returns the
/// completed value without replaying `Call`.
pub(crate) extern "C" fn jit_deopt_stack_call_stub(
    ctx: *mut JitCtx,
    callee_frame: *mut otter_vm::native_abi::NativeFrame,
    caller_function_id: u64,
    caller_call_pc: u64,
    callee_code_object_id: u64,
    caller_code_object_id: u64,
    call_kind: u64,
    callee_side_exit: u64,
) -> NativeResultPair {
    // SAFETY: generated linkage keeps both context and stack frame live and
    // published until this function returns.
    let ctx = unsafe { &mut *ctx };
    let Some(callee_frame) = (unsafe { callee_frame.as_mut() }) else {
        return compiled_fatal(ctx, VmError::InvalidOperand);
    };
    let Some(callee_side_exit) = otter_vm::native_abi::SideExit::from_bits(callee_side_exit) else {
        return compiled_fatal(ctx, VmError::InvalidOperand);
    };
    if callee_side_exit.logical_pc() != callee_frame.header.pc {
        return compiled_fatal(ctx, VmError::InvalidOperand);
    }
    let (Ok(caller_function_id), Ok(caller_call_pc)) = (
        u32::try_from(caller_function_id),
        u32::try_from(caller_call_pc),
    ) else {
        return compiled_fatal(ctx, VmError::InvalidOperand);
    };
    let call_kind = match call_kind {
        0 => otter_vm::JitDirectCallKind::Plain,
        1 => otter_vm::JitDirectCallKind::Method,
        2 => otter_vm::JitDirectCallKind::Construct,
        3 => otter_vm::JitDirectCallKind::DerivedConstruct,
        4 => otter_vm::JitDirectCallKind::SuperConstruct,
        5 => otter_vm::JitDirectCallKind::DerivedSuperConstruct,
        _ => {
            return compiled_fatal(ctx, VmError::InvalidOperand);
        }
    };
    let activation = ctx.activation();
    let vm = unsafe { &mut *activation.vm_ptr() };
    let stack = unsafe { &mut *activation.stack_ptr() };
    let context = unsafe { &*activation.context_ptr() };
    match vm.jit_deopt_materialize_stack_call(
        context,
        stack,
        callee_frame,
        caller_function_id,
        caller_call_pc,
        callee_code_object_id,
        caller_code_object_id,
        call_kind,
        callee_side_exit,
    ) {
        Ok(value) => NativeResultPair::success(value),
        Err(err) => compiled_error(ctx, err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use otter_vm::{
        Value,
        native_abi::{
            NativeFrame, NativeFrameKind, NativeResultDomain, NativeResultStatus, VmFrameHeader,
            VmThread,
        },
    };

    #[test]
    fn deopt_stack_call_rejects_a_bail_payload_that_disagrees_with_frame_pc() {
        let mut registers = [Value::undefined()];
        let mut frame = NativeFrame::new(
            VmFrameHeader {
                function_id: 7,
                pc: 41,
                register_count: 1,
                kind: NativeFrameKind::Optimizing,
                flags: Default::default(),
            },
            registers.as_mut_ptr() as u64,
            Value::function(7),
            Value::undefined(),
        );
        let mut thread = VmThread::empty();
        let mut error = None;
        let mut ctx = JitCtx {
            thread: std::ptr::addr_of_mut!(thread),
            native_frame: std::ptr::null_mut(),
            error: std::ptr::addr_of_mut!(error),
            activation_base: std::ptr::null_mut(),
            activation_top_ptr: std::ptr::null_mut(),
            activation_limit: 0,
            global_this_offset: std::ptr::null(),
            native_stack_limit: 0,
            generated_feedback_clean: 1,
            machine_roots_ptr: std::ptr::null_mut(),
            receiver_alloc: otter_vm::jit::JitMachineAllocationWindow::disabled(),
            runtime_stats: std::ptr::null_mut(),
        };

        let result = jit_deopt_stack_call_stub(&mut ctx, &mut frame, 1, 2, 3, 4, 0, 42);
        assert_eq!(
            result.validate(NativeResultDomain::Compiled),
            Some(NativeResultStatus::Fatal)
        );
        assert!(matches!(error, Some(VmError::InvalidOperand)));
    }
}
