//! # Otter VM Core
//!
//! Core execution engine for the Otter JavaScript/TypeScript runtime.
//!
//! ## Design Principles
//!
//! - **Thread-safe**: Values are `Send + Sync` for multi-threaded execution
//! - **NaN-boxing**: Efficient 64-bit value representation
//! - **Hidden classes**: V8-style property access optimization
//! - **Register-based**: Matches the bytecode instruction format

#![warn(clippy::all)]
#![warn(missing_docs)]
// Allow unsafe for NaN-boxing and GC operations
// All unsafe code must have SAFETY comments

pub mod context;
pub mod error;
pub mod gc;
pub mod globals;
pub mod interpreter;
pub mod object;
pub mod promise;
pub mod runtime;
pub mod shared_buffer;
pub mod string;
pub mod structured_clone;
pub mod value;

pub use context::VmContext;
pub use error::{VmError, VmResult};
pub use interpreter::Interpreter;
pub use object::{JsObject, PropertyKey};
pub use promise::JsPromise;
pub use runtime::VmRuntime;
pub use shared_buffer::SharedArrayBuffer;
pub use string::JsString;
pub use structured_clone::{StructuredCloneError, StructuredCloner, structured_clone};
pub use value::{NativeFn, Value};
