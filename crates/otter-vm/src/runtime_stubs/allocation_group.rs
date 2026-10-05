//! Rooted speculative admission for one fixed generated allocation group.
//!
//! # Contents
//! - `alloc_group_ensure` calls the sole collector LAB/cap/refill owner.
//! - Validated current-frame roots survive success, group refusal and collector error.
//!
//! # Invariants
//! The entry never creates a cell, initializes candidate bytes, advances top,
//! latches a group-sized JS error or enters JS. Miss/OOM both let generated code
//! restore collector-updated homes and eagerly resume the first source member.
//! The canonical source allocator alone decides the legal prefix and actual OOM.
//!
//! # See also
//! - `otter_gc::GcHeap::ensure_machine_allocation_with_roots` owns accounting.

use super::*;

#[must_use]
pub extern "C" fn alloc_group_ensure(
    ctx: *mut RuntimeStubAllocContext,
    safepoint: SafepointId,
    bytes: u64,
    padding0: u64,
    padding1: u64,
) -> NativeResultPair {
    record_alloc_value_stub_result(ctx, ensure(ctx, safepoint, [bytes, padding0, padding1]))
}

fn ensure(
    ctx: *mut RuntimeStubAllocContext,
    safepoint: SafepointId,
    bits: [u64; 3],
) -> NativeResultPair {
    let values = bits.map(Value::from_abi_bits);
    let Some(bytes) = values[0].as_i32().filter(|bytes| *bytes > 0) else {
        return NativeResultPair::miss();
    };
    if !values[1].is_undefined() || !values[2].is_undefined() {
        return NativeResultPair::miss();
    }
    let Some(ctx) = alloc_context_mut(ctx) else {
        return NativeResultPair::miss();
    };
    let Some(vm) = alloc_interpreter_mut(ctx) else {
        return NativeResultPair::miss();
    };
    // SAFETY: the compiled caller retains its current published frame/map and
    // all initialized tagged homes throughout this synchronous boundary.
    let Ok(roots) = (unsafe { alloc_value_stub_call_roots(ctx, safepoint, values) }) else {
        return NativeResultPair::miss();
    };
    let _roots = vm
        .gc_heap
        .register_extra_roots(otter_gc::ExtraRoots::new(&roots));
    match vm
        .gc_heap
        .ensure_machine_allocation_with_roots(bytes as usize, &mut |_| {})
    {
        Ok(true) => NativeResultPair::success(Value::undefined()),
        Ok(false) => NativeResultPair::miss(),
        Err(_) => NativeResultPair::out_of_memory(),
    }
}
