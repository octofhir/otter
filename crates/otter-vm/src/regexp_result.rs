//! Folded construction of the array a RegExp match returns.
//!
//! # Contents
//! - [`carve_match_result`] — the match array, its substrings and its
//!   `index` / `input` / `groups` record, carved from an allocation buffer
//!   that already holds them: the leaf `exec` entry's builder.
//! - [`build_match_result_folded`] — the same cells from one reservation
//!   the subject and receiver are rooted across.
//!
//! # Invariants
//! - One reservation admits every cell of the result. No allocation after it
//!   can collect, so the raw handles the construction holds stay valid. When
//!   the current buffer cannot hold them, the subject and the receiver are
//!   rooted across the reservation, which may collect even when it declines:
//!   the caller's copies are updated in place.
//! - The record's hidden class is resolved once per isolate and kept: shapes
//!   never move and the layout cache roots this one. Its field summaries are
//!   generalized, so the record is written without observing its values.
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
//! - [`crate::regexp_fast::exec_direct`] and [`crate::regexp_fast::exec_leaf`]
//!   produce the match this builds.
//! - [`otter_gc::GcHeap::ensure_machine_allocation_with_roots`] makes the
//!   reservation generated allocation groups also use.

use crate::regexp::engine::{MatchGroups, MatchOffsets};
use crate::rooting::RootScopeExt;
use crate::{Interpreter, Value, VmError};

/// Groups a match array is planned for on the stack; a match with more
/// plans each group twice instead.
const PLANNED_GROUPS: usize = 16;

/// The array `exec` returns for `matched` in the string `input`, read
/// through `source` (its contiguous units), its record of the kept `shape`
/// ([`Interpreter::regexp_match_shape`]), carved from the allocation buffer
/// in one group when it already holds every cell: the slab first, each
/// group's substring written straight into its slot, then the record and
/// the array over them. Nothing here collects or moves a value. The caller
/// rules out named groups. `None` when the buffer has no room.
pub(crate) fn carve_match_result(
    heap: &mut otter_gc::GcHeap,
    source: &crate::string::gc_body::SubstringSource,
    shape: crate::object::ShapeHandle,
    input: Value,
    matched: impl MatchGroups,
) -> Option<Value> {
    use crate::string::gc_body::SubstringCell;
    let groups = matched.groups();
    let mut bytes = crate::array::tagged_array_cell_bytes(groups)
        + crate::object::object_cell_bytes(crate::object::shape_body::inline_capacity_of(shape));
    // Each group's cell, planned once; `None` for a group that did not
    // participate.
    let mut planned: [Option<SubstringCell>; PLANNED_GROUPS] = [None; PLANNED_GROUPS];
    let plan = |heap: &otter_gc::GcHeap, index: usize| {
        matched.group(index).map(|range| source.plan(heap, range))
    };
    for index in 0..groups {
        if let Some((cell, cell_bytes)) = plan(heap, index) {
            bytes += cell_bytes;
            if let Some(slot) = planned.get_mut(index) {
                *slot = Some(cell);
            }
        }
    }
    let mut carve = heap.carve(bytes)?;
    let slab = crate::array::place_tagged_slab(heap, &mut carve, groups);
    for index in 0..groups {
        let cell = match planned.get(index) {
            Some(cell) => *cell,
            None => plan(heap, index).map(|(cell, _)| cell),
        };
        let value = match cell {
            Some(cell) => Value::string_gc(source.place(heap, &mut carve, cell)),
            None => Value::undefined(),
        };
        // SAFETY: `index < groups`, the slab's live length.
        unsafe { crate::array::write_placed_tagged_slot(slab, index, value) };
    }
    let record = [
        Value::number_f64(matched.range().start as f64),
        input,
        Value::undefined(),
    ];
    let bag = crate::object::place_generalized_object(heap, &mut carve, shape, &record);
    let array = crate::array::place_tagged_array(heap, &mut carve, slab, groups, bag);
    debug_assert_eq!(carve.remaining(), 0, "the group placed what it carved");
    Some(Value::array(array))
}

