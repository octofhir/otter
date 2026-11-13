//! Scoped preparation and live mutation for Array fill and reverse.
//!
//! # Contents
//! - Rooted ToObject, LengthOfArrayLike and fill-bound coercions.
//! - Existing dense fill admission and generic Set operations.
//! - Ordered reverse HasProperty/Get/Set/Delete with per-pair scoped values.
//!
//! # Invariants
//! - Receivers and pending fill operands are handles before any collecting or
//!   reentrant operation; every later use resolves the current slot.
//! - Empty ranges return the current boxed receiver after coercion.
//! - Fill captures length once and coerces start before end, including empty
//!   ranges. The pending end is never cached across start coercion.
//! - Reverse retains both fetched values across subsequent getters and writes.
//! - Reverse walks every pair in the captured length, with a native work and
//!   cancellation checkpoint before each pair, including two absent indices.
//! - Fill walks the full captured range. Enforcing budgets checkpoint every
//!   Set attempt; observing bulk paths charge the actual span before effects.
//! - Dense admission, rooted reservation and write barriers remain required.
//!
//! # See also
//! - `super::Interpreter::array_prototype_chain_has_indexed_property` owns fill's chain proof.
//! - `crate::array::fill_dense_range_with_roots` owns dense reservation/writes.

use crate::native_abi::CommittedValueError;
use crate::{ActivationStack, ExecutionContext, Interpreter, Value, ValueWindow};

use super::{format_index_key, length_of_array_like};

impl Interpreter {
    /// §23.1.3.27 live `Array.prototype.reverse`.
    pub(crate) fn array_reverse(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        receiver: Value,
        roots: &mut [&mut [Value]],
    ) -> Result<Value, CommittedValueError> {
        self.with_handle_scope(|interp, scope| {
            let o_root = interp.scoped_value(scope, receiver);
            let receiver = interp.escape_scoped(o_root);
            if !receiver.is_object_type() {
                let boxed = interp
                    .box_sloppy_this_primitive_runtime_rooted(receiver, roots)
                    .map_err(CommittedValueError::JavaScript)?;
                interp.set_scoped(o_root, boxed);
            }
            let o = interp.escape_scoped(o_root);
            let len = length_of_array_like(interp, stack, context, &o)?;
            if len < 2 {
                return Ok(interp.escape_scoped(o_root));
            }
            for lower in 0..(len / 2) {
                interp
                    .charge_native_loop_iteration()
                    .map_err(CommittedValueError::Fatal)?;
                interp.with_handle_scope(|interp, step| {
                    let upper = len - lower - 1;
                    let lower_key = lower.to_string();
                    let upper_key = format_index_key(upper as f64);
                    let o = interp.escape_scoped(o_root);
                    let lower_exists =
                        interp.array_method_has_property(stack, context, o, &lower_key)?;
                    let lower_value = if lower_exists {
                        let o = interp.escape_scoped(o_root);
                        let value =
                            interp.array_method_get_property(stack, context, o, &lower_key)?;
                        Some(interp.scoped_value(step, value))
                    } else {
                        None
                    };
                    let o = interp.escape_scoped(o_root);
                    let upper_exists =
                        interp.array_method_has_property(stack, context, o, &upper_key)?;
                    let upper_value = if upper_exists {
                        let o = interp.escape_scoped(o_root);
                        let value =
                            interp.array_method_get_property(stack, context, o, &upper_key)?;
                        Some(interp.scoped_value(step, value))
                    } else {
                        None
                    };
                    match (lower_value, upper_value) {
                        (Some(l), Some(u)) => {
                            let (o, u_value) =
                                (interp.escape_scoped(o_root), interp.escape_scoped(u));
                            interp.array_set_property_throwing(
                                stack, context, o, &lower_key, u_value,
                            )?;
                            let (o, l_value) =
                                (interp.escape_scoped(o_root), interp.escape_scoped(l));
                            interp.array_set_property_throwing(
                                stack, context, o, &upper_key, l_value,
                            )?;
                        }
                        (Some(l), None) => {
                            let o = interp.escape_scoped(o_root);
                            interp.array_delete_property_throwing(stack, context, o, &lower_key)?;
                            let (o, l_value) =
                                (interp.escape_scoped(o_root), interp.escape_scoped(l));
                            interp.array_set_property_throwing(
                                stack, context, o, &upper_key, l_value,
                            )?;
                        }
                        (None, Some(u)) => {
                            let (o, u_value) =
                                (interp.escape_scoped(o_root), interp.escape_scoped(u));
                            interp.array_set_property_throwing(
                                stack, context, o, &lower_key, u_value,
                            )?;
                            let o = interp.escape_scoped(o_root);
                            interp.array_delete_property_throwing(stack, context, o, &upper_key)?;
                        }
                        (None, None) => {}
                    }
                    Ok::<(), CommittedValueError>(())
                })?;
            }
            Ok(interp.escape_scoped(o_root))
        })
    }

