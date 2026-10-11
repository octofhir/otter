//! Scoped construction of source-owned frozen template arrays.
//!
//! # Contents
//! - Cooked/raw template strings allocated through native handle scopes.
//! - The ordinary expando descriptor for `.raw` and shared Array integrity.
//!
//! # Invariants
//! Every string and array remains rooted across later allocations. The source
//! function owns both the template site and realm; disposal has no ambient
//! fallback. `.raw` is non-enumerable, non-writable and non-configurable, and
//! both arrays are frozen before the cooked array becomes observable.
//!
//! # See also
//! - `jit_class_value_ops` owns template-site caching and register publication.
//! - `object_internal_ops::array_define` owns Array descriptors and integrity.

use crate::{ExecutionContext, Interpreter, Value, VmError, VmPropertyKey, object};

impl Interpreter {
    /// §13.2.8.4 GetTemplateObject steps 7–15.
    pub(crate) fn build_template_object(
        &mut self,
        context: &ExecutionContext,
        function_id: u32,
        site_idx: u32,
    ) -> Result<Value, VmError> {
        let site = context
            .template_site_for_function(function_id, site_idx)
            .ok_or(VmError::InvalidOperand)?
            .clone();
        let build = |vm: &mut Self| {
            vm.with_handle_scope(|vm, scope| {
                let cooked = site
                    .cooked
                    .iter()
                    .map(|entry| match entry {
                        Some(text) => vm.scoped_string(scope, text),
                        None => Ok(vm.scoped_value(scope, Value::undefined())),
                    })
                    .collect::<Result<Vec<_>, VmError>>()?;
                let raw = site
                    .raw
                    .iter()
                    .map(|text| vm.scoped_string(scope, text))
                    .collect::<Result<Vec<_>, VmError>>()?;
                let raw = vm.scoped_array_from(scope, &raw)?;
                let strings = vm.scoped_array_from(scope, &cooked)?;
                let array = vm
                    .escape_scoped(strings)
                    .as_array()
                    .ok_or(VmError::InvalidOperand)?;
                if !vm.define_array_expando_property(
                    array,
                    &VmPropertyKey::String("raw"),
                    object::PartialPropertyDescriptor {
                        value: Some(vm.escape_scoped(raw)),
                        writable: Some(false),
                        enumerable: Some(false),
                        configurable: Some(false),
                        ..Default::default()
                    },
                )? {
                    return Err(VmError::InvalidOperand);
                }
                let array = vm
                    .escape_scoped(raw)
                    .as_array()
                    .ok_or(VmError::InvalidOperand)?;
                vm.set_array_integrity_level(array, true)?;
                let array = vm
                    .escape_scoped(strings)
                    .as_array()
                    .ok_or(VmError::InvalidOperand)?;
                vm.set_array_integrity_level(array, true)?;
                Ok(vm.escape_scoped(strings))
            })
        };
        match self.foreign_function_realm(function_id) {
            Some(realm) => self.with_host_realm_id(realm, build),
            None => build(self),
        }
    }
}
