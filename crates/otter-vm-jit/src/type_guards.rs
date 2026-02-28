//! Type guard emission helpers for JIT-compiled code.
//!
//! Provides functions to emit NaN-boxing tag checks, unboxing/boxing operations,
//! truthiness checks, and guarded arithmetic/comparison fast paths in Cranelift IR.
//!
//! # Guard pattern
//!
//! Each guarded operation emits a diamond-shaped control flow:
//!
//! ```text
//!   [current block]
//!     │  type check
//!     ├─────────────┐
//!     ▼             ▼
//!   [fast path]   [slow path]  ← caller fills in via runtime helper call
//!     │             │
//!     └──────┬──────┘
//!            ▼
//!       [merge block]  ← block param carries result
//! ```

use cranelift_codegen::ir::condcodes::{FloatCC, IntCC};
use cranelift_codegen::ir::instructions::BlockArg;
use cranelift_codegen::ir::{Block, InstBuilder, MemFlags, Value, types};
use cranelift_frontend::FunctionBuilder;

// ---------------------------------------------------------------------------
// NaN-boxing constants (must match otter-vm-core/src/value.rs)
// ---------------------------------------------------------------------------

/// NaN-boxed int32 tag: high 32 bits for int32 values.
pub(crate) const TAG_INT32: i64 = 0x7FF8_0001_0000_0000_u64 as i64;

/// Mask to isolate the high 32 bits of a NaN-boxed value.
const INT32_TAG_MASK: i64 = 0xFFFF_FFFF_0000_0000_u64 as i64;

/// Mask to isolate the low 32 bits (payload) of a NaN-boxed int32.
const LOW32_MASK: i64 = 0x0000_0000_FFFF_FFFF_u64 as i64;

/// NaN-boxed `undefined`.
pub(crate) const TAG_UNDEFINED: i64 = 0x7FF8_0000_0000_0000_u64 as i64;

/// NaN-boxed `null`.
pub(crate) const TAG_NULL: i64 = 0x7FF8_0000_0000_0001_u64 as i64;

/// NaN-boxed `true`.
pub(crate) const TAG_TRUE: i64 = 0x7FF8_0000_0000_0002_u64 as i64;

/// NaN-boxed `false`.
pub(crate) const TAG_FALSE: i64 = 0x7FF8_0000_0000_0003_u64 as i64;

/// NaN-boxed canonical NaN.
pub(crate) const TAG_NAN: i64 = 0x7FFA_0000_0000_0000_u64 as i64;

// ---------------------------------------------------------------------------
// Primitive type checks
// ---------------------------------------------------------------------------

/// Emit: is this value a NaN-boxed int32?
///
/// Returns a Cranelift `i8` value (0 or 1).
pub(crate) fn emit_is_int32(builder: &mut FunctionBuilder, val: Value) -> Value {
    let mask = builder.ins().iconst(types::I64, INT32_TAG_MASK);
    let tag = builder.ins().band(val, mask);
    let expected = builder.ins().iconst(types::I64, TAG_INT32);
    builder.ins().icmp(IntCC::Equal, tag, expected)
}

/// Emit: are both values NaN-boxed int32?
///
/// Returns a Cranelift `i8` value (0 or 1).
pub(crate) fn emit_both_int32(builder: &mut FunctionBuilder, lhs: Value, rhs: Value) -> Value {
    let l = emit_is_int32(builder, lhs);
    let r = emit_is_int32(builder, rhs);
    builder.ins().band(l, r)
}

/// Quiet NaN prefix — values with `(bits & QUIET_NAN) == QUIET_NAN` are NaN-boxed tags.
/// Raw f64 values do NOT have this pattern (except canonical NaN, stored as TAG_NAN).
pub(crate) const QUIET_NAN: i64 = 0x7FF8_0000_0000_0000_u64 as i64;

// ---------------------------------------------------------------------------
// Float64 type checks
// ---------------------------------------------------------------------------

/// Emit: is this value a raw f64 (not a NaN-boxed tag)?
///
/// Returns a Cranelift `i8` value (0 or 1).
pub(crate) fn emit_is_float64(builder: &mut FunctionBuilder, val: Value) -> Value {
    let mask = builder.ins().iconst(types::I64, QUIET_NAN);
    let tag = builder.ins().band(val, mask);
    builder.ins().icmp(IntCC::NotEqual, tag, mask)
}

/// Emit: are both values raw f64?
///
/// Returns a Cranelift `i8` value (0 or 1).
pub(crate) fn emit_both_float64(builder: &mut FunctionBuilder, lhs: Value, rhs: Value) -> Value {
    let l = emit_is_float64(builder, lhs);
    let r = emit_is_float64(builder, rhs);
    builder.ins().band(l, r)
}

/// Emit: is this value any JS Number representation (int32, f64, NaN-tag)?
///
/// Returns a Cranelift `i8` value (0 or 1).
pub(crate) fn emit_is_number(builder: &mut FunctionBuilder, val: Value) -> Value {
    let is_i32 = emit_is_int32(builder, val);
    let is_f64 = emit_is_float64(builder, val);
    let is_nan_tag = builder.ins().icmp_imm(IntCC::Equal, val, TAG_NAN);
    let i32_or_f64 = builder.ins().bor(is_i32, is_f64);
    builder.ins().bor(i32_or_f64, is_nan_tag)
}

// ---------------------------------------------------------------------------
// Boxing / unboxing
// ---------------------------------------------------------------------------

/// Unbox an int32 from a NaN-boxed value.
///
/// Caller must ensure the value IS an int32 (via a prior guard check).
/// Returns a Cranelift `i32` value.
pub(crate) fn emit_unbox_int32(builder: &mut FunctionBuilder, val: Value) -> Value {
    builder.ins().ireduce(types::I32, val)
}

/// Box an `i32` Cranelift value into NaN-boxed int32 representation.
///
/// Returns a Cranelift `i64` NaN-boxed value.
pub(crate) fn emit_box_int32(builder: &mut FunctionBuilder, val_i32: Value) -> Value {
    let extended = builder.ins().uextend(types::I64, val_i32);
    let low_mask = builder.ins().iconst(types::I64, LOW32_MASK);
    let masked = builder.ins().band(extended, low_mask);
    let tag = builder.ins().iconst(types::I64, TAG_INT32);
    builder.ins().bor(tag, masked)
}

/// Box a compile-time i32 constant as a NaN-boxed int32.
pub(crate) fn emit_box_int32_const(builder: &mut FunctionBuilder, val: i32) -> Value {
    let bits = TAG_INT32 | ((val as u32) as i64);
    builder.ins().iconst(types::I64, bits)
}

// ---------------------------------------------------------------------------
// Truthiness
// ---------------------------------------------------------------------------

/// Emit a NaN-boxing-aware truthiness check.
///
/// Falsy values: `false`, `null`, `undefined`, `int32(0)`, `f64(0.0)`, `NaN`.
/// Everything else is truthy (including objects, non-empty strings, non-zero numbers).
///
/// Returns a Cranelift `i8` (0 = falsy, 1 = truthy).
pub(crate) fn emit_is_truthy(builder: &mut FunctionBuilder, val: Value) -> Value {
    let is_false = builder.ins().icmp_imm(IntCC::Equal, val, TAG_FALSE);
    let is_null = builder.ins().icmp_imm(IntCC::Equal, val, TAG_NULL);
    let is_undef = builder.ins().icmp_imm(IntCC::Equal, val, TAG_UNDEFINED);
    let is_zero_i32 = builder.ins().icmp_imm(IntCC::Equal, val, TAG_INT32); // int32(0) = TAG_INT32 | 0
    let is_zero_f64 = builder.ins().icmp_imm(IntCC::Equal, val, 0); // f64(0.0) = 0x0
    let is_nan = builder.ins().icmp_imm(IntCC::Equal, val, TAG_NAN);

    let f1 = builder.ins().bor(is_false, is_null);
    let f2 = builder.ins().bor(is_undef, is_zero_i32);
    let f3 = builder.ins().bor(is_zero_f64, is_nan);
    let f4 = builder.ins().bor(f1, f2);
    let is_falsy = builder.ins().bor(f4, f3);

    // truthy = !falsy
    let zero_i8 = builder.ins().iconst(types::I8, 0);
    builder.ins().icmp(IntCC::Equal, is_falsy, zero_i8)
}

