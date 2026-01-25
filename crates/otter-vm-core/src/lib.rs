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

pub mod async_context;
pub mod context;
pub mod drop_guard;
pub mod error;
pub mod gc;
pub mod generator;
pub mod globals;
pub mod interpreter;
pub mod object;
pub mod promise;
pub mod proxy;
pub mod regexp;
pub mod runtime;
pub mod shape;
pub mod shared_buffer;
pub mod string;
pub mod structured_clone;
pub mod value;

pub use async_context::{AsyncContext, SavedFrame, VmExecutionResult};
pub use context::{
    VmContext, DEFAULT_MAX_NATIVE_DEPTH, DEFAULT_MAX_STACK_DEPTH, INTERRUPT_CHECK_INTERVAL,
};
pub use error::{VmError, VmResult};
pub use generator::{GeneratorContext, GeneratorState, IteratorResult, JsGenerator};
pub use interpreter::Interpreter;
pub use object::{JsObject, PropertyKey};
pub use promise::{JsPromise, PromiseState, PromiseWithResolvers};
pub use proxy::{JsProxy, RevocableProxy};
pub use runtime::VmRuntime;
pub use shape::Shape;
pub use shared_buffer::SharedArrayBuffer;
pub use string::JsString;
pub use structured_clone::{StructuredCloneError, StructuredCloner, structured_clone};
pub use value::{NativeFn, Symbol, Value};
