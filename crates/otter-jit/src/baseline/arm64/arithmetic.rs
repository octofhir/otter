//! Numeric, comparison, conversion, and bitwise opcode emitters.
//!
//! # Contents
//! - Numeric guards and boxing macros.
//! - Integer and floating arithmetic lowering.
//! - Float-residency tracking for linear numeric regions.
//! - Fast ToInt32/ToUint32 and bitwise lowering.
//!
//! # Invariants
//! - Frame memory remains authoritative when values are FP-resident.
//! - Non-number coercions either use the planned runtime fallback or bail.
//! - Allocating addition paths publish their preplanned safepoint.

use super::*;

/// Comparison flavors that emit a `cset` from integer `cmp` flags.
pub(super) enum Cmp {
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
    Ne,
}
/// Box the int32 payload in the low 32 bits of `Xt` by setting `NUMBER_TAG`.
/// The producing op wrote `Xt` through its `W` view, which on AArch64 zeroes
/// bits [63:32], so a single `orr` with the tag completes the box.
macro_rules! box_int32 {
    ($ops:expr, $t:literal, $scratch:literal) => {
        dynasm!($ops
            ; .arch aarch64
            ; movz X($scratch), NUMBER_TAG_HI16, lsl #48
            ; orr X($t), X($t), X($scratch)
        );
    };
}

/// Box a boolean: a preceding `cset` wrote `0`/`1` into `W(t)`; adding
/// `VALUE_FALSE` yields the full `VALUE_FALSE` / `VALUE_TRUE` immediate word
/// (the high bits are already zero from the `W` write).
macro_rules! box_bool {
    ($ops:expr, $t:literal, $scratch:literal) => {
        dynasm!($ops
            ; .arch aarch64
            ; movz W($scratch), VALUE_FALSE_LOW
            ; add W($t), W($t), W($scratch)
        );
    };
}

/// Emit an int32 guard on x-register `r`: bail unless every `NUMBER_TAG` bit
/// is set (`(r & NUMBER_TAG) == NUMBER_TAG`). Clobbers x14/x15.
macro_rules! guard_int32 {
    ($ops:expr, $r:literal, $bail:expr) => {
        dynasm!($ops
            ; .arch aarch64
            ; movz x15, NUMBER_TAG_HI16, lsl #48
            ; and x14, X($r), x15
            ; cmp x14, x15
            ; b.ne =>$bail
        );
    };
}

/// Emit a "value is a Number" guard on x-register `r`: bail unless any
/// `NUMBER_TAG` bit is set (an int32 sets all of them, a boxed double at
/// least one). Cells / immediates carry none and bail. Clobbers x15.
macro_rules! guard_number {
    ($ops:expr, $r:literal, $bail:expr) => {
        dynasm!($ops
            ; .arch aarch64
            ; movz x15, NUMBER_TAG_HI16, lsl #48
            ; tst X($r), x15
            ; b.eq =>$bail
        );
    };
}

/// Emit `dst = ToPrimitive(src)` for already-primitive values. Heap cells
/// (objects, callables, strings) and bytecode-function references bail to
/// the interpreter so observable `@@toPrimitive` / `valueOf` / `toString`
/// hooks still run; numbers and the `null` / boolean / `undefined`
/// immediates pass through unchanged. Clobbers x9/x14/x15.
pub(super) fn emit_to_primitive_identity(
    ops: &mut Assembler,
    dst: u16,
    src: u16,
    bail: DynamicLabel,
) -> Result<(), Unsupported> {
    let keep = ops.new_dynamic_label();
    load_reg(ops, 9, src)?;
    dynasm!(ops
        ; .arch aarch64
        ; movz x15, NUMBER_TAG_HI16, lsl #48
        ; tst x9, x15                 // number → already primitive
        ; b.ne =>keep
        ; orr x15, x15, #value_tag::OTHER_TAG   // NOT_CELL_MASK
        ; tst x9, x15
        ; b.eq =>bail                 // heap cell (object/string/callable)
        ; and x14, x9, #0xffff
        ; cmp x14, #(FUNCTION_ID_TAG as u32)
        ; b.eq =>bail                 // closure-less function reference
        ; =>keep
    );
    store_reg(ops, 9, dst)
}

