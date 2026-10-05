//! §23.1.2.1 `Array.fromAsync` — collect an async iterable, a sync
//! iterable, or an array-like whose elements are promises.
//!
//! # Contents
//! - [`Interpreter::array_from_async`] answers with the promise the call
//!   returns and drives the collection behind it.
//!
//! # Invariants
//! - The walk is a promise chain, not a loop: each step resolves, appends,
//!   and schedules the next one, so an iterable of any length costs one
//!   microtask per element and no native stack.
//! - Catchable step failures settle the caller's promise rather than only a
//!   derived reaction promise. Fatal failures and actual allocation refusal
//!   propagate through the canonical typed completion boundary.
//! - The walk's state lives in one ordinary object held as a traced
//!   capture, so a collection between steps relocates it whole.
//! - One handle scope owns pending target arrays and all state operands until
//!   ordered state publication; array length growth reloads that same owner.
//! - IteratorClose suppresses catchable close errors behind the original
//!   rejection and its incoming provenance; suppressed cleanup detail/frames
//!   never escape into later reactions. Fatal failures keep their actual cause.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-array.fromasync>
//! - [`crate::async_from_sync_iterator`] — the adapter that awaits the
//!   values of a synchronous iterable.

use crate::native_abi::CommittedValueError;
use smallvec::SmallVec;

use crate::activation_stack::ActivationStack;
use crate::execution_context::ExecutionContext;
use crate::runtime_cx::NativeCtx;
use crate::{Interpreter, NativeError, Value, VmError, VmGetOutcome, VmPropertyKey, symbol};

/// Field names on the walk's state object. They are ordinary own
/// properties of an object that never leaves this module.
mod slot {
    pub const ITERATOR: &str = "iterator";
    pub const NEXT: &str = "next";
    pub const ARRAY: &str = "array";
    pub const ARRAY_LIKE: &str = "arrayLike";
    pub const LENGTH: &str = "length";
    pub const INDEX: &str = "index";
    pub const MAPFN: &str = "mapfn";
    pub const THIS_ARG: &str = "thisArg";
    pub const RESOLVE: &str = "resolve";
    pub const REJECT: &str = "reject";
}

impl Interpreter {
    /// `Array.fromAsync(asyncItems, mapfn, thisArg)` with `this` as the
    /// constructor `C`.
    ///
    /// # Errors
    /// Returns allocation refusal or a fatal completion. Catchable collection
    /// failures reject the returned promise with the original thrown value.
    pub(crate) fn array_from_async(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        this_value: Value,
        args: &[Value],
    ) -> Result<Value, CommittedValueError> {
        let items = args.first().copied().unwrap_or_else(Value::undefined);
        let mapfn = args.get(1).copied().unwrap_or_else(Value::undefined);
        let this_arg = args.get(2).copied().unwrap_or_else(Value::undefined);

        // Everything below allocates repeatedly (the capability, property
        // stores that grow shapes, fresh handlers), so every value that must
        // survive rides the traced anchor stack from the start and is re-read
        // after each step — raw locals go stale under a moving collection.
        let items_slot = self.push_iteration_anchor(items) - 1;
        let mapfn_slot = self.push_iteration_anchor(mapfn) - 1;
        let this_arg_slot = self.push_iteration_anchor(this_arg) - 1;
        let this_value_slot = self.push_iteration_anchor(this_value) - 1;
        let capability =
            match crate::promise_dispatch::PromiseBuilder::with_context(Some(context.clone()))
                .capability_stack_rooted(self, stack, &[], &[])
            {
                Ok(capability) => capability,
                Err(error) => {
                    self.pop_iteration_anchors_to(items_slot);
                    return Err(CommittedValueError::JavaScript(error.into()));
                }
            };
        let base = items_slot;
        let promise_slot = self.push_iteration_anchor(capability.promise) - 1;
        let resolve_slot = self.push_iteration_anchor(capability.resolve) - 1;
        let reject_slot = self.push_iteration_anchor(capability.reject) - 1;
        let result = (|| -> Result<Value, CommittedValueError> {
            let state_obj = self
                .alloc_runtime_rooted_object_with_roots(&[], &[])
                .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
            let st = self.push_iteration_anchor(Value::object(state_obj)) - 1;
            let resolve = self.iteration_anchor(resolve_slot);
            self.state_set(self.iteration_anchor(st), slot::RESOLVE, resolve)
                .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
            let reject = self.iteration_anchor(reject_slot);
            self.state_set(self.iteration_anchor(st), slot::REJECT, reject)
                .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
            let mapfn = self.iteration_anchor(mapfn_slot);
            self.state_set(self.iteration_anchor(st), slot::MAPFN, mapfn)
                .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
            let this_arg = self.iteration_anchor(this_arg_slot);
            self.state_set(self.iteration_anchor(st), slot::THIS_ARG, this_arg)
                .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
            self.state_set(
                self.iteration_anchor(st),
                slot::INDEX,
                Value::number_f64(0.0),
            )
            .map_err(|error| CommittedValueError::JavaScript(error.into()))?;

            match self.collect_begin(stack, context, st, this_value_slot, items_slot, mapfn_slot) {
                Ok(()) => {}
                Err(err) => self.collect_settle_error(stack, context, st, err)?,
            }
            Ok(self.iteration_anchor(promise_slot))
        })();
        self.pop_iteration_anchors_to(base);
        result
    }

