//! VM re-entry and compiled-call lifecycle stubs.
//!
//! # Contents
//! - Native activation publication and release.
//! - Direct-call preparation, completion, and bailout recovery.
//! - Reentrant equality, numeric-family, and unary-coercion completion.
//! - Cooperative backedge polling.
//!
//! # Invariants
//! Every entry receives a live JIT context whose frame/register roots are
//! published for the entire call. Errors are parked in the shared context slot.
//!
//! # See also
//! - `super::super::abi` — machine-visible entry context.

use otter_vm::{JitExceptionOutcome, Value, VmError};

use super::super::{
    JitCtx, JitRet, STATUS_BAILED, STATUS_CONTINUE, STATUS_RETURNED, STATUS_THREW,
    unpack_method_arg_regs,
};

pub(crate) fn park_jit_error(ctx: &mut JitCtx, err: VmError) {
    // SAFETY: every `JitCtx` is built with an initialized error slot that lives
    // for the compiled entry's dynamic extent; nested direct-call contexts copy
    // the same pointer.
    unsafe {
        *ctx.error = Some(err);
    }
}

/// Complete one structured-exception opcode. Unlike ordinary status-word
/// transitions this returns the full compiled-entry pair: a committed handler
/// mutation may continue, resume at a dynamic logical PC, return a value, or
/// propagate a parked error.
pub(crate) extern "C" fn jit_exception_op_stub(
    ctx: *mut JitCtx,
    opcode: u64,
    arg0: u64,
    arg1: u64,
    arg2: u64,
) -> JitRet {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let vm = unsafe { &mut *ctx.activation().vm_ptr() };
    let stack = unsafe { &mut *ctx.activation().stack_ptr() };
    let context = unsafe { &*ctx.activation().context_ptr() };
    match vm.jit_runtime_exception_op(
        context,
        stack,
        ctx.frame_index,
        opcode as u8,
        arg0,
        arg1,
        arg2,
    ) {
        Ok(JitExceptionOutcome::Continue) => JitRet {
            value: 0,
            status: STATUS_CONTINUE,
        },
        Ok(JitExceptionOutcome::Resume(pc)) => JitRet {
            value: u64::from(pc),
            status: STATUS_BAILED,
        },
        Ok(JitExceptionOutcome::Return(value)) => JitRet {
            value: value.to_bits(),
            status: STATUS_RETURNED,
        },
        Err(err) => {
            park_jit_error(ctx, err);
            JitRet {
                value: 0,
                status: STATUS_THREW,
            }
        }
    }
}

/// Complete one iterator-lifecycle opcode. `0` means the VM committed the
/// opcode and the template may fall through; `1` reports a parked throw; `2`
/// is reserved for an absent activation and therefore remains an exact
/// pre-effect side exit.
pub(crate) extern "C" fn jit_iterator_op_stub(
    ctx: *mut JitCtx,
    opcode: u64,
    arg0: u64,
    arg1: u64,
    arg2: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let Some(activation) = ctx.checked_activation() else {
        return STATUS_BAILED;
    };
    let vm = unsafe { &mut *activation.vm_ptr() };
    let stack = unsafe { &mut *activation.stack_ptr() };
    let context = unsafe { &*activation.context_ptr() };
    match vm.jit_runtime_iterator_op(
        context,
        stack,
        ctx.frame_index,
        opcode as u8,
        arg0,
        arg1,
        arg2,
    ) {
        Ok(()) => 0,
        Err(err) => {
            park_jit_error(ctx, err);
            STATUS_THREW
        }
    }
}

/// Complete one private-member opcode (`PrivateGet`, `PrivateSet`,
/// `PrivateBrandCheck`). `0` means the VM committed the opcode and the template
/// may fall through; `1` reports a parked throw; `2` remains an exact pre-effect
/// side exit for an absent activation.
pub(crate) extern "C" fn jit_private_op_stub(
    ctx: *mut JitCtx,
    opcode: u64,
    arg0: u64,
    arg1: u64,
    arg2: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let Some(activation) = ctx.checked_activation() else {
        return STATUS_BAILED;
    };
    let vm = unsafe { &mut *activation.vm_ptr() };
    let stack = unsafe { &mut *activation.stack_ptr() };
    let context = unsafe { &*activation.context_ptr() };
    match vm.jit_runtime_private_op(
        context,
        stack,
        ctx.frame_index,
        opcode as u8,
        arg0,
        arg1,
        arg2,
    ) {
        Ok(()) => 0,
        Err(err) => {
            park_jit_error(ctx, err);
            STATUS_THREW
        }
    }
}

