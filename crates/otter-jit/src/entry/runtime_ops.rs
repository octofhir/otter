//! Typed C ABI entries for baseline JIT runtime slow paths.
//!
//! # Contents
//! - Fixed-operand descriptor operations and compile-owned variadic entries.
//! - [`calls`] — native activation and generated-call deoptimization.
//! - [`reentry`] — exception, coercion, and non-call reentrant completion.
//! - [`literals`] — committed allocation from boxed-value spans.
//! - [`vm_ops`] — typed VM operations.
//!
//! # Invariants
//! - No entry accepts a byte PC. Most operands are decoded during compilation;
//!   fixed-value named-property entries use the published function/logical-PC
//!   identity to validate the exact `CodeBlockInstruction` and derive its
//!   immutable property name and feedback site.
//! - Raw metadata pointers target immutable boxed slices retained by the
//!   active code object for the executable mapping's full lifetime.
//! - JS values remain in the published frame window or its precise safepoint
//!   spill map across every allocating or throwing operation. Binding and
//!   declaration entries receive only fixed boxed values; names, flags,
//!   indices, and destinations never cross their ABI.
//! - Arithmetic and coercion entries validate machine-word operands, then use
//!   the VM-owned typed runtime boundary; no VM/container pointer escapes.
//!
//! # See also
//! - `crate::template::code` for metadata ownership.
//! - `otter_vm::jit_runtime_ops` for the safe typed VM operations.

use otter_vm::{VmError, native_abi::NativeResultStatus};

use super::JitCtx;

mod calls;
mod forward_arguments;
mod literals;
mod reentry;
mod vm_ops;
pub(crate) use calls::*;
pub(crate) use forward_arguments::*;
pub(crate) use literals::*;
pub(crate) use reentry::*;
pub(crate) use vm_ops::*;

fn park_result(ctx: &mut JitCtx, result: Result<(), VmError>) -> u64 {
    match result {
        Ok(()) => NativeResultStatus::Success as u64,
        Err(err) => {
            park_jit_error(ctx, err);
            NativeResultStatus::Throw as u64
        }
    }
}

/// Validate one machine-word register operand before it enters VM semantics.
pub(super) fn decode_register(raw: u64) -> Result<u16, VmError> {
    u16::try_from(raw).map_err(|_| VmError::InvalidOperand)
}

fn complete_add(ctx: &mut JitCtx, dst: u16, lhs: u16, rhs: u16) -> Result<(), VmError> {
    ctx.runtime_call()?.add(dst, lhs, rhs)
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

pub(super) extern "C" fn jit_define_data_property_stub(
    ctx: *mut JitCtx,
    object: u64,
    key: u64,
    value: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let result = (|| {
        ctx.runtime_call()?
            .define_data_property(object as u16, key as u16, value as u16)
    })();
    park_result(ctx, result)
}

pub(super) extern "C" fn jit_fresh_upvalue_stub(ctx: *mut JitCtx, idx: u64) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let result = (|| {
        let idx = u32::try_from(idx).map_err(|_| VmError::InvalidOperand)? as i32;
        ctx.runtime_call()?.fresh_upvalue(idx)
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
        ctx.runtime_call()?
            .load_builtin_error(dst as u16, kind_index as u32)
    })();
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
        ctx.runtime_call()?
            .define_own_property(target as u16, key as u16, descriptor as u16)
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
        ctx.runtime_call()?.make_closure(
            function_id as u32,
            dst as u16,
            function_index as u32,
            parent_indices,
        )
    })();
    park_result(ctx, result)
}