    /// A `TypeError` instance, as the thrown value: the caller's promise
    /// rejects with the error object itself, not with a rendering of it.
    fn collect_type_error(
        &mut self,
        stack: &ActivationStack,
        message: &str,
    ) -> CommittedValueError {
        self.collect_error(stack, crate::error_classes::ErrorKind::TypeError, message)
    }

    /// A `RangeError` instance, as the thrown value.
    fn collect_range_error(
        &mut self,
        stack: &ActivationStack,
        message: &str,
    ) -> CommittedValueError {
        self.collect_error(stack, crate::error_classes::ErrorKind::RangeError, message)
    }

    fn collect_error(
        &mut self,
        stack: &ActivationStack,
        kind: crate::error_classes::ErrorKind,
        message: &str,
    ) -> CommittedValueError {
        match self.make_error_instance_with_stack_roots(
            stack,
            kind,
            Some(message.to_string()),
            &Value::undefined(),
        ) {
            Ok(object) => {
                self.set_pending_uncaught_throw(Value::object(object));
                CommittedValueError::JavaScript(self.err_uncaught(message.to_string().into()))
            }
            Err(err) => CommittedValueError::Fatal(err),
        }
    }

    /// Steps 3.a–3.j: validate `mapfn`, pick the iteration protocol, build
    /// the target array, and start the walk.
    fn collect_begin(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        st: usize,
        this_value_slot: usize,
        items_slot: usize,
        mapfn_slot: usize,
    ) -> Result<(), CommittedValueError> {
        let mapfn = self.iteration_anchor(mapfn_slot);
        if !mapfn.is_undefined() && !self.is_callable_runtime(&mapfn) {
            return Err(self.collect_type_error(stack, "mapper is not a function"));
        }

        let async_iterator_sym = self
            .well_known_symbols
            .get(symbol::WellKnown::AsyncIterator);
        let items = self.iteration_anchor(items_slot);
        let using_async = self.collect_method(stack, context, items, async_iterator_sym)?;
        let iterator = if let Some(method) = using_async {
            let items = self.iteration_anchor(items_slot);
            Some(
                self.run_callable_sync_rooted(
                    stack,
                    Some(context),
                    &method,
                    items,
                    SmallVec::new(),
                )
                .map_err(CommittedValueError::completed_call)?,
            )
        } else {
            let iterator_sym = self.well_known_symbols.get(symbol::WellKnown::Iterator);
            let items = self.iteration_anchor(items_slot);
            match self.collect_method(stack, context, items, iterator_sym)? {
                Some(method) => {
                    let items = self.iteration_anchor(items_slot);
                    let sync = self
                        .run_callable_sync_rooted(
                            stack,
                            Some(context),
                            &method,
                            items,
                            SmallVec::new(),
                        )
                        .map_err(CommittedValueError::completed_call)?;
                    if !sync.is_object_type() && !sync.is_proxy() {
                        return Err(self.collect_type_error(
                            stack,
                            "iterator method did not answer with an object",
                        ));
                    }
                    Some(self.create_async_from_sync_iterator(stack, context, sync)?)
                }
                None => None,
            }
        };

        let this_value = self.iteration_anchor(this_value_slot);
        let constructing =
            crate::abstract_ops::is_constructor(&this_value, context, self.gc_heap());
        match iterator {
            Some(iterator) => {
                if !iterator.is_object_type() && !iterator.is_proxy() {
                    return Err(self.collect_type_error(
                        stack,
                        "iterator method did not answer with an object",
                    ));
                }
                // The iterator and `next` survive the target-array
                // construction (which can run a user constructor).
                let iterator_slot = self.push_iteration_anchor(iterator) - 1;
                let result = (|| -> Result<(), CommittedValueError> {
                    let next = self.collect_get(stack, context, iterator, "next")?;
                    let next_slot = self.push_iteration_anchor(next) - 1;
                    let array = if constructing {
                        let this_value = self.iteration_anchor(this_value_slot);
                        self.run_construct_sync_rooted(
                            stack,
                            context,
                            &this_value,
                            this_value,
                            SmallVec::new(),
                            0,
                        )
                        .map_err(CommittedValueError::completed_call)?
                    } else {
                        self.collect_new_array(0)
                            .map_err(|error| CommittedValueError::JavaScript(error.into()))?
                    };
                    let iterator = self.iteration_anchor(iterator_slot);
                    let next = self.iteration_anchor(next_slot);
                    self.collect_publish_array_state(
                        st,
                        array,
                        &[(slot::ITERATOR, iterator), (slot::NEXT, next)],
                    )
                    .map_err(|error| CommittedValueError::JavaScript(error.into()))
                })();
                self.pop_iteration_anchors_to(iterator_slot);
                result?;
                self.collect_pump(stack, context, st)
            }
            None => {
                let items = self.iteration_anchor(items_slot);
                let array_like = if items.is_object_type() || items.is_proxy() {
                    items
                } else {
                    self.box_sloppy_this_primitive_runtime_rooted(items, &[])
                        .map_err(|error| CommittedValueError::JavaScript(error.into()))?
                };
                let array_like_slot = self.push_iteration_anchor(array_like) - 1;
                let result = (|| -> Result<(), CommittedValueError> {
                    let array_like = self.iteration_anchor(array_like_slot);
                    let len = crate::array_prototype::length_of_array_like(
                        self,
                        stack,
                        context,
                        &array_like,
                    )? as f64;
                    let array = if constructing {
                        let mut args: SmallVec<[Value; 8]> = SmallVec::new();
                        args.push(Value::number_f64(len));
                        let this_value = self.iteration_anchor(this_value_slot);
                        self.run_construct_sync_rooted(
                            stack,
                            context,
                            &this_value,
                            this_value,
                            args,
                            0,
                        )
                        .map_err(CommittedValueError::completed_call)?
                    } else {
                        // §10.4.2.2 ArrayCreate — a length past 2^32 - 1 is
                        // not an array length at all.
                        if len > 4_294_967_295.0 {
                            return Err(self.collect_range_error(stack, "Invalid array length"));
                        }
                        self.collect_new_array(len as usize)
                            .map_err(|error| CommittedValueError::JavaScript(error.into()))?
                    };
                    let array_like = self.iteration_anchor(array_like_slot);
                    self.collect_publish_array_state(
                        st,
                        array,
                        &[
                            (slot::ARRAY_LIKE, array_like),
                            (slot::LENGTH, Value::number_f64(len)),
                        ],
                    )
                    .map_err(|error| CommittedValueError::JavaScript(error.into()))
                })();
                self.pop_iteration_anchors_to(array_like_slot);
                result?;
                self.collect_pump(stack, context, st)
            }
        }
    }