/// Emit nullish check (`value === null || value === undefined`).
///
/// Returns a Cranelift `i8` (0 = not nullish, 1 = nullish).
pub(crate) fn emit_is_nullish(builder: &mut FunctionBuilder, val: Value) -> Value {
    let is_null = builder.ins().icmp_imm(IntCC::Equal, val, TAG_NULL);
    let is_undef = builder.ins().icmp_imm(IntCC::Equal, val, TAG_UNDEFINED);
    builder.ins().bor(is_null, is_undef)
}

// ---------------------------------------------------------------------------
// Boolean conversion
// ---------------------------------------------------------------------------

/// Convert a Cranelift `i8` condition (0/1) to a NaN-boxed boolean (TAG_TRUE/TAG_FALSE).
///
/// Uses the identity: `TAG_TRUE = TAG_FALSE - 1`, so `result = TAG_FALSE - zext(cond)`.
pub(crate) fn emit_bool_to_nanbox(builder: &mut FunctionBuilder, cond: Value) -> Value {
    let false_val = builder.ins().iconst(types::I64, TAG_FALSE);
    let cond_i64 = builder.ins().uextend(types::I64, cond);
    builder.ins().isub(false_val, cond_i64)
}

// ---------------------------------------------------------------------------
// Guarded arithmetic
// ---------------------------------------------------------------------------

/// Result of emitting a guarded operation.
///
/// The caller must:
/// 1. Switch to `slow_block` and emit the slow-path code (typically a runtime helper call)
/// 2. Jump from `slow_block` to `merge_block` with the slow-path result as a block param
/// 3. Switch to `merge_block` and continue — `result` holds the merged value
pub(crate) struct GuardedResult {
    /// Block where the fast and slow paths converge. Has one `i64` block parameter.
    pub merge_block: Block,
    /// Block for the slow (fallback) path. Caller must fill this in.
    pub slow_block: Block,
    /// The merged result value (block parameter of `merge_block`).
    pub result: Value,
}

/// Supported arithmetic operations for the i32/f64 fast path.
#[derive(Debug, Clone, Copy)]
pub(crate) enum ArithOp {
    /// Integer addition with overflow check.
    Add,
    /// Integer subtraction with overflow check.
    Sub,
    /// Integer multiplication with overflow check.
    Mul,
}

/// Emit a guarded i32 binary arithmetic operation.
///
/// Checks that both operands are NaN-boxed int32, unboxes them, performs the
/// operation with overflow detection, and reboxes the result. On type-check
/// failure or overflow, branches to `slow_block`.
///
/// The caller must fill in `slow_block` with a generic fallback (e.g., runtime
/// helper call) and jump to `merge_block` with the result.
pub(crate) fn emit_guarded_i32_arith(
    builder: &mut FunctionBuilder,
    op: ArithOp,
    lhs: Value,
    rhs: Value,
) -> GuardedResult {
    let i32_fast = builder.create_block();
    let box_block = builder.create_block();
    let slow_block = builder.create_block();
    let merge_block = builder.create_block();
    builder.append_block_param(merge_block, types::I64);

    // Guard: both operands must be int32
    let both = emit_both_int32(builder, lhs, rhs);
    builder.ins().brif(both, i32_fast, &[], slow_block, &[]);

    // Fast path: unbox, compute, check overflow
    builder.switch_to_block(i32_fast);
    let l32 = emit_unbox_int32(builder, lhs);
    let r32 = emit_unbox_int32(builder, rhs);
    let l64 = builder.ins().sextend(types::I64, l32);
    let r64 = builder.ins().sextend(types::I64, r32);

    let result_i64 = match op {
        ArithOp::Add => builder.ins().iadd(l64, r64),
        ArithOp::Sub => builder.ins().isub(l64, r64),
        ArithOp::Mul => builder.ins().imul(l64, r64),
    };

    // Overflow check: truncate to i32, sign-extend back, compare
    let result_i32 = builder.ins().ireduce(types::I32, result_i64);
    let check = builder.ins().sextend(types::I64, result_i32);
    let no_overflow = builder.ins().icmp(IntCC::Equal, result_i64, check);
    builder
        .ins()
        .brif(no_overflow, box_block, &[], slow_block, &[]);

    // Rebox the i32 result
    builder.switch_to_block(box_block);
    let boxed = emit_box_int32(builder, result_i32);
    builder.ins().jump(merge_block, &[BlockArg::Value(boxed)]);

    let result = builder.block_params(merge_block)[0];
    GuardedResult {
        merge_block,
        slow_block,
        result,
    }
}

/// Emit a guarded i32 comparison.
///
/// Checks that both operands are NaN-boxed int32, unboxes them, performs a
/// signed i32 comparison, and returns TAG_TRUE or TAG_FALSE.
///
/// On type-check failure, branches to `slow_block` for the generic path.
pub(crate) fn emit_guarded_i32_cmp(
    builder: &mut FunctionBuilder,
    cc: IntCC,
    lhs: Value,
    rhs: Value,
) -> GuardedResult {
    let i32_fast = builder.create_block();
    let slow_block = builder.create_block();
    let merge_block = builder.create_block();
    builder.append_block_param(merge_block, types::I64);

    let both = emit_both_int32(builder, lhs, rhs);
    builder.ins().brif(both, i32_fast, &[], slow_block, &[]);

    // Fast path: unbox and compare as signed i32
    builder.switch_to_block(i32_fast);
    let l32 = emit_unbox_int32(builder, lhs);
    let r32 = emit_unbox_int32(builder, rhs);
    let cmp = builder.ins().icmp(cc, l32, r32);
    let result = emit_bool_to_nanbox(builder, cmp);
    builder.ins().jump(merge_block, &[BlockArg::Value(result)]);

    let result_val = builder.block_params(merge_block)[0];
    GuardedResult {
        merge_block,
        slow_block,
        result: result_val,
    }
}

