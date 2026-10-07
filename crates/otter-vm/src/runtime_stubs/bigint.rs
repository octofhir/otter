//! BigInt binary operators for compiled code.
//!
//! # Contents
//! - [`bigint_binary_alloc`]: one allocating Probe over two BigInt operands
//!   and an [`crate::bigint::ops::Operator`] code (V8's `BigIntAdd` and
//!   friends: the kernel runs straight into the result, no runtime frame).
//!
//! # Invariants
//! - A non-BigInt operand and an operator failure (`RangeError`) are
//!   pre-effect Misses: the canonical operation re-executes and owns coercion
//!   and throwing.
//! - The operator roots its operands across its one result allocation; the
//!   caller's frame roots are published to that allocation's collection only,
//!   and its safepoint record is resolved only if the collection happens.
//!
//! # See also
//! - [`crate::bigint::ops`] owns the operators.

use super::*;
use crate::bigint::ops::{OpError, Operator};

/// `lhs <operator> rhs` for two BigInts.
#[must_use]
pub extern "C" fn bigint_binary_alloc(
    ctx: *mut RuntimeStubAllocContext,
    safepoint: SafepointId,
    lhs: u64,
    rhs: u64,
    operator: u64,
) -> NativeResultPair {
    let Some(ctx) = alloc_context_mut(ctx) else {
        return NativeResultPair::miss();
    };
    let Some(vm) = alloc_interpreter_mut(ctx) else {
        return NativeResultPair::miss();
    };
    let result = binary(ctx, vm, safepoint, [lhs, rhs, operator]);
    match result.validate(NativeResultDomain::Probe) {
        Some(status) => vm.record_jit_alloc_value_stub_status(status),
        None => return NativeResultPair::fatal_internal(),
    }
    result
}

fn binary(
    ctx: &RuntimeStubAllocContext,
    vm: &mut Interpreter,
    safepoint: SafepointId,
    [lhs, rhs, operator]: [u64; 3],
) -> NativeResultPair {
    let (Some(lhs), Some(rhs), Some(operator)) = (
        Value::from_abi_bits(lhs).as_big_int(),
        Value::from_abi_bits(rhs).as_big_int(),
        Value::from_abi_bits(operator)
            .as_i32()
            .and_then(Operator::decode),
    ) else {
        return NativeResultPair::miss();
    };
    // SAFETY: the exact current typed call packet and immutable safepoint
    // stay published until the result body is initialized.
    let Ok(frame_roots) = (unsafe { AllocSafepointFrameRoots::new(ctx, safepoint) }) else {
        return NativeResultPair::miss();
    };
    let mut roots = |visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
        otter_gc::ExtraRootSource::visit_extra_roots(&frame_roots, visitor);
    };
    match operator.function()(&mut vm.gc_heap, lhs, rhs, &mut roots) {
        Ok(value) => NativeResultPair::success(Value::big_int(value)),
        Err(OpError::OutOfMemory(_)) => NativeResultPair::out_of_memory(),
        Err(_) => NativeResultPair::miss(),
    }
}
