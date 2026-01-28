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

use crate::value::{UpvalueCell, Value};
use otter_vm_bytecode::Module;
use parking_lot::Mutex;
use std::sync::Arc;

/// Generator execution state
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GeneratorState {
    /// Generator has been created but not started (before first next())
    SuspendedStart,
    /// Generator has yielded and is waiting to be resumed
    SuspendedYield,
    /// Generator is currently executing
    Executing,
    /// Generator has completed (returned or thrown)
    Completed,
}

/// Try handler entry for exception handling
#[derive(Debug, Clone)]
pub struct TryEntry {
    /// PC to jump to on catch
    pub catch_pc: usize,
    /// Frame depth when try was entered
    pub frame_depth: usize,
}

/// Generator completion type for return/throw semantics
#[derive(Debug, Clone)]
pub enum CompletionType {
    /// Normal completion (return or continue)
    Normal,
    /// Return completion with value
    Return(Value),
    /// Throw completion with error value
    Throw(Value),
}

// Using manual Default impl as CompletionType::Normal doesn't carry data
// and derive(Default) would require all variants to be unit variants
impl Default for CompletionType {
    #[inline]
    fn default() -> Self {
        Self::Normal
    }
}

/// Complete saved execution context for generator suspension
///
/// This captures all state needed to resume a generator from where it was suspended.
#[derive(Debug, Clone)]
pub struct GeneratorFrame {
    /// Program counter (instruction offset)
    pub pc: usize,
    /// Function index in the module
    pub function_index: u32,
    /// The module this function belongs to
    pub module: Arc<Module>,
    /// Local variables
    pub locals: Vec<Value>,
    /// Register values
    pub registers: Vec<Value>,
    /// Captured upvalues (closure variables)
    pub upvalues: Vec<UpvalueCell>,
    /// Try/catch handler stack
    pub try_stack: Vec<TryEntry>,
    /// The `this` value for this generator
    pub this_value: Value,
    /// Whether this is a constructor call (new.target)
    pub is_construct: bool,
    /// Unique frame ID for upvalue tracking
    pub frame_id: usize,
    /// Number of arguments passed to this function
    pub argc: usize,
    /// Value sent via next(value) - to be received after yield
    pub received_value: Option<Value>,
    /// Pending throw value (for generator.throw())
    pub pending_throw: Option<Value>,
    /// Completion type (for generator.return())
    pub completion_type: CompletionType,
    /// Destination register for the yield expression result (sent value goes here on resume)
    pub yield_dst: Option<u16>,
}

impl GeneratorFrame {
    /// Create a new generator frame with all execution state
    pub fn new(
        pc: usize,
        function_index: u32,
        module: Arc<Module>,
        locals: Vec<Value>,
        registers: Vec<Value>,
        upvalues: Vec<UpvalueCell>,
        try_stack: Vec<TryEntry>,
        this_value: Value,
        is_construct: bool,
        frame_id: usize,
        argc: usize,
    ) -> Self {
        Self {
            pc,
            function_index,
            module,
            locals,
            registers,
            upvalues,
            try_stack,
            this_value,
            is_construct,
            frame_id,
            argc,
            received_value: None,
            pending_throw: None,
            completion_type: CompletionType::Normal,
            yield_dst: None,
        }
    }

    /// Create a new generator frame with yield destination register
    pub fn with_yield_dst(
        pc: usize,
        function_index: u32,
        module: Arc<Module>,
        locals: Vec<Value>,
        registers: Vec<Value>,
        upvalues: Vec<UpvalueCell>,
        try_stack: Vec<TryEntry>,
        this_value: Value,
        is_construct: bool,
        frame_id: usize,
        argc: usize,
        yield_dst: u16,
    ) -> Self {
        Self {
            pc,
            function_index,
            module,
            locals,
            registers,
            upvalues,
            try_stack,
            this_value,
            is_construct,
            frame_id,
            argc,
            received_value: None,
            pending_throw: None,
            completion_type: CompletionType::Normal,
            yield_dst: Some(yield_dst),
        }
    }

    /// Create an initial frame for a generator that hasn't started yet
    pub fn initial(
        function_index: u32,
        module: Arc<Module>,
        locals: Vec<Value>,
        upvalues: Vec<UpvalueCell>,
        this_value: Value,
        is_construct: bool,
        frame_id: usize,
        argc: usize,
    ) -> Self {
        Self {
            pc: 0,
            function_index,
            module,
            locals,
            registers: Vec::new(),
            upvalues,
            try_stack: Vec::new(),
            this_value,
            is_construct,
            frame_id,
            argc,
            received_value: None,
            pending_throw: None,
            completion_type: CompletionType::Normal,
            yield_dst: None,
        }
    }
}

