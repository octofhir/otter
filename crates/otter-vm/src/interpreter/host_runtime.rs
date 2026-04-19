//! M0 stubs for the host/runtime helpers that previously lived in the
//! v1 dispatch module.
//!
//! These methods sit on [`Interpreter`] and are called from
//! non-dispatch crate surfaces (`runtime_state::call`,
//! `runtime_state::iterators`, `interpreter::call_function`) for
//! constructing receivers, invoking native functions, and resuming
//! generators.
//!
//! During the M0 migration the source compiler is a stub that rejects
//! every input with [`SourceLoweringError::Unsupported`]
//! (`crate::source_compiler`), so no JS actually runs and none of these
//! paths are reached. Each stub returns
//! [`InterpreterError::NativeCall`] with an "M0" tag so that if a path
//! is unexpectedly entered — e.g. during intrinsic bootstrap — the
//! failure mode is loud and easy to trace.
//!
//! The stubs will be replaced with real implementations in later
//! milestones (see `V2_MIGRATION.md`) once the AST lowering covers
//! enough of ES2024 to need them.

use crate::descriptors::VmNativeCallError;
use crate::intrinsics::{GeneratorResumeKind, IntrinsicKey};
use crate::object::ObjectHandle;
use crate::value::RegisterValue;

use super::step_outcome::Completion;
use super::{Interpreter, InterpreterError, RuntimeState};

const M0_UNSUPPORTED: &str = "runtime helper not available in M0 (source compiler stub blocks all execution; \
     this path will be restored in a later milestone)";

impl Interpreter {
    /// §10.4.1.1 `[[Call]]` for host-backed callables.
    ///
    /// M19: delegate to [`RuntimeState::call_host_function`], which
    /// already handles bound functions, promise capabilities, and
    /// native-function descriptor dispatch. Needed so `console.log`
    /// and other host intrinsics are actually reachable from
    /// compiled bytecode. Returns `Completion::Return` on success
    /// and `Completion::Throw` on a caught JS throw; internal
    /// errors surface as `InterpreterError::NativeCall`.
    pub(super) fn invoke_host_function_handle(
        runtime: &mut RuntimeState,
        callable: ObjectHandle,
        receiver: RegisterValue,
        arguments: &[RegisterValue],
    ) -> Result<Completion, InterpreterError> {
        match runtime.call_host_function(Some(callable), receiver, arguments) {
            Ok(value) => Ok(Completion::Return(value)),
            Err(VmNativeCallError::Thrown(value)) => Ok(Completion::Throw(value)),
            Err(VmNativeCallError::Internal(message)) => Err(InterpreterError::NativeCall(message)),
        }
    }

    /// Invoke a native function resolved to a
    /// [`crate::host::HostFunctionId`]. Falls through to the
    /// handle-based entry point — the handle itself carries the
    /// descriptor id, and `call_host_function` already performs
    /// the descriptor lookup and callback dispatch. The
    /// `is_construct` parameter is currently ignored — constructor
    /// calls reach a different path in `construct_callable` and
    /// shouldn't end up here in practice.
    pub(super) fn invoke_registered_host_function(
        runtime: &mut RuntimeState,
        _host_function: crate::host::HostFunctionId,
        callable: ObjectHandle,
        receiver: RegisterValue,
        arguments: &[RegisterValue],
        _is_construct: bool,
    ) -> Result<Completion, InterpreterError> {
        Self::invoke_host_function_handle(runtime, callable, receiver, arguments)
    }

