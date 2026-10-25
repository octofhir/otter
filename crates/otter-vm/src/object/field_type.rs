//! Shape-owned summaries of values stored in ordinary data fields.
//!
//! # Contents
//! - Int32 constants, numeric/category unions and two exact shapes describe a field.
//! - `owner` finds the shape which introduced a logical slot.
//! - `snapshot` and `observe_owner` read and widen its scalar summary.
//! - `seed` accounts for initialized fields at birth and shape adoption.
//!
//! # Invariants
//! - Every live field is observed before another mutator operation can read it.
//! - Append descendants share the defining shape's summary; independent
//!   lineages observe their retained values when adopted.
//! - Other primitives, opaque objects and wider unions produce `TOP`, which
//!   never narrows. Int32 denotes the stored representation, not integer-valued
//!   doubles. Shape identities are range-checked before packing.
//! - An exact Int32 word retains the first value until a different observation
//!   permanently replaces it with a category union. Its payload is not a shape.
//! - Union counts match their canonical sorted identities. Every widening changes
//!   the low byte, which cannot return to a former value in one owner's history.
//!   Comparing that byte with a captured byte proves the whole word unchanged.
//! - Shape identities describe values at observation time. A loaded-value
//!   bound additionally requires every target shape to remain stable. A
//!   writer's exact current shape can match an unstable historical identity.
//! - One mutator widens summaries. The words contain identities, not handles,
//!   so collector relocation neither traces nor changes them.
//!
//! # See also
//! - `super::shape_body` owns the non-moving atomic words.
//! - `super::ObjectBody` owns field publication and adoption.

use crate::Value;

use super::{ObjectBody, ShapeHandle, shape_body};

/// Undefined belongs to this field's stored-value union.
pub const UNDEFINED: u64 = 1;
/// Null belongs to this field's stored-value union.
pub const NULL: u64 = 1 << 1;
/// An int32-tagged number belongs to this field's stored-value union.
pub const INT32: u64 = 1 << 4;
/// The word contains one exact Int32 payload instead of shape identities.
pub const CONST_INT32: u64 = 1 << 5;
/// An offset-encoded double belongs to this field's stored-value union.
pub const DOUBLE: u64 = 1 << 6;
/// Bit position of the count of packed exact identities.
pub const SHAPE_COUNT_SHIFT: u32 = 2;
/// Two count bits encode zero, one or two exact identities.
pub const SHAPE_COUNT_MASK: u64 = 3 << SHAPE_COUNT_SHIFT;
/// Low bits present in a constrained canonical summary.
pub const VALID_LOW_BITS: u64 = UNDEFINED | NULL | INT32 | CONST_INT32 | DOUBLE | SHAPE_COUNT_MASK;
/// Any value can belong to this field's stored-value union.
pub const TOP: u64 = 1 << 7;
/// Largest exact identity which fits a packed shape slot.
pub const SHAPE_MASK: u64 = (1 << 28) - 1;
/// Bit position of the first packed shape identity.
pub const FIRST_SHAPE_SHIFT: u32 = 8;
/// Bit position of the second packed shape identity.
pub const SECOND_SHAPE_SHIFT: u32 = 36;

const SCALAR_BITS: u64 = UNDEFINED | NULL | INT32 | DOUBLE;
const LOW_MASK: u64 = (1 << FIRST_SHAPE_SHIFT) - 1;

/// First observation of one Int32-tagged value.
#[must_use]
pub const fn int32_sample(value: i32) -> u64 {
    INT32 | CONST_INT32 | ((value as u32 as u64) << FIRST_SHAPE_SHIFT)
}

/// The exact Int32 value of a canonical constant word.
#[must_use]
pub const fn int32_constant(word: u64) -> Option<i32> {
    if word & LOW_MASK == INT32 | CONST_INT32 && word >> (FIRST_SHAPE_SHIFT + 32) == 0 {
        Some((word >> FIRST_SHAPE_SHIFT) as u32 as i32)
    } else {
        None
    }
}

/// Whether the entire stored-value bound has an Int32 representation.
#[must_use]
pub const fn is_int32_only(word: u64) -> bool {
    word == INT32 || int32_constant(word).is_some()
}