/// Emit guarded numeric binary arithmetic (`+`, `-`, `*`).
///
/// Fast path 1: int32 + int32 with overflow check.
/// Fast path 2: Number + Number (int32/f64/NaN-tag) via f64 arithmetic.
/// Slow path: non-number operands.
pub(crate) fn emit_guarded_numeric_arith(
    builder: &mut FunctionBuilder,
    op: ArithOp,
    lhs: Value,
    rhs: Value,
) -> GuardedResult {
    let i32_fast = builder.create_block();
    let i32_box = builder.create_block();
    let numeric_entry = builder.create_block();
    let lhs_i32_block = builder.create_block();
    let lhs_i32_convert = builder.create_block();
    let lhs_f64_block = builder.create_block();
    let rhs_i32_block = builder.create_block();
    let rhs_f64_block = builder.create_block();
    let compute_block = builder.create_block();
    let nan_block = builder.create_block();
    let slow_block = builder.create_block();
    let merge_block = builder.create_block();

    builder.append_block_param(rhs_i32_block, types::F64);
    builder.append_block_param(rhs_f64_block, types::F64);
    builder.append_block_param(compute_block, types::F64);
    builder.append_block_param(compute_block, types::F64);
    builder.append_block_param(merge_block, types::I64);

    let both_i32 = emit_both_int32(builder, lhs, rhs);
    builder
        .ins()
        .brif(both_i32, i32_fast, &[], numeric_entry, &[]);

    builder.switch_to_block(i32_fast);
    let l32 = emit_unbox_int32(builder, lhs);
    let r32 = emit_unbox_int32(builder, rhs);
    let l64 = builder.ins().sextend(types::I64, l32);
    let r64 = builder.ins().sextend(types::I64, r32);
    let result_i64 = match op {
        ArithOp::Add => builder.ins().iadd(l64, r64),
        ArithOp::Sub => builder.ins().isub(l64, r64),
        ArithOp::Mul => builder.ins().imul(l64, r64),
    };
    let result_i32 = builder.ins().ireduce(types::I32, result_i64);
    let check = builder.ins().sextend(types::I64, result_i32);
    let no_overflow = builder.ins().icmp(IntCC::Equal, result_i64, check);
    builder
        .ins()
        .brif(no_overflow, i32_box, &[], numeric_entry, &[]);

    builder.switch_to_block(i32_box);
    let boxed = emit_box_int32(builder, result_i32);
    builder.ins().jump(merge_block, &[BlockArg::Value(boxed)]);

    builder.switch_to_block(numeric_entry);
    let lhs_is_i32 = emit_is_int32(builder, lhs);
    let rhs_is_i32 = emit_is_int32(builder, rhs);
    let lhs_is_number = emit_is_number(builder, lhs);
    let rhs_is_number = emit_is_number(builder, rhs);
    let both_numbers = builder.ins().band(lhs_is_number, rhs_is_number);
    builder
        .ins()
        .brif(both_numbers, lhs_i32_block, &[], slow_block, &[]);

    builder.switch_to_block(lhs_i32_block);
    builder
        .ins()
        .brif(lhs_is_i32, lhs_i32_convert, &[], lhs_f64_block, &[]);

    builder.switch_to_block(lhs_i32_convert);
    let lhs_i32 = emit_unbox_int32(builder, lhs);
    let lhs_i64 = builder.ins().sextend(types::I64, lhs_i32);
    let lhs_num_f64 = builder.ins().fcvt_from_sint(types::F64, lhs_i64);
    builder.ins().brif(
        rhs_is_i32,
        rhs_i32_block,
        &[BlockArg::Value(lhs_num_f64)],
        rhs_f64_block,
        &[BlockArg::Value(lhs_num_f64)],
    );

    builder.switch_to_block(lhs_f64_block);
    let lhs_num_f64 = builder.ins().bitcast(types::F64, MemFlags::new(), lhs);
    builder.ins().brif(
        rhs_is_i32,
        rhs_i32_block,
        &[BlockArg::Value(lhs_num_f64)],
        rhs_f64_block,
        &[BlockArg::Value(lhs_num_f64)],
    );

    builder.switch_to_block(rhs_i32_block);
    let lhs_num_f64 = builder.block_params(rhs_i32_block)[0];
    let rhs_i32 = emit_unbox_int32(builder, rhs);
    let rhs_i64 = builder.ins().sextend(types::I64, rhs_i32);
    let rhs_num_f64 = builder.ins().fcvt_from_sint(types::F64, rhs_i64);
    builder.ins().jump(
        compute_block,
        &[BlockArg::Value(lhs_num_f64), BlockArg::Value(rhs_num_f64)],
    );

    builder.switch_to_block(rhs_f64_block);
    let lhs_num_f64 = builder.block_params(rhs_f64_block)[0];
    let rhs_num_f64 = builder.ins().bitcast(types::F64, MemFlags::new(), rhs);
    builder.ins().jump(
        compute_block,
        &[BlockArg::Value(lhs_num_f64), BlockArg::Value(rhs_num_f64)],
    );

    builder.switch_to_block(compute_block);
    let lhs_num_f64 = builder.block_params(compute_block)[0];
    let rhs_num_f64 = builder.block_params(compute_block)[1];
    let result_f64 = match op {
        ArithOp::Add => builder.ins().fadd(lhs_num_f64, rhs_num_f64),
        ArithOp::Sub => builder.ins().fsub(lhs_num_f64, rhs_num_f64),
        ArithOp::Mul => builder.ins().fmul(lhs_num_f64, rhs_num_f64),
    };
    let is_nan = builder
        .ins()
        .fcmp(FloatCC::Unordered, result_f64, result_f64);
    let result_bits = builder
        .ins()
        .bitcast(types::I64, MemFlags::new(), result_f64);
    builder.ins().brif(
        is_nan,
        nan_block,
        &[],
        merge_block,
        &[BlockArg::Value(result_bits)],
    );

    builder.switch_to_block(nan_block);
    let nan_val = builder.ins().iconst(types::I64, TAG_NAN);
    builder.ins().jump(merge_block, &[BlockArg::Value(nan_val)]);

    let result = builder.block_params(merge_block)[0];
    GuardedResult {
        merge_block,
        slow_block,
        result,
    }
}

/// Emit guarded numeric comparison.
///
/// Fast path 1: int32 + int32 using `int_cc`.
/// Fast path 2: Number + Number using `float_cc`.
/// Slow path: non-number operands.
pub(crate) fn emit_guarded_numeric_cmp(
    builder: &mut FunctionBuilder,
    int_cc: IntCC,
    float_cc: FloatCC,
    lhs: Value,
    rhs: Value,
) -> GuardedResult {
    let i32_fast = builder.create_block();
    let numeric_entry = builder.create_block();
    let lhs_i32_block = builder.create_block();
    let lhs_i32_convert = builder.create_block();
    let lhs_f64_block = builder.create_block();
    let rhs_i32_block = builder.create_block();
    let rhs_f64_block = builder.create_block();
    let compare_block = builder.create_block();
    let slow_block = builder.create_block();
    let merge_block = builder.create_block();

    builder.append_block_param(rhs_i32_block, types::F64);
    builder.append_block_param(rhs_f64_block, types::F64);
    builder.append_block_param(compare_block, types::F64);
    builder.append_block_param(compare_block, types::F64);
    builder.append_block_param(merge_block, types::I64);

    let both_i32 = emit_both_int32(builder, lhs, rhs);
    builder
        .ins()
        .brif(both_i32, i32_fast, &[], numeric_entry, &[]);

    builder.switch_to_block(i32_fast);
    let l32 = emit_unbox_int32(builder, lhs);
    let r32 = emit_unbox_int32(builder, rhs);
    let cmp = builder.ins().icmp(int_cc, l32, r32);
    let i32_result = emit_bool_to_nanbox(builder, cmp);
    builder
        .ins()
        .jump(merge_block, &[BlockArg::Value(i32_result)]);

    builder.switch_to_block(numeric_entry);
    let lhs_is_i32 = emit_is_int32(builder, lhs);
    let rhs_is_i32 = emit_is_int32(builder, rhs);
    let lhs_is_number = emit_is_number(builder, lhs);
    let rhs_is_number = emit_is_number(builder, rhs);
    let both_numbers = builder.ins().band(lhs_is_number, rhs_is_number);
    builder
        .ins()
        .brif(both_numbers, lhs_i32_block, &[], slow_block, &[]);

    builder.switch_to_block(lhs_i32_block);
    builder
        .ins()
        .brif(lhs_is_i32, lhs_i32_convert, &[], lhs_f64_block, &[]);

    builder.switch_to_block(lhs_i32_convert);
    let lhs_i32 = emit_unbox_int32(builder, lhs);
    let lhs_i64 = builder.ins().sextend(types::I64, lhs_i32);
    let lhs_num_f64 = builder.ins().fcvt_from_sint(types::F64, lhs_i64);
    builder.ins().brif(
        rhs_is_i32,
        rhs_i32_block,
        &[BlockArg::Value(lhs_num_f64)],
        rhs_f64_block,
        &[BlockArg::Value(lhs_num_f64)],
    );

    builder.switch_to_block(lhs_f64_block);
    let lhs_num_f64 = builder.ins().bitcast(types::F64, MemFlags::new(), lhs);
    builder.ins().brif(
        rhs_is_i32,
        rhs_i32_block,
        &[BlockArg::Value(lhs_num_f64)],
        rhs_f64_block,
        &[BlockArg::Value(lhs_num_f64)],
    );

    builder.switch_to_block(rhs_i32_block);
    let lhs_num_f64 = builder.block_params(rhs_i32_block)[0];
    let rhs_i32 = emit_unbox_int32(builder, rhs);
    let rhs_i64 = builder.ins().sextend(types::I64, rhs_i32);
    let rhs_num_f64 = builder.ins().fcvt_from_sint(types::F64, rhs_i64);
    builder.ins().jump(
        compare_block,
        &[BlockArg::Value(lhs_num_f64), BlockArg::Value(rhs_num_f64)],
    );

    builder.switch_to_block(rhs_f64_block);
    let lhs_num_f64 = builder.block_params(rhs_f64_block)[0];
    let rhs_num_f64 = builder.ins().bitcast(types::F64, MemFlags::new(), rhs);
    builder.ins().jump(
        compare_block,
        &[BlockArg::Value(lhs_num_f64), BlockArg::Value(rhs_num_f64)],
    );

    builder.switch_to_block(compare_block);
    let lhs_num_f64 = builder.block_params(compare_block)[0];
    let rhs_num_f64 = builder.block_params(compare_block)[1];
    let cmp = builder.ins().fcmp(float_cc, lhs_num_f64, rhs_num_f64);
    let numeric_result = emit_bool_to_nanbox(builder, cmp);
    builder
        .ins()
        .jump(merge_block, &[BlockArg::Value(numeric_result)]);

    let result = builder.block_params(merge_block)[0];
    GuardedResult {
        merge_block,
        slow_block,
        result,
    }
}

