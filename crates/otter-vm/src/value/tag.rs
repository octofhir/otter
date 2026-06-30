//! Pointer-cheap NaN-box encoding for [`crate::value::Value`].
//!
//! # Encoding
//!
//! Every `Value` is a single `u64` pointer-cheap NaN-box, chosen so that
//! **heap pointers are stored verbatim** and need no unmask before a
//! dereference — the hot property/method path pays nothing to turn a `Value`
//! into an object address. Doubles pay a single add/subtract at box/unbox
//! time; that is the conscious trade (we are already competitive on float
//! benches).
//!
//! ```text
//! Cell pointer : the full 48-bit address, top 16 bits zero.
//!                v & (NUMBER_TAG | OTHER_TAG) == 0  &&  v != 0
//! Int32        : NUMBER_TAG | (i32 as u32)
//!                (v & NUMBER_TAG) == NUMBER_TAG
//! Double       : f64.bits + DOUBLE_ENCODE_OFFSET   (NaN purified first)
//!                (v & NUMBER_TAG) != 0  &&  not int32
//! Immediates   : null / undefined / false / true / hole — OTHER_TAG-tagged
//!                low-bit patterns, top 16 bits zero, never a number/cell.
//! FunctionId   : (id as u64) << 16 | FUNCTION_ID_TAG  (closure-less ref)
//! ```
//!
//! Heap pointers carry the **full** `cage_base + offset` address. Because
//! the GC cage is 4 GiB-aligned (`otter_gc::CAGE_ALIGN_BYTES`), the low 32
//! bits of that address are exactly the [`otter_gc::raw::RawGc`] compressed
//! offset, and the high half is the constant cage prefix. So the moving
//! collector still rewrites the 4-byte offset in place during relocation
//! (the address auto-updates), while the interpreter derefs the full
//! pointer directly. The four object families (object / string / callable /
//! other) are read from [`otter_gc::header::GcHeader::type_tag`], not from
//! the value bits.
//!
//! # Invariants
//!
//! - Cell addresses are ≥8-aligned, so a cell never has `OTHER_TAG` (bit 1)
//!   set; that is how a cell is told apart from a tagged immediate.
//! - Every incoming NaN is purified to [`CANONICAL_NAN`] before the double
//!   offset is applied, so no encoded double aliases the cell space.
//! - The cage base is 4 GiB-aligned and in the low-48-bit VA window
//!   (asserted at cage init), so `top16 == 0` for every cell.
//!
//! # Spec
//!
//! - ECMA-262 §6.1 ECMAScript Language Types
//! - ECMA-262 §6.1.6.1 The Number Type (NaN canonicalisation)

/// High-bits tag marking a number (int32 or boxed double).
pub const NUMBER_TAG: u64 = 0xfffe_0000_0000_0000;

/// Low-bit tag distinguishing a tagged immediate from a cell pointer.
/// A cell never has this bit set.
pub const OTHER_TAG: u64 = 0x2;

/// Low-bit marker shared by the two boolean immediates.
pub const BOOL_TAG: u64 = 0x4;

/// Low-bit marker for `undefined`.
pub const UNDEFINED_TAG: u64 = 0x8;

/// Added to a purified `f64`'s bit pattern when boxing a double, and
/// subtracted when unboxing. `2^49`.
pub const DOUBLE_ENCODE_OFFSET: u64 = 0x0002_0000_0000_0000;

/// `(v & NOT_CELL_MASK) == 0` (and non-zero) identifies a cell pointer.
pub const NOT_CELL_MASK: u64 = NUMBER_TAG | OTHER_TAG;

/// Canonical quiet NaN — every `Number(NaN)` boxes from this single
/// `f64` bit pattern so all NaNs compare bit-equal and none of them
/// collide with the cell space once the double offset is applied.
pub const CANONICAL_NAN: u64 = 0x7ff8_0000_0000_0000;

// ---------------------------------------------------------------------------
// Tagged immediates (full `u64` values; top 16 bits zero, OTHER_TAG set).
// ---------------------------------------------------------------------------

/// `null` immediate.
pub const VALUE_NULL: u64 = OTHER_TAG;
/// `false` immediate.
pub const VALUE_FALSE: u64 = OTHER_TAG | BOOL_TAG;
/// `true` immediate.
pub const VALUE_TRUE: u64 = OTHER_TAG | BOOL_TAG | 0x1;
/// `undefined` immediate.
pub const VALUE_UNDEFINED: u64 = OTHER_TAG | UNDEFINED_TAG;
/// Internal "array hole" sentinel — never observed by user code. A
/// distinct OTHER_TAG-bearing pattern, disjoint from the spec immediates.
pub const VALUE_HOLE: u64 = OTHER_TAG | 0x10;

/// Low-16-bit tag selecting the closure-less bytecode-function-id
/// immediate. The function id lives in bits `[16, 48)`; the low 16 bits
/// are this constant. OTHER_TAG keeps it out of the cell space and the
/// `0x20` marker keeps it disjoint from the other immediates.
pub const FUNCTION_ID_TAG: u64 = OTHER_TAG | 0x20;

