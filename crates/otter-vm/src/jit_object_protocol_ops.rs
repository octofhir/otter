//! Compiled object property-protocol query transitions.
//!
//! # Contents
//! - `Instanceof`, `HasProperty` (`in`), `GetPrototype`, and `SetPrototype`
//!   completion through the VM's existing Proxy-aware drivers and fast paths.
//!
//! # Invariants
//! - Each transition first runs the interpreter's synchronous
//!   `drive_*_proxy` helper (which fires `@@hasInstance`, `has`,
//!   `getPrototypeOf`, and `setPrototypeOf` traps through `run_callable_sync`)
//!   and otherwise the same `run_*_regs` fast path; no protocol semantics are
//!   copied into JIT code.
//! - Trap reentry runs through the shared HoltStack/VmThread path; a committed
//!   protocol effect is never replayed by an exact side exit.
//!
//! # See also
//! - [`crate::Interpreter::drive_has_property_proxy`]
//! - [`crate::Interpreter::drive_instanceof`]

use otter_bytecode::{Op, Operand};

use crate::{ExecutionContext, Interpreter, VmError, holt_stack::HoltStack};

impl Interpreter {
    /// Complete one object property-protocol opcode for a published compiled
    /// frame. `arg0`/`arg1`/`arg2` name the destination/left/right (or
    /// object/prototype) registers per opcode.
    pub fn jit_runtime_object_protocol_op(
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
        let a = arg0 as u16;
        let b = arg1 as u16;
        let c = arg2 as u16;
        let ops = [
            Operand::Register(a),
            Operand::Register(b),
            Operand::Register(c),
        ];
        match opcode {
            value if value == Op::Instanceof as u8 => {
                if !self.drive_instanceof(stack, context, &ops)? {
                    self.run_instanceof_legacy_regs(&mut stack[frame_index], a, b, c)?;
                }
            }
            value if value == Op::HasProperty as u8 => {
                if !self.drive_has_property_proxy(stack, context, &ops)? {
                    self.run_has_property_regs(&mut stack[frame_index], context, a, b, c)?;
                }
            }
            value if value == Op::GetPrototype as u8 => {
                if !self.drive_get_prototype_proxy(stack, context, &ops)? {
                    self.run_get_prototype_regs(&mut stack[frame_index], a, b)?;
                }
            }
            value if value == Op::SetPrototype as u8 => {
                if !self.drive_set_prototype_proxy(stack, context, &ops)? {
                    self.run_set_prototype_regs(context, &mut stack[frame_index], a, b)?;
                }
            }
            _ => return Err(VmError::InvalidOperand),
        }
        stack[frame_index].pc = saved_pc;
        Ok(())
    }
}
