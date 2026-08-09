//! Interpreter-owned class-construction helpers.
//!
//! # Contents
//! - Single-implementation value/register helpers for `BindThisValue`,
//!   `ClassCheck`, and `SetFunctionName`, shared by interpreter and compiled
//!   dispatch.
//!
//! # Invariants
//! - Compiled activations use [`crate::RuntimeCall::class_op`] and never recover
//!   a materialized frame index through this module.
//! - A committed binding/name effect is never replayed by an exact side exit.
//!
//! # See also
//! - [`crate::abstract_ops::is_constructor`]

use crate::{
    ExecutionContext, Interpreter, VmError, abstract_ops, activation_stack::ActivationStack,
    object, read_register,
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
        self.run_bind_this_value(stack, top_idx, value)
    }

    /// Value-form of [`Self::run_bind_this_value_reg`] for Machine IR, where
    /// `super()` has already produced an SSA value.
    pub(crate) fn run_bind_this_value(
        &mut self,
        stack: &mut ActivationStack,
        top_idx: usize,
        value: crate::Value,
    ) -> Result<(), VmError> {
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
            self.with_handle_scope(|interp, scope| {
                let callee = interp.scoped_value(scope, callee);
                let name = interp.scoped_string(scope, &name)?;
                let callee = interp.escape_scoped(callee);
                let owner = callee.as_closure(&interp.gc_heap);
                let descriptor = object::PropertyDescriptor {
                    kind: object::DescriptorKind::Data {
                        value: interp.escape_scoped(name),
                    },
                    flags: object::PropertyFlags::new(false, false, true),
                };
                interp.ordinary_function_define_own_property(
                    stack,
                    Some(context),
                    owner,
                    fid,
                    "name",
                    None,
                    descriptor,
                )?;
                Ok::<(), VmError>(())
            })?;
        }
        stack[top_idx].advance_pc()?;
        Ok(())
    }
}