/// Whether every stored alternative is an Int32 or encoded double number.
#[must_use]
pub const fn is_number_only(word: u64) -> bool {
    int32_constant(word).is_some() || (word != 0 && word & !(INT32 | DOUBLE) == 0)
}

/// Encode one exact shape identity, saturating identities outside the range.
#[must_use]
pub const fn shape_id_sample(id: u64) -> u64 {
    if id == 0 || id > SHAPE_MASK {
        TOP
    } else {
        (1 << SHAPE_COUNT_SHIFT) | (id << FIRST_SHAPE_SHIFT)
    }
}

/// Whether a word has a complete canonical identity count and ordering.
#[must_use]
pub(crate) const fn is_canonical(word: u64) -> bool {
    if word == TOP {
        return true;
    }
    if word & CONST_INT32 != 0 {
        return int32_constant(word).is_some();
    }
    if word & LOW_MASK & !VALID_LOW_BITS != 0 {
        return false;
    }
    let first = (word >> FIRST_SHAPE_SHIFT) & SHAPE_MASK;
    let second = (word >> SECOND_SHAPE_SHIFT) & SHAPE_MASK;
    match (word & SHAPE_COUNT_MASK) >> SHAPE_COUNT_SHIFT {
        0 => first == 0 && second == 0,
        1 => first != 0 && second == 0,
        2 => first != 0 && first < second,
        _ => false,
    }
}

/// Join two summaries with sorted exact identities and an absorbing top.
#[must_use]
pub fn merge(left: u64, right: u64) -> u64 {
    if left == TOP || right == TOP || !is_canonical(left) || !is_canonical(right) {
        return TOP;
    }
    if left == 0 {
        return right;
    }
    if right == 0 || left == right {
        return left;
    }
    // Only the first observation or an identical write retains constness.
    // Category unions never become constants again, even after equal stores.
    let left = if left & CONST_INT32 != 0 { INT32 } else { left };
    let right = if right & CONST_INT32 != 0 {
        INT32
    } else {
        right
    };
    let mut first = 0;
    let mut second = 0;
    for id in [
        (left >> FIRST_SHAPE_SHIFT) & SHAPE_MASK,
        (left >> SECOND_SHAPE_SHIFT) & SHAPE_MASK,
        (right >> FIRST_SHAPE_SHIFT) & SHAPE_MASK,
        (right >> SECOND_SHAPE_SHIFT) & SHAPE_MASK,
    ] {
        if id == 0 || id == first || id == second {
            continue;
        }
        if first == 0 {
            first = id;
        } else if second == 0 {
            second = id;
        } else {
            return TOP;
        }
    }
    if second != 0 && second < first {
        std::mem::swap(&mut first, &mut second);
    }
    let count = u64::from(first != 0) + u64::from(second != 0);
    (left | right) & SCALAR_BITS
        | (count << SHAPE_COUNT_SHIFT)
        | (first << FIRST_SHAPE_SHIFT)
        | (second << SECOND_SHAPE_SHIFT)
}

/// Defining data-field shape for a logical slot in a live shaped lineage.
#[must_use]
pub(crate) fn owner(shape: ShapeHandle, logical_slot: u32) -> Option<ShapeHandle> {
    shape_body::field_owner(shape, logical_slot)
}

/// Writer authority, with null reserved for an already absorbing top summary.
#[must_use]
pub(crate) fn observer_owner(shape: ShapeHandle, logical_slot: u32) -> Option<ShapeHandle> {
    let owner = owner(shape, logical_slot)?;
    Some(if snapshot(owner) == TOP {
        ShapeHandle::null()
    } else {
        owner
    })
}

/// Current packed summary of a live defining data-field shape.
#[must_use]
pub(crate) fn snapshot(owner: ShapeHandle) -> u64 {
    shape_body::field_type_snapshot(owner)
}

/// Install a packed summary for isolated metadata fixtures.
#[cfg(test)]
pub(crate) fn set_snapshot_for_test(owner: ShapeHandle, value: u64) {
    shape_body::set_field_type_snapshot(owner, value);
}

