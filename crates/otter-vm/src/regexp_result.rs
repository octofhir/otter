//! Folded construction of the array a RegExp match returns.
//!
//! # Contents
//! - [`build_match_result_folded`] — the match array, its substrings and its
//!   `index` / `input` / `groups` record, taken from one allocation-buffer
//!   reservation.
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
    let Some(shape) = interp.regexp_match_shape()? else {
        return Ok(None);
    };
    let groups = matched.groups();
    let substrings = 1 + matched.captures().filter(Option::is_some).count();
    let bytes = substrings * crate::string::gc_body::SUBSTRING_CELL_BYTES
        + crate::array::tagged_array_cell_bytes(groups)
        + crate::object::object_cell_bytes(crate::object::shape_body::inline_capacity_of(shape));
    // A buffer that already holds every cell needs no roots; otherwise the
    // reservation may collect, which moves the subject and the receiver.
    if !interp.gc_heap.machine_allocation_ready(bytes) {
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
    }
    // Nothing below collects: every cell comes out of the reservation.
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
    let bag = crate::object::try_alloc_generalized_object_no_collect(heap, shape, &record)
        .ok_or(VmError::TypeMismatch)?;
    heap.with_payload(array, |body| body.expando = bag);
    heap.record_write(array, &bag);
    Ok(Some(Value::array(array)))
}

impl Interpreter {
    /// The hidden class of match records, resolved once from the fixed
    /// layout, whose field summaries are generalized; `None` when the
    /// layout names no live class.
    fn regexp_match_shape(&mut self) -> Result<Option<crate::object::ShapeHandle>, VmError> {
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