/// Integer binary ops that share the int32 fast-path shape: guard both
/// operands int32, apply a single 32-bit instruction, re-box as int32.
pub(super) enum IntBinOp {
    Or,
    And,
    Xor,
    Shl,
    Shr,
}

/// Emit `Add`/`Sub`/`Mul`: an int32 fast path that falls through to the
/// f64 path on a non-int32 operand or an overflowing int32 result (never to
/// `bail` — an overflowing integer product is just its exact f64 value). The
/// double path decodes both operands to f64, computes, and reboxes; a
/// non-number operand on that path bails to `bail`.
pub(super) fn emit_add_sub_mul(
    ops: &mut Assembler,
    operands: impl WordOperands,
    bail: DynamicLabel,
    op: Op,
) -> Result<(), Unsupported> {
    let (dst, lhs, rhs) = reg3(operands)?;
    load_reg(ops, 9, lhs)?;
    load_reg(ops, 10, rhs)?;
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
    match op {
        Op::Add => dynasm!(ops ; .arch aarch64 ; adds w13, w9, w10 ; b.vs =>float_path),
        Op::Sub => dynasm!(ops ; .arch aarch64 ; subs w13, w9, w10 ; b.vs =>float_path),
        Op::Mul => dynasm!(ops
            ; .arch aarch64
            ; smull x13, w9, w10
            ; cmp x13, w13, sxtw
            ; b.ne =>float_path
        ),
        _ => return Err(Unsupported::ArgCount(0)),
    }
    box_int32!(ops, 13, 12);
    store_reg(ops, 13, dst)?;
    dynasm!(ops ; .arch aarch64 ; b =>done ; =>float_path);
    emit_num_to_double(ops, 9, 0, bail);
    emit_num_to_double(ops, 10, 1, bail);
    match op {
        Op::Add => dynasm!(ops ; .arch aarch64 ; fadd d2, d0, d1),
        Op::Sub => dynasm!(ops ; .arch aarch64 ; fsub d2, d0, d1),
        Op::Mul => dynasm!(ops ; .arch aarch64 ; fmul d2, d0, d1),
        _ => return Err(Unsupported::ArgCount(0)),
    }
    emit_box_double(ops, 2, 13);
    store_reg(ops, 13, dst)?;
    dynasm!(ops ; .arch aarch64 ; =>done);
    Ok(())
}

