// Allow raw pointer dereference in public functions - this is an FFI wrapper
// where the caller is responsible for providing valid JSContextRef pointers.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

//! Safe wrappers for JavaScriptCore.
//!
//! This crate provides memory-safe, RAII-based wrappers around
//! the raw JSC FFI bindings in jsc-sys.
//!
//! # Example
//!
//! ```
//! use otter_jsc_core::JscContext;
//!
//! let ctx = JscContext::new().unwrap();
//! let result = ctx.eval("1 + 1").unwrap();
//! assert_eq!(result.to_number().unwrap(), 2.0);
//! ```
//!
//! # Thread Safety
//!
//! All types in this crate are `!Send` and `!Sync` because JavaScriptCore
//! contexts and values are not thread-safe. Attempting to use them from
//! multiple threads causes undefined behavior.
//!
//! For multi-threaded usage, use `otter-runtime`'s `EngineHandle` which
//! provides a thread-safe API by marshaling operations to dedicated
//! runtime threads.
//!
//! ## Example: Wrong (won't compile)
//!
//! ```compile_fail
//! use otter_jsc_core::JscContext;
//! use std::thread;
//!
//! let ctx = JscContext::new().unwrap();
//! thread::spawn(move || {
//!     ctx.eval("1 + 1"); // Error: JscContext is !Send
//! });
//! ```
//!
//! ## Example: Correct
//!
//! ```ignore
//! use otter_runtime::EngineHandle;
//!
//! let handle = engine.handle(); // EngineHandle is Send + Sync
//! tokio::spawn(async move {
//!     handle.eval("1 + 1").await; // OK: marshaled to runtime thread
//! });
//! ```

mod context;
mod error;
mod object;
pub mod string;
mod value;

pub use context::JscContext;
pub use error::{JscError, JscResult};
pub use object::JscObject;
pub use string::{JscString, js_string_to_rust};
pub use value::{JscValue, extract_exception};

// Re-export jsc-sys for direct FFI access when needed
pub use otter_jsc_sys;
