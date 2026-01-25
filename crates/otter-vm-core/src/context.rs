//! VM execution context
//!
//! The context holds per-execution state: registers, call stack, locals.

use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::async_context::SavedFrame;
use crate::error::{VmError, VmResult};
use crate::object::JsObject;
use crate::string::JsString;
use crate::value::{UpvalueCell, Value};

/// Maximum call stack depth
const MAX_STACK_DEPTH: usize = 1000;

/// Maximum number of registers per function
const MAX_REGISTERS: usize = 65536;

/// A call stack frame
#[derive(Debug)]
pub struct CallFrame {
    /// Function index in the module
    pub function_index: u32,
    /// The module this function belongs to
    pub module: std::sync::Arc<otter_vm_bytecode::Module>,
    /// Program counter (instruction index)
    pub pc: usize,
    /// Base register index
    pub register_base: usize,
    /// Local variables
    pub locals: Vec<Value>,
    /// Captured upvalues (heap-allocated cells for shared mutable access)
    pub upvalues: Vec<UpvalueCell>,
    /// Return register (where to put the result)
    pub return_register: Option<u16>,
    /// Source location for error reporting
    pub source_location: Option<SourceLocation>,
    /// The `this` value for this call frame
    pub this_value: Value,
    /// Whether this call is a `new`/constructor invocation
    pub is_construct: bool,
    /// Whether this is an async function (return value should be wrapped in Promise)
    pub is_async: bool,
    /// Unique frame ID for tracking open upvalues
    pub frame_id: usize,
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
    /// Try/catch handler stack (catch pc + frame depth)
    try_stack: Vec<TryHandler>,
    /// Is context running
    running: bool,
    /// Pending arguments for next call
    pending_args: Vec<Value>,
    /// Pending `this` value for next call
    pending_this: Option<Value>,
    /// Pending upvalues for next call (captured closure cells)
    pending_upvalues: Vec<UpvalueCell>,
    /// Open upvalues: maps (frame_id, local_idx) to the cell.
    /// When a closure captures a local, we create/reuse a cell here.
    /// Multiple closures in the same frame share the same cell.
    open_upvalues: HashMap<(usize, u16), UpvalueCell>,
    /// Next frame ID counter (monotonically increasing)
    next_frame_id: usize,
    /// Interrupt flag for timeout/cancellation support
    interrupt_flag: Arc<AtomicBool>,
}

#[derive(Debug, Clone)]
struct TryHandler {
    catch_pc: usize,
    frame_depth: usize,
}

impl VmContext {
    /// Create a new context with a global object
    pub fn new(global: Arc<JsObject>) -> Self {
        Self {
            registers: vec![Value::undefined(); MAX_REGISTERS],
            call_stack: Vec::with_capacity(64),
            global,
            exception: None,
            try_stack: Vec::new(),
            running: false,
            pending_args: Vec::new(),
            pending_this: None,
            pending_upvalues: Vec::new(),
            open_upvalues: HashMap::new(),
            next_frame_id: 0,
            interrupt_flag: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Get the interrupt flag for external timeout/cancellation
    ///
    /// Call `flag.store(true, Ordering::Relaxed)` to interrupt execution.
    /// The VM will check this flag periodically and return an error if set.
    pub fn interrupt_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.interrupt_flag)
    }

    /// Set a custom interrupt flag (for sharing across contexts)
    pub fn set_interrupt_flag(&mut self, flag: Arc<AtomicBool>) {
        self.interrupt_flag = flag;
    }

    /// Check if execution was interrupted
    #[inline]
    pub fn is_interrupted(&self) -> bool {
        self.interrupt_flag.load(Ordering::Relaxed)
    }

    /// Request interruption of execution
    pub fn interrupt(&self) {
        self.interrupt_flag.store(true, Ordering::Relaxed);
    }

    /// Clear the interrupt flag
    pub fn clear_interrupt(&self) {
        self.interrupt_flag.store(false, Ordering::Relaxed);
    }

    /// Get a register value
    #[inline]
    pub fn get_register(&self, index: u16) -> &Value {
        let frame = self.current_frame().expect("no call frame");
        &self.registers[frame.register_base + index as usize]
    }