/// Emit guarded numeric division.
///
/// Fast path 1: exact int32 division.
/// Fast path 2: Number + Number via f64 division.
/// Slow path: non-number operands.
pub(crate) fn emit_guarded_numeric_div(
    builder: &mut FunctionBuilder,
    lhs: Value,
    rhs: Value,
) -> GuardedResult {
    let i32_fast = builder.create_block();
    let i32_overflow_check = builder.create_block();
    let i32_exact_check = builder.create_block();
    let i32_box = builder.create_block();
    let numeric_entry = builder.create_block();
    let lhs_i32_block = builder.create_block();
    let lhs_i32_convert = builder.create_block();
    let lhs_f64_block = builder.create_block();
    let rhs_i32_block = builder.create_block();
    let rhs_f64_block = builder.create_block();
    let compute_block = builder.create_block();
    let nan_block = builder.create_block();
    let slow_block = builder.create_block();
    let merge_block = builder.create_block();

    builder.append_block_param(rhs_i32_block, types::F64);
    builder.append_block_param(rhs_f64_block, types::F64);
    builder.append_block_param(compute_block, types::F64);
    builder.append_block_param(compute_block, types::F64);
    builder.append_block_param(merge_block, types::I64);

    let both_i32 = emit_both_int32(builder, lhs, rhs);
    builder
        .ins()
        .brif(both_i32, i32_fast, &[], numeric_entry, &[]);

    builder.switch_to_block(i32_fast);
    let l32 = emit_unbox_int32(builder, lhs);
    let r32 = emit_unbox_int32(builder, rhs);
    let zero = builder.ins().iconst(types::I32, 0);
    let rhs_nonzero = builder.ins().icmp(IntCC::NotEqual, r32, zero);
    builder
        .ins()
        .brif(rhs_nonzero, i32_overflow_check, &[], numeric_entry, &[]);

    builder.switch_to_block(i32_overflow_check);
    let neg_one = builder.ins().iconst(types::I32, -1);
    let int_min = builder.ins().iconst(types::I32, i32::MIN as i64);
    let rhs_is_neg_one = builder.ins().icmp(IntCC::Equal, r32, neg_one);
    let lhs_is_int_min = builder.ins().icmp(IntCC::Equal, l32, int_min);
    let overflow_pair = builder.ins().band(rhs_is_neg_one, lhs_is_int_min);
    builder
        .ins()
        .brif(overflow_pair, numeric_entry, &[], i32_exact_check, &[]);

    builder.switch_to_block(i32_exact_check);
    let remainder = builder.ins().srem(l32, r32);
    let exact = builder.ins().icmp(IntCC::Equal, remainder, zero);
    builder.ins().brif(exact, i32_box, &[], numeric_entry, &[]);

    builder.switch_to_block(i32_box);
    let quotient = builder.ins().sdiv(l32, r32);
    let boxed = emit_box_int32(builder, quotient);
    builder.ins().jump(merge_block, &[BlockArg::Value(boxed)]);

    builder.switch_to_block(numeric_entry);
    let lhs_is_i32 = emit_is_int32(builder, lhs);
    let rhs_is_i32 = emit_is_int32(builder, rhs);
    let lhs_is_number = emit_is_number(builder, lhs);
    let rhs_is_number = emit_is_number(builder, rhs);
    let both_numbers = builder.ins().band(lhs_is_number, rhs_is_number);
    builder
        .ins()
        .brif(both_numbers, lhs_i32_block, &[], slow_block, &[]);

    builder.switch_to_block(lhs_i32_block);
    builder
        .ins()
        .brif(lhs_is_i32, lhs_i32_convert, &[], lhs_f64_block, &[]);

    builder.switch_to_block(lhs_i32_convert);
    let lhs_i32 = emit_unbox_int32(builder, lhs);
    let lhs_i64 = builder.ins().sextend(types::I64, lhs_i32);
    let lhs_num_f64 = builder.ins().fcvt_from_sint(types::F64, lhs_i64);
    builder.ins().brif(
        rhs_is_i32,
        rhs_i32_block,
        &[BlockArg::Value(lhs_num_f64)],
        rhs_f64_block,
        &[BlockArg::Value(lhs_num_f64)],
    );

    builder.switch_to_block(lhs_f64_block);
    let lhs_num_f64 = builder.ins().bitcast(types::F64, MemFlags::new(), lhs);
    builder.ins().brif(
        rhs_is_i32,
        rhs_i32_block,
        &[BlockArg::Value(lhs_num_f64)],
        rhs_f64_block,
        &[BlockArg::Value(lhs_num_f64)],
    );

    builder.switch_to_block(rhs_i32_block);
    let lhs_num_f64 = builder.block_params(rhs_i32_block)[0];
    let rhs_i32 = emit_unbox_int32(builder, rhs);
    let rhs_i64 = builder.ins().sextend(types::I64, rhs_i32);
    let rhs_num_f64 = builder.ins().fcvt_from_sint(types::F64, rhs_i64);
    builder.ins().jump(
        compute_block,
        &[BlockArg::Value(lhs_num_f64), BlockArg::Value(rhs_num_f64)],
    );

    builder.switch_to_block(rhs_f64_block);
    let lhs_num_f64 = builder.block_params(rhs_f64_block)[0];
    let rhs_num_f64 = builder.ins().bitcast(types::F64, MemFlags::new(), rhs);
    builder.ins().jump(
        compute_block,
        &[BlockArg::Value(lhs_num_f64), BlockArg::Value(rhs_num_f64)],
    );

    builder.switch_to_block(compute_block);
    let lhs_num_f64 = builder.block_params(compute_block)[0];
    let rhs_num_f64 = builder.block_params(compute_block)[1];
    let result_f64 = builder.ins().fdiv(lhs_num_f64, rhs_num_f64);
    let is_nan = builder
        .ins()
        .fcmp(FloatCC::Unordered, result_f64, result_f64);
    let result_bits = builder
        .ins()
        .bitcast(types::I64, MemFlags::new(), result_f64);
    builder.ins().brif(
        is_nan,
        nan_block,
        &[],
        merge_block,
        &[BlockArg::Value(result_bits)],
    );

    builder.switch_to_block(nan_block);
    let nan_val = builder.ins().iconst(types::I64, TAG_NAN);
    builder.ins().jump(merge_block, &[BlockArg::Value(nan_val)]);

    let result = builder.block_params(merge_block)[0];
    GuardedResult {
        merge_block,
        slow_block,
        result,
    }
}

