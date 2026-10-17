//! Canonical value-level prototype changes and ordinary cycle validation.
//!
//! # Contents
//! - Scoped `[[SetPrototypeOf]]` for Proxy and every ordinary receiver family.
//! - One cycle walk over ordinary `[[GetPrototypeOf]]` storage.
//! - Atomic class constructor and statics-link publication.
//!
//! # Invariants
//! - Target and requested prototype remain rooted through allocation and traps.
//!   Proxy invariants use the target captured before handler lookup, including
//!   when a handler getter or trap revokes the Proxy.
//! - SameValue no-ops precede extensibility and cycle checks. An ordinary cycle
//!   refuses publication; Proxy and namespace prototype methods stop the walk
//!   without invoking their internal methods or running user code.
//! - An explicit Array null prototype is a stored override. Only an absent
//!   override selects the implicit default-realm prototype.
//! - Class constructor state changes only after its statics-link change succeeds.
//!
//! # See also
//! - `super::descriptors` owns GetPrototypeOf and IsExtensible.
//! - `crate::interp::protos` owns per-family prototype storage.
//! - `crate::interp::prototype_shapes` owns ordinary shape publication.

use crate::activation_stack::ActivationStack;
use crate::handles::HandleScope;
use crate::native_abi::CommittedValueError;
use crate::{ExecutionContext, Interpreter, Local, Value, VmError, abstract_ops, array, object};
use smallvec::SmallVec;

impl Interpreter {
    /// §10.1.2.1: walk only ordinary prototype links, without Proxy traps.
    fn ordinary_prototype_chain_avoids(
        &mut self,
        scope: &HandleScope,
        target: Local<'_>,
        prototype: Local<'_>,
    ) -> Result<bool, VmError> {
        let cursor = self.scoped_value(scope, self.escape_scoped(prototype));
        let mut seen: Vec<Local<'_>> = Vec::new();
        loop {
            let current = self.escape_scoped(cursor);
            if current.is_null() {
                return Ok(true);
            }
            if abstract_ops::same_value(&current, &self.escape_scoped(target), &self.gc_heap) {
                return Ok(false);
            }
            if current.is_proxy() {
                return Ok(true);
            }
            if let Some(object) = current.as_object()
                && (object::deferred_namespace_target(object, &self.gc_heap).is_some()
                    || object::module_namespace_env(object, &self.gc_heap).is_some())
            {
                return Ok(true);
            }
            if !current.is_object_type() {
                return Err(VmError::InvalidOperand);
            }
            if seen.iter().any(|previous| {
                abstract_ops::same_value(&current, &self.escape_scoped(*previous), &self.gc_heap)
            }) {
                return Ok(false);
            }
            seen.push(self.scoped_value(scope, current));
            let next = self.get_prototype_for_op(&current)?;
            self.set_scoped(cursor, next);
        }
    }

