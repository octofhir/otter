//! AArch64 numeric, comparison, conversion, and bitwise template emitters.
//!
//! # Contents
//! - Int32 fast paths with overflow promotion to the double path.
//! - Full double arithmetic and NaN-correct comparisons.
//! - Strict/loose equality over numbers and non-number immediates.
//! - Bitwise/shift lowering over the full finite-double `ToInt32`/`ToUint32`.
//!
//! # Invariants
//! - An overflowing int32 result is its exact f64 value, never a side exit.
//! - A non-number operand on a numeric-only path takes an exact side exit
//!   before any observable effect; the interpreter re-executes the opcode.
//! - Heap cells never decide equality inline; the interpreter owns object
//!   identity and string/BigInt content equality.
//!
//! # See also
//! - [`super::values`] — the tagged encode/decode primitives used here.

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};

use super::transitions::{TransitionTable, emit_add_delegate, emit_string_concat_alloc_call};
use super::values::{
    emit_box_bool, emit_box_double, emit_box_int32, emit_guard_int32, emit_guard_number,
    emit_load_reg, emit_load_u64, emit_num_to_double, emit_store_reg, emit_to_int32_fast,
    emit_to_uint32_fast,
};
use crate::baseline::{
    DOUBLE_OFFSET_HI16, FUNCTION_ID_TAG, NUMBER_TAG_HI16, Unsupported, VALUE_NULL, VALUE_UNDEFINED,
};

/// Function-id immediate low tag as a 32-bit `dynasm` operand.
const FUNCTION_ID_TAG_IMM: u32 = FUNCTION_ID_TAG as u32;
use crate::template::{ArithKind, BitwiseKind, CompareKind};

/// Emit `Add`/`Sub`/`Mul`/`Div`/`Rem` over tagged numbers.
///
/// `Add`/`Sub`/`Mul` take an int32 fast path that falls through to the f64
/// path on a non-int32 operand or an overflowing result (never to the side
/// exit — an overflowing integer result is just its exact f64 value). `Div`
/// always computes in f64 (ECMAScript division yields a Number even for exact
/// integer quotients). `Rem` keeps the truncating int32 remainder inline and
/// side-exits the cases int32 cannot represent (zero divisor → NaN, zero
/// remainder of a negative dividend → `-0`).
pub(super) fn emit_binary_arith(
    ops: &mut Assembler,
    dst: u16,
    lhs: u16,
    rhs: u16,
    kind: ArithKind,
    bail: DynamicLabel,
) -> Result<(), Unsupported> {
    emit_load_reg(ops, 9, lhs)?;
    emit_load_reg(ops, 10, rhs)?;
    match kind {
        ArithKind::Div => {
            emit_num_to_double(ops, 9, 0, bail);
            emit_num_to_double(ops, 10, 1, bail);
            dynasm!(ops ; .arch aarch64 ; fdiv d2, d0, d1);
            emit_box_double(ops, 2, 13);
            return emit_store_reg(ops, 13, dst);
        }
        ArithKind::Rem => {
            emit_guard_int32(ops, 9, bail);
            emit_guard_int32(ops, 10, bail);
            let store = ops.new_dynamic_label();
            dynasm!(ops
                ; .arch aarch64
                ; cbz w10, =>bail          // rhs == 0 → interpreter yields NaN
                ; sdiv w11, w9, w10        // truncating quotient
                ; msub w13, w11, w10, w9   // remainder = lhs - quotient * rhs
                ; cbnz w13, =>store        // nonzero remainder: sign correct
                ; tbnz w9, #31, =>bail     // zero remainder, negative lhs → -0
                ; =>store
            );
            emit_box_int32(ops, 13, 12);
            return emit_store_reg(ops, 13, dst);
        }
        ArithKind::Sub | ArithKind::Mul => {}
    }
    let float_path = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch aarch64
        ; movz x15, NUMBER_TAG_HI16, lsl #48
        ; and x14, x9, x15
        ; cmp x14, x15
        ; b.ne =>float_path
        ; and x14, x10, x15
        ; cmp x14, x15
        ; b.ne =>float_path
    );
    match kind {
        ArithKind::Sub => dynasm!(ops ; .arch aarch64 ; subs w13, w9, w10 ; b.vs =>float_path),
        ArithKind::Mul => dynasm!(ops
            ; .arch aarch64
            ; smull x13, w9, w10
            ; cmp x13, w13, sxtw
            ; b.ne =>float_path
        ),
        _ => unreachable!("Div/Rem returned above"),
    }
    emit_box_int32(ops, 13, 12);
    emit_store_reg(ops, 13, dst)?;
    dynasm!(ops ; .arch aarch64 ; b =>done ; =>float_path);
    emit_num_to_double(ops, 9, 0, bail);
    emit_num_to_double(ops, 10, 1, bail);
    match kind {
        ArithKind::Sub => dynasm!(ops ; .arch aarch64 ; fsub d2, d0, d1),
        ArithKind::Mul => dynasm!(ops ; .arch aarch64 ; fmul d2, d0, d1),
        _ => unreachable!("Div/Rem returned above"),
    }
    emit_box_double(ops, 2, 13);
    emit_store_reg(ops, 13, dst)?;
    dynasm!(ops ; .arch aarch64 ; =>done);
    Ok(())
}

