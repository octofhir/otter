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
pub mod context;
pub mod error;
pub mod event_loop;
pub mod extension;
pub mod runtime;
pub mod transpiler;
pub mod value;

pub use apis::console::{ConsoleLevel, set_console_handler};
pub use apis::register_all_apis;
pub use context::JscContext;
pub use error::{JscError, JscResult};
pub use extension::{
    Extension, ExtensionState, OpContext, OpDecl, OpHandler, OpResult, op_async, op_sync,
};
pub use runtime::{JscConfig, JscRuntime, JscRuntimePool, PromiseDriver};
pub use transpiler::{
    TranspileError, TranspileOptions, TranspileResult, is_typescript, transpile_typescript,
    transpile_typescript_with_options,
};
pub use value::JscValue;

pub mod prelude {
    pub use crate::apis::console::{ConsoleLevel, set_console_handler};
    pub use crate::apis::register_all_apis;
    pub use crate::context::JscContext;
    pub use crate::error::{JscError, JscResult};
    pub use crate::extension::{
        Extension, ExtensionState, OpContext, OpDecl, OpHandler, OpResult, op_async, op_sync,
    };
    pub use crate::runtime::{JscConfig, JscRuntime, JscRuntimePool, PromiseDriver};
    pub use crate::transpiler::{
        TranspileError, TranspileOptions, TranspileResult, is_typescript, transpile_typescript,
        transpile_typescript_with_options,
    };
    pub use crate::value::JscValue;
}