    /// §9.1.12 `OrdinaryCreateFromConstructor` — creates a new
    /// object whose `[[Prototype]]` is `new_target.prototype`
    /// (falling back to the intrinsic default's prototype when
    /// `new_target.prototype` isn't an Object). Called by
    /// `construct_callable` when the constructor is a plain
    /// class-constructor closure and needs a receiver.
    ///
    /// M27: real implementation. Reads the new_target's
    /// `prototype` property via the runtime's property-lookup
    /// path, falls through to `IntrinsicKey::ObjectPrototype`
    /// when the read doesn't produce an object.
    pub(super) fn allocate_construct_receiver(
        runtime: &mut RuntimeState,
        new_target: ObjectHandle,
        intrinsic_default: IntrinsicKey,
    ) -> Result<ObjectHandle, InterpreterError> {
        let prototype_prop = runtime.intern_property_name("prototype");
        let proto_value = match runtime.property_lookup(new_target, prototype_prop)? {
            Some(lookup) => match lookup.value() {
                crate::object::PropertyValue::Data { value, .. } => value,
                crate::object::PropertyValue::Accessor { .. } => RegisterValue::undefined(),
            },
            None => RegisterValue::undefined(),
        };
        let proto = proto_value
            .as_object_handle()
            .map(ObjectHandle)
            .unwrap_or_else(|| runtime.intrinsics().get(intrinsic_default));
        Ok(runtime.alloc_object_with_prototype(Some(proto)))
    }

    /// Resolve which intrinsic a native constructor uses as the default
    /// prototype when its own `.prototype` slot is absent.
    ///
    /// Placeholder in M0 — returns [`IntrinsicKey::ObjectPrototype`] as
    /// the safe default so call sites don't have to short-circuit.
    pub(super) fn host_function_default_intrinsic(
        _runtime: &RuntimeState,
        _host_function: crate::host::HostFunctionId,
    ) -> IntrinsicKey {
        IntrinsicKey::ObjectPrototype
    }

    /// §9.2.1.16 — `Construct` return rule: if the body returned
    /// an Object, keep it; otherwise use the default receiver
    /// (the freshly-allocated `this`). M27 implements the real
    /// rule; pre-M27 builds had a stub that returned the body's
    /// value verbatim, which wrongly surfaced `undefined` from
    /// empty constructors instead of the new object.
    pub(super) fn apply_construct_return_override(
        completion: Completion,
        default_receiver: RegisterValue,
    ) -> Completion {
        match completion {
            Completion::Return(value) => {
                // Keep explicit Object returns; replace
                // primitives / undefined with the allocated
                // receiver. The object-handle check covers
                // plain Objects, Arrays, closures, etc.; the
                // NaN-boxed non-object values (int32, bool,
                // null, undefined, symbol, bigint-handle,
                // f64) all fall through to the else arm.
                if value.as_object_handle().is_some() {
                    Completion::Return(value)
                } else {
                    Completion::Return(default_receiver)
                }
            }
            other => other,
        }
    }