/// Emit `+` with the full ECMAScript semantics: the inline numeric paths of
/// [`emit_binary_arith`], then the allocating string-concat runtime call
/// rooted at `concat_safepoint`, then the interpreter-completing delegate for
/// every remaining coercive case. Non-number operands never side-exit — `+`
/// stays resident in compiled code.
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_add_generic(
    ops: &mut Assembler,
    table: &TransitionTable,
    dst: u16,
    lhs: u16,
    rhs: u16,
    concat_safepoint: otter_vm::SafepointId,
    threw: DynamicLabel,
) -> Result<(), Unsupported> {
    emit_load_reg(ops, 9, lhs)?;
    emit_load_reg(ops, 10, rhs)?;
    let float_path = ops.new_dynamic_label();
    let runtime_path = ops.new_dynamic_label();
    let delegate_path = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch aarch64
        ; movz x15, NUMBER_TAG_HI16, lsl #48
        ; and x14, x9, x15
        ; cmp x14, x15
        ; b.ne =>float_path
        ; and x14, x10, x15
        ; cmp x14, x15
        ; b.ne =>float_path
        ; adds w13, w9, w10
        ; b.vs =>float_path
    );
    emit_box_int32(ops, 13, 12);
    emit_store_reg(ops, 13, dst)?;
    dynasm!(ops ; .arch aarch64 ; b =>done ; =>float_path);
    emit_num_to_double(ops, 9, 0, runtime_path);
    emit_num_to_double(ops, 10, 1, runtime_path);
    dynasm!(ops ; .arch aarch64 ; fadd d2, d0, d1);
    emit_box_double(ops, 2, 13);
    emit_store_reg(ops, 13, dst)?;
    dynasm!(ops ; .arch aarch64 ; b =>done ; =>runtime_path);
    emit_string_concat_alloc_call(ops, dst, lhs, rhs, concat_safepoint, delegate_path, done)?;
    dynasm!(ops ; .arch aarch64 ; =>delegate_path);
    emit_add_delegate(ops, table, dst, lhs, rhs, threw);
    dynasm!(ops ; .arch aarch64 ; =>done);
    Ok(())
}

/// Emit a comparison producing a boolean.
///
/// Both operands int32 → signed integer compare. Otherwise the double path
/// decodes and `fcmp`s with FP condition codes, so an unordered (NaN) compare
/// yields the ECMAScript result (every relational compare false, `!=` true).
/// Strict (in)equality additionally decides non-number immediates by raw bit
/// identity and side-exits on heap cells.
pub(super) fn emit_compare(
    ops: &mut Assembler,
    dst: u16,
    lhs: u16,
    rhs: u16,
    kind: CompareKind,
    bail: DynamicLabel,
) -> Result<(), Unsupported> {
    emit_load_reg(ops, 9, lhs)?;
    emit_load_reg(ops, 10, rhs)?;
    let float_path = ops.new_dynamic_label();
    let have_bool = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch aarch64
        ; movz x15, NUMBER_TAG_HI16, lsl #48
        ; and x14, x9, x15
        ; cmp x14, x15
        ; b.ne =>float_path
        ; and x14, x10, x15
        ; cmp x14, x15
        ; b.ne =>float_path
        ; cmp w9, w10
    );
    emit_cset(ops, kind, IntCondition);
    dynasm!(ops ; .arch aarch64 ; b =>have_bool ; =>float_path);
    if matches!(kind, CompareKind::Eq | CompareKind::Ne) {
        let lhs_non_number = ops.new_dynamic_label();
        let number_path = ops.new_dynamic_label();
        let strict_false = ops.new_dynamic_label();
        // Strict equality on non-number immediates (null / undefined /
        // boolean / hole / function id) decides by raw bit identity. Any
        // heap cell (object, string, BigInt, …) side-exits: the interpreter
        // owns object identity and string / BigInt content equality.
        dynasm!(ops
            ; .arch aarch64
            ; movz x11, NUMBER_TAG_HI16, lsl #48
            ; tst x9, x11
            ; b.eq =>lhs_non_number
            ; tst x10, x11
            ; b.eq =>strict_false        // number !== non-number
            ; b =>number_path
            ; =>lhs_non_number
            ; tst x10, x11
            ; b.ne =>strict_false        // non-number !== number
            ; orr x11, x11, #0x2         // NOT_CELL_MASK (OTHER_TAG)
            ; tst x9, x11
            ; b.eq =>bail                // lhs heap cell → interpreter
            ; tst x10, x11
            ; b.eq =>bail                // rhs heap cell → interpreter
            ; cmp x9, x10
        );
        emit_cset(ops, kind, IntCondition);
        let false_value = match kind {
            CompareKind::Eq => 0,
            _ => 1,
        };
        dynasm!(ops
            ; .arch aarch64
            ; b =>have_bool
            ; =>strict_false
            ; movz w13, false_value
            ; b =>have_bool
            ; =>number_path
        );
    }
    emit_num_to_double(ops, 9, 0, bail);
    emit_num_to_double(ops, 10, 1, bail);
    dynasm!(ops ; .arch aarch64 ; fcmp d0, d1);
    emit_cset(ops, kind, FloatCondition);
    dynasm!(ops ; .arch aarch64 ; =>have_bool);
    emit_box_bool(ops, 13, 12);
    emit_store_reg(ops, 13, dst)
}

