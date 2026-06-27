//! Shared ABI constants and CLIF-type mapping for the Cranelift backend.
//!
//! The Cranelift backend emits functions with the *identical* entry ABI as the
//! dynasm tier — `extern "C" fn(*mut JitCtx) -> JitRet` — so both run through the
//! one shared [`crate::baseline::enter_compiled`] and decode their result the
//! same way. This module re-exports the NaN-box tags, `JitCtx` field offsets, and
//! `JitRet` status discriminants the lowering and deopt code bake into IR, and
//! maps the typed-SSA [`Repr`] lattice onto Cranelift value types.
//!
//! # Contents
//! - [`clif_type`] — `Repr` → Cranelift [`Type`].
//! - NaN-box tag re-exports and `JitCtx`/`JitRet` ABI constants.
//!
//! # Invariants
//! - The compiled function signature is `(i64 ctx) -> (i64 value, i64 status)`.
//!   Under the host `extern "C"` call convention the two return values land in
//!   the same registers a `#[repr(C)] JitRet { value, status }` does (x0/x1 on
//!   aarch64), so `enter_compiled`'s transmute-and-decode is exact for both
//!   backends.
//! - `JitCtx.regs` is field 0, so the register-window base is `load [ctx + 0]`.
//!
//! # See also
//! - [`crate::baseline`] — the `JitCtx` definition and the shared entry path.
//! - [`super::lower`] — the consumer of [`clif_type`] and the box/unbox helpers.

use cranelift_codegen::ir::{Type, types};

use crate::optimizing::ir::Repr;

/// NaN-box tag for a 32-bit signed integer immediate (`value/tag.rs`).
pub(super) const TAG_INT32: u64 = crate::baseline::TAG_INT32;
/// NaN-box tag for special immediates (undefined/null/hole/boolean).
pub(super) const TAG_SPECIAL: u64 = crate::baseline::TAG_SPECIAL;
/// NaN-box tag for object-class heap pointers (`value/tag.rs`). The low 32 bits
/// are a `Gc` offset added to the cage base to decompress the pointer.
pub(super) const TAG_PTR_OBJECT: u64 = crate::baseline::TAG_PTR_OBJECT;
/// NaN-box high-16 for the canonical quiet NaN double. A non-int double result
/// whose own bits land in the tagged range is canonicalised to this so it stays
/// a valid `Number(NaN)` and never aliases a tag.
pub(super) const TAG_NAN: u64 = 0x7FF8;
/// `SPECIAL` payload for `false`.
pub(super) const SPECIAL_FALSE: u64 = crate::baseline::SPECIAL_FALSE as u64;
/// `SPECIAL` payload for `true`.
pub(super) const SPECIAL_TRUE: u64 = crate::baseline::SPECIAL_TRUE as u64;

/// Boxed `undefined` bit pattern (`TAG_SPECIAL << 48`).
pub(super) const UNDEFINED_BITS: u64 = TAG_SPECIAL << 48;
/// Boxed `null` bit pattern.
pub(super) const NULL_BITS: u64 = (TAG_SPECIAL << 48) | crate::baseline::SPECIAL_NULL;
/// Boxed `false` bit pattern.
pub(super) const FALSE_BITS: u64 = (TAG_SPECIAL << 48) | SPECIAL_FALSE;
/// Boxed `true` bit pattern.
pub(super) const TRUE_BITS: u64 = (TAG_SPECIAL << 48) | SPECIAL_TRUE;
/// Boxed array/`this` hole sentinel bit pattern (a dense-array element slot that
/// is a hole misses the inline fast path and deopts).
pub(super) const HOLE_BITS: u64 = (TAG_SPECIAL << 48) | crate::baseline::SPECIAL_HOLE;

/// `JitRet.status` for a normal return (`Returned(value)`).
pub(super) const STATUS_RETURNED: i64 = crate::baseline::STATUS_RETURNED as i64;
/// `JitRet.status` for a guard bail (`Bailed(bail_pc)`).
pub(super) const STATUS_BAILED: i64 = crate::baseline::STATUS_BAILED as i64;
/// `JitRet.status` for a parked VM error.
pub(super) const STATUS_THREW: i64 = crate::baseline::STATUS_THREW as i64;

/// Byte offset of the register-window base pointer within `JitCtx` (field 0).
pub(super) const REGS_OFFSET: i32 = 0;
/// Byte offset of `JitCtx.bail_pc`, where a side exit stamps the resume byte-PC.
pub(super) const BAIL_PC_OFFSET: i32 = crate::baseline::BAIL_PC_OFFSET as i32;
/// Byte offset of `JitCtx.array_index_accessor_protector_ptr`. A dense-array
/// element store reads through it at the store site (a re-entered call can
/// invalidate the protector), deopting when the protector is live.
pub(super) const ARRAY_INDEX_ACCESSOR_PROTECTOR_PTR_OFFSET: u32 =
    crate::baseline::ARRAY_INDEX_ACCESSOR_PROTECTOR_PTR_OFFSET;

/// Cranelift value type a typed-SSA value of `repr` is materialized in.
///
/// `Tagged` is the NaN-boxed `Value` (an opaque `i64`); `Int32`/`Float64` are
/// unboxed numeric islands; `Bool` is the `i8` predicate Cranelift's `icmp` /
/// `fcmp` produce.
#[must_use]
pub(super) fn clif_type(repr: Repr) -> Type {
    match repr {
        Repr::Tagged => types::I64,
        Repr::Int32 => types::I32,
        Repr::Float64 => types::F64,
        Repr::Bool => types::I8,
    }
}