/// Emit `Add` with the same numeric inline path as [`emit_add_sub_mul`],
/// but delegate non-number operands back to the VM instead of bailing out
/// of compiled code. That keeps string/boolean/null `+` loops resident in
/// baseline JIT while preserving the interpreter's full `+` semantics.
pub(super) fn emit_add_with_runtime_fallback(
    ops: &mut Assembler,
    operands: impl WordOperands,
    string_concat_safepoint: Option<SafepointId>,
    register_count: u16,
    threw: DynamicLabel,
) -> Result<(), Unsupported> {
    let (dst, lhs, rhs) = reg3(operands)?;
    load_reg(ops, 9, lhs)?;
    load_reg(ops, 10, rhs)?;
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
    box_int32!(ops, 13, 12);
    store_reg(ops, 13, dst)?;
    dynasm!(ops ; .arch aarch64 ; b =>done ; =>float_path);
    emit_num_to_double(ops, 9, 0, runtime_path);
    emit_num_to_double(ops, 10, 1, runtime_path);
    dynasm!(ops ; .arch aarch64 ; fadd d2, d0, d1);
    emit_box_double(ops, 2, 13);
    store_reg(ops, 13, dst)?;
    dynasm!(ops ; .arch aarch64 ; b =>done ; =>runtime_path);
    if let Some(safepoint) = string_concat_safepoint {
        emit_string_concat_alloc_call(
            ops,
            dst,
            lhs,
            rhs,
            safepoint,
            register_count,
            delegate_path,
            done,
        )?;
    }
    dynasm!(ops
        ; .arch aarch64
        ; =>delegate_path
        ; mov x0, x20
        ; movz x1, dst as u32
        ; movz x2, lhs as u32
        ; movz x3, rhs as u32
    );
    emit_call_stub(ops, jit_add_stub as *const () as usize, threw);
    dynasm!(ops ; .arch aarch64 ; =>done);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) fn emit_string_concat_alloc_call(
    ops: &mut Assembler,
    dst: u16,
    lhs: u16,
    rhs: u16,
    safepoint: SafepointId,
    _register_count: u16,
    miss: DynamicLabel,
    done: DynamicLabel,
) -> Result<(), Unsupported> {
    let Some(stub_addr) =
        alloc_value_stub_by_id(STUB_STRING_CONCAT_ALLOC.id).and_then(|stub| stub.entry_addr())
    else {
        return Ok(());
    };
    let undefined_bits = VALUE_UNDEFINED;
    dynasm!(ops
        ; .arch aarch64
        ; sub sp, sp, ALLOC_CTX_STACK_SIZE
        ; ldr x9, [x20, THREAD_OFFSET]
        ; str x9, [sp, ALLOC_CTX_THREAD_OFFSET]
        ; ldr x10, [x20, NATIVE_FRAME_OFFSET]
        ; str x10, [sp, ALLOC_CTX_FRAME_OFFSET]
        ; ldr x9, [x10, NATIVE_FRAME_CODE_OBJECT_ID_OFFSET]
        ; str x9, [sp, ALLOC_CTX_CODE_OBJECT_ID_OFFSET]
        ; movz w9, safepoint
        ; str w9, [sp, ALLOC_CTX_SAFEPOINT_ID_OFFSET]
        ; str wzr, [sp, ALLOC_CTX_RESERVED0_OFFSET]
        ; movz w9, #0
        ; strh w9, [sp, ALLOC_CTX_SPILL_SLOT_COUNT_OFFSET]
        ; strh w9, [sp, ALLOC_CTX_RESERVED1_OFFSET]
        ; str xzr, [sp, ALLOC_CTX_SPILL_SLOTS_OFFSET]
        ; mov x0, sp
    );
    emit_load_u64(ops, 1, u64::from(safepoint));
    load_reg(ops, 2, lhs)?;
    load_reg(ops, 3, rhs)?;
    emit_load_u64(ops, 4, undefined_bits);
    emit_load_u64(ops, 16, stub_addr as u64);
    dynasm!(ops
        ; .arch aarch64
        ; blr x16
        ; and x1, x1, #0xff
        ; mov x5, x1
        ; add sp, sp, ALLOC_CTX_STACK_SIZE
        ; cbnz x5, =>miss
    );
    store_reg(ops, 0, dst)?;
    dynasm!(ops ; .arch aarch64 ; b =>done);
    Ok(())
}

/// Emit `Div`: division always yields a Number (f64) in ECMAScript — even
/// `6 / 2` is the Number `3` — so there is no int fast path; decode both
/// operands to f64 and `fdiv`. A non-number operand bails to `bail`.
pub(super) fn emit_div(
    ops: &mut Assembler,
    operands: impl WordOperands,
    bail: DynamicLabel,
) -> Result<(), Unsupported> {
    let (dst, lhs, rhs) = reg3(operands)?;
    load_reg(ops, 9, lhs)?;
    load_reg(ops, 10, rhs)?;
    emit_num_to_double(ops, 9, 0, bail);
    emit_num_to_double(ops, 10, 1, bail);
    dynasm!(ops ; .arch aarch64 ; fdiv d2, d0, d1);
    emit_box_double(ops, 2, 13);
    store_reg(ops, 13, dst)?;
    Ok(())
}