/// Complete one `super` property opcode (`LoadSuperProperty`,
/// `LoadSuperElement`, `SetSuperProperty`, `SetSuperElement`). `0` means the VM
/// committed the opcode and the template may fall through; `1` reports a parked
/// throw; `2` remains an exact pre-effect side exit for an absent activation.
pub(crate) extern "C" fn jit_super_op_stub(
    ctx: *mut JitCtx,
    opcode: u64,
    arg0: u64,
    arg1: u64,
    arg2: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let Some(activation) = ctx.checked_activation() else {
        return STATUS_BAILED;
    };
    let vm = unsafe { &mut *activation.vm_ptr() };
    let stack = unsafe { &mut *activation.stack_ptr() };
    let context = unsafe { &*activation.context_ptr() };
    match vm.jit_runtime_super_op(
        context,
        stack,
        ctx.frame_index,
        opcode as u8,
        arg0,
        arg1,
        arg2,
    ) {
        Ok(()) => 0,
        Err(err) => {
            park_jit_error(ctx, err);
            STATUS_THREW
        }
    }
}

/// Complete one scalar value-query/coercion opcode (`ToObject`,
/// `ToPropertyKey`, `TypeOf`, `LoadNewTarget`, `SameValue`, `IsArray`,
/// `ArrayLength`, `LoadLength`). `0` means the VM committed the opcode and the
/// template may fall through; `1` reports a parked throw; `2` remains an exact
/// pre-effect side exit for an absent activation.
pub(crate) extern "C" fn jit_scalar_op_stub(
    ctx: *mut JitCtx,
    opcode: u64,
    arg0: u64,
    arg1: u64,
    arg2: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let Some(activation) = ctx.checked_activation() else {
        return STATUS_BAILED;
    };
    let vm = unsafe { &mut *activation.vm_ptr() };
    let stack = unsafe { &mut *activation.stack_ptr() };
    let context = unsafe { &*activation.context_ptr() };
    match vm.jit_runtime_scalar_op(
        context,
        stack,
        ctx.frame_index,
        opcode as u8,
        arg0,
        arg1,
        arg2,
    ) {
        Ok(()) => 0,
        Err(err) => {
            park_jit_error(ctx, err);
            STATUS_THREW
        }
    }
}

/// Complete one `delete` opcode (`DeleteProperty`, `DeleteElement`,
/// `DeleteDynamic`). `0` means the VM committed the opcode and the template may
/// fall through; `1` reports a parked throw; `2` remains an exact pre-effect
/// side exit for an absent activation.
pub(crate) extern "C" fn jit_delete_op_stub(
    ctx: *mut JitCtx,
    opcode: u64,
    arg0: u64,
    arg1: u64,
    arg2: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let Some(activation) = ctx.checked_activation() else {
        return STATUS_BAILED;
    };
    let vm = unsafe { &mut *activation.vm_ptr() };
    let stack = unsafe { &mut *activation.stack_ptr() };
    let context = unsafe { &*activation.context_ptr() };
    match vm.jit_runtime_delete_op(
        context,
        stack,
        ctx.frame_index,
        opcode as u8,
        arg0,
        arg1,
        arg2,
    ) {
        Ok(()) => 0,
        Err(err) => {
            park_jit_error(ctx, err);
            STATUS_THREW
        }
    }
}

/// Complete one object property-protocol opcode (`Instanceof`, `HasProperty`,
/// `GetPrototype`, `SetPrototype`). `0` means the VM committed the opcode and
/// the template may fall through; `1` reports a parked throw; `2` remains an
/// exact pre-effect side exit for an absent activation.
pub(crate) extern "C" fn jit_object_protocol_op_stub(
    ctx: *mut JitCtx,
    opcode: u64,
    arg0: u64,
    arg1: u64,
    arg2: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let Some(activation) = ctx.checked_activation() else {
        return STATUS_BAILED;
    };
    let vm = unsafe { &mut *activation.vm_ptr() };
    let stack = unsafe { &mut *activation.stack_ptr() };
    let context = unsafe { &*activation.context_ptr() };
    match vm.jit_runtime_object_protocol_op(
        context,
        stack,
        ctx.frame_index,
        opcode as u8,
        arg0,
        arg1,
        arg2,
    ) {
        Ok(()) => 0,
        Err(err) => {
            park_jit_error(ctx, err);
            STATUS_THREW
        }
    }
}