// Frozen encoding: the optimizing tier bakes these exact bit patterns into
// emitted box/unbox and the deopt frame-state record, so an accidental edit
// is a compile error here rather than a silent interpreter/JIT divergence.
// Move a literal only in lockstep with the codegen that bakes it.
const _: () = assert!(NUMBER_TAG == 0xfffe_0000_0000_0000);
const _: () = assert!(OTHER_TAG == 0x2);
const _: () = assert!(BOOL_TAG == 0x4);
const _: () = assert!(UNDEFINED_TAG == 0x8);
const _: () = assert!(DOUBLE_ENCODE_OFFSET == 1 << 49);
const _: () = assert!(NOT_CELL_MASK == NUMBER_TAG | OTHER_TAG);
const _: () = assert!(CANONICAL_NAN == 0x7ff8_0000_0000_0000);
// A cell pointer carries none of the non-cell bits; every tagged immediate
// carries OTHER_TAG, so a cell is never mistaken for an immediate (and vice
// versa) by the single `bits & NOT_CELL_MASK` test the guard lowers.
const _: () = assert!(OTHER_TAG & 1 == 0 && OTHER_TAG != 0);
const _: () = assert!(VALUE_NULL & NOT_CELL_MASK != 0);
const _: () = assert!(VALUE_UNDEFINED & NOT_CELL_MASK != 0);
const _: () = assert!(VALUE_TRUE & NOT_CELL_MASK != 0);
const _: () = assert!(VALUE_FALSE & NOT_CELL_MASK != 0);
const _: () = assert!(VALUE_HOLE & NOT_CELL_MASK != 0);
const _: () = assert!(FUNCTION_ID_TAG & NOT_CELL_MASK != 0);
// The boxed-double offset purifies into the number space: a boxed double's
// top bits carry NUMBER_TAG, disjoint from the immediate patterns above.
const _: () = assert!(CANONICAL_NAN.wrapping_add(DOUBLE_ENCODE_OFFSET) & NUMBER_TAG != 0);

/// `true` if the bit pattern encodes a number (int32 or boxed double).
#[inline(always)]
pub const fn is_number_bits(bits: u64) -> bool {
    (bits & NUMBER_TAG) != 0
}

/// `true` if the bit pattern encodes an int32 immediate.
#[inline(always)]
pub const fn is_int32_bits(bits: u64) -> bool {
    (bits & NUMBER_TAG) == NUMBER_TAG
}

/// `true` if the bit pattern encodes a boxed (offset) double.
#[inline(always)]
pub const fn is_double_bits(bits: u64) -> bool {
    is_number_bits(bits) && !is_int32_bits(bits)
}

/// `true` if the bit pattern is a heap-cell pointer (full address,
/// top 16 bits zero, `OTHER_TAG` clear, non-zero).
#[inline(always)]
pub const fn is_cell_bits(bits: u64) -> bool {
    (bits & NOT_CELL_MASK) == 0 && bits != 0
}

/// `true` if the bit pattern is the closure-less function-id immediate.
#[inline(always)]
pub const fn is_function_id_bits(bits: u64) -> bool {
    !is_number_bits(bits) && (bits & 0xFFFF) == FUNCTION_ID_TAG
}

/// Box an `i32` into its int32 immediate pattern.
#[inline(always)]
pub const fn box_int32(n: i32) -> u64 {
    NUMBER_TAG | (n as u32 as u64)
}

/// Unbox an int32 immediate. Caller guarantees [`is_int32_bits`].
#[inline(always)]
pub const fn unbox_int32(bits: u64) -> i32 {
    bits as u32 as i32
}

/// Box an `f64` (already NaN-purified) into its offset-double pattern.
#[inline(always)]
pub const fn box_double(bits: u64) -> u64 {
    bits.wrapping_add(DOUBLE_ENCODE_OFFSET)
}

/// Unbox an offset-double back to raw `f64` bits. Caller guarantees
/// [`is_double_bits`].
#[inline(always)]
pub const fn unbox_double(bits: u64) -> u64 {
    bits.wrapping_sub(DOUBLE_ENCODE_OFFSET)
}

/// Box a closure-less function id into its immediate pattern.
#[inline(always)]
pub const fn box_function_id(id: u32) -> u64 {
    ((id as u64) << 16) | FUNCTION_ID_TAG
}

/// Unbox a function-id immediate. Caller guarantees [`is_function_id_bits`].
#[inline(always)]
pub const fn unbox_function_id(bits: u64) -> u32 {
    (bits >> 16) as u32
}

/// Low 32 bits of a cell pointer — the [`otter_gc::raw::RawGc`] offset,
/// since the cage base is 4 GiB-aligned.
#[inline(always)]
pub const fn cell_offset(bits: u64) -> u32 {
    bits as u32
}