/// Emit `Rem` (`%`): an int32 fast path that computes the truncating integer
/// remainder with `sdiv`/`msub`. Cases the integer path cannot represent
/// `bail` to the interpreter, which owns the full `f64`/`fmod` semantics:
/// a non-int32 operand, a zero divisor (`NaN`), and a zero remainder from a
/// negative dividend (JS yields `-0`, which int32 cannot encode). A zero
/// remainder from a non-negative dividend is `+0` and stays on the int path.
/// `i32::MIN % -1` needs no special case: AArch64 `sdiv` defines it as
/// `i32::MIN`, so `msub` yields the correct `0` remainder.
pub(super) fn emit_rem(
    ops: &mut Assembler,
    operands: impl WordOperands,
    bail: DynamicLabel,
) -> Result<(), Unsupported> {
    let (dst, lhs, rhs) = reg3(operands)?;
    load_reg(ops, 9, lhs)?;
    load_reg(ops, 10, rhs)?;
    guard_int32!(ops, 9, bail);
    guard_int32!(ops, 10, bail);
    let store = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch aarch64
        ; cbz w10, =>bail          // rhs == 0 → interpreter yields NaN
        ; sdiv w11, w9, w10        // truncating quotient
        ; msub w13, w11, w10, w9   // remainder = lhs - quotient * rhs
        ; cbnz w13, =>store        // nonzero remainder: sign already correct
        ; tbnz w9, #31, =>bail     // zero remainder, negative dividend → -0
        ; =>store
    );
    box_int32!(ops, 13, 12);
    store_reg(ops, 13, dst)?;
    Ok(())
}

/// First caller-saved FP register used to park decoded `f64` values.
const FP_RESIDENCY_BASE: u8 = 3;
/// Number of FP residency registers (`d3`..=`d7`). Caller-saved (`v8`–`v15`
/// are callee-saved and would force a prologue spill on every call), and
/// clobbered across calls — which is exactly where residency is cleared.
const FP_RESIDENCY_REGS: usize = 5;

/// Tracks which frame slots have their decoded `f64` currently parked in a
/// caller-saved FP register, so a later float consumer reads the register
/// instead of reloading + NaN-decoding the slot. This is the first
/// optimizing-tier slice ([`OPTIMIZING_TIER.md`] S1): a write-through read
/// cache over the linear emitter. Memory stays authoritative (every parked
/// value is also boxed and stored to its slot), so the cache is advisory —
/// dropping any entry is always sound. Unboxed numbers are not GC pointers,
/// so holding one in a register across ops cannot dangle.
#[derive(Default)]
pub(super) struct FloatResidency {
    /// `entries[i] == Some(slot)` means `d(FP_RESIDENCY_BASE + i)` holds the
    /// `f64` of frame slot `slot`.
    entries: [Option<u16>; FP_RESIDENCY_REGS],
    /// Round-robin victim for the next assignment.
    next: usize,
}

impl FloatResidency {
    /// Drop all residency — used at block boundaries (branch targets,
    /// safepoints, any op outside the modelled numeric set).
    pub(super) fn clear(&mut self) {
        self.entries = [None; FP_RESIDENCY_REGS];
    }

    /// Drop any entry for `slot` (its value in memory/registers changed).
    pub(super) fn invalidate(&mut self, slot: u16) {
        for e in self.entries.iter_mut() {
            if *e == Some(slot) {
                *e = None;
            }
        }
    }

    /// FP register currently holding `slot`'s `f64`, if any.
    pub(super) fn lookup(&self, slot: u16) -> Option<u8> {
        self.entries
            .iter()
            .position(|e| *e == Some(slot))
            .map(|i| FP_RESIDENCY_BASE + i as u8)
    }