    /// §23.1.3.7 live `Array.prototype.fill`.
    pub(crate) fn array_fill(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        receiver: Value,
        value: Value,
        args: ValueWindow<'_>,
        roots: &mut [&mut [Value]],
    ) -> Result<Value, CommittedValueError> {
        self.with_handle_scope(|interp, scope| {
            // The start coercion may move the end operand before it is read.
            // Capture all pending inputs before boxing or the length getter.
            let o_root = interp.scoped_value(scope, receiver);
            let value_root = interp.scoped_value(scope, value);
            let start_root = args.get(1).map(|value| interp.scoped_value(scope, *value));
            let end_root = args.get(2).map(|value| interp.scoped_value(scope, *value));
            let receiver = interp.escape_scoped(o_root);
            if !receiver.is_object_type() {
                let boxed = interp
                    .box_sloppy_this_primitive_runtime_rooted(receiver, roots)
                    .map_err(CommittedValueError::JavaScript)?;
                interp.set_scoped(o_root, boxed);
            }
            let o = interp.escape_scoped(o_root);
            let len = length_of_array_like(interp, stack, context, &o)?;
            let start_value = start_root.map(|root| interp.escape_scoped(root));
            let start =
                interp.array_relative_index(stack, context, start_value.as_ref(), 0.0, len)?;
            let end_value = end_root.map(|root| interp.escape_scoped(root));
            let end =
                interp.array_relative_index(stack, context, end_value.as_ref(), len as f64, len)?;
            if start >= end {
                return Ok(interp.escape_scoped(o_root));
            }
            let o = interp.escape_scoped(o_root);
            let value = interp.escape_scoped(value_root);
            if interp
                .try_fill_numeric_typed_array(o, value, start, end)
                .map_err(CommittedValueError::Fatal)?
            {
                return Ok(interp.escape_scoped(o_root));
            }
            if !interp.work_budget().enforces_on_exceedance()
                && let Some(arr) = o.as_array()
                && crate::array::can_fast_fill_dense_range(arr, interp.gc_heap(), start, end)
                && !interp
                    .array_prototype_chain_has_indexed_property(start, end)
                    .map_err(CommittedValueError::JavaScript)?
            {
                interp
                    .charge_native_loop_work((end - start) as u64)
                    .map_err(CommittedValueError::Fatal)?;
                let arr = interp
                    .escape_scoped(o_root)
                    .as_array()
                    .expect("rooted fill receiver");
                let value = interp.escape_scoped(value_root);
                let mut external_visit = |visit: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
                    for root in roots.iter_mut() {
                        for value in root.iter_mut() {
                            value.trace_value_slot_mut(visit);
                        }
                    }
                };
                crate::array::fill_dense_range_with_roots(
                    arr,
                    interp.gc_heap_mut(),
                    start,
                    end,
                    value,
                    &mut external_visit,
                )
                .map_err(|_| interp.err_range(("Invalid array length".to_string()).into()))
                .map_err(CommittedValueError::JavaScript)?;
                return Ok(interp.escape_scoped(o_root));
            }
            for k in start..end {
                interp
                    .charge_native_loop_iteration()
                    .map_err(CommittedValueError::Fatal)?;
                let (o, value) = (
                    interp.escape_scoped(o_root),
                    interp.escape_scoped(value_root),
                );
                interp.array_set_property_throwing(
                    stack,
                    context,
                    o,
                    &format_index_key(k as f64),
                    value,
                )?;
            }
            Ok(interp.escape_scoped(o_root))
        })
    }
}
