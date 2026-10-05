//! §27.1.4 `CreateAsyncFromSyncIterator` — the adapter `for await` puts in
//! front of an iterable that has no `@@asyncIterator`.
//!
//! # Contents
//! - [`Interpreter::create_async_from_sync_iterator`] wraps a synchronous
//!   iterator in an object whose `next` / `return` / `throw` answer with
//!   promises.
//!
//! # Invariants
//! - Each step's *value* is awaited, not just the result record: iterating
//!   a plain array of promises with `for await` yields what those promises
//!   resolve to, which is the whole point of the adapter.
//!   `done` is read before the await and travels with the awaited value.
//! - The wrapper never reaches user code — it lives in the loop's own
//!   register — so its methods sit on the object itself rather than on a
//!   shared prototype, and an abrupt step propagates as a throw rather
//!   than as a rejected promise nobody could observe.
//! - Every capture is a traced `Value`, so a collection between steps
//!   relocates the sync iterator with the wrapper that holds it.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-createasyncfromsynciterator>
//! - [`crate::iterator_ops`] — `GetAsyncIterator`, which builds the wrapper.

use smallvec::SmallVec;

use crate::activation_stack::ActivationStack;
use crate::execution_context::ExecutionContext;
use crate::runtime_activation::CommittedValueError;
use crate::runtime_cx::NativeCtx;
use crate::{Interpreter, NativeError, Value, VmError, VmGetOutcome, VmPropertyKey};

impl Interpreter {
    /// §27.1.4.1 — wrap `sync_iterator` (the object a `@@iterator` call
    /// produced) in the async iterator `for await` drives.
    ///
    /// # Errors
    /// Returns the failure behind allocating the wrapper or its methods.
    pub(crate) fn create_async_from_sync_iterator(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        sync_iterator: Value,
    ) -> Result<Value, CommittedValueError> {
        // §27.1.4.1 / GetIteratorDirect — the sync record caches its `next`
        // method once at adapter creation; every later step calls the
        // cached function instead of re-reading the property.
        //
        // Every value that must survive the three method allocations is
        // parked in the handle arena and re-read after each allocating
        // sub-call: a stack-local `Value` "updated in place" through a
        // shared reference is not a root the optimizer has to honor.
        // Reading `next` runs a getter, so the iterator is parked before it.
        let base = self.json_root_push(sync_iterator);
        let next_method = match self.iterator_member(stack, Some(context), sync_iterator, "next") {
            Ok(next_method) => next_method,
            Err(error) => {
                self.json_root_pop_to(base);
                return Err(error);
            }
        };
        let next_root = self.json_root_push(next_method);
        let result = (|| {
            let sync_iterator = self.json_root_get(base);
            let next_method = self.json_root_get(next_root);
            let wrapper = self
                .alloc_runtime_rooted_object_with_roots(&[&sync_iterator, &next_method], &[])
                .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
            let wrapper_root = self.json_root_push(Value::object(wrapper));
            for (name, call, caches_next) in [
                ("next", step_next as StepFn, true),
                ("return", step_return as StepFn, false),
                ("throw", step_throw as StepFn, false),
            ] {
                let sync_iterator = self.json_root_get(base);
                let cached = caches_next.then(|| self.json_root_get(next_root));
                let method = self
                    .async_from_sync_step(name, call, sync_iterator, cached)
                    .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
                let Some(mut wrapper) = self.json_root_get(wrapper_root).as_object() else {
                    return Err(CommittedValueError::Fatal(VmError::InvalidOperand));
                };
                self.create_data_property(&mut wrapper, name, method)
                    .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
            }
            Ok(self.json_root_get(wrapper_root))
        })();
        self.json_root_pop_to(base);
        result
    }

    /// One of the wrapper's three methods, holding the sync iterator as a
    /// traced capture.
    fn async_from_sync_step(
        &mut self,
        name: &'static str,
        call: StepFn,
        sync_iterator: Value,
        cached_method: Option<Value>,
    ) -> Result<Value, VmError> {
        let mut captures = SmallVec::from_slice(&[sync_iterator]);
        if let Some(method) = cached_method {
            captures.push(method);
        }
        let value = crate::native_function::native_value_with_captures_unchecked_with_roots(
            &mut self.gc_heap,
            name,
            captures,
            &mut |_visitor| {},
            move |ctx, args, captures| {
                let sync_iterator = captures.first().copied().unwrap_or_else(Value::undefined);
                let cached_method = captures.get(1).copied();
                let argument = args.first().copied();
                call(ctx, sync_iterator, cached_method, argument)
            },
        )
        .map_err(VmError::from)?;
        Ok(self.stamp_native_creation_realm(value))
    }

