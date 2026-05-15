//! Module-related opcode helpers.
//!
//! Static namespace imports and `import.meta.resolve` are fixed-width
//! bytecodes, so dispatch can decode their operands from the executable view.
//!
//! # Contents
//! - Static namespace object resolution.
//! - Dynamic `import(specifier)` promise construction / scheduling.
//! - `import.meta.resolve(specifier)` relative URL resolution.
//!
//! # Invariants
//! - Static namespace imports must already be present in the linked module
//!   namespace table.
//! - Dynamic import always writes a Promise to the destination register.
//! - `import.meta.resolve` accepts only string specifiers.
//!
//! # See also
//! - [`crate::execution_context`]

use crate::{
    ExecutionContext, Frame, Interpreter, JsString, Value, VmError,
    operand_decode::register_operand, promise_dispatch, read_register, resolve_relative_url,
    write_register,
};
use otter_bytecode::Operand;
use smallvec::SmallVec;

impl Interpreter {
    pub(crate) fn run_import_namespace_reg(
        &mut self,
        context: &ExecutionContext,
        frame: &mut Frame,
        dst: u16,
        spec_idx: u32,
    ) -> Result<(), VmError> {
        let specifier = context
            .string_constant_str(spec_idx)
            .ok_or(VmError::InvalidOperand)?;
        let referrer = frame.module_url.clone();
        let namespace = self
            .resolve_module_namespace(context, referrer.as_ref(), specifier)
            .ok_or_else(|| VmError::UnknownIntrinsic {
                name: format!("import \"{specifier}\""),
            })?;
        write_register(frame, dst, Value::Object(namespace))?;
        frame.pc += 1;
        Ok(())
    }

    pub(crate) fn run_import_meta_resolve_regs(
        &self,
        frame: &mut Frame,
        dst: u16,
        spec_reg: u16,
    ) -> Result<(), VmError> {
        let spec_value = read_register(frame, spec_reg)?.clone();
        let specifier = match spec_value {
            Value::String(s) => s.to_lossy_string(),
            _ => return Err(VmError::TypeMismatch),
        };
        let resolved = resolve_relative_url(Some(&frame.module_url), &specifier);
        let resolved_str =
            JsString::from_str(&resolved, &self.string_heap).map_err(|_| VmError::TypeMismatch)?;
        write_register(frame, dst, Value::String(resolved_str))?;
        frame.pc += 1;
        Ok(())
    }

    pub(crate) fn run_import_namespace_dynamic_operands(
        &mut self,
        context: &ExecutionContext,
        stack: &mut SmallVec<[Frame; 8]>,
        top_idx: usize,
        operands: &[Operand],
    ) -> Result<(), VmError> {
        let dst = register_operand(operands.first())?;
        let spec_reg = register_operand(operands.get(1))?;
        let spec_value = read_register(&stack[top_idx], spec_reg)?.clone();
        let referrer = stack[top_idx].module_url.clone();
        let import_context = context.clone();
        let promise = match spec_value {
            Value::String(s) => {
                let specifier = s.to_lossy_string();
                if let Some(ns) =
                    self.resolve_module_namespace(context, referrer.as_ref(), &specifier)
                {
                    let namespace_value = Value::Object(ns);
                    promise_dispatch::PromiseBuilder::with_context(import_context.clone())
                        .fulfilled_stack_rooted(self, stack, namespace_value, &[], &[])?
                } else if let Some(loader) = self.dynamic_import_loader.clone() {
                    let pending =
                        promise_dispatch::PromiseBuilder::with_context(import_context.clone())
                            .pending_stack_rooted(self, stack, &[], &[])?;
                    let token = self
                        .dynamic_import_registry
                        .insert(pending, import_context.clone());
                    self.record_runtime_host_op_enqueued();
                    loader.schedule(token, specifier, referrer.as_ref().to_string());
                    pending
                } else {
                    let reason = self.make_type_error(&format!(
                        "dynamic import: module not resolvable: \"{specifier}\""
                    ))?;
                    promise_dispatch::PromiseBuilder::with_context(import_context.clone())
                        .rejected_stack_rooted(self, stack, reason, &[], &[])?
                }
            }
            _ => {
                let reason = self.make_type_error("dynamic import: specifier must be a string")?;
                promise_dispatch::PromiseBuilder::with_context(import_context)
                    .rejected_stack_rooted(self, stack, reason, &[], &[])?
            }
        };
        write_register(&mut stack[top_idx], dst, Value::Promise(promise))?;
        stack[top_idx].pc += 1;
        Ok(())
    }
}