    /// Reserve an FP register for `slot` (evicting round-robin) and return
    /// its number. The evicted slot is simply dropped from the cache — its
    /// authoritative value is still in memory.
    pub(super) fn assign(&mut self, slot: u16) -> u8 {
        self.invalidate(slot);
        let i = self.next;
        self.next = (self.next + 1) % FP_RESIDENCY_REGS;
        self.entries[i] = Some(slot);
        FP_RESIDENCY_BASE + i as u8
    }
}

/// Materialize `slot` as an `f64` in `dst_d`. Reads the parked residency
/// register when present (a plain `fmov`); otherwise loads the boxed Value
/// into `scratch_x` and NaN-decodes it, bailing on a non-number.
pub(super) fn load_operand_f64(
    ops: &mut Assembler,
    fres: &FloatResidency,
    slot: u16,
    dst_d: u8,
    scratch_x: u8,
    bail: DynamicLabel,
) -> Result<(), Unsupported> {
    if let Some(src_d) = fres.lookup(slot) {
        if src_d != dst_d {
            dynasm!(ops ; .arch aarch64 ; fmov D(dst_d), D(src_d));
        }
    } else {
        load_reg(ops, scratch_x, slot)?;
        emit_num_to_double(ops, scratch_x, dst_d, bail);
    }
    Ok(())
}

/// Residency-aware `Add`/`Sub`/`Mul`/`Div`. Computes purely in `f64` (no
/// int fast path), writes the boxed result through to the frame slot so
/// memory stays authoritative for bails/safepoints, then parks the result's
/// `f64` in a residency register for later consumers. Only used for
/// float-natured functions (those containing `Op::Div`); for an all-integer
/// result the `f64` box is the same Number as the int32 box and
/// `int32 op int32` is exact in `f64` (operands are ≤ 32-bit).
pub(super) fn emit_float_binop_res(
    ops: &mut Assembler,
    operands: impl WordOperands,
    bail: DynamicLabel,
    op: Op,
    fres: &mut FloatResidency,
) -> Result<(), Unsupported> {
    let (dst, lhs, rhs) = reg3(operands)?;
    load_operand_f64(ops, fres, lhs, 0, 9, bail)?;
    load_operand_f64(ops, fres, rhs, 1, 10, bail)?;
    match op {
        Op::Add => dynasm!(ops ; .arch aarch64 ; fadd d2, d0, d1),
        Op::Sub => dynasm!(ops ; .arch aarch64 ; fsub d2, d0, d1),
        Op::Mul => dynasm!(ops ; .arch aarch64 ; fmul d2, d0, d1),
        Op::Div => dynasm!(ops ; .arch aarch64 ; fdiv d2, d0, d1),
        _ => return Err(Unsupported::ArgCount(0)),
    }
    emit_box_double(ops, 2, 13);
    store_reg(ops, 13, dst)?;
    let park = fres.assign(dst);
    dynasm!(ops ; .arch aarch64 ; fmov D(park), d2);
    Ok(())
}

/// Residency-aware comparison: decode both operands to `f64` (from residency
/// or memory) and `fcmp`, matching the `f64` path of [`emit_cmp`]. The
/// destination receives a boolean, so its residency is dropped.
pub(super) fn emit_cmp_res(
    ops: &mut Assembler,
    operands: impl WordOperands,
    bail: DynamicLabel,
    cmp: Cmp,
    fres: &mut FloatResidency,
) -> Result<(), Unsupported> {
    let (dst, lhs, rhs) = reg3(operands)?;
    load_operand_f64(ops, fres, lhs, 0, 9, bail)?;
    load_operand_f64(ops, fres, rhs, 1, 10, bail)?;
    dynasm!(ops ; .arch aarch64 ; fcmp d0, d1);
    match cmp {
        Cmp::Lt => dynasm!(ops ; .arch aarch64 ; cset w13, mi),
        Cmp::Le => dynasm!(ops ; .arch aarch64 ; cset w13, ls),
        Cmp::Gt => dynasm!(ops ; .arch aarch64 ; cset w13, gt),
        Cmp::Ge => dynasm!(ops ; .arch aarch64 ; cset w13, ge),
        Cmp::Eq => dynasm!(ops ; .arch aarch64 ; cset w13, eq),
        Cmp::Ne => dynasm!(ops ; .arch aarch64 ; cset w13, ne),
    }
    box_bool!(ops, 13, 12);
    store_reg(ops, 13, dst)?;
    fres.invalidate(dst);
    Ok(())
}

