//! Compiled synchronous module-operation transitions.
//!
//! # Contents
//! - Packed template-tier dispatch for static namespace and binding reads.
//! - Module-record marking, star re-export, and `import.meta.resolve`.
//!
//! # Invariants
//! - Every transition calls the same VM helper as interpreter dispatch.
//! - Constant indices resolve through the compiled frame's owning function.
//! - Promise-producing module evaluation and dynamic import are not owned here;
//!   they remain exact template side exits.
//!
//! # See also
//! - [`crate::module_ops`]
//! - [`crate::Interpreter::run_star_reexport_regs`]

use otter_bytecode::Op;

use crate::{ExecutionContext, Interpreter, VmError, activation_stack::ActivationStack};

impl Interpreter {
    /// Complete one synchronous module-family opcode for a published compiled
    /// frame. Arguments are schema-decoded scalar operands packed by the JIT.
    pub fn jit_runtime_module_op(
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
        match opcode {
            value if value == Op::ImportNamespace as u8 => {
                self.run_import_namespace_reg(
                    context,
                    stack,
                    frame_index,
                    arg0 as u16,
                    arg1 as u32,
                )?;
            }
            value if value == Op::ImportNamespaceDeferred as u8 => {
                self.run_import_namespace_deferred_reg(
                    context,
                    stack,
                    frame_index,
                    arg0 as u16,
                    arg1 as u32,
                )?;
            }
            value if value == Op::ModuleNamespaceObject as u8 => {
                self.run_module_namespace_object_reg(
                    context,
                    stack,
                    frame_index,
                    arg0 as u16,
                    arg1 as u32,
                )?;
            }
            value if value == Op::LoadImportBinding as u8 => {
                self.run_load_import_binding_reg(
                    context,
                    stack,
                    frame_index,
                    arg0 as u16,
                    arg1 as u32,
                    arg2 as u32,
                )?;
            }
            value if value == Op::StarReexport as u8 => {
                self.run_star_reexport_regs(context, stack, frame_index, arg0 as u16, arg1 as u16)?;
            }
            value if value == Op::MarkModuleEvaluated as u8 => {
                self.run_mark_module_evaluated_const(context, stack, frame_index, arg0 as u32)?;
            }
            value if value == Op::ImportMetaResolve as u8 => {
                self.run_import_meta_resolve_regs(
                    context,
                    stack,
                    frame_index,
                    arg0 as u16,
                    arg1 as u16,
                )?;
            }
            _ => return Err(VmError::InvalidOperand),
        }
        stack[frame_index].pc = saved_pc;
        Ok(())
    }
}