    /// One step: read the next element (or the next array-like slot) and
    /// arrange for the walk to continue once it resolves.
    fn collect_pump(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        st: usize,
    ) -> Result<(), CommittedValueError> {
        let state = self.iteration_anchor(st);
        let index = self.state_number(state, slot::INDEX);
        let iterator = self.state_get(state, slot::ITERATOR);
        if iterator.is_undefined() {
            // Array-like: every slot is read, then awaited.
            let len = self.state_number(state, slot::LENGTH);
            if index >= len {
                let array = self.state_get(state, slot::ARRAY);
                self.collect_set_length(stack, context, array, len)?;
                // The length store can run user code (an accessor) —
                // re-read the array from the anchored state.
                let array = self.state_get(self.iteration_anchor(st), slot::ARRAY);
                return self.collect_settle(stack, context, st, slot::RESOLVE, array);
            }
            let array_like = self.state_get(state, slot::ARRAY_LIKE);
            let key = crate::number::NumberValue::from_f64(index).to_display_string();
            let value = self.collect_get(stack, context, array_like, &key)?;
            return self.collect_await(stack, context, st, value, Step::ArrayLikeValue);
        }

        let next = self.state_get(state, slot::NEXT);
        let result = self
            .run_callable_sync_rooted(stack, Some(context), &next, iterator, SmallVec::new())
            .map_err(CommittedValueError::completed_call)?;
        if !result.is_object_type() && !result.is_proxy() {
            return Err(self.collect_type_error(stack, "iterator result is not an object"));
        }
        self.collect_await(stack, context, st, result, Step::IteratorResult)
    }

