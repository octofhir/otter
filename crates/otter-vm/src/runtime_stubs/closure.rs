//! Boxed closure allocation from the published semantic source.
//!
//! # Contents
//! - One typed Probe entry for each function/closure bytecode operation.
//! - Innermost source constant resolution and rooted explicit lexical inputs.
//!
//! # Invariants
//! - Operands are values, never window indices or physical-frame bindings.
//! - Source metadata borrows end before allocation; all three value copies and
//!   the caller's safepoint roots remain registered throughout the allocation.
//! - Success publishes one fully initialized cell; refusal has no JS effect.
//!
//! # See also
//! - `crate::function_ops` owns the shared interpreter construction kernel.
//! - `crate::runtime_activation::semantic_source` owns source lineage.

use super::*;
use otter_bytecode::Op;

/// Allocate the current source's capture-free function using rooted ABI words.
#[must_use]
pub extern "C" fn make_function_alloc(
    ctx: *mut RuntimeStubAllocContext,
    safepoint: SafepointId,
    value0: u64,
    value1: u64,
    value2: u64,
) -> NativeResultPair {
    record_alloc_value_stub_result(
        ctx,
        allocate(ctx, safepoint, [value0, value1, value2], Op::MakeFunction),
    )
}

/// Allocate the current source's closure from explicit rooted lexical bindings.
#[must_use]
pub extern "C" fn make_closure_alloc(
    ctx: *mut RuntimeStubAllocContext,
    safepoint: SafepointId,
    value0: u64,
    value1: u64,
    value2: u64,
) -> NativeResultPair {
    record_alloc_value_stub_result(
        ctx,
        allocate(ctx, safepoint, [value0, value1, value2], Op::MakeClosure),
    )
}

fn allocate(
    ctx: *mut RuntimeStubAllocContext,
    safepoint: SafepointId,
    bits: [u64; 3],
    opcode: Op,
) -> NativeResultPair {
    let Some(ctx) = alloc_context_mut(ctx) else {
        return NativeResultPair::miss();
    };
    let Some(context) = alloc_execution_context(ctx) else {
        return NativeResultPair::miss();
    };
    let Some(vm) = alloc_interpreter_mut(ctx) else {
        return NativeResultPair::miss();
    };
    let frame = ctx.current_frame();
    if frame.is_null() {
        return NativeResultPair::miss();
    }
    // SAFETY: the packet's current frame stays published for this synchronous call.
    let Ok((function_id, pc)) =
        crate::runtime_activation::semantic_source::frame_semantic_source(vm, context, unsafe {
            &*frame
        })
    else {
        return NativeResultPair::miss();
    };
    let Ok(owner) = context.for_function(function_id) else {
        return NativeResultPair::miss();
    };
    let Some(function) = owner.exec_function(function_id) else {
        return NativeResultPair::miss();
    };
    let Some(instruction) = function.instr_at_index(pc as usize) else {
        return NativeResultPair::miss();
    };
    if function.op(instruction) != opcode {
        return NativeResultPair::miss();
    }
    let Some(constant) = function.const_index(instruction, 1) else {
        return NativeResultPair::miss();
    };
    let values = bits.map(Value::from_abi_bits);
    if opcode == Op::MakeFunction && values.iter().any(|value| !value.is_undefined()) {
        return NativeResultPair::miss();
    }
    if crate::context_ops::context_operand(values[0]).is_err() {
        return NativeResultPair::miss();
    }
    // SAFETY: the packet and its current frame/spill record stay live for the call.
    let Ok(roots) = (unsafe { alloc_value_stub_call_roots(ctx, safepoint, values) }) else {
        return NativeResultPair::miss();
    };
    let _roots_guard = vm
        .gc_heap
        .register_extra_roots(otter_gc::ExtraRoots::new(&roots));
    let result = if opcode == Op::MakeFunction {
        vm.make_function_value(&owner, constant)
    } else {
        vm.make_closure_value(
            &owner,
            constant,
            roots.value(0),
            roots.value(1),
            roots.value(2),
        )
    };
    match result {
        Ok(value) => NativeResultPair::success(value),
        Err(crate::VmError::OutOfMemory { .. }) => NativeResultPair::out_of_memory(),
        Err(_) => NativeResultPair::miss(),
    }
}

#[cfg(test)]
mod tests;