/// Allocation-buffer bytes [`carve_match_result`] takes for `matched`.
fn match_result_bytes(
    heap: &otter_gc::GcHeap,
    source: &crate::string::gc_body::SubstringSource,
    shape: crate::object::ShapeHandle,
    matched: impl MatchGroups,
) -> usize {
    let groups = matched.groups();
    let mut bytes = crate::array::tagged_array_cell_bytes(groups)
        + crate::object::object_cell_bytes(crate::object::shape_body::inline_capacity_of(shape));
    for index in 0..groups {
        if let Some(range) = matched.group(index) {
            bytes += source.plan(heap, range).1;
        }
    }
    bytes
}

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
    let Some(shape) = interp.regexp_match_shape()? else {
        return Ok(None);
    };
    let text = input
        .as_string(&interp.gc_heap)
        .ok_or(VmError::TypeMismatch)?
        .handle();
    let Some(source) = crate::string::gc_body::SubstringSource::of(&interp.gc_heap, text) else {
        return Ok(None);
    };
    let bytes = match_result_bytes(&interp.gc_heap, &source, shape, matched);
    // A buffer that already holds every cell needs no roots; otherwise the
    // reservation may collect, which moves the subject and the receiver,
    // and the source is resolved again over the moved subject.
    if interp.gc_heap.machine_allocation_ready(bytes) {
        return Ok(carve_match_result(
            &mut interp.gc_heap,
            &source,
            shape,
            *input,
            matched,
        ));
    }
    let mut roots = otter_gc::RootScope::new(&mut interp.gc_heap);
    // SAFETY: both slots belong to the caller and outlive the scope.
    unsafe {
        roots.add_value(receiver);
        roots.add_value(input);
    }
    if !interp
        .gc_heap
        .ensure_machine_allocation_with_roots(bytes, &mut |_| {})?
    {
        return Ok(None);
    }
    let text = input
        .as_string(&interp.gc_heap)
        .ok_or(VmError::TypeMismatch)?
        .handle();
    let Some(source) = crate::string::gc_body::SubstringSource::of(&interp.gc_heap, text) else {
        return Ok(None);
    };
    Ok(carve_match_result(
        &mut interp.gc_heap,
        &source,
        shape,
        *input,
        matched,
    ))
}