/// Fast-path `ToInt32` for bitwise operators.
///
/// Int32-tagged values are unboxed directly. Any finite double is truncated
/// toward zero and reduced modulo 2^32 — the full ECMAScript `ToInt32`, not
/// just the already-in-range case — so an integer arithmetic result that
/// overflowed int32 into a double (e.g. `(a + b) | 0`) stays in compiled
/// code instead of bailing. Only NaN / infinity / `|x| >= 2^63` (which would
/// saturate the 64-bit `fcvtzs`) and non-number tags (string / BigInt /
/// object) bail to the interpreter for exact coercion.
pub(super) fn emit_to_int32_fast(ops: &mut Assembler, src_x: u8, dst_w: u8, bail: DynamicLabel) {
    let is_non_int = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch aarch64
        ; movz x15, NUMBER_TAG_HI16, lsl #48
        ; and x14, X(src_x), x15
        ; cmp x14, x15
        ; b.ne =>is_non_int
        ; mov W(dst_w), W(src_x)
        ; b =>done
        ; =>is_non_int
        // A boxed double carries at least one NUMBER_TAG bit; a cell or
        // tagged immediate carries none and bails for exact coercion. The
        // canonical NaN flows to the fcmp check below and bails as non-finite.
        ; tst X(src_x), x15           // any NUMBER_TAG bit → boxed double
        ; b.eq =>bail                 // cell / immediate → exact coercion
        ; movz x14, DOUBLE_OFFSET_HI16, lsl #48
        ; sub x14, X(src_x), x14      // unbox double
        ; fmov d0, x14
        ; fcmp d0, d0
        ; b.vs =>bail
    );
    // A finite double with `|x| < 2^63` truncates toward zero into i64
    // exactly (`fcvtzs`, round-to-zero); its low 32 bits are `ToInt32(x)`
    // (the truncated integer mod 2^32 mapped to the signed range). Only
    // `|x| >= 2^63` / infinity would saturate `fcvtzs`, so those bail.
    emit_load_u64(ops, 14, 9_223_372_036_854_775_808.0f64.to_bits());
    dynasm!(ops
        ; .arch aarch64
        ; fabs d1, d0
        ; fmov d2, x14
        ; fcmp d1, d2
        ; b.ge =>bail
        ; fcvtzs X(dst_w), d0
        ; =>done
    );
}

/// Fast-path `ToUint32` for unsigned shifts.
///
/// Int32-tagged values pass through as raw low-32 bits (a negative int32
/// reinterprets to its `mod 2^32` value). Any finite double is truncated
/// toward zero and reduced modulo 2^32 — the full ECMAScript `ToUint32`,
/// including negatives (`-1 >>> 0 === 4294967295`) — so it stays compiled
/// instead of bailing. Only NaN / infinity / `|x| >= 2^63` (which would
/// saturate the 64-bit `fcvtzs`) and non-number tags bail.
pub(super) fn emit_to_uint32_fast(ops: &mut Assembler, src_x: u8, dst_w: u8, bail: DynamicLabel) {
    let is_non_int = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch aarch64
        ; movz x15, NUMBER_TAG_HI16, lsl #48
        ; and x14, X(src_x), x15
        ; cmp x14, x15
        ; b.ne =>is_non_int
        ; mov W(dst_w), W(src_x)
        ; b =>done
        ; =>is_non_int
        ; tst X(src_x), x15           // any NUMBER_TAG bit → boxed double
        ; b.eq =>bail                 // cell / immediate → exact coercion
        ; movz x14, DOUBLE_OFFSET_HI16, lsl #48
        ; sub x14, X(src_x), x14      // unbox double
        ; fmov d0, x14
        ; fcmp d0, d0
        ; b.vs =>bail
    );
    // Truncate toward zero into i64 (`fcvtzs`); the low 32 bits are the
    // `mod 2^32` residue regardless of sign. Only `|x| >= 2^63` / infinity
    // would saturate, so those bail.
    emit_load_u64(ops, 14, 9_223_372_036_854_775_808.0f64.to_bits());
    dynasm!(ops
        ; .arch aarch64
        ; fabs d1, d0
        ; fmov d2, x14
        ; fcmp d1, d2
        ; b.ge =>bail
        ; fcvtzs X(dst_w), d0
        ; =>done
    );
}

