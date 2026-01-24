//! Async context for suspending and resuming async functions
//!
//! When an async function encounters an `await` on a pending Promise,
//! the VM must suspend execution and later resume when the Promise settles.
//! This module provides types for capturing and restoring VM state.

use std::sync::Arc;

use otter_vm_bytecode::Module;

use crate::promise::JsPromise;
use crate::value::{UpvalueCell, Value};

/// Captured state for async function suspension
///
/// When an async function awaits a pending Promise, we capture the entire
/// call stack state so we can resume execution later.
#[derive(Debug)]
pub struct AsyncContext {
    /// Saved call stack frames (from bottom to top)
    pub frames: Vec<SavedFrame>,
    /// The result promise for this async function
    /// This is what the caller awaits on
    pub result_promise: Arc<JsPromise>,
    /// The promise we're currently awaiting
    pub awaited_promise: Arc<JsPromise>,
    /// Register where the await result should be stored
    pub resume_register: u8,
    /// Whether the VM was running before suspension
    pub was_running: bool,
}

impl AsyncContext {
    /// Create a new async context
    pub fn new(
        frames: Vec<SavedFrame>,
        result_promise: Arc<JsPromise>,
        awaited_promise: Arc<JsPromise>,
        resume_register: u8,
        was_running: bool,
    ) -> Self {
        Self {
            frames,
            result_promise,
            awaited_promise,
            resume_register,
            was_running,
        }
    }
}

/// A saved call frame for later restoration
///
/// This is a snapshot of a `CallFrame` that can be used to restore
/// the VM state after async suspension.
#[derive(Debug, Clone)]
pub struct SavedFrame {
    /// Function index in the module
    pub function_index: u32,
    /// The module this function belongs to
    pub module: Arc<Module>,
    /// Program counter (instruction index)
    pub pc: usize,
    /// Local variables snapshot
    pub locals: Vec<Value>,
    /// Register values for this frame (256 registers max)
    pub registers: Vec<Value>,
    /// Captured upvalues (heap-allocated cells)
    pub upvalues: Vec<UpvalueCell>,
    /// Return register (where to put the result)
    pub return_register: Option<u8>,
    /// The `this` value for this call frame
    pub this_value: Value,
    /// Whether this call is a `new`/constructor invocation
    pub is_construct: bool,
    /// Whether this is an async function
    pub is_async: bool,
    /// Unique frame ID for tracking
    pub frame_id: usize,
}

impl SavedFrame {
    /// Create a new saved frame from the given parameters
    pub fn new(
        function_index: u32,
        module: Arc<Module>,
        pc: usize,
        locals: Vec<Value>,
        registers: Vec<Value>,
        upvalues: Vec<UpvalueCell>,
        return_register: Option<u8>,
        this_value: Value,
        is_construct: bool,
        is_async: bool,
        frame_id: usize,
    ) -> Self {
        Self {
            function_index,
            module,
            pc,
            locals,
            registers,
            upvalues,
            return_register,
            this_value,
            is_construct,
            is_async,
            frame_id,
        }
    }
}

/// Result of VM execution that can indicate suspension
#[derive(Debug)]
pub enum VmExecutionResult {
    /// Execution completed normally with a value
    Complete(Value),
    /// Execution suspended waiting for a Promise
    Suspended(AsyncContext),
    /// Execution failed with an error
    Error(String),
}

impl VmExecutionResult {
    /// Check if execution completed normally
    pub fn is_complete(&self) -> bool {
        matches!(self, Self::Complete(_))
    }

    /// Check if execution was suspended
    pub fn is_suspended(&self) -> bool {
        matches!(self, Self::Suspended(_))
    }

    /// Get the completed value if any
    pub fn value(self) -> Option<Value> {
        match self {
            Self::Complete(v) => Some(v),
            _ => None,
        }
    }

    /// Get the async context if suspended
    pub fn async_context(self) -> Option<AsyncContext> {
        match self {
            Self::Suspended(ctx) => Some(ctx),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_vm_execution_result() {
        let result = VmExecutionResult::Complete(Value::int32(42));
        assert!(result.is_complete());
        assert!(!result.is_suspended());

        let value = VmExecutionResult::Complete(Value::int32(42)).value();
        assert_eq!(value.unwrap().as_int32(), Some(42));
    }
}
