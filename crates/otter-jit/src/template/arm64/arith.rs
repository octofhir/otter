//! AArch64 numeric, comparison, conversion, and bitwise template emitters.
//!
//! # Contents
//! - Int32 fast paths with overflow promotion to the double path.
//! - Full double arithmetic and NaN-correct comparisons.
//! - Strict/loose equality over numbers and non-number immediates.
//! - Early int32 equality before the complete nullish/double/coercive loose
//!   comparison path.
//! - Bitwise/shift lowering over the full finite-double `ToInt32`/`ToUint32`.
//!
//! # Invariants
//! - An overflowing int32 result is its exact f64 value, never a side exit.
//! - A non-number operand on a numeric-only path takes an exact side exit
//!   before any observable effect; the interpreter re-executes the opcode.
//! - Heap-cell equality decides reference identity inline and completes
//!   content equality through the leaf strict-equality probe and the
//!   reentrant loose-equality transition; no equality opcode side-exits on
//!   a live isolate.
//! - Coercive `ToPrimitive`/`ToNumeric` cases publish their operands in the
//!   frame window and complete through cold VM-transition tails; user hooks
//!   are never replayed and cold transition code does not split hot blocks.
//!
//! # See also
//! - [`super::values`] — the tagged encode/decode primitives used here.

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};
use otter_bytecode::Op;

use super::transitions::{TransitionTable, emit_add_delegate, emit_string_concat_alloc_call};
use super::values::{
    emit_box_bool, emit_box_double, emit_box_int32, emit_guard_int32, emit_load_reg,
    emit_load_runtime_stub, emit_load_u64, emit_num_to_double, emit_store_reg, emit_to_int32_fast,
    emit_to_uint32_fast,
};
use crate::artifact::relocation::RelocationCapture;
use crate::entry::{
    DOUBLE_OFFSET_HI16, FUNCTION_ID_TAG, NUMBER_TAG_HI16, THREAD_OFFSET, Unsupported, VALUE_NULL,
    VALUE_TRUE, VALUE_UNDEFINED, VM_THREAD_GC_HEAP_OFFSET,
};
use otter_vm::native_abi as abi;

/// One cold coercion continuation emitted after the function's hot operation
/// stream. The source instruction has already published its canonical PC;
/// success branches to `resume`, while throws use the function's shared status
/// epilogue. A live canonical activation is part of the runtime-op contract;
/// function ownership is read from its frame header rather than emitted again.
pub(super) struct CoercionSlowPath {
    entry: DynamicLabel,
    resume: DynamicLabel,
    dst: u16,
    src: u16,
    mode: u32,
    hint: u32,
}

/// One cold completion for a numeric-family fast-path miss. `rhs_or_delta`
/// is a register index for binary operations and the signed immediate bits for
/// `Increment`; unary operations ignore it.
pub(super) struct NumericSlowPath {
    entry: DynamicLabel,
    resume: DynamicLabel,
    dst: u16,
    lhs: u16,
    rhs_or_delta: u64,
    opcode: Op,
}

