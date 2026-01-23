//! # Otter VM Runtime
//!
//! Event loop and async runtime for the VM.

#![warn(clippy::all)]
#![warn(missing_docs)]

pub mod event_loop;
pub mod extension;
pub mod microtask;
pub mod module_loader;
pub mod promise;
pub mod timer;
pub mod worker;

pub use event_loop::EventLoop;
pub use extension::{
    Extension, ExtensionRegistry, NativeOpResult, Op, OpHandler, op_async, op_native, op_sync,
};
pub use module_loader::{
    LoadedModule, ModuleError, ModuleLoader, ModuleNamespace, ModuleState, ModuleType,
    module_extension,
};
pub use promise::Promise;
pub use timer::{Immediate, ImmediateId, Timer, TimerCallback, TimerHeapEntry, TimerId};
pub use worker::{Worker, WorkerContext, WorkerError, WorkerMessage, WorkerPool};
