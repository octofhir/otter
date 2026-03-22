//! NaN-boxing emit helpers for Cranelift IR.
//!
//! These functions emit CLIF instructions to box/unbox values
//! and check NaN-boxing tags.

use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::types;
use cranelift_codegen::ir::{InstBuilder, Value};
use cranelift_frontend::FunctionBuilder;

// NaN-boxing constants (must match otter-vm-bytecode/src/value_tags.rs)
pub const TAG_UNDEFINED: u64 = 0x7FF8_0000_0000_0000;
pub const TAG_NULL: u64 = 0x7FF8_0000_0000_0001;
pub const TAG_TRUE: u64 = 0x7FF8_0000_0000_0002;
pub const TAG_FALSE: u64 = 0x7FF8_0000_0000_0003;
pub const TAG_INT32: u64 = 0x7FF8_0001_0000_0000;
pub const INT32_TAG_MASK: u64 = 0xFFFF_FFFF_0000_0000;
pub const TAG_PTR_OBJECT: u64 = 0x7FFC_0000_0000_0000;
pub const TAG_PTR_STRING: u64 = 0x7FFD_0000_0000_0000;
pub const TAG_PTR_FUNCTION: u64 = 0x7FFE_0000_0000_0000;
pub const TAG_MASK: u64 = 0xFFFF_0000_0000_0000;
pub const PAYLOAD_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;

/// Box an i32 value into NaN-boxed u64.
/// Result = TAG_INT32 | (val as u32)
pub fn emit_box_int32(builder: &mut FunctionBuilder, val: Value) -> Value {
    let tag = builder.ins().iconst(types::I64, TAG_INT32 as i64);
    // Zero-extend i32 to i64
    let extended = builder.ins().uextend(types::I64, val);
    builder.ins().bor(tag, extended)
}

/// Box an f64 value into NaN-boxed u64.
/// Result = bitcast(f64 → i64)
pub fn emit_box_float64(builder: &mut FunctionBuilder, val: Value) -> Value {
    builder.ins().bitcast(types::I64, cranelift_codegen::ir::MemFlags::new(), val)
}

/// Box a boolean into NaN-boxed u64.
/// Result = TAG_TRUE if val != 0, TAG_FALSE otherwise
pub fn emit_box_bool(builder: &mut FunctionBuilder, val: Value) -> Value {
    let true_val = builder.ins().iconst(types::I64, TAG_TRUE as i64);
    let false_val = builder.ins().iconst(types::I64, TAG_FALSE as i64);
    builder.ins().select(val, true_val, false_val)
}

/// Unbox an i32 from NaN-boxed u64 (unchecked — caller must have guarded).
/// Result = val as i32 (truncate lower 32 bits)
pub fn emit_unbox_int32(builder: &mut FunctionBuilder, val: Value) -> Value {
    builder.ins().ireduce(types::I32, val)
}

/// Unbox an f64 from NaN-boxed u64 (unchecked).
/// Result = bitcast(i64 → f64)
pub fn emit_unbox_float64(builder: &mut FunctionBuilder, val: Value) -> Value {
    builder.ins().bitcast(types::F64, cranelift_codegen::ir::MemFlags::new(), val)
}

/// Check if a NaN-boxed value is an Int32.
/// Returns an i8 boolean (1 = is int32, 0 = not).
pub fn emit_is_int32(builder: &mut FunctionBuilder, val: Value) -> Value {
    let mask = builder.ins().iconst(types::I64, INT32_TAG_MASK as i64);
    let tag = builder.ins().band(val, mask);
    let expected = builder.ins().iconst(types::I64, TAG_INT32 as i64);
    builder.ins().icmp(IntCC::Equal, tag, expected)
}

/// Check if a NaN-boxed value is a float64 (i.e., NOT a tagged value).
/// A valid f64 has bits where the upper 13 bits are NOT all-ones-quiet-NaN.
/// Simplified: it's f64 if it's not int32, not a pointer, and not a singleton.
pub fn emit_is_float64(builder: &mut FunctionBuilder, val: Value) -> Value {
    // Quick check: if upper 16 bits < 0x7FF8, it's a normal f64.
    // If upper 16 bits == 0x7FFA, it's NaN (which is also a valid f64 for us).
    // For now: check that it's NOT int32 and NOT a pointer tag.
    let tag_mask = builder.ins().iconst(types::I64, TAG_MASK as i64);
    let tag = builder.ins().band(val, tag_mask);
    let ptr_threshold = builder.ins().iconst(types::I64, TAG_PTR_OBJECT as i64);
    let is_not_ptr = builder.ins().icmp(IntCC::UnsignedLessThan, tag, ptr_threshold);

    let int32_mask = builder.ins().iconst(types::I64, INT32_TAG_MASK as i64);
    let int32_tag = builder.ins().band(val, int32_mask);
    let expected_int32 = builder.ins().iconst(types::I64, TAG_INT32 as i64);
    let is_not_int32 = builder.ins().icmp(IntCC::NotEqual, int32_tag, expected_int32);

    builder.ins().band(is_not_ptr, is_not_int32)
}

/// Check if a NaN-boxed value is an object pointer (tag == 0x7FFC).
pub fn emit_is_object(builder: &mut FunctionBuilder, val: Value) -> Value {
    let mask = builder.ins().iconst(types::I64, TAG_MASK as i64);
    let tag = builder.ins().band(val, mask);
    let expected = builder.ins().iconst(types::I64, TAG_PTR_OBJECT as i64);
    builder.ins().icmp(IntCC::Equal, tag, expected)
}

/// Extract a pointer from a NaN-boxed value (mask off tag bits).
pub fn emit_extract_pointer(builder: &mut FunctionBuilder, val: Value) -> Value {
    let mask = builder.ins().iconst(types::I64, PAYLOAD_MASK as i64);
    builder.ins().band(val, mask)
}

/// Emit a NaN-boxed undefined constant.
pub fn emit_undefined(builder: &mut FunctionBuilder) -> Value {
    builder.ins().iconst(types::I64, TAG_UNDEFINED as i64)
}

/// Emit a NaN-boxed null constant.
pub fn emit_null(builder: &mut FunctionBuilder) -> Value {
    builder.ins().iconst(types::I64, TAG_NULL as i64)
}

/// Emit a NaN-boxed true constant.
pub fn emit_true(builder: &mut FunctionBuilder) -> Value {
    builder.ins().iconst(types::I64, TAG_TRUE as i64)
}

/// Emit a NaN-boxed false constant.
pub fn emit_false(builder: &mut FunctionBuilder) -> Value {
    builder.ins().iconst(types::I64, TAG_FALSE as i64)
}