/// Marker: integer condition codes for [`emit_cset`].
struct IntCondition;
/// Marker: FP condition codes for [`emit_cset`] (unordered-aware).
struct FloatCondition;

trait ConditionSet {
    fn emit(ops: &mut Assembler, kind: CompareKind);
}

impl ConditionSet for IntCondition {
    fn emit(ops: &mut Assembler, kind: CompareKind) {
        match kind {
            CompareKind::Lt => dynasm!(ops ; .arch aarch64 ; cset w13, lt),
            CompareKind::Le => dynasm!(ops ; .arch aarch64 ; cset w13, le),
            CompareKind::Gt => dynasm!(ops ; .arch aarch64 ; cset w13, gt),
            CompareKind::Ge => dynasm!(ops ; .arch aarch64 ; cset w13, ge),
            CompareKind::Eq => dynasm!(ops ; .arch aarch64 ; cset w13, eq),
            CompareKind::Ne => dynasm!(ops ; .arch aarch64 ; cset w13, ne),
        }
    }
}

impl ConditionSet for FloatCondition {
    fn emit(ops: &mut Assembler, kind: CompareKind) {
        // FP flags after `fcmp`: unordered (NaN) makes every relational
        // condition below false and `ne` true, matching §7.2.13.
        match kind {
            CompareKind::Lt => dynasm!(ops ; .arch aarch64 ; cset w13, mi),
            CompareKind::Le => dynasm!(ops ; .arch aarch64 ; cset w13, ls),
            CompareKind::Gt => dynasm!(ops ; .arch aarch64 ; cset w13, gt),
            CompareKind::Ge => dynasm!(ops ; .arch aarch64 ; cset w13, ge),
            CompareKind::Eq => dynasm!(ops ; .arch aarch64 ; cset w13, eq),
            CompareKind::Ne => dynasm!(ops ; .arch aarch64 ; cset w13, ne),
        }
    }
}

fn emit_cset<C: ConditionSet>(ops: &mut Assembler, kind: CompareKind, _condition: C) {
    C::emit(ops, kind);
}

