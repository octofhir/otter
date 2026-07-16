//! Compiled `super` property access transitions.
//!
//! # Contents
//! - `LoadSuperProperty`, `LoadSuperElement`, `SetSuperProperty`, and
//!   `SetSuperElement` completion through the VM's super read/write helpers.
//!
//! # Invariants
//! - Each transition calls the same `run_load_super_property` /
//!   `run_store_super_property` helper the interpreter dispatches, so home-object
//!   `[[Prototype]]` accessor getters/setters fire identically and no super
//!   semantics are copied into JIT code.
//! - Accessor reentry runs through the shared ActivationStack/VmThread path; a
//!   committed super effect is never replayed by an exact side exit.
//!
//! # See also
//! - [`crate::Interpreter::run_load_super_property`]
//! - [`crate::Interpreter::run_store_super_property`]

use otter_bytecode::Op;

use crate::{
    ExecutionContext, Interpreter, SuperReadKey, VmError, VmPropertyKey,
    activation_stack::ActivationStack, read_register,
};

impl Interpreter {
    /// Complete one `super` property opcode for a published compiled frame.
    /// `arg0`/`arg1`/`arg2` name the destination/home/name-or-key registers per
    /// opcode; a name index is a constant, a computed key is a register.
    pub fn jit_runtime_super_op(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
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
        let strict = context.function_is_strict(stack[frame_index].function_id);
        match opcode {
            value if value == Op::LoadSuperProperty as u8 => {
                let dst = arg0 as u16;
                let atom = context
                    .property_atom(arg2 as u32)
                    .ok_or(VmError::InvalidOperand)?;
                let name = atom.name();
                let home = *read_register(&stack[frame_index], arg1 as u16)?;
                self.run_load_super_property(
                    context,
                    stack,
                    frame_index,
                    dst,
                    home,
                    SuperReadKey::Resolved(VmPropertyKey::String(name)),
                )?;
            }
            value if value == Op::LoadSuperElement as u8 => {
                let dst = arg0 as u16;
                let home = *read_register(&stack[frame_index], arg1 as u16)?;
                let key_raw = *read_register(&stack[frame_index], arg2 as u16)?;
                self.run_load_super_property(
                    context,
                    stack,
                    frame_index,
                    dst,
                    home,
                    SuperReadKey::Computed(key_raw),
                )?;
            }
            value if value == Op::SetSuperProperty as u8 => {
                let atom = context
                    .property_atom(arg1 as u32)
                    .ok_or(VmError::InvalidOperand)?;
                let name = atom.name();
                let home = *read_register(&stack[frame_index], arg0 as u16)?;
                let value = *read_register(&stack[frame_index], arg2 as u16)?;
                self.run_store_super_property(
                    context,
                    stack,
                    frame_index,
                    home,
                    SuperReadKey::Resolved(VmPropertyKey::String(name)),
                    value,
                    strict,
                )?;
            }
            value if value == Op::SetSuperElement as u8 => {
                let home = *read_register(&stack[frame_index], arg0 as u16)?;
                let key_raw = *read_register(&stack[frame_index], arg1 as u16)?;
                let value = *read_register(&stack[frame_index], arg2 as u16)?;
                self.run_store_super_property(
                    context,
                    stack,
                    frame_index,
                    home,
                    SuperReadKey::Computed(key_raw),
                    value,
                    strict,
                )?;
            }
            _ => return Err(VmError::InvalidOperand),
        }
        stack[frame_index].pc = saved_pc;
        Ok(())
    }
}
