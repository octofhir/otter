//! Shape-guarded direct-method call-site cache.
//!
//! # Contents
//! - Cache records for own-slot and direct-prototype method loads.
//! - Guarded cache-hit preparation through the shared call-target resolver.
//! - Cache installation, polymorphic-way budgeting, and targeted invalidation.
//!
//! # Invariants
//! - Cache records contain only immortal shape/slot metadata, a function id,
//!   scalar entry plan, invalidation epoch, and the exact owning code `Arc`.
//!   They never retain a raw or movable callable/closure value.
//! - Every hit reloads the method slot and decodes that fresh callable, so
//!   closure upvalues and effective `this` belong to the current invocation.
//! - A cached plan is used only at its installation epoch and travels with the
//!   exact code owner from which it was derived.
//! - Cache misses and stale plans are semantic no-ops: the caller re-enters the
//!   normal IC resolution path and may safely replace the matching way.
//!
//! # See also
//! - [`super::resolve`] for dynamic callable decoding and eligibility policy.
//! - [`super::frame`] for frameless owner publication and code pinning.

use std::sync::Arc;

use crate::*;

/// One guarded receiver-shape way in a direct-method call-site cache.
#[derive(Clone)]
pub(crate) struct JitDirectMethodCache {
    hit: crate::interp::MethodLoadHit,
    function_id: u32,
    /// Strong owner for the exact executable entry captured in `plan`.
    code: Arc<dyn jit::JitFunctionCode>,
    /// Entry and frame-layout metadata frozen at installation.
    plan: jit::JitDirectCallPlan,
    /// Registry invalidation epoch at installation.
    plan_epoch: u64,
}

impl JitDirectMethodCache {
    /// The receiver shape id this cached way keys on.
    fn cached_shape_id(&self) -> crate::object::ShapeId {
        self.hit.receiver_shape_id()
    }

    /// Exact code owner retained by this cache way.
    pub(crate) fn code(&self) -> &Arc<dyn jit::JitFunctionCode> {
        &self.code
    }
}

/// Maximum receiver shapes cached per direct-method call site before it is
/// left to the generic path.
const MAX_DIRECT_METHOD_WAYS: usize = jit::JIT_DIRECT_METHOD_WAYS;

impl Interpreter {
    /// Drop only cached direct-method ways whose callee is `fid`.
    pub(crate) fn clear_jit_direct_method_cache_for_fid(&mut self, fid: u32) {
        for set in &mut self.jit_direct_method_cache {
            set.retain(|cache| cache.function_id != fid);
        }
    }

    /// Reload the callable guarded by one shape/slot cache hit.
    fn cached_direct_method_value(
        &self,
        recv: crate::object::JsObject,
        hit: &crate::interp::MethodLoadHit,
    ) -> Option<Value> {
        hit.reload(recv, &self.gc_heap)
    }

    /// Try a guarded cache way, then feed its freshly loaded callable through
    /// the same resolver and frameless owner transaction as an uncached call.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn try_prepare_cached_direct_method_call(
        &mut self,
        context: &ExecutionContext,
        recv: Value,
        obj: crate::object::JsObject,
        site: usize,
        arg_regs: &[u16],
        caller_regs: *const Value,
    ) -> Result<Option<jit::JitPreparedDirectCall>, VmError> {
        // A matching way contributes only code-selection state. The callable
        // comes from this invocation's guarded slot load and is decoded below.
        let Some(set) = self.jit_direct_method_cache.get(site) else {
            return Ok(None);
        };
        let current_epoch = self.jit_code_registry.invalidation_epoch();
        let mut resolved = None;
        for (way, cache) in set.iter().enumerate() {
            let Some(method) = self.cached_direct_method_value(obj, &cache.hit) else {
                continue;
            };
            if cache.plan_epoch != current_epoch {
                return Ok(None);
            }
            resolved = Some((
                way,
                cache.cached_shape_id(),
                method,
                cache.function_id,
                cache.plan,
                cache.code.clone(),
            ));
            break;
        }
        let Some((way, shape_id, method, function_id, plan, code)) = resolved else {
            return Ok(None);
        };
        let Some(target) = self.resolve_cached_jit_call_target(
            context,
            method,
            recv,
            function_id,
            plan,
            code.clone(),
        ) else {
            return Ok(None);
        };
        let target_code = target
            .code
            .as_ref()
            .expect("method resolution retains its exact code owner");
        if !Arc::ptr_eq(&code, target_code) {
            let promoted_code = target_code.clone();
            if let Some(cache) = self
                .jit_direct_method_cache
                .get_mut(site)
                .and_then(|set| set.get_mut(way))
                && cache.cached_shape_id() == shape_id
                && cache.function_id == function_id
                && cache.plan_epoch == current_epoch
            {
                cache.code = promoted_code;
                cache.plan = target.plan;
            }
        }
        self.prepare_jit_resolved_call(target, arg_regs, caller_regs)
            .map(Some)
    }

    /// Install or replace one guarded direct-method cache way.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn install_jit_direct_method_cache(
        &mut self,
        site: usize,
        obj: crate::object::JsObject,
        key: crate::property_atom::AtomizedPropertyKey<'_>,
        method: Value,
        function_id: u32,
        code: Arc<dyn jit::JitFunctionCode>,
        plan: jit::JitDirectCallPlan,
    ) {
        let method_is_plain_function = method == Value::function(function_id);
        // A closure method only caches at a monomorphic property-load site.
        // Plain functions retain the existing mono/poly cache behavior.
        if !method_is_plain_function
            && self
                .feedback_directory
                .property_entry_count(site, crate::property_ic::PropertyIcKind::Load)
                != Some(1)
        {
            return;
        }
        let method_fid = method.as_function().or_else(|| {
            method
                .as_closure(&self.gc_heap)
                .map(|closure| closure.function_id())
        });
        if method_fid != Some(function_id) {
            return;
        }

        // A saturated site with a new receiver shape cannot benefit from a
        // shape walk because the resulting way would be dropped.
        let recv_shape_id = object::shape_id(obj, &self.gc_heap);
        if let Some(set) = self.jit_direct_method_cache.get(site)
            && set.len() >= MAX_DIRECT_METHOD_WAYS
            && !set
                .iter()
                .any(|cache| cache.cached_shape_id() == recv_shape_id)
        {
            return;
        }
        let Some(is_megamorphic) = self
            .feedback_directory
            .property_is_megamorphic(site, crate::property_ic::PropertyIcKind::Load)
        else {
            return;
        };
        if is_megamorphic {
            return;
        }

        let cached_hit =
            self.feedback_directory
                .method_load_hit(site, obj, &self.gc_heap, key, method);

        if let Some(hit) = cached_hit {
            let new_shape = hit.receiver_shape_id();
            if let Some(set) = self.jit_direct_method_cache.get_mut(site) {
                let entry = JitDirectMethodCache {
                    hit,
                    function_id,
                    code,
                    plan,
                    plan_epoch: self.jit_code_registry.invalidation_epoch(),
                };
                // A same-shape entry may have been reassigned; replace it in
                // place. Otherwise append while the site remains within budget.
                let position = set
                    .iter()
                    .position(|cache| cache.cached_shape_id() == new_shape);
                match position {
                    Some(index) => set[index] = entry,
                    None if set.len() < MAX_DIRECT_METHOD_WAYS => set.push(entry),
                    None => {}
                }
            }
        }
    }
}