    /// Resolve `value` and continue at `step` once it settles.
    fn collect_await(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        st: usize,
        value: Value,
        step: Step,
    ) -> Result<(), CommittedValueError> {
        self.collect_await_with(stack, context, st, value, step, Step::Reject)
    }

    /// Resolve `value`, continuing at `step` when it settles and at
    /// `failure` when it rejects.
    fn collect_await_with(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        st: usize,
        value: Value,
        step: Step,
        failure: Step,
    ) -> Result<(), CommittedValueError> {
        let inner_value = self.promise_resolve_value(stack, Some(context), value)?;
        // Handler and capability allocations move the heap — anchor the
        // resolved promise and both handlers, re-reading each before use.
        let inner_slot = self.push_iteration_anchor(inner_value) - 1;
        let result = (|| -> Result<(), CommittedValueError> {
            let state = self.iteration_anchor(st);
            let on_fulfilled = self.collect_handler(state, step)?;
            let fulfilled_slot = self.push_iteration_anchor(on_fulfilled) - 1;
            let state = self.iteration_anchor(st);
            let on_rejected = self.collect_handler(state, failure)?;
            let rejected_slot = self.push_iteration_anchor(on_rejected) - 1;
            let capability =
                crate::promise_dispatch::PromiseBuilder::with_context(Some(context.clone()))
                    .capability_stack_rooted(self, stack, &[], &[])
                    .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
            let inner_value = self.iteration_anchor(inner_slot);
            let inner = inner_value
                .as_promise()
                .ok_or(VmError::InvalidOperand)
                .map_err(|error| CommittedValueError::Fatal(error.into()))?;
            let on_fulfilled = self.iteration_anchor(fulfilled_slot);
            let on_rejected = self.iteration_anchor(rejected_slot);
            let outcome = self
                .register_promise_reactions(
                    inner,
                    Some(on_fulfilled),
                    Some(on_rejected),
                    capability,
                    Some(context.clone()),
                )
                .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
            if let Some(job) = outcome.immediate_job {
                self.microtasks.enqueue(job);
            }
            Ok(())
        })();
        self.pop_iteration_anchors_to(inner_slot);
        result
    }

    /// One reaction handler, carrying the walk's state.
    fn collect_handler(&mut self, state: Value, step: Step) -> Result<Value, CommittedValueError> {
        let value = crate::native_function::native_value_with_captures_unchecked_with_roots(
            &mut self.gc_heap,
            "Array.fromAsync",
            SmallVec::from_slice(&[state]),
            &mut |_visitor| {},
            move |ctx, args, captures| {
                let state = captures.first().copied().unwrap_or_else(Value::undefined);
                let value = args.first().copied().unwrap_or_else(Value::undefined);
                let context = ctx.execution_context().cloned();
                ctx.with_turn_parts(|interp, stack| {
                    let Some(context) = context else {
                        return Err(crate::native_function::vm_to_native_error(
                            interp,
                            VmError::InvalidOperand,
                            "Array.fromAsync",
                        ));
                    };
                    // Anchor the state for the whole synchronous drive:
                    // every step below allocates, and the capture-slab
                    // copy in `state` would go stale.
                    let st = interp.push_iteration_anchor(state) - 1;
                    let outcome = match interp.collect_step(stack, &context, st, value, step) {
                        Ok(()) => Ok(()),
                        Err(err) => interp.collect_settle_error(stack, &context, st, err),
                    };
                    interp.pop_iteration_anchors_to(st);
                    outcome.map_err(|err| err.into_native(interp, "Array.fromAsync"))
                })?;
                Ok(Value::undefined())
            },
        )
        .map_err(VmError::from)
        .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
        Ok(self.stamp_native_creation_realm(value))
    }

