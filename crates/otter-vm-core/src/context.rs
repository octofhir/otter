//! VM execution context
//!
//! The context holds per-execution state: registers, call stack, locals.

use parking_lot::Mutex;
use std::sync::Arc;

use crate::error::{VmError, VmResult};
use crate::object::JsObject;
use crate::value::Value;

/// Maximum call stack depth
const MAX_STACK_DEPTH: usize = 10000;

/// Maximum number of registers per function
const MAX_REGISTERS: usize = 256;

/// A call stack frame
#[derive(Debug)]
pub struct CallFrame {
    /// Function index in the module
    pub function_index: u32,
    /// Program counter (instruction index)
    pub pc: usize,
    /// Base register index
    pub register_base: usize,
    /// Local variables
    pub locals: Vec<Value>,
    /// Return register (where to put the result)
    pub return_register: Option<u8>,
    /// Source location for error reporting
    pub source_location: Option<SourceLocation>,
}

/// Source location for error reporting
#[derive(Debug, Clone)]
pub struct SourceLocation {
    /// File path
    pub file: String,
    /// Line number
    pub line: u32,
    /// Column number
    pub column: u32,
}

/// VM execution context
///
/// Holds execution state for a single "thread" of execution.
/// Note: This is not thread-safe internally, but the VmRuntime
/// coordinates access across threads.
pub struct VmContext {
    /// Virtual registers
    registers: Vec<Value>,
    /// Call stack
    call_stack: Vec<CallFrame>,
    /// Global object
    global: Arc<JsObject>,
    /// Exception state
    exception: Option<Value>,
    /// Is context running
    running: bool,
    /// Pending arguments for next call
    pending_args: Vec<Value>,
}

impl VmContext {
    /// Create a new context with a global object
    pub fn new(global: Arc<JsObject>) -> Self {
        Self {
            registers: vec![Value::undefined(); MAX_REGISTERS],
            call_stack: Vec::with_capacity(64),
            global,
            exception: None,
            running: false,
            pending_args: Vec::new(),
        }
    }

    /// Get a register value
    #[inline]
    pub fn get_register(&self, index: u8) -> &Value {
        let frame = self.current_frame().expect("no call frame");
        &self.registers[frame.register_base + index as usize]
    }

    /// Set a register value
    #[inline]
    pub fn set_register(&mut self, index: u8, value: Value) {
        let base = self.current_frame().expect("no call frame").register_base;
        self.registers[base + index as usize] = value;
    }

    /// Get a local variable
    #[inline]
    pub fn get_local(&self, index: u16) -> VmResult<&Value> {
        let frame = self
            .current_frame()
            .ok_or_else(|| VmError::internal("no call frame"))?;
        frame
            .locals
            .get(index as usize)
            .ok_or_else(|| VmError::internal(format!("local index {} out of bounds", index)))
    }

    /// Set a local variable
    #[inline]
    pub fn set_local(&mut self, index: u16, value: Value) -> VmResult<()> {
        let frame = self
            .current_frame_mut()
            .ok_or_else(|| VmError::internal("no call frame"))?;
        if (index as usize) < frame.locals.len() {
            frame.locals[index as usize] = value;
            Ok(())
        } else {
            Err(VmError::internal(format!(
                "local index {} out of bounds",
                index
            )))
        }
    }

    /// Get global object
    pub fn global(&self) -> &Arc<JsObject> {
        &self.global
    }

    /// Get global variable
    pub fn get_global(&self, name: &str) -> Option<Value> {
        use crate::object::PropertyKey;
        self.global.get(&PropertyKey::string(name))
    }

    /// Set global variable
    pub fn set_global(&self, name: &str, value: Value) {
        use crate::object::PropertyKey;
        self.global.set(PropertyKey::string(name), value);
    }

    /// Push a new call frame
    pub fn push_frame(
        &mut self,
        function_index: u32,
        local_count: u16,
        return_register: Option<u8>,
    ) -> VmResult<()> {
        if self.call_stack.len() >= MAX_STACK_DEPTH {
            return Err(VmError::StackOverflow);
        }

        let register_base = self
            .call_stack
            .last()
            .map(|f| f.register_base + MAX_REGISTERS)
            .unwrap_or(0);

        // Ensure we have enough registers
        let needed = register_base + MAX_REGISTERS;
        if needed > self.registers.len() {
            self.registers.resize(needed, Value::undefined());
        }

        // Take pending arguments and copy to locals
        let args = self.take_pending_args();
        let mut locals = vec![Value::undefined(); local_count as usize];
        for (i, arg) in args.into_iter().enumerate() {
            if i < locals.len() {
                locals[i] = arg;
            }
        }

        self.call_stack.push(CallFrame {
            function_index,
            pc: 0,
            register_base,
            locals,
            return_register,
            source_location: None,
        });

        Ok(())
    }

    /// Pop the current call frame
    pub fn pop_frame(&mut self) -> Option<CallFrame> {
        self.call_stack.pop()
    }