/// Emit an int32 bitwise/shift op (`BitwiseOr`/`And`/`Xor`/`Shl`/`Shr`).
///
/// Operands take the guarded `ToInt32` fast path above; misses bail to the
/// interpreter. Result is int32, matching JS semantics for accepted inputs:
/// the AArch64 32-bit `lsl`/`asr` mask the shift count to its low 5 bits
/// exactly as JS masks the right operand to `& 31`.
pub(super) fn emit_int_binop(
    ops: &mut Assembler,
    operands: impl WordOperands,
    bail: DynamicLabel,
    kind: IntBinOp,
) -> Result<(), Unsupported> {
    let (dst, lhs, rhs) = reg3(operands)?;
    load_reg(ops, 9, lhs)?;
    load_reg(ops, 10, rhs)?;
    emit_to_int32_fast(ops, 9, 11, bail);
    emit_to_int32_fast(ops, 10, 12, bail);
    match kind {
        IntBinOp::Or => dynasm!(ops ; .arch aarch64 ; orr w13, w11, w12),
        IntBinOp::And => dynasm!(ops ; .arch aarch64 ; and w13, w11, w12),
        IntBinOp::Xor => dynasm!(ops ; .arch aarch64 ; eor w13, w11, w12),
        IntBinOp::Shl => dynasm!(ops ; .arch aarch64 ; lsl w13, w11, w12),
        IntBinOp::Shr => dynasm!(ops ; .arch aarch64 ; asr w13, w11, w12),
    }
    box_int32!(ops, 13, 12);
    store_reg(ops, 13, dst)?;
    Ok(())
}

/// Emit unsigned right shift. The result is boxed as a double because JS
/// `>>>` returns a uint32-valued Number and values above `i32::MAX` cannot
/// be represented by Otter's int32 tag.
pub(super) fn emit_ushr(
    ops: &mut Assembler,
    operands: impl WordOperands,
    bail: DynamicLabel,
) -> Result<(), Unsupported> {
    let (dst, lhs, rhs) = reg3(operands)?;
    load_reg(ops, 9, lhs)?;
    load_reg(ops, 10, rhs)?;
    emit_to_uint32_fast(ops, 9, 11, bail);
    emit_to_uint32_fast(ops, 10, 12, bail);
    dynasm!(ops
        ; .arch aarch64
        ; lsr w13, w11, w12
        ; ucvtf d0, w13
    );
    emit_box_double(ops, 0, 13);
    store_reg(ops, 13, dst)?;
    Ok(())
}

