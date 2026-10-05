//! Exact source and realm admission for isolate-owned jobs.
//!
//! # Contents
//! - Exact bytecode FunctionID-to-CodeSpace context resolution.
//! - No-allocation GetFunctionRealm selection for direct, bound and Proxy callbacks.
//! - Creation-realm stamping of completed native functions.
//!
//! # Invariants
//! These reads never allocate, invoke JavaScript or retain raw values across a
//! collecting boundary. A missing bytecode owner is structural failure. Native
//! jobs may have no source context. A reaction without a handler explicitly
//! chooses its registration realm; a revoked Proxy uses that same normative
//! reaction fallback. Neither choice depends on the future drain's ambient state.
//!
//! # See also
//! - [`super::Microtask`] owns the admitted realm and async-context roots.
//! - [`crate::execution_context::ExecutionContext`] owns chunk/source identity.
//! - <https://tc39.es/ecma262/#sec-getfunctionrealm>

use crate::{ExecutionContext, Interpreter, Value, VmError};

impl Interpreter {
    /// Resolve the exact linked source owner, preserving an admitted chunk's
    /// local fast path while permitting native-to-bytecode calls without one.
    pub(crate) fn function_context(
        &self,
        admitted: Option<&ExecutionContext>,
        function_id: u32,
    ) -> Result<ExecutionContext, VmError> {
        match admitted {
            Some(context) => context
                .for_function(function_id)
                .map(|owner| (*owner).clone()),
            None => ExecutionContext::for_function_in(&self.code_space, function_id),
        }
        .map_err(|_| VmError::InvalidOperand)
    }

    /// Admit the defining bytecode chunk when present. Native-only work keeps
    /// the caller's optional source and never invents a realm context.
    pub(crate) fn callable_context(
        &self,
        admitted: Option<&ExecutionContext>,
        mut callable: Value,
    ) -> Result<Option<ExecutionContext>, VmError> {
        loop {
            if let Some(bound) = callable.as_bound_function() {
                callable = self.gc_heap.read_payload(bound.inner, |body| body.target);
                continue;
            }
            if let Some(proxy) = callable.as_proxy() {
                if proxy.is_revoked(&self.gc_heap) {
                    return Ok(admitted.cloned());
                }
                callable = proxy.target(&self.gc_heap);
                continue;
            }
            if let Some(class) = callable.as_class_constructor() {
                callable = class.ctor(&self.gc_heap);
                continue;
            }
            let function_id = callable.as_function().or_else(|| {
                callable
                    .as_closure(&self.gc_heap)
                    .map(|closure| closure.cached_function_id)
            });
            return match function_id {
                Some(function_id) => self.function_context(admitted, function_id).map(Some),
                None => Ok(admitted.cloned()),
            };
        }
    }

    /// GetFunctionRealm for a callable reaction handler. The explicit fallback
    /// is used only for absent handlers or the spec's abrupt GetFunctionRealm
    /// case (revoked Proxy), never as a general source-context fallback.
    pub(crate) fn reaction_realm(
        &self,
        handler: Option<Value>,
        registration_realm: u32,
    ) -> Result<u32, VmError> {
        let Some(mut callable) = handler else {
            return Ok(registration_realm);
        };
        loop {
            if let Some(bound) = callable.as_bound_function() {
                callable = self.gc_heap.read_payload(bound.inner, |body| body.target);
                continue;
            }
            if let Some(proxy) = callable.as_proxy() {
                if proxy.is_revoked(&self.gc_heap) {
                    return Ok(registration_realm);
                }
                callable = proxy.target(&self.gc_heap);
                continue;
            }
            if let Some(class) = callable.as_class_constructor() {
                callable = class.ctor(&self.gc_heap);
                continue;
            }
            if let Some(function_id) = callable.as_function().or_else(|| {
                callable
                    .as_closure(&self.gc_heap)
                    .map(|closure| closure.cached_function_id)
            }) {
                self.function_context(None, function_id)?;
                return Ok(self
                    .function_realm_ids
                    .get(&function_id)
                    .copied()
                    .unwrap_or(0));
            }
            let native = callable
                .as_native_function()
                .or_else(|| {
                    callable.as_object().and_then(|object| {
                        crate::object::call_native(object, &self.gc_heap)
                            .and_then(|value| value.as_native_function())
                    })
                })
                .ok_or(VmError::InvalidOperand)?;
            let Some(global) = native.realm_global(&self.gc_heap) else {
                return Ok(0);
            };
            if global == self.global_this {
                return Ok(self.active_realm_id);
            }
            return self
                .extra_realms
                .iter()
                .find(|realm| realm.global_this == global)
                .map(|realm| realm.id)
                .ok_or(VmError::InvalidOperand);
        }
    }

    /// Stamp the fully initialized native using the current rooted realm
    /// global. The allocation has completed; this publication cannot collect.
    pub(crate) fn stamp_native_creation_realm(&mut self, value: Value) -> Value {
        if self.active_realm_is_extra {
            let native = value
                .as_native_function()
                .expect("native allocation returns native value");
            native.set_realm_global(&mut self.gc_heap, Some(self.global_this));
        }
        value
    }

    /// Admit both handlers before mutably borrowing the promise body. Missing
    /// handlers explicitly use registration realm; actual callable handlers
    /// use their own realm, including bound and Proxy targets.
    pub(crate) fn register_promise_reactions(
        &mut self,
        promise: crate::promise::JsPromiseHandle,
        on_fulfilled: Option<Value>,
        on_rejected: Option<Value>,
        capability: crate::promise::PromiseCapability,
        context: Option<ExecutionContext>,
    ) -> Result<crate::promise::PromiseThenOutcome, VmError> {
        use crate::promise::JsPromise;
        let registration_realm = self.active_realm_id;
        let fulfill_realm = self.reaction_realm(on_fulfilled, registration_realm)?;
        let reject_realm = self.reaction_realm(on_rejected, registration_realm)?;
        let async_context = self.async_context();
        Ok(promise.perform_then_with_context(
            &mut self.gc_heap,
            on_fulfilled,
            on_rejected,
            capability,
            context,
            async_context,
            fulfill_realm,
            reject_realm,
        ))
    }

    pub(crate) fn job_realm_is_live(&self, realm_id: u32) -> bool {
        realm_id == self.active_realm_id
            || self.extra_realms.iter().any(|realm| realm.id == realm_id)
    }
}

#[cfg(test)]
mod tests;
