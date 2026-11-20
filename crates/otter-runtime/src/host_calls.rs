//! Global installation and calls from the embedder into JavaScript.
//!
//! # Contents
//! Native globals, rooted callable handles and reentrant host calls.
//!
//! # Invariants
//! Host calls enter the runtime execution and rooting boundary.
//!
//! # See also
//! - [`crate::Runtime`] owns the isolate session.

use crate::*;

impl Runtime {
    /// Install `value` as a `globalThis.<name>` data property.
    /// Standard descriptor attributes (`{ writable: true,
    /// enumerable: false, configurable: true }` per §17 + §19) are
    /// applied so the binding behaves like every other default
    /// global. Embedders call this from the runner thread before re-entering
    /// script execution; async native results instead materialize through the
    /// typed host-completion protocol.
    pub fn set_global(&mut self, name: &str, value: otter_vm::Value) -> Result<(), OtterError> {
        self.interp.set_global(name, value).map_err(|error| {
            map_vm_error(otter_vm::RunError {
                error,
                frames: Vec::new(),
                detail: None,
            })
        })
    }

    /// Define `value[Symbol.toStringTag]` as a non-enumerable host tag.
    ///
    /// Host integrations use this for Node/Web globals whose observable brand
    /// differs from a plain object while still being ordinary host-owned
    /// objects in the VM.
    pub fn define_to_string_tag(
        &mut self,
        value: otter_vm::Value,
        tag: &str,
    ) -> Result<(), OtterError> {
        self.interp
            .define_to_string_tag(value, tag)
            .map_err(OtterError::from)
    }

    pub(crate) fn global_this_value(&self) -> otter_vm::Value {
        otter_vm::Value::object(*self.interp.global_this())
    }

    /// The `globalThis` object as a value. Used by hosts that expose an alias
    /// global (e.g. Node's `global`).
    #[must_use]
    pub fn global_this(&self) -> otter_vm::Value {
        self.global_this_value()
    }

    /// Install a host-defined native function as a global binding.
    ///
    /// The function is allocated on the runtime's GC heap as a
    /// `Value::native_function` with the given `name` and arity, then
    /// stored on `globalThis` via [`Self::set_global`]. Standard
    /// descriptor attributes (`{ writable: true, enumerable: false,
    /// configurable: true }`) are applied.
    ///
    /// Used by `crates/otter-test262/src/agent.rs` to plug the
    /// `__otter_agent_*` family of `$262.agent` host bindings into
    /// every fresh per-test runtime without modifying the otter-vm
    /// bootstrap.
    ///
    /// # Errors
    /// Returns [`OtterError::OutOfMemory`] when the heap cap blocks
    /// the native function allocation.
    pub fn install_native_global(
        &mut self,
        name: &'static str,
        length: u8,
        call: RuntimeNativeFastFn,
    ) -> Result<(), OtterError> {
        let value = self
            .interp
            .native_function_static_host_rooted(name, length, call, &mut [], &mut [])
            .map_err(|oom| OtterError::OutOfMemory {
                requested_bytes: oom.requested_bytes(),
                heap_limit_bytes: oom.heap_limit_bytes(),
            })?;
        self.set_global(name, value)?;
        Ok(())
    }

    /// Install a host-defined native function or closure as a global binding.
    ///
    /// This is the captured-state counterpart to [`Self::install_native_global`].
    /// Product crates use it when a global native needs immutable runtime-owned
    /// captures such as a capability snapshot. Captured JS values must still be
    /// supplied through the native-function capture list; this helper only
    /// exposes the call target shape, not arbitrary untraced VM handles.
    ///
    /// # Errors
    /// Returns [`OtterError::OutOfMemory`] when the heap cap blocks the native
    /// function allocation.
    pub fn install_native_global_call(
        &mut self,
        name: &'static str,
        length: u8,
        call: RuntimeNativeCall,
    ) -> Result<(), OtterError> {
        let value = self
            .interp
            .native_function_from_call_host_rooted(name, length, call, &mut [], &mut [])
            .map_err(|oom| OtterError::OutOfMemory {
                requested_bytes: oom.requested_bytes(),
                heap_limit_bytes: oom.heap_limit_bytes(),
            })?;
        self.set_global(name, value)?;
        Ok(())
    }