/// Emit a correct strict equality (`===` / `!==`) check.
///
/// Handles all Number representation combinations:
/// - int32 vs int32
/// - f64 vs f64 (including `+0.0` / `-0.0`)
/// - int32 vs f64 (e.g. `1 === 1.0`)
///
/// Also preserves `NaN !== NaN`.
pub(crate) fn emit_strict_eq(
    builder: &mut FunctionBuilder,
    lhs: Value,
    rhs: Value,
    negated: bool,
) -> Value {
    let same_block = builder.create_block();
    let diff_block = builder.create_block();
    let numeric_block = builder.create_block();
    let lhs_i32_block = builder.create_block();
    let lhs_f64_block = builder.create_block();
    let rhs_i32_block = builder.create_block();
    let rhs_f64_block = builder.create_block();
    let rhs_compare_block = builder.create_block();
    let not_equal_block = builder.create_block();
    let merge_block = builder.create_block();

    // Blocks carrying converted numeric values.
    builder.append_block_param(rhs_i32_block, types::F64);
    builder.append_block_param(rhs_f64_block, types::F64);
    builder.append_block_param(rhs_compare_block, types::F64);
    builder.append_block_param(rhs_compare_block, types::F64);
    builder.append_block_param(merge_block, types::I64);

    // Step 1: are the bits identical?
    let bits_equal = builder.ins().icmp(IntCC::Equal, lhs, rhs);
    builder
        .ins()
        .brif(bits_equal, same_block, &[], diff_block, &[]);

    // Same bits: equal UNLESS both are NaN
    builder.switch_to_block(same_block);
    let is_nan = builder.ins().icmp_imm(IntCC::Equal, lhs, TAG_NAN);
    let same_result = if negated {
        // !== : same bits → false, unless NaN → true
        emit_bool_to_nanbox(builder, is_nan)
    } else {
        // === : same bits → true, unless NaN → false
        let zero_i8 = builder.ins().iconst(types::I8, 0);
        let not_nan = builder.ins().icmp(IntCC::Equal, is_nan, zero_i8);
        emit_bool_to_nanbox(builder, not_nan)
    };
    builder
        .ins()
        .jump(merge_block, &[BlockArg::Value(same_result)]);

    // Different bits: if both are Numbers (int32/f64), compare numerically.
    // Otherwise strict equality is false.
    builder.switch_to_block(diff_block);
    let lhs_is_i32 = emit_is_int32(builder, lhs);
    let rhs_is_i32 = emit_is_int32(builder, rhs);
    let lhs_is_f64 = emit_is_float64(builder, lhs);
    let rhs_is_f64 = emit_is_float64(builder, rhs);
    let lhs_is_number = builder.ins().bor(lhs_is_i32, lhs_is_f64);
    let rhs_is_number = builder.ins().bor(rhs_is_i32, rhs_is_f64);
    let both_numbers = builder.ins().band(lhs_is_number, rhs_is_number);
    builder
        .ins()
        .brif(both_numbers, numeric_block, &[], not_equal_block, &[]);

    // Convert lhs to f64.
    builder.switch_to_block(numeric_block);
    builder
        .ins()
        .brif(lhs_is_i32, lhs_i32_block, &[], lhs_f64_block, &[]);

    builder.switch_to_block(lhs_i32_block);
    let lhs_i32 = emit_unbox_int32(builder, lhs);
    let lhs_i64 = builder.ins().sextend(types::I64, lhs_i32);
    let lhs_num_f64 = builder.ins().fcvt_from_sint(types::F64, lhs_i64);
    builder.ins().brif(
        rhs_is_i32,
        rhs_i32_block,
        &[BlockArg::Value(lhs_num_f64)],
        rhs_f64_block,
        &[BlockArg::Value(lhs_num_f64)],
    );

    builder.switch_to_block(lhs_f64_block);
    let lhs_num_f64 = builder.ins().bitcast(types::F64, MemFlags::new(), lhs);
    builder.ins().brif(
        rhs_is_i32,
        rhs_i32_block,
        &[BlockArg::Value(lhs_num_f64)],
        rhs_f64_block,
        &[BlockArg::Value(lhs_num_f64)],
    );

    // Convert rhs to f64 with lhs carried through as a block parameter.
    builder.switch_to_block(rhs_i32_block);
    let lhs_num_f64 = builder.block_params(rhs_i32_block)[0];
    let rhs_i32 = emit_unbox_int32(builder, rhs);
    let rhs_i64 = builder.ins().sextend(types::I64, rhs_i32);
    let rhs_num_f64 = builder.ins().fcvt_from_sint(types::F64, rhs_i64);
    builder.ins().jump(
        rhs_compare_block,
        &[BlockArg::Value(lhs_num_f64), BlockArg::Value(rhs_num_f64)],
    );

    builder.switch_to_block(rhs_f64_block);
    let lhs_num_f64 = builder.block_params(rhs_f64_block)[0];
    let rhs_num_f64 = builder.ins().bitcast(types::F64, MemFlags::new(), rhs);
    builder.ins().jump(
        rhs_compare_block,
        &[BlockArg::Value(lhs_num_f64), BlockArg::Value(rhs_num_f64)],
    );

    builder.switch_to_block(rhs_compare_block);
    let lhs_num_f64 = builder.block_params(rhs_compare_block)[0];
    let rhs_num_f64 = builder.block_params(rhs_compare_block)[1];
    let float_cc = if negated {
        FloatCC::NotEqual
    } else {
        FloatCC::Equal
    };
    let fcmp_result = builder.ins().fcmp(float_cc, lhs_num_f64, rhs_num_f64);
    let numeric_result = emit_bool_to_nanbox(builder, fcmp_result);
    builder
        .ins()
        .jump(merge_block, &[BlockArg::Value(numeric_result)]);

    // Not both f64, different bits → not equal
    builder.switch_to_block(not_equal_block);
    let ne_val = builder
        .ins()
        .iconst(types::I64, if negated { TAG_TRUE } else { TAG_FALSE });
    builder.ins().jump(merge_block, &[BlockArg::Value(ne_val)]);

    builder.switch_to_block(merge_block);
    builder.block_params(merge_block)[0]
}

/// Emit a guarded i32 division.
///
/// Division is special: JS division always produces f64 (`7 / 2 === 3.5`).
/// We check for the exact-division case (result * rhs == lhs) and only take
/// the fast path then.
pub(crate) fn emit_guarded_i32_div(
    builder: &mut FunctionBuilder,
    lhs: Value,
    rhs: Value,
) -> GuardedResult {
    let i32_fast = builder.create_block();
    let overflow_check = builder.create_block();
    let div_check = builder.create_block();
    let box_block = builder.create_block();
    let slow_block = builder.create_block();
    let merge_block = builder.create_block();
    builder.append_block_param(merge_block, types::I64);

    // Guard: both must be int32
    let both = emit_both_int32(builder, lhs, rhs);
    builder.ins().brif(both, i32_fast, &[], slow_block, &[]);

    builder.switch_to_block(i32_fast);
    let l32 = emit_unbox_int32(builder, lhs);
    let r32 = emit_unbox_int32(builder, rhs);
    // Check rhs != 0 (avoid div-by-zero trap)
    let zero = builder.ins().iconst(types::I32, 0);
    let rhs_nonzero = builder.ins().icmp(IntCC::NotEqual, r32, zero);
    builder
        .ins()
        .brif(rhs_nonzero, overflow_check, &[], slow_block, &[]);

    // Prevent signed div/mod overflow trap on INT_MIN / -1.
    builder.switch_to_block(overflow_check);
    let neg_one = builder.ins().iconst(types::I32, -1);
    let int_min = builder.ins().iconst(types::I32, i32::MIN as i64);
    let rhs_is_neg_one = builder.ins().icmp(IntCC::Equal, r32, neg_one);
    let lhs_is_int_min = builder.ins().icmp(IntCC::Equal, l32, int_min);
    let overflow_pair = builder.ins().band(rhs_is_neg_one, lhs_is_int_min);
    builder
        .ins()
        .brif(overflow_pair, slow_block, &[], div_check, &[]);

    builder.switch_to_block(div_check);
    // Check for exact division: lhs % rhs == 0
    let remainder = builder.ins().srem(l32, r32);
    let exact = builder.ins().icmp(IntCC::Equal, remainder, zero);
    builder.ins().brif(exact, box_block, &[], slow_block, &[]);

    builder.switch_to_block(box_block);
    let quotient = builder.ins().sdiv(l32, r32);
    let boxed = emit_box_int32(builder, quotient);
    builder.ins().jump(merge_block, &[BlockArg::Value(boxed)]);

    let result = builder.block_params(merge_block)[0];
    GuardedResult {
        merge_block,
        slow_block,
        result,
    }
}

