//! Tier-independent AArch64 compiled-to-compiled call emission.
//!
//! # Contents
//! - [`CallTrampoline`] — one owned compiled-to-compiled call lifecycle shared
//!   by every native tier.
//! - [`emit_prepared_call`] — emits the small call-site dispatch into that
//!   trampoline.
//!
//! # Invariants
//! - A prepare transition publishes one complete `direct_call` record before
//!   the trampoline is called with `(caller_ctx, destination_register)`.
//! - After compiled callee entry, its frame lives exactly as long as its
//!   machine-stack reservation; the caller's frame is republished before the
//!   returned, bailed, or threw status is dispatched.
//! - The callee's SELF and `this` slots are published in the activation arena
//!   before compiled entry and removed exactly once on every completed entry.
//! - Runtime-stub addresses come from [`TransitionTable`]; the callee machine
//!   entry comes only from an acquired registry-owned `CodeEntryCell`.
//!
//! # See also
//! - [`crate::entry`] — the shared context, frame, and transition ABI.
//! - `crates/otter-vm/src/native_abi/frame.rs` — the published activation.

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};
use otter_vm::native_abi as abi;

use crate::entry::{
    ACTIVATION_BASE_OFFSET, ACTIVATION_LIMIT_OFFSET, ACTIVATION_TOP_PTR_OFFSET,
    CODE_ENTRY_ACTIVE_COUNT_OFFSET, CTX_PLUS_FRAME_STACK_SIZE, DIRECT_CODE_OBJECT_ID_OFFSET,
    DIRECT_ENTRY_CELL_OFFSET, DIRECT_FRAME_IDS_OFFSET, DIRECT_FRAME_INDEX_OFFSET,
    DIRECT_FRAME_META_OFFSET, DIRECT_REGS_OFFSET, DIRECT_SELF_OFFSET, DIRECT_THIS_OFFSET,
    DIRECT_UPVALUES_OFFSET, ERROR_SLOT_OFFSET, FRAME_INDEX_OFFSET, JIT_CTX_STACK_SIZE,
    NATIVE_FRAME_ARGUMENT_BASE_OFFSET, NATIVE_FRAME_CODE_OBJECT_ID_OFFSET,
    NATIVE_FRAME_FEEDBACK_BASE_OFFSET, NATIVE_FRAME_NEW_TARGET_OFFSET, NATIVE_FRAME_OFFSET,
    NATIVE_FRAME_PC_OFFSET, NATIVE_FRAME_PREVIOUS_OFFSET, NATIVE_FRAME_REGISTER_BASE_OFFSET,
    NATIVE_FRAME_RETURN_REGISTER_OFFSET, NATIVE_FRAME_TAIL_OFFSET, NATIVE_FRAME_THIS_OFFSET,
    REG_STACK_BASE_OFFSET, REG_TOP_PTR_OFFSET, STATUS_BAILED, STATUS_RETURNED, THREAD_OFFSET,
    TransitionTable, UPVALUES_PTR_OFFSET, VALUE_UNDEFINED,
};
use crate::{BackendFailure, CompiledCode, Unsupported};

/// Status returned from [`CallTrampoline`] to a compiled caller.
const CALL_DONE: u64 = 0;
const CALL_THREW: u64 = 1;
const CALL_BAILED: u64 = 2;

/// Shared AArch64 compiled-to-compiled call lifecycle.
///
/// The emitted ABI is `extern "C" fn(*mut JitCtx, u64) -> u64`: `x0` carries
/// the caller context, `x1` the dynamic destination register, and `x0` returns
/// `0` for completed, `1` for throw, or `2` for caller side exit. Keeping this
/// lifecycle in its own executable mapping removes a large tier-local body
/// from every call site while preserving the VM-owned prepare/finish split.
pub(crate) struct CallTrampoline {
    code: CompiledCode,
}

impl CallTrampoline {
    /// Assemble and finalize the hook-lifetime trampoline.
    pub(crate) fn compile(table: &TransitionTable) -> Result<Self, Unsupported> {
        let mut ops = Assembler::new()
            .map_err(|_| Unsupported::Backend(BackendFailure::AssemblerAllocation))?;
        let entry = ops.offset();
        emit_call_trampoline(&mut ops, table);
        let buffer = ops
            .finalize()
            .map_err(|_| Unsupported::Backend(BackendFailure::Finalization))?;
        Ok(Self {
            code: CompiledCode::new(buffer, entry),
        })
    }

    /// Address baked into every compiled call site that retains this owner.
    fn entry_addr(&self) -> u64 {
        // SAFETY: callers retain an `Arc<CallTrampoline>` for at least as long
        // as the compiled mapping containing the baked address.
        unsafe { self.code.entry_ptr() as u64 }
    }

    #[cfg(test)]
    fn invoke(&self, ctx: *mut crate::entry::JitCtx, dst: u64) -> u64 {
        type Entry = extern "C" fn(*mut crate::entry::JitCtx, u64) -> u64;
        // SAFETY: `compile` emits exactly `Entry`, and `self` keeps the mapping
        // executable for the full call.
        let entry: Entry = unsafe { std::mem::transmute(self.code.entry_ptr()) };
        entry(ctx, dst)
    }
}

impl std::fmt::Debug for CallTrampoline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CallTrampoline")
            .field("code_len", &self.code.len())
            .finish()
    }
}