pub(super) fn emit_cmp(
    ops: &mut Assembler,
    operands: impl WordOperands,
    bail: DynamicLabel,
    cmp: Cmp,
) -> Result<(), Unsupported> {
    let (dst, lhs, rhs) = reg3(operands)?;
    load_reg(ops, 9, lhs)?;
    load_reg(ops, 10, rhs)?;
    let float_path = ops.new_dynamic_label();
    let have_bool = ops.new_dynamic_label();
    // int32 fast path: both operands int32 → signed integer compare.
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
    match cmp {
        Cmp::Lt => dynasm!(ops ; .arch aarch64 ; cset w13, lt),
        Cmp::Le => dynasm!(ops ; .arch aarch64 ; cset w13, le),
        Cmp::Gt => dynasm!(ops ; .arch aarch64 ; cset w13, gt),
        Cmp::Ge => dynasm!(ops ; .arch aarch64 ; cset w13, ge),
        Cmp::Eq => dynasm!(ops ; .arch aarch64 ; cset w13, eq),
        Cmp::Ne => dynasm!(ops ; .arch aarch64 ; cset w13, ne),
    }
    dynasm!(ops ; .arch aarch64 ; b =>have_bool ; =>float_path);
    if matches!(cmp, Cmp::Eq | Cmp::Ne) {
        let lhs_non_number = ops.new_dynamic_label();
        let number_path = ops.new_dynamic_label();
        let raw_identity = ops.new_dynamic_label();
        let strict_false = ops.new_dynamic_label();
        // Strict equality on non-number immediates (null / undefined /
        // boolean / hole / function id) decides by raw bit identity. Any
        // heap cell (object, string, BigInt, …) bails to the interpreter,
        // which owns object identity and string / BigInt content equality.
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
            ; orr x11, x11, #value_tag::OTHER_TAG   // NOT_CELL_MASK
            ; tst x9, x11
            ; b.eq =>bail                // lhs heap cell → interpreter
            ; tst x10, x11
            ; b.eq =>bail                // rhs heap cell → interpreter
            ; =>raw_identity
            ; cmp x9, x10
        );
        match cmp {
            Cmp::Eq => dynasm!(ops ; .arch aarch64 ; cset w13, eq),
            Cmp::Ne => dynasm!(ops ; .arch aarch64 ; cset w13, ne),
            _ => unreachable!(),
        }
        let false_value = match cmp {
            Cmp::Eq => 0,
            Cmp::Ne => 1,
            _ => unreachable!(),
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
    // Double path: decode both to f64 and `fcmp`. The FP condition codes
    // differ from the integer ones so an unordered (NaN) compare yields the
    // ECMAScript result (every relational compare false, `!=` true):
    // Lt→mi, Le→ls, Gt→gt, Ge→ge, Eq→eq, Ne→ne.
    emit_num_to_double(ops, 9, 0, bail);
    emit_num_to_double(ops, 10, 1, bail);
    dynasm!(ops ; .arch aarch64 ; fcmp d0, d1);
    match cmp {
        Cmp::Lt => dynasm!(ops ; .arch aarch64 ; cset w13, mi),
        Cmp::Le => dynasm!(ops ; .arch aarch64 ; cset w13, ls),
        Cmp::Gt => dynasm!(ops ; .arch aarch64 ; cset w13, gt),
        Cmp::Ge => dynasm!(ops ; .arch aarch64 ; cset w13, ge),
        Cmp::Eq => dynasm!(ops ; .arch aarch64 ; cset w13, eq),
        Cmp::Ne => dynasm!(ops ; .arch aarch64 ; cset w13, ne),
    }
    dynasm!(ops ; .arch aarch64 ; =>have_bool);
    box_bool!(ops, 13, 12);
    store_reg(ops, 13, dst)?;
    Ok(())
}

/// Inline abstract equality for numbers and the null/undefined equivalence
/// class. String/object/coercive cases bail before observable work to the
/// exact interpreter instruction.
pub(super) fn emit_loose_cmp(
    ops: &mut Assembler,
    operands: impl WordOperands,
    negate: bool,
    bail: DynamicLabel,
) -> Result<(), Unsupported> {
    let (dst, lhs, rhs) = reg3(operands)?;
    load_reg(ops, 9, lhs)?;
    load_reg(ops, 10, rhs)?;
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
    box_bool!(ops, 13, 12);
    store_reg(ops, 13, dst)
}