/// Emit a guarded i32 modulo.
///
/// Checks both operands are int32 and rhs != 0.
/// JS `%` has sign-of-dividend semantics matching Cranelift `srem`.
pub(crate) fn emit_guarded_i32_mod(
    builder: &mut FunctionBuilder,
    lhs: Value,
    rhs: Value,
) -> GuardedResult {
    let i32_fast = builder.create_block();
    let overflow_check = builder.create_block();
    let safe_block = builder.create_block();
    let slow_block = builder.create_block();
    let merge_block = builder.create_block();
    builder.append_block_param(merge_block, types::I64);

    let both = emit_both_int32(builder, lhs, rhs);
    builder.ins().brif(both, i32_fast, &[], slow_block, &[]);

    builder.switch_to_block(i32_fast);
    let l32 = emit_unbox_int32(builder, lhs);
    let r32 = emit_unbox_int32(builder, rhs);
    // Check rhs != 0
    let zero = builder.ins().iconst(types::I32, 0);
    let rhs_nonzero = builder.ins().icmp(IntCC::NotEqual, r32, zero);
    builder
        .ins()
        .brif(rhs_nonzero, overflow_check, &[], slow_block, &[]);

    // Prevent signed remainder overflow trap on INT_MIN % -1.
    builder.switch_to_block(overflow_check);
    let neg_one = builder.ins().iconst(types::I32, -1);
    let int_min = builder.ins().iconst(types::I32, i32::MIN as i64);
    let rhs_is_neg_one = builder.ins().icmp(IntCC::Equal, r32, neg_one);
    let lhs_is_int_min = builder.ins().icmp(IntCC::Equal, l32, int_min);
    let overflow_pair = builder.ins().band(rhs_is_neg_one, lhs_is_int_min);
    builder
        .ins()
        .brif(overflow_pair, slow_block, &[], safe_block, &[]);

    builder.switch_to_block(safe_block);
    let remainder = builder.ins().srem(l32, r32);
    let boxed = emit_box_int32(builder, remainder);
    builder.ins().jump(merge_block, &[BlockArg::Value(boxed)]);

    let result = builder.block_params(merge_block)[0];
    GuardedResult {
        merge_block,
        slow_block,
        result,
    }
}

// ---------------------------------------------------------------------------
// Float64 guarded arithmetic
// ---------------------------------------------------------------------------

/// Emit a guarded f64 binary arithmetic operation.
///
/// Checks that both operands are raw f64 (not NaN-boxed), performs the
/// float operation, and canonicalizes NaN results to TAG_NAN.
///
/// The caller must fill in `slow_block` with a generic fallback.
pub(crate) fn emit_guarded_f64_arith(
    builder: &mut FunctionBuilder,
    op: ArithOp,
    lhs: Value,
    rhs: Value,
) -> GuardedResult {
    let f64_fast = builder.create_block();
    let nan_block = builder.create_block();
    let slow_block = builder.create_block();
    let merge_block = builder.create_block();
    builder.append_block_param(merge_block, types::I64);

    // Guard: both operands must be raw f64
    let both = emit_both_float64(builder, lhs, rhs);
    builder.ins().brif(both, f64_fast, &[], slow_block, &[]);

    // Fast path: bitcast to f64, compute, canonicalize NaN
    builder.switch_to_block(f64_fast);
    let l_f64 = builder.ins().bitcast(types::F64, MemFlags::new(), lhs);
    let r_f64 = builder.ins().bitcast(types::F64, MemFlags::new(), rhs);

    let result_f64 = match op {
        ArithOp::Add => builder.ins().fadd(l_f64, r_f64),
        ArithOp::Sub => builder.ins().fsub(l_f64, r_f64),
        ArithOp::Mul => builder.ins().fmul(l_f64, r_f64),
    };

    // NaN canonicalization: hardware NaN bits could collide with TAG_UNDEFINED
    let is_nan = builder
        .ins()
        .fcmp(FloatCC::Unordered, result_f64, result_f64);
    let result_bits = builder
        .ins()
        .bitcast(types::I64, MemFlags::new(), result_f64);
    builder.ins().brif(
        is_nan,
        nan_block,
        &[],
        merge_block,
        &[BlockArg::Value(result_bits)],
    );

    builder.switch_to_block(nan_block);
    let nan_val = builder.ins().iconst(types::I64, TAG_NAN);
    builder.ins().jump(merge_block, &[BlockArg::Value(nan_val)]);

    let result = builder.block_params(merge_block)[0];
    GuardedResult {
        merge_block,
        slow_block,
        result,
    }
}

/// Emit a guarded f64 division.
///
/// Same as `emit_guarded_f64_arith` but uses `fdiv`. JS division always
/// returns f64, so this is the natural fast path for division.
pub(crate) fn emit_guarded_f64_div(
    builder: &mut FunctionBuilder,
    lhs: Value,
    rhs: Value,
) -> GuardedResult {
    let f64_fast = builder.create_block();
    let nan_block = builder.create_block();
    let slow_block = builder.create_block();
    let merge_block = builder.create_block();
    builder.append_block_param(merge_block, types::I64);

    let both = emit_both_float64(builder, lhs, rhs);
    builder.ins().brif(both, f64_fast, &[], slow_block, &[]);

    builder.switch_to_block(f64_fast);
    let l_f64 = builder.ins().bitcast(types::F64, MemFlags::new(), lhs);
    let r_f64 = builder.ins().bitcast(types::F64, MemFlags::new(), rhs);
    let result_f64 = builder.ins().fdiv(l_f64, r_f64);

    let is_nan = builder
        .ins()
        .fcmp(FloatCC::Unordered, result_f64, result_f64);
    let result_bits = builder
        .ins()
        .bitcast(types::I64, MemFlags::new(), result_f64);
    builder.ins().brif(
        is_nan,
        nan_block,
        &[],
        merge_block,
        &[BlockArg::Value(result_bits)],
    );

    builder.switch_to_block(nan_block);
    let nan_val = builder.ins().iconst(types::I64, TAG_NAN);
    builder.ins().jump(merge_block, &[BlockArg::Value(nan_val)]);

    let result = builder.block_params(merge_block)[0];
    GuardedResult {
        merge_block,
        slow_block,
        result,
    }
}

/// Emit a guarded f64 comparison.
///
/// Checks that both operands are raw f64, then uses `fcmp` which handles
/// NaN correctly (NaN comparisons return false for ordered comparisons).
#[allow(dead_code)]
pub(crate) fn emit_guarded_f64_cmp(
    builder: &mut FunctionBuilder,
    cc: FloatCC,
    lhs: Value,
    rhs: Value,
) -> GuardedResult {
    let f64_fast = builder.create_block();
    let slow_block = builder.create_block();
    let merge_block = builder.create_block();
    builder.append_block_param(merge_block, types::I64);

    let both = emit_both_float64(builder, lhs, rhs);
    builder.ins().brif(both, f64_fast, &[], slow_block, &[]);

    builder.switch_to_block(f64_fast);
    let l_f64 = builder.ins().bitcast(types::F64, MemFlags::new(), lhs);
    let r_f64 = builder.ins().bitcast(types::F64, MemFlags::new(), rhs);
    let cmp = builder.ins().fcmp(cc, l_f64, r_f64);
    let result = emit_bool_to_nanbox(builder, cmp);
    builder.ins().jump(merge_block, &[BlockArg::Value(result)]);

    let result_val = builder.block_params(merge_block)[0];
    GuardedResult {
        merge_block,
        slow_block,
        result: result_val,
    }
}

