//! Typed C ABI entries for baseline JIT runtime slow paths.
//!
//! # Contents
//! Fixed-operand coercion and descriptor operations plus compile-owned variadic
//! metadata adapters for arrays, closures, and `Math` calls.
//!
//! # Invariants
//! - Operands are decoded during compilation. No entry accepts a byte PC or
//!   looks up a `CodeBlockInstruction` at runtime.
//! - Raw metadata pointers target immutable boxed slices retained by the active
//!   `BaselineCode` for the executable mapping's full lifetime.
//! - JS values remain in the published frame window across every allocating or
//!   throwing operation, preserving precise moving-GC roots.
//!
//! # See also
//! - [`super::BaselineCode`] for metadata ownership.
//! - `otter_vm::jit_runtime_ops` for the safe typed VM operations.

use otter_vm::VmError;

use super::JitCtx;

mod reentry;
pub(super) use reentry::*;

fn park_result(ctx: &mut JitCtx, result: Result<(), VmError>) -> u64 {
    match result {
        Ok(()) => 0,
        Err(err) => {
            park_jit_error(ctx, err);
            1
        }
    }
}

pub(super) extern "C" fn jit_add_stub(ctx: *mut JitCtx, dst: u64, lhs: u64, rhs: u64) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let vm = unsafe { &mut *ctx.activation().vm_ptr() };
    let stack = unsafe { &mut *ctx.activation().stack_ptr() };
    let result = vm.jit_runtime_add(stack, ctx.frame_index, dst as u16, lhs as u16, rhs as u16);
    park_result(ctx, result)
}

pub(super) extern "C" fn jit_store_upvalue_checked_stub(
    ctx: *mut JitCtx,
    src: u64,
    idx: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let vm = unsafe { &mut *ctx.activation().vm_ptr() };
    let stack = unsafe { &mut *ctx.activation().stack_ptr() };
    let result =
        vm.jit_runtime_store_upvalue_checked(stack, ctx.frame_index, src as u16, idx as u32 as i32);
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
    let vm = unsafe { &mut *ctx.activation().vm_ptr() };
    let stack = unsafe { &mut *ctx.activation().stack_ptr() };
    let context = unsafe { &*ctx.activation().context_ptr() };
    let result = vm.jit_runtime_load_string(
        context,
        stack,
        ctx.frame_index,
        function_id as u32,
        dst as u16,
        constant_index as u32,
    );
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
    let vm = unsafe { &mut *ctx.activation().vm_ptr() };
    let stack = unsafe { &mut *ctx.activation().stack_ptr() };
    let context = unsafe { &*ctx.activation().context_ptr() };
    let result = vm.jit_runtime_define_data_property(
        context,
        stack,
        ctx.frame_index,
        object as u16,
        key as u16,
        value as u16,
    );
    park_result(ctx, result)
}

pub(super) extern "C" fn jit_fresh_upvalue_stub(ctx: *mut JitCtx, idx: u64) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let vm = unsafe { &mut *ctx.activation().vm_ptr() };
    let stack = unsafe { &mut *ctx.activation().stack_ptr() };
    let result = vm.jit_runtime_fresh_upvalue(stack, ctx.frame_index, idx as u32 as i32);
    park_result(ctx, result)
}

pub(super) extern "C" fn jit_load_builtin_error_stub(
    ctx: *mut JitCtx,
    dst: u64,
    kind_index: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let vm = unsafe { &mut *ctx.activation().vm_ptr() };
    let stack = unsafe { &mut *ctx.activation().stack_ptr() };
    let context = unsafe { &*ctx.activation().context_ptr() };
    let result = vm.jit_runtime_load_builtin_error(
        context,
        stack,
        ctx.frame_index,
        dst as u16,
        kind_index as u32,
    );
    park_result(ctx, result)
}

pub(super) extern "C" fn jit_neg_stub(ctx: *mut JitCtx, dst: u64, src: u64) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let vm = unsafe { &mut *ctx.activation().vm_ptr() };
    let stack = unsafe { &mut *ctx.activation().stack_ptr() };
    let result = vm.jit_runtime_neg(stack, ctx.frame_index, dst as u16, src as u16);
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
    let vm = unsafe { &mut *ctx.activation().vm_ptr() };
    let stack = unsafe { &mut *ctx.activation().stack_ptr() };
    let context = unsafe { &*ctx.activation().context_ptr() };
    let result = vm.jit_runtime_define_own_property(
        context,
        stack,
        ctx.frame_index,
        target as u16,
        key as u16,
        descriptor as u16,
    );
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
    let vm = unsafe { &mut *ctx.activation().vm_ptr() };
    let stack = unsafe { &mut *ctx.activation().stack_ptr() };
    let context = unsafe { &*ctx.activation().context_ptr() };
    let parent_indices =
        unsafe { std::slice::from_raw_parts(parent_indices, parent_count as usize) };
    let result = vm.jit_runtime_make_closure(
        context,
        stack,
        ctx.frame_index,
        function_id as u32,
        dst as u16,
        function_index as u32,
        parent_indices,
    );
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
    let vm = unsafe { &mut *ctx.activation().vm_ptr() };
    let stack = unsafe { &mut *ctx.activation().stack_ptr() };
    let context = unsafe { &*ctx.activation().context_ptr() };
    let argument_regs =
        unsafe { std::slice::from_raw_parts(argument_regs, argument_count as usize) };
    let result = vm.jit_runtime_math_call(
        context,
        stack,
        ctx.frame_index,
        dst as u16,
        method_id as u32,
        argument_regs,
    );
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
    let vm = unsafe { &mut *ctx.activation().vm_ptr() };
    let stack = unsafe { &mut *ctx.activation().stack_ptr() };
    let source_regs = unsafe { std::slice::from_raw_parts(source_regs, source_count as usize) };
    let result = vm.jit_runtime_new_array(stack, ctx.frame_index, dst as u16, source_regs);
    park_result(ctx, result)
}