    /// Read one property off a value the way an ordinary `[[Get]]` would,
    /// running an accessor when the property is one.
    fn iterator_member(
        &mut self,
        stack: &mut ActivationStack,
        context: Option<&ExecutionContext>,
        target: Value,
        name: &str,
    ) -> Result<Value, CommittedValueError> {
        self.with_handle_scope(|interp, scope| {
            let target_root = interp.scoped_value(scope, target);

            match interp.ordinary_get_value(
                stack,
                context,
                target,
                target,
                &VmPropertyKey::String(name),
                0,
            )? {
                VmGetOutcome::Value(value) => Ok(value),
                VmGetOutcome::InvokeGetter { getter } => interp
                    .run_callable_sync_rooted(
                        stack,
                        context,
                        &getter,
                        interp.escape_scoped(target_root),
                        SmallVec::new(),
                    )
                    .map_err(CommittedValueError::completed_call),
            }
        })
    }

    /// §27.1.4.4 AsyncFromSyncIteratorContinuation — settle on the step's
    /// value once it resolves, carrying the `done` read before the await.
    ///
    /// `close_on_rejection` runs the sync iterator's `return` when the
    /// awaited value rejects mid-iteration, the way an abrupt body
    /// completion would.
    fn async_from_sync_continuation(
        &mut self,
        stack: &mut ActivationStack,
        context: Option<&ExecutionContext>,
        result: Value,
        sync_iterator: Value,
        close_on_rejection: bool,
    ) -> Result<Value, CommittedValueError> {
        // The `done` and `value` reads run getters and PromiseResolve runs
        // `then` lookups: the result and the iterator are parked for them.
        let entry = self.json_root_push(result);
        self.json_root_push(sync_iterator);
        let outcome =
            self.async_from_sync_continuation_parked(stack, context, entry, close_on_rejection);
        self.json_root_pop_to(entry);
        outcome
    }