// ---------------------------------------------------------------------------
// Unary operations
// ---------------------------------------------------------------------------

/// Emit a guarded i32 negation.
///
/// Checks that the operand is NaN-boxed int32, unboxes, negates with overflow
/// check (INT_MIN case), and reboxes.
pub(crate) fn emit_guarded_i32_neg(builder: &mut FunctionBuilder, val: Value) -> GuardedResult {
    let i32_fast = builder.create_block();
    let box_block = builder.create_block();
    let slow_block = builder.create_block();
    let merge_block = builder.create_block();
    builder.append_block_param(merge_block, types::I64);

    let is_i32 = emit_is_int32(builder, val);
    builder.ins().brif(is_i32, i32_fast, &[], slow_block, &[]);

    builder.switch_to_block(i32_fast);
    let v32 = emit_unbox_int32(builder, val);
    // Check for INT_MIN (-2^31) which overflows on negation
    let int_min = builder.ins().iconst(types::I32, i32::MIN as i64);
    let not_min = builder.ins().icmp(IntCC::NotEqual, v32, int_min);
    builder.ins().brif(not_min, box_block, &[], slow_block, &[]);

    builder.switch_to_block(box_block);
    let negated = builder.ins().ineg(v32);
    let boxed = emit_box_int32(builder, negated);
    builder.ins().jump(merge_block, &[BlockArg::Value(boxed)]);

    let result = builder.block_params(merge_block)[0];
    GuardedResult {
        merge_block,
        slow_block,
        result,
    }
}

/// Emit a guarded i32 increment (value + 1).
///
/// Checks that the operand is NaN-boxed int32 and not INT_MAX (overflow).
pub(crate) fn emit_guarded_i32_inc(builder: &mut FunctionBuilder, val: Value) -> GuardedResult {
    let i32_fast = builder.create_block();
    let box_block = builder.create_block();
    let slow_block = builder.create_block();
    let merge_block = builder.create_block();
    builder.append_block_param(merge_block, types::I64);

    let is_i32 = emit_is_int32(builder, val);
    builder.ins().brif(is_i32, i32_fast, &[], slow_block, &[]);

    builder.switch_to_block(i32_fast);
    let v32 = emit_unbox_int32(builder, val);
    let int_max = builder.ins().iconst(types::I32, i32::MAX as i64);
    let not_max = builder.ins().icmp(IntCC::NotEqual, v32, int_max);
    builder.ins().brif(not_max, box_block, &[], slow_block, &[]);

    builder.switch_to_block(box_block);
    let one = builder.ins().iconst(types::I32, 1);
    let result = builder.ins().iadd(v32, one);
    let boxed = emit_box_int32(builder, result);
    builder.ins().jump(merge_block, &[BlockArg::Value(boxed)]);

    let result_val = builder.block_params(merge_block)[0];
    GuardedResult {
        merge_block,
        slow_block,
        result: result_val,
    }
}

/// Emit a guarded i32 decrement (value - 1).
///
/// Checks that the operand is NaN-boxed int32 and not INT_MIN (overflow).
pub(crate) fn emit_guarded_i32_dec(builder: &mut FunctionBuilder, val: Value) -> GuardedResult {
    let i32_fast = builder.create_block();
    let box_block = builder.create_block();
    let slow_block = builder.create_block();
    let merge_block = builder.create_block();
    builder.append_block_param(merge_block, types::I64);

    let is_i32 = emit_is_int32(builder, val);
    builder.ins().brif(is_i32, i32_fast, &[], slow_block, &[]);

    builder.switch_to_block(i32_fast);
    let v32 = emit_unbox_int32(builder, val);
    let int_min = builder.ins().iconst(types::I32, i32::MIN as i64);
    let not_min = builder.ins().icmp(IntCC::NotEqual, v32, int_min);
    builder.ins().brif(not_min, box_block, &[], slow_block, &[]);

    builder.switch_to_block(box_block);
    let one = builder.ins().iconst(types::I32, 1);
    let result = builder.ins().isub(v32, one);
    let boxed = emit_box_int32(builder, result);
    builder.ins().jump(merge_block, &[BlockArg::Value(boxed)]);

    let result_val = builder.block_params(merge_block)[0];
    GuardedResult {
        merge_block,
        slow_block,
        result: result_val,
    }
}

// ---------------------------------------------------------------------------
// Bitwise operations
// ---------------------------------------------------------------------------

/// Convert a numeric JS value (int32/f64/NaN-tag) to int32.
///
/// This matches the runtime's current `ToInt32` behavior:
/// - `NaN` / `±Infinity` => 0
/// - finite number => trunc toward zero, then wrap via low 32 bits
/// - int32 values pass through unchanged
fn emit_number_to_int32(builder: &mut FunctionBuilder, val: Value) -> Value {
    let i32_block = builder.create_block();
    let number_block = builder.create_block();
    let finite_block = builder.create_block();
    let zero_block = builder.create_block();
    let merge_block = builder.create_block();
    builder.append_block_param(merge_block, types::I32);

    let is_i32 = emit_is_int32(builder, val);
    builder
        .ins()
        .brif(is_i32, i32_block, &[], number_block, &[]);

    builder.switch_to_block(i32_block);
    let unboxed = emit_unbox_int32(builder, val);
    builder.ins().jump(merge_block, &[BlockArg::Value(unboxed)]);

    builder.switch_to_block(number_block);
    let num = builder.ins().bitcast(types::F64, MemFlags::new(), val);
    let is_nan = builder.ins().fcmp(FloatCC::Unordered, num, num);
    let abs_num = builder.ins().fabs(num);
    let inf_bits = builder
        .ins()
        .iconst(types::I64, f64::INFINITY.to_bits() as i64);
    let inf = builder.ins().bitcast(types::F64, MemFlags::new(), inf_bits);
    let is_inf = builder.ins().fcmp(FloatCC::Equal, abs_num, inf);
    let non_finite = builder.ins().bor(is_nan, is_inf);
    builder
        .ins()
        .brif(non_finite, zero_block, &[], finite_block, &[]);

    builder.switch_to_block(zero_block);
    let zero_i32 = builder.ins().iconst(types::I32, 0);
    builder
        .ins()
        .jump(merge_block, &[BlockArg::Value(zero_i32)]);

    builder.switch_to_block(finite_block);
    let int64 = builder.ins().fcvt_to_sint_sat(types::I64, num);
    let int32 = builder.ins().ireduce(types::I32, int64);
    builder.ins().jump(merge_block, &[BlockArg::Value(int32)]);

    builder.switch_to_block(merge_block);
    builder.block_params(merge_block)[0]
}

