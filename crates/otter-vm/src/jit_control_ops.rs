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
//! - Shadowing walks the same nearest-first GC-owned eval environment as every
//!   other dynamic binding operation; no frame-local mirror exists.
//! - The JIT transition calls the same register helper as interpreter dispatch;
//!   dynamic-scope semantics are not duplicated in machine-code support.
//!
//! # See also
//! - [`crate::ExecutionContext::string_constant_str_for_function`]

use otter_bytecode::Op;

use crate::{ActiveFrameMut, ExecutionContext, Frame, Interpreter, VmError};

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
        let mut frame = ActiveFrameMut::materialized(frame);
        self.run_load_shadowed_upvalue_active_reg(context, &mut frame, dst, name_idx, uv_idx)
    }

    fn run_load_shadowed_upvalue_active_reg(
        &mut self,
        context: &ExecutionContext,
        frame: &mut ActiveFrameMut<'_>,
        dst: u16,
        name_idx: u32,
        uv_idx: usize,
    ) -> Result<(), VmError> {
        let dynamic_cell = context
            .string_constant_str_for_function(frame.function_id(), name_idx)
            .and_then(|name| {
                let env = frame.eval_env()?;
                crate::eval_env::eval_env_lookup_chain(&self.gc_heap, env, name)
            });
        let cell = if let Some(cell) = dynamic_cell {
            cell
        } else {
            frame.upvalue(u32::try_from(uv_idx).map_err(|_| VmError::InvalidOperand)?)?
        };
        let value = crate::read_upvalue(&self.gc_heap, cell);
        frame.write(dst, value)?;
        frame.advance_pc()?;
        Ok(())
    }

    /// Complete one reentrant control-family opcode for a published compiled
    /// frame. `arg0`/`arg1`/`arg2` carry destination, name constant, and
    /// upvalue index respectively.
    pub fn jit_runtime_control_op(
        &mut self,
        context: &ExecutionContext,
        frame: &mut ActiveFrameMut<'_>,
        opcode: u8,
        arg0: u64,
        arg1: u64,
        arg2: u64,
    ) -> Result<(), VmError> {
        self.record_jit_runtime_stub_class(crate::native_abi::RuntimeStubClass::Reentrant);
        let saved_pc = frame.pc();
        match opcode {
            value if value == Op::LoadShadowedUpvalue as u8 => {
                self.run_load_shadowed_upvalue_active_reg(
                    context,
                    frame,
                    arg0 as u16,
                    arg1 as u32,
                    arg2 as usize,
                )?;
            }
            _ => return Err(VmError::InvalidOperand),
        }
        frame.set_pc(saved_pc);
        Ok(())
    }
}
