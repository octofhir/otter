//! New VM backend for Otter.
//!
//! This crate is the fresh execution backend that will replace the current VM
//! architecture incrementally. It starts as a small scaffold with a strict
//! module split and a minimal public API.

#![deny(clippy::all)]
#![forbid(unsafe_code)]

/// Shared execution ABI.
pub mod abi;
/// ECMAScript abstract operations shared across interpreter and intrinsics.
pub mod abstract_ops;
/// Suspended async function context for await support.
pub mod async_context;
/// BigInt-constant side tables.
pub mod bigint;
/// Engine/runtime integration boundary.
pub mod bridge;
/// Builder-side adapters for descriptor-driven bootstrap.
pub mod builders;
/// Runtime bytecode model.
pub mod bytecode;
/// Call-site side tables for direct calls.
pub mod call;
/// Closure creation metadata and upvalue identifiers.
pub mod closure;
/// Console API with pluggable backend.
pub mod console;
/// Deoptimization metadata and handoff types.
pub mod deopt;
/// Descriptor layer shared by proc-macros and future builders.
pub mod descriptors;
/// Default tokio-powered event loop and timer registry.
pub mod event_loop;
/// Event loop host trait for async runtime abstraction.
pub mod event_loop_host;
/// Exception table metadata.
pub mod exception;
/// Feedback and profiling side-table layout.
pub mod feedback;
/// Float-constant side tables.
pub mod float;
/// Frame and register-window layout.
pub mod frame;
/// Runtime host-function registry for native callbacks.
pub mod host;
/// Cross-thread host completion queue drained on the VM thread.
pub mod host_callbacks;
/// Bytecode interpreter entry points.
pub mod interpreter;
/// Runtime-owned intrinsic registry and root model.
pub mod intrinsics;
/// JIT-facing ABI surface.
pub mod jit_abi;
/// Tiny lowering bridge from structured subset to bytecode/module form.
pub mod lowering;
/// VM-internal microtask queue (promise jobs, nextTick, queueMicrotask).
pub mod microtask;
/// Executable module and function containers.
pub mod module;
/// §16.2.1 — Module loading, linking, and evaluation.
pub mod module_loader;
/// Minimal object heap for the new VM.
pub mod object;
/// Native payload storage and tracing contracts for JS-visible host objects.
pub mod payload;
/// Promise intrinsic — ES2024 §27.2 implementation.
pub mod promise;
/// Property side tables for named access.
pub mod property;
/// Shared CopyDataProperties helpers for object spread/rest semantics.
mod property_copy;
/// RegExp-literal side tables for functions.
pub mod regexp;
/// Small smoke harness for iterative validation.
pub mod smoke;
/// Tiny JS source lowering for the first new-VM migration slice.
pub mod source;
/// Primary JS source compiler for the new VM source path.
pub(crate) mod source_compiler;
/// Source-location metadata.
pub mod source_map;
/// String-literal side tables for functions.
pub mod string;
/// Minimal register value representation.
pub mod value;

pub use abi::VmAbiVersion;
pub use builders::{
    ClassAccessorPlan, ClassBuilder, ClassBuilderError, ClassInstallPlan, ClassMemberPlan,
    ConstructorBuilder, GlobalBuilder, NamespaceBuilder, ObjectAccessorPlan, ObjectBuilderError,
    ObjectInstallPlan, ObjectMemberPlan, PrototypeBuilder,
};
pub use descriptors::{
    ClassDescriptorConsumer, JsClassDescriptor, JsNamespaceDescriptor, NamespaceDescriptorConsumer,
    NativeBindingDescriptor, NativeBindingTarget, NativeDescriptorConsumer, NativeEntrypointKind,
    NativeFunctionDescriptor, NativeSlotKind, VmNativeCallError, VmNativeFunction,
};
pub use frame::FrameLayout;
pub use host::{HostFunctionId, NativeFunctionRegistry};
pub use host_callbacks::HostCallbackSender;
pub use interpreter::{Interpreter, RuntimeState};
pub use intrinsics::{IntrinsicRoot, IntrinsicsStage, VmIntrinsics, WellKnownSymbol};
pub use module::{
    ExportRecord, Function, FunctionIndex, ImportBinding, ImportRecord, Module, ModuleError,
};
pub use object::{ObjectShapeId, PropertyInlineCache};
pub use payload::{
    NativePayloadError, NativePayloadId, NativePayloadRegistry, VmNativePayload, VmTrace,
    VmValueTracer,
};
pub use promise::VmPromise;
pub use value::RegisterValue;

/// Returns the current execution ABI version of the new VM.
#[must_use]
pub const fn abi_version() -> VmAbiVersion {
    VmAbiVersion::V1
}
