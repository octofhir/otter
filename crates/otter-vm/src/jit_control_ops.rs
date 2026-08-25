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

use crate::{ActiveFrameMut, ExecutionContext, Frame, Interpreter, Value, VmError};

impl Interpreter {
    /// Read a captured binding unless a direct-eval `var` shadows it in the
    /// running frame's dynamic environment, probing at most `eval_depth`
    /// physical eval records strictly inside the declaration owner.
    pub(crate) fn run_load_shadowed_upvalue_reg(
        &mut self,
        context: &ExecutionContext,
        frame: &mut Frame,
        dst: u16,
        name_idx: u32,
        uv_idx: usize,
        eval_depth: u32,
    ) -> Result<(), VmError> {
        self.run_load_shadowed_upvalue_snap_reg(
            context, frame, dst, name_idx, uv_idx, eval_depth, None,
        )
    }

    /// [`Self::run_load_shadowed_upvalue_reg`] with an optional pre-RHS
    /// binding-sequence snapshot bounding which eval bindings are visible.
    pub(crate) fn run_load_shadowed_upvalue_snap_reg(
        &mut self,
        context: &ExecutionContext,
        frame: &mut Frame,
        dst: u16,
        name_idx: u32,
        uv_idx: usize,
        eval_depth: u32,
        snapshot: Option<u64>,
    ) -> Result<(), VmError> {
        let mut frame = ActiveFrameMut::materialized(frame);
        let index = u32::try_from(uv_idx).map_err(|_| VmError::InvalidOperand)?;
        let value = self.load_shadowed_upvalue_value(
            context, &mut frame, name_idx, index, eval_depth, snapshot,
        )?;
        frame.write(dst, value)?;
        frame.advance_pc()?;
        Ok(())
    }

    /// Value core of the shadowed-capture read, shared by interpreter
    /// dispatch and the committed binding call.
    pub(crate) fn load_shadowed_upvalue_value(
        &mut self,
        context: &ExecutionContext,
        frame: &mut ActiveFrameMut<'_>,
        name_idx: u32,
        index: u32,
        eval_depth: u32,
        snapshot: Option<u64>,
    ) -> Result<Value, VmError> {
        let dynamic_cell = context
            .string_constant_str_for_function(frame.function_id(), name_idx)
            .and_then(|name| {
                let env = frame.eval_env()?;
                crate::eval_env::eval_env_lookup_chain_bounded_snap(
                    &self.gc_heap,
                    env,
                    name,
                    eval_depth,
                    snapshot,
                )
            });
        if let Some(cell) = dynamic_cell {
            return Ok(crate::read_upvalue(&self.gc_heap, cell));
        }
        // Captured fallback keeps ordinary lexical semantics: reading the
        // still-uninitialized declaration is a named TDZ ReferenceError.
        let cell = frame.upvalue(index)?;
        let value = crate::read_upvalue(&self.gc_heap, cell);
        if value.is_hole() {
            let name = context
                .string_constant_str_for_function(frame.function_id(), name_idx)
                .ok_or(VmError::InvalidOperand)?;
            return Err(self.err_this_uninit(
                (format!("Cannot access '{name}' before initialization")).into(),
            ));
        }
        Ok(value)
    }

    /// Assign a captured binding whose name a live direct-eval `var` may
    /// shadow. The eval chain is probed within the bounded physical depth at
    /// store time; a hit always writes that cell, and the captured fallback
    /// follows the policy's mutability alphabet.
    pub(crate) fn run_store_shadowed_upvalue_checked_reg(
        &mut self,
        context: &ExecutionContext,
        frame: &mut Frame,
        value_reg: u16,
        name_idx: u32,
        uv_idx: usize,
        policy_imm: i32,
        snapshot: Option<u64>,
    ) -> Result<(), VmError> {
        let policy =
            otter_bytecode::opcode_schema::ShadowedUpvalueStorePolicy::from_imm32(policy_imm)
                .ok_or(VmError::InvalidOperand)?;
        let mut frame = ActiveFrameMut::materialized(frame);
        let index = u32::try_from(uv_idx).map_err(|_| VmError::InvalidOperand)?;
        let value = frame.read(value_reg)?;
        self.store_shadowed_upvalue_value(
            context,
            &mut frame,
            name_idx,
            index,
            policy.eval_depth,
            policy.fallback,
            value,
            snapshot,
        )?;
        frame.advance_pc()?;
        Ok(())
    }

