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

    /// §9.1.12 `OrdinaryCreateFromConstructor` default receiver
    /// allocation for construct calls.
    ///
    /// Placeholder in M0.
    pub(super) fn allocate_construct_receiver(
        _runtime: &mut RuntimeState,
        _new_target: ObjectHandle,
        _intrinsic_default: IntrinsicKey,
    ) -> Result<ObjectHandle, InterpreterError> {
        Err(InterpreterError::NativeCall(M0_UNSUPPORTED.into()))
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

    /// §9.2.1.16 — Apply the ES2024 rule that a `Construct` call returns
    /// the explicit object produced by the body, else the default
    /// `this` receiver.
    ///
    /// Placeholder in M0: returns whatever the completion already holds.
    pub(super) fn apply_construct_return_override(
        completion: Completion,
        _default_receiver: RegisterValue,
    ) -> Completion {
        completion
    }

    /// Resume a suspended generator. Placeholder in M0.
    pub(super) fn resume_generator_impl(
        _runtime: &mut RuntimeState,
        _generator: ObjectHandle,
        _sent_value: RegisterValue,
        _resume_kind: GeneratorResumeKind,
    ) -> Result<RegisterValue, VmNativeCallError> {
        Err(VmNativeCallError::Internal(M0_UNSUPPORTED.into()))
    }

    /// Resume a suspended async generator. Placeholder in M0.
    pub(super) fn resume_async_generator_impl(
        _runtime: &mut RuntimeState,
        _generator: ObjectHandle,
    ) -> Result<(), VmNativeCallError> {
        Err(VmNativeCallError::Internal(M0_UNSUPPORTED.into()))
    }
}