fn numeric_slow_path(
    ops: &mut Assembler,
    slow_paths: &mut Vec<NumericSlowPath>,
    dst: u16,
    lhs: u16,
    rhs_or_delta: u64,
    opcode: Op,
) -> (DynamicLabel, DynamicLabel) {
    let entry = ops.new_dynamic_label();
    let resume = ops.new_dynamic_label();
    slow_paths.push(NumericSlowPath {
        entry,
        resume,
        dst,
        lhs,
        rhs_or_delta,
        opcode,
    });
    (entry, resume)
}

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
    relocations: &mut RelocationCapture,
    dst: u16,
    lhs: u16,
    rhs: u16,
    kind: ArithKind,
    slow_paths: &mut Vec<NumericSlowPath>,
) -> Result<(), Unsupported> {
    let opcode = match kind {
        ArithKind::Sub => Op::Sub,
        ArithKind::Mul => Op::Mul,
        ArithKind::Div => Op::Div,
        ArithKind::Rem => Op::Rem,
        ArithKind::Pow => Op::Pow,
    };
    let (slow, resume) = numeric_slow_path(ops, slow_paths, dst, lhs, u64::from(rhs), opcode);
    emit_load_reg(ops, 9, lhs)?;
    emit_load_reg(ops, 10, rhs)?;
    match kind {
        ArithKind::Div => {
            emit_num_to_double(ops, 9, 0, slow);
            emit_num_to_double(ops, 10, 1, slow);
            dynasm!(ops ; .arch aarch64 ; fdiv d2, d0, d1);
            emit_box_double(ops, 2, 13);
            emit_store_reg(ops, 13, dst)?;
            dynasm!(ops ; .arch aarch64 ; =>resume);
            return Ok(());
        }
        ArithKind::Rem => {
            let store = ops.new_dynamic_label();
            let number_slow = ops.new_dynamic_label();
            let done = ops.new_dynamic_label();
            emit_guard_int32(ops, 9, number_slow);
            emit_guard_int32(ops, 10, number_slow);
            dynasm!(ops
                ; .arch aarch64
                ; cbz w10, =>number_slow   // rhs == 0 → NaN via the f64 probe
                ; sdiv w11, w9, w10        // truncating quotient
                ; msub w13, w11, w10, w9   // remainder = lhs - quotient * rhs
                ; cbnz w13, =>store        // nonzero remainder: sign correct
                ; tbnz w9, #31, =>number_slow // zero remainder, negative lhs → -0
                ; =>store
            );
            emit_box_int32(ops, 13, 12);
            emit_store_reg(ops, 13, dst)?;
            // Doubles and the results int32 cannot represent complete
            // through the leaf f64-remainder probe; a non-number operand
            // misses so the interpreter owns coercion.
            dynasm!(ops
                ; .arch aarch64
                ; b =>done
                ; =>number_slow
                ; ldr x0, [x20, THREAD_OFFSET]
                ; ldr x0, [x0, VM_THREAD_GC_HEAP_OFFSET]
                ; mov x1, x9
                ; mov x2, x10
            );
            emit_load_runtime_stub(
                ops,
                relocations,
                16,
                otter_vm::runtime_stubs::NUMBER_REM_LEAF.entry_addr() as u64,
                abi::STUB_NUMBER_REM_LEAF,
            );
            dynasm!(ops
                ; .arch aarch64
                ; blr x16
                ; and x1, x1, #0xff
                ; cbnz x1, =>slow
                ; mov x13, x0
            );
            emit_store_reg(ops, 13, dst)?;
            dynasm!(ops ; .arch aarch64 ; =>done ; =>resume);
            return Ok(());
        }
        ArithKind::Pow => {
            dynasm!(ops ; .arch aarch64 ; b =>slow ; =>resume);
            return Ok(());
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
    emit_num_to_double(ops, 9, 0, slow);
    emit_num_to_double(ops, 10, 1, slow);
    match kind {
        ArithKind::Sub => dynasm!(ops ; .arch aarch64 ; fsub d2, d0, d1),
        ArithKind::Mul => dynasm!(ops ; .arch aarch64 ; fmul d2, d0, d1),
        _ => unreachable!("Div/Rem returned above"),
    }
    emit_box_double(ops, 2, 13);
    emit_store_reg(ops, 13, dst)?;
    dynasm!(ops ; .arch aarch64 ; =>done ; =>resume);
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
    relocations: &mut RelocationCapture,
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
    emit_string_concat_alloc_call(
        ops,
        relocations,
        dst,
        lhs,
        rhs,
        concat_safepoint,
        delegate_path,
        done,
    )?;
    dynasm!(ops ; .arch aarch64 ; =>delegate_path);
    emit_add_delegate(ops, relocations, table, dst, lhs, rhs, threw);
    dynasm!(ops ; .arch aarch64 ; =>done);
    Ok(())
}

/// Emit a comparison producing a boolean.
///
/// Both operands int32 → signed integer compare. Otherwise the double path
/// decodes and `fcmp`s with FP condition codes, so an unordered (NaN) compare
/// yields the ECMAScript result (every relational compare false, `!=` true).
/// Total strict (in)equality over two tagged values in `x9`/`x10`, leaving the
/// 0/1 answer in `w13`. Numbers compare numerically (int32 fast path, doubles
/// via `fcmp`), non-number immediates by raw bit identity, heap cells by
/// reference identity with distinct cells completing content equality through
/// the leaf probe. `bail` fires only on the probe's null-heap miss.
pub(crate) fn emit_strict_eq_tagged(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    negate: bool,
    bail: DynamicLabel,
) {
    let kind = if negate {
        CompareKind::Ne
    } else {
        CompareKind::Eq
    };
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
    {
        let lhs_non_number = ops.new_dynamic_label();
        let number_path = ops.new_dynamic_label();
        let strict_false = ops.new_dynamic_label();
        let cell_path = ops.new_dynamic_label();
        let leaf_call = ops.new_dynamic_label();
        dynasm!(ops
            ; .arch aarch64
            ; movz x11, NUMBER_TAG_HI16, lsl #48
            ; tst x9, x11
            ; b.eq =>lhs_non_number
            ; tst x10, x11
            ; b.eq =>strict_false
            ; b =>number_path
            ; =>lhs_non_number
            ; tst x10, x11
            ; b.ne =>strict_false
            ; orr x11, x11, #0x2
            ; tst x9, x11
            ; b.eq =>cell_path
            ; tst x10, x11
            ; b.eq =>cell_path
            ; cmp x9, x10
        );
        emit_cset(ops, kind, IntCondition);
        dynasm!(ops
            ; .arch aarch64
            ; b =>have_bool
            ; =>cell_path
            ; cmp x9, x10
            ; b.ne =>leaf_call
        );
        emit_cset(ops, kind, IntCondition);
        dynasm!(ops
            ; .arch aarch64
            ; b =>have_bool
            ; =>leaf_call
            ; ldr x0, [x20, THREAD_OFFSET]
            ; ldr x0, [x0, VM_THREAD_GC_HEAP_OFFSET]
            ; mov x1, x9
            ; mov x2, x10
        );
        emit_load_runtime_stub(
            ops,
            relocations,
            16,
            otter_vm::runtime_stubs::STRICT_EQ_LEAF.entry_addr() as u64,
            abi::STUB_STRICT_EQ_LEAF,
        );
        dynasm!(ops
            ; .arch aarch64
            ; blr x16
            ; and x1, x1, #0xff
            ; cbnz x1, =>bail
        );
        emit_load_u64(ops, 11, VALUE_TRUE);
        dynasm!(ops ; .arch aarch64 ; cmp x0, x11);
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
}

/// Strict (in)equality additionally decides non-number immediates by raw bit
/// identity and side-exits on heap cells.
pub(super) fn emit_compare(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    dst: u16,
    lhs: u16,
    rhs: u16,
    kind: CompareKind,
    bail: DynamicLabel,
    slow_paths: &mut Vec<NumericSlowPath>,
) -> Result<(), Unsupported> {
    let relational_slow = match kind {
        CompareKind::Lt => Some(numeric_slow_path(
            ops,
            slow_paths,
            dst,
            lhs,
            u64::from(rhs),
            Op::LessThan,
        )),
        CompareKind::Le => Some(numeric_slow_path(
            ops,
            slow_paths,
            dst,
            lhs,
            u64::from(rhs),
            Op::LessEq,
        )),
        CompareKind::Gt => Some(numeric_slow_path(
            ops,
            slow_paths,
            dst,
            lhs,
            u64::from(rhs),
            Op::GreaterThan,
        )),
        CompareKind::Ge => Some(numeric_slow_path(
            ops,
            slow_paths,
            dst,
            lhs,
            u64::from(rhs),
            Op::GreaterEq,
        )),
        CompareKind::Eq | CompareKind::Ne => None,
    };
    let numeric_miss = relational_slow.map_or(bail, |(entry, _)| entry);
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
        // boolean / hole / function id) decides by raw bit identity. Heap
        // cells decide reference identity inline; distinct cells complete
        // content equality (strings, BigInts) through the leaf probe.
        let cell_path = ops.new_dynamic_label();
        let leaf_call = ops.new_dynamic_label();
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
            ; b.eq =>cell_path           // lhs heap cell
            ; tst x10, x11
            ; b.eq =>cell_path           // rhs heap cell
            ; cmp x9, x10
        );
        emit_cset(ops, kind, IntCondition);
        dynasm!(ops
            ; .arch aarch64
            ; b =>have_bool
            // Identical bits are the same cell — strictly equal without a
            // probe; distinct cells ask the leaf `(heap, lhs, rhs)` probe,
            // whose only miss is a null heap (isolate-less test harness).
            ; =>cell_path
            ; cmp x9, x10
            ; b.ne =>leaf_call
        );
        emit_cset(ops, kind, IntCondition);
        dynasm!(ops
            ; .arch aarch64
            ; b =>have_bool
            ; =>leaf_call
            ; ldr x0, [x20, THREAD_OFFSET]
            ; ldr x0, [x0, VM_THREAD_GC_HEAP_OFFSET]
            ; mov x1, x9
            ; mov x2, x10
        );
        emit_load_runtime_stub(
            ops,
            relocations,
            16,
            otter_vm::runtime_stubs::STRICT_EQ_LEAF.entry_addr() as u64,
            abi::STUB_STRICT_EQ_LEAF,
        );
        dynasm!(ops
            ; .arch aarch64
            ; blr x16
            ; and x1, x1, #0xff
            ; cbnz x1, =>bail
        );
        emit_load_u64(ops, 11, VALUE_TRUE);
        dynasm!(ops ; .arch aarch64 ; cmp x0, x11);
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
    emit_num_to_double(ops, 9, 0, numeric_miss);
    emit_num_to_double(ops, 10, 1, numeric_miss);
    dynasm!(ops ; .arch aarch64 ; fcmp d0, d1);
    emit_cset(ops, kind, FloatCondition);
    dynasm!(ops ; .arch aarch64 ; =>have_bool);
    emit_box_bool(ops, 13, 12);
    emit_store_reg(ops, 13, dst)?;
    if let Some((_, resume)) = relational_slow {
        dynasm!(ops ; .arch aarch64 ; =>resume);
    }
    Ok(())
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
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_loose_compare(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    table: &TransitionTable,
    dst: u16,
    lhs: u16,
    rhs: u16,
    negate: bool,
    bail: DynamicLabel,
    threw: DynamicLabel,
) -> Result<(), Unsupported> {
    emit_load_reg(ops, 9, lhs)?;
    emit_load_reg(ops, 10, rhs)?;
    let lhs_nullish = ops.new_dynamic_label();
    let rhs_nullish = ops.new_dynamic_label();
    let have_bool = ops.new_dynamic_label();
    let generic = ops.new_dynamic_label();
    let slow = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    // Integer state/id comparisons dominate object-oriented dispatch loops.
    // Prove both canonical int32 tags once, compare their payload words, and
    // bypass nullish tests plus double conversion. Any other representation
    // falls through to the complete existing loose-equality implementation.
    dynasm!(ops
        ; .arch aarch64
        ; movz x15, NUMBER_TAG_HI16, lsl #48
        ; and x14, x9, x15
        ; cmp x14, x15
        ; b.ne =>generic
        ; and x14, x10, x15
        ; cmp x14, x15
        ; b.ne =>generic
        ; cmp w9, w10
        ; cset w13, eq
    );
    if negate {
        dynasm!(ops ; .arch aarch64 ; eor w13, w13, #1);
    }
    emit_box_bool(ops, 13, 12);
    emit_store_reg(ops, 13, dst)?;
    dynasm!(ops ; .arch aarch64 ; b =>done ; =>generic);

    emit_load_u64(ops, 11, VALUE_NULL);
    dynasm!(ops ; .arch aarch64 ; cmp x9, x11 ; b.eq =>lhs_nullish);
    emit_load_u64(ops, 11, VALUE_UNDEFINED);
    dynasm!(ops ; .arch aarch64 ; cmp x9, x11 ; b.eq =>lhs_nullish);
    emit_load_u64(ops, 11, VALUE_NULL);
    dynasm!(ops ; .arch aarch64 ; cmp x10, x11 ; b.eq =>rhs_nullish);
    emit_load_u64(ops, 11, VALUE_UNDEFINED);
    dynasm!(ops ; .arch aarch64 ; cmp x10, x11 ; b.eq =>rhs_nullish);

    emit_num_to_double(ops, 9, 0, slow);
    emit_num_to_double(ops, 10, 1, slow);
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
    emit_store_reg(ops, 13, dst)?;
    // Coercive operands (strings, booleans, objects) complete the whole
    // opcode through the reentrant loose-equality transition; the stub
    // writes the (negated) boolean into the destination register.
    dynasm!(ops
        ; .arch aarch64
        ; b =>done
        ; =>slow
        ; mov x0, x20
        ; movz x1, dst as u32
        ; movz x2, lhs as u32
        ; movz x3, rhs as u32
        ; movz x4, u32::from(negate)
    );
    emit_load_runtime_stub(
        ops,
        relocations,
        16,
        table.entry(abi::STUB_JIT_LOOSE_EQ),
        abi::STUB_JIT_LOOSE_EQ,
    );
    dynasm!(ops
        ; .arch aarch64
        ; blr x16
        ; cmp x0, #1
        ; b.eq =>threw
        ; cmp x0, #2
        ; b.eq =>bail
        ; =>done
    );
    Ok(())
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
    slow_paths: &mut Vec<NumericSlowPath>,
) -> Result<(), Unsupported> {
    let opcode = match kind {
        BitwiseKind::Or => Op::BitwiseOr,
        BitwiseKind::And => Op::BitwiseAnd,
        BitwiseKind::Xor => Op::BitwiseXor,
        BitwiseKind::Shl => Op::Shl,
        BitwiseKind::Shr => Op::Shr,
    };
    let (slow, resume) = numeric_slow_path(ops, slow_paths, dst, lhs, u64::from(rhs), opcode);
    emit_load_reg(ops, 9, lhs)?;
    emit_load_reg(ops, 10, rhs)?;
    emit_to_int32_fast(ops, 9, 11, slow);
    emit_to_int32_fast(ops, 10, 12, slow);
    match kind {
        BitwiseKind::Or => dynasm!(ops ; .arch aarch64 ; orr w13, w11, w12),
        BitwiseKind::And => dynasm!(ops ; .arch aarch64 ; and w13, w11, w12),
        BitwiseKind::Xor => dynasm!(ops ; .arch aarch64 ; eor w13, w11, w12),
        BitwiseKind::Shl => dynasm!(ops ; .arch aarch64 ; lsl w13, w11, w12),
        BitwiseKind::Shr => dynasm!(ops ; .arch aarch64 ; asr w13, w11, w12),
    }
    emit_box_int32(ops, 13, 12);
    emit_store_reg(ops, 13, dst)?;
    dynasm!(ops ; .arch aarch64 ; =>resume);
    Ok(())
}

/// Emit unsigned right shift. The result boxes as a double because JS `>>>`
/// returns a uint32-valued Number and values above `i32::MAX` cannot be
/// represented by the int32 tag.
pub(super) fn emit_unsigned_shift_right(
    ops: &mut Assembler,
    dst: u16,
    lhs: u16,
    rhs: u16,
    slow_paths: &mut Vec<NumericSlowPath>,
) -> Result<(), Unsupported> {
    let (slow, resume) = numeric_slow_path(ops, slow_paths, dst, lhs, u64::from(rhs), Op::Ushr);
    emit_load_reg(ops, 9, lhs)?;
    emit_load_reg(ops, 10, rhs)?;
    emit_to_uint32_fast(ops, 9, 11, slow);
    emit_to_uint32_fast(ops, 10, 12, slow);
    dynasm!(ops
        ; .arch aarch64
        ; lsr w13, w11, w12
        ; ucvtf d0, w13
    );
    emit_box_double(ops, 0, 13);
    emit_store_reg(ops, 13, dst)?;
    dynasm!(ops ; .arch aarch64 ; =>resume);
    Ok(())
}

/// Emit `dst = ToNumeric(src) + delta` (§13.4 UpdateExpression): int32 fast
/// path with overflow promotion to double; double path otherwise; non-number
/// side exit.
pub(super) fn emit_increment(
    ops: &mut Assembler,
    dst: u16,
    src: u16,
    delta: i32,
    slow_paths: &mut Vec<NumericSlowPath>,
) -> Result<(), Unsupported> {
    let (slow, resume) = numeric_slow_path(
        ops,
        slow_paths,
        dst,
        src,
        u64::from(delta as u32),
        Op::Increment,
    );
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
    emit_num_to_double(ops, 9, 0, slow);
    dynasm!(ops ; .arch aarch64 ; scvtf d1, w12 ; fadd d2, d0, d1);
    emit_box_double(ops, 2, 13);
    emit_store_reg(ops, 13, dst)?;
    dynasm!(ops ; .arch aarch64 ; =>done ; =>resume);
    Ok(())
}

/// Emit `dst = -ToNumeric(src)` (§6.1.6.1.1 unaryMinus). The int32 fast path
/// promotes the two unrepresentable results to their exact boxed doubles:
/// `-0` (from payload `0`) and `2147483648` (from `-i32::MIN`).
pub(super) fn emit_negate(
    ops: &mut Assembler,
    dst: u16,
    src: u16,
    slow_paths: &mut Vec<NumericSlowPath>,
) -> Result<(), Unsupported> {
    let (slow, resume) = numeric_slow_path(ops, slow_paths, dst, src, 0, Op::Neg);
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
        ; b.eq =>slow                 // cell / immediate → VM numeric completion
        ; movz x14, DOUBLE_OFFSET_HI16, lsl #48
        ; sub x14, x9, x14
        ; fmov d0, x14
        ; fneg d1, d0
    );
    emit_box_double(ops, 1, 13);
    emit_store_reg(ops, 13, dst)?;
    dynasm!(ops ; .arch aarch64 ; =>done ; =>resume);
    Ok(())
}

/// Emit unary bitwise-not over the Number fast path. BigInt and uncommon
/// numeric representations complete in the shared cold numeric transition.
pub(super) fn emit_bitwise_not(
    ops: &mut Assembler,
    dst: u16,
    src: u16,
    slow_paths: &mut Vec<NumericSlowPath>,
) -> Result<(), Unsupported> {
    let (slow, resume) = numeric_slow_path(ops, slow_paths, dst, src, 0, Op::BitwiseNot);
    emit_load_reg(ops, 9, src)?;
    emit_to_int32_fast(ops, 9, 11, slow);
    dynasm!(ops ; .arch aarch64 ; mvn w13, w11);
    emit_box_int32(ops, 13, 12);
    emit_store_reg(ops, 13, dst)?;
    dynasm!(ops ; .arch aarch64 ; =>resume);
    Ok(())
}

/// Emit `dst = ToNumeric(src)`: identity on a number (int32 or double);
/// every coercive case completes through the shared reentrant VM transition.
pub(super) fn emit_to_numeric(
    ops: &mut Assembler,
    dst: u16,
    src: u16,
    slow_paths: &mut Vec<CoercionSlowPath>,
) -> Result<(), Unsupported> {
    let slow = ops.new_dynamic_label();
    let resume = ops.new_dynamic_label();
    emit_load_reg(ops, 9, src)?;
    dynasm!(ops
        ; .arch aarch64
        ; movz x15, NUMBER_TAG_HI16, lsl #48
        ; tst x9, x15
        ; b.eq =>slow
    );
    emit_store_reg(ops, 9, dst)?;
    dynasm!(ops ; .arch aarch64 ; =>resume);
    slow_paths.push(CoercionSlowPath {
        entry: slow,
        resume,
        dst,
        src,
        mode: 1,
        hint: 0,
    });
    Ok(())
}

/// Emit `dst = ToPrimitive(src)` for already-primitive values. Heap cells
/// and bytecode-function references complete observable `@@toPrimitive` /
/// `valueOf` / `toString` hooks through the shared VM transition; immediate
/// primitives pass through inline.
pub(super) fn emit_to_primitive(
    ops: &mut Assembler,
    dst: u16,
    src: u16,
    hint: u32,
    slow_paths: &mut Vec<CoercionSlowPath>,
) -> Result<(), Unsupported> {
    let keep = ops.new_dynamic_label();
    let slow = ops.new_dynamic_label();
    let resume = ops.new_dynamic_label();
    emit_load_reg(ops, 9, src)?;
    dynasm!(ops
        ; .arch aarch64
        ; movz x15, NUMBER_TAG_HI16, lsl #48
        ; tst x9, x15                 // number → already primitive
        ; b.ne =>keep
        ; orr x15, x15, #0x2          // NOT_CELL_MASK (OTHER_TAG)
        ; tst x9, x15
        ; b.eq =>slow                 // heap cell (object/string/callable)
        ; and x14, x9, #0xffff
        ; cmp x14, FUNCTION_ID_TAG_IMM
        ; b.eq =>slow                 // closure-less function reference
        ; =>keep
    );
    emit_store_reg(ops, 9, dst)?;
    dynasm!(ops ; .arch aarch64 ; =>resume);
    slow_paths.push(CoercionSlowPath {
        entry: slow,
        resume,
        dst,
        src,
        mode: 0,
        hint,
    });
    Ok(())
}

/// Emit every deferred numeric-family completion after the hot operation
/// stream. The VM helper writes `dst` only after the full operation succeeds;
/// a live runtime therefore returns handled-or-threw and never replays an
/// observable conversion through interpreter resume.
pub(super) fn emit_numeric_slow_paths(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    table: &TransitionTable,
    slow_paths: Vec<NumericSlowPath>,
    bail: DynamicLabel,
    threw: DynamicLabel,
) {
    let entry = table.entry(abi::STUB_JIT_NUMERIC_OP);
    for path in slow_paths {
        let NumericSlowPath {
            entry: slow_entry,
            resume,
            dst,
            lhs,
            rhs_or_delta,
            opcode,
        } = path;
        dynasm!(ops
            ; .arch aarch64
            ; =>slow_entry
            ; mov x0, x20
            ; movz x1, dst as u32
            ; movz x2, lhs as u32
        );
        emit_load_u64(ops, 3, rhs_or_delta);
        emit_load_u64(ops, 4, u64::from(opcode as u8));
        emit_load_runtime_stub(ops, relocations, 16, entry, abi::STUB_JIT_NUMERIC_OP);
        dynasm!(ops
            ; .arch aarch64
            ; blr x16
            ; cmp x0, #1
            ; b.eq =>threw
            ; cmp x0, #2
            ; b.eq =>bail
            ; b =>resume
        );
    }
}

/// Emit all deferred coercion continuations after the hot operation stream.
/// The stub commits `dst` only after the VM coercion succeeds, so branching
/// back to `resume` cannot expose a partially completed operation.
pub(super) fn emit_coercion_slow_paths(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    table: &TransitionTable,
    slow_paths: Vec<CoercionSlowPath>,
    threw: DynamicLabel,
) {
    let entry = table.entry(abi::STUB_JIT_COERCE_UNARY);
    for path in slow_paths {
        let CoercionSlowPath {
            entry: slow_entry,
            resume,
            dst,
            src,
            mode,
            hint,
        } = path;
        dynasm!(ops
            ; .arch aarch64
            ; =>slow_entry
            ; mov x0, x20
            ; movz x1, dst as u32
            ; movz x2, src as u32
            ; movz x3, mode
        );
        emit_load_u64(ops, 4, u64::from(hint));
        emit_load_runtime_stub(ops, relocations, 16, entry, abi::STUB_JIT_COERCE_UNARY);
        dynasm!(ops
            ; .arch aarch64
            ; blr x16
            ; cmp x0, #1
            ; b.eq =>threw
            ; b =>resume
        );
    }
}