    /// Get current call frame
    #[inline]
    pub fn current_frame(&self) -> Option<&CallFrame> {
        self.call_stack.last()
    }

    /// Get current call frame mutably
    #[inline]
    pub fn current_frame_mut(&mut self) -> Option<&mut CallFrame> {
        self.call_stack.last_mut()
    }

    /// Get program counter
    #[inline]
    pub fn pc(&self) -> usize {
        self.current_frame().map(|f| f.pc).unwrap_or(0)
    }

    /// Set program counter
    #[inline]
    pub fn set_pc(&mut self, pc: usize) {
        if let Some(frame) = self.current_frame_mut() {
            frame.pc = pc;
        }
    }

    /// Increment program counter
    #[inline]
    pub fn advance_pc(&mut self) {
        if let Some(frame) = self.current_frame_mut() {
            frame.pc += 1;
        }
    }

    /// Jump relative to current PC
    #[inline]
    pub fn jump(&mut self, offset: i32) {
        if let Some(frame) = self.current_frame_mut() {
            frame.pc = (frame.pc as i64 + offset as i64) as usize;
        }
    }

    /// Get call stack depth
    pub fn stack_depth(&self) -> usize {
        self.call_stack.len()
    }

    /// Get exception if any
    pub fn exception(&self) -> Option<&Value> {
        self.exception.as_ref()
    }

    /// Set exception
    pub fn set_exception(&mut self, value: Value) {
        self.exception = Some(value);
    }

    /// Clear exception
    pub fn clear_exception(&mut self) {
        self.exception = None;
    }

    /// Set pending arguments for next function call
    pub fn set_pending_args(&mut self, args: Vec<Value>) {
        self.pending_args = args;
    }

    /// Take pending arguments (transfers ownership)
    pub fn take_pending_args(&mut self) -> Vec<Value> {
        std::mem::take(&mut self.pending_args)
    }

    /// Check if context is running
    pub fn is_running(&self) -> bool {
        self.running
    }

    /// Set running state
    pub fn set_running(&mut self, running: bool) {
        self.running = running;
    }

    /// Get stack trace for error reporting
    pub fn stack_trace(&self) -> Vec<SourceLocation> {
        self.call_stack
            .iter()
            .rev()
            .filter_map(|frame| frame.source_location.clone())
            .collect()
    }
}

impl std::fmt::Debug for VmContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VmContext")
            .field("stack_depth", &self.call_stack.len())
            .field("running", &self.running)
            .field("has_exception", &self.exception.is_some())
            .finish()
    }
}

/// A thread-safe wrapper for VmContext
pub struct SharedContext(Mutex<VmContext>);

impl SharedContext {
    /// Create a new shared context
    pub fn new(ctx: VmContext) -> Self {
        Self(Mutex::new(ctx))
    }

    /// Lock and access the context
    pub fn lock(&self) -> parking_lot::MutexGuard<'_, VmContext> {
        self.0.lock()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_context_registers() {
        let global = Arc::new(JsObject::new(None));
        let mut ctx = VmContext::new(global);

        ctx.push_frame(0, 0, None).unwrap();
        ctx.set_register(0, Value::int32(42));

        assert_eq!(ctx.get_register(0).as_int32(), Some(42));
    }

    #[test]
    fn test_context_locals() {
        let global = Arc::new(JsObject::new(None));
        let mut ctx = VmContext::new(global);

        ctx.push_frame(0, 3, None).unwrap();
        ctx.set_local(0, Value::int32(1)).unwrap();
        ctx.set_local(1, Value::int32(2)).unwrap();
        ctx.set_local(2, Value::int32(3)).unwrap();

        assert_eq!(ctx.get_local(0).unwrap().as_int32(), Some(1));
        assert_eq!(ctx.get_local(1).unwrap().as_int32(), Some(2));
        assert_eq!(ctx.get_local(2).unwrap().as_int32(), Some(3));
    }

    #[test]
    fn test_stack_overflow() {
        let global = Arc::new(JsObject::new(None));
        let mut ctx = VmContext::new(global);

        // Push frames until overflow
        for i in 0..MAX_STACK_DEPTH {
            ctx.push_frame(i as u32, 0, None).unwrap();
        }

        // Next push should fail
        let result = ctx.push_frame(0, 0, None);
        assert!(matches!(result, Err(VmError::StackOverflow)));
    }

    #[test]
    fn test_program_counter() {
        let global = Arc::new(JsObject::new(None));
        let mut ctx = VmContext::new(global);

        ctx.push_frame(0, 0, None).unwrap();
        assert_eq!(ctx.pc(), 0);

        ctx.advance_pc();
        assert_eq!(ctx.pc(), 1);

        ctx.jump(5);
        assert_eq!(ctx.pc(), 6);

        ctx.jump(-3);
        assert_eq!(ctx.pc(), 3);
    }
}
