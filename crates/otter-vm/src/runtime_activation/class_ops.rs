//! Stack-owned class construction operations.
//!
//! # Contents
//! - [`ClassRuntimeOp`] is the decoded class-operation descriptor.
//! - [`RuntimeCall::class_op`] handles derived `this`, heritage checks, and
//!   computed function naming for either physical activation representation.
//!
//! # Invariants
//! - Heritage/name inputs are copied from the published root window before VM work.
//! - `SetFunctionName` performs its one observable property definition exactly once.
//! - A failed check or name definition never requests replay at the bytecode site.
//! - Moving-GC inputs remain reachable through the active frame while scoped
//!   handles protect intermediate allocations.
//!
//! # See also
//! - [`crate::class_ops`]

use crate::{VmError, abstract_ops, object};

use super::RuntimeCall;

/// Decoded class-construction operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClassRuntimeOp {
    /// Bind the value in `source` as a derived constructor's `this`.
    BindThis {
        /// Source register containing the completed `super()` result.
        source: u16,
    },
    /// Validate a heritage value or computed static key.
    Check {
        /// Register containing the value to validate.
        register: u16,
        /// Zero for heritage validation; nonzero for a computed static key.
        kind: u32,
    },
    /// Apply `SetFunctionName` to one computed class member.
    SetFunctionName {
        /// Register containing the function or class constructor.
        function: u16,
        /// Register containing the computed property key.
        key: u16,
        /// Function-local constant index for the optional name prefix.
        prefix_index: u32,
    },
}

impl RuntimeCall<'_> {
    /// Complete one decoded class operation without materializing the caller.
    pub fn class_op(&mut self, operation: ClassRuntimeOp) -> Result<(), VmError> {
        // SAFETY: RuntimeCall brands exclusive mutator access for this operation.
        let vm = unsafe { &mut *self.vm.as_ptr() };
        vm.record_jit_runtime_stub_class(crate::native_abi::RuntimeStubClass::Reentrant);
        match operation {
            ClassRuntimeOp::BindThis { source } => self.bind_derived_this_value(self.read(source)?),
            ClassRuntimeOp::Check { register, kind } => {
                let value = self.read(register)?;
                if kind == 0 {
                    if !value.is_null()
                        && !abstract_ops::is_constructor(
                            &value,
                            unsafe { self.context.as_ref() },
                            &vm.gc_heap,
                        )
                    {
                        return Err(vm.err_type(
                            "Class extends value is not a constructor or null"
                                .to_string()
                                .into(),
                        ));
                    }
                } else if value
                    .as_string(&vm.gc_heap)
                    .is_some_and(|string| string.to_lossy_string(&vm.gc_heap) == "prototype")
                {
                    return Err(vm.err_type(
                        "Classes may not have a static property named 'prototype'"
                            .to_string()
                            .into(),
                    ));
                }
                Ok(())
            }
            ClassRuntimeOp::SetFunctionName {
                function,
                key,
                prefix_index,
            } => {
                let callee = self.read(function)?;
                let key_value = self.read(key)?;
                let context = unsafe { self.context.as_ref() };
                let prefix = context
                    .property_atom_for_function(self.function_id(), prefix_index)
                    .map(|atom| atom.name().to_string())
                    .unwrap_or_default();
                let mut name = if let Some(symbol) = key_value.as_symbol(&vm.gc_heap) {
                    match symbol.description() {
                        Some(description) => {
                            format!("[{}]", description.to_lossy_string(&vm.gc_heap))
                        }
                        None => String::new(),
                    }
                } else {
                    key_value.display_string(&vm.gc_heap)
                };
                if !prefix.is_empty() {
                    name = format!("{prefix} {name}");
                }
                let callee = match callee.as_class_constructor() {
                    Some(class) => class.ctor(&vm.gc_heap),
                    None => callee,
                };
                if let Some(function_id) = callee.as_function().or_else(|| {
                    callee
                        .as_closure(&vm.gc_heap)
                        .map(|closure| closure.cached_function_id)
                }) {
                    vm.with_handle_scope(|vm, scope| {
                        let callee = vm.scoped_value(scope, callee);
                        let name = vm.scoped_string(scope, &name)?;
                        let callee = vm.escape_scoped(callee);
                        let owner = callee.as_closure(&vm.gc_heap);
                        let descriptor = object::PropertyDescriptor {
                            kind: object::DescriptorKind::Data {
                                value: vm.escape_scoped(name),
                            },
                            flags: object::PropertyFlags::new(false, false, true),
                        };
                        vm.ordinary_function_define_own_property(
                            unsafe { &mut *self.stack.as_ptr() },
                            Some(context),
                            owner,
                            function_id,
                            "name",
                            None,
                            descriptor,
                        )?;
                        Ok::<(), VmError>(())
                    })?;
                }
                Ok(())
            }
        }
    }
}
