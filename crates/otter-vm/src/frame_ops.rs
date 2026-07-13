//! Frame-local opcode helpers.
//!
//! These opcodes only read the active frame and write registers. Keeping them
//! out of the fallback interpreter body helps `lib.rs` shrink while preserving
//! the dense executable operand path.
//!
//! # Contents
//! - `this` and `new.target` register loads.
//! - Upvalue load/store register operations.
//! - Rest/arguments materialisation.
//! - Try-handler stack maintenance.
//!
//! # Invariants
//! - Inputs are decoded from the executable instruction format before reaching
//!   these helpers.
//! - Helpers never mutate the call stack shape.
//!
//! # See also
//! - [`crate::Frame`]
//! - [`crate::executable`]

use crate::holt_stack::HoltStack;
use smallvec::SmallVec;

use crate::{
    Frame, Interpreter, TryHandler, Value, VmError, read_register, read_upvalue, store_upvalue,
    write_register,
};

impl Interpreter {
    pub(crate) fn run_load_new_target_reg(
        &self,
        frame: &mut Frame,
        dst: u16,
    ) -> Result<(), VmError> {
        let value = self
            .frame_cold(frame)
            .and_then(|c| c.new_target)
            .unwrap_or(Value::undefined());
        write_register(frame, dst, value)?;
        frame.advance_pc()?;
        Ok(())
    }

    pub(crate) fn run_load_upvalue_reg(
        &self,
        frame: &mut Frame,
        dst: u16,
        idx: i32,
    ) -> Result<(), VmError> {
        if idx < 0 {
            return Err(VmError::InvalidOperand);
        }
        let cell = *frame
            .upvalues
            .get(idx as usize)
            .ok_or(VmError::InvalidOperand)?;
        let value = read_upvalue(&self.gc_heap, cell);
        // §13.3.1 — a hole in an upvalue cell marks the Temporal Dead
        // Zone (`Op::FreshUpvalue` installs it for a per-iteration /
        // head-TDZ `let`). Reading it before the initializer's
        // `Op::StoreUpvalue` runs is a `ReferenceError`.
        if value.is_hole() {
            return Err(VmError::TemporalDeadZone {
                local_index: idx as u32,
            });
        }
        write_register(frame, dst, value)?;
        frame.advance_pc()?;
        Ok(())
    }

    /// `Op::FreshUpvalue idx` — install a freshly allocated hole cell at
    /// own-upvalue index `idx`. Closures created before this op keep the
    /// prior cell handle, so `for (let x of …)` materialises a distinct
    /// `x` per iteration and a head `let` spends RHS evaluation in the
    /// TDZ. The hole is cleared by the iteration's `Op::StoreUpvalue`.
    pub(crate) fn run_fresh_upvalue_reg(
        &mut self,
        frame: &mut Frame,
        idx: i32,
    ) -> Result<(), VmError> {
        if idx < 0 {
            return Err(VmError::InvalidOperand);
        }
        let fresh = crate::alloc_upvalue(&mut self.gc_heap, Value::hole())?;
        let slot = frame
            .upvalues
            .get_mut(idx as usize)
            .ok_or(VmError::InvalidOperand)?;
        *slot = fresh;
        frame.advance_pc()?;
        Ok(())
    }

    pub(crate) fn run_store_upvalue_reg(
        &mut self,
        frame: &mut Frame,
        src: u16,
        idx: i32,
    ) -> Result<(), VmError> {
        if idx < 0 {
            return Err(VmError::InvalidOperand);
        }
        let value = *read_register(frame, src)?;
        let cell = *frame
            .upvalues
            .get(idx as usize)
            .ok_or(VmError::InvalidOperand)?;
        store_upvalue(&mut self.gc_heap, cell, value);
        frame.advance_pc()?;
        Ok(())
    }

    /// `Op::StoreUpvalueChecked` — assignment (PutValue, §6.2.4.6) to a
    /// captured `let` / `const`. A cell still holding the Temporal Dead
    /// Zone hole means the write precedes the declaration's initializer,
    /// which is a `ReferenceError`. Binding initialization keeps using
    /// [`Self::run_store_upvalue_reg`], which clears the hole.
    pub(crate) fn run_store_upvalue_checked_reg(
        &mut self,
        frame: &mut Frame,
        src: u16,
        idx: i32,
    ) -> Result<(), VmError> {
        if idx < 0 {
            return Err(VmError::InvalidOperand);
        }
        let value = *read_register(frame, src)?;
        let cell = *frame
            .upvalues
            .get(idx as usize)
            .ok_or(VmError::InvalidOperand)?;
        if read_upvalue(&self.gc_heap, cell).is_hole() {
            return Err(VmError::TemporalDeadZone {
                local_index: idx as u32,
            });
        }
        store_upvalue(&mut self.gc_heap, cell, value);
        frame.advance_pc()?;
        Ok(())
    }

