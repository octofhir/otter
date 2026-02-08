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

pub mod array_buffer;
pub mod async_context;
pub mod builtin_builder;
pub mod context;
pub mod data_view;
pub mod drop_guard;
pub mod error;
pub mod format;
pub mod gc;
pub mod generator;
pub mod globals;
pub mod interpreter;
pub mod intrinsics_impl;
pub mod intrinsics;
pub mod map_data;
pub mod memory;
pub mod object;
/// Thread-confined interior mutability for VM objects.
pub mod object_cell;
pub mod promise;
pub mod proxy;
pub mod proxy_operations;
pub mod regexp;
pub mod realm;
pub mod runtime;
pub mod shape;
pub mod shared_buffer;
pub mod string;
pub mod structured_clone;
pub mod symbol_registry;
pub mod trace;
pub mod typed_array;
pub mod value;

pub use async_context::{AsyncContext, SavedFrame, VmExecutionResult};
pub use builtin_builder::{BuiltInBuilder, NamespaceBuilder};
pub use intrinsics::Intrinsics;
pub use context::{
    DEFAULT_MAX_NATIVE_DEPTH, DEFAULT_MAX_STACK_DEPTH, INTERRUPT_CHECK_INTERVAL, VmContext,
    VmContextSnapshot,
};
pub use error::{VmError, VmResult};
pub use gc::GcRef;
pub use generator::{
    CompletionType, GeneratorFrame, GeneratorState, IteratorResult, JsGenerator, TryEntry,
};
pub use interpreter::{GeneratorResult, Interpreter};
pub use memory::MemoryManager;
pub use object::{JsObject, PropertyKey, SetPropertyError};
pub use promise::{JsPromise, PromiseState, PromiseWithResolvers};
pub use proxy::{JsProxy, RevocableProxy};
pub use runtime::VmRuntime;
pub use shape::Shape;
pub use array_buffer::JsArrayBuffer;
pub use data_view::JsDataView;
pub use shared_buffer::SharedArrayBuffer;
pub use typed_array::{JsTypedArray, TypedArrayKind};
pub use string::{JsString, clear_global_string_table, global_string_table_size};
pub use structured_clone::{StructuredCloneError, StructuredCloner, structured_clone};
pub use trace::{TraceConfig, TraceEntry, TraceMode, TraceRingBuffer, TraceState};
pub use value::{NativeFn, Symbol, Value};
