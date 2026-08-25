//! Compiled `delete` transitions.
//!
//! # Contents
//! - `DeleteProperty`, `DeleteElement`, and `DeleteDynamic` completion through
//!   the VM's Proxy-aware delete drivers, fast paths, and unqualified-delete
//!   helper.
//!
//! # Invariants
//! - Each transition mirrors the interpreter dispatch exactly: the same
//!   deferred-namespace readiness step, the same `drive_delete_*_proxy` driver
//!   (Proxy `deleteProperty` trap through `run_callable_sync`), and the same
//!   `run_delete_*` fast path; no delete semantics are copied into JIT code.
//! - A committed delete (including a strict-mode throw) is never replayed by an
//!   exact side exit.
//!
//! # See also
//! - [`crate::Interpreter::drive_delete_property_proxy`]
//! - [`crate::Interpreter::run_delete_dynamic_reg`]

use otter_bytecode::{Op, Operand};

use crate::{
    ActiveFrameMut, ExecutionContext, Interpreter, VmError, activation_stack::ActivationStack,
    read_register,
};

impl Interpreter {
    /// Complete one `delete` opcode for a published compiled frame. For
    /// `DeleteProperty` `arg1`/`arg2` are the object register and constant name
    /// index; for `DeleteElement` they are the object and key registers; for
    /// `DeleteDynamic` `arg1` is the constant name index.
    pub(crate) fn jit_runtime_delete_op(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        frame_index: usize,
        frame: &mut ActiveFrameMut<'_>,
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
        let saved_native_pc = frame.pc();
        let dst = arg0 as u16;
        let strict = context.function_is_strict(stack[frame_index].function_id);
        match opcode {
            value if value == Op::DeleteProperty as u8 => {
                let obj_reg = arg1 as u16;
                let name_idx = arg2 as u32;
                // The atom index is chunk-local and a generated direct call
                // can run a sibling chunk's function under this activation,
                // so resolve against the frame's own chunk.
                let key = context
                    .property_atom_for_function(stack[frame_index].function_id, name_idx)
                    .ok_or(VmError::InvalidOperand)?;
                let receiver = *read_register(&stack[frame_index], obj_reg)?;
                if receiver.as_object().is_some_and(|o| {
                    crate::object::deferred_namespace_target(o, &self.gc_heap).is_some()
                }) {
                    self.ensure_deferred_namespace_ready(
                        stack,
                        context,
                        &receiver,
                        key.name() != "then",
                    )?;
                }
                let ops = [
                    Operand::Register(dst),
                    Operand::Register(obj_reg),
                    Operand::ConstIndex(name_idx),
                ];
                if !self.drive_delete_property_proxy(stack, context, &ops)? {
                    self.run_delete_property_reg(
                        &mut stack[frame_index],
                        dst,
                        obj_reg,
                        key,
                        strict,
                    )?;
                }
            }
            value if value == Op::DeleteElement as u8 => {
                let obj_reg = arg1 as u16;
                let idx_reg = arg2 as u16;
                let receiver = *read_register(&stack[frame_index], obj_reg)?;
                if receiver.as_object().is_some_and(|o| {
                    crate::object::deferred_namespace_target(o, &self.gc_heap).is_some()
                }) {
                    let key_val = *read_register(&stack[frame_index], idx_reg)?;
                    let symbol_like = key_val.as_symbol(&self.gc_heap).is_some()
                        || key_val
                            .as_string(&self.gc_heap)
                            .is_some_and(|s| s.to_lossy_string(&self.gc_heap) == "then");
                    self.ensure_deferred_namespace_ready(stack, context, &receiver, !symbol_like)?;
                }
                let ops = [
                    Operand::Register(dst),
                    Operand::Register(obj_reg),
                    Operand::Register(idx_reg),
                ];
                if !self.drive_delete_element_proxy(stack, context, &ops)? {
                    self.run_delete_element_regs(
                        &mut stack[frame_index],
                        dst,
                        obj_reg,
                        idx_reg,
                        strict,
                    )?;
                }
            }
            value if value == Op::DeleteDynamic as u8 => {
                self.run_delete_dynamic_active_reg(context, frame, dst, arg1 as u32)?;
            }
            _ => return Err(VmError::InvalidOperand),
        }
        stack[frame_index].pc = saved_pc;
        frame.set_pc(saved_native_pc);
        Ok(())
    }
}
