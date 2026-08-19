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
//! - Every step that can fail settles the caller's promise. A handler that
//!   simply threw would reject a derived promise nobody holds, and the
//!   caller's promise would never settle at all.
//! - The walk's state lives in one ordinary object held as a traced
//!   capture, so a collection between steps relocates it whole.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-array.fromasync>
//! - [`crate::async_from_sync_iterator`] — the adapter that awaits the
//!   values of a synchronous iterable.

use smallvec::SmallVec;

use crate::activation_stack::ActivationStack;
use crate::execution_context::ExecutionContext;
use crate::promise::JsPromise;
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
    /// Returns the failure behind allocating the promise itself. Everything
    /// the collection can go wrong with settles that promise instead.
    pub(crate) fn array_from_async(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        this_value: Value,
        args: &[Value],
    ) -> Result<Value, VmError> {
        let items = args.first().copied().unwrap_or_else(Value::undefined);
        let mapfn = args.get(1).copied().unwrap_or_else(Value::undefined);
        let this_arg = args.get(2).copied().unwrap_or_else(Value::undefined);

        let capability = crate::promise_dispatch::PromiseBuilder::with_context(context.clone())
            .capability_stack_rooted(self, stack, &[&items, &mapfn, &this_arg], &[])?;
        let promise = capability.promise;
        let state = self.alloc_runtime_rooted_object_with_roots(
            &[&promise, &capability.resolve, &capability.reject, &items],
            &[],
        )?;
        let state = Value::object(state);
        self.state_set(state, slot::RESOLVE, capability.resolve);
        self.state_set(state, slot::REJECT, capability.reject);
        self.state_set(state, slot::MAPFN, mapfn);
        self.state_set(state, slot::THIS_ARG, this_arg);
        self.state_set(state, slot::INDEX, Value::number_f64(0.0));

        match self.collect_begin(stack, context, state, this_value, items, mapfn) {
            Ok(()) => {}
            Err(err) => self.collect_settle_error(stack, context, state, err),
        }
        Ok(promise)
    }

    /// A `TypeError` instance, as the thrown value: the caller's promise
    /// rejects with the error object itself, not with a rendering of it.
    fn collect_type_error(&mut self, stack: &ActivationStack, message: &str) -> VmError {
        self.collect_error(stack, crate::error_classes::ErrorKind::TypeError, message)
    }

    /// A `RangeError` instance, as the thrown value.
    fn collect_range_error(&mut self, stack: &ActivationStack, message: &str) -> VmError {
        self.collect_error(stack, crate::error_classes::ErrorKind::RangeError, message)
    }

    fn collect_error(
        &mut self,
        stack: &ActivationStack,
        kind: crate::error_classes::ErrorKind,
        message: &str,
    ) -> VmError {
        match self.make_error_instance_with_stack_roots(
            stack,
            kind,
            Some(message.to_string()),
            &Value::undefined(),
        ) {
            Ok(object) => {
                self.set_pending_uncaught_throw(Value::object(object));
                self.err_uncaught(message.to_string().into())
            }
            Err(err) => err,
        }
    }

    /// Steps 3.a–3.j: validate `mapfn`, pick the iteration protocol, build
    /// the target array, and start the walk.
    fn collect_begin(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        state: Value,
        this_value: Value,
        items: Value,
        mapfn: Value,
    ) -> Result<(), VmError> {
        if !mapfn.is_undefined() && !self.is_callable_runtime(&mapfn) {
            return Err(self.collect_type_error(stack, "mapper is not a function"));
        }

        let async_iterator_sym = self
            .well_known_symbols
            .get(symbol::WellKnown::AsyncIterator);
        let using_async = self.collect_method(stack, context, items, async_iterator_sym)?;
        let iterator = if let Some(method) = using_async {
            Some(self.run_callable_sync_rooted(stack, context, &method, items, SmallVec::new())?)
        } else {
            let iterator_sym = self.well_known_symbols.get(symbol::WellKnown::Iterator);
            match self.collect_method(stack, context, items, iterator_sym)? {
                Some(method) => {
                    let sync = self.run_callable_sync_rooted(
                        stack,
                        context,
                        &method,
                        items,
                        SmallVec::new(),
                    )?;
                    if !sync.is_object_type() && !sync.is_proxy() {
                        return Err(self.collect_type_error(
                            stack,
                            "iterator method did not answer with an object",
                        ));
                    }
                    Some(self.create_async_from_sync_iterator(sync)?)
                }
                None => None,
            }
        };

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
                let next = self.collect_get(stack, context, iterator, "next")?;
                let array = if constructing {
                    self.run_construct_sync_rooted(
                        stack,
                        context,
                        &this_value,
                        this_value,
                        SmallVec::new(),
                    )?
                } else {
                    self.collect_new_array(stack, 0)?
                };
                self.state_set(state, slot::ITERATOR, iterator);
                self.state_set(state, slot::NEXT, next);
                self.state_set(state, slot::ARRAY, array);
                self.collect_pump(stack, context, state)
            }
            None => {
                let array_like = if items.is_object_type() || items.is_proxy() {
                    items
                } else {
                    self.box_sloppy_this_primitive_runtime_rooted(items, &[])?
                };
                let len = crate::array_prototype::length_of_array_like(
                    self,
                    stack,
                    context,
                    &array_like,
                )? as f64;
                let array = if constructing {
                    let mut args: SmallVec<[Value; 8]> = SmallVec::new();
                    args.push(Value::number_f64(len));
                    self.run_construct_sync_rooted(stack, context, &this_value, this_value, args)?
                } else {
                    // §10.4.2.2 ArrayCreate — a length past 2^32 - 1 is not
                    // an array length at all.
                    if len > 4_294_967_295.0 {
                        return Err(self.collect_range_error(stack, "Invalid array length"));
                    }
                    self.collect_new_array(stack, len as usize)?
                };
                self.state_set(state, slot::ARRAY_LIKE, array_like);
                self.state_set(state, slot::LENGTH, Value::number_f64(len));
                self.state_set(state, slot::ARRAY, array);
                self.collect_pump(stack, context, state)
            }
        }
    }

    /// One step: read the next element (or the next array-like slot) and
    /// arrange for the walk to continue once it resolves.
    fn collect_pump(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        state: Value,
    ) -> Result<(), VmError> {
        let index = self.state_number(state, slot::INDEX);
        let iterator = self.state_get(state, slot::ITERATOR);
        if iterator.is_undefined() {
            // Array-like: every slot is read, then awaited.
            let len = self.state_number(state, slot::LENGTH);
            if index >= len {
                let array = self.state_get(state, slot::ARRAY);
                self.collect_set_length(stack, context, array, len)?;
                return self.collect_settle(stack, context, state, slot::RESOLVE, array);
            }
            let array_like = self.state_get(state, slot::ARRAY_LIKE);
            let key = crate::number::NumberValue::from_f64(index).to_display_string();
            let value = self.collect_get(stack, context, array_like, &key)?;
            return self.collect_await(stack, context, state, value, Step::ArrayLikeValue);
        }

        let next = self.state_get(state, slot::NEXT);
        let result =
            self.run_callable_sync_rooted(stack, context, &next, iterator, SmallVec::new())?;
        if !result.is_object_type() && !result.is_proxy() {
            return Err(self.collect_type_error(stack, "iterator result is not an object"));
        }
        self.collect_await(stack, context, state, result, Step::IteratorResult)
    }

    /// Resolve `value` and continue at `step` once it settles.
    fn collect_await(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        state: Value,
        value: Value,
        step: Step,
    ) -> Result<(), VmError> {
        self.collect_await_with(stack, context, state, value, step, Step::Reject)
    }

    /// Resolve `value`, continuing at `step` when it settles and at
    /// `failure` when it rejects.
    fn collect_await_with(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        state: Value,
        value: Value,
        step: Step,
        failure: Step,
    ) -> Result<(), VmError> {
        let inner_value = self.promise_resolve_value(stack, context, value)?;

        let on_fulfilled = self.collect_handler(state, &inner_value, step)?;
        let on_rejected = self.collect_handler(state, &inner_value, failure)?;
        let capability = crate::promise_dispatch::PromiseBuilder::with_context(context.clone())
            .capability_stack_rooted(
                self,
                stack,
                &[&on_fulfilled, &on_rejected, &inner_value, &state],
                &[],
            )?;
        let inner = inner_value.as_promise().ok_or(VmError::InvalidOperand)?;
        let async_context = self.async_context();
        let outcome = JsPromise::perform_then_with_context(
            &inner,
            &mut self.gc_heap,
            Some(on_fulfilled),
            Some(on_rejected),
            capability,
            Some(context.clone()),
            async_context,
        );
        if let Some(job) = outcome.immediate_job {
            self.microtasks.enqueue(job);
        }
        Ok(())
    }

    /// One reaction handler, carrying the walk's state.
    fn collect_handler(
        &mut self,
        state: Value,
        root: &Value,
        step: Step,
    ) -> Result<Value, VmError> {
        crate::native_function::native_value_with_captures_unchecked_with_roots(
            &mut self.gc_heap,
            "Array.fromAsync",
            SmallVec::from_slice(&[state]),
            &mut |visitor| root.trace_value_slots(visitor),
            move |ctx, args, captures| {
                let state = captures.first().copied().unwrap_or_else(Value::undefined);
                let value = args.first().copied().unwrap_or_else(Value::undefined);
                ctx.with_turn_parts(|interp, stack| {
                    let Some(context) = interp.realm_execution_context() else {
                        return;
                    };
                    let outcome = interp.collect_step(stack, &context, state, value, step);
                    if let Err(err) = outcome {
                        interp.collect_settle_error(stack, &context, state, err);
                    }
                });
                Ok(Value::undefined())
            },
        )
        .map_err(VmError::from)
    }

    /// Continue the walk after one awaited value.
    fn collect_step(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        state: Value,
        value: Value,
        step: Step,
    ) -> Result<(), VmError> {
        match step {
            Step::Reject => self.collect_settle(stack, context, state, slot::REJECT, value),
            Step::RejectAndClose => {
                self.collect_close_iterator(stack, context, state, value);
                self.pending_uncaught_throw = None;
                self.collect_settle(stack, context, state, slot::REJECT, value)
            }
            Step::IteratorResult => {
                let done = self.collect_get(stack, context, value, "done")?;
                if done.to_boolean(&self.gc_heap) {
                    let array = self.state_get(state, slot::ARRAY);
                    let index = self.state_number(state, slot::INDEX);
                    self.collect_set_length(stack, context, array, index)?;
                    return self.collect_settle(stack, context, state, slot::RESOLVE, array);
                }
                let element = self.collect_get(stack, context, value, "value")?;
                self.collect_map(stack, context, state, element)
            }
            Step::ArrayLikeValue => self.collect_map(stack, context, state, value),
            Step::Mapped => self.collect_append(stack, context, state, value),
        }
    }

    /// Apply `mapfn` when there is one, then append; the mapped value is
    /// awaited in turn.
    fn collect_map(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        state: Value,
        element: Value,
    ) -> Result<(), VmError> {
        let mapfn = self.state_get(state, slot::MAPFN);
        if mapfn.is_undefined() {
            return self.collect_append(stack, context, state, element);
        }
        let index = self.state_number(state, slot::INDEX);
        let this_arg = self.state_get(state, slot::THIS_ARG);
        let mut args: SmallVec<[Value; 8]> = SmallVec::new();
        args.push(element);
        args.push(Value::number_f64(index));
        let mapped = self.run_callable_sync_rooted(stack, context, &mapfn, this_arg, args)?;
        self.collect_await_with(
            stack,
            context,
            state,
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
        state: Value,
        value: Value,
    ) -> Result<(), VmError> {
        let array = self.state_get(state, slot::ARRAY);
        let index = self.state_number(state, slot::INDEX);
        self.create_data_property_array_index(stack, context, array, index as usize, value)?;
        self.state_set(state, slot::INDEX, Value::number_f64(index + 1.0));
        self.collect_pump(stack, context, state)
    }

    /// Settle the caller's promise through the capability's own function.
    fn collect_settle(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        state: Value,
        which: &str,
        value: Value,
    ) -> Result<(), VmError> {
        let settle = self.state_get(state, which);
        if !self.is_callable_runtime(&settle) {
            return Ok(());
        }
        let mut args: SmallVec<[Value; 8]> = SmallVec::new();
        args.push(value);
        self.run_callable_sync_rooted(stack, context, &settle, Value::undefined(), args)?;
        Ok(())
    }

    /// Reject the caller's promise with whatever the failed step threw.
    fn collect_settle_error(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        state: Value,
        err: VmError,
    ) {
        let reason = match self.pending_uncaught_throw.take() {
            Some(thrown) => thrown,
            None => {
                // The failure carries a message but no thrown value yet.
                // The caller catches an error object, so build the one the
                // failure stands for rather than a rendering of it.
                let kind = match err {
                    VmError::RangeError => crate::error_classes::ErrorKind::RangeError,
                    VmError::SyntaxError => crate::error_classes::ErrorKind::SyntaxError,
                    VmError::URIError => crate::error_classes::ErrorKind::URIError,
                    _ => crate::error_classes::ErrorKind::TypeError,
                };
                let message = self.render_vm_error(&err);
                match self.make_error_instance_with_stack_roots(
                    stack,
                    kind,
                    Some(message),
                    &Value::undefined(),
                ) {
                    Ok(object) => Value::object(object),
                    Err(_) => Value::undefined(),
                }
            }
        };
        // §23.1.2.1 3.j.ii.8 — a failure part-way through iteration closes
        // the iterator before the caller's promise rejects.
        self.collect_close_iterator(stack, context, state, reason);
        let _ = self.collect_settle(stack, context, state, slot::REJECT, reason);
    }

    /// AsyncIteratorClose — ask the iterator to finish. The original
    /// failure is what the caller sees, so whatever `return` does with its
    /// own abrupt completion is discarded.
    fn collect_close_iterator(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        state: Value,
        reason: Value,
    ) {
        let iterator = self.state_get(state, slot::ITERATOR);
        if iterator.is_undefined() {
            return;
        }
        // Only once: a `return` that fails must not be retried by the
        // rejection it causes.
        self.state_set(state, slot::ITERATOR, Value::undefined());
        let Ok(method) = self.collect_get(stack, context, iterator, "return") else {
            self.pending_uncaught_throw = Some(reason);
            return;
        };
        if !self.is_callable_runtime(&method) {
            return;
        }
        let _ = self.run_callable_sync_rooted(stack, context, &method, iterator, SmallVec::new());
        // The close must not replace the failure that caused it.
        self.pending_uncaught_throw = None;
    }

    fn collect_new_array(&mut self, stack: &ActivationStack, len: usize) -> Result<Value, VmError> {
        let array = self.alloc_stack_rooted_array_from_values_with_root_slices(
            stack,
            Vec::new(),
            &[],
            &[],
        )?;
        if len > 0 {
            crate::array::set_length(array, self.gc_heap_mut(), len).map_err(VmError::from)?;
        }
        Ok(Value::array(array))
    }

    fn collect_set_length(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        array: Value,
        len: f64,
    ) -> Result<(), VmError> {
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
    ) -> Result<Option<Value>, VmError> {
        if target.is_nullish() {
            return Err(self.collect_type_error(stack, "Array.fromAsync of null or undefined"));
        }
        // GetMethod goes through ToObject, so every non-nullish operand is
        // asked — a generator, a boxed primitive and a plain number alike.
        let method = match self.ordinary_get_value(
            stack,
            context,
            target,
            target,
            &VmPropertyKey::Symbol(key),
            0,
        )? {
            VmGetOutcome::Value(value) => value,
            VmGetOutcome::InvokeGetter { getter } => {
                self.run_callable_sync_rooted(stack, context, &getter, target, SmallVec::new())?
            }
        };
        if method.is_nullish() {
            return Ok(None);
        }
        if !self.is_callable_runtime(&method) {
            return Err(self.collect_type_error(stack, "iterator method is not callable"));
        }
        Ok(Some(method))
    }

    fn collect_get(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        target: Value,
        key: &str,
    ) -> Result<Value, VmError> {
        match self.ordinary_get_value(
            stack,
            context,
            target,
            target,
            &VmPropertyKey::String(key),
            0,
        )? {
            VmGetOutcome::Value(value) => Ok(value),
            VmGetOutcome::InvokeGetter { getter } => {
                self.run_callable_sync_rooted(stack, context, &getter, target, SmallVec::new())
            }
        }
    }

    fn state_set(&mut self, state: Value, key: &str, value: Value) {
        if let Some(mut object) = state.as_object() {
            crate::object::set(&mut object, &mut self.gc_heap, key, value);
        }
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
    let context = ctx
        .execution_context()
        .cloned()
        .ok_or_else(|| NativeError::TypeError {
            name: "Array.fromAsync",
            reason: "missing execution context".to_string(),
        })?;
    let args: SmallVec<[Value; 4]> = SmallVec::from_slice(args);
    ctx.with_turn_parts(|interp, stack| {
        interp
            .array_from_async(stack, &context, this_value, &args)
            .map_err(|err| {
                crate::native_function::vm_to_native_error(interp, err, "Array.fromAsync")
            })
    })
}
