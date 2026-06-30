//! 32-bit compressed object-slot value codec.
//!
//! An object's property slab stores each value in 4 bytes instead of a full
//! 8-byte [`Value`], halving slab footprint and GC scan bytes. The encoding
//! exploits two facts: heap cells are ≥8-aligned (low 3 bits clear) and the
//! cage is 4 GiB-aligned (the compressed offset is the low 32 bits of a cell
//! address). The low bits select the slot kind:
//!
//! ```text
//! ........1  small int  : (i31 << 1) | 1               i ∈ [-2^30, 2^30)
//! .....N000  cell ref   : the 8-aligned compressed offset, verbatim
//! .....N010  boxed num  : (HeapNumber offset) | 0b010  (double / wide int)
//! kkkkk100   immediate  : (kind << 3) | 0b100          undefined/null/...
//! .....N110  function   : (id << 3)   | 0b110          closure-less function id
//! ```
//!
//! A cell ref decodes with no header read, so reference-valued property loads
//! (the common case) stay a single decompress. Only a boxed-number slot reads
//! the heap, and only a number that does not fit `i31` allocates a box —
//! preserving the value's exact representation (a wide int32 stays int32).
//!
//! # Invariants
//!
//! - A cell offset is always 8-aligned, so tagging a boxed-number slot with
//!   `0b010` never collides with a cell-ref slot (`0b000`).
//! - The boxed-number box holds non-cell bits, so a compressed slot's only
//!   forwardable GC offsets are the `0b000` and `0b010` kinds.

use super::Value;
use crate::heap_number::{alloc_heap_number_with_roots, read_heap_number};
use otter_gc::raw::RootSlotVisitor;

/// Low-3-bit tag for a boxed-number slot.
const TAG_BOXED: u32 = 0b010;
/// Low-3-bit tag for an immediate slot.
const TAG_IMMEDIATE: u32 = 0b100;
/// Low-3-bit tag for an inline closure-less function-id slot.
const TAG_FUNCTION_ID: u32 = 0b110;
/// Mask isolating the low-3-bit slot tag.
const TAG_MASK: u32 = 0b111;
/// Largest function id that fits inline beside the 3-bit tag.
const FUNCTION_ID_LIMIT: u32 = 1 << 29;

/// Inclusive lower bound of the inline small-int range (`-2^30`).
const SMI_MIN: i32 = -(1 << 30);
/// Exclusive upper bound of the inline small-int range (`2^30`).
const SMI_LIMIT: i32 = 1 << 30;

// Immediate kinds, packed as `(kind << 3) | TAG_IMMEDIATE`.
const IMM_UNDEFINED: u32 = 0;
const IMM_NULL: u32 = 1;
const IMM_TRUE: u32 = 2;
const IMM_FALSE: u32 = 3;
const IMM_HOLE: u32 = 4;

#[inline]
const fn immediate(kind: u32) -> u32 {
    (kind << 3) | TAG_IMMEDIATE
}

// Frozen slot encoding: the optimizing tier bakes these tags into emitted
// property loads/stores and the deopt frame-state record (which must know how
// to reconstitute a full 8-byte Value from a 4-byte slot), so an accidental
// edit is a compile error here, not a silent miscompile of every slot access.
const _: () = assert!(TAG_BOXED == 0b010);
const _: () = assert!(TAG_IMMEDIATE == 0b100);
const _: () = assert!(TAG_FUNCTION_ID == 0b110);
const _: () = assert!(TAG_MASK == 0b111);
// A small int has bit 0 set; every other kind has it clear. The forwardable
// GC kinds (cell ref 0b000, boxed number 0b010) have bit 2 clear, while the
// non-pointer kinds (immediate, function id) have it set — so a single
// `bit0 clear && bit2 clear && non-zero` test isolates the offsets the
// collector relocates.
const _: () = assert!(TAG_BOXED & 0b1 == 0 && TAG_BOXED & 0b100 == 0);
const _: () = assert!(TAG_IMMEDIATE & 0b100 != 0);
const _: () = assert!(TAG_FUNCTION_ID & 0b100 != 0);
const _: () = assert!(FUNCTION_ID_LIMIT == 1 << 29);
const _: () = assert!(SMI_MIN == -(1 << 30) && SMI_LIMIT == 1 << 30);