/// Legacy type alias for backward compatibility
pub type GeneratorContext = GeneratorFrame;

impl Default for GeneratorFrame {
    fn default() -> Self {
        Self {
            pc: 0,
            function_index: 0,
            module: Arc::new(Module::builder("").build()),
            locals: Vec::new(),
            registers: Vec::new(),
            upvalues: Vec::new(),
            try_stack: Vec::new(),
            this_value: Value::undefined(),
            is_construct: false,
            frame_id: 0,
            argc: 0,
            received_value: None,
            pending_throw: None,
            completion_type: CompletionType::Normal,
            yield_dst: None,
        }
    }
}

use crate::gc::GcRef;
/// A JavaScript Generator object
///
/// Generators maintain their execution state across yields.
/// The generator captures a complete frame snapshot on each yield,
/// allowing full state restoration on resume.
use crate::object::JsObject;

pub struct JsGenerator {
    /// Associated JavaScript object (for properties and prototype)
    pub object: GcRef<JsObject>,
    /// Function index in the module
    pub function_index: u32,
    /// The module containing the generator function
    pub module: Arc<Module>,
    /// Captured upvalues (closure variables)
    pub upvalues: Vec<UpvalueCell>,
    /// Current state
    pub(crate) state: Mutex<GeneratorState>,
    /// Saved execution frame for resumption (complete state snapshot)
    pub(crate) frame: Mutex<Option<GeneratorFrame>>,
    /// Initial arguments passed when the generator was created
    pub(crate) initial_args: Mutex<Vec<Value>>,
    /// Initial `this` value
    pub(crate) initial_this: Mutex<Value>,
    /// Whether this is a constructor invocation
    pub is_construct: bool,
    /// Whether this is an async generator
    pub is_async: bool,
    /// Pending return value (persists independently of frame, for generator.return())
    pub(crate) abrupt_return: Mutex<Option<Value>>,
    /// Pending throw value (persists independently of frame, for generator.throw())
    pub(crate) abrupt_throw: Mutex<Option<Value>>,
}

impl std::fmt::Debug for JsGenerator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Generator")
            .field("function_index", &self.function_index)
            .field("state", &*self.state.lock())
            .field("has_frame", &self.frame.lock().is_some())
            .finish()
    }
}

impl JsGenerator {
    /// Create a new generator in suspended-start state
    ///
    /// The generator hasn't been started yet and will begin execution
    /// on the first call to next().
    pub fn new(
        function_index: u32,
        module: Arc<Module>,
        upvalues: Vec<UpvalueCell>,
        args: Vec<Value>,
        this_value: Value,
        is_construct: bool,
        is_async: bool,
        object: GcRef<JsObject>,
    ) -> Arc<Self> {
        Arc::new(Self {
            object,
            function_index,
            module,
            upvalues,
            state: Mutex::new(GeneratorState::SuspendedStart),
            frame: Mutex::new(None),
            initial_args: Mutex::new(args),
            initial_this: Mutex::new(this_value),
            is_construct,
            is_async,
            abrupt_return: Mutex::new(None),
            abrupt_throw: Mutex::new(None),
        })
    }

    /// Create a simple generator (for backward compatibility)
    pub fn new_simple(
        function_index: u32,
        upvalues: Vec<UpvalueCell>,
        object: GcRef<JsObject>,
    ) -> Arc<Self> {
        Arc::new(Self {
            object,
            function_index,
            module: Arc::new(Module::builder("").build()),
            upvalues,
            state: Mutex::new(GeneratorState::SuspendedStart),
            frame: Mutex::new(None),
            initial_args: Mutex::new(Vec::new()),
            initial_this: Mutex::new(Value::undefined()),
            is_construct: false,
            is_async: false,
            abrupt_return: Mutex::new(None),
            abrupt_throw: Mutex::new(None),
        })
    }

    /// Get the current state
    pub fn state(&self) -> GeneratorState {
        *self.state.lock()
    }

    /// Check if generator is suspended (either start or yield)
    pub fn is_suspended(&self) -> bool {
        matches!(
            *self.state.lock(),
            GeneratorState::SuspendedStart | GeneratorState::SuspendedYield
        )
    }

    /// Check if generator is in suspended-start state (not yet started)
    pub fn is_suspended_start(&self) -> bool {
        *self.state.lock() == GeneratorState::SuspendedStart
    }