    /// §10.5.2 / §10.1.2: one value-level `[[SetPrototypeOf]]` owner.
    pub(crate) fn set_prototype_value_proxy_aware(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        target: &Value,
        proto: &Value,
    ) -> Result<bool, CommittedValueError> {
        // A new [[Prototype]] for %Array.prototype% puts objects the array
        // index protector never watched into every plain array's chain.
        if self.is_realm_element_prototype(*target) {
            self.activate_array_index_accessor_protector();
        }
        self.with_handle_scope(|interp, scope| {
            let target = interp.scoped_value(scope, *target);
            let proto = interp.scoped_value(scope, *proto);
            let current_target = interp.escape_scoped(target);
            // Namespace prototypes are immutable even before deferred export
            // population. Their SetImmutablePrototype performs no lookup.
            if let Some(object) = current_target.as_object()
                && (object::deferred_namespace_target(object, &interp.gc_heap).is_some()
                    || object::module_namespace_env(object, &interp.gc_heap).is_some())
            {
                return Ok(interp.escape_scoped(proto).is_null());
            }
            if let Some(proxy) = current_target.as_proxy() {
                if proxy.is_revoked(&interp.gc_heap) {
                    return Err(CommittedValueError::JavaScript(interp.err_type(
                        "Cannot perform 'setPrototypeOf' on a proxy that has been revoked"
                            .into(),
                    )));
                }
                let captured_target = interp.scoped_value(scope, proxy.target(&interp.gc_heap));
                let args: SmallVec<[Value; 8]> = smallvec::smallvec![
                    interp.escape_scoped(captured_target),
                    interp.escape_scoped(proto)
                ];
                return match interp.invoke_proxy_trap(
                    stack,
                    Some(context),
                    &proxy,
                    "setPrototypeOf",
                    args,
                )? {
                    super::ProxyTrap::Trapped(result) => {
                        if !result.to_boolean(&interp.gc_heap) {
                            return Ok(false);
                        }
                        let extensible = interp.is_extensible_value(
                            stack,
                            Some(context),
                            &interp.escape_scoped(captured_target),
                        )?;
                        if !extensible {
                            let actual = interp.ordinary_get_prototype_value(
                                stack,
                                context,
                                interp.escape_scoped(captured_target),
                                0,
                            )?;
                            if !abstract_ops::same_value(
                                &interp.escape_scoped(proto),
                                &actual,
                                &interp.gc_heap,
                            ) {
                                return Err(CommittedValueError::JavaScript(interp.err_type(
                                    "Proxy setPrototypeOf invariant violated: target is non-extensible and prototypes differ"
                                        .into(),
                                )));
                            }
                        }
                        Ok(true)
                    }
                    super::ProxyTrap::NoTrap {
                        target: fallthrough_target,
                    } => interp.set_prototype_value_proxy_aware(
                        stack,
                        context,
                        &fallthrough_target,
                        &interp.escape_scoped(proto),
                    ),
                };
            }
            let current = interp
                .get_prototype_for_op(&current_target)
                .map_err(CommittedValueError::JavaScript)?;
            if abstract_ops::same_value(&interp.escape_scoped(proto), &current, &interp.gc_heap) {
                return Ok(true);
            }
            let current_target = interp.escape_scoped(target);
            if current_target.as_object().is_some_and(|object| {
                interp.object_prototype_object_opt() == Some(object)
            }) || !interp.is_extensible_non_proxy(&current_target)
            {
                return Ok(false);
            }
            if !interp
                .ordinary_prototype_chain_avoids(scope, target, proto)
                .map_err(CommittedValueError::JavaScript)?
            {
                return Ok(false);
            }
            let current_target = interp.escape_scoped(target);
            let current_proto = interp.escape_scoped(proto);
            if let Some(class) = current_target.as_class_constructor() {
                let statics = interp.scoped_value(scope, Value::object(class.statics(&interp.gc_heap)));
                let accepted = interp.set_prototype_value_proxy_aware(
                    stack,
                    context,
                    &interp.escape_scoped(statics),
                    &interp.escape_scoped(proto),
                )?;
                if accepted {
                    let class = interp
                        .escape_scoped(target)
                        .as_class_constructor()
                        .ok_or(CommittedValueError::Fatal(VmError::InvalidOperand))?;
                    let current_proto = interp.escape_scoped(proto);
                    class.set_ctor_proto(&mut interp.gc_heap, current_proto);
                }
                return Ok(accepted);
            }
            if let Some(mut object) = current_target.as_object() {
                let prototype = (!current_proto.is_null()).then_some(current_proto);
                return interp
                    .set_ordinary_prototype(&mut object, prototype)
                    .map_err(CommittedValueError::JavaScript);
            }
            if let Some(array) = current_target.as_array() {
                if !current_proto.is_null() && !current_proto.is_object_type() {
                    return Ok(false);
                }
                array::set_prototype_override(array, &mut interp.gc_heap, Some(current_proto))
                    .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
                return Ok(true);
            }
            let owner = current_target.as_closure(&interp.gc_heap);
            if current_target
                .as_function()
                .or_else(|| owner.map(|closure| closure.cached_function_id))
                .is_some()
            {
                interp.set_function_prototype_override(&current_target, Some(current_proto));
                return Ok(true);
            }
            if let Some(bound) = current_target.as_bound_function() {
                bound.set_prototype_override(&mut interp.gc_heap, current_proto);
                return Ok(true);
            }
            Ok(interp.set_exotic_prototype_override(&current_target, current_proto))
        })
    }
}