/// A property value packed into 4 bytes. See the module docs for the layout.
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default, Debug)]
pub struct CompressedValue(pub u32);

impl CompressedValue {
    /// The empty slot (decodes to `undefined`). A freshly grown slab byte
    /// pattern.
    pub const EMPTY: Self = Self(0);

    /// `true` if the slot carries a forwardable GC offset (a cell ref or a
    /// boxed number). The collector relocates these; small ints and
    /// immediates carry none.
    #[inline]
    #[must_use]
    pub const fn is_gc_offset(self) -> bool {
        let v = self.0;
        if v & 1 == 1 || v == 0 {
            return false;
        }
        matches!(v & TAG_MASK, 0b000 | TAG_BOXED)
    }

    /// The 8-aligned GC offset of a cell-ref or boxed-number slot. Caller
    /// guarantees [`Self::is_gc_offset`].
    #[inline]
    #[must_use]
    pub const fn gc_offset(self) -> u32 {
        self.0 & !TAG_MASK
    }

    /// Rebuild a slot from a forwarded GC offset, preserving the slot tag.
    /// Caller guarantees [`Self::is_gc_offset`] held for the original.
    #[inline]
    #[must_use]
    pub const fn with_gc_offset(self, offset: u32) -> Self {
        Self((offset & !TAG_MASK) | (self.0 & TAG_MASK))
    }
}

