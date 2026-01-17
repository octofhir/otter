// Allow unsafe operations inside unsafe functions without nested unsafe blocks.
// This is the Rust 2021 behavior - in 2024 edition this is stricter but for FFI
// code wrapping every call is overly verbose.
#![allow(unsafe_op_in_unsafe_fn)]
// Allow raw pointer dereference in public functions - this is an FFI-heavy crate
// where the caller is responsible for providing valid pointers.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

//! otter-runtime - JavaScriptCore runtime for Otter.
//!
//! This crate provides a safe Rust wrapper around JavaScriptCore for executing
//! JavaScript code with a multi-threaded runtime pool.
//!
//! # Features
//!
//! - **ES2020+ Support**: Full modern JavaScript syntax support (Safari-tested)
//! - **JIT Compilation**: Fast execution with JSC's multi-tier JIT compiler
//! - **Thread-safe Pool**: Multiple JSC contexts for concurrent execution
//! - **Native APIs**: Rust implementations of `console.*`, `http.fetch`
//! - **GC Protection**: Automatic garbage collection management
//!
//! # Example
//!
//! ```no_run
//! use otter_runtime::{JscRuntimePool, JscConfig};
//!
//! let config = JscConfig {
//!     pool_size: 4,
//!     timeout_ms: 5000,
//!     enable_console: true,
//! };
//!
//! let pool = JscRuntimePool::new(config).unwrap();
//!
//! let result = pool.eval("2 + 2").unwrap();
//! assert_eq!(result.to_number().unwrap(), 4.0);
//! ```
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────┐
//! │                    JscRuntimePool                            │
//! │  ┌─────────────┐ ┌─────────────┐ ┌─────────────┐           │
//! │  │ JscRuntime  │ │ JscRuntime  │ │ JscRuntime  │  ...      │
//! │  │  (Mutex)    │ │  (Mutex)    │ │  (Mutex)    │           │
//! │  └─────────────┘ └─────────────┘ └─────────────┘           │
//! │         │               │               │                   │
//! │         └───────────────┼───────────────┘                   │
//! │                         ↓                                   │
//! │              Round-robin selection                          │
//! └─────────────────────────────────────────────────────────────┘
//!                           ↓
//! ┌─────────────────────────────────────────────────────────────┐
//! │                    JscContext                                │
//! │  - Script evaluation                                         │
//! │  - Object creation                                           │
//! │  - Native function registration                              │
//! └─────────────────────────────────────────────────────────────┘
//!                           ↓
//! ┌─────────────────────────────────────────────────────────────┐
//! │                    JscValue                                  │
//! │  - GC-protected JavaScript values                            │
//! │  - Type conversion (to Rust types)                           │
//! │  - JSON serialization/deserialization                        │
//! └─────────────────────────────────────────────────────────────┘
//! ```

pub mod apis;
pub mod bindings;
pub mod bootstrap;
pub mod commonjs;
pub mod config;
pub mod context;
pub mod den;
pub mod engine;
pub mod error;
pub mod event_loop;
pub mod extension;
pub mod holt;
pub mod memory;
pub mod modules;
pub mod runtime;
pub mod transpiler;
pub mod tsconfig;
pub mod tsgo;
pub mod types;
pub mod value;
mod worker;

// Re-export worker types for event-driven dispatch
pub use worker::{HttpEvent, NetEvent};

