//! Boxing and side-exit deopt for the Cranelift backend.
//!
//! Cranelift has no native deoptimization, so every speculation guard lowers to a
//! cold side-exit block that reconstructs the interpreter frame and returns
//! `Bailed(byte_pc)` — the identical contract the dynasm tier and the VM already
//! use (CRANELIFT_TIER2.md §4). This module provides the value boxing those exits
//! (and edge/return boxing) need, plus [`emit_bail`], which materializes one
//! [`DeoptPoint`] into the frame register window and returns the bail status.
//!
//! # Contents
//! - [`box_tagged`] — unboxed `Int32`/`Float64`/`Bool` → NaN-boxed `Value` bits.
//! - [`emit_bail`] — store the live registers, stamp `bail_pc`, return `Bailed`.
//!
//! # Invariants
//! - A `Float64` is boxed to its bits verbatim, except a `NaN` is canonicalized
//!   to `TAG_NAN << 48` so it never aliases a NaN-box tag (matching the dynasm
//!   `emit_box_double`). Every non-`NaN` double is its own boxed representation,
//!   so the stored bits are bit-identical to the dynasm and interpreter paths.
//! - The live values a bail stores are operands of, or dominators of, the guard,
//!   so they dominate the cold exit block and are readable there.
//!
//! # See also
//! - [`super::lower`] — emits the guards that branch to these exits.
//! - [`super::deopt`](crate::optimizing::deopt) — the [`DeoptPoint`] source.

use cranelift_codegen::ir::condcodes::FloatCC;
use cranelift_codegen::ir::{InstBuilder, MemFlagsData, Value, types};
use cranelift_frontend::FunctionBuilder;

/// Memory-access flags a function reuses: `trusted` (aligned, non-trapping) for
/// mutable accesses (the frame register window, element data); `readonly` adds
/// the immutable flag for loads of never-written fields (object/buffer metadata),
/// which lets Cranelift's GVN/LICM dedup and hoist them out of loops across the
/// intervening element stores; `plain` for the `f64`↔`i64` boxing bitcast. The
/// instruction builder interns these into the DFG itself, so they are passed by
/// value.
#[derive(Clone, Copy)]
pub(super) struct Flags {
    pub trusted: MemFlagsData,
    pub readonly: MemFlagsData,
    pub plain: MemFlagsData,
}

use super::abi::{BAIL_PC_OFFSET, FALSE_BITS, STATUS_BAILED, TAG_INT32, TAG_NAN, TRUE_BITS};
use crate::optimizing::deopt::DeoptPoint;
use crate::optimizing::ir::{Graph, Repr};

/// Materialize the NaN-boxed `Value` bits (an `i64`) of `v`, given its `repr`.
///
/// `Tagged` values are already boxed and returned unchanged. `Int32` packs the
/// low 32 bits under [`TAG_INT32`]; `Bool` selects the boxed `true`/`false`
/// constants; `Float64` is its bits verbatim with a `NaN` canonicalized so it
/// never lands in the tag range.
#[must_use]
pub(super) fn box_tagged(b: &mut FunctionBuilder, flags: Flags, v: Value, repr: Repr) -> Value {
    match repr {
        Repr::Tagged => v,
        Repr::Int32 => {
            let widened = b.ins().uextend(types::I64, v);
            let tag = b.ins().iconst(types::I64, (TAG_INT32 << 48) as i64);
            b.ins().bor(widened, tag)
        }
        Repr::Bool => {
            let t = b.ins().iconst(types::I64, TRUE_BITS as i64);
            let f = b.ins().iconst(types::I64, FALSE_BITS as i64);
            // `v` is the `i8` 0/1 predicate; non-zero selects the boxed `true`.
            b.ins().select(v, t, f)
        }
        Repr::Float64 => {
            let bits = b.ins().bitcast(types::I64, flags.plain, v);
            // `Unordered(v, v)` is true iff `v` is NaN.
            let is_nan = b.ins().fcmp(FloatCC::Unordered, v, v);
            let canonical = b.ins().iconst(types::I64, (TAG_NAN << 48) as i64);
            b.ins().select(is_nan, canonical, bits)
        }
    }
}

/// Emit the body of a cold side-exit: box each live register to its tagged
/// `Value`, store it into the frame register window, stamp the resume byte-PC
/// into `JitCtx.bail_pc`, and return `Bailed`.
///
/// `values` is the lowering's SSA node → Cranelift value map. A live register
/// whose value has no Cranelift home is a lowering bug, surfaced as `None` so the
/// whole compile declines and falls back to the dynasm tier.
pub(super) fn emit_bail(
    b: &mut FunctionBuilder,
    flags: Flags,
    ctx_ptr: Value,
    regs_base: Value,
    point: &DeoptPoint,
    graph: &Graph,
    values: &[Option<Value>],
) -> Result<(), super::Unsupported> {
    for &(regn, value) in &point.registers {
        let v = values[value as usize].ok_or(super::Unsupported::Unlowered(
            "clif: deopt value without home",
        ))?;
        let boxed = box_tagged(b, flags, v, graph.node(value).repr);
        let off = i32::from(regn) * 8;
        b.ins().store(flags.trusted, boxed, regs_base, off);
    }
    let pc = b.ins().iconst(types::I32, i64::from(point.byte_pc));
    b.ins().store(flags.trusted, pc, ctx_ptr, BAIL_PC_OFFSET);
    // The bail value is ignored by `enter_compiled` on `STATUS_BAILED` (it reads
    // `ctx.bail_pc`); return a zero placeholder in the value slot.
    let zero = b.ins().iconst(types::I64, 0);
    let status = b.ins().iconst(types::I64, STATUS_BAILED);
    b.ins().return_(&[zero, status]);
    Ok(())
}
