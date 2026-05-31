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
        let referrer: String = context
            .exec_function(frame.function_id)
            .map(|f| f.module_url.as_ref().to_string())
            .unwrap_or_default();
        let namespace = self
            .resolve_module_namespace(context, referrer.as_str(), specifier)
            .ok_or_else(|| VmError::UnknownIntrinsic {
                name: format!("import \"{specifier}\""),
            })?;
        write_register(frame, dst, Value::object(namespace))?;
        frame.advance_pc(self.current_byte_len)?;
        Ok(())
    }

    /// `Op::ImportNamespaceDeferred` — resolve (or lazily create) the
    /// deferred namespace object for `specifier` and write it to `dst`.
    /// The target module is **not** evaluated here (TC39 import defer).
    pub(crate) fn run_import_namespace_deferred_reg(
        &mut self,
        context: &ExecutionContext,
        frame: &mut Frame,
        dst: u16,
        spec_idx: u32,
    ) -> Result<(), VmError> {
        let specifier = context
            .string_constant_str(spec_idx)
            .ok_or(VmError::InvalidOperand)?
            .to_string();
        let referrer: String = context
            .exec_function(frame.function_id)
            .map(|f| f.module_url.as_ref().to_string())
            .unwrap_or_default();
        let target = context
            .module_resolution_target(referrer.as_str(), specifier.as_str())
            .ok_or_else(|| VmError::UnknownIntrinsic {
                name: format!("import defer \"{specifier}\""),
            })?
            .to_string();
        let target_url: std::sync::Arc<str> = std::sync::Arc::from(target.as_str());
        let ns = self.get_or_create_deferred_namespace(target_url)?;
        write_register(frame, dst, Value::object(ns))?;
        frame.advance_pc(self.current_byte_len)?;
        Ok(())
    }

    /// `Op::EvaluateModule` — evaluate the module named by the constant
    /// operand and its non-deferred dependency closure (idempotent).
    pub(crate) fn run_evaluate_module_const(
        &mut self,
        context: &ExecutionContext,
        frame: &mut Frame,
        url_idx: u32,
    ) -> Result<(), VmError> {
        let url = context
            .string_constant_str(url_idx)
            .ok_or(VmError::InvalidOperand)?
            .to_string();
        self.evaluate_module_rec(context, &url)?;
        frame.advance_pc(self.current_byte_len)?;
        Ok(())
    }

    /// Evaluate `url`'s `<module-init>` body, after evaluating its
    /// not-yet-run non-deferred dependencies in post-order. Idempotent:
    /// a module already evaluated (or mid-evaluation, for cycles) is a
    /// no-op. Mirrors §16.2.1.5 InnerModuleEvaluation restricted to the
    /// synchronous, non-deferred sub-graph.
    pub(crate) fn evaluate_module_rec(
        &mut self,
        context: &ExecutionContext,
        url: &str,
    ) -> Result<(), VmError> {
        let url_arc: std::sync::Arc<str> = std::sync::Arc::from(url);
        if self.evaluated_modules.contains(&url_arc) {
            return Ok(());
        }
        // §28.3 ReadyForSyncExecution — a module with top-level await
        // cannot be force-evaluated synchronously from a deferred
        // namespace access; that is a TypeError, not a silent suspension.
        if context
            .module_init_function_id(url)
            .and_then(|fid| context.function(fid))
            .is_some_and(|f| f.is_async)
        {
            return Err(VmError::TypeError {
                message:
                    "Cannot synchronously evaluate a deferred module that uses top-level await"
                        .to_string(),
            });
        }
        // Mark before running so an import cycle treats this module as
        // already evaluating (no re-entry).
        self.evaluated_modules.insert(url_arc.clone());

        let deps: Vec<String> = context
            .eager_dep_targets(url)
            .into_iter()
            .map(str::to_string)
            .collect();
        for dep in deps {
            self.evaluate_module_rec(context, &dep)?;
        }

        let Some(function_id) = context.module_init_function_id(url) else {
            return Ok(());
        };
        let Some(env) = self.module_environments.get(&url_arc).copied() else {
            return Ok(());
        };
        let meta = self.build_import_meta(url)?;
        let init = Value::function(function_id);
        let args: SmallVec<[Value; 8]> = smallvec::smallvec![Value::object(env), meta];
        self.run_callable_sync(context, &init, Value::undefined(), args)?;
        Ok(())
    }

    /// Build a module's `import.meta` object with its `url` property.
    fn build_import_meta(&mut self, url: &str) -> Result<Value, VmError> {
        let obj = self.alloc_host_object_with_roots(&[], &[])?;
        let url_str =
            JsString::from_str(url, &mut self.gc_heap).map_err(|_| VmError::TypeMismatch)?;
        crate::object::set(obj, &mut self.gc_heap, "url", Value::string(url_str));
        Ok(Value::object(obj))
    }

    /// Deferred namespace object for `target_url`, created on first use
    /// and cached so repeated `import defer` of the same module yield
    /// the identical object.
    fn get_or_create_deferred_namespace(
        &mut self,
        target_url: std::sync::Arc<str>,
    ) -> Result<crate::object::JsObject, VmError> {
        if let Some(obj) = self.deferred_namespaces.get(&target_url) {
            return Ok(*obj);
        }
        let obj = self.alloc_deferred_namespace_object(target_url.clone())?;
        self.deferred_namespaces.insert(target_url, obj);
        Ok(obj)
    }

    /// Ensure a deferred module namespace is ready for an operation.
    ///
    /// When `value` is a deferred namespace and `trigger` is `true`, the
    /// wrapped module is evaluated and its export data properties are
    /// installed (idempotent). `trigger` is `false` for symbol-like keys
    /// (symbols and `"then"`), which §28.3 reads without forcing
    /// evaluation. A no-op for ordinary values, so callers fall through
    /// to normal handling on the same object afterward.
    pub(crate) fn ensure_deferred_namespace_ready(
        &mut self,
        context: &ExecutionContext,
        value: &Value,
        trigger: bool,
    ) -> Result<(), VmError> {
        let Some(obj) = value.as_object() else {
            return Ok(());
        };
        let Some(target_url) = crate::object::deferred_namespace_target(obj, &self.gc_heap) else {
            return Ok(());
        };
        if !trigger || crate::object::deferred_namespace_is_populated(obj, &self.gc_heap) {
            return Ok(());
        }
        self.evaluate_module_rec(context, &target_url)?;
        if let Some(env) = self.module_environments.get(&target_url).copied() {
            let env_value = Value::object(env);
            let mut names = self.enumerable_own_string_keys_for_value(context, env_value, 0)?;
            names.sort();
            for name in &names {
                let key = crate::VmPropertyKey::String(name);
                let val = match self.ordinary_get_value(context, env_value, env_value, &key, 0)? {
                    crate::VmGetOutcome::Value(v) => v,
                    crate::VmGetOutcome::InvokeGetter { getter } => {
                        self.run_callable_sync(context, &getter, env_value, SmallVec::new())?
                    }
                };
                // §28.3 namespace export properties: writable, enumerable,
                // non-configurable.
                crate::object::define_own_property(
                    obj,
                    &mut self.gc_heap,
                    name,
                    crate::object::PropertyDescriptor::data(val, true, true, false),
                );
            }
            crate::object::prevent_extensions(obj, &mut self.gc_heap);
        }
        crate::object::set_deferred_namespace_populated(obj, &self.gc_heap);
        Ok(())
    }

    /// `true` when `key` is symbol-like for a deferred namespace: a
    /// Symbol, or the string `"then"` (§28.3 IsSymbolLikeNamespaceKey).
    /// Reads of such keys do not trigger module evaluation.
    pub(crate) fn deferred_key_is_symbol_like(key: &crate::VmPropertyKey<'_>) -> bool {
        match key.string_name() {
            Some(name) => name == "then",
            None => true,
        }
    }

    pub(crate) fn run_import_meta_resolve_regs(
        &mut self,
        context: &ExecutionContext,
        frame: &mut Frame,
        dst: u16,
        spec_reg: u16,
    ) -> Result<(), VmError> {
        let spec_value = *read_register(frame, spec_reg)?;
        let specifier = spec_value
            .as_string(&self.gc_heap)
            .ok_or(VmError::TypeMismatch)?
            .to_lossy_string(&self.gc_heap);
        let referrer: Option<&str> = context
            .exec_function(frame.function_id)
            .map(|f| f.module_url.as_ref());
        let resolved = resolve_relative_url(referrer, &specifier);
        let resolved_str =
            JsString::from_str(&resolved, &mut self.gc_heap).map_err(|_| VmError::TypeMismatch)?;
        write_register(frame, dst, Value::string(resolved_str))?;
        frame.advance_pc(self.current_byte_len)?;
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
        let spec_value = *read_register(&stack[top_idx], spec_reg)?;
        let referrer: String = context
            .exec_function(stack[top_idx].function_id)
            .map(|f| f.module_url.as_ref().to_string())
            .unwrap_or_default();
        let import_context = context.clone();
        let promise =
            if let Some(s) = spec_value.as_string(&self.gc_heap) {
                let specifier = s.to_lossy_string(&self.gc_heap);
                if let Some(ns) =
                    self.resolve_module_namespace(context, referrer.as_str(), &specifier)
                {
                    let namespace_value = Value::object(ns);
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
                    loader.schedule(token, specifier, referrer.clone());
                    pending
                } else {
                    let reason = self.make_type_error_with_stack_roots(
                        stack,
                        &format!("dynamic import: module not resolvable: \"{specifier}\""),
                    )?;
                    promise_dispatch::PromiseBuilder::with_context(import_context.clone())
                        .rejected_stack_rooted(self, stack, reason, &[], &[])?
                }
            } else {
                let reason = self.make_type_error_with_stack_roots(
                    stack,
                    "dynamic import: specifier must be a string",
                )?;
                promise_dispatch::PromiseBuilder::with_context(import_context)
                    .rejected_stack_rooted(self, stack, reason, &[], &[])?
            };
        write_register(&mut stack[top_idx], dst, Value::promise(promise))?;
        stack[top_idx].advance_pc(self.current_byte_len)?;
        Ok(())
    }
}
