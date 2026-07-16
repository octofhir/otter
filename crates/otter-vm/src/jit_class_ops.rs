//! Compiled class-construction transitions.
//!
//! # Contents
//! - Single-implementation register helpers for `BindThisValue`, `ClassCheck`,
//!   and `SetFunctionName`, shared by the interpreter dispatch and the compiled
//!   transition.
//!
//! # Invariants
//! - No class-construction semantics are duplicated in JIT code; each opcode
//!   calls the same VM register helper the interpreter dispatches.
//! - A committed binding/name effect is never replayed by an exact side exit.
//!
//! # See also
//! - [`crate::abstract_ops::is_constructor`]

use otter_bytecode::Op;

use crate::{
    ExecutionContext, Interpreter, JsString, Value, VmError, abstract_ops,
    activation_stack::ActivationStack, object, read_register,
};

impl Interpreter {
    /// §13.3.7.2 BindThisValue — bind `super()`'s result into the nearest
    /// derived-constructor `this`, rejecting a double binding.
    pub(crate) fn run_bind_this_value_reg(
        &mut self,
        stack: &mut ActivationStack,
        top_idx: usize,
        src: u16,
    ) -> Result<(), VmError> {
        let value = *read_register(&stack[top_idx], src)?;
        let target = (0..=top_idx).rev().find(|&i| {
            self.frame_cold(&stack[i])
                .is_some_and(|c| c.is_derived_constructor)
        });
        if let Some(ti) = target {
            if !stack[ti].this_value.is_hole() {
                return Err(self.err_this_uninit(
                    ("super constructor may only be called once".to_string()).into(),
                ));
            }
            stack[ti].this_value = value;
            let frame = &mut stack[ti];
            let derived_this_cell = self
                .frame_cold(frame)
                .and_then(|cold| cold.derived_this_cell);
            if let Some(cell) = derived_this_cell {
                crate::store_upvalue(&mut self.gc_heap, cell, value);
            }
            if let Some(obj) = value.as_object() {
                let cold = self.frame_ensure_cold(frame);
                cold.construct_target = Some(obj);
            }
        } else {
            let derived_this_cell = self
                .frame_cold(&stack[top_idx])
                .and_then(|cold| cold.derived_this_cell);
            let Some(cell) = derived_this_cell else {
                return Err(self.err_this_uninit(
                    ("super called outside a derived constructor".to_string()).into(),
                ));
            };
            if !crate::read_upvalue(&self.gc_heap, cell).is_hole() {
                return Err(self.err_this_uninit(
                    ("super constructor may only be called once".to_string()).into(),
                ));
            }
            crate::store_upvalue(&mut self.gc_heap, cell, value);
        }
        stack[top_idx].advance_pc()?;
        Ok(())
    }

    /// §15.7.14 class-definition validation: heritage IsConstructor (`kind == 0`)
    /// or a static computed key that must not be `"prototype"`.
    pub(crate) fn run_class_check_reg(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        top_idx: usize,
        kind: u32,
        reg: u16,
    ) -> Result<(), VmError> {
        let value = *read_register(&stack[top_idx], reg)?;
        match kind {
            0 => {
                if !value.is_null() && !abstract_ops::is_constructor(&value, context, &self.gc_heap)
                {
                    return Err(self.err_type(
                        ("Class extends value is not a constructor or null".to_string()).into(),
                    ));
                }
            }
            _ => {
                if value
                    .as_string(&self.gc_heap)
                    .is_some_and(|s| s.to_lossy_string(&self.gc_heap) == "prototype")
                {
                    return Err(self.err_type(
                        ("Classes may not have a static property named 'prototype'".to_string())
                            .into(),
                    ));
                }
            }
        }
        stack[top_idx].advance_pc()?;
        Ok(())
    }

    /// §10.2.10 SetFunctionName — name an anonymous function from a run-time key.
    pub(crate) fn run_set_function_name_reg(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        top_idx: usize,
        fn_reg: u16,
        key_reg: u16,
        prefix_idx: u32,
    ) -> Result<(), VmError> {
        let callee = *read_register(&stack[top_idx], fn_reg)?;
        let key_value = *read_register(&stack[top_idx], key_reg)?;
        let prefix = context
            .property_atom_for_function(stack[top_idx].function_id, prefix_idx)
            .map(|atom| atom.name().to_string())
            .unwrap_or_default();
        let mut name = if let Some(sym) = key_value.as_symbol(&self.gc_heap) {
            match sym.description() {
                Some(desc) => format!("[{}]", desc.to_lossy_string(&self.gc_heap)),
                None => String::new(),
            }
        } else {
            key_value.display_string(&self.gc_heap)
        };
        if !prefix.is_empty() {
            name = format!("{prefix} {name}");
        }
        let callee = match callee.as_class_constructor() {
            Some(c) => c.ctor(&self.gc_heap),
            None => callee,
        };
        if let Some(fid) = callee.as_function().or_else(|| {
            callee
                .as_closure(&self.gc_heap)
                .map(|c| c.cached_function_id)
        }) {
            let owner = callee.as_closure(&self.gc_heap);
            let name_str = JsString::from_str(&name, &mut self.gc_heap)?;
            let descriptor = object::PropertyDescriptor {
                kind: object::DescriptorKind::Data {
                    value: Value::string(name_str),
                },
                flags: object::PropertyFlags::new(false, false, true),
            };
            self.ordinary_function_define_own_property(
                Some(context),
                owner,
                fid,
                "name",
                None,
                descriptor,
            )?;
        }
        stack[top_idx].advance_pc()?;
        Ok(())
    }

    /// Complete one class-construction opcode for a published compiled frame.
    pub fn jit_runtime_class_op(
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
            value if value == Op::BindThisValue as u8 => {
                self.run_bind_this_value_reg(stack, frame_index, arg0 as u16)?;
            }
            value if value == Op::ClassCheck as u8 => {
                self.run_class_check_reg(context, stack, frame_index, arg1 as u32, arg0 as u16)?;
            }
            value if value == Op::SetFunctionName as u8 => {
                self.run_set_function_name_reg(
                    context,
                    stack,
                    frame_index,
                    arg0 as u16,
                    arg2 as u16,
                    arg1 as u32,
                )?;
            }
            _ => return Err(VmError::InvalidOperand),
        }
        stack[frame_index].pc = saved_pc;
        Ok(())
    }
}
