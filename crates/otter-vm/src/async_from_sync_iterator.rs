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
use crate::promise::JsPromise;
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
        sync_iterator: Value,
    ) -> Result<Value, VmError> {
        let wrapper = self.alloc_runtime_rooted_object_with_roots(&[&sync_iterator], &[])?;
        let wrapper_value = Value::object(wrapper);
        for (name, call) in [
            ("next", step_next as StepFn),
            ("return", step_return as StepFn),
            ("throw", step_throw as StepFn),
        ] {
            let method = self.async_from_sync_step(name, call, sync_iterator, &wrapper_value)?;
            let Some(wrapper) = wrapper_value.as_object() else {
                return Err(VmError::InvalidOperand);
            };
            let mut wrapper = wrapper;
            crate::object::set(&mut wrapper, &mut self.gc_heap, name, method);
        }
        Ok(wrapper_value)
    }

    /// One of the wrapper's three methods, holding the sync iterator as a
    /// traced capture.
    fn async_from_sync_step(
        &mut self,
        name: &'static str,
        call: StepFn,
        sync_iterator: Value,
        wrapper_root: &Value,
    ) -> Result<Value, VmError> {
        crate::native_function::native_value_with_captures_unchecked_with_roots(
            &mut self.gc_heap,
            name,
            SmallVec::from_slice(&[sync_iterator]),
            &mut |visitor| wrapper_root.trace_value_slots(visitor),
            move |ctx, args, captures| {
                let sync_iterator = captures.first().copied().unwrap_or_else(Value::undefined);
                let argument = args.first().copied();
                call(ctx, sync_iterator, argument)
            },
        )
        .map_err(VmError::from)
    }

    /// Read one property off a value the way an ordinary `[[Get]]` would,
    /// running an accessor when the property is one.
    fn iterator_member(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        target: Value,
        name: &str,
    ) -> Result<Value, VmError> {
        match self.ordinary_get_value(
            stack,
            context,
            target,
            target,
            &VmPropertyKey::String(name),
            0,
        )? {
            VmGetOutcome::Value(value) => Ok(value),
            VmGetOutcome::InvokeGetter { getter } => {
                self.run_callable_sync_rooted(stack, context, &getter, target, SmallVec::new())
            }
        }
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
        context: &ExecutionContext,
        result: Value,
        sync_iterator: Value,
        close_on_rejection: bool,
    ) -> Result<Value, VmError> {
        let done = self.iterator_member(stack, context, result, "done")?;
        let done = done.to_boolean(&self.gc_heap);
        let value = self.iterator_member(stack, context, result, "value")?;

        // PromiseResolve(%Promise%, value): a thenable is adopted, anything
        // else settles at once.
        let inner_value = self.promise_resolve_value(stack, context, value)?;

        let on_fulfilled = crate::native_function::native_value_with_captures_unchecked_with_roots(
            &mut self.gc_heap,
            "AsyncFromSyncIteratorContinuation",
            SmallVec::from_slice(&[Value::boolean(done)]),
            &mut |visitor| {
                inner_value.trace_value_slots(visitor);
                sync_iterator.trace_value_slots(visitor);
            },
            move |ctx, args, captures| {
                let done = captures
                    .first()
                    .and_then(|value| value.as_boolean())
                    .unwrap_or(false);
                let value = args.first().copied().unwrap_or_else(Value::undefined);
                iter_result_object(ctx, value, done)
            },
        )
        .map_err(VmError::from)?;

        let on_rejected = if close_on_rejection && !done {
            Some(
                crate::native_function::native_value_with_captures_unchecked_with_roots(
                    &mut self.gc_heap,
                    "AsyncFromSyncIteratorRejected",
                    SmallVec::from_slice(&[sync_iterator]),
                    &mut |visitor| {
                        inner_value.trace_value_slots(visitor);
                        on_fulfilled.trace_value_slots(visitor);
                    },
                    move |ctx, args, captures| {
                        let sync_iterator =
                            captures.first().copied().unwrap_or_else(Value::undefined);
                        let reason = args.first().copied().unwrap_or_else(Value::undefined);
                        ctx.with_turn_parts(|interp, stack| {
                            let Some(context) = interp.realm_execution_context() else {
                                return;
                            };
                            let _ = interp.iterator_close_sync(stack, &context, &sync_iterator);
                        });
                        ctx.interp_mut().set_pending_uncaught_throw(reason);
                        Err(NativeError::Thrown {
                            name: "AsyncFromSyncIteratorRejected",
                            message: String::new(),
                        })
                    },
                )
                .map_err(VmError::from)?,
            )
        } else {
            None
        };

        let capability = crate::promise_dispatch::PromiseBuilder::with_context(context.clone())
            .capability_stack_rooted(
                self,
                stack,
                &[&on_fulfilled, &inner_value, &sync_iterator],
                &[],
            )?;
        let promise = capability.promise;
        let inner = inner_value.as_promise().ok_or(VmError::InvalidOperand)?;
        let async_context = self.async_context();
        let outcome = JsPromise::perform_then_with_context(
            &inner,
            &mut self.gc_heap,
            Some(on_fulfilled),
            on_rejected,
            capability,
            Some(context.clone()),
            async_context,
        );
        if let Some(job) = outcome.immediate_job {
            self.microtasks.enqueue(job);
        }
        Ok(promise)
    }
}

