//! Interpreter-owned class-construction helpers.
//!
//! # Contents
//! - Bind-only and register-dispatch helpers for `BindThisValue` over a
//!   derived constructor's frame-held `this`.
//! - Interpreter class checks and function-name installation.
//!
//! # Invariants
//! - Compiled `BindThisValue` uses the committed scalar-value boundary; the
//!   bind-only kernel never advances the caller-owned logical PC.
//! - A committed binding/name effect is never replayed by an exact side exit.
//!
//! # See also
//! - [`crate::abstract_ops::is_constructor`]

use crate::{
    ExecutionContext, Interpreter, VmError, abstract_ops, activation_stack::ActivationStack,
    object, read_register,
};

impl Interpreter {
    /// §9.1.1.3.1 BindThisValue — bind `super()`'s result as the running
    /// derived constructor's frame-held `this`, rejecting a double binding.
    pub(crate) fn run_bind_this_value_reg(
        &mut self,
        stack: &mut ActivationStack,
        top_idx: usize,
        src: u16,
    ) -> Result<(), VmError> {
        let value = *read_register(&stack[top_idx], src)?;
        self.bind_this_value(stack, top_idx, value)?;
        stack[top_idx].advance_pc()
    }

    /// Bind-only value kernel shared with committed generated callers.
    ///
    /// The caller owns logical-PC advancement. `BindThisValue` runs only in
    /// the derived constructor whose `this` it binds: a `this` reached from
    /// an arrow or a direct eval lives in a context slot and binds through
    /// `BindThisContextSlot` instead.
    pub(crate) fn bind_this_value(
        &mut self,
        stack: &mut ActivationStack,
        top_idx: usize,
        value: crate::Value,
    ) -> Result<crate::Value, VmError> {
        let frame = &mut stack[top_idx];
        if !frame.is_derived_constructor() {
            return Err(self.err_this_uninit(
                ("super called outside a derived constructor".to_string()).into(),
            ));
        }
        if !frame.this_value.is_hole() {
            return Err(self.err_this_uninit(crate::context_ops::SUPER_CALLED_TWICE.into()));
        }
        frame.this_value = value;
        Ok(value)
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
