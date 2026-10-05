//! Generated writeback entry and preparation for scalar and inline deopt.
//!
//! # Contents
//! - Canonical-home/literal decoding and window writeback.
//! - Owned inline descendant inputs and in-place catch reconstruction.
//! - Architecture entries transfer to the common trampoline after Rust returns.
//!
//! # Invariants
//! Code-owned recipes and canonical homes remain valid throughout preparation.
//! The exit's recipe record is published before anything may collect, so the
//! collector roots exactly the homes the exit wrote.
//! Every physical recipe reads canonical homes or constants. No Rust
//! preparation frame runs JavaScript. One-frame writeback returns its exact
//! side exit; an inline exit resumes the restored physical frame and its
//! descendants after every VM borrow ends. The call is never replayed.
//!
//! # See also
//! - `otter_vm::native_abi::call_trampoline` for continuation ownership.
//! - `otter_vm::deopt::DeoptRuntime` for immutable writeback recipes.

use super::super::JitCtx;
use super::reentry::{compiled_error, compiled_fatal};
use otter_vm::{
    VmError,
    native_abi::{NativeResultPair, NativeResultStatus, SideExit},
};

/// Rebuild every interpreter frame a deopt exit owes, from deopt metadata.
///
/// The generated exit site selects a code-owned recipe. Graph exits populate
/// canonical stack homes before entering this helper.
/// This walks [`otter_vm::deopt::DeoptRuntime`] to reconstitute every owed frame.
///
/// An exit that owes only the compiled function's own frame writes it back
/// into the published window and reports `Bail(exact_pc)`: the activation the
/// entry already owns resumes at the exact PC.
///
/// An exit inside a spliced body owes a whole chain, and that chain is
/// prepared as owned descendant inputs. The enclosing physical activation is
/// restored in place. This helper returns before assembly runs the chain to
/// completion and returns its outermost result. The same continuation applies
/// regardless of how the generated function was entered.
///
/// Each spliced frame restores the exact callable from its entry recipe as
/// its SELF; the rebuilt frame reaches its captured bindings through that live
/// closure's context. Caller and callee bindings must never be substituted
/// merely because their function ids match.
extern "C" fn prepare_deopt_writeback(
    ctx: *mut JitCtx,
    exit_index: u64,
    deopt_runtime: *const otter_vm::deopt::DeoptRuntime,
    frame_sp: u64,
    window: u64,
) -> NativeResultPair {
    use otter_vm::deopt::{DeoptFrame, DeoptLocation, DeoptRuntime};

    // SAFETY: the live `JitCtx` reentry contract; the deopt-runtime allocation
    // lives exactly as long as the code that baked its address.
    let ctx = unsafe { &mut *ctx };
    let runtime: &DeoptRuntime = unsafe { &*deopt_runtime };
    let Some(exit) = runtime.exits.get(exit_index as usize) else {
        return compiled_fatal(ctx, VmError::InvalidOperand);
    };
    // Root exactly this recipe's homes, which the exit's cold moves wrote,
    // before any decoding or materialization step may collect.
    if exit.safepoint != otter_vm::native_abi::NO_SAFEPOINT {
        // SAFETY: the live entry contract publishes this native frame.
        let Some(frame) = (unsafe { ctx.native_frame.as_mut() }) else {
            return compiled_fatal(ctx, VmError::InvalidOperand);
        };
        frame.call_site = exit.safepoint;
    }
    let Some(state) = runtime.table.lookup(exit.state) else {
        return compiled_fatal(ctx, VmError::InvalidOperand);
    };
    if exit.resume_pcs.len() != state.frames.len() {
        return compiled_fatal(ctx, VmError::InvalidOperand);
    }

    let slot_raw = |location: DeoptLocation| -> u64 {
        match location {
            DeoptLocation::StackSlot(offset) => {
                let address = frame_sp.wrapping_add(offset as i64 as u64);
                // SAFETY: the verified deopt table bounds every stack-slot
                // offset inside the live spill frame.
                unsafe { *(address as *const u64) }
            }
            DeoptLocation::Literal(raw) => raw,
            DeoptLocation::VirtualObject(_) => {
                unreachable!("virtual objects are decoded from FrameState recipes")
            }
        }
    };
    let decode_physical = |slot: otter_vm::deopt::DeoptSlot| {
        slot.reconstitute(slot_raw).ok_or(VmError::InvalidOperand)
    };
    let write_frame = |frame: &DeoptFrame, window: *mut otter_vm::Value| {
        // SAFETY: the window spans exactly the frame's declared registers and
        // stays rooted for the writeback. Unlisted registers resume undefined.
        for register in 0..usize::from(frame.register_count) {
            unsafe { window.add(register).write(otter_vm::Value::undefined()) };
        }
        for &(register, slot) in frame.slots.iter() {
            let value = decode_physical(slot).unwrap_or_else(|_| otter_vm::Value::undefined());
            // SAFETY: verified recipes name registers inside the window.
            unsafe { window.add(usize::from(register)).write(value) };
        }
    };

    if state.is_single_frame() {
        let register_count = state.outermost().register_count;
        let Some(native_frame) = (unsafe { ctx.native_frame.as_ref() }) else {
            return compiled_fatal(ctx, VmError::InvalidOperand);
        };
        if native_frame.header.function_id != state.outermost().function_id {
            return compiled_fatal(ctx, VmError::InvalidOperand);
        }
        // SAFETY: runtime-capable JIT contexts publish this native frame for
        // the full compiled entry dynamic extent.
        let Some(native_frame) = (unsafe { ctx.native_frame.as_mut() }) else {
            return compiled_fatal(ctx, VmError::InvalidOperand);
        };
        native_frame.header.register_count = register_count;
        write_frame(state.outermost(), window as *mut otter_vm::Value);
        let recipes = match state
            .virtual_objects
            .iter()
            .map(|object| {
                Ok(otter_vm::deopt::VirtualObject {
                    id: object.id,
                    kind: object.kind,
                    fields: object
                        .fields
                        .iter()
                        .map(|slot| match slot.location {
                            DeoptLocation::VirtualObject(dependency) => {
                                Ok(otter_vm::deopt::VirtualMaterializationValue::VirtualObject(
                                    dependency,
                                ))
                            }
                            _ => decode_physical(*slot)
                                .map(otter_vm::deopt::VirtualMaterializationValue::Value),
                        })
                        .collect::<Result<_, VmError>>()?,
                })
            })
            .collect::<Result<Vec<_>, VmError>>()
        {
            Ok(recipes) => recipes,
            Err(error) => return compiled_fatal(ctx, error),
        };
        let materialized = if recipes.is_empty() {
            Vec::new()
        } else {
            match ctx
                .runtime_call()
                .and_then(|mut runtime| runtime.materialize_virtual_objects(&recipes))
            {
                Ok(values) => values,
                Err(error) => return compiled_error(ctx, error),
            }
        };
        for &(register, slot) in state.outermost().slots.iter() {
            let register = usize::from(register);
            let DeoptLocation::VirtualObject(object) = slot.location else {
                continue;
            };
            let Some(&value) = materialized.get(object.0 as usize) else {
                return compiled_fatal(ctx, VmError::InvalidOperand);
            };
            // SAFETY: the validated published window is full-width and no
            // allocation occurs after the materializer returns.
            unsafe { (window as *mut otter_vm::Value).add(register).write(value) };
        }
        native_frame.header.pc = exit.resume_pcs[0];
        let resume_pc = exit.resume_pcs[0];
        debug_assert_eq!(native_frame.header.pc, resume_pc);
        return NativeResultPair::side_exit(SideExit::new(resume_pc, exit.reason, exit.action));
    }

    // Decode the one shared frame schema. Root entry bindings remain owned by
    // the published native activation; descendants carry explicit operands.
    let decode = |slot: otter_vm::deopt::DeoptSlot| decode_physical(slot);
    let frames = match state
        .frames
        .iter()
        .map(|frame| {
            Ok(otter_vm::deopt::DeoptFrame {
                function_id: frame.function_id,
                byte_pc: frame.byte_pc,
                entry: frame
                    .entry
                    .map(|entry| {
                        Ok::<_, VmError>(otter_vm::deopt::DeoptFrameEntry {
                            return_register: entry.return_register,
                            this: decode(entry.this)?,
                            closure: decode(entry.closure)?,
                            new_target: decode(entry.new_target)?,
                        })
                    })
                    .transpose()?,
                register_count: frame.register_count,
                slots: frame
                    .slots
                    .iter()
                    .map(|&(register, slot)| Ok((register, decode(slot)?)))
                    .collect::<Result<_, VmError>>()?,
            })
        })
        .collect::<Result<Vec<_>, VmError>>()
    {
        Ok(frames) => frames,
        Err(error) => return compiled_fatal(ctx, error),
    };

    let vm = unsafe { &mut *ctx.activation().vm_ptr() };
    let stack = unsafe { &mut *ctx.activation().stack_ptr() };
    // SAFETY: the published frame and activation context are live.
    let Some(owner) = (unsafe {
        ctx.activation()
            .owner_context((*ctx.native_frame).header.function_id)
    }) else {
        return compiled_fatal(ctx, VmError::InvalidOperand);
    };
    let context = &owner;
    let Some(native) = (unsafe { ctx.native_frame.as_mut() }) else {
        return compiled_fatal(ctx, VmError::InvalidOperand);
    };
    match vm.prepare_inline_deopt_frames(
        context,
        stack,
        native,
        &frames,
        SideExit::new(exit.resume_pcs[0], exit.reason, exit.action),
    ) {
        Ok(()) => {
            ctx.pending_call = otter_vm::native_abi::CallRequest::resume_interpreter();
            NativeResultPair::continue_execution()
        }
        Err(error) => compiled_error(ctx, error),
    }
}