impl Interpreter {
    /// The hidden class of match records, resolved once from the fixed
    /// layout, whose field summaries are generalized; `None` when the
    /// layout names no live class.
    pub(crate) fn regexp_match_shape(
        &mut self,
    ) -> Result<Option<crate::object::ShapeHandle>, VmError> {
        if !self.regexp_match_shape.is_null() {
            return Ok(Some(self.regexp_match_shape));
        }
        let kind = crate::object_layout_cache::RegExpRecordLayout::Match;
        let layout = match self.object_layout_cache.regexp(kind) {
            Some(layout) => layout,
            None => self.regexp_record_layout(kind)?,
        };
        let shape = self
            .shape_runtime
            .handle_for_id(&self.gc_heap, layout.shape_id());
        if let Some(shape) = shape {
            self.regexp_match_shape = shape;
        }
        Ok(shape)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object;
    use crate::object_layout_cache::RegExpRecordLayout;

    fn folded(vm: &mut Interpreter, receiver: &mut Value, input: &mut Value) -> Value {
        let Some(Ok(crate::regexp_fast::DirectExec::Matched(offsets))) =
            crate::regexp_fast::exec_direct(vm, *receiver, *input, false)
        else {
            panic!("plain matching RegExp");
        };
        let result = build_match_result_folded(vm, receiver, input, &offsets);
        vm.recycle_match_offsets(offsets);
        result
            .expect("record representation preparation")
            .expect("prepared record retains folded allocation")
    }

    #[test]
    fn regexp_result_keeps_one_layout_across_numeric_and_nonnumber_history() {
        let mut vm = Interpreter::new().expect("fixture interpreter");
        vm.gc_heap.set_gc_stress(0, false);
        let _runtime_roots = vm.scope_runtime_roots_guard();
        vm.with_handle_scope(|vm, scope| {
            let regexp = crate::regexp::JsRegExp::compile_cached(
                &mut vm.gc_heap,
                &mut vm.regex_compile_cache,
                &[u16::from(b'a')],
                "",
            )
            .expect("plain pattern");
            let receiver_root = vm.scoped_value(scope, Value::regexp(regexp));
            let text = crate::string::JsString::from_str("a", &mut vm.gc_heap).expect("subject");
            let input_root = vm.scoped_value(scope, Value::string(text));
            let mut receiver = vm.escape_scoped(receiver_root);
            let mut input = vm.escape_scoped(input_root);
            // No scoped result builder has observed the fixed layout yet:
            // its first String input must prepare the ordinary cold birth.
            let first = folded(vm, &mut receiver, &mut input);
            let first_root = vm.scoped_value(scope, first);
            let logical = vm.regexp_record_layout(RegExpRecordLayout::Match).unwrap();
            let first_bag =
                crate::array::expando_object(first.as_array().unwrap(), &vm.gc_heap).unwrap();
            assert_eq!(
                object::get_own(first_bag, &vm.gc_heap, "input"),
                Some(input)
            );
            assert_eq!(
                object::get_own(first_bag, &vm.gc_heap, "groups"),
                Some(Value::UNDEFINED)
            );
            assert!(
                vm.ordinary_set_data_property(first_bag, "index", Value::number_f64(1.5))
                    .expect("user assignment promotes the actual record field")
            );
            receiver = vm.escape_scoped(receiver_root);
            input = vm.escape_scoped(input_root);
            let second = folded(vm, &mut receiver, &mut input);
            assert_eq!(
                vm.regexp_record_layout(RegExpRecordLayout::Match).unwrap(),
                logical
            );
            let second_bag =
                crate::array::expando_object(second.as_array().unwrap(), &vm.gc_heap).unwrap();
            assert_eq!(
                vm.object_layout_cache
                    .regexp(RegExpRecordLayout::Match)
                    .unwrap()
                    .shape_id(),
                object::shape_id(second_bag, &vm.gc_heap),
                "the existing private slot retains the one geometric layout"
            );
            assert_eq!(
                object::get_own(second_bag, &vm.gc_heap, "index"),
                Some(Value::number_i32(0)),
                "the next integral index is independent of prior field values"
            );
            assert_eq!(
                object::get_own(second_bag, &vm.gc_heap, "input"),
                Some(input)
            );
            let first = vm.escape_scoped(first_root).as_array().unwrap();
            let first_bag = crate::array::expando_object(first, &vm.gc_heap).unwrap();
            assert_eq!(
                object::get_own(first_bag, &vm.gc_heap, "index"),
                Some(Value::number_f64(1.5)),
                "a new result does not rewrite the previous match"
            );
            assert!(
                vm.ordinary_set_data_property(first_bag, "index", Value::null())
                    .expect("non-number assignment keeps the same geometry")
            );
            receiver = vm.escape_scoped(receiver_root);
            input = vm.escape_scoped(input_root);
            let third = folded(vm, &mut receiver, &mut input);
            let third_bag =
                crate::array::expando_object(third.as_array().unwrap(), &vm.gc_heap).unwrap();
            assert_eq!(
                vm.regexp_record_layout(RegExpRecordLayout::Match).unwrap(),
                logical,
                "layout tokens are independent of stored value history"
            );
            assert_eq!(
                vm.object_layout_cache
                    .regexp(RegExpRecordLayout::Match)
                    .unwrap()
                    .shape_id(),
                object::shape_id(third_bag, &vm.gc_heap)
            );
        });
    }
}
