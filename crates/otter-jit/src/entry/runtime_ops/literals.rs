//! Boxed-value literal allocation shared by compiled tiers.
//!
//! # Contents
//! - Object allocation from an empty span and array allocation from elements.
//!
//! # Invariants
//! - Generated callers publish precise roots independently of this value ABI.
//! - Array elements are copied before the VM may collect; the canonical
//!   allocator traces the owned element buffer during construction.
//! - The result is committed once. No destination register, metadata pointer,
//!   or interpreter-frame adapter crosses the allocation boundary.
//!
//! # See also
//! - `otter_vm::RuntimeCall` for the shared representation-neutral allocator.

use otter_vm::{Value, VmError, native_abi::NativeResultPair};

use super::{JitCtx, reentry::committed_vm_result};

pub(crate) extern "C" fn jit_new_object_stub(
    ctx: *mut JitCtx,
    _values: *const Value,
    count: u32,
) -> NativeResultPair {
    // SAFETY: the caller owns a live published native activation and JitCtx.
    let ctx = unsafe { &mut *ctx };
    let result = if count == 0 {
        ctx.runtime_call()
            .and_then(|mut runtime| runtime.new_object_value())
    } else {
        Err(VmError::InvalidOperand)
    };
    committed_vm_result(ctx, result)
}

pub(crate) extern "C" fn jit_new_array_stub(
    ctx: *mut JitCtx,
    values: *const Value,
    count: u32,
) -> NativeResultPair {
    let copied = if count == 0 {
        Ok(smallvec::SmallVec::<[Value; 8]>::new())
    } else if values.is_null() || !(values as usize).is_multiple_of(std::mem::align_of::<Value>()) {
        Err(VmError::InvalidOperand)
    } else {
        // SAFETY: generated code owns count initialized, aligned Value words
        // for this synchronous call. The u32 extent fits isize on supported
        // 64-bit targets. No VM operation happens before the copy completes.
        let values = unsafe { std::slice::from_raw_parts(values, count as usize) };
        Ok(smallvec::SmallVec::<[Value; 8]>::from_slice(values))
    };
    // SAFETY: as above; the stack packet is no longer borrowed.
    let ctx = unsafe { &mut *ctx };
    let result = copied.and_then(|values| {
        ctx.runtime_call()
            .and_then(|mut runtime| runtime.new_array_value(&values))
    });
    committed_vm_result(ctx, result)
}