/// Decode one code-owned writeback and resume an inline exit after Rust returns.
///
/// # Safety
/// All arguments describe the live generated exit homes and retained code
/// metadata. The complete physical
/// frame and roots stay published until return.
#[cfg(target_arch = "aarch64")]
#[unsafe(naked)]
pub(crate) unsafe extern "C" fn jit_deopt_writeback_entry(
    _ctx: *mut JitCtx,
    _exit_index: u64,
    _runtime: *const otter_vm::deopt::DeoptRuntime,
    _frame_sp: u64,
    _window: u64,
) -> NativeResultPair {
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
        prepare = sym prepare_deopt_writeback,
        trampoline = sym otter_vm::native_abi::call_trampoline,
        continue_status = const NativeResultStatus::Continue as u64,
    );
}

/// Writeback entry with the System V and Windows x86 aggregate-return ABIs.
///
/// # Safety
/// Metadata and publication requirements match the ARM64 entry.
#[cfg(target_arch = "x86_64")]
#[unsafe(naked)]
pub(crate) unsafe extern "C" fn jit_deopt_writeback_entry(
    _ctx: *mut JitCtx,
    _exit_index: u64,
    _runtime: *const otter_vm::deopt::DeoptRuntime,
    _frame_sp: u64,
    _window: u64,
) -> NativeResultPair {
    core::arch::naked_asm!(
        "push rbp",
        "mov rbp, rsp",
        "push rbx",
        ".if {windows}",
        "push r12",
        "sub rsp, 48",
        "mov r12, rcx",
        "mov rbx, rdx",
        "mov rax, [rbp + 48]",
        "mov [rsp + 32], rax",
        "mov rax, [rbp + 56]",
        "mov [rsp + 40], rax",
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
        prepare = sym prepare_deopt_writeback,
        trampoline = sym otter_vm::native_abi::call_trampoline,
        continue_status = const NativeResultStatus::Continue as u64,
    );
}