/// Emit guarded numeric bitwise operation.
///
/// JS bitwise ops convert operands with `ToInt32`/`ToUint32`; this fast path
/// accepts numeric operands (int32/f64/NaN-tag), applies conversion, executes
/// bitwise op, and returns either int32 boxed value or f64 for `>>>` high-bit
/// results that exceed int32 range.
pub(crate) fn emit_guarded_i32_bitwise(
    builder: &mut FunctionBuilder,
    op: BitwiseOp,
    lhs: Value,
    rhs: Value,
) -> GuardedResult {
    let numeric_fast = builder.create_block();
    let ushr_number_block = builder.create_block();
    let box_block = builder.create_block();
    let slow_block = builder.create_block();
    let merge_block = builder.create_block();
    builder.append_block_param(merge_block, types::I64);

    let lhs_is_number = emit_is_number(builder, lhs);
    let rhs_is_number = emit_is_number(builder, rhs);
    let both_numbers = builder.ins().band(lhs_is_number, rhs_is_number);
    builder
        .ins()
        .brif(both_numbers, numeric_fast, &[], slow_block, &[]);

    builder.switch_to_block(numeric_fast);
    let l32 = emit_number_to_int32(builder, lhs);
    let r32 = emit_number_to_int32(builder, rhs);
    let shift_mask = builder.ins().iconst(types::I32, 0x1f);
    let shift_amt = builder.ins().band(r32, shift_mask);

    let result = match op {
        BitwiseOp::And => builder.ins().band(l32, r32),
        BitwiseOp::Or => builder.ins().bor(l32, r32),
        BitwiseOp::Xor => builder.ins().bxor(l32, r32),
        BitwiseOp::Shl => builder.ins().ishl(l32, shift_amt),
        BitwiseOp::Shr => builder.ins().sshr(l32, shift_amt),
        BitwiseOp::Ushr => builder.ins().ushr(l32, shift_amt),
    };

    // JS `>>>` can produce uint32 values > i32::MAX, which cannot be represented
    // as a NaN-boxed int32 result. Return raw f64 bits for those values.
    if matches!(op, BitwiseOp::Ushr) {
        let is_negative_i32 = builder.ins().icmp_imm(IntCC::SignedLessThan, result, 0);
        builder
            .ins()
            .brif(is_negative_i32, ushr_number_block, &[], box_block, &[]);
    } else {
        builder.ins().jump(box_block, &[]);
    }

    builder.switch_to_block(ushr_number_block);
    let as_u64 = builder.ins().uextend(types::I64, result);
    let as_f64 = builder.ins().fcvt_from_uint(types::F64, as_u64);
    let bits = builder.ins().bitcast(types::I64, MemFlags::new(), as_f64);
    builder.ins().jump(merge_block, &[BlockArg::Value(bits)]);

    builder.switch_to_block(box_block);
    let boxed = emit_box_int32(builder, result);
    builder.ins().jump(merge_block, &[BlockArg::Value(boxed)]);

    let result_val = builder.block_params(merge_block)[0];
    GuardedResult {
        merge_block,
        slow_block,
        result: result_val,
    }
}

/// Emit guarded numeric bitwise NOT (`~`).
pub(crate) fn emit_guarded_i32_bitnot(builder: &mut FunctionBuilder, val: Value) -> GuardedResult {
    let numeric_fast = builder.create_block();
    let slow_block = builder.create_block();
    let merge_block = builder.create_block();
    builder.append_block_param(merge_block, types::I64);

    let is_number = emit_is_number(builder, val);
    builder
        .ins()
        .brif(is_number, numeric_fast, &[], slow_block, &[]);

    builder.switch_to_block(numeric_fast);
    let v32 = emit_number_to_int32(builder, val);
    let result = builder.ins().bnot(v32);
    let boxed = emit_box_int32(builder, result);
    builder.ins().jump(merge_block, &[BlockArg::Value(boxed)]);

    let result_val = builder.block_params(merge_block)[0];
    GuardedResult {
        merge_block,
        slow_block,
        result: result_val,
    }
}

/// Supported bitwise operations.
pub(crate) enum BitwiseOp {
    /// `&`
    And,
    /// `|`
    Or,
    /// `^`
    Xor,
    /// `<<`
    Shl,
    /// `>>`
    Shr,
    /// `>>>`
    Ushr,
}

// ---------------------------------------------------------------------------
// Feedback-driven specialization
// ---------------------------------------------------------------------------

/// Hint for which type guard to emit, derived from feedback vector TypeFlags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SpecializationHint {
    /// Only int32 values observed — emit i32 guard (most likely to succeed).
    Int32,
    /// Only float64 values observed — emit f64 guard.
    Float64,
    /// Both int32 and f64 observed — emit i32 guard, f64 fallback before generic.
    Numeric,
    /// Non-numeric types observed or no feedback — go directly to generic helper.
    Generic,
}

impl SpecializationHint {
    /// Derive specialization hint from feedback vector TypeFlags.
    ///
    /// If `flags` is None (no feedback available), defaults to `Int32` (the most
    /// common case for hot loops with counters/indices).
    pub fn from_type_flags(flags: Option<&otter_vm_bytecode::TypeFlags>) -> Self {
        match flags {
            None => Self::Int32, // default: optimistic i32 guard
            Some(tf) => {
                if tf.is_int32_only() {
                    Self::Int32
                } else if tf.is_number_only() {
                    Self::Float64
                } else if tf.is_numeric_only() {
                    Self::Numeric
                } else if tf.seen_string || tf.seen_object || tf.seen_function {
                    // Non-numeric types observed — go directly to generic
                    Self::Generic
                } else {
                    // No observations yet or only booleans/null/undefined —
                    // default to i32 guard (optimistic for hot counter loops)
                    Self::Int32
                }
            }
        }
    }
}

/// Emit type-specialized arithmetic based on feedback-vector observations.
///
/// Selects the most efficient guard pattern for the observed type profile:
/// - `Int32`: i32 guard only (1 branch, no f64 path)
/// - `Float64`: f64 guard only (1 branch, no i32 path)
/// - `Numeric`: full i32 + f64 cascade
/// - `Generic`: immediate bailout (can't specialize)
pub(crate) fn emit_specialized_arith(
    builder: &mut FunctionBuilder,
    op: ArithOp,
    lhs: Value,
    rhs: Value,
    hint: SpecializationHint,
) -> GuardedResult {
    match hint {
        SpecializationHint::Int32 => emit_guarded_i32_arith(builder, op, lhs, rhs),
        SpecializationHint::Float64 => emit_guarded_f64_arith(builder, op, lhs, rhs),
        SpecializationHint::Numeric => emit_guarded_numeric_arith(builder, op, lhs, rhs),
        SpecializationHint::Generic => {
            // Can't specialize — emit immediate bailout
            let slow_block = builder.create_block();
            let merge_block = builder.create_block();
            builder.append_block_param(merge_block, types::I64);
            builder.ins().jump(slow_block, &[]);
            let result = builder.block_params(merge_block)[0];
            GuardedResult {
                merge_block,
                slow_block,
                result,
            }
        }
    }
}

/// Emit type-specialized comparison based on feedback-vector observations.
///
/// Like `emit_specialized_arith`, selects the guard pattern matching the type profile.
pub(crate) fn emit_specialized_cmp(
    builder: &mut FunctionBuilder,
    int_cc: IntCC,
    float_cc: FloatCC,
    lhs: Value,
    rhs: Value,
    hint: SpecializationHint,
) -> GuardedResult {
    match hint {
        SpecializationHint::Int32 => emit_guarded_i32_cmp(builder, int_cc, lhs, rhs),
        SpecializationHint::Float64 => emit_guarded_f64_cmp(builder, float_cc, lhs, rhs),
        SpecializationHint::Numeric | SpecializationHint::Generic => {
            emit_guarded_numeric_cmp(builder, int_cc, float_cc, lhs, rhs)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nan_box_int32_roundtrip() {
        // Verify that our NaN-boxing constants produce correct bit patterns
        for val in [0i32, 1, -1, 42, i32::MAX, i32::MIN] {
            let boxed = TAG_INT32 | ((val as u32) as i64);
            // Check tag
            assert_eq!(boxed & INT32_TAG_MASK, TAG_INT32, "tag mismatch for {val}");
            // Check unbox
            let unboxed = (boxed & LOW32_MASK) as u32 as i32;
            assert_eq!(unboxed, val, "roundtrip failed for {val}");
        }
    }

    #[test]
    fn bool_to_nanbox_identity() {
        // TAG_FALSE - 1 = TAG_TRUE
        assert_eq!(TAG_FALSE - 1, TAG_TRUE);
        // TAG_FALSE - 0 = TAG_FALSE
        assert_eq!(TAG_FALSE, TAG_FALSE);
    }
}