    /// Set a register value
    #[inline]
    pub fn set_register(&mut self, index: u16, value: Value) {
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
    /// If this local has been captured by a closure, also update the shared cell
    #[inline]
    pub fn set_local(&mut self, index: u16, value: Value) -> VmResult<()> {
        let frame = self
            .current_frame_mut()
            .ok_or_else(|| VmError::internal("no call frame"))?;
        if (index as usize) < frame.locals.len() {
            frame.locals[index as usize] = value.clone();
            // If this local has been captured, update the cell too
            let frame_id = frame.frame_id;
            if let Some(cell) = self.open_upvalues.get(&(frame_id, index)) {
                cell.set(value);
            }
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

    /// Push a try handler for the current frame.
    pub fn push_try(&mut self, catch_pc: usize) {
        self.try_stack.push(TryHandler {
            catch_pc,
            frame_depth: self.call_stack.len(),
        });
    }

    /// Pop the most recently pushed try handler.
    pub fn pop_try(&mut self) {
        self.try_stack.pop();
    }

    /// Pop the most recent try handler if it belongs to the current frame.
    pub fn pop_try_for_current_frame(&mut self) {
        if let Some(top) = self.try_stack.last()
            && top.frame_depth == self.call_stack.len()
        {
            self.try_stack.pop();
        }
    }

    /// Pop and return the nearest try handler.
    pub fn take_nearest_try(&mut self) -> Option<(usize, usize)> {
        let handler = self.try_stack.pop()?;
        Some((handler.frame_depth, handler.catch_pc))
    }

    /// Get global variable
    pub fn get_global(&self, name: &str) -> Option<Value> {
        use crate::object::PropertyKey;
        self.global.get(&PropertyKey::string(name))
    }

    /// Get global variable by UTF-16 code units
    pub fn get_global_utf16(&self, units: &[u16]) -> Option<Value> {
        use crate::object::PropertyKey;
        let key = PropertyKey::from_js_string(JsString::intern_utf16(units));
        self.global.get(&key)
    }

    /// Set global variable
    pub fn set_global(&self, name: &str, value: Value) {
        use crate::object::PropertyKey;
        self.global.set(PropertyKey::string(name), value);
    }

    /// Set global variable by UTF-16 code units
    pub fn set_global_utf16(&self, units: &[u16], value: Value) {
        use crate::object::PropertyKey;
        let key = PropertyKey::from_js_string(JsString::intern_utf16(units));
        self.global.set(key, value);
    }

    /// Push a new call frame
    pub fn push_frame(
        &mut self,
        function_index: u32,
        module: std::sync::Arc<otter_vm_bytecode::Module>,
        local_count: u16,
        return_register: Option<u16>,
        is_construct: bool,
        is_async: bool,
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

        // Take pending this value (defaults to undefined)
        let this_value = self.take_pending_this();

        // Take pending upvalues (captured closure cells)
        let upvalues = self.take_pending_upvalues();

        // Assign a unique frame ID
        let frame_id = self.next_frame_id;
        self.next_frame_id += 1;

        self.call_stack.push(CallFrame {
            function_index,
            module,
            pc: 0,
            register_base,
            locals,
            upvalues,
            return_register,
            source_location: None,
            this_value,
            is_construct,
            is_async,
            frame_id,
        });

        Ok(())
    }

    /// Pop the current call frame
    pub fn pop_frame(&mut self) -> Option<CallFrame> {
        if let Some(frame) = self.call_stack.last() {
            // Clean up open upvalues for this frame
            // (cells are already synced via set_local updates)
            let frame_id = frame.frame_id;
            self.open_upvalues.retain(|(fid, _), _| *fid != frame_id);
        }
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

    /// Take and clear exception value
    pub fn take_exception(&mut self) -> Option<Value> {
        self.exception.take()
    }

    /// Set pending arguments for next function call
    pub fn set_pending_args(&mut self, args: Vec<Value>) {
        self.pending_args = args;
    }

    /// Take pending arguments (transfers ownership)
    pub fn take_pending_args(&mut self) -> Vec<Value> {
        std::mem::take(&mut self.pending_args)
    }

    /// Set pending `this` value for next function call
    pub fn set_pending_this(&mut self, this_value: Value) {
        self.pending_this = Some(this_value);
    }

    /// Take pending `this` value (defaults to undefined)
    pub fn take_pending_this(&mut self) -> Value {
        self.pending_this.take().unwrap_or_else(Value::undefined)
    }

    /// Set pending upvalues for next function call (captured closure cells)
    pub fn set_pending_upvalues(&mut self, upvalues: Vec<UpvalueCell>) {
        self.pending_upvalues = upvalues;
    }

    /// Take pending upvalues (transfers ownership)
    pub fn take_pending_upvalues(&mut self) -> Vec<UpvalueCell> {
        std::mem::take(&mut self.pending_upvalues)
    }

    /// Get an upvalue value from the current call frame
    #[inline]
    pub fn get_upvalue(&self, index: u16) -> VmResult<Value> {
        let frame = self
            .current_frame()
            .ok_or_else(|| VmError::internal("no call frame"))?;
        let cell = frame
            .upvalues
            .get(index as usize)
            .ok_or_else(|| VmError::internal(format!("upvalue index {} out of bounds", index)))?;
        Ok(cell.get())
    }

    /// Get an upvalue cell from the current call frame (for capturing)
    #[inline]
    pub fn get_upvalue_cell(&self, index: u16) -> VmResult<&UpvalueCell> {
        let frame = self
            .current_frame()
            .ok_or_else(|| VmError::internal("no call frame"))?;
        frame
            .upvalues
            .get(index as usize)
            .ok_or_else(|| VmError::internal(format!("upvalue index {} out of bounds", index)))
    }

    /// Set an upvalue in the current call frame
    #[inline]
    pub fn set_upvalue(&mut self, index: u16, value: Value) -> VmResult<()> {
        let frame = self
            .current_frame()
            .ok_or_else(|| VmError::internal("no call frame"))?;
        let cell = frame
            .upvalues
            .get(index as usize)
            .ok_or_else(|| VmError::internal(format!("upvalue index {} out of bounds", index)))?;
        cell.set(value);
        Ok(())
    }

    /// Get or create an open upvalue cell for a local variable in the current frame.
    /// If the cell already exists, return the existing one (shared between closures).
    pub fn get_or_create_open_upvalue(&mut self, local_idx: u16) -> VmResult<UpvalueCell> {
        let frame = self
            .current_frame()
            .ok_or_else(|| VmError::internal("no call frame"))?;
        let frame_id = frame.frame_id;
        let key = (frame_id, local_idx);

        if let Some(cell) = self.open_upvalues.get(&key) {
            return Ok(cell.clone());
        }

        // Create a new cell with the current local value
        let value = self.get_local(local_idx)?.clone();
        let cell = UpvalueCell::new(value);
        self.open_upvalues.insert(key, cell.clone());
        Ok(cell)
    }

    /// Close an upvalue: sync the local variable's current value to the cell
    /// and remove from open upvalues map. Called when exiting a scope where
    /// the local was captured.
    pub fn close_upvalue(&mut self, local_idx: u16) -> VmResult<()> {
        let frame = self
            .current_frame()
            .ok_or_else(|| VmError::internal("no call frame"))?;
        let frame_id = frame.frame_id;
        let key = (frame_id, local_idx);

        if let Some(cell) = self.open_upvalues.get(&key) {
            // Sync the current local value into the cell
            let value = self.get_local(local_idx)?.clone();
            cell.set(value);
        }
        // Remove from open upvalues (the closures keep their own clones of the cell)
        self.open_upvalues.remove(&key);
        Ok(())
    }

    /// Clean up all open upvalues for a frame that's being popped
    pub fn close_all_upvalues_for_frame(&mut self, frame_id: usize) {
        self.open_upvalues.retain(|(fid, _), _| *fid != frame_id);
    }

    /// Get the `this` value of the current call frame
    pub fn this_value(&self) -> Value {
        self.current_frame()
            .map(|f| f.this_value.clone())
            .unwrap_or_else(Value::undefined)
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

    // ==================== Async Context Save/Restore ====================

    /// Save all call frames as SavedFrames for async suspension
    ///
    /// This captures the complete call stack state so we can restore it later.
    /// Includes both locals and registers for each frame.
    pub fn save_frames(&self) -> Vec<SavedFrame> {
        self.call_stack
            .iter()
            .map(|frame| {
                // Extract registers for this frame (256 registers per frame)
                let reg_start = frame.register_base;
                let reg_end = (reg_start + MAX_REGISTERS).min(self.registers.len());
                let frame_registers = self.registers[reg_start..reg_end].to_vec();

                SavedFrame::new(
                    frame.function_index,
                    Arc::clone(&frame.module),
                    frame.pc,
                    frame.locals.clone(),
                    frame_registers,
                    frame.upvalues.clone(),
                    frame.return_register,
                    frame.this_value.clone(),
                    frame.is_construct,
                    frame.is_async,
                    frame.frame_id,
                )
            })
            .collect()
    }

    /// Restore call frames from SavedFrames after async resumption
    ///
    /// This replaces the current call stack with the saved state.
    /// Restores both locals and registers for each frame.
    pub fn restore_frames(&mut self, saved_frames: Vec<SavedFrame>) -> VmResult<()> {
        // Clear current call stack
        self.call_stack.clear();

        // Ensure we have enough registers for all frames
        let max_registers_needed = saved_frames.len() * MAX_REGISTERS;
        if max_registers_needed > self.registers.len() {
            self.registers
                .resize(max_registers_needed, Value::undefined());
        }

        // Restore each frame
        for (i, saved) in saved_frames.into_iter().enumerate() {
            let register_base = i * MAX_REGISTERS;

            // Restore registers for this frame
            for (j, reg) in saved.registers.into_iter().enumerate() {
                if register_base + j < self.registers.len() {
                    self.registers[register_base + j] = reg;
                }
            }

            self.call_stack.push(CallFrame {
                function_index: saved.function_index,
                module: saved.module,
                pc: saved.pc,
                register_base,
                locals: saved.locals,
                upvalues: saved.upvalues,
                return_register: saved.return_register,
                source_location: None,
                this_value: saved.this_value,
                is_construct: saved.is_construct,
                is_async: saved.is_async,
                frame_id: saved.frame_id,
            });

            // Update next_frame_id to be greater than any restored frame
            if saved.frame_id >= self.next_frame_id {
                self.next_frame_id = saved.frame_id + 1;
            }
        }

        Ok(())
    }

    /// Get mutable access to the call stack (for advanced manipulation)
    pub fn call_stack_mut(&mut self) -> &mut Vec<CallFrame> {
        &mut self.call_stack
    }

    /// Get the call stack (for inspection)
    pub fn call_stack(&self) -> &[CallFrame] {
        &self.call_stack
    }

    pub fn registers_to_trace(&self) -> &[Value] {
        &self.registers
    }

    pub fn pending_args_to_trace(&self) -> &[Value] {
        &self.pending_args
    }

    pub fn pending_this_to_trace(&self) -> Option<&Value> {
        self.pending_this.as_ref()
    }

    pub fn pending_upvalues_to_trace(&self) -> &[UpvalueCell] {
        &self.pending_upvalues
    }

    pub fn open_upvalues_to_trace(&self) -> &HashMap<(usize, u16), UpvalueCell> {
        &self.open_upvalues
    }

    /// Teardown the context and break reference cycles
    pub fn teardown(&mut self) {
        // Break the globalThis cycle
        use crate::object::PropertyKey;
        self.global
            .set(PropertyKey::string("globalThis"), Value::undefined());

        // Also clear other properties of global to help break other cycles
        // Since we don't have a cycle-collecting GC, this is a best-effort approach
        // to free as much memory as possible.
        let keys = self.global.own_keys();
        for key in keys {
            // Check if it's configurable before trying to delete/clear?
            // For now just try to set to undefined to release references
            self.global.set(key, Value::undefined());
        }
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
    use otter_vm_bytecode::Module;

    fn dummy_module() -> Arc<Module> {
        Arc::new(Module::builder("test.js").build())
    }

    #[test]
    fn test_context_registers() {
        let global = Arc::new(JsObject::new(None));
        let mut ctx = VmContext::new(global);

        ctx.push_frame(0, dummy_module(), 0, None, false, false)
            .unwrap();
        ctx.set_register(0, Value::int32(42));

        assert_eq!(ctx.get_register(0).as_int32(), Some(42));
    }

    #[test]
    fn test_context_locals() {
        let global = Arc::new(JsObject::new(None));
        let mut ctx = VmContext::new(global);

        ctx.push_frame(0, dummy_module(), 3, None, false, false)
            .unwrap();
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
        let module = dummy_module();

        // Push frames until overflow
        for i in 0..MAX_STACK_DEPTH {
            ctx.push_frame(i as u32, Arc::clone(&module), 0, None, false, false)
                .unwrap();
        }

        // Next push should fail
        let result = ctx.push_frame(0, module, 0, None, false, false);
        assert!(matches!(result, Err(VmError::StackOverflow)));
    }

    #[test]
    fn test_program_counter() {
        let global = Arc::new(JsObject::new(None));
        let mut ctx = VmContext::new(global);

        ctx.push_frame(0, dummy_module(), 0, None, false, false)
            .unwrap();
        assert_eq!(ctx.pc(), 0);

        ctx.advance_pc();
        assert_eq!(ctx.pc(), 1);

        ctx.jump(5);
        assert_eq!(ctx.pc(), 6);

        ctx.jump(-3);
        assert_eq!(ctx.pc(), 3);
    }
}
