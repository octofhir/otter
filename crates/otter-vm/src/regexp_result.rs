//! Folded construction of the array a RegExp match returns.
//!
//! # Contents
//! - [`build_match_result_folded`] — the match array, its substrings and its
//!   `index` / `input` / `groups` record, taken from one allocation-buffer
//!   reservation.
//!
//! # Invariants
//! - One reservation admits every cell of the result. No allocation after it
//!   can collect, so the raw handles the construction holds stay valid. The
//!   subject and the receiver are rooted across the reservation and the
//!   record layout, which may collect even when the reservation is declined:
//!   the caller's copies are updated in place.
//! - The cells are the ones the scoped builder
//!   ([`crate::regexp_prototype::build_match_result_native`]) makes: a
//!   substring per participating group, `undefined` for the others, a dense
//!   tagged array of them, and an object of the record layout's shape,
//!   field types seeded, attached as the array's expando.
//! - A declined reservation (collector stress, marking, bootstrap tenuring)
//!   and a pattern with named groups leave the result to the scoped builder.
//!   `hasIndices` is the caller's to route there.
//!
//! # See also
//! - [`crate::regexp_fast::exec_direct`] produces the match this builds.
//! - [`otter_gc::GcHeap::ensure_machine_allocation_with_roots`] makes the
//!   reservation generated allocation groups also use.

use crate::regexp::engine::MatchOffsets;
use crate::rooting::RootScopeExt;
use crate::{Interpreter, Value, VmError};

/// The array `exec` of `receiver` returns for `matched` in `input`, or
/// `None` when the scoped builder must make it. Both values are updated in
/// place if a collection moved them.
///
/// # Errors
/// Reports the reservation's out-of-memory, or a failure to build the record
/// layout.
pub(crate) fn build_match_result_folded(
    interp: &mut Interpreter,
    receiver: &mut Value,
    input: &mut Value,
    matched: &MatchOffsets,
) -> Result<Option<Value>, VmError> {
    if receiver
        .as_regexp()
        .is_none_or(|regexp| regexp.has_named_groups(&interp.gc_heap))
    {
        return Ok(None);
    }
    let mut roots = otter_gc::RootScope::new(&mut interp.gc_heap);
    // SAFETY: both slots belong to the caller and outlive the scope.
    unsafe {
        roots.add_value(receiver);
        roots.add_value(input);
    }
    let layout =
        interp.regexp_record_layout(crate::object_layout_cache::RegExpRecordLayout::Match)?;
    let Some(shape) = interp.shape_runtime.handle_for_id(layout.shape_id()) else {
        return Ok(None);
    };
    let groups = matched.groups();
    let substrings = 1 + matched.captures().filter(Option::is_some).count();
    let bytes = substrings * crate::string::gc_body::SUBSTRING_CELL_BYTES
        + crate::array::tagged_array_cell_bytes(groups)
        + crate::object::object_cell_bytes(crate::object::shape_body::inline_capacity_of(shape));
    if !interp
        .gc_heap
        .ensure_machine_allocation_with_roots(bytes, &mut |_| {})?
    {
        return Ok(None);
    }
    drop(roots);
    // Nothing below collects: every cell comes out of the reservation.
    let Some(shape) = interp.shape_runtime.handle_for_id(layout.shape_id()) else {
        return Ok(None);
    };
    let heap = &mut interp.gc_heap;
    let text = input.as_string(heap).ok_or(VmError::TypeMismatch)?.handle();
    let mut elements = smallvec::SmallVec::<[Value; 16]>::with_capacity(groups);
    crate::string::gc_body::try_substrings_no_collect(
        heap,
        text,
        (0..groups).map(|index| matched.group(index)),
        &mut elements,
    )
    .ok_or(VmError::TypeMismatch)?;
    let array = crate::array::try_tagged_from_values_no_collect(heap, &elements)
        .ok_or(VmError::TypeMismatch)?;
    let record = [
        Value::number_f64(matched.range().start as f64),
        *input,
        Value::undefined(),
    ];
    let bag = crate::object::try_alloc_object_with_values_no_collect(heap, shape, &record)
        .ok_or(VmError::TypeMismatch)?;
    heap.with_payload(array, |body| body.expando = bag);
    heap.record_write(array, &bag);
    Ok(Some(Value::array(array)))
}