    /// Continue the walk after one awaited value.
    fn collect_step(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        st: usize,
        value: Value,
        step: Step,
    ) -> Result<(), CommittedValueError> {
        match step {
            Step::Reject => self.collect_settle(stack, context, st, slot::REJECT, value),
            Step::RejectAndClose => {
                let reason_slot = self.push_iteration_anchor(value) - 1;
                let settled = (|| {
                    self.collect_close_iterator(stack, context, st, value)?;
                    self.pending_uncaught_throw = None;
                    let reason = self.iteration_anchor(reason_slot);
                    self.collect_settle(stack, context, st, slot::REJECT, reason)
                })();
                self.pop_iteration_anchors_to(reason_slot);
                settled
            }
            Step::IteratorResult => {
                let result_slot = self.push_iteration_anchor(value) - 1;
                let outcome = (|| -> Result<(), CommittedValueError> {
                    let value = self.iteration_anchor(result_slot);
                    let done = self.collect_get(stack, context, value, "done")?;
                    if done.to_boolean(&self.gc_heap) {
                        let state = self.iteration_anchor(st);
                        let array = self.state_get(state, slot::ARRAY);
                        let index = self.state_number(state, slot::INDEX);
                        self.collect_set_length(stack, context, array, index)?;
                        let array = self.state_get(self.iteration_anchor(st), slot::ARRAY);
                        return self.collect_settle(stack, context, st, slot::RESOLVE, array);
                    }
                    let value = self.iteration_anchor(result_slot);
                    let element = self.collect_get(stack, context, value, "value")?;
                    self.collect_map(stack, context, st, element)
                })();
                self.pop_iteration_anchors_to(result_slot);
                outcome
            }
            Step::ArrayLikeValue => self.collect_map(stack, context, st, value),
            Step::Mapped => self.collect_append(stack, context, st, value),
        }
    }

    /// Apply `mapfn` when there is one, then append; the mapped value is
    /// awaited in turn.
    fn collect_map(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        st: usize,
        element: Value,
    ) -> Result<(), CommittedValueError> {
        let state = self.iteration_anchor(st);
        let mapfn = self.state_get(state, slot::MAPFN);
        if mapfn.is_undefined() {
            return self.collect_append(stack, context, st, element);
        }
        let index = self.state_number(state, slot::INDEX);
        let this_arg = self.state_get(state, slot::THIS_ARG);
        let mut args: SmallVec<[Value; 8]> = SmallVec::new();
        args.push(element);
        args.push(Value::number_f64(index));
        let mapped = self
            .run_callable_sync_rooted(stack, Some(context), &mapfn, this_arg, args)
            .map_err(CommittedValueError::completed_call)?;
        self.collect_await_with(
            stack,
            context,
            st,
            mapped,
            Step::Mapped,
            Step::RejectAndClose,
        )
    }

    /// Store one element and take the next step.
    fn collect_append(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        st: usize,
        value: Value,
    ) -> Result<(), CommittedValueError> {
        let state = self.iteration_anchor(st);
        let array = self.state_get(state, slot::ARRAY);
        let index = self.state_number(state, slot::INDEX);
        self.create_data_property_array_index(stack, context, array, index as usize, value)?;
        self.state_set(
            self.iteration_anchor(st),
            slot::INDEX,
            Value::number_f64(index + 1.0),
        )
        .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
        self.collect_pump(stack, context, st)
    }

    /// Settle the caller's promise through the capability's own function.
    fn collect_settle(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        st: usize,
        which: &str,
        value: Value,
    ) -> Result<(), CommittedValueError> {
        let settle = self.state_get(self.iteration_anchor(st), which);
        if !self.is_callable_runtime(&settle) {
            return Ok(());
        }
        let mut args: SmallVec<[Value; 8]> = SmallVec::new();
        args.push(value);
        self.run_callable_sync_rooted(stack, Some(context), &settle, Value::undefined(), args)
            .map_err(CommittedValueError::completed_call)?;
        Ok(())
    }

