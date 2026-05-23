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

use smallvec::SmallVec;

use crate::{
    Frame, Interpreter, TryHandler, Value, VmError, read_register, read_upvalue, store_upvalue,
    write_register,
};

impl Interpreter {
    pub(crate) fn run_load_this_reg(&self, frame: &mut Frame, dst: u16) -> Result<(), VmError> {
        let value = frame.this_value;
        write_register(frame, dst, value)?;
        frame.pc += 1;
        Ok(())
    }

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
        frame.pc += 1;
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
        write_register(frame, dst, value)?;
        frame.pc += 1;
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
        frame.pc += 1;
        Ok(())
    }

    pub(crate) fn run_collect_rest_reg(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
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
        frame.pc += 1;
        Ok(())
    }

    pub(crate) fn run_collect_arguments_reg(
        &self,
        frame: &mut Frame,
        dst: u16,
    ) -> Result<(), VmError> {
        write_register(frame, dst, Value::undefined())?;
        frame.pc += 1;
        Ok(())
    }

    pub(crate) fn run_enter_try_regs(
        &mut self,
        frame: &mut Frame,
        catch_off: i32,
        finally_off: i32,
        exc_register: u16,
    ) -> Result<(), VmError> {
        let next_pc = frame.pc.checked_add(1).ok_or(VmError::InvalidOperand)? as i64;
        let resolve = |off: i32| -> Result<Option<u32>, VmError> {
            if off == crate::NO_HANDLER_OFFSET {
                return Ok(None);
            }
            let target = next_pc + off as i64;
            if target < 0 || target > u32::MAX as i64 {
                return Err(VmError::InvalidOperand);
            }
            Ok(Some(target as u32))
        };
        let catch_pc = resolve(catch_off)?;
        let finally_pc = resolve(finally_off)?;
        if catch_pc.is_none() && finally_pc.is_none() {
            return Err(VmError::InvalidOperand);
        }
        self.frame_ensure_cold(frame).handlers.push(TryHandler {
            catch_pc,
            finally_pc,
            exc_register,
        });
        frame.pc += 1;
        Ok(())
    }

    pub(crate) fn run_leave_try(&mut self, frame: &mut Frame) -> Result<(), VmError> {
        let popped = self.frame_cold_mut(frame).and_then(|c| c.handlers.pop());
        if popped.is_none() {
            return Err(VmError::InvalidOperand);
        }
        frame.pc += 1;
        Ok(())
    }
}