    pub(crate) fn install_native_constructor_global_call(
        &mut self,
        name: &'static str,
        length: u8,
        call: RuntimeNativeCall,
    ) -> Result<(), OtterError> {
        let value = self
            .interp
            .native_constructor_from_call_host_rooted(name, length, call, &mut [], &mut [])
            .map_err(|oom| OtterError::OutOfMemory {
                requested_bytes: oom.requested_bytes(),
                heap_limit_bytes: oom.heap_limit_bytes(),
            })?;
        self.set_global(name, value)?;
        Ok(())
    }

    pub(crate) fn dispatch_worker_message_event<F>(
        &mut self,
        materialize_data: F,
    ) -> Result<(), MessageEventDispatchError>
    where
        F: FnOnce(&mut otter_vm::NativeCtx<'_>) -> Result<otter_vm::Value, otter_vm::NativeError>,
    {
        let global = *self.interp.global_this();
        let global_value = otter_vm::Value::object(global);
        otter_vm::NativeCtx::with_host_context(
            &mut self.interp,
            otter_vm::NativeCallInfo::call(global_value),
            None,
            |ctx| {
                let global = ctx
                    .this_value()
                    .as_object()
                    .expect("worker event receiver is globalThis");
                let handler = otter_vm::object::get(global, ctx.heap(), "onmessage")
                    .unwrap_or_else(otter_vm::Value::undefined);
                if !handler.is_callable() {
                    return Ok(());
                }

                // The handler must survive payload materialization, which can
                // move the young generation before the call starts.
                let handler_root = ctx.persistent_root_insert(handler);
                let data = match materialize_data(ctx) {
                    Ok(data) => data,
                    Err(err) => {
                        ctx.persistent_root_remove(handler_root);
                        return Err(MessageEventDispatchError::Materialize(map_native_error(
                            err,
                        )));
                    }
                };
                let data_root = ctx.persistent_root_insert(data);
                let handler = ctx
                    .persistent_root_get(handler_root)
                    .expect("fresh worker handler root");
                let data = ctx
                    .persistent_root_get(data_root)
                    .expect("fresh worker message data root");

                let dispatch = ctx.scope(|mut scope| {
                    let global = scope.this();
                    let handler = scope.value(handler);
                    let data = scope.value(data);
                    let event = scope
                        .object()
                        .map_err(map_native_error)
                        .map_err(MessageEventDispatchError::Materialize)?;
                    let ty = scope
                        .string("message")
                        .map_err(map_native_error)
                        .map_err(MessageEventDispatchError::Materialize)?;
                    scope
                        .set(event, "type", ty)
                        .map_err(map_native_error)
                        .map_err(MessageEventDispatchError::Materialize)?;
                    scope
                        .set(event, "data", data)
                        .map_err(map_native_error)
                        .map_err(MessageEventDispatchError::Materialize)?;
                    // Preserve the actual callback's typed result until the
                    // scope releases its arena. The pending throw is already
                    // rooted by the interpreter; projection below transfers
                    // its defining-source frames and detail exactly once.
                    Ok(scope.call(handler, global, &[event]).map(|_| ()))
                });

                ctx.persistent_root_remove(data_root);
                ctx.persistent_root_remove(handler_root);
                match dispatch {
                    Ok(Ok(())) => Ok(()),
                    Ok(Err(error)) => Err(MessageEventDispatchError::Handler(map_vm_error(
                        ctx.take_native_error(error),
                    ))),
                    Err(error) => Err(error),
                }
            },
        )?;
        self.interp
            .drain_microtasks(|_, _| Ok(false))
            .map_err(|err| {
                MessageEventDispatchError::Handler(enrich_runtime_diagnostic_with_cause(
                    &mut self.interp,
                    map_vm_error(err),
                ))
            })
    }

    /// Keep the function bound to the global name `name` for
    /// [`Self::call`]: an embedder that calls the same function on every
    /// event, timer or observer turn calls it directly instead of compiling
    /// a source string that names it each time.
    ///
    /// # Errors
    /// [`OtterError::Internal`] when `name` is not a callable global.
    pub fn global_function(&mut self, name: &str) -> Result<RuntimeFunction, OtterError> {
        let global = otter_vm::Value::object(*self.interp.global_this());
        let context = self.interp.realm_execution_context();
        let root = otter_vm::NativeCtx::with_host_context(
            &mut self.interp,
            otter_vm::NativeCallInfo::call(global),
            context.as_ref(),
            |ctx| {
                Ok(ctx.scope(|mut scope| {
                    scope
                        .global(name)
                        .filter(|value| scope.is_callable(*value))
                        .map(|value| scope.persistent_root_insert(value))
                }))
            },
        )
        .map_err(map_native_error)?;
        root.map(|root| RuntimeFunction { root })
            .ok_or_else(|| OtterError::Internal {
                code: DiagnosticCode::NotCallable.as_str().to_string(),
                message: format!("`{name}` is not a callable global"),
            })
    }

    /// Stop keeping a function [`Self::global_function`] returned.
    pub fn release_function(&mut self, function: RuntimeFunction) {
        self.interp.persistent_root_remove(function.root);
    }

    /// Call a kept function with `this` undefined and `arguments`, as one
    /// turn: the same guarantees a [`Self::run_script`] of the source
    /// `name(...arguments)` gives — the microtask checkpoint runs after the
    /// call whether or not it threw, an exception nothing caught is offered
    /// to the uncaught-exception handlers and otherwise returned as the
    /// error, and an interrupt stops the call — without parsing, compiling
    /// or linking anything.
    ///
    /// # Errors
    /// An uncaught exception, a microtask that failed, or an interrupt.
    pub fn call(
        &mut self,
        function: RuntimeFunction,
        arguments: &[CallArgument<'_>],
    ) -> Result<ExecutionResult, OtterError> {
        self.with_direct_timeout(move |runtime| runtime.call_unbounded(function, arguments))
    }

    pub(super) fn call_unbounded(
        &mut self,
        function: RuntimeFunction,
        arguments: &[CallArgument<'_>],
    ) -> Result<ExecutionResult, OtterError> {
        let start = std::time::Instant::now();
        let global = otter_vm::Value::object(*self.interp.global_this());
        let context = self.interp.realm_execution_context();
        let called: Result<otter_vm::PersistentRootId, otter_vm::NativeError> =
            otter_vm::NativeCtx::with_host_context(
                &mut self.interp,
                otter_vm::NativeCallInfo::call(global),
                context.as_ref(),
                |ctx| {
                    let target = ctx.persistent_root_get(function.root).ok_or_else(|| {
                        otter_vm::NativeError::Error {
                            message: "the kept function was released".into(),
                        }
                    })?;
                    ctx.scope(|mut scope| {
                        let target = scope.value(target);
                        let receiver = scope.undefined();
                        let mut locals = Vec::with_capacity(arguments.len());
                        for argument in arguments {
                            locals.push(match *argument {
                                CallArgument::Undefined => scope.undefined(),
                                CallArgument::Null => scope.null(),
                                CallArgument::Bool(value) => scope.boolean(value),
                                CallArgument::Number(value) => scope.number(value),
                                CallArgument::String(text) => scope.string(text)?,
                            });
                        }
                        let result = scope.call(target, receiver, &locals)?;
                        Ok(scope.persistent_root_insert(result))
                    })
                },
            );
        let completion_root = match called {
            Ok(root) => Some(root),
            Err(error) if error.exit_code().is_some() => {
                self.pending_exit_code = error.exit_code();
                return Ok(ExecutionResult::from_exit_code(
                    error.exit_code().unwrap_or_default(),
                    start.elapsed(),
                ));
            }
            Err(error)
                if error.is_fatal()
                    || matches!(error, otter_vm::NativeError::OutOfMemory { .. }) =>
            {
                return Err(map_native_error(error));
            }
            Err(error) => {
                // The checkpoint still runs: a call can settle a promise and
                // then throw, and the reactions are owed their turn.
                let handled = self.dispatch_uncaught_exception(context.as_ref());
                let drained = self.drain_microtasks_dispatching_uncaught();
                if let Err(drain) = drained
                    && !self.absorb_termination(&drain)
                {
                    return Err(enrich_runtime_diagnostic_with_cause(
                        &mut self.interp,
                        map_vm_error(drain),
                    ));
                }
                return match handled {
                    Ok(true) => Ok(ExecutionResult::from_vm_value(
                        otter_vm::Value::undefined(),
                        start.elapsed(),
                        self.interp.gc_heap_mut(),
                    )),
                    Ok(false) => Err(map_native_error(error)),
                    Err(handler) => Err(handler),
                };
            }
        };
        let drained = self.drain_microtasks_dispatching_uncaught();
        let completion = completion_root
            .and_then(|root| self.interp.persistent_root_remove(root))
            .unwrap_or_else(otter_vm::Value::undefined);
        if let Err(drain) = drained
            && !self.absorb_termination(&drain)
        {
            return Err(enrich_runtime_diagnostic_with_cause(
                &mut self.interp,
                map_vm_error(drain),
            ));
        }
        Ok(ExecutionResult::from_vm_value(
            completion,
            start.elapsed(),
            self.interp.gc_heap_mut(),
        ))
    }

    /// Run one host event on the isolate thread through a fresh native context.
    ///
    /// The closure must materialize any JS values from owned host data while it
    /// is executing. VM values must not be stored in the task that calls this
    /// method; use persistent root ids for long-lived references.
    ///
    /// # Errors
    /// Returns mapped runtime errors for native failures or microtask drain
    /// failures.
    pub fn run_native_event<F>(
        &mut self,
        context: &ExecutionContext,
        run: F,
    ) -> Result<(), OtterError>
    where
        F: FnOnce(&mut otter_vm::NativeCtx<'_>) -> Result<otter_vm::Value, otter_vm::NativeError>,
    {
        let context = context.clone();
        self.with_direct_timeout(move |runtime| {
            runtime.run_native_event_unbounded(Some(&context), run)
        })
    }

    pub(super) fn run_native_event_unbounded<F>(
        &mut self,
        context: Option<&ExecutionContext>,
        run: F,
    ) -> Result<(), OtterError>
    where
        F: FnOnce(&mut otter_vm::NativeCtx<'_>) -> Result<otter_vm::Value, otter_vm::NativeError>,
    {
        let global = *self.interp.global_this();
        let global_value = otter_vm::Value::object(global);
        let result_root = match otter_vm::NativeCtx::with_host_context(
            &mut self.interp,
            otter_vm::NativeCallInfo::call(global_value),
            context,
            |ctx| run(ctx).map(|value| ctx.persistent_root_insert(value)),
        ) {
            Ok(root) => Some(root),
            Err(error) if error.exit_code().is_some() => {
                // Both direct and imported VM exits retain their exact code
                // after the callback's realm and scoped roots have restored.
                self.pending_exit_code = error.exit_code();
                return Ok(());
            }
            Err(error)
                if error.is_fatal()
                    || matches!(error, otter_vm::NativeError::OutOfMemory { .. }) =>
            {
                return Err(map_native_error(error));
            }
            Err(error) => {
                // A native event delivery that throws is an uncaught error
                // like a timer callback that throws: `process` and any active
                // domain get first refusal before it becomes the embedder's
                // problem. Only an unhandled throw fails the event.
                if self.dispatch_uncaught_exception(context)? {
                    None
                } else {
                    return Err(map_native_error(error));
                }
            }
        };
        let drain = match self.drain_microtasks_dispatching_uncaught() {
            Ok(()) => Ok(()),
            Err(err) => {
                if let otter_vm::VmError::Exit { code } = err.error {
                    self.pending_exit_code = Some(code);
                    Ok(())
                } else {
                    Err(enrich_runtime_diagnostic_with_cause(
                        &mut self.interp,
                        map_vm_error(err),
                    ))
                }
            }
        };
        if let Some(root) = result_root {
            self.interp.persistent_root_remove(root);
        }
        drain?;
        Ok(())
    }

    /// Record a termination raised by a drained microtask, if that is what
    /// this error is.
    ///
    /// Every host boundary that drains has to ask: a termination is not a
    /// diagnostic, and mapping one into an `OtterError` loses the exit —
    /// the loop then runs on until its remaining work drains.
    pub(super) fn absorb_termination(&mut self, error: &otter_vm::RunError) -> bool {
        if let otter_vm::VmError::Exit { code } = error.error {
            self.pending_exit_code = Some(code);
            return true;
        }
        false
    }

    /// Take an exit code requested by JavaScript during a host-driven
    /// callback. The isolate runner polls this after every task or timer
    /// message and completes the in-flight run with it.
    pub fn take_pending_exit_code(&mut self) -> Option<u8> {
        self.pending_exit_code.take()
    }

    /// The realm's execution context, once a top-level run has established
    /// one. A host task that must re-enter JavaScript needs it and has no
    /// context of its own.
    #[must_use]
    pub fn realm_execution_context(&self) -> Option<otter_vm::ExecutionContext> {
        self.interp.realm_execution_context()
    }
}