/// Emit abstract (in)equality for numbers and the null/undefined equivalence
/// class. String/object/coercive cases side-exit before observable work.
pub(super) fn emit_loose_compare(
    ops: &mut Assembler,
    dst: u16,
    lhs: u16,
    rhs: u16,
    negate: bool,
    bail: DynamicLabel,
) -> Result<(), Unsupported> {
    emit_load_reg(ops, 9, lhs)?;
    emit_load_reg(ops, 10, rhs)?;
    let lhs_nullish = ops.new_dynamic_label();
    let rhs_nullish = ops.new_dynamic_label();
    let have_bool = ops.new_dynamic_label();
    emit_load_u64(ops, 11, VALUE_NULL);
    dynasm!(ops ; .arch aarch64 ; cmp x9, x11 ; b.eq =>lhs_nullish);
    emit_load_u64(ops, 11, VALUE_UNDEFINED);
    dynasm!(ops ; .arch aarch64 ; cmp x9, x11 ; b.eq =>lhs_nullish);
    emit_load_u64(ops, 11, VALUE_NULL);
    dynasm!(ops ; .arch aarch64 ; cmp x10, x11 ; b.eq =>rhs_nullish);
    emit_load_u64(ops, 11, VALUE_UNDEFINED);
    dynasm!(ops ; .arch aarch64 ; cmp x10, x11 ; b.eq =>rhs_nullish);

    emit_num_to_double(ops, 9, 0, bail);
    emit_num_to_double(ops, 10, 1, bail);
    dynasm!(ops ; .arch aarch64 ; fcmp d0, d1 ; cset w13, eq ; b =>have_bool);

    dynasm!(ops ; .arch aarch64 ; =>lhs_nullish);
    emit_load_u64(ops, 11, VALUE_NULL);
    dynasm!(ops ; .arch aarch64 ; cmp x10, x11 ; b.eq >both_nullish);
    emit_load_u64(ops, 11, VALUE_UNDEFINED);
    dynasm!(ops
        ; .arch aarch64
        ; cmp x10, x11
        ; cset w13, eq
        ; b =>have_bool
        ; both_nullish:
        ; movz w13, #1
        ; b =>have_bool
        ; =>rhs_nullish
        ; movz w13, #0
        ; =>have_bool
    );
    if negate {
        dynasm!(ops ; .arch aarch64 ; eor w13, w13, #1);
    }
    emit_box_bool(ops, 13, 12);
    emit_store_reg(ops, 13, dst)
}

/// Emit an int32 bitwise/shift op over the full `ToInt32` fast path.
///
/// The AArch64 32-bit `lsl`/`asr` mask the shift count to its low 5 bits
/// exactly as JS masks the right operand with `& 31`.
pub(super) fn emit_int_bitwise(
    ops: &mut Assembler,
    dst: u16,
    lhs: u16,
    rhs: u16,
    kind: BitwiseKind,
    bail: DynamicLabel,
) -> Result<(), Unsupported> {
    emit_load_reg(ops, 9, lhs)?;
    emit_load_reg(ops, 10, rhs)?;
    emit_to_int32_fast(ops, 9, 11, bail);
    emit_to_int32_fast(ops, 10, 12, bail);
    match kind {
        BitwiseKind::Or => dynasm!(ops ; .arch aarch64 ; orr w13, w11, w12),
        BitwiseKind::And => dynasm!(ops ; .arch aarch64 ; and w13, w11, w12),
        BitwiseKind::Xor => dynasm!(ops ; .arch aarch64 ; eor w13, w11, w12),
        BitwiseKind::Shl => dynasm!(ops ; .arch aarch64 ; lsl w13, w11, w12),
        BitwiseKind::Shr => dynasm!(ops ; .arch aarch64 ; asr w13, w11, w12),
    }
    emit_box_int32(ops, 13, 12);
    emit_store_reg(ops, 13, dst)
}

/// Emit unsigned right shift. The result boxes as a double because JS `>>>`
/// returns a uint32-valued Number and values above `i32::MAX` cannot be
/// represented by the int32 tag.
pub(super) fn emit_unsigned_shift_right(
    ops: &mut Assembler,
    dst: u16,
    lhs: u16,
    rhs: u16,
    bail: DynamicLabel,
) -> Result<(), Unsupported> {
    emit_load_reg(ops, 9, lhs)?;
    emit_load_reg(ops, 10, rhs)?;
    emit_to_uint32_fast(ops, 9, 11, bail);
    emit_to_uint32_fast(ops, 10, 12, bail);
    dynasm!(ops
        ; .arch aarch64
        ; lsr w13, w11, w12
        ; ucvtf d0, w13
    );
    emit_box_double(ops, 0, 13);
    emit_store_reg(ops, 13, dst)
}

/// Emit `dst = ToNumeric(src) + delta` (§13.4 UpdateExpression): int32 fast
/// path with overflow promotion to double; double path otherwise; non-number
/// side exit.
pub(super) fn emit_increment(
    ops: &mut Assembler,
    dst: u16,
    src: u16,
    delta: i32,
    bail: DynamicLabel,
) -> Result<(), Unsupported> {
    emit_load_reg(ops, 9, src)?;
    emit_load_u64(ops, 12, u64::from(delta as u32));
    let float_path = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch aarch64
        ; movz x15, NUMBER_TAG_HI16, lsl #48
        ; and x14, x9, x15
        ; cmp x14, x15
        ; b.ne =>float_path
        ; adds w13, w9, w12
        ; b.vs =>float_path
    );
    emit_box_int32(ops, 13, 11);
    emit_store_reg(ops, 13, dst)?;
    dynasm!(ops ; .arch aarch64 ; b =>done ; =>float_path);
    emit_num_to_double(ops, 9, 0, bail);
    dynasm!(ops ; .arch aarch64 ; scvtf d1, w12 ; fadd d2, d0, d1);
    emit_box_double(ops, 2, 13);
    emit_store_reg(ops, 13, dst)?;
    dynasm!(ops ; .arch aarch64 ; =>done);
    Ok(())
}