    /// Reject the caller's promise with whatever the failed step threw.
    fn collect_settle_error(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        st: usize,
        error: CommittedValueError,
    ) -> Result<(), CommittedValueError> {
        // A completed child or error-build refusal cannot run IteratorClose
        // or promise rejection. A fresh local allocator refusal keeps this
        // builtin's existing escaping allocation policy and exact cause.
        let err = match error {
            CommittedValueError::Fatal(_) => return Err(error),
            CommittedValueError::JavaScript(err @ VmError::OutOfMemory { .. }) => {
                return Err(CommittedValueError::JavaScript(err));
            }
            CommittedValueError::JavaScript(err) => err,
        };
        let reason = self
            .vm_error_to_throwable_with_stack_roots(Some(context), stack, &err)
            .map_err(CommittedValueError::Fatal)?;
        // §23.1.2.1 3.j.ii.8 — a failure part-way through iteration closes
        // the iterator before the caller's promise rejects. The close runs
        // user code, so the reason rides an anchor across it.
        let reason_slot = self.push_iteration_anchor(reason) - 1;
        let settled = (|| {
            self.collect_close_iterator(stack, context, st, reason)?;
            let reason = self.iteration_anchor(reason_slot);
            self.collect_settle(stack, context, st, slot::REJECT, reason)
        })();
        self.pop_iteration_anchors_to(reason_slot);
        settled
    }

    /// AsyncIteratorClose — ask the iterator to finish. The original
    /// rejection takes precedence over catchable `return` failures. Fatal
    /// completion and allocation refusal keep their actual typed cause.
    fn collect_close_iterator(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        st: usize,
        reason: Value,
    ) -> Result<(), CommittedValueError> {
        let state = self.iteration_anchor(st);
        let iterator = self.state_get(state, slot::ITERATOR);
        if iterator.is_undefined() {
            return Ok(());
        }
        // Only once: a `return` that fails must not be retried by the
        // rejection it causes.
        // The state write may collect before the observable `return` read;
        // one handle scope owns the iterator and original rejection throughout.
        self.with_handle_scope(|interp, scope| {
            let iterator = interp.scoped_value(scope, iterator);
            // The incoming pending throw may differ from the rejection reason.
            // Preserve only that actual owner; do not manufacture a pending
            // exception from a promise's ordinary rejection argument.
            let _reason = interp.scoped_value(scope, reason);
            let thrown = interp
                .take_pending_uncaught_throw()
                .map(|value| interp.scoped_value(scope, value));
            let detail = interp.take_error_detail();
            let frames = interp.pending_throw_provenance.take();
            interp
                .state_set(
                    interp.iteration_anchor(st),
                    slot::ITERATOR,
                    Value::undefined(),
                )
                .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
            let current = interp.escape_scoped(iterator);
            let close = match interp.collect_get(stack, context, current, "return") {
                Ok(method) if interp.is_callable_runtime(&method) => {
                    let current = interp.escape_scoped(iterator);
                    interp
                        .run_callable_sync_rooted(
                            stack,
                            Some(context),
                            &method,
                            current,
                            SmallVec::new(),
                        )
                        .map(|_| ())
                        .map_err(CommittedValueError::completed_call)
                }
                Ok(_) => Ok(()),
                Err(error) => Err(error),
            };
            if let Err(
                error @ (CommittedValueError::Fatal(_)
                | CommittedValueError::JavaScript(VmError::OutOfMemory { .. })),
            ) = close
            {
                // Cleanup's new failure owns its detail and frames. The
                // original completion must not replace its actual cause.
                return Err(error);
            }
            let current_thrown = thrown.map(|value| interp.escape_scoped(value));
            let _ = interp.take_pending_uncaught_throw();
            if let Some(value) = current_thrown {
                interp.set_pending_uncaught_throw(value);
            }
            *interp.pending_error_detail.borrow_mut() = detail;
            interp.pending_throw_provenance = frames;
            Ok(())
        })
    }

    fn collect_new_array(&mut self, len: usize) -> Result<Value, VmError> {
        self.with_handle_scope(|interp, scope| {
            let array = interp.scoped_array(scope, len)?;
            Ok(interp.escape_scoped(array))
        })
    }

    /// Publish the ordered iteration fields before the target array. All
    /// operands, including the unpublished target, belong to one handle scope.
    fn collect_publish_array_state(
        &mut self,
        st: usize,
        array: Value,
        fields: &[(&str, Value)],
    ) -> Result<(), VmError> {
        self.with_handle_scope(|interp, scope| {
            let state = interp.scoped_value(scope, interp.iteration_anchor(st));
            let array = interp.scoped_value(scope, array);
            let fields: Vec<_> = fields
                .iter()
                .map(|(key, value)| (*key, interp.scoped_value(scope, *value)))
                .collect();
            for (key, value) in fields {
                interp.state_set(
                    interp.escape_scoped(state),
                    key,
                    interp.escape_scoped(value),
                )?;
            }
            interp.state_set(
                interp.escape_scoped(state),
                slot::ARRAY,
                interp.escape_scoped(array),
            )
        })
    }

