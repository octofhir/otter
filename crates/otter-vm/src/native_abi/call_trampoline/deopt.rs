//! Generated continuation after a physical callee side exit.
//!
//! # Contents
//! - [`deopt_call_entry`] owns preparation and interpreter continuation.
//! - The Rust helper installs an in-place resume request and returns to assembly.
//!
//! # Invariants
//! The caller retains the published callee and its complete register/actual
//! windows. Rust preparation ends before the trampoline resumes JavaScript.
//! No frame is copied, no call is replayed, and only committed success, throw or
//! fatal completion returns to the generated caller. Every platform preserves
//! its nonvolatile registers and aggregate-return convention.
//!
//! # See also
//! - `crate::runtime_activation::RuntimeCall` for exact preparation.
//! - [`super::call_trampoline`] for same-frame continuation execution.

use super::{CallRequest, JitCtx, NativeResultPair};
use crate::{
    VmError,
    native_abi::{NativeResultStatus, SideExit},
};

extern "C" fn prepare_call_resume(ctx: *mut JitCtx, exit: u64) -> NativeResultPair {
    // SAFETY: generated linkage retains this context and its published frame.
    let ctx = unsafe { &mut *ctx };
    let result = SideExit::from_bits(exit)
        .ok_or(VmError::InvalidOperand)
        .and_then(|exit| ctx.runtime_call()?.prepare_deopt_call(exit));
    if let Err(error) = result {
        if let Some(slot) = unsafe { ctx.error.as_mut() } {
            *slot = Some(error);
        }
        return NativeResultPair::fatal_internal();
    }
    ctx.pending_call = CallRequest::resume_interpreter();
    NativeResultPair::continue_execution()
}

/// Resume the exact published generated callee after its side exit.
///
/// # Safety
/// `ctx` retains a live runtime activation, error slot and canonical frame.
/// The complete physical chain and its root windows remain published until
/// this entry returns. `exit` is the callee's original encoded side exit.
#[cfg(target_arch = "aarch64")]
#[unsafe(naked)]
pub unsafe extern "C" fn deopt_call_entry(_ctx: *mut JitCtx, _exit: u64) -> NativeResultPair {
    core::arch::naked_asm!(
        "stp x29, x30, [sp, #-32]!",
        "mov x29, sp",
        "stp x19, x20, [sp, #16]",
        "mov x19, x0",
        "bl {prepare}",
        "cmp x1, {continue_status}",
        "b.ne 20f",
        "mov x0, x19",
        "ldp x19, x20, [sp, #16]",
        "ldp x29, x30, [sp], #32",
        "b {trampoline}",
        "20:",
        "ldp x19, x20, [sp, #16]",
        "ldp x29, x30, [sp], #32",
        "ret",
        prepare = sym prepare_call_resume,
        trampoline = sym super::call_trampoline,
        continue_status = const NativeResultStatus::Continue as u64,
    );
}

/// Resume the published callee using the x86 platform aggregate-return ABI.
///
/// # Safety
/// The publication, lifetime and side-exit requirements match the ARM64 entry.
#[cfg(target_arch = "x86_64")]
#[unsafe(naked)]
pub unsafe extern "C" fn deopt_call_entry(_ctx: *mut JitCtx, _exit: u64) -> NativeResultPair {
    core::arch::naked_asm!(
        "push rbp",
        "mov rbp, rsp",
        "push rbx",
        ".if {windows}",
        "push r12",
        "sub rsp, 48",
        "mov r12, rcx",
        "mov rbx, rdx",
        "call {prepare}",
        "cmp qword ptr [r12 + 8], {continue_status}",
        "jne 20f",
        "mov rcx, r12",
        "mov rdx, rbx",
        "add rsp, 48",
        "pop r12",
        "pop rbx",
        "pop rbp",
        "jmp {trampoline}",
        "20:",
        "mov rax, r12",
        "add rsp, 48",
        "pop r12",
        ".else",
        "sub rsp, 8",
        "mov rbx, rdi",
        "call {prepare}",
        "cmp rdx, {continue_status}",
        "jne 20f",
        "mov rdi, rbx",
        "add rsp, 8",
        "pop rbx",
        "pop rbp",
        "jmp {trampoline}",
        "20:",
        "add rsp, 8",
        ".endif",
        "pop rbx",
        "pop rbp",
        "ret",
        windows = const cfg!(target_os = "windows") as u8,
        prepare = sym prepare_call_resume,
        trampoline = sym super::call_trampoline,
        continue_status = const NativeResultStatus::Continue as u64,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ActivationStack, ExecutionContext, Frame, Interpreter, Value, VmRuntimeActivation,
        native_abi::{
            ExitAction, ExitReason, NativeFrameFlags, NativeFrameKind, VmFrameHeader, VmThread,
        },
    };

    #[test]
    fn native_deopt_entry_rejects_pc_and_payload_before_frame_transition() {
        let context = ExecutionContext::from_module(
            crate::test_support::minimal_bytecode_module("deopt-entry-validation.js"),
            crate::source_registry::SourceRegistry::default(),
        )
        .unwrap();
        let mut vm = Interpreter::new().expect("fixture interpreter bootstrap");
        let mut stack = ActivationStack::new();
        let mut activation = VmRuntimeActivation::new(&mut vm, &mut stack, Some(&context));
        let mut thread = VmThread::empty();
        thread.runtime_context = std::ptr::from_mut(&mut activation) as u64;
        let mut registers = [Value::number_i32(91)];
        let mut frame = Frame::new(
            VmFrameHeader {
                function_id: 0,
                pc: 41,
                register_count: 1,
                kind: NativeFrameKind::Optimizing,
                flags: NativeFrameFlags::from_bits(NativeFrameFlags::CONSTRUCT),
            },
            registers.as_mut_ptr() as u64,
            Value::function(0),
            Value::UNDEFINED,
        );
        frame.code_object_id = 73;
        let mut error = None;
        let mut ctx = super::super::tests::context(&mut thread, &mut error);
        ctx.native_frame = &mut frame;
        let published = ctx.native_frame;
        for exit in [
            0,
            SideExit::new(42, ExitReason::Int32Overflow, ExitAction::Recompile).to_bits(),
        ] {
            // SAFETY: the live fixture owns all records; validation fails
            // before allocation, generation policy or continuation dispatch.
            assert_eq!(
                unsafe { deopt_call_entry(&mut ctx, exit) },
                NativeResultPair::fatal_internal()
            );
            assert_eq!(ctx.native_frame, published);
            assert_eq!(frame.header.pc, 41);
            assert_eq!(frame.header.kind, NativeFrameKind::Optimizing);
            assert_eq!(frame.code_object_id, 73);
            assert!(frame.header.flags.contains(NativeFrameFlags::CONSTRUCT));
            assert_eq!(registers[0], Value::number_i32(91));
            assert!(matches!(
                unsafe { (&mut *ctx.error).take() },
                Some(VmError::InvalidOperand)
            ));
        }
    }
}
