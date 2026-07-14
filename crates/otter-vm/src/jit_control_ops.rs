//! Compiled control and dynamic-upvalue transitions.
//!
//! # Contents
//! - The shared `LoadShadowedUpvalue` register helper used by interpreter and
//!   template-tier dispatch.
//! - The reentrant control-family transition for published compiled frames.
//!
//! # Invariants
//! - Shadow names are resolved from the executing frame's function-owned
//!   constant pool, including cross-chunk callees.
//! - The JIT transition calls the same register helper as interpreter dispatch;
//!   dynamic-scope semantics are not duplicated in machine-code support.
//!
//! # See also
//! - [`crate::ExecutionContext::string_constant_str_for_function`]

use otter_bytecode::Op;

use crate::{ExecutionContext, Frame, Interpreter, VmError, holt_stack::HoltStack};

impl Interpreter {
    /// Read a captured binding unless a direct-eval `var` shadows it in the
    /// running frame's dynamic environment.
    pub(crate) fn run_load_shadowed_upvalue_reg(
        &mut self,
        context: &ExecutionContext,
        frame: &mut Frame,
        dst: u16,
        name_idx: u32,
        uv_idx: usize,
    ) -> Result<(), VmError> {
        let cell = if let Some(name) =
            context.string_constant_str_for_function(frame.function_id, name_idx)
            && let Some(cell) = self
                .frame_cold(frame)
                .and_then(|cold| cold.eval_vars.as_ref())
                .and_then(|map| map.get(name))
                .copied()
        {
            cell
        } else {
            frame
                .upvalues
                .get(uv_idx)
                .copied()
                .ok_or(VmError::InvalidOperand)?
        };
        let value = crate::read_upvalue(&self.gc_heap, cell);
        crate::write_register(frame, dst, value)?;
        frame.advance_pc()?;
        Ok(())
    }

    /// Complete one reentrant control-family opcode for a published compiled
    /// frame. `arg0`/`arg1`/`arg2` carry destination, name constant, and
    /// upvalue index respectively.
    pub fn jit_runtime_control_op(
        &mut self,
        context: &ExecutionContext,
        stack: &mut HoltStack,
        frame_index: usize,
        opcode: u8,
        arg0: u64,
        arg1: u64,
        arg2: u64,
    ) -> Result<(), VmError> {
        self.record_jit_runtime_stub_class(crate::native_abi::RuntimeStubClass::Reentrant);
        if frame_index + 1 != stack.len() {
            return Err(VmError::InvalidOperand);
        }
        let saved_pc = stack[frame_index].pc;
        match opcode {
            value if value == Op::LoadShadowedUpvalue as u8 => {
                self.run_load_shadowed_upvalue_reg(
                    context,
                    &mut stack[frame_index],
                    arg0 as u16,
                    arg1 as u32,
                    arg2 as usize,
                )?;
            }
            _ => return Err(VmError::InvalidOperand),
        }
        stack[frame_index].pc = saved_pc;
        Ok(())
    }
}
