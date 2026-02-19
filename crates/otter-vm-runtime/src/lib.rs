//! # Otter VM Runtime
//!
//! High-level runtime for the Otter VM, providing:
//! - Event loop for async operations
//! - Extension system for native functions
//! - Builder API for configuration
//! - Capabilities for permission checking
//! - Environment store for secure env var access

#![warn(clippy::all)]
#![warn(missing_docs)]

pub mod builder;
pub mod capabilities;
pub mod capabilities_context;
pub mod env_store;
pub mod event_loop;
pub mod extension;
pub mod extension_state;
pub mod extension_v2;
pub mod microtask;
pub mod module_loader;
pub mod module_provider;
pub mod otter_runtime;
pub mod promise;
pub mod registration;
pub mod timer;
pub mod timer_root_set;
pub mod timers_ext;
pub mod worker;
pub mod worker_ext;

// Re-export main types
pub use builder::OtterBuilder;
pub use event_loop::{ActiveServerCount, EventLoop, HttpEvent, WsEvent};
pub use extension::{
    Extension, ExtensionRegistry, NativeOpResult, Op, OpHandler, op_async, op_native,
    op_native_with_mm, op_sync,
};
pub use microtask::{MicrotaskQueue, NextTickQueue};
pub use module_loader::{
    LoadedModule, ModuleError, ModuleLoader, ModuleNamespace, ModuleState, module_extension,
};
pub use module_provider::{
    MediaType, ModuleProvider, ModuleResolution, ModuleSource, ModuleType, ProviderRegistry,
};
pub use otter_runtime::{Otter, OtterError};
pub use otter_vm_core::isolate::IsolateConfig;

// Native extension system
pub use extension_state::ExtensionState;
pub use extension_v2::{NativeExtensionRegistry, OtterExtension, Profile};
pub use registration::{ModuleNamespaceBuilder, RegistrationContext};

// Legacy alias for backwards compatibility
#[deprecated(since = "0.2.0", note = "Renamed to Otter")]
pub type OtterRuntime = Otter;
pub use promise::Promise;
pub use timer::{Immediate, ImmediateId, Timer, TimerCallback, TimerHeapEntry, TimerId};
pub use worker::{Worker, WorkerContext, WorkerError, WorkerMessage, WorkerPool};
pub use worker_ext::{WorkerExtension, worker_extension};

// Re-export capabilities and env store
pub use capabilities::{Capabilities, CapabilitiesBuilder, PermissionDenied};
pub use capabilities_context::CapabilitiesGuard;
pub use env_store::{
    DEFAULT_DENY_PATTERNS, EnvFileError, EnvStoreBuilder, EnvWriteError, IsolatedEnvStore,
    parse_env_file,
};
