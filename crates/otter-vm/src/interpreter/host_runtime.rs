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