/// The shape of one wrapper method: the sync iterator plus the argument the
/// caller passed.
type StepFn = fn(&mut NativeCtx<'_>, Value, Option<Value>) -> Result<Value, NativeError>;

/// `{ value, done }`, freshly allocated per step.
fn iter_result_object(
    ctx: &mut NativeCtx<'_>,
    value: Value,
    done: bool,
) -> Result<Value, NativeError> {
    ctx.scope(|mut scope| {
        let result = scope.object()?;
        let value = scope.value(value);
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
    argument: Option<Value>,
) -> Result<Value, NativeError> {
    step(ctx, sync_iterator, "next", argument, true, |_, _| {
        // `next` is required: an iterator without one is not one.
        None
    })
}

/// §27.1.4.2.2 `%AsyncFromSyncIteratorPrototype%.return`.
fn step_return(
    ctx: &mut NativeCtx<'_>,
    sync_iterator: Value,
    argument: Option<Value>,
) -> Result<Value, NativeError> {
    step(ctx, sync_iterator, "return", argument, false, |ctx, arg| {
        // A sync iterator with no `return` is already finished.
        Some(iter_result_object(
            ctx,
            arg.unwrap_or_else(Value::undefined),
            true,
        ))
    })
}

/// §27.1.4.2.3 `%AsyncFromSyncIteratorPrototype%.throw`.
fn step_throw(
    ctx: &mut NativeCtx<'_>,
    sync_iterator: Value,
    argument: Option<Value>,
) -> Result<Value, NativeError> {
    step(ctx, sync_iterator, "throw", argument, true, |ctx, arg| {
        // A sync iterator with no `throw` is closed and the reason is
        // handed back to the caller.
        let reason = arg.unwrap_or_else(Value::undefined);
        ctx.interp_mut().set_pending_uncaught_throw(reason);
        Some(Err(NativeError::Thrown {
            name: "AsyncFromSyncIterator.throw",
            message: String::new(),
        }))
    })
}

/// Drive one step of the sync iterator and hand its result to the
/// continuation. `missing` supplies the answer when the sync iterator has
/// no such method.
fn step(
    ctx: &mut NativeCtx<'_>,
    sync_iterator: Value,
    name: &'static str,
    argument: Option<Value>,
    close_on_rejection: bool,
    missing: impl FnOnce(&mut NativeCtx<'_>, Option<Value>) -> Option<Result<Value, NativeError>>,
) -> Result<Value, NativeError> {
    let context = ctx
        .execution_context()
        .cloned()
        .ok_or_else(|| NativeError::TypeError {
            name,
            reason: "missing execution context".to_string(),
        })?;
    let method = ctx.with_turn_parts(|interp, stack| {
        interp.iterator_member(stack, &context, sync_iterator, name)
    });
    let method = method.map_err(|err| vm_error(ctx, err, name))?;
    if method.is_nullish() {
        return match missing(ctx, argument) {
            Some(answer) => answer,
            None => Err(NativeError::TypeError {
                name,
                reason: format!("iterator has no '{name}' method"),
            }),
        };
    }

    let mut args: SmallVec<[Value; 8]> = SmallVec::new();
    if let Some(argument) = argument {
        args.push(argument);
    }
    let result = ctx.with_turn_parts(|interp, stack| {
        interp.run_callable_sync_rooted(stack, &context, &method, sync_iterator, args)
    });
    let result = result.map_err(|err| vm_error(ctx, err, name))?;
    if !is_object_like(&result) {
        return Err(NativeError::TypeError {
            name,
            reason: format!("'{name}' did not answer with an object"),
        });
    }
    ctx.with_turn_parts(|interp, stack| {
        interp.async_from_sync_continuation(
            stack,
            &context,
            result,
            sync_iterator,
            close_on_rejection,
        )
    })
    .map_err(|err| vm_error(ctx, err, name))
}

/// Whether an iterator step answered with something that can carry
/// `value` / `done`.
fn is_object_like(value: &Value) -> bool {
    value.is_object_type() || value.is_proxy()
}

fn vm_error(ctx: &mut NativeCtx<'_>, err: VmError, name: &'static str) -> NativeError {
    crate::native_function::vm_to_native_error(ctx.interp_mut(), err, name)
}