    /// Check if generator is in suspended-yield state (has yielded)
    pub fn is_suspended_yield(&self) -> bool {
        *self.state.lock() == GeneratorState::SuspendedYield
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

    /// Suspend the generator with a complete frame snapshot
    ///
    /// This captures all execution state needed to resume the generator.
    pub fn suspend_with_frame(&self, frame: GeneratorFrame) {
        *self.state.lock() = GeneratorState::SuspendedYield;
        *self.frame.lock() = Some(frame);
    }

    /// Legacy suspend method for backward compatibility
    pub fn suspend(&self, pc: usize, locals: Vec<Value>, registers: Vec<Value>) {
        let frame = GeneratorFrame {
            pc,
            function_index: self.function_index,
            module: Arc::clone(&self.module),
            locals,
            registers,
            upvalues: self.upvalues.clone(),
            try_stack: Vec::new(),
            this_value: Value::undefined(),
            is_construct: false,
            frame_id: 0,
            argc: 0,
            received_value: None,
            pending_throw: None,
            completion_type: CompletionType::Normal,
            yield_dst: None,
        };
        self.suspend_with_frame(frame);
    }

    /// Complete the generator
    pub fn complete(&self) {
        *self.state.lock() = GeneratorState::Completed;
        *self.frame.lock() = None; // Clear saved frame
    }

    /// Get the saved frame (if any)
    pub fn get_frame(&self) -> Option<GeneratorFrame> {
        self.frame.lock().clone()
    }

    /// Take the saved frame (returns None if not set)
    pub fn take_frame(&self) -> Option<GeneratorFrame> {
        self.frame.lock().take()
    }

    /// Get the saved context (legacy compatibility)
    pub fn get_context(&self) -> GeneratorContext {
        self.frame.lock().clone().unwrap_or_default()
    }

    /// Set the value to be sent to the generator on next resume
    pub fn set_sent_value(&self, value: Value) {
        if let Some(frame) = self.frame.lock().as_mut() {
            frame.received_value = Some(value);
        }
    }

    /// Take the sent value (returns None if not set)
    pub fn take_sent_value(&self) -> Option<Value> {
        if let Some(frame) = self.frame.lock().as_mut() {
            frame.received_value.take()
        } else {
            None
        }
    }

    /// Set a pending throw value (for generator.throw())
    pub fn set_pending_throw(&self, error: Value) {
        if let Some(frame) = self.frame.lock().as_mut() {
            frame.pending_throw = Some(error);
        } else {
            *self.abrupt_throw.lock() = Some(error);
        }
    }

    /// Take pending throw value
    pub fn take_pending_throw(&self) -> Option<Value> {
        if let Some(frame) = self.frame.lock().as_mut() {
            if let Some(error) = frame.pending_throw.take() {
                return Some(error);
            }
        }
        self.abrupt_throw.lock().take()
    }

    /// Set completion type (for generator.return())
    pub fn set_completion_type(&self, completion: CompletionType) {
        if let Some(frame) = self.frame.lock().as_mut() {
            frame.completion_type = completion;
        }
    }

    /// Get completion type
    pub fn completion_type(&self) -> CompletionType {
        self.frame
            .lock()
            .as_ref()
            .map(|f| f.completion_type.clone())
            .unwrap_or_default()
    }

    /// Take initial arguments (only valid for suspended-start state)
    pub fn take_initial_args(&self) -> Vec<Value> {
        std::mem::take(&mut *self.initial_args.lock())
    }

    /// Take initial this value (only valid for suspended-start state)
    pub fn take_initial_this(&self) -> Value {
        std::mem::take(&mut *self.initial_this.lock())
    }

    /// Check if this is a constructor invocation
    pub fn is_construct(&self) -> bool {
        self.is_construct
    }

    /// Check if this is an async generator
    pub fn is_async(&self) -> bool {
        self.is_async
    }

    /// Check if the generator has active try handlers (for finally block handling)
    ///
    /// If the generator has try handlers, calling `.return()` needs to resume
    /// execution to allow finally blocks to run.
    pub fn has_try_handlers(&self) -> bool {
        self.frame
            .lock()
            .as_ref()
            .map(|f| !f.try_stack.is_empty())
            .unwrap_or(false)
    }

    /// Set pending return value (for generator.return() with finally blocks)
    /// This persists independently of the frame for abrupt returns.
    pub fn set_pending_return(&self, value: Value) {
        *self.abrupt_return.lock() = Some(value);
    }

    /// Take pending return value if set
    pub fn take_pending_return(&self) -> Option<Value> {
        self.abrupt_return.lock().take()
    }

    /// Check if there's a pending return value
    pub fn has_pending_return(&self) -> bool {
        self.abrupt_return.lock().is_some()
    }

    /// Get pending return value without taking it (peek)
    pub fn get_pending_return(&self) -> Option<Value> {
        self.abrupt_return.lock().clone()
    }

    /// Get the yield destination register (if any)
    pub fn get_yield_dst(&self) -> Option<u16> {
        self.frame.lock().as_ref().and_then(|f| f.yield_dst)
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
        let module = Arc::new(Module::builder("test").build());
        let mm = Arc::new(crate::memory::MemoryManager::test());
        let obj = GcRef::new(JsObject::new(None, mm.clone()));
        let generator = JsGenerator::new(
            0,
            module,
            vec![],
            vec![],
            Value::undefined(),
            false,
            false,
            obj,
        );
        assert!(generator.is_suspended());
        assert!(generator.is_suspended_start());
        assert!(!generator.is_executing());
        assert!(!generator.is_completed());
    }

    #[test]
    fn test_generator_state_transitions() {
        let module = Arc::new(Module::builder("test").build());
        let mm = Arc::new(crate::memory::MemoryManager::test());
        let obj = GcRef::new(JsObject::new(None, mm));
        let generator = JsGenerator::new(
            0,
            Arc::clone(&module),
            vec![],
            vec![],
            Value::undefined(),
            false,
            false,
            obj,
        );

        // Start executing
        generator.start_executing();
        assert!(generator.is_executing());

        // Suspend with full frame
        let frame = GeneratorFrame::new(
            10, // pc
            0,  // function_index
            module,
            vec![],             // locals
            vec![],             // registers
            vec![],             // upvalues
            vec![],             // try_stack
            Value::undefined(), // this_value
            false,              // is_construct
            0,                  // frame_id
            0,                  // argc
        );
        generator.suspend_with_frame(frame);
        assert!(generator.is_suspended());
        assert!(generator.is_suspended_yield());
        let ctx = generator.get_context();
        assert_eq!(ctx.pc, 10);

        // Complete
        generator.complete();
        assert!(generator.is_completed());
    }

    #[test]
    fn test_sent_value() {
        let module = Arc::new(Module::builder("test").build());
        let mm = Arc::new(crate::memory::MemoryManager::test());
        let obj = GcRef::new(JsObject::new(None, mm));
        let generator = JsGenerator::new(
            0,
            Arc::clone(&module),
            vec![],
            vec![],
            Value::undefined(),
            false,
            false,
            obj,
        );

        // No value initially (no frame yet)
        assert!(generator.take_sent_value().is_none());

        let frame = GeneratorFrame::new(
            0,
            0,
            module,
            vec![],
            vec![],
            vec![],
            vec![],
            Value::undefined(),
            false,
            0,
            0,
        );
        generator.suspend_with_frame(frame);

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

    #[test]
    fn test_generator_frame() {
        let module = Arc::new(Module::builder("test").build());
        let frame = GeneratorFrame::new(
            42,
            1,
            Arc::clone(&module),
            vec![Value::int32(1), Value::int32(2)],
            vec![Value::int32(10)],
            vec![],
            vec![TryEntry {
                catch_pc: 100,
                frame_depth: 1,
            }],
            Value::int32(999),
            false,
            5,
            1,
        );

        assert_eq!(frame.pc, 42);
        assert_eq!(frame.function_index, 1);
        assert_eq!(frame.locals.len(), 2);
        assert_eq!(frame.registers.len(), 1);
        assert_eq!(frame.try_stack.len(), 1);
        assert_eq!(frame.try_stack[0].catch_pc, 100);
        assert_eq!(frame.this_value.as_int32(), Some(999));
        assert_eq!(frame.frame_id, 5);
    }

    #[test]
    fn test_completion_type() {
        let module = Arc::new(Module::builder("test").build());
        let mm = Arc::new(crate::memory::MemoryManager::test());
        let obj = GcRef::new(JsObject::new(None, mm));
        let generator = JsGenerator::new(
            0,
            Arc::clone(&module),
            vec![],
            vec![],
            Value::undefined(),
            false,
            false,
            obj,
        );

        // Create frame and test completion types
        let frame = GeneratorFrame::new(
            0,
            0,
            module,
            vec![],
            vec![],
            vec![],
            vec![],
            Value::undefined(),
            false,
            0,
            0,
        );
        generator.suspend_with_frame(frame);

        // Default is Normal
        assert!(matches!(
            generator.completion_type(),
            CompletionType::Normal
        ));

        // Set to Return
        generator.set_completion_type(CompletionType::Return(Value::int32(42)));
        if let CompletionType::Return(v) = generator.completion_type() {
            assert_eq!(v.as_int32(), Some(42));
        } else {
            panic!("Expected Return completion");
        }

        // Set to Throw
        generator.set_completion_type(CompletionType::Throw(Value::int32(500)));
        if let CompletionType::Throw(v) = generator.completion_type() {
            assert_eq!(v.as_int32(), Some(500));
        } else {
            panic!("Expected Throw completion");
        }
    }
}