    /// JIT bridge for `LoadUpvalue` from compiled code, delegating to
    /// [`Self::run_load_upvalue_reg`] (captured-binding read with a TDZ-hole
    /// check). Frame PC is saved/restored so the helper's `advance_pc` does not
    /// disturb the compiled frame's program counter.
    ///
    /// # Errors
    /// Propagates `ReferenceError` for a TDZ-hole cell and `InvalidOperand`.
    pub fn jit_runtime_load_upvalue(
        &mut self,
        stack: &mut HoltStack,
        frame_index: usize,
        dst: u16,
        idx: i32,
    ) -> Result<(), VmError> {
        self.record_jit_runtime_property_stub();
        let saved_pc = stack[frame_index].pc;
        let frame = &mut stack[frame_index];
        let result = self.run_load_upvalue_reg(frame, dst, idx);
        stack[frame_index].pc = saved_pc;
        result
    }

    /// JIT bridge for `StoreUpvalue` from compiled code, delegating to
    /// [`Self::run_store_upvalue_reg`] (captured-binding write, including the
    /// write barrier). Frame PC is saved/restored as in
    /// [`Self::jit_runtime_load_upvalue`].
    ///
    /// # Errors
    /// Propagates `InvalidOperand` for a negative or out-of-range index.
    pub fn jit_runtime_store_upvalue(
        &mut self,
        stack: &mut HoltStack,
        frame_index: usize,
        src: u16,
        idx: i32,
    ) -> Result<(), VmError> {
        self.record_jit_runtime_property_stub();
        let saved_pc = stack[frame_index].pc;
        let frame = &mut stack[frame_index];
        let result = self.run_store_upvalue_reg(frame, src, idx);
        stack[frame_index].pc = saved_pc;
        result
    }

    pub(crate) fn run_collect_rest_reg(
        &mut self,
        stack: &mut HoltStack,
        top_idx: usize,
        dst: u16,
    ) -> Result<(), VmError> {
        // Drain rather than clone: the rest array is built once per call and
        // CollectRest is the single consumer.
        let elements: SmallVec<[Value; 4]> = self
            .frame_cold_mut(&mut stack[top_idx])
            .map(|c| std::mem::take(&mut c.rest_args))
            .unwrap_or_default();
        let array = self.alloc_stack_rooted_array_from_values(&*stack, elements, &[], &[])?;
        let frame = &mut stack[top_idx];
        write_register(frame, dst, Value::array(array))?;
        frame.advance_pc()?;
        Ok(())
    }

    pub(crate) fn run_enter_try_region(
        &mut self,
        frame: &mut Frame,
        region: crate::executable::code_block_cfg::CodeBlockExceptionRegion,
    ) -> Result<(), VmError> {
        debug_assert_eq!(region.enter_pc, frame.pc);
        self.run_enter_try_handler(
            frame,
            TryHandler {
                catch_pc: region.catch_pc,
                finally_pc: region.finally_pc,
                exc_register: region.exception_register,
            },
        )
    }

    pub(crate) fn run_enter_try_handler(
        &mut self,
        frame: &mut Frame,
        handler: TryHandler,
    ) -> Result<(), VmError> {
        self.frame_ensure_cold(frame).handlers.push(handler);
        frame.advance_pc()?;
        Ok(())
    }

    pub(crate) fn run_pop_parked_finally(
        &mut self,
        frame: &mut Frame,
        count: usize,
    ) -> Result<(), VmError> {
        if let Some(cold) = self.frame_cold_mut(frame) {
            for _ in 0..count {
                cold.parked_finally.pop();
            }
        }
        frame.advance_pc()?;
        Ok(())
    }

    pub(crate) fn run_leave_try(&mut self, frame: &mut Frame) -> Result<(), VmError> {
        let popped = self.frame_cold_mut(frame).and_then(|c| c.handlers.pop());
        let Some(handler) = popped else {
            return Err(VmError::InvalidOperand);
        };
        // §14.15.3 — leaving a try (or catch) body whose handler owns
        // a `finally` falls through into the finally block; park a
        // Normal completion so `Op::EndFinally` knows this entry was
        // not an unwind.
        if handler.finally_pc.is_some() {
            let cold = self.frame_ensure_cold(frame);
            let depth = cold.handlers.len() as u32;
            cold.parked_finally
                .push((crate::cold_frame::ParkedFinally::Normal, depth));
        }
        frame.advance_pc()?;
        Ok(())
    }
}
