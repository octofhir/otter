//! Typed C ABI entries for baseline JIT runtime slow paths.
//!
//! # Contents
//! - Fixed-operand descriptor operations and compile-owned variadic adapters.
//! - [`calls`] — plain/method-call ABI adapters and direct-call lifecycle.
//! - [`reentry`] — exception, coercion, and non-call reentrant completion.
//! - [`vm_ops`] — typed VM operation bridges.
//!
//! # Invariants
//! - Operands are decoded during compilation. No entry accepts a byte PC or
//!   looks up a `CodeBlockInstruction` at runtime.
//! - Raw metadata pointers target immutable boxed slices retained by the
//!   active code object for the executable mapping's full lifetime.
//! - JS values remain in the published frame window across every allocating or
//!   throwing operation, preserving precise moving-GC roots.
//! - Arithmetic and coercion entries validate machine-word operands, then use
//!   the canonical native activation through `JitCtx::active_frame_mut`.
//!
//! # See also
//! - `crate::template::code` for metadata ownership.
//! - `otter_vm::jit_runtime_ops` for the safe typed VM operations.

use otter_vm::VmError;

use super::JitCtx;

mod calls;
mod reentry;
mod vm_ops;
pub(crate) use calls::*;
pub(crate) use reentry::*;
pub(crate) use vm_ops::*;

fn park_result(ctx: &mut JitCtx, result: Result<(), VmError>) -> u64 {
    match result {
        Ok(()) => 0,
        Err(err) => {
            park_jit_error(ctx, err);
            1
        }
    }
}

/// Validate one machine-word register operand before it enters VM semantics.
pub(super) fn decode_register(raw: u64) -> Result<u16, VmError> {
    u16::try_from(raw).map_err(|_| VmError::InvalidOperand)
}

fn complete_add(ctx: &mut JitCtx, dst: u16, lhs: u16, rhs: u16) -> Result<(), VmError> {
    let activation = ctx.checked_activation().ok_or(VmError::InvalidOperand)?;
    let vm_ptr = activation.vm_ptr();
    let mut frame = ctx.active_frame_mut()?;
    // SAFETY: the published activation retains the interpreter for this
    // compiled entry's dynamic extent. ActiveFrame retains only raw validated
    // window descriptors, so reconstructing the VM creates no overlapping
    // `&mut [Value]`; its read/commit operations are slot-scoped.
    unsafe { &mut *vm_ptr }.jit_runtime_add(&mut frame, dst, lhs, rhs)
}

fn complete_neg(ctx: &mut JitCtx, dst: u16, src: u16) -> Result<(), VmError> {
    let activation = ctx.checked_activation().ok_or(VmError::InvalidOperand)?;
    let vm_ptr = activation.vm_ptr();
    let mut frame = ctx.active_frame_mut()?;
    // SAFETY: same published-activation contract as `complete_add`.
    unsafe { &mut *vm_ptr }.jit_runtime_neg(&mut frame, dst, src)
}

fn complete_new_array(ctx: &mut JitCtx, dst: u16, source_regs: &[u16]) -> Result<(), VmError> {
    let activation = ctx.checked_activation().ok_or(VmError::InvalidOperand)?;
    let vm_ptr = activation.vm_ptr();
    let mut frame = ctx.active_frame_mut()?;
    // SAFETY: the activation and raw canonical register descriptor remain
    // published for the complete allocating operation. No register borrow
    // spans the VM call.
    unsafe { &mut *vm_ptr }.jit_runtime_new_array(&mut frame, dst, source_regs)
}

pub(super) extern "C" fn jit_add_stub(ctx: *mut JitCtx, dst: u64, lhs: u64, rhs: u64) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let result = (|| {
        complete_add(
            ctx,
            decode_register(dst)?,
            decode_register(lhs)?,
            decode_register(rhs)?,
        )
    })();
    park_result(ctx, result)
}

pub(super) extern "C" fn jit_store_upvalue_checked_stub(
    ctx: *mut JitCtx,
    src: u64,
    idx: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let result = (|| {
        let vm_ptr = ctx
            .checked_activation()
            .ok_or(VmError::InvalidOperand)?
            .vm_ptr();
        let mut frame = ctx.active_frame_mut()?;
        let src = decode_register(src)?;
        let idx = u32::try_from(idx).map_err(|_| VmError::InvalidOperand)? as i32;
        // SAFETY: the activation retains the VM; ActiveFrame accesses the live
        // upvalue-handle window one checked slot at a time.
        unsafe { &mut *vm_ptr }.jit_runtime_store_upvalue_checked(&mut frame, src, idx)
    })();
    park_result(ctx, result)
}

pub(super) extern "C" fn jit_load_string_stub(
    ctx: *mut JitCtx,
    function_id: u64,
    dst: u64,
    constant_index: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let result = (|| {
        let frame_index = ctx.materialized_frame_index()?;
        let vm = unsafe { &mut *ctx.activation().vm_ptr() };
        let stack = unsafe { &mut *ctx.activation().stack_ptr() };
        let context = unsafe { &*ctx.activation().context_ptr() };
        vm.jit_runtime_load_string(
            context,
            stack,
            frame_index,
            function_id as u32,
            dst as u16,
            constant_index as u32,
        )
    })();
    park_result(ctx, result)
}