/// Materialize a 64-bit constant into x-register `t` via `movz`/`movk`.
fn emit_load_u64(ops: &mut Assembler, t: u8, v: u64) {
    dynasm!(ops ; .arch aarch64 ; movz X(t), (v & 0xFFFF) as u32);
    if (v >> 16) & 0xFFFF != 0 {
        dynasm!(ops ; .arch aarch64 ; movk X(t), ((v >> 16) & 0xFFFF) as u32, lsl #16);
    }
    if (v >> 32) & 0xFFFF != 0 {
        dynasm!(ops ; .arch aarch64 ; movk X(t), ((v >> 32) & 0xFFFF) as u32, lsl #32);
    }
    if (v >> 48) & 0xFFFF != 0 {
        dynasm!(ops ; .arch aarch64 ; movk X(t), ((v >> 48) & 0xFFFF) as u32, lsl #48);
    }
}

/// Emit the small call-site bridge after a prepare transition returned status
/// `0`. The call site's owner must retain the same [`CallTrampoline`] for the
/// complete lifetime of this baked address.
pub(crate) fn emit_prepared_call(
    ops: &mut Assembler,
    trampoline: &CallTrampoline,
    dst: u16,
    bail: DynamicLabel,
    threw: DynamicLabel,
    done: DynamicLabel,
) {
    emit_load_u64(ops, 16, trampoline.entry_addr());
    dynasm!(ops
        ; .arch aarch64
        ; mov x0, x20
        ; movz x1, dst as u32
        ; blr x16
        ; cmp x0, CALL_THREW as u32
        ; b.eq =>threw
        ; cmp x0, CALL_BAILED as u32
        ; b.eq =>bail
        ; b =>done
    );
}

/// Emit the single callable trampoline body.
///
/// It builds the callee `JitCtx` and published `NativeFrame`, enters the
/// prepared code, and owns every finish/abort path. The caller context is kept
/// in callee-saved `x20`; the dynamic destination register is kept in `x19`;
/// `x21`/`x22` retain the acquired entry cell and exact generation address.
fn emit_call_trampoline(ops: &mut Assembler, table: &TransitionTable) {
    let entry_acquire_retry = ops.new_dynamic_label();
    let entry_acquire_saturated = ops.new_dynamic_label();
    let entry_acquire_rollback = ops.new_dynamic_label();
    let entry_rollback_retry = ops.new_dynamic_label();
    let entry_rejected = ops.new_dynamic_label();
    let entry_release_after_call = ops.new_dynamic_label();
    let entry_release_after_push_failure = ops.new_dynamic_label();
    let direct_returned = ops.new_dynamic_label();
    let direct_bailed = ops.new_dynamic_label();
    let direct_threw = ops.new_dynamic_label();
    let push_slow = ops.new_dynamic_label();
    let push_failed = ops.new_dynamic_label();
    let push_done = ops.new_dynamic_label();
    let direct_done = ops.new_dynamic_label();
    let direct_finish_threw = ops.new_dynamic_label();
    let direct_finish_bailed = ops.new_dynamic_label();
    let direct_exit = ops.new_dynamic_label();
    let push_activation = table.entry(abi::STUB_JIT_PUSH_NATIVE_ACTIVATION);
    let abort_direct_call = table.entry(abi::STUB_JIT_ABORT_DIRECT_CALL);
    dynasm!(ops
        ; .arch aarch64
        ; stp x29, x30, [sp, #-48]!
        ; stp x19, x20, [sp, #16]
        ; stp x21, x22, [sp, #32]
        ; mov x29, sp
        ; mov x20, x0
        ; mov x19, x1
        // Acquire the exact registry-owned code generation before constructing
        // native activation state. x21 retains the never-reused cell; x22
        // retains the confirmed entry while invalidation is allowed to unlink
        // the cell concurrently. active_count prevents mapping retirement.
        ; ldr x21, [x20, DIRECT_ENTRY_CELL_OFFSET]
        ; cbz x21, =>entry_rejected
        ; ldar x22, [x21]
        ; cbz x22, =>entry_rejected
        ; add x15, x21, CODE_ENTRY_ACTIVE_COUNT_OFFSET
        ; =>entry_acquire_retry
        ; ldaxr w9, [x15]
        ; cmn w9, #1
        ; b.eq =>entry_acquire_saturated
        ; add w10, w9, #1
        ; stlxr w11, w10, [x15]
        ; cbnz w11, =>entry_acquire_retry
        // Recheck after publication. Cells are never relinked, so a changed
        // nonzero address is treated as rejection just like an unlink.
        ; ldar x9, [x21]
        ; cbz x9, =>entry_acquire_rollback
        ; cmp x9, x22
        ; b.ne =>entry_acquire_rollback
        ; sub sp, sp, CTX_PLUS_FRAME_STACK_SIZE
        ; ldr x9, [x20, DIRECT_REGS_OFFSET]
        ; str x9, [sp]
        ; ldr x9, [x20, DIRECT_SELF_OFFSET]
        ; str x9, [sp, #8]
        ; ldr x9, [x20, DIRECT_THIS_OFFSET]
        ; str x9, [sp, #16]
        ; ldr x9, [x20, THREAD_OFFSET]
        ; str x9, [sp, THREAD_OFFSET]
        ; ldr x10, [x20, NATIVE_FRAME_OFFSET]
        ; add x15, sp, JIT_CTX_STACK_SIZE
        ; ldr x9, [x20, DIRECT_FRAME_IDS_OFFSET]
        ; str x9, [x15]
        ; ldr x9, [x20, DIRECT_FRAME_META_OFFSET]
        ; str x9, [x15, #8]
        ; str x10, [x15, NATIVE_FRAME_PREVIOUS_OFFSET]
        ; ldr x9, [x20, DIRECT_REGS_OFFSET]
        ; str x9, [x15, NATIVE_FRAME_REGISTER_BASE_OFFSET]
        ; str xzr, [x15, NATIVE_FRAME_ARGUMENT_BASE_OFFSET]
        ; str xzr, [x15, NATIVE_FRAME_FEEDBACK_BASE_OFFSET]
        ; ldr x9, [x20, DIRECT_CODE_OBJECT_ID_OFFSET]
        ; str x9, [x15, NATIVE_FRAME_CODE_OBJECT_ID_OFFSET]
        ; ldr x9, [x20, DIRECT_THIS_OFFSET]
        ; str x9, [x15, NATIVE_FRAME_THIS_OFFSET]
    );
    emit_load_u64(ops, 9, VALUE_UNDEFINED);
    dynasm!(ops
        ; .arch aarch64
        ; str x9, [x15, NATIVE_FRAME_NEW_TARGET_OFFSET]
        ; movn x9, #0
        ; str x9, [x15, NATIVE_FRAME_RETURN_REGISTER_OFFSET]
        ; str xzr, [x15, NATIVE_FRAME_TAIL_OFFSET]
        ; str x15, [sp, NATIVE_FRAME_OFFSET]
        ; ldr x9, [x20, THREAD_OFFSET]
        ; str x15, [x9]
        ; ldr x9, [x20, DIRECT_FRAME_INDEX_OFFSET]
        ; str x9, [sp, FRAME_INDEX_OFFSET]
        ; ldr x9, [x20, ERROR_SLOT_OFFSET]
        ; str x9, [sp, ERROR_SLOT_OFFSET]
        ; ldr x9, [x20, DIRECT_UPVALUES_OFFSET]
        ; str x9, [sp, UPVALUES_PTR_OFFSET]
        ; ldr x9, [x20, REG_STACK_BASE_OFFSET]
        ; str x9, [sp, REG_STACK_BASE_OFFSET]
        ; ldr x9, [x20, REG_TOP_PTR_OFFSET]
        ; str x9, [sp, REG_TOP_PTR_OFFSET]
        ; ldr x9, [x20, ACTIVATION_BASE_OFFSET]
        ; str x9, [sp, ACTIVATION_BASE_OFFSET]
        ; ldr x9, [x20, ACTIVATION_TOP_PTR_OFFSET]
        ; str x9, [sp, ACTIVATION_TOP_PTR_OFFSET]
        ; ldr x9, [x20, ACTIVATION_LIMIT_OFFSET]
        ; str x9, [sp, ACTIVATION_LIMIT_OFFSET]
        // Publish the callee's SELF/`this` GC slots inline: bump the
        // activation cursor and record the two slot addresses. Overflow takes
        // the stub slow path, which parks the stack-overflow error.
        ; ldr x9, [x20, ACTIVATION_TOP_PTR_OFFSET]
        ; ldr x10, [x9]
        ; ldr x11, [x20, ACTIVATION_LIMIT_OFFSET]
        ; cmp x10, x11
        ; b.hs =>push_slow
        ; ldr x11, [x20, ACTIVATION_BASE_OFFSET]
        ; add x12, x11, x10, lsl #4
        ; add x13, sp, #8
        ; str x13, [x12]
        ; add x13, sp, #16
        ; str x13, [x12, #8]
        ; add x10, x10, #1
        ; str x10, [x9]
        ; =>push_done
        ; mov x0, sp
        ; blr x22
        // The callee has returned. Unpublish its native frame before releasing
        // the entry lease so retirement can never observe a published frame
        // whose code-object metadata is no longer registry-owned.
        ; ldr x10, [x20, NATIVE_FRAME_OFFSET]
        ; ldr x9, [x20, THREAD_OFFSET]
        ; str x10, [x9]
        ; add x15, x21, CODE_ENTRY_ACTIVE_COUNT_OFFSET
        ; =>entry_release_after_call
        ; ldaxr w9, [x15]
        ; sub w10, w9, #1
        ; stlxr w11, w10, [x15]
        ; cbnz w11, =>entry_release_after_call
        ; cmp x1, STATUS_RETURNED as u32
        ; b.eq =>direct_returned
        ; cmp x1, STATUS_BAILED as u32
        ; b.eq =>direct_bailed
        ; b =>direct_threw
        ; =>direct_returned
        ; mov x3, x0
        ; ldr x9, [x20, ACTIVATION_TOP_PTR_OFFSET]
        ; ldr x10, [x9]
        ; sub x10, x10, #1
        ; str x10, [x9]
        ; ldr x11, [x20, ACTIVATION_BASE_OFFSET]
        ; add x12, x11, x10, lsl #4
        ; str xzr, [x12]
        ; str xzr, [x12, #8]
        ; ldr x2, [x20, DIRECT_FRAME_INDEX_OFFSET]
        ; add sp, sp, CTX_PLUS_FRAME_STACK_SIZE
        ; mov x0, x20
        ; mov x1, x19
    );
    emit_load_u64(
        ops,
        16,
        table.entry(abi::STUB_JIT_FINISH_DIRECT_CALL_RETURNED),
    );
    dynasm!(ops
        ; .arch aarch64
        ; blr x16
        ; cbnz x0, =>direct_finish_threw
        ; b =>direct_done
        ; =>direct_bailed
        // The callee stamped its exact bail PC into its own published frame.
        ; add x9, sp, JIT_CTX_STACK_SIZE
        ; ldr w3, [x9, NATIVE_FRAME_PC_OFFSET]
        ; ldr x9, [x20, ACTIVATION_TOP_PTR_OFFSET]
        ; ldr x10, [x9]
        ; sub x10, x10, #1
        ; str x10, [x9]
        ; ldr x11, [x20, ACTIVATION_BASE_OFFSET]
        ; add x12, x11, x10, lsl #4
        ; str xzr, [x12]
        ; str xzr, [x12, #8]
        ; ldr x2, [x20, DIRECT_FRAME_INDEX_OFFSET]
        ; add sp, sp, CTX_PLUS_FRAME_STACK_SIZE
        ; mov x0, x20
        ; mov x1, x19
    );
    emit_load_u64(
        ops,
        16,
        table.entry(abi::STUB_JIT_FINISH_DIRECT_CALL_BAILED),
    );
    dynasm!(ops
        ; .arch aarch64
        ; blr x16
        ; cbnz x0, =>direct_finish_threw
        ; b =>direct_done
        ; =>direct_threw
        ; ldr x9, [x20, ACTIVATION_TOP_PTR_OFFSET]
        ; ldr x10, [x9]
        ; sub x10, x10, #1
        ; str x10, [x9]
        ; ldr x11, [x20, ACTIVATION_BASE_OFFSET]
        ; add x12, x11, x10, lsl #4
        ; str xzr, [x12]
        ; str xzr, [x12, #8]
        ; ldr x1, [x20, DIRECT_FRAME_INDEX_OFFSET]
        ; add sp, sp, CTX_PLUS_FRAME_STACK_SIZE
        ; mov x0, x20
    );
    emit_load_u64(ops, 16, abort_direct_call);
    dynasm!(ops
        ; .arch aarch64
        ; blr x16
        ; cmp x0, #2
        ; b.eq =>direct_finish_bailed
        ; b =>direct_finish_threw
        // Out-of-line activation-publish overflow: the stub re-checks, parks
        // the stack-overflow error, and reports it.
        ; =>push_slow
        ; mov x0, sp
    );
    emit_load_u64(ops, 16, push_activation);
    dynasm!(ops
        ; .arch aarch64
        ; blr x16
        ; cbnz x0, =>push_failed
        ; b =>push_done
        ; =>push_failed
        // Activation publication failed before the cursor advanced. Restore
        // the caller machine state, then abort the VM-owned prepared frame and
        // its sync-reentry guard without attempting an activation pop.
        ; ldr x10, [x20, NATIVE_FRAME_OFFSET]
        ; ldr x9, [x20, THREAD_OFFSET]
        ; str x10, [x9]
        ; add x15, x21, CODE_ENTRY_ACTIVE_COUNT_OFFSET
        ; =>entry_release_after_push_failure
        ; ldaxr w9, [x15]
        ; sub w10, w9, #1
        ; stlxr w11, w10, [x15]
        ; cbnz w11, =>entry_release_after_push_failure
        ; ldr x1, [x20, DIRECT_FRAME_INDEX_OFFSET]
        ; add sp, sp, CTX_PLUS_FRAME_STACK_SIZE
        ; mov x0, x20
    );
    emit_load_u64(ops, 16, abort_direct_call);
    dynasm!(ops
        ; .arch aarch64
        ; blr x16
        ; cmp x0, #2
        ; b.eq =>direct_finish_bailed
        ; b =>direct_finish_threw
        // Saturation leaves an outstanding exclusive monitor but does not own
        // a lease. Clear it before running the VM abort path.
        ; =>entry_acquire_saturated
        ; clrex
        ; b =>entry_rejected
        // Invalidation won the acquire race. Drop the provisional count, then
        // abort the already-published VM frame and side-exit the caller.
        ; =>entry_acquire_rollback
        ; add x15, x21, CODE_ENTRY_ACTIVE_COUNT_OFFSET
        ; =>entry_rollback_retry
        ; ldaxr w9, [x15]
        ; sub w10, w9, #1
        ; stlxr w11, w10, [x15]
        ; cbnz w11, =>entry_rollback_retry
        ; =>entry_rejected
        ; ldr x1, [x20, DIRECT_FRAME_INDEX_OFFSET]
        ; mov x0, x20
    );
    emit_load_u64(ops, 16, abort_direct_call);
    dynasm!(ops
        ; .arch aarch64
        ; blr x16
        ; cmp x0, #1
        ; b.eq =>direct_finish_threw
        ; b =>direct_finish_bailed
        ; =>direct_done
        ; movz x0, CALL_DONE as u32
        ; b =>direct_exit
        ; =>direct_finish_threw
        ; movz x0, CALL_THREW as u32
        ; b =>direct_exit
        ; =>direct_finish_bailed
        ; movz x0, CALL_BAILED as u32
        ; =>direct_exit
        ; ldp x21, x22, [sp, #32]
        ; ldp x19, x20, [sp, #16]
        ; ldp x29, x30, [sp], #48
        ; ret
    );
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

    use otter_vm::{
        Value, VmError,
        jit::JitPreparedDirectCall,
        native_abi::{
            CodeEntryCell, NativeFrame, NativeFrameFlags, NativeFrameKind, VmFrameHeader, VmThread,
        },
    };

    use super::*;
    use crate::entry::{JitCtx, JitRet, STATUS_BAILED, STATUS_RETURNED, STATUS_THREW};

    static PUSH_SAW_CALLEE_FRAME: AtomicBool = AtomicBool::new(false);
    static ABORT_SAW_CALLER_FRAME: AtomicBool = AtomicBool::new(false);
    static ABORT_FRAME_INDEX: AtomicUsize = AtomicUsize::new(usize::MAX);
    static RETURN_CALLEE_SAW_PUBLISHED_FRAME: AtomicBool = AtomicBool::new(false);
    static RETURN_FINISH_SAW_CALLER_FRAME: AtomicBool = AtomicBool::new(false);
    static RETURN_FINISH_DST: AtomicU64 = AtomicU64::new(u64::MAX);
    static RETURN_FINISH_FRAME: AtomicU64 = AtomicU64::new(u64::MAX);
    static RETURN_FINISH_VALUE: AtomicU64 = AtomicU64::new(0);
    static BAIL_CALLEE_SAW_PUBLISHED_FRAME: AtomicBool = AtomicBool::new(false);
    static BAIL_FINISH_SAW_CALLER_FRAME: AtomicBool = AtomicBool::new(false);
    static BAIL_FINISH_DST: AtomicU64 = AtomicU64::new(u64::MAX);
    static BAIL_FINISH_FRAME: AtomicU64 = AtomicU64::new(u64::MAX);
    static BAIL_FINISH_PC: AtomicU64 = AtomicU64::new(u64::MAX);
    static THROW_CALLEE_SAW_PUBLISHED_FRAME: AtomicBool = AtomicBool::new(false);
    static THROW_ABORT_SAW_CALLER_FRAME: AtomicBool = AtomicBool::new(false);
    static THROW_ABORT_FRAME: AtomicU64 = AtomicU64::new(u64::MAX);
    static UNLINKED_CALLEE_ENTERED: AtomicBool = AtomicBool::new(false);
    static SATURATED_CALLEE_ENTERED: AtomicBool = AtomicBool::new(false);

    const RETURN_VALUE: u64 = 0xfffe_0000_0000_002a;
    const BAIL_PC: u32 = 73;

    extern "C" fn fail_activation_push(ctx: *mut JitCtx) -> u64 {
        // SAFETY: the generated tail has initialized every required nested
        // context field; `direct_call` is explicitly `MaybeUninit`.
        let ctx = unsafe { &mut *ctx };
        // SAFETY: the fixture supplies a live thread for the emitted call.
        let thread = unsafe { &*ctx.thread };
        PUSH_SAW_CALLEE_FRAME.store(
            thread.current_frame == ctx.native_frame as u64,
            Ordering::SeqCst,
        );
        // SAFETY: nested contexts share the caller's initialized error slot.
        unsafe {
            *ctx.error = Some(VmError::StackOverflow { limit: 1 });
        }
        1
    }

    extern "C" fn observe_abort(ctx: *mut JitCtx, callee_frame_index: u64) -> u64 {
        // SAFETY: the failure path must pass the live outer context after
        // restoring its stack reservation.
        let ctx = unsafe { &mut *ctx };
        // SAFETY: the fixture supplies a live thread for the emitted call.
        let thread = unsafe { &*ctx.thread };
        ABORT_SAW_CALLER_FRAME.store(
            thread.current_frame == ctx.native_frame as u64,
            Ordering::SeqCst,
        );
        ABORT_FRAME_INDEX.store(callee_frame_index as usize, Ordering::SeqCst);
        0
    }

    extern "C" fn returning_callee(ctx: *mut JitCtx) -> JitRet {
        // SAFETY: the trampoline passes its fully initialized nested context.
        let ctx = unsafe { &mut *ctx };
        // SAFETY: the nested context shares the fixture's live thread.
        let thread = unsafe { &*ctx.thread };
        RETURN_CALLEE_SAW_PUBLISHED_FRAME.store(
            thread.current_frame == ctx.native_frame as u64,
            Ordering::SeqCst,
        );
        JitRet {
            value: RETURN_VALUE,
            status: STATUS_RETURNED,
        }
    }

    extern "C" fn finish_returned(
        ctx: *mut JitCtx,
        dst: u64,
        callee_frame_index: u64,
        value: u64,
    ) -> u64 {
        // SAFETY: the trampoline has restored the live caller context.
        let ctx = unsafe { &mut *ctx };
        // SAFETY: the fixture supplies a live thread.
        let thread = unsafe { &*ctx.thread };
        RETURN_FINISH_SAW_CALLER_FRAME.store(
            thread.current_frame == ctx.native_frame as u64,
            Ordering::SeqCst,
        );
        RETURN_FINISH_DST.store(dst, Ordering::SeqCst);
        RETURN_FINISH_FRAME.store(callee_frame_index, Ordering::SeqCst);
        RETURN_FINISH_VALUE.store(value, Ordering::SeqCst);
        0
    }

    extern "C" fn bailing_callee(ctx: *mut JitCtx) -> JitRet {
        // SAFETY: the trampoline passes its fully initialized nested context.
        let ctx = unsafe { &mut *ctx };
        // SAFETY: the nested context shares the fixture's live thread/frame.
        let thread = unsafe { &*ctx.thread };
        BAIL_CALLEE_SAW_PUBLISHED_FRAME.store(
            thread.current_frame == ctx.native_frame as u64,
            Ordering::SeqCst,
        );
        unsafe {
            (*ctx.native_frame).header.pc = BAIL_PC;
        }
        JitRet {
            value: 0,
            status: STATUS_BAILED,
        }
    }

    extern "C" fn finish_bailed(
        ctx: *mut JitCtx,
        dst: u64,
        callee_frame_index: u64,
        resume_pc: u64,
    ) -> u64 {
        // SAFETY: the trampoline has restored the live caller context.
        let ctx = unsafe { &mut *ctx };
        // SAFETY: the fixture supplies a live thread.
        let thread = unsafe { &*ctx.thread };
        BAIL_FINISH_SAW_CALLER_FRAME.store(
            thread.current_frame == ctx.native_frame as u64,
            Ordering::SeqCst,
        );
        BAIL_FINISH_DST.store(dst, Ordering::SeqCst);
        BAIL_FINISH_FRAME.store(callee_frame_index, Ordering::SeqCst);
        BAIL_FINISH_PC.store(resume_pc, Ordering::SeqCst);
        0
    }

    extern "C" fn throwing_callee(ctx: *mut JitCtx) -> JitRet {
        // SAFETY: the trampoline passes its fully initialized nested context.
        let ctx = unsafe { &mut *ctx };
        // SAFETY: the nested context shares the fixture's live thread/error.
        let thread = unsafe { &*ctx.thread };
        THROW_CALLEE_SAW_PUBLISHED_FRAME.store(
            thread.current_frame == ctx.native_frame as u64,
            Ordering::SeqCst,
        );
        unsafe {
            *ctx.error = Some(VmError::InvalidOperand);
        }
        JitRet {
            value: 0,
            status: STATUS_THREW,
        }
    }

    extern "C" fn unlinked_probe_callee(_ctx: *mut JitCtx) -> JitRet {
        UNLINKED_CALLEE_ENTERED.store(true, Ordering::SeqCst);
        JitRet {
            value: RETURN_VALUE,
            status: STATUS_RETURNED,
        }
    }

    extern "C" fn saturated_probe_callee(_ctx: *mut JitCtx) -> JitRet {
        SATURATED_CALLEE_ENTERED.store(true, Ordering::SeqCst);
        JitRet {
            value: RETURN_VALUE,
            status: STATUS_RETURNED,
        }
    }

    extern "C" fn abort_thrown_callee(ctx: *mut JitCtx, callee_frame_index: u64) -> u64 {
        // SAFETY: the trampoline restores the caller before aborting.
        let ctx = unsafe { &mut *ctx };
        // SAFETY: the fixture supplies a live thread.
        let thread = unsafe { &*ctx.thread };
        THROW_ABORT_SAW_CALLER_FRAME.store(
            thread.current_frame == ctx.native_frame as u64,
            Ordering::SeqCst,
        );
        THROW_ABORT_FRAME.store(callee_frame_index, Ordering::SeqCst);
        0
    }

    struct FixtureOutcome {
        status: u64,
        entry_active_count: u32,
        activation_top: usize,
        activation_slots: [u64; 2],
        caller_frame_restored: bool,
        error: Option<VmError>,
    }

    fn invoke_prepared(
        transitions: &TransitionTable,
        callee_entry: extern "C" fn(*mut JitCtx) -> JitRet,
        dst: u64,
    ) -> FixtureOutcome {
        let entry_cell =
            CodeEntryCell::new(callee_entry as *const () as usize, 17, 9, 0, 1, 0, 0, 0);
        invoke_prepared_cell(transitions, &entry_cell, dst)
    }

    fn invoke_prepared_cell(
        transitions: &TransitionTable,
        entry_cell: &CodeEntryCell,
        dst: u64,
    ) -> FixtureOutcome {
        let trampoline = CallTrampoline::compile(transitions).expect("call trampoline");
        let mut regs = [Value::undefined().to_bits()];
        let mut caller_frame = native_frame(regs.as_mut_ptr());
        let caller_frame_addr = std::ptr::addr_of_mut!(caller_frame) as u64;
        let mut thread = VmThread::empty();
        thread.current_frame = caller_frame_addr;
        let mut error = None;
        let mut activation_slots = [0u64; 2];
        let mut activation_top = 0usize;
        let mut ctx = JitCtx {
            regs: regs.as_mut_ptr(),
            self_closure: Value::undefined().to_bits(),
            this_value: Value::undefined().to_bits(),
            thread: std::ptr::addr_of_mut!(thread),
            native_frame: std::ptr::addr_of_mut!(caller_frame),
            frame_index: 3,
            upvalues_ptr: 0,
            error: std::ptr::addr_of_mut!(error),
            direct_call: std::mem::MaybeUninit::new(JitPreparedDirectCall {
                entry_cell: std::ptr::from_ref(entry_cell) as u64,
                regs: regs.as_mut_ptr(),
                self_closure: Value::undefined().to_bits(),
                this_value: Value::undefined().to_bits(),
                frame_index: 41,
                upvalues_ptr: 0,
                frame_ids: 9 | (9_u64 << 32),
                frame_meta: 1_u64 << 32,
                code_object_id: 17,
            }),
            reg_stack_base: std::ptr::null_mut(),
            reg_top_ptr: std::ptr::null_mut(),
            activation_base: activation_slots.as_mut_ptr().cast(),
            activation_top_ptr: std::ptr::addr_of_mut!(activation_top),
            activation_limit: 1,
        };

        let status = trampoline.invoke(std::ptr::addr_of_mut!(ctx), dst);
        FixtureOutcome {
            status,
            entry_active_count: entry_cell.active_count(),
            activation_top,
            activation_slots,
            caller_frame_restored: thread.current_frame == caller_frame_addr,
            error,
        }
    }

    fn native_frame(regs: *mut u64) -> NativeFrame {
        NativeFrame {
            header: VmFrameHeader {
                function_id: 7,
                code_block_id: 7,
                pc: 0,
                register_count: 1,
                kind: NativeFrameKind::Baseline,
                flags: NativeFrameFlags::empty(),
            },
            previous_frame: 0,
            register_base: regs as u64,
            argument_base: 0,
            feedback_base: 0,
            code_object_id: 11,
            this_value_bits: Value::undefined().to_bits(),
            new_target_bits: Value::undefined().to_bits(),
            return_register: u32::MAX,
            cold_state_index: u32::MAX,
            argument_count: 0,
            reserved0: 0,
            feedback_id: 0,
        }
    }

    #[test]
    fn returned_callee_finishes_dynamic_destination_and_releases_activation() {
        RETURN_CALLEE_SAW_PUBLISHED_FRAME.store(false, Ordering::SeqCst);
        RETURN_FINISH_SAW_CALLER_FRAME.store(false, Ordering::SeqCst);
        RETURN_FINISH_DST.store(u64::MAX, Ordering::SeqCst);
        RETURN_FINISH_FRAME.store(u64::MAX, Ordering::SeqCst);
        RETURN_FINISH_VALUE.store(0, Ordering::SeqCst);

        let mut transitions = TransitionTable::resolve();
        transitions.replace_entry_for_test(
            abi::STUB_JIT_FINISH_DIRECT_CALL_RETURNED,
            finish_returned as *const () as usize,
        );
        let outcome = invoke_prepared(&transitions, returning_callee, 29);

        assert_eq!(outcome.status, CALL_DONE);
        assert_eq!(outcome.entry_active_count, 0);
        assert!(outcome.caller_frame_restored);
        assert_eq!(outcome.activation_top, 0);
        assert_eq!(outcome.activation_slots, [0, 0]);
        assert!(outcome.error.is_none());
        assert!(RETURN_CALLEE_SAW_PUBLISHED_FRAME.load(Ordering::SeqCst));
        assert!(RETURN_FINISH_SAW_CALLER_FRAME.load(Ordering::SeqCst));
        assert_eq!(RETURN_FINISH_DST.load(Ordering::SeqCst), 29);
        assert_eq!(RETURN_FINISH_FRAME.load(Ordering::SeqCst), 41);
        assert_eq!(RETURN_FINISH_VALUE.load(Ordering::SeqCst), RETURN_VALUE);
    }

    #[test]
    fn bailed_callee_forwards_exact_pc_and_releases_activation() {
        BAIL_CALLEE_SAW_PUBLISHED_FRAME.store(false, Ordering::SeqCst);
        BAIL_FINISH_SAW_CALLER_FRAME.store(false, Ordering::SeqCst);
        BAIL_FINISH_DST.store(u64::MAX, Ordering::SeqCst);
        BAIL_FINISH_FRAME.store(u64::MAX, Ordering::SeqCst);
        BAIL_FINISH_PC.store(u64::MAX, Ordering::SeqCst);

        let mut transitions = TransitionTable::resolve();
        transitions.replace_entry_for_test(
            abi::STUB_JIT_FINISH_DIRECT_CALL_BAILED,
            finish_bailed as *const () as usize,
        );
        let outcome = invoke_prepared(&transitions, bailing_callee, 31);

        assert_eq!(outcome.status, CALL_DONE);
        assert_eq!(outcome.entry_active_count, 0);
        assert!(outcome.caller_frame_restored);
        assert_eq!(outcome.activation_top, 0);
        assert_eq!(outcome.activation_slots, [0, 0]);
        assert!(outcome.error.is_none());
        assert!(BAIL_CALLEE_SAW_PUBLISHED_FRAME.load(Ordering::SeqCst));
        assert!(BAIL_FINISH_SAW_CALLER_FRAME.load(Ordering::SeqCst));
        assert_eq!(BAIL_FINISH_DST.load(Ordering::SeqCst), 31);
        assert_eq!(BAIL_FINISH_FRAME.load(Ordering::SeqCst), 41);
        assert_eq!(BAIL_FINISH_PC.load(Ordering::SeqCst), u64::from(BAIL_PC));
    }

    #[test]
    fn thrown_callee_restores_caller_then_aborts_once() {
        THROW_CALLEE_SAW_PUBLISHED_FRAME.store(false, Ordering::SeqCst);
        THROW_ABORT_SAW_CALLER_FRAME.store(false, Ordering::SeqCst);
        THROW_ABORT_FRAME.store(u64::MAX, Ordering::SeqCst);

        let mut transitions = TransitionTable::resolve();
        transitions.replace_entry_for_test(
            abi::STUB_JIT_ABORT_DIRECT_CALL,
            abort_thrown_callee as *const () as usize,
        );
        let outcome = invoke_prepared(&transitions, throwing_callee, 37);

        assert_eq!(outcome.status, CALL_THREW);
        assert_eq!(outcome.entry_active_count, 0);
        assert!(outcome.caller_frame_restored);
        assert_eq!(outcome.activation_top, 0);
        assert_eq!(outcome.activation_slots, [0, 0]);
        assert!(matches!(outcome.error, Some(VmError::InvalidOperand)));
        assert!(THROW_CALLEE_SAW_PUBLISHED_FRAME.load(Ordering::SeqCst));
        assert!(THROW_ABORT_SAW_CALLER_FRAME.load(Ordering::SeqCst));
        assert_eq!(THROW_ABORT_FRAME.load(Ordering::SeqCst), 41);
    }

    #[test]
    fn failed_activation_push_restores_caller_and_aborts_prepared_frame() {
        PUSH_SAW_CALLEE_FRAME.store(false, Ordering::SeqCst);
        ABORT_SAW_CALLER_FRAME.store(false, Ordering::SeqCst);
        ABORT_FRAME_INDEX.store(usize::MAX, Ordering::SeqCst);

        let mut transitions = TransitionTable::resolve();
        transitions.replace_entry_for_test(
            abi::STUB_JIT_PUSH_NATIVE_ACTIVATION,
            fail_activation_push as *const () as usize,
        );
        transitions.replace_entry_for_test(
            abi::STUB_JIT_ABORT_DIRECT_CALL,
            observe_abort as *const () as usize,
        );

        let trampoline = CallTrampoline::compile(&transitions).expect("call trampoline");
        let entry_cell =
            CodeEntryCell::new(returning_callee as *const () as usize, 17, 9, 0, 1, 0, 0, 0);

        let mut regs = [Value::undefined().to_bits()];
        let mut caller_frame = native_frame(regs.as_mut_ptr());
        let mut thread = VmThread::empty();
        thread.current_frame = std::ptr::addr_of_mut!(caller_frame) as u64;
        let mut error = None;
        let mut activation_top = 1usize;
        let mut ctx = JitCtx {
            regs: regs.as_mut_ptr(),
            self_closure: Value::undefined().to_bits(),
            this_value: Value::undefined().to_bits(),
            thread: std::ptr::addr_of_mut!(thread),
            native_frame: std::ptr::addr_of_mut!(caller_frame),
            frame_index: 3,
            upvalues_ptr: 0,
            error: std::ptr::addr_of_mut!(error),
            direct_call: std::mem::MaybeUninit::new(JitPreparedDirectCall {
                entry_cell: std::ptr::addr_of!(entry_cell) as u64,
                regs: regs.as_mut_ptr(),
                self_closure: Value::undefined().to_bits(),
                this_value: Value::undefined().to_bits(),
                frame_index: 41,
                upvalues_ptr: 0,
                frame_ids: 9 | (9_u64 << 32),
                frame_meta: 1_u64 << 32,
                code_object_id: 17,
            }),
            reg_stack_base: std::ptr::null_mut(),
            reg_top_ptr: std::ptr::null_mut(),
            activation_base: std::ptr::null_mut(),
            activation_top_ptr: std::ptr::addr_of_mut!(activation_top),
            activation_limit: 1,
        };

        let status = trampoline.invoke(std::ptr::addr_of_mut!(ctx), 0);

        assert_eq!(status, CALL_THREW);
        assert!(PUSH_SAW_CALLEE_FRAME.load(Ordering::SeqCst));
        assert!(ABORT_SAW_CALLER_FRAME.load(Ordering::SeqCst));
        assert_eq!(ABORT_FRAME_INDEX.load(Ordering::SeqCst), 41);
        assert_eq!(
            thread.current_frame,
            std::ptr::addr_of!(caller_frame) as u64
        );
        assert_eq!(activation_top, 1, "failed push must not pop activation");
        assert_eq!(entry_cell.active_count(), 0);
        assert!(matches!(error, Some(VmError::StackOverflow { limit: 1 })));
    }

    #[test]
    fn unlinked_entry_cell_aborts_prepared_frame_without_native_entry() {
        ABORT_SAW_CALLER_FRAME.store(false, Ordering::SeqCst);
        ABORT_FRAME_INDEX.store(usize::MAX, Ordering::SeqCst);
        UNLINKED_CALLEE_ENTERED.store(false, Ordering::SeqCst);

        let mut transitions = TransitionTable::resolve();
        transitions.replace_entry_for_test(
            abi::STUB_JIT_ABORT_DIRECT_CALL,
            observe_abort as *const () as usize,
        );
        let entry_cell = CodeEntryCell::new(
            unlinked_probe_callee as *const () as usize,
            17,
            9,
            0,
            1,
            0,
            0,
            0,
        );
        assert!(entry_cell.unlink().is_some());

        let outcome = invoke_prepared_cell(&transitions, &entry_cell, 0);

        assert_eq!(outcome.status, CALL_BAILED);
        assert_eq!(outcome.entry_active_count, 0);
        assert!(outcome.caller_frame_restored);
        assert_eq!(outcome.activation_top, 0);
        assert!(!UNLINKED_CALLEE_ENTERED.load(Ordering::SeqCst));
        assert!(ABORT_SAW_CALLER_FRAME.load(Ordering::SeqCst));
        assert_eq!(ABORT_FRAME_INDEX.load(Ordering::SeqCst), 41);
    }

    #[test]
    fn saturated_entry_cell_rejects_without_wrapping_activation_count() {
        ABORT_SAW_CALLER_FRAME.store(false, Ordering::SeqCst);
        ABORT_FRAME_INDEX.store(usize::MAX, Ordering::SeqCst);
        SATURATED_CALLEE_ENTERED.store(false, Ordering::SeqCst);

        let mut transitions = TransitionTable::resolve();
        transitions.replace_entry_for_test(
            abi::STUB_JIT_ABORT_DIRECT_CALL,
            observe_abort as *const () as usize,
        );
        let entry_cell = CodeEntryCell::new(
            saturated_probe_callee as *const () as usize,
            17,
            9,
            0,
            1,
            0,
            0,
            0,
        );
        entry_cell.active_count.store(u32::MAX, Ordering::Release);

        let outcome = invoke_prepared_cell(&transitions, &entry_cell, 0);

        assert_eq!(outcome.status, CALL_BAILED);
        assert_eq!(outcome.entry_active_count, u32::MAX);
        assert!(outcome.caller_frame_restored);
        assert_eq!(outcome.activation_top, 0);
        assert!(!SATURATED_CALLEE_ENTERED.load(Ordering::SeqCst));
        assert!(ABORT_SAW_CALLER_FRAME.load(Ordering::SeqCst));
        assert_eq!(ABORT_FRAME_INDEX.load(Ordering::SeqCst), 41);
    }
}
