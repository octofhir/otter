//! Async context for suspending and resuming async functions
//!
//! When an async function encounters an `await` on a pending Promise,
//! the VM must suspend execution and later resume when the Promise settles.
//! This module provides types for capturing and restoring VM state.

use crate::gc::GcRef;
use crate::promise::JsPromise;
use crate::value::{UpvalueCell, Value};

/// Captured state for async function suspension
///
/// When an async function awaits a pending Promise, we capture the entire
/// call stack state so we can resume execution later.
///
/// The registers are stored as a flat Vec moved from VmContext (zero-copy).
/// Each SavedFrame stores its register_base/register_count as offsets into
/// this flat array.
#[derive(Debug)]
pub struct AsyncContext {
    /// Saved call stack frames (from bottom to top)
    pub frames: Vec<SavedFrame>,
    /// Flat register array moved from VmContext (locals + scratch for all frames)
    pub registers: Vec<Value>,
    /// The result promise for this async function
    /// This is what the caller awaits on
    pub result_promise: GcRef<JsPromise>,
    /// The promise we're currently awaiting
    pub awaited_promise: GcRef<JsPromise>,
    /// Register where the await result should be stored
    pub resume_register: u16,
    /// Whether the VM was running before suspension
    pub was_running: bool,
}

impl AsyncContext {
    /// Create a new async context
    pub fn new(
        frames: Vec<SavedFrame>,
        registers: Vec<Value>,
        result_promise: GcRef<JsPromise>,
        awaited_promise: GcRef<JsPromise>,
        resume_register: u16,
        was_running: bool,
    ) -> Self {
        Self {
            frames,
            registers,
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
/// the VM state after async suspension. Register data lives in the
/// flat `AsyncContext::registers` array; this struct stores offsets.
#[derive(Debug, Clone)]
pub struct SavedFrame {
    /// Function index in the module
    pub function_index: u32,
    /// Module id for O(1) lookup in VmContext::module_table
    pub module_id: u64,
    /// Realm id for this frame
    pub realm_id: crate::realm::RealmId,
    /// Program counter (instruction index)
    pub pc: usize,
    /// Number of local variable slots at the start of the register window
    pub local_count: u16,
    /// Base index in the flat register array
    pub register_base: usize,
    /// Total window size (local_count + scratch registers)
    pub register_count: u16,
    /// Captured upvalues (heap-allocated cells)
    pub upvalues: Vec<UpvalueCell>,
    /// Return register (where to put the result)
    pub return_register: Option<u16>,
    /// The `this` value for this call frame
    pub this_value: Value,
    /// Whether this call is a `new`/constructor invocation
    pub is_construct: bool,
    /// Whether this is an async function
    pub is_async: bool,
    /// Unique frame ID for tracking
    pub frame_id: u32,
    /// Number of arguments passed to this function
    pub argc: u16,
    /// Offset from `register_base` to spilled extra arguments.
    pub extra_args_offset: u16,
    /// Number of spilled extra arguments.
    pub extra_args_count: u16,
}

/// Result of VM execution that can indicate suspension
#[derive(Debug)]
pub enum VmExecutionResult {
    /// Execution completed normally with a value
    Complete(Value),
    /// Execution suspended waiting for a Promise
    Suspended(AsyncContext),
    /// Execution failed with an error
    Error(crate::VmError),
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