/// Complete one global-access opcode (`LoadGlobalThis`,
/// `LoadGlobalOrUndefined`, `StoreGlobalBinding`, `StoreGlobalChecked`). `0`
/// means the VM committed the opcode and the template may fall through; `1`
/// reports a parked throw; `2` remains an exact pre-effect side exit for an
/// absent activation.
pub(crate) extern "C" fn jit_global_op_stub(
    ctx: *mut JitCtx,
    opcode: u64,
    arg0: u64,
    arg1: u64,
    arg2: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let Some(activation) = ctx.checked_activation() else {
        return STATUS_BAILED;
    };
    let vm = unsafe { &mut *activation.vm_ptr() };
    let stack = unsafe { &mut *activation.stack_ptr() };
    let context = unsafe { &*activation.context_ptr() };
    match vm.jit_runtime_global_op(
        context,
        stack,
        ctx.frame_index,
        opcode as u8,
        arg0,
        arg1,
        arg2,
    ) {
        Ok(()) => 0,
        Err(err) => {
            park_jit_error(ctx, err);
            STATUS_THREW
        }
    }
}

/// Complete one `Op::BindFunction`. `packed_meta` is
/// `dst | callee<<16 | this<<32 | argc<<48`; `packed_args` holds the bound-arg
/// registers. `0` means the VM committed the bind and the template may fall
/// through; `1` reports a parked throw; `2` remains an exact pre-effect side
/// exit for an absent activation.
pub(crate) extern "C" fn jit_bind_function_stub(
    ctx: *mut JitCtx,
    packed_meta: u64,
    packed_args: u64,
    _reserved0: u64,
    _reserved1: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let Some(activation) = ctx.checked_activation() else {
        return STATUS_BAILED;
    };
    let vm = unsafe { &mut *activation.vm_ptr() };
    let stack = unsafe { &mut *activation.stack_ptr() };
    let context = unsafe { &*activation.context_ptr() };
    match vm.jit_runtime_bind_function(context, stack, ctx.frame_index, packed_meta, packed_args) {
        Ok(()) => 0,
        Err(err) => {
            park_jit_error(ctx, err);
            STATUS_THREW
        }
    }
}

