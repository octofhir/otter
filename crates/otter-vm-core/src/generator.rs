//! JavaScript Generator implementation
//!
//! Generators are functions that can be paused and resumed, yielding values.
//!
//! ## Usage
//!
//! ```ignore
//! function* gen() {
//!     yield 1;
//!     yield 2;
//!     return 3;
//! }
//! const g = gen();
//! g.next(); // { value: 1, done: false }
//! g.next(); // { value: 2, done: false }
//! g.next(); // { value: 3, done: true }
//! ```

use crate::value::Value;
use parking_lot::Mutex;
use std::sync::Arc;

/// Generator execution state
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GeneratorState {
    /// Generator has been created but not started
    Suspended,
    /// Generator is currently executing
    Executing,
    /// Generator has completed (returned or thrown)
    Completed,
}

/// Saved execution context for generator suspension
#[derive(Debug, Clone)]
pub struct GeneratorContext {
    /// Program counter (instruction offset)
    pub pc: usize,
    /// Local variables
    pub locals: Vec<Value>,
    /// Register values
    pub registers: Vec<Value>,
}

impl GeneratorContext {
    /// Create a new empty context
    pub fn new() -> Self {
        Self {
            pc: 0,
            locals: Vec::new(),
            registers: Vec::new(),
        }
    }
}

impl Default for GeneratorContext {
    fn default() -> Self {
        Self::new()
    }
}

/// A JavaScript Generator object
///
/// Generators maintain their execution state across yields.
pub struct JsGenerator {
    /// Function index in the module
    pub function_index: u32,
    /// Captured upvalues (closure variables)
    pub upvalues: Vec<Value>,
    /// Current state
    state: Mutex<GeneratorState>,
    /// Saved execution context for resumption
    context: Mutex<GeneratorContext>,
    /// The most recent value sent to the generator via next(value)
    sent_value: Mutex<Option<Value>>,
}

impl std::fmt::Debug for JsGenerator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Generator")
            .field("function_index", &self.function_index)
            .field("state", &*self.state.lock())
            .finish()
    }
}

impl JsGenerator {
    /// Create a new generator
    pub fn new(function_index: u32, upvalues: Vec<Value>) -> Arc<Self> {
        Arc::new(Self {
            function_index,
            upvalues,
            state: Mutex::new(GeneratorState::Suspended),
            context: Mutex::new(GeneratorContext::new()),
            sent_value: Mutex::new(None),
        })
    }

    /// Get the current state
    pub fn state(&self) -> GeneratorState {
        *self.state.lock()
    }

    /// Check if generator is suspended
    pub fn is_suspended(&self) -> bool {
        *self.state.lock() == GeneratorState::Suspended
    }

    /// Check if generator is executing
    pub fn is_executing(&self) -> bool {
        *self.state.lock() == GeneratorState::Executing
    }

    /// Check if generator is completed
    pub fn is_completed(&self) -> bool {
        *self.state.lock() == GeneratorState::Completed
    }

    /// Set state to executing
    pub fn start_executing(&self) {
        *self.state.lock() = GeneratorState::Executing;
    }

    /// Suspend the generator with saved context
    pub fn suspend(&self, pc: usize, locals: Vec<Value>, registers: Vec<Value>) {
        *self.state.lock() = GeneratorState::Suspended;
        *self.context.lock() = GeneratorContext {
            pc,
            locals,
            registers,
        };
    }

    /// Complete the generator
    pub fn complete(&self) {
        *self.state.lock() = GeneratorState::Completed;
    }

    /// Get the saved context
    pub fn get_context(&self) -> GeneratorContext {
        self.context.lock().clone()
    }

    /// Set the value to be sent to the generator on next resume
    pub fn set_sent_value(&self, value: Value) {
        *self.sent_value.lock() = Some(value);
    }

    /// Take the sent value (returns None if not set)
    pub fn take_sent_value(&self) -> Option<Value> {
        self.sent_value.lock().take()
    }
}

/// Result of calling generator.next()
#[derive(Debug, Clone)]
pub struct IteratorResult {
    /// The yielded/returned value
    pub value: Value,
    /// Whether the generator is done
    pub done: bool,
}

impl IteratorResult {
    /// Create a new iterator result
    pub fn new(value: Value, done: bool) -> Self {
        Self { value, done }
    }

    /// Create a "not done" result
    pub fn yielded(value: Value) -> Self {
        Self { value, done: false }
    }

    /// Create a "done" result
    pub fn done(value: Value) -> Self {
        Self { value, done: true }
    }

    /// Create a "done with undefined" result
    pub fn done_undefined() -> Self {
        Self {
            value: Value::undefined(),
            done: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generator_creation() {
        let generator = JsGenerator::new(0, vec![]);
        assert!(generator.is_suspended());
        assert!(!generator.is_executing());
        assert!(!generator.is_completed());
    }

    #[test]
    fn test_generator_state_transitions() {
        let generator = JsGenerator::new(0, vec![]);

        // Start executing
        generator.start_executing();
        assert!(generator.is_executing());

        // Suspend
        generator.suspend(10, vec![], vec![]);
        assert!(generator.is_suspended());
        let ctx = generator.get_context();
        assert_eq!(ctx.pc, 10);

        // Complete
        generator.complete();
        assert!(generator.is_completed());
    }

    #[test]
    fn test_sent_value() {
        let generator = JsGenerator::new(0, vec![]);

        // No value initially
        assert!(generator.take_sent_value().is_none());

        // Set and take value
        generator.set_sent_value(Value::number(42.0));
        let val = generator.take_sent_value();
        assert!(val.is_some());
        assert_eq!(val.unwrap().as_number(), Some(42.0));

        // Value is consumed
        assert!(generator.take_sent_value().is_none());
    }

    #[test]
    fn test_iterator_result() {
        let yielded = IteratorResult::yielded(Value::number(1.0));
        assert!(!yielded.done);
        assert_eq!(yielded.value.as_number(), Some(1.0));

        let done = IteratorResult::done(Value::number(2.0));
        assert!(done.done);
        assert_eq!(done.value.as_number(), Some(2.0));

        let done_undef = IteratorResult::done_undefined();
        assert!(done_undef.done);
        assert!(done_undef.value.is_undefined());
    }
}