    /// Value core of the shadowed-capture write.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn store_shadowed_upvalue_value(
        &mut self,
        context: &ExecutionContext,
        frame: &mut ActiveFrameMut<'_>,
        name_idx: u32,
        index: u32,
        eval_depth: u32,
        fallback: otter_bytecode::opcode_schema::ShadowedUpvalueFallback,
        value: Value,
        snapshot: Option<u64>,
    ) -> Result<(), VmError> {
        let dynamic_cell = context
            .string_constant_str_for_function(frame.function_id(), name_idx)
            .and_then(|name| {
                let env = frame.eval_env()?;
                crate::eval_env::eval_env_lookup_chain_bounded_snap(
                    &self.gc_heap,
                    env,
                    name,
                    eval_depth,
                    snapshot,
                )
            });
        if let Some(cell) = dynamic_cell {
            crate::store_upvalue(&mut self.gc_heap, cell, value);
            return Ok(());
        }
        match fallback {
            otter_bytecode::opcode_schema::ShadowedUpvalueFallback::Mutable => {
                let cell = frame.upvalue(index)?;
                if crate::read_upvalue(&self.gc_heap, cell).is_hole() {
                    return Err(VmError::TemporalDeadZone { local_index: index });
                }
                crate::store_upvalue(&mut self.gc_heap, cell, value);
            }
            otter_bytecode::opcode_schema::ShadowedUpvalueFallback::ImmutableThrow => {
                let name = context
                    .string_constant_str_for_function(frame.function_id(), name_idx)
                    .unwrap_or_default();
                return Err(
                    self.err_type((format!("Assignment to constant variable `{name}`")).into())
                );
            }
            otter_bytecode::opcode_schema::ShadowedUpvalueFallback::ImmutableIgnore => {}
        }
        Ok(())
    }

    /// Delete a nearer eval-introduced binding only within the bounded inner
    /// eval prefix. When the name still resolves to the captured declarative
    /// binding, it stays intact and the result is `false`.
    pub(crate) fn run_delete_shadowed_upvalue_reg(
        &mut self,
        context: &ExecutionContext,
        frame: &mut Frame,
        dst: u16,
        name_idx: u32,
        uv_idx: usize,
        eval_depth: u32,
    ) -> Result<(), VmError> {
        let mut frame = ActiveFrameMut::materialized(frame);
        let index = u32::try_from(uv_idx).map_err(|_| VmError::InvalidOperand)?;
        let deleted =
            self.delete_shadowed_upvalue_value(context, &mut frame, name_idx, index, eval_depth)?;
        frame.write(dst, deleted)?;
        frame.advance_pc()?;
        Ok(())
    }

    /// Value core of the shadowed-capture delete.
    pub(crate) fn delete_shadowed_upvalue_value(
        &mut self,
        context: &ExecutionContext,
        frame: &mut ActiveFrameMut<'_>,
        name_idx: u32,
        _index: u32,
        eval_depth: u32,
    ) -> Result<Value, VmError> {
        let deleted = context
            .string_constant_str_for_function(frame.function_id(), name_idx)
            .is_some_and(|name| {
                let Some(env) = frame.eval_env() else {
                    return false;
                };
                crate::eval_env::eval_env_delete_chain_bounded(
                    &mut self.gc_heap,
                    env,
                    name,
                    eval_depth,
                )
            });
        Ok(crate::Value::boolean(deleted))
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
                let value = self.load_shadowed_upvalue_value(
                    context,
                    frame,
                    arg1 as u32,
                    arg2 as u32,
                    u32::MAX,
                    None,
                )?;
                frame.write(arg0 as u16, value)?;
                frame.advance_pc()?;
            }
            _ => return Err(VmError::InvalidOperand),
        }
        frame.set_pc(saved_pc);
        Ok(())
    }
}

/// Decode the snapshot operand of the `*Snap` opcodes: a positive
/// integral number issued by [`otter_bytecode::Op::EvalBindingSeq`].
pub(crate) fn read_snapshot_register(
    frame: &crate::Frame,
    register: u16,
) -> Result<u64, crate::VmError> {
    let value = *crate::read_register(frame, register)?;
    let number = value
        .as_number()
        .map(|number| number.as_f64())
        .ok_or(crate::VmError::InvalidOperand)?;
    if !number.is_finite() || number < 0.0 {
        return Err(crate::VmError::InvalidOperand);
    }
    Ok(number as u64)
}