/// Publish one machine-constructed [`JitCtx`] before its compiled entry can
/// reach an allocating/reentrant safepoint. Returns `0` on success and parks a
/// stack-overflow error in the shared slot on failure.
pub(crate) extern "C" fn jit_push_native_activation_stub(ctx: *mut JitCtx) -> u64 {
    // SAFETY: the caller has fully initialized `ctx` on its native stack and
    // keeps it live until the matching pop stub.
    let ctx = unsafe { &mut *ctx };
    let vm = unsafe { &mut *ctx.activation().vm_ptr() };
    // SAFETY: both fields live inside `ctx`, whose native allocation remains
    // live across the compiled callee's dynamic extent.
    match unsafe {
        vm.jit_push_native_activation(
            std::ptr::addr_of_mut!(ctx.self_closure),
            std::ptr::addr_of_mut!(ctx.this_value),
        )
    } {
        Ok(()) => 0,
        Err(err) => {
            park_jit_error(ctx, err);
            1
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
    0
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
    let vm = unsafe { &mut *ctx.activation().vm_ptr() };
    // SAFETY: `callee_reg` comes from a bytecode register operand inside the
    // active frame window.
    let callee_bits = unsafe { *ctx.regs.add(callee_reg as usize) };
    vm.jit_inline_closure_upvalues(Value::from_bits(callee_bits), expected_fid as u32)
        .unwrap_or(0)
}

/// Prepare a direct compiled **plain** call (`callee(args…)`).
///
/// Fills `ctx.direct_*` and returns `0` on success; `1` parks a thrown error
/// in the ctx; `2` means "ineligible — bail to the interpreter", which runs
/// the call with full semantics.
pub(crate) extern "C" fn jit_prepare_direct_call_stub(
    ctx: *mut JitCtx,
    callee_reg: u64,
    argc: u64,
    packed_args: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let vm = unsafe { &mut *ctx.activation().vm_ptr() };
    let stack = unsafe { &mut *ctx.activation().stack_ptr() };
    let context = unsafe { &*ctx.activation().context_ptr() };
    let all = unpack_method_arg_regs(packed_args);
    let argc = (argc as usize).min(all.len());
    match vm.jit_prepare_direct_call(
        context,
        stack,
        ctx.frame_index,
        callee_reg as u16,
        &all[..argc],
        ctx.regs.cast::<otter_vm::Value>().cast_const(),
    ) {
        Ok(Some(prepared)) => {
            ctx.direct_entry_addr = prepared.entry_addr;
            ctx.direct_regs = prepared.regs;
            ctx.direct_self_closure = prepared.self_closure;
            ctx.direct_this_value = prepared.this_value;
            ctx.direct_frame_index = prepared.frame_index;
            ctx.direct_upvalues_ptr = prepared.upvalues_ptr;
            ctx.direct_frame_ids = prepared.frame_ids;
            ctx.direct_frame_meta = prepared.frame_meta;
            ctx.direct_code_object_id = prepared.code_object_id;
            0
        }
        Ok(None) => 2,
        Err(err) => {
            park_jit_error(ctx, err);
            1
        }
    }
}

/// Prepare a direct compiled **method** call (`recv.name(args…)`). Same
/// `ctx.direct_*` / status contract as [`jit_prepare_direct_call_stub`], but
/// status `2` means "ineligible — use the in-place full method-call stub"
/// rather than "bail to the interpreter" (a native/polymorphic method in a hot
/// loop must keep running compiled).
#[allow(clippy::too_many_arguments)]
pub(crate) extern "C" fn jit_prepare_direct_method_call_stub(
    ctx: *mut JitCtx,
    recv: u64,
    name_idx: u64,
    site: u64,
    argc: u64,
    packed_args: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let vm = unsafe { &mut *ctx.activation().vm_ptr() };
    let stack = unsafe { &mut *ctx.activation().stack_ptr() };
    let context = unsafe { &*ctx.activation().context_ptr() };
    let all = unpack_method_arg_regs(packed_args);
    let argc = (argc as usize).min(all.len());
    match vm.jit_prepare_direct_method_call(
        context,
        stack,
        ctx.frame_index,
        recv as u16,
        name_idx as u32,
        site as usize,
        &all[..argc],
        ctx.regs.cast::<otter_vm::Value>().cast_const(),
    ) {
        Ok(Some(prepared)) => {
            ctx.direct_entry_addr = prepared.entry_addr;
            ctx.direct_regs = prepared.regs;
            ctx.direct_self_closure = prepared.self_closure;
            ctx.direct_this_value = prepared.this_value;
            ctx.direct_frame_index = prepared.frame_index;
            ctx.direct_upvalues_ptr = prepared.upvalues_ptr;
            ctx.direct_frame_ids = prepared.frame_ids;
            ctx.direct_frame_meta = prepared.frame_meta;
            ctx.direct_code_object_id = prepared.code_object_id;
            0
        }
        Ok(None) => 2,
        Err(err) => {
            park_jit_error(ctx, err);
            1
        }
    }
}

/// Complete one full `CallMethodValue` in the VM after the direct-call
/// prepare reported an ineligible resolution. `0` = destination written and
/// the compiled caller continues, `1` = threw (including a resolved missing or
/// non-callable method), `2` = an exotic receiver whose bespoke opcode branch
/// has not started and must run in the interpreter (exact side exit).
pub(crate) extern "C" fn jit_call_method_generic_stub(
    ctx: *mut JitCtx,
    dst: u64,
    recv: u64,
    name_idx: u64,
    site: u64,
    argc: u64,
    packed_args: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let Some(activation) = ctx.checked_activation() else {
        return 2;
    };
    let vm = unsafe { &mut *activation.vm_ptr() };
    let stack = unsafe { &mut *activation.stack_ptr() };
    let context = unsafe { &*activation.context_ptr() };
    let all = unpack_method_arg_regs(packed_args);
    let argc = (argc as usize).min(all.len());
    match vm.jit_runtime_call_method_in_place(
        context,
        stack,
        ctx.frame_index,
        dst as u16,
        recv as u16,
        name_idx as u32,
        site as usize,
        &all[..argc],
        ctx.regs.cast::<otter_vm::Value>(),
    ) {
        Ok(true) => 0,
        Ok(false) => 2,
        Err(err) => {
            park_jit_error(ctx, err);
            1
        }
    }
}

/// Complete one full plain `Call` in the VM after the direct-call prepare
/// reported an ineligible callee. `0` = destination written and the compiled
/// caller continues, `1` = threw, `2` = non-callable (exact side exit; the
/// interpreter owns the thrown error).
pub(crate) extern "C" fn jit_call_generic_stub(
    ctx: *mut JitCtx,
    dst: u64,
    callee: u64,
    argc: u64,
    packed_args: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let Some(activation) = ctx.checked_activation() else {
        return 2;
    };
    let vm = unsafe { &mut *activation.vm_ptr() };
    let context = unsafe { &*activation.context_ptr() };
    let all = unpack_method_arg_regs(packed_args);
    let argc = (argc as usize).min(all.len());
    match vm.jit_runtime_call_in_place(
        context,
        dst as u16,
        callee as u16,
        &all[..argc],
        ctx.regs.cast::<otter_vm::Value>(),
    ) {
        Ok(true) => 0,
        Ok(false) => 2,
        Err(err) => {
            park_jit_error(ctx, err);
            1
        }
    }
}

/// Complete one full `Op::New` construct in the VM. `0` = destination
/// written and the compiled caller continues, `1` = threw, `2` = a
/// non-constructor callee or no live activation (exact side exit; the
/// interpreter owns the thrown error).
pub(crate) extern "C" fn jit_construct_stub(
    ctx: *mut JitCtx,
    dst: u64,
    callee: u64,
    argc: u64,
    packed_args: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let Some(activation) = ctx.checked_activation() else {
        return 2;
    };
    let vm = unsafe { &mut *activation.vm_ptr() };
    let context = unsafe { &*activation.context_ptr() };
    let all = unpack_method_arg_regs(packed_args);
    let argc = (argc as usize).min(all.len());
    match vm.jit_runtime_construct_in_place(
        context,
        dst as u16,
        callee as u16,
        &all[..argc],
        ctx.regs.cast::<otter_vm::Value>(),
    ) {
        Ok(true) => 0,
        Ok(false) => 2,
        Err(err) => {
            park_jit_error(ctx, err);
            1
        }
    }
}

/// Complete one full loose-equality opcode in the VM. `0` = destination
/// written, `1` = threw (coercion raised), `2` = no live activation
/// (isolate-less probe harness; exact side exit).
pub(crate) extern "C" fn jit_loose_eq_stub(
    ctx: *mut JitCtx,
    dst: u64,
    lhs: u64,
    rhs: u64,
    negate: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let Some(activation) = ctx.checked_activation() else {
        return 2;
    };
    let vm = unsafe { &mut *activation.vm_ptr() };
    let context = unsafe { &*activation.context_ptr() };
    match vm.jit_runtime_loose_equal_in_place(
        context,
        dst as u16,
        lhs as u16,
        rhs as u16,
        negate != 0,
        ctx.regs.cast::<otter_vm::Value>(),
    ) {
        Ok(()) => 0,
        Err(err) => {
            park_jit_error(ctx, err);
            1
        }
    }
}

/// Complete one `ToPrimitive`/`ToNumeric` opcode in the VM. `0` means the
/// destination was written, `1` means a coercion hook threw, and `2` is
/// reserved for an isolate-less probe context with no published activation.
pub(crate) extern "C" fn jit_coerce_unary_stub(
    ctx: *mut JitCtx,
    dst: u64,
    src: u64,
    numeric: u64,
    hint_index: u64,
    function_id: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let Some(activation) = ctx.checked_activation() else {
        return 2;
    };
    let vm = unsafe { &mut *activation.vm_ptr() };
    let context = unsafe { &*activation.context_ptr() };
    match vm.jit_runtime_coerce_unary_in_place(
        context,
        dst as u16,
        src as u16,
        numeric != 0,
        hint_index as u32,
        function_id as u32,
        ctx.regs.cast::<otter_vm::Value>(),
    ) {
        Ok(()) => 0,
        Err(err) => {
            park_jit_error(ctx, err);
            1
        }
    }
}

/// Complete one numeric-family opcode in the VM. `0` means the destination
/// was committed, `1` means the operation threw, and `2` is reserved for an
/// isolate-less compiled-entry probe where no VM activation exists.
pub(crate) extern "C" fn jit_numeric_op_stub(
    ctx: *mut JitCtx,
    dst: u64,
    lhs: u64,
    rhs_or_delta: u64,
    opcode: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let Some(activation) = ctx.checked_activation() else {
        return 2;
    };
    let vm = unsafe { &mut *activation.vm_ptr() };
    let stack = unsafe { &mut *activation.stack_ptr() };
    let context = unsafe { &*activation.context_ptr() };
    match vm.jit_runtime_numeric_op(
        context,
        stack,
        ctx.frame_index,
        dst as u16,
        lhs as u16,
        rhs_or_delta,
        opcode as u8,
    ) {
        Ok(()) => 0,
        Err(err) => {
            park_jit_error(ctx, err);
            1
        }
    }
}

pub(crate) extern "C" fn jit_finish_direct_call_returned_stub(
    ctx: *mut JitCtx,
    dst: u64,
    callee_frame_index: u64,
    value: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let vm = unsafe { &mut *ctx.activation().vm_ptr() };
    let stack = unsafe { &mut *ctx.activation().stack_ptr() };
    match vm.jit_finish_direct_call_returned(
        stack,
        ctx.frame_index,
        callee_frame_index as usize,
        dst as u16,
        Value::from_bits(value),
    ) {
        Ok(()) => 0,
        Err(err) => {
            park_jit_error(ctx, err);
            1
        }
    }
}

pub(crate) extern "C" fn jit_finish_direct_call_bailed_stub(
    ctx: *mut JitCtx,
    dst: u64,
    callee_frame_index: u64,
    resume_pc: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let vm = unsafe { &mut *ctx.activation().vm_ptr() };
    let stack = unsafe { &mut *ctx.activation().stack_ptr() };
    let context = unsafe { &*ctx.activation().context_ptr() };
    match vm.jit_finish_direct_call_bailed(
        context,
        stack,
        ctx.frame_index,
        callee_frame_index as usize,
        dst as u16,
        resume_pc as u32,
    ) {
        Ok(()) => 0,
        Err(err) => {
            park_jit_error(ctx, err);
            1
        }
    }
}

pub(crate) extern "C" fn jit_abort_direct_call_stub(
    ctx: *mut JitCtx,
    callee_frame_index: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let vm = unsafe { &mut *ctx.activation().vm_ptr() };
    let stack = unsafe { &mut *ctx.activation().stack_ptr() };
    vm.jit_abort_direct_call(stack, callee_frame_index as usize);
    0
}

/// Bridge stub for a *frameless* self-recursive callee that bailed: rebuild an
/// interpreter frame from the live register-stack window and run it to
/// completion. Returns the callee's value in `x0` with `STATUS_RETURNED`, or
/// `STATUS_THREW` (error parked in `ctx`) on an uncaught throw.
pub(crate) extern "C" fn jit_self_call_bail_stub(
    ctx: *mut JitCtx,
    resume_pc: u64,
    regcount: u64,
) -> JitRet {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let vm = unsafe { &mut *ctx.activation().vm_ptr() };
    let stack = unsafe { &mut *ctx.activation().stack_ptr() };
    let context = unsafe { &*ctx.activation().context_ptr() };
    match vm.jit_self_call_bail(
        context,
        stack,
        ctx.frame_index,
        resume_pc as u32,
        regcount as usize,
    ) {
        Ok(value) => JitRet {
            value: value.to_bits(),
            status: STATUS_RETURNED,
        },
        Err(err) => {
            park_jit_error(ctx, err);
            JitRet {
                value: 0,
                status: STATUS_THREW,
            }
        }
    }
}

/// Bridge stub: build a `MakeFunction` closure from compiled code. Returns `0`
/// on success, `1` when construction threw (error parked in `ctx`).
pub(crate) extern "C" fn jit_make_fn_stub(ctx: *mut JitCtx, dst: u64, idx: u64) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let vm = unsafe { &mut *ctx.activation().vm_ptr() };
    let stack = unsafe { &mut *ctx.activation().stack_ptr() };
    let context = unsafe { &*ctx.activation().context_ptr() };
    match vm.jit_runtime_make_function(context, stack, ctx.frame_index, dst as u16, idx as u32) {
        Ok(()) => 0,
        Err(err) => {
            park_jit_error(ctx, err);
            1
        }
    }
}

/// Poll VM interrupts and runtime budget on compiled back-edges. Mirrors the
/// interpreter's cooperative checkpoint so watchdogs and budget rejection apply
/// equally after a loop tiers up through OSR.
pub(crate) extern "C" fn jit_backedge_poll_stub(ctx: *mut JitCtx) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let vm_ptr = ctx
        .checked_activation()
        .map_or(std::ptr::null_mut(), |activation| activation.vm_ptr());
    if vm_ptr.is_null() {
        return 0;
    }
    // SAFETY: a published activation names a live interpreter for this entry.
    let vm = unsafe { &mut *vm_ptr };
    match vm.jit_backedge_poll() {
        Ok(()) => 0,
        Err(err) => {
            park_jit_error(ctx, err);
            1
        }
    }
}