/// Pack `value` into a 4-byte slot, boxing a double or a wide int32 on the
/// heap. Boxing can collect, so `external_visit` must forward every live
/// root the caller holds across this call (the receiver object in
/// particular); non-boxed values never allocate and leave the roots
/// untouched.
///
/// # Errors
///
/// Surfaces [`otter_gc::OutOfMemory`] from the box allocation.
pub fn compress(
    value: Value,
    heap: &mut otter_gc::GcHeap,
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<CompressedValue, otter_gc::OutOfMemory> {
    if let Some(i) = value.as_i32() {
        if (SMI_MIN..SMI_LIMIT).contains(&i) {
            return Ok(CompressedValue(((i as u32) << 1) | 1));
        }
    }
    if let Some(raw) = value.as_raw_gc() {
        debug_assert_eq!(raw.0 & TAG_MASK, 0, "cell offset must be 8-aligned");
        return Ok(CompressedValue(raw.0));
    }
    let kind = if value.is_undefined() {
        IMM_UNDEFINED
    } else if value.is_null() {
        IMM_NULL
    } else if value == Value::TRUE {
        IMM_TRUE
    } else if value == Value::FALSE {
        IMM_FALSE
    } else if value.is_hole() {
        IMM_HOLE
    } else {
        if let Some(id) = value.as_function_id() {
            if id < FUNCTION_ID_LIMIT {
                return Ok(CompressedValue((id << 3) | TAG_FUNCTION_ID));
            }
        }
        // Doubles, wide int32s, and (vanishingly rare) out-of-range function
        // ids do not fit a 4-byte slot, so box the full value bits and
        // reference the box by its offset.
        let boxed = alloc_heap_number_with_roots(heap, value.to_bits(), external_visit)?;
        return Ok(CompressedValue(boxed.offset() | TAG_BOXED));
    };
    Ok(CompressedValue(immediate(kind)))
}

/// Unpack a 4-byte slot back to a full [`Value`], reading the heap only for a
/// boxed number.
#[must_use]
pub fn decompress(slot: CompressedValue, heap: &otter_gc::GcHeap) -> Value {
    let v = slot.0;
    if v & 1 == 1 {
        return Value::number_i32((v as i32) >> 1);
    }
    match v & TAG_MASK {
        TAG_BOXED => {
            // SAFETY: a `TAG_BOXED` slot was produced by `compress` from a live
            // `alloc_heap_number`, so the offset names a `HeapNumberBody`.
            let boxed = unsafe { otter_gc::Gc::from_offset(v & !TAG_MASK) };
            Value::from_bits(read_heap_number(heap, boxed))
        }
        TAG_FUNCTION_ID => Value::function_id(v >> 3),
        TAG_IMMEDIATE => match v >> 3 {
            IMM_NULL => Value::NULL,
            IMM_TRUE => Value::TRUE,
            IMM_FALSE => Value::FALSE,
            IMM_HOLE => Value::HOLE,
            _ => Value::UNDEFINED,
        },
        _ => {
            if v == 0 {
                Value::UNDEFINED
            } else {
                Value::from_object_gc(otter_gc::raw::RawGc(v))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::alloc_object_with_roots;
    use crate::string::{JsStringId, alloc_flat_string_body_with_roots};
    use otter_gc::GcHeap;
    use otter_gc::raw::RawGc;

    #[test]
    fn round_trips_every_value_kind() {
        let mut heap = GcHeap::new().expect("heap");
        let mut roots = |_v: &mut dyn FnMut(*mut RawGc)| {};
        let obj = alloc_object_with_roots(&mut heap, &mut roots).expect("obj");
        let body = alloc_flat_string_body_with_roots(
            &mut heap,
            JsStringId::new(1),
            &[b'a' as u16],
            &mut roots,
        )
        .expect("string");

        let cases = [
            Value::undefined(),
            Value::null(),
            Value::boolean(true),
            Value::boolean(false),
            Value::hole(),
            Value::number_i32(0),
            Value::number_i32(1),
            Value::number_i32(-1),
            Value::number_i32(SMI_MIN),
            Value::number_i32(SMI_LIMIT - 1),
            // Out-of-range int32: must keep int32 representation through the box.
            Value::number_i32(SMI_LIMIT),
            Value::number_i32(i32::MAX),
            Value::number_i32(i32::MIN),
            Value::number_f64(1.5),
            Value::number_f64(-0.0),
            Value::number_f64(f64::INFINITY),
            Value::number_f64(f64::NAN),
            Value::object(obj),
            Value::string_gc(body),
            Value::function_id(0),
            Value::function_id(42),
            Value::function_id(FUNCTION_ID_LIMIT - 1),
        ];
        for v in cases {
            let c = compress(v, &mut heap, &mut |_v| {}).expect("compress");
            let back = decompress(c, &heap);
            // Exact bit round-trip (covers NaN, ±0, and int32-vs-double repr).
            assert_eq!(v.to_bits(), back.to_bits(), "{v:?}");
        }
    }

    #[test]
    fn empty_slot_is_undefined() {
        let heap = GcHeap::new().expect("heap");
        assert!(decompress(CompressedValue::EMPTY, &heap).is_undefined());
    }

    #[test]
    fn cell_and_boxed_slots_expose_forwardable_offsets() {
        let mut heap = GcHeap::new().expect("heap");
        let mut roots = |_v: &mut dyn FnMut(*mut RawGc)| {};
        let obj = alloc_object_with_roots(&mut heap, &mut roots).expect("obj");
        let cell = compress(Value::object(obj), &mut heap, &mut |_v| {}).expect("c");
        assert!(cell.is_gc_offset());
        assert_eq!(cell.gc_offset(), obj.raw().0);
        let boxed = compress(Value::number_f64(2.5), &mut heap, &mut |_v| {}).expect("c");
        assert!(boxed.is_gc_offset());
        // Re-pointing to a forwarded offset keeps the kind tag.
        let moved = boxed.with_gc_offset(boxed.gc_offset());
        assert_eq!(moved, boxed);
        assert!(
            !compress(Value::number_i32(3), &mut heap, &mut |_v| {})
                .unwrap()
                .is_gc_offset()
        );
        assert!(
            !compress(Value::null(), &mut heap, &mut |_v| {})
                .unwrap()
                .is_gc_offset()
        );
    }
}