/// Emit `dst = -ToNumeric(src)` (§6.1.6.1.1 unaryMinus). The int32 fast path
/// promotes the two unrepresentable results to their exact boxed doubles:
/// `-0` (from payload `0`) and `2147483648` (from `-i32::MIN`).
pub(super) fn emit_negate(
    ops: &mut Assembler,
    dst: u16,
    src: u16,
    bail: DynamicLabel,
) -> Result<(), Unsupported> {
    emit_load_reg(ops, 9, src)?;
    let maybe_double = ops.new_dynamic_label();
    let zero_case = ops.new_dynamic_label();
    let overflow_case = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch aarch64
        ; movz x15, NUMBER_TAG_HI16, lsl #48
        ; and x14, x9, x15
        ; cmp x14, x15
        ; b.ne =>maybe_double
        ; cbz w9, =>zero_case
        ; negs w13, w9
        ; b.vs =>overflow_case
    );
    emit_box_int32(ops, 13, 12);
    emit_store_reg(ops, 13, dst)?;
    dynasm!(ops ; .arch aarch64 ; b =>done ; =>zero_case);
    emit_load_u64(
        ops,
        13,
        otter_vm::value::tag::box_double((-0.0f64).to_bits()),
    );
    emit_store_reg(ops, 13, dst)?;
    dynasm!(ops ; .arch aarch64 ; b =>done ; =>overflow_case);
    emit_load_u64(
        ops,
        13,
        otter_vm::value::tag::box_double(2_147_483_648.0f64.to_bits()),
    );
    emit_store_reg(ops, 13, dst)?;
    dynasm!(ops ; .arch aarch64 ; b =>done ; =>maybe_double);
    dynasm!(ops
        ; .arch aarch64
        ; tst x9, x15
        ; b.eq =>bail                 // cell / immediate → exact coercion
        ; movz x14, DOUBLE_OFFSET_HI16, lsl #48
        ; sub x14, x9, x14
        ; fmov d0, x14
        ; fneg d1, d0
    );
    emit_box_double(ops, 1, 13);
    emit_store_reg(ops, 13, dst)?;
    dynasm!(ops ; .arch aarch64 ; =>done);
    Ok(())
}

/// Emit `dst = ToNumeric(src)`: identity on a number (int32 or double);
/// every other value side-exits for exact coercion.
pub(super) fn emit_to_numeric(
    ops: &mut Assembler,
    dst: u16,
    src: u16,
    bail: DynamicLabel,
) -> Result<(), Unsupported> {
    emit_load_reg(ops, 9, src)?;
    emit_guard_number(ops, 9, bail);
    emit_store_reg(ops, 9, dst)
}

/// Emit `dst = ToPrimitive(src)` for already-primitive values. Heap cells
/// (objects, callables, strings) and bytecode-function references side-exit
/// so observable `@@toPrimitive` / `valueOf` / `toString` hooks still run;
/// numbers and the `null` / boolean / `undefined` immediates pass through.
pub(super) fn emit_to_primitive(
    ops: &mut Assembler,
    dst: u16,
    src: u16,
    bail: DynamicLabel,
) -> Result<(), Unsupported> {
    let keep = ops.new_dynamic_label();
    emit_load_reg(ops, 9, src)?;
    dynasm!(ops
        ; .arch aarch64
        ; movz x15, NUMBER_TAG_HI16, lsl #48
        ; tst x9, x15                 // number → already primitive
        ; b.ne =>keep
        ; orr x15, x15, #0x2          // NOT_CELL_MASK (OTHER_TAG)
        ; tst x9, x15
        ; b.eq =>bail                 // heap cell (object/string/callable)
        ; and x14, x9, #0xffff
        ; cmp x14, FUNCTION_ID_TAG_IMM
        ; b.eq =>bail                 // closure-less function reference
        ; =>keep
    );
    emit_store_reg(ops, 9, dst)
}