    fn async_from_sync_continuation_parked(
        &mut self,
        stack: &mut ActivationStack,
        context: Option<&ExecutionContext>,
        entry: usize,
        close_on_rejection: bool,
    ) -> Result<Value, CommittedValueError> {
        let done = self.iterator_member(stack, context, self.json_root_get(entry), "done")?;
        let done = done.to_boolean(&self.gc_heap);
        let value = self.iterator_member(stack, context, self.json_root_get(entry), "value")?;

        // PromiseResolve(%Promise%, value): a thenable is adopted, anything
        // else settles at once.
        //
        // Every value carried across the handler / capability allocations is
        // parked in the handle arena and re-read afterwards — a stack-local
        // `Value` "updated in place" through a shared reference is not a
        // root the optimizer has to honor.
        // §27.1.4.4 step 6 — an abrupt PromiseResolve (a poisoned
        // `constructor` / `then` on the step's value) closes the sync
        // iterator first when the wrapper still drives iteration, then the
        // original abrupt value rejects the capability (IteratorClose
        // preserves the incoming throw completion).
        let inner_value = match self.promise_resolve_value(stack, context, value) {
            Ok(inner) => inner,
            Err(error) => {
                // An escaping engine/control failure never starts observable
                // IteratorClose or becomes a rejected promise completion.
                if matches!(&error, CommittedValueError::Fatal(_)) {
                    return Err(error);
                }
                if close_on_rejection && !done {
                    let sync_iterator = self.json_root_get(entry + 1);
                    self.iterator_close_discarding_completion(stack, context, &sync_iterator)?;
                }
                return Err(error);
            }
        };
        let base = self.json_root_push(inner_value);
        let sync_root = entry + 1;
        let result = (|| {
            let on_fulfilled =
                crate::native_function::native_value_with_captures_unchecked_with_roots(
                    &mut self.gc_heap,
                    "AsyncFromSyncIteratorContinuation",
                    SmallVec::from_slice(&[Value::boolean(done)]),
                    &mut |_visitor| {},
                    move |ctx, args, captures| {
                        let done = captures
                            .first()
                            .and_then(|value| value.as_boolean())
                            .unwrap_or(false);
                        let value = args.first().copied().unwrap_or_else(Value::undefined);
                        iter_result_object(ctx, value, done)
                    },
                )
                .map_err(VmError::from)
                .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
            let on_fulfilled = self.stamp_native_creation_realm(on_fulfilled);
            let fulfilled_root = self.json_root_push(on_fulfilled);

            let rejected_root = if close_on_rejection && !done {
                let sync_iterator = self.json_root_get(sync_root);
                let on_rejected =
                    crate::native_function::native_value_with_captures_unchecked_with_roots(
                        &mut self.gc_heap,
                        "AsyncFromSyncIteratorRejected",
                        SmallVec::from_slice(&[sync_iterator]),
                        &mut |_visitor| {},
                        move |ctx, args, captures| {
                            let context = ctx.execution_context().cloned();
                            ctx.scope(|mut scope| {
                                let iterator = scope.value(
                                    captures.first().copied().unwrap_or_else(Value::undefined),
                                );
                                let reason = scope
                                    .value(args.first().copied().unwrap_or_else(Value::undefined));
                                let value = scope.raw(reason);
                                scope
                                    .context()
                                    .interp_mut()
                                    .set_pending_uncaught_throw(value);
                                let iterator = scope.raw(iterator);
                                scope.with_turn_parts(|interp, stack| {
                                    interp
                                        .iterator_close_discarding_completion(
                                            stack,
                                            context.as_ref(),
                                            &iterator,
                                        )
                                        .map_err(|error| {
                                            error.into_native(
                                                interp,
                                                "AsyncFromSyncIteratorRejected",
                                            )
                                        })
                                })?;
                                let reason = scope.raw(reason);
                                Err(scope
                                    .context()
                                    .throw_value("AsyncFromSyncIteratorRejected", reason))
                            })
                        },
                    )
                    .map_err(VmError::from)
                    .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
                let on_rejected = self.stamp_native_creation_realm(on_rejected);
                Some(self.json_root_push(on_rejected))
            } else {
                None
            };

            let capability =
                crate::promise_dispatch::PromiseBuilder::with_context(context.cloned())
                    .capability_stack_rooted(self, stack, &[], &[])
                    .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
            let promise_root = self.json_root_push(capability.promise);
            let inner = self
                .json_root_get(base)
                .as_promise()
                .ok_or(VmError::InvalidOperand)
                .map_err(|error| CommittedValueError::Fatal(error.into()))?;
            let on_fulfilled = self.json_root_get(fulfilled_root);
            let on_rejected = rejected_root.map(|root| self.json_root_get(root));
            let outcome = self
                .register_promise_reactions(
                    inner,
                    Some(on_fulfilled),
                    on_rejected,
                    capability,
                    context.cloned(),
                )
                .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
            if let Some(job) = outcome.immediate_job {
                self.microtasks.enqueue(job);
            }
            Ok(self.json_root_get(promise_root))
        })();
        self.json_root_pop_to(base);
        result
    }
}

/// IfAbruptRejectPromise — every one of the adapter's methods answers with
/// a promise. Catchable source failures become rejections; completed terminal
/// failures leave before rejection materialization.
fn rejected_promise(
    ctx: &mut NativeCtx<'_>,
    context: Option<&ExecutionContext>,
    error: CommittedValueError,
    name: &'static str,
) -> Result<Value, NativeError> {
    let err = match error {
        CommittedValueError::Fatal(error) => {
            return Err(CommittedValueError::Fatal(error).into_native(ctx.interp_mut(), name));
        }
        CommittedValueError::JavaScript(error) => error,
    };
    let reason = ctx.with_turn_parts(|interp, stack| {
        interp
            .vm_error_to_throwable_with_stack_roots(context, stack, &err)
            .map_err(|error| crate::native_function::vm_to_native_error(interp, error, name))
    })?;
    reject_with(ctx, context, reason, name)
}

/// A promise already rejected with `reason`.
fn reject_with(
    ctx: &mut NativeCtx<'_>,
    context: Option<&ExecutionContext>,
    reason: Value,
    name: &'static str,
) -> Result<Value, NativeError> {
    ctx.with_turn_parts(|interp, stack| {
        crate::promise_dispatch::PromiseBuilder::with_context(context.cloned())
            .rejected_stack_rooted(interp, stack, reason, &[&reason], &[])
            .map(Value::promise)
            .map_err(|error| {
                CommittedValueError::JavaScript(VmError::from(error)).into_native(interp, name)
            })
    })
}