pub use apis::console::{ConsoleLevel, set_console_handler};
pub use apis::{NetPermissionChecker, clear_net_permission_checker, set_net_permission_checker};
pub use apis::{register_all_apis, register_apis_with_config};
pub use bootstrap::register_bootstrap;
pub use config::TypeScriptConfig;
pub use context::JscContext;
pub use engine::{Engine, EngineBuilder, EngineHandle, EngineStats, EngineStatsSnapshot};
pub use error::{JscError, JscResult};
pub use extension::{
    Extension, ExtensionState, OpContext, OpDecl, OpHandler, OpResult, RuntimeContextHandle,
    op_async, op_sync, set_tokio_handle,
};
pub use holt::{Holt, HoltError, HoltResult, Paw};
pub use memory::{JscHeapStats, jsc_heap_stats};
pub use commonjs::{register_commonjs_runtime, require_module, wrap_commonjs_module};
pub use modules::{
    ModuleFormat, ModuleInfo, bundle_modules, bundle_modules_mixed, entry_execution,
    entry_execution_mixed, transform_module, wrap_module,
};
pub use runtime::{JscConfig, JscRuntime, JscRuntimePool, PromiseDriver};
pub use transpiler::{
    TranspileError, TranspileOptions, TranspileResult, is_typescript, needs_transpilation,
    transpile_typescript, transpile_typescript_with_options,
};
pub use tsconfig::{
    CompilerOptions, TsConfigJson, find_tsconfig, load_tsconfig_for_dir,
    load_typescript_config_for_dir,
};
pub use tsgo::{
    Diagnostic as TsgoDiagnostic, DiagnosticSeverity as TsgoDiagnosticSeverity,
    TypeCheckConfig as TsgoTypeCheckConfig, TypeChecker as TsgoTypeChecker,
    check_file as tsgo_check_file, check_project as tsgo_check_project,
    check_types as tsgo_check_types, format_diagnostics as tsgo_format_diagnostics,
    has_errors as tsgo_has_errors,
};
pub use types::{EMBEDDED_TYPES, get_embedded_type, list_embedded_types};
pub use value::JscValue;

pub mod prelude {
    pub use crate::apis::console::{ConsoleLevel, set_console_handler};
    pub use crate::apis::{
        NetPermissionChecker, clear_net_permission_checker, set_net_permission_checker,
    };
    pub use crate::apis::{register_all_apis, register_apis_with_config};
    pub use crate::bootstrap::register_bootstrap;
    pub use crate::config::TypeScriptConfig;
    pub use crate::context::JscContext;
    pub use crate::engine::{
        Engine, EngineBuilder, EngineHandle, EngineStats, EngineStatsSnapshot,
    };
    pub use crate::worker::HttpEvent;
    pub use crate::error::{JscError, JscResult};
    pub use crate::extension::{
        Extension, ExtensionState, OpContext, OpDecl, OpHandler, OpResult, op_async, op_sync,
        set_tokio_handle,
    };
    pub use crate::holt::{Holt, HoltError, HoltResult, Paw};
    pub use crate::commonjs::{register_commonjs_runtime, require_module, wrap_commonjs_module};
    pub use crate::modules::{
        ModuleFormat, ModuleInfo, bundle_modules, bundle_modules_mixed, entry_execution,
        entry_execution_mixed, transform_module, wrap_module,
    };
    pub use crate::runtime::{JscConfig, JscRuntime, JscRuntimePool, PromiseDriver};
    pub use crate::transpiler::{
        TranspileError, TranspileOptions, TranspileResult, is_typescript, needs_transpilation,
        transpile_typescript, transpile_typescript_with_options,
    };
    pub use crate::tsconfig::{
        CompilerOptions, TsConfigJson, find_tsconfig, load_tsconfig_for_dir,
        load_typescript_config_for_dir,
    };
    pub use crate::tsgo::{
        Diagnostic as TsgoDiagnostic, DiagnosticSeverity as TsgoDiagnosticSeverity,
        TypeCheckConfig as TsgoTypeCheckConfig, TypeChecker as TsgoTypeChecker,
        check_file as tsgo_check_file, check_project as tsgo_check_project,
        check_types as tsgo_check_types, format_diagnostics as tsgo_format_diagnostics,
        has_errors as tsgo_has_errors,
    };
    pub use crate::types::{EMBEDDED_TYPES, get_embedded_type, list_embedded_types};
    pub use crate::value::JscValue;
}