/// Observe a value without allocation, collection or user-code execution.
pub(crate) fn observe_owner(owner: ShapeHandle, value: Value) {
    let old = snapshot(owner);
    if old == TOP {
        return;
    }
    let sample = classify(value);
    // Most stores repeat a category the summary already holds.
    let covered = match sample {
        UNDEFINED | NULL | DOUBLE => old & CONST_INT32 == 0 && old & sample != 0,
        _ if sample & CONST_INT32 != 0 => {
            old == sample || (old & CONST_INT32 == 0 && old & INT32 != 0)
        }
        _ => false,
    };
    if covered {
        return;
    }
    let next = merge(old, sample);
    if next != old {
        shape_body::set_field_type_snapshot(owner, next);
    }
}

/// Private non-collecting writer boundary for admitted generated field stores.
pub(crate) extern "C" fn observe_native(
    heap: *mut otter_gc::GcHeap,
    owner_offset: u64,
    rhs_bits: u64,
) -> crate::native_abi::NativeResultPair {
    let Ok(offset) = u32::try_from(owner_offset) else {
        return crate::native_abi::NativeResultPair::miss();
    };
    if heap.is_null() || offset == 0 || !offset.is_multiple_of(8) {
        return crate::native_abi::NativeResultPair::miss();
    }
    // SAFETY: admitted callers retain this defining shape and the boxed RHS
    // for this synchronous boundary. It cannot allocate, collect or reenter.
    let owner = unsafe { otter_gc::Gc::from_offset(offset) };
    observe_owner(owner, Value::from_abi_bits(rhs_bits));
    crate::native_abi::NativeResultPair::success(Value::UNDEFINED)
}

pub(super) fn observe_slot(shape: ShapeHandle, logical_slot: usize, value: Value) {
    if let Ok(slot) = u32::try_from(logical_slot)
        && let Some(owner) = owner(shape, slot)
    {
        observe_owner(owner, value);
    }
}

/// Widen the summary of each of `shape`'s fields to [`TOP`]: a layout whose
/// cells may hold anything there tracks nothing, so seeding and stores into
/// those fields return at their first test.
pub(crate) fn generalize(shape: ShapeHandle) {
    if shape.is_null() || shape_body::is_dictionary_of(shape) {
        return;
    }
    let mut cursor = shape;
    while shape_body::property_count_of(cursor) != 0 {
        let slot = shape_body::property_count_of(cursor) - 1;
        if let Some(owner) = owner(cursor, slot)
            && snapshot(owner) != TOP
        {
            shape_body::set_field_type_snapshot(owner, TOP);
        }
        cursor = shape_body::parent_of(cursor);
    }
}

pub(super) fn seed(shape: ShapeHandle, count: usize, mut read: impl FnMut(u32) -> Value) {
    if shape.is_null() || shape_body::is_dictionary_of(shape) {
        return;
    }
    let mut cursor = shape;
    while shape_body::property_count_of(cursor) != 0 {
        let slot = shape_body::property_count_of(cursor) - 1;
        if (slot as usize) < count
            && let Some(owner) = owner(cursor, slot)
        {
            observe_owner(owner, read(slot));
        }
        cursor = shape_body::parent_of(cursor);
    }
}

fn classify(value: Value) -> u64 {
    if let Some(value) = value.as_i32() {
        return int32_sample(value);
    }
    if value.is_number() {
        return DOUBLE;
    }
    if value.is_undefined() {
        return UNDEFINED;
    }
    if value.is_null() {
        return NULL;
    }
    let Some(object) = value.as_object() else {
        return TOP;
    };
    // SAFETY: the checked object value names a resident body. Observation is
    // non-collecting, so the shape read cannot be overtaken by relocation.
    let shape = unsafe {
        (*(object
            .as_header_ptr()
            .cast::<u8>()
            .add(otter_gc::header::HEADER_SIZE)
            .cast::<ObjectBody>()))
        .shape
    };
    if shape.is_null() {
        return TOP;
    }
    let state = shape_body::state_of(shape);
    if state.is_dictionary() || state.is_opaque() {
        return TOP;
    }
    shape_id_sample(shape_body::id_of(shape).raw())
}

#[cfg(test)]
mod tests;