    fn collect_set_length(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        array: Value,
        len: f64,
    ) -> Result<(), CommittedValueError> {
        self.array_set_property_throwing(stack, context, array, "length", Value::number_f64(len))
    }

    /// §7.3.10 GetMethod — `undefined` and `null` both mean "absent"; a
    /// present non-callable is a TypeError.
    fn collect_method(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        target: Value,
        key: symbol::JsSymbol,
    ) -> Result<Option<Value>, CommittedValueError> {
        if target.is_nullish() {
            return Err(self.collect_type_error(stack, "Array.fromAsync of null or undefined"));
        }
        self.with_handle_scope(|interp, scope| {
            let receiver = interp.scoped_value(scope, target);
            // GetMethod goes through ToObject, so every non-nullish operand is
            // asked — a generator, a boxed primitive and a plain number alike.
            let method = match interp.ordinary_get_value_scoped(
                stack,
                Some(context),
                scope,
                receiver,
                receiver,
                &VmPropertyKey::Symbol(key),
                0,
            )? {
                VmGetOutcome::Value(value) => value,
                VmGetOutcome::InvokeGetter { getter } => interp
                    .run_callable_sync_rooted(
                        stack,
                        Some(context),
                        &getter,
                        interp.escape_scoped(receiver),
                        SmallVec::new(),
                    )
                    .map_err(CommittedValueError::completed_call)?,
            };
            if method.is_nullish() {
                return Ok(None);
            }
            if !interp.is_callable_runtime(&method) {
                return Err(interp.collect_type_error(stack, "iterator method is not callable"));
            }
            Ok(Some(method))
        })
    }

    fn collect_get(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        target: Value,
        key: &str,
    ) -> Result<Value, CommittedValueError> {
        self.with_handle_scope(|interp, scope| {
            let receiver = interp.scoped_value(scope, target);

            match interp.ordinary_get_value_scoped(
                stack,
                Some(context),
                scope,
                receiver,
                receiver,
                &VmPropertyKey::String(key),
                0,
            )? {
                VmGetOutcome::Value(value) => Ok(value),
                VmGetOutcome::InvokeGetter { getter } => interp
                    .run_callable_sync_rooted(
                        stack,
                        Some(context),
                        &getter,
                        interp.escape_scoped(receiver),
                        SmallVec::new(),
                    )
                    .map_err(CommittedValueError::completed_call),
            }
        })
    }

    fn state_set(&mut self, state: Value, key: &str, value: Value) -> Result<(), VmError> {
        let mut object = state.as_object().ok_or(VmError::InvalidOperand)?;
        self.create_data_property(&mut object, key, value)
    }

    fn state_get(&self, state: Value, key: &str) -> Value {
        state
            .as_object()
            .and_then(|object| crate::object::get(object, &self.gc_heap, key))
            .unwrap_or_else(Value::undefined)
    }

    fn state_number(&self, state: Value, key: &str) -> f64 {
        self.state_get(state, key).as_f64().unwrap_or(0.0)
    }
}

/// Where the walk resumes once an awaited value settles.
#[derive(Clone, Copy)]
enum Step {
    /// The result record of an iterator step.
    IteratorResult,
    /// One slot of an array-like.
    ArrayLikeValue,
    /// The value `mapfn` produced.
    Mapped,
    /// The awaited value rejected.
    Reject,
    /// The value `mapfn` produced rejected, which closes the iterator the
    /// walk was part-way through.
    RejectAndClose,
}

/// `Array.fromAsync(items, mapfn?, thisArg?)`.
pub(crate) fn native_from_async(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    let this_value = *ctx.this_value();
    let context = match ctx.execution_context().cloned() {
        Some(context) => context,
        None => {
            return Err(crate::native_function::vm_to_native_error(
                ctx.interp_mut(),
                VmError::InvalidOperand,
                "Array.fromAsync",
            ));
        }
    };
    let args: SmallVec<[Value; 4]> = SmallVec::from_slice(args);
    ctx.with_turn_parts(|interp, stack| {
        interp
            .array_from_async(stack, &context, this_value, &args)
            .map_err(|err| err.into_native(interp, "Array.fromAsync"))
    })
}

#[cfg(test)]
mod tests;