pub(super) extern "C" fn jit_define_data_property_stub(
    ctx: *mut JitCtx,
    object: u64,
    key: u64,
    value: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let result = (|| {
        let frame_index = ctx.materialized_frame_index()?;
        let vm = unsafe { &mut *ctx.activation().vm_ptr() };
        let stack = unsafe { &mut *ctx.activation().stack_ptr() };
        let context = unsafe { &*ctx.activation().context_ptr() };
        vm.jit_runtime_define_data_property(
            context,
            stack,
            frame_index,
            object as u16,
            key as u16,
            value as u16,
        )
    })();
    park_result(ctx, result)
}

pub(super) extern "C" fn jit_fresh_upvalue_stub(ctx: *mut JitCtx, idx: u64) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let result = (|| {
        let vm_ptr = ctx
            .checked_activation()
            .ok_or(VmError::InvalidOperand)?
            .vm_ptr();
        let mut frame = ctx.active_frame_mut()?;
        let idx = u32::try_from(idx).map_err(|_| VmError::InvalidOperand)? as i32;
        // SAFETY: same activation-owned upvalue-window contract as the checked
        // store stub.
        unsafe { &mut *vm_ptr }.jit_runtime_fresh_upvalue(&mut frame, idx)
    })();
    park_result(ctx, result)
}

pub(super) extern "C" fn jit_load_builtin_error_stub(
    ctx: *mut JitCtx,
    dst: u64,
    kind_index: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let result = (|| {
        let frame_index = ctx.materialized_frame_index()?;
        let vm = unsafe { &mut *ctx.activation().vm_ptr() };
        let stack = unsafe { &mut *ctx.activation().stack_ptr() };
        let context = unsafe { &*ctx.activation().context_ptr() };
        vm.jit_runtime_load_builtin_error(
            context,
            stack,
            frame_index,
            dst as u16,
            kind_index as u32,
        )
    })();
    park_result(ctx, result)
}

pub(super) extern "C" fn jit_neg_stub(ctx: *mut JitCtx, dst: u64, src: u64) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let result = (|| complete_neg(ctx, decode_register(dst)?, decode_register(src)?))();
    park_result(ctx, result)
}

pub(super) extern "C" fn jit_define_own_property_stub(
    ctx: *mut JitCtx,
    target: u64,
    key: u64,
    descriptor: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let result = (|| {
        let frame_index = ctx.materialized_frame_index()?;
        let vm = unsafe { &mut *ctx.activation().vm_ptr() };
        let stack = unsafe { &mut *ctx.activation().stack_ptr() };
        let context = unsafe { &*ctx.activation().context_ptr() };
        vm.jit_runtime_define_own_property(
            context,
            stack,
            frame_index,
            target as u16,
            key as u16,
            descriptor as u16,
        )
    })();
    park_result(ctx, result)
}

pub(super) extern "C" fn jit_make_closure_stub(
    ctx: *mut JitCtx,
    function_id: u64,
    dst: u64,
    function_index: u64,
    parent_indices: *const u32,
    parent_count: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract and immutable metadata owner.
    let ctx = unsafe { &mut *ctx };
    let parent_indices =
        unsafe { std::slice::from_raw_parts(parent_indices, parent_count as usize) };
    let result = (|| {
        let frame_index = ctx.materialized_frame_index()?;
        let vm = unsafe { &mut *ctx.activation().vm_ptr() };
        let stack = unsafe { &mut *ctx.activation().stack_ptr() };
        let context = unsafe { &*ctx.activation().context_ptr() };
        vm.jit_runtime_make_closure(
            context,
            stack,
            frame_index,
            function_id as u32,
            dst as u16,
            function_index as u32,
            parent_indices,
        )
    })();
    park_result(ctx, result)
}

pub(super) extern "C" fn jit_math_call_stub(
    ctx: *mut JitCtx,
    dst: u64,
    method_id: u64,
    argument_regs: *const u16,
    argument_count: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract and immutable metadata owner.
    let ctx = unsafe { &mut *ctx };
    let argument_regs =
        unsafe { std::slice::from_raw_parts(argument_regs, argument_count as usize) };
    let result = (|| {
        let frame_index = ctx.materialized_frame_index()?;
        let vm = unsafe { &mut *ctx.activation().vm_ptr() };
        let stack = unsafe { &mut *ctx.activation().stack_ptr() };
        let context = unsafe { &*ctx.activation().context_ptr() };
        vm.jit_runtime_math_call(
            context,
            stack,
            frame_index,
            dst as u16,
            method_id as u32,
            argument_regs,
        )
    })();
    park_result(ctx, result)
}

pub(super) extern "C" fn jit_new_array_stub(
    ctx: *mut JitCtx,
    dst: u64,
    source_regs: *const u16,
    source_count: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract and immutable metadata owner.
    let ctx = unsafe { &mut *ctx };
    let source_regs = unsafe { std::slice::from_raw_parts(source_regs, source_count as usize) };
    let result = (|| complete_new_array(ctx, decode_register(dst)?, source_regs))();
    park_result(ctx, result)
}