/// A `TypeError` instance, as a value.
fn type_error_value(ctx: &mut NativeCtx<'_>, message: &str) -> Result<Value, NativeError> {
    let message = message.to_string();
    ctx.with_turn_parts(|interp, stack| {
        interp
            .make_error_instance_with_stack_roots(
                stack,
                crate::error_classes::ErrorKind::TypeError,
                Some(message),
                &Value::undefined(),
            )
            .map(Value::object)
            .map_err(|error| {
                crate::native_function::vm_to_native_error(interp, error, "AsyncFromSyncIterator")
            })
    })
}

/// The shape of one wrapper method: the sync iterator plus the argument the
/// caller passed.
type StepFn =
    fn(&mut NativeCtx<'_>, Value, Option<Value>, Option<Value>) -> Result<Value, NativeError>;

/// `{ value, done }`, freshly allocated per step.
fn iter_result_object(
    ctx: &mut NativeCtx<'_>,
    value: Value,
    done: bool,
) -> Result<Value, NativeError> {
    ctx.scope(|mut scope| {
        let value = scope.value(value);
        let result = scope.object()?;
        scope.set(result, "value", value)?;
        let done = scope.boolean(done);
        scope.set(result, "done", done)?;
        Ok(scope.finish(result))
    })
}

/// §27.1.4.2.1 `%AsyncFromSyncIteratorPrototype%.next`.
fn step_next(
    ctx: &mut NativeCtx<'_>,
    sync_iterator: Value,
    cached_method: Option<Value>,
    argument: Option<Value>,
) -> Result<Value, NativeError> {
    step(
        ctx,
        sync_iterator,
        "next",
        cached_method,
        argument,
        true,
        |_, _| {
            // `next` is required: an iterator without one is not one.
            None
        },
    )
}

/// §27.1.4.2.2 `%AsyncFromSyncIteratorPrototype%.return`.
fn step_return(
    ctx: &mut NativeCtx<'_>,
    sync_iterator: Value,
    _cached_method: Option<Value>,
    argument: Option<Value>,
) -> Result<Value, NativeError> {
    step(
        ctx,
        sync_iterator,
        "return",
        None,
        argument,
        false,
        |ctx, arg| {
            // A sync iterator with no `return` is already finished, and the
            // answer is still a promise.
            let context = ctx.execution_context().cloned();
            let record = match iter_result_object(ctx, arg.unwrap_or_else(Value::undefined), true) {
                Ok(record) => record,
                Err(err) => return Some(Err(err)),
            };
            Some(ctx.with_turn_parts(|interp, stack| {
                crate::promise_dispatch::PromiseBuilder::with_context(context)
                    .fulfilled_stack_rooted(interp, stack, record, &[&record], &[])
                    .map(Value::promise)
                    .map_err(|error| {
                        CommittedValueError::JavaScript(VmError::from(error))
                            .into_native(interp, "AsyncFromSyncIterator.return")
                    })
            }))
        },
    )
}

/// §27.1.4.2.3 `%AsyncFromSyncIteratorPrototype%.throw`.
fn step_throw(
    ctx: &mut NativeCtx<'_>,
    sync_iterator: Value,
    _cached_method: Option<Value>,
    argument: Option<Value>,
) -> Result<Value, NativeError> {
    step(
        ctx,
        sync_iterator,
        "throw",
        None,
        argument,
        true,
        move |ctx, _arg| {
            // §27.1.4.2.3 — a sync iterator with no `throw` is closed, and the
            // caller learns that the protocol was not there, not what it tried
            // to throw. A THROWING close wins, though: IteratorClose step 6
            // returns its own abrupt completion (e.g. a poisoned `return`
            // getter), and that value rejects the promise instead of the
            // protocol TypeError.
            let context = ctx.execution_context().cloned();
            let close_error = ctx.with_turn_parts(|interp, stack| {
                match interp.iterator_close_sync(stack, context.as_ref(), &sync_iterator) {
                    Ok(()) => Ok(None),
                    Err(error @ CommittedValueError::Fatal(_)) => {
                        Err(error.into_native(interp, "AsyncFromSyncIterator.throw"))
                    }
                    Err(CommittedValueError::JavaScript(error)) => interp
                        .vm_error_to_throwable_with_stack_roots(context.as_ref(), stack, &error)
                        .map(Some)
                        .map_err(|error| {
                            crate::native_function::vm_to_native_error(
                                interp,
                                error,
                                "AsyncFromSyncIterator.throw",
                            )
                        }),
                }
            });
            let reason = match close_error {
                Err(error) => return Some(Err(error)),
                Ok(Some(reason)) => reason,
                Ok(None) => match type_error_value(ctx, "iterator has no 'throw' method") {
                    Ok(reason) => reason,
                    Err(error) => return Some(Err(error)),
                },
            };
            Some(reject_with(
                ctx,
                context.as_ref(),
                reason,
                "AsyncFromSyncIterator.throw",
            ))
        },
    )
}

/// Drive one step of the sync iterator and hand its result to the
/// continuation. `missing` supplies the answer when the sync iterator has
/// no such method.
fn step(
    ctx: &mut NativeCtx<'_>,
    sync_iterator: Value,
    name: &'static str,
    cached_method: Option<Value>,
    argument: Option<Value>,
    close_on_rejection: bool,
    missing: impl FnOnce(&mut NativeCtx<'_>, Option<Value>) -> Option<Result<Value, NativeError>>,
) -> Result<Value, NativeError> {
    // The member read and the method call run JavaScript, which allocates:
    // the iterator and the argument ride anchors for the whole step and are
    // read back at each use.
    let anchor = ctx.with_turn_parts(|interp, _| {
        let anchor = interp.push_iteration_anchor(sync_iterator) - 1;
        interp.push_iteration_anchor(argument.unwrap_or_else(Value::undefined));
        anchor
    });
    let outcome = step_anchored(
        ctx,
        anchor,
        argument.is_some(),
        name,
        cached_method,
        close_on_rejection,
        missing,
    );
    ctx.with_turn_parts(|interp, _| interp.pop_iteration_anchors_to(anchor));
    outcome
}

fn step_anchored(
    ctx: &mut NativeCtx<'_>,
    anchor: usize,
    has_argument: bool,
    name: &'static str,
    cached_method: Option<Value>,
    close_on_rejection: bool,
    missing: impl FnOnce(&mut NativeCtx<'_>, Option<Value>) -> Option<Result<Value, NativeError>>,
) -> Result<Value, NativeError> {
    let anchored = |ctx: &mut NativeCtx<'_>, index: usize| {
        ctx.with_turn_parts(|interp, _| interp.iteration_anchor(index))
    };
    let context = ctx.execution_context().cloned();
    let method = match cached_method {
        // The sync record cached this method at adapter creation
        // (GetIteratorDirect); no per-step property read.
        Some(method) => Ok(method),
        None => ctx.with_turn_parts(|interp, stack| {
            let sync_iterator = interp.iteration_anchor(anchor);
            interp.iterator_member(stack, context.as_ref(), sync_iterator, name)
        }),
    };
    let method = match method {
        Ok(method) => method,
        Err(err) => return rejected_promise(ctx, context.as_ref(), err, name),
    };
    if method.is_nullish() {
        let argument = has_argument.then(|| anchored(ctx, anchor + 1));
        return match missing(ctx, argument) {
            Some(answer) => answer,
            None => {
                let reason = type_error_value(ctx, "iterator has no 'next' method")?;
                reject_with(ctx, context.as_ref(), reason, name)
            }
        };
    }

    let result = ctx.with_turn_parts(|interp, stack| {
        let mut args: SmallVec<[Value; 8]> = SmallVec::new();
        if has_argument {
            args.push(interp.iteration_anchor(anchor + 1));
        }
        let sync_iterator = interp.iteration_anchor(anchor);
        interp.run_callable_sync_rooted(stack, context.as_ref(), &method, sync_iterator, args)
    });
    let result = match result {
        Ok(result) => result,
        Err(error) => {
            return rejected_promise(
                ctx,
                context.as_ref(),
                CommittedValueError::completed_call(error),
                name,
            );
        }
    };
    if !is_object_like(&result) {
        let reason = type_error_value(ctx, "iterator result is not an object")?;
        return reject_with(ctx, context.as_ref(), reason, name);
    }
    let outcome = ctx.with_turn_parts(|interp, stack| {
        let sync_iterator = interp.iteration_anchor(anchor);
        interp.async_from_sync_continuation(
            stack,
            context.as_ref(),
            result,
            sync_iterator,
            close_on_rejection,
        )
    });
    match outcome {
        Ok(promise) => Ok(promise),
        Err(error @ CommittedValueError::Fatal(_)) => {
            Err(error.into_native(ctx.interp_mut(), name))
        }
        Err(CommittedValueError::JavaScript(error)) => rejected_promise(
            ctx,
            context.as_ref(),
            CommittedValueError::JavaScript(error),
            name,
        ),
    }
}

/// Whether an iterator step answered with something that can carry
/// `value` / `done`.
fn is_object_like(value: &Value) -> bool {
    value.is_object_type() || value.is_proxy()
}
