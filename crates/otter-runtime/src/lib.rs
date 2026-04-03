//! Public API for the Otter JavaScript runtime.
//!
//! This crate is the primary entry point for users and embedders.
//! It provides [`OtterRuntime`] with a builder pattern for configuration,
//! and handles the full execution lifecycle: compile → execute → microtask
//! drain → event loop.
//!
//! # Quick Start
//!
//! ```rust,no_run
//! use otter_runtime::OtterRuntime;
//!
//! let mut rt = OtterRuntime::builder().build();
//! rt.run_script("console.log('hello world')", "main.js").unwrap();
//! ```
//!
//! # Custom Console Backend
//!
//! ```rust,no_run
//! use otter_runtime::OtterRuntime;
//! use otter_runtime::console::StdioConsoleBackend;
//!
//! let mut rt = OtterRuntime::builder()
//!     .console(StdioConsoleBackend)
//!     .build();
//! ```
//!
//! # Architecture
//!
//! ```text
//! otter-runtime (this crate — public API)
//!     ↓
//! otter-vm (low-level: bytecode, interpreter, intrinsics, GC integration)
//!     ↓
//! otter-gc (page-based generational garbage collector)
//! ```

mod builder;
mod host;
mod runtime;

pub use builder::RuntimeBuilder;
pub use host::{
    Capabilities, CapabilitiesBuilder, DEFAULT_DENY_PATTERNS, EnvFileError, EnvStoreBuilder,
    EnvWriteError, HostConfig, HostedExtension, HostedExtensionModule, HostedExtensionRegistry,
    HostedNativeModule, HostedNativeModuleKind, HostedNativeModuleLoader,
    HostedNativeModuleRegistry, ImportContext, IsolatedEnvStore, ModuleDependency, ModuleGraph,
    ModuleGraphError, ModuleGraphNode, ModuleLoader, ModuleLoaderConfig, ModuleLoaderError,
    ModuleType, PermissionDenied, ResolvedModule, RuntimeProfile, SourceType, current_capabilities,
    parse_env_file,
};
pub use runtime::{OtterRuntime, RunError};

// Re-export commonly used types from otter-vm so users don't need to
// depend on otter-vm directly.
pub use otter_vm::console;
pub use otter_vm::descriptors::{NativeFunctionDescriptor, VmNativeCallError, VmNativeFunction};
pub use otter_vm::interpreter::{ExecutionResult, InterpreterError, RuntimeState};
pub use otter_vm::object::ObjectHandle;
pub use otter_vm::source;
pub use otter_vm::value::RegisterValue;
