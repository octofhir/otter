//! New VM backend for Otter.
//!
//! This crate is the fresh execution backend that will replace the current VM
//! architecture incrementally. It starts as a small scaffold with a strict
//! module split and a minimal public API.

#![deny(clippy::all)]
#![forbid(unsafe_code)]

/// Shared execution ABI.
pub mod abi;
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
/// Deoptimization metadata and handoff types.
pub mod deopt;
/// Descriptor layer shared by proc-macros and future builders.
pub mod descriptors;
/// Exception table metadata.
pub mod exception;
/// Feedback and profiling side-table layout.
pub mod feedback;
/// Frame and register-window layout.
pub mod frame;
/// Runtime host-function registry for native callbacks.
pub mod host;
/// Bytecode interpreter entry points.
pub mod interpreter;
/// Runtime-owned intrinsic registry and root model.
pub mod intrinsics;
/// JIT-facing ABI surface.
pub mod jit_abi;
/// Tiny lowering bridge from structured subset to bytecode/module form.
pub mod lowering;
/// Executable module and function containers.
pub mod module;
/// Minimal object heap for the new VM.
pub mod object;
/// Property side tables for named access.
pub mod property;
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
    ClassDescriptorConsumer, JsClassDescriptor, NativeBindingDescriptor, NativeBindingTarget,
    NativeDescriptorConsumer, NativeEntrypointKind, NativeFunctionDescriptor, NativeSlotKind,
    VmNativeCallError, VmNativeFunction,
};
pub use frame::FrameLayout;
pub use host::{HostFunctionId, NativeFunctionRegistry};
pub use interpreter::{Interpreter, RuntimeState};
pub use intrinsics::{IntrinsicRoot, IntrinsicsStage, VmIntrinsics, WellKnownSymbol};
pub use module::{Function, FunctionIndex, Module, ModuleError};
pub use object::{ObjectShapeId, PropertyInlineCache};
pub use value::RegisterValue;

/// Returns the current execution ABI version of the new VM.
#[must_use]
pub const fn abi_version() -> VmAbiVersion {
    VmAbiVersion::V1
}
