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
        // §13.3.7.3 — a derived constructor's `this` is in the TDZ
        // until `super(...)` binds it (sentinel: `Value::hole()`).
        // Reading it early, including via `super.prop`, is a
        // ReferenceError.
        if value.is_hole() {
            return Err(VmError::ThisUninitialized {
                message: "must call super constructor in derived class before accessing 'this' or returning from derived constructor".to_string(),
            });
        }
        write_register(frame, dst, value)?;
        frame.advance_pc(self.current_byte_len)?;
        Ok(())
    }

    /// §13.3.7.3 SuperCall steps 7–9 — bind the derived-constructor
    /// `this` to the object `super(...)` produced. The result is also
    /// recorded as the construct target so an implicit `return` from
    /// the constructor yields the bound object (§10.2.2). A second
    /// `super(...)` (i.e. `this` already initialized) is a
    /// ReferenceError.
    pub(crate) fn run_bind_this_value(
        &mut self,
        frame: &mut Frame,
        src: u16,
    ) -> Result<(), VmError> {
        if !frame.this_value.is_hole() {
            return Err(VmError::ThisUninitialized {
                message: "super constructor may only be called once".to_string(),
            });
        }
        let value = *read_register(frame, src)?;
        frame.this_value = value;
        if let Some(obj) = value.as_object() {
            let cold = self.frame_ensure_cold(frame);
            cold.construct_target = Some(obj);
        }
        frame.advance_pc(self.current_byte_len)?;
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
        frame.advance_pc(self.current_byte_len)?;
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
        frame.advance_pc(self.current_byte_len)?;
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
        frame.advance_pc(self.current_byte_len)?;
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
        frame.advance_pc(self.current_byte_len)?;
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
        frame.advance_pc(self.current_byte_len)?;
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
        frame.advance_pc(self.current_byte_len)?;
        Ok(())
    }

    pub(crate) fn run_leave_try(&mut self, frame: &mut Frame) -> Result<(), VmError> {
        let popped = self.frame_cold_mut(frame).and_then(|c| c.handlers.pop());
        if popped.is_none() {
            return Err(VmError::InvalidOperand);
        }
        frame.advance_pc(self.current_byte_len)?;
        Ok(())
    }
}