    /// §27.5.1.1 GeneratorResume / §27.5.1.2 GeneratorResumeAbrupt —
    /// drives a suspended generator to its next yield / completion.
    ///
    /// Dispatches per the generator's current state:
    /// - `SuspendedStart`: copies the stored arguments into a fresh
    ///   activation and runs the body from PC 0.
    /// - `SuspendedYield`: restores the saved register window,
    ///   places the caller-provided `sent_value` into the
    ///   accumulator (Next), throws it (Throw), or forces a
    ///   premature return (Return), then resumes at the PC after
    ///   the `Yield` opcode.
    /// - `Completed`: Next/Throw return
    ///   `{ value: undefined, done: true }` / re-throw, Return
    ///   returns the supplied value.
    ///
    /// The returned register value is an iterator-result object
    /// (`{ value, done }`) on normal completion; thrown errors
    /// surface as `VmNativeCallError::Thrown`.
    pub(super) fn resume_generator_impl(
        runtime: &mut RuntimeState,
        generator: ObjectHandle,
        sent_value: RegisterValue,
        resume_kind: GeneratorResumeKind,
    ) -> Result<RegisterValue, VmNativeCallError> {
        use crate::frame::{FrameFlags, FrameMetadata};
        use crate::object::GeneratorState;
        use crate::property::PropertyNameId;

        use super::Activation;
        use super::step_outcome::{Completion, StepOutcome};

        let state = runtime.objects.generator_state(generator).map_err(|err| {
            VmNativeCallError::Internal(format!("generator_state: {err:?}").into())
        })?;

        // §27.5.1.2 step 3 — resuming an already-completed
        // generator: Next yields `{ undefined, true }`,
        // Return yields `{ value, true }`, Throw re-raises.
        if state == GeneratorState::Completed {
            return match resume_kind {
                GeneratorResumeKind::Throw => Err(VmNativeCallError::Thrown(sent_value)),
                GeneratorResumeKind::Return => {
                    let handle = runtime.create_iter_result(sent_value, true)?;
                    Ok(RegisterValue::from_object_handle(handle.0))
                }
                GeneratorResumeKind::Next => {
                    let handle = runtime.create_iter_result(RegisterValue::undefined(), true)?;
                    Ok(RegisterValue::from_object_handle(handle.0))
                }
            };
        }

        // §27.5.1.2 step 2 — resuming an already-executing
        // generator is a bug (should be caught by the prototype
        // dispatch, but guard here too).
        if state == GeneratorState::Executing {
            return Err(Self::throw_as_type_error_native(
                runtime,
                "generator is already running",
            ));
        }

        let (
            module,
            function_index,
            closure_handle,
            arguments,
            saved_registers,
            resume_pc,
            _resume_reg,
        ) = runtime
            .objects
            .generator_take_state(generator)
            .map_err(|err| {
                VmNativeCallError::Internal(format!("generator_take_state: {err:?}").into())
            })?;

        let callee_function = module
            .function(function_index)
            .ok_or(InterpreterError::InvalidCallTarget)
            .map_err(|e| VmNativeCallError::Internal(format!("{e}").into()))?;

        let register_count = callee_function.frame_layout().register_count();
        let mut activation = Activation::with_context(
            function_index,
            register_count,
            FrameMetadata::new(arguments.len() as u16, FrameFlags::default()),
            closure_handle,
        );

        if let Some(saved) = saved_registers {
            activation
                .copy_registers_from_slice(&saved)
                .map_err(|e| VmNativeCallError::Internal(format!("{e}").into()))?;
            activation.set_pc(resume_pc);
            match resume_kind {
                GeneratorResumeKind::Next => {
                    activation.set_accumulator(sent_value);
                }
                GeneratorResumeKind::Return => {
                    // §27.5.1.2 GeneratorResumeAbrupt with Return —
                    // short-circuit: mark completed and return the
                    // iterator result directly.
                    let _ = runtime
                        .objects
                        .set_generator_state(generator, GeneratorState::Completed);
                    let handle = runtime.create_iter_result(sent_value, true)?;
                    return Ok(RegisterValue::from_object_handle(handle.0));
                }
                GeneratorResumeKind::Throw => {
                    // §27.5.1.2 GeneratorResumeAbrupt with
                    // Throw — the thrown value surfaces at the
                    // yield point. If the current PC sits
                    // inside a try handler, route to the
                    // handler; otherwise propagate the throw
                    // out of the generator and mark it
                    // completed.
                    if !Self::for_runtime(runtime).transfer_exception(
                        callee_function,
                        &mut activation,
                        sent_value,
                    ) {
                        let _ = runtime
                            .objects
                            .set_generator_state(generator, GeneratorState::Completed);
                        return Err(VmNativeCallError::Thrown(sent_value));
                    }
                }
            }
        } else {
            // First call — bind arguments into parameter slots.
            let param_count = callee_function.frame_layout().parameter_count();
            for (i, &arg) in arguments.iter().take(param_count as usize).enumerate() {
                let abs = callee_function
                    .frame_layout()
                    .resolve_user_visible(i as u16)
                    .ok_or(InterpreterError::RegisterOutOfBounds)
                    .map_err(|e| VmNativeCallError::Internal(format!("{e}").into()))?;
                activation
                    .set_register(abs, arg)
                    .map_err(|e| VmNativeCallError::Internal(format!("{e}").into()))?;
            }
            if arguments.len() > param_count as usize {
                activation.overflow_args = arguments[param_count as usize..].to_vec();
            }
            // `GeneratorResumeKind::Throw` on a first-call gen:
            // §27.5.1.2 step 4.b throws before running anything.
            if matches!(resume_kind, GeneratorResumeKind::Throw) {
                let _ = runtime
                    .objects
                    .set_generator_state(generator, GeneratorState::Completed);
                return Err(VmNativeCallError::Thrown(sent_value));
            }
            if matches!(resume_kind, GeneratorResumeKind::Return) {
                let _ = runtime
                    .objects
                    .set_generator_state(generator, GeneratorState::Completed);
                let handle = runtime.create_iter_result(sent_value, true)?;
                return Ok(RegisterValue::from_object_handle(handle.0));
            }
        }

        // Custom step loop — like `run_completion_with_runtime`
        // but intercepts `StepOutcome::GeneratorYield` to
        // capture register state + PC into the generator
        // before returning to the `.next()` caller.
        let interpreter = Self::for_runtime(runtime);
        let mut frame_runtime = crate::interpreter::FrameRuntimeState::new(callee_function);
        let completion = loop {
            activation.begin_step();
            let outcome = match interpreter.step(
                callee_function,
                &module,
                &mut activation,
                runtime,
                &mut frame_runtime,
            ) {
                Ok(o) => o,
                Err(InterpreterError::UncaughtThrow(v)) => break Completion::Throw(v),
                Err(other) => {
                    return Err(VmNativeCallError::Internal(format!("{other}").into()));
                }
            };
            match outcome {
                StepOutcome::Continue => {
                    activation
                        .sync_written_open_upvalues(runtime)
                        .map_err(|e| VmNativeCallError::Internal(format!("{e}").into()))?;
                    activation
                        .refresh_open_upvalues_from_cells(runtime)
                        .map_err(|e| VmNativeCallError::Internal(format!("{e}").into()))?;
                }
                StepOutcome::Return(v) => break Completion::Return(v),
                StepOutcome::Throw(v) => {
                    if Self::for_runtime(runtime).transfer_exception(
                        callee_function,
                        &mut activation,
                        v,
                    ) {
                        continue;
                    }
                    break Completion::Throw(v);
                }
                StepOutcome::GeneratorYield {
                    yielded_value,
                    resume_register,
                } => {
                    // Snapshot activation state into the generator
                    // so the next `.next()` can resume exactly
                    // where we left off.
                    let snapshot: Box<[RegisterValue]> =
                        activation.registers().to_vec().into_boxed_slice();
                    runtime
                        .objects
                        .generator_save_state(generator, snapshot, activation.pc(), resume_register)
                        .map_err(|err| {
                            VmNativeCallError::Internal(
                                format!("generator_save_state: {err:?}").into(),
                            )
                        })?;
                    let handle = runtime.create_iter_result(yielded_value, false)?;
                    return Ok(RegisterValue::from_object_handle(handle.0));
                }
                StepOutcome::TailCall(_) => {
                    return Err(VmNativeCallError::Internal(
                        "tail call inside generator body not supported".into(),
                    ));
                }
                StepOutcome::Suspend { .. } => {
                    return Err(VmNativeCallError::Internal(
                        "await inside generator body reached without async wrapper".into(),
                    ));
                }
            }
        };

        // Body finished or threw — mark generator completed and
        // return the appropriate iterator result / throw.
        let _ = runtime
            .objects
            .set_generator_state(generator, GeneratorState::Completed);
        match completion {
            Completion::Return(v) => {
                let handle = runtime.create_iter_result(v, true)?;
                // Borrow of `PropertyNameId` needed to silence
                // the unused-import warning in some feature
                // combinations; keep the type alias even though
                // we don't read from it here.
                let _unused: PropertyNameId = runtime.intern_property_name("value");
                Ok(RegisterValue::from_object_handle(handle.0))
            }
            Completion::Throw(v) => Err(VmNativeCallError::Thrown(v)),
        }
    }

    /// Small helper mirroring the runtime's
    /// `throw_as_type_error` — but callable from
    /// `host_runtime.rs` where the runtime-state private helper
    /// isn't in scope.
    fn throw_as_type_error_native(runtime: &mut RuntimeState, message: &str) -> VmNativeCallError {
        match runtime.alloc_type_error(message) {
            Ok(handle) => VmNativeCallError::Thrown(RegisterValue::from_object_handle(handle.0)),
            Err(err) => VmNativeCallError::Internal(format!("{err}").into()),
        }
    }

    /// Resume a suspended async generator. Placeholder in M0.
    pub(super) fn resume_async_generator_impl(
        _runtime: &mut RuntimeState,
        _generator: ObjectHandle,
    ) -> Result<(), VmNativeCallError> {
        Err(VmNativeCallError::Internal(M0_UNSUPPORTED.into()))
    }
}
