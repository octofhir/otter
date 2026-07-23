//! ECMA-262 §28.1 `Reflect` object — full static surface.
//!
//! Two entry points share one implementation:
//!
//! - [`call`] — typed-dispatch fast path reached from
//!   [`crate::otter_bytecode::Op::ReflectCall`] when the compiler can
//!   prove the receiver is the global `Reflect`. Skips property lookup
//!   and argument-vector allocation.
//! - [`REFLECT_SPEC`] — static [`crate::js_surface::NamespaceSpec`]
//!   installed at bootstrap. Exposes every method as a JS-visible
//!   own property on the `Reflect` namespace object so user code that
//!   extracts a builtin (`const get = Reflect.get`), enumerates with
//!   `Object.getOwnPropertyNames(Reflect)`, or checks
//!   `Reflect.get.length` observes the spec-required descriptor shape.
//!   Each method delegates straight back to [`call`].
//!
//! Internal-method dispatch routes through the shared
//! `Interpreter::ordinary_*_value` helpers so the same `[[Get]]`,
//! `[[Set]]`, `[[HasProperty]]`, `[[GetOwnProperty]]`, `[[Delete]]`,
//! and `[[GetPrototypeOf]]` paths cover ordinary objects, arrays,
//! callables, and proxies (which walk handler traps).
//!
//! # Contents
//! - [`call`] — typed dispatcher keyed by [`ReflectMethod`].
//! - [`REFLECT_SPEC`] — namespace spec consumed by bootstrap.
//!
//! # Invariants
//! - Type-of-Object checks accept any heap-bound value
//!   ([`is_type_object`]) so Reflect mirrors the spec's `Object`
//!   type, not just plain objects.
//! - Property-key coercion (§7.1.19 ToPropertyKey) stringifies primitive
//!   keys directly, passes symbols through, and drives the observable
//!   `[[ToPrimitive]]` ladder for object keys.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-reflect-object>

use smallvec::SmallVec;

use crate::ExecutionContext;
use crate::activation_stack::ActivationStack;
use crate::{Interpreter, NativeCtx, NativeError, Value, VmError, VmGetOutcome, VmPropertyKey};

/// Dispatch `Reflect.<name>(args...)`.
///
/// `string_heap` is the runtime heap used for the rare cases
/// where this dispatcher needs to allocate (e.g. `ownKeys` returning
/// a string array). Most paths only forward existing `Value`s and
/// avoid allocation.
///
/// # Errors
/// - [`VmError::TypeMismatch`] for receiver / argument shape errors.
pub fn call(
    interp: &mut Interpreter,
    stack: &mut ActivationStack,
    context: &ExecutionContext,
    method: otter_bytecode::method_id::ReflectMethod,
    args: &[Value],
) -> Result<Value, VmError> {
    use otter_bytecode::method_id::ReflectMethod as M;
    match method {
        // §28.1.2 Reflect.apply(target, thisArgument, argumentsList)
        // <https://tc39.es/ecma262/#sec-reflect.apply>
        M::Apply => {
            let target = args.first().cloned().unwrap_or(Value::undefined());
            if !is_callable(&target, interp.gc_heap()) {
                return Err(VmError::NotCallable);
            }
            let this_value = args.get(1).cloned().unwrap_or(Value::undefined());
            let argv = create_list_from_array_like(interp, stack, context, args.get(2))?;
            interp.run_callable_sync_rooted(stack, context, &target, this_value, argv)
        }
        // §28.1.3 Reflect.construct(target, argumentsList[, newTarget])
        // <https://tc39.es/ecma262/#sec-reflect.construct>
        M::Construct => {
            let target = args.first().cloned().unwrap_or(Value::undefined());
            if !is_constructor(&target, context, interp.gc_heap()) {
                return Err(VmError::NotCallable);
            }
            let new_target = args.get(2).cloned().unwrap_or(target);
            if !is_constructor(&new_target, context, interp.gc_heap()) {
                return Err(VmError::NotCallable);
            }
            let argv = create_list_from_array_like(interp, stack, context, args.get(1))?;
            interp.run_construct_sync_rooted(stack, context, &target, new_target, argv)
        }
        // §28.1.4 Reflect.defineProperty(target, propertyKey, attributes)
        // <https://tc39.es/ecma262/#sec-reflect.defineproperty>
        M::DefineProperty => {
            // Step 1 type check runs before key coercion (observable
            // order), but the key coercion can run user code and move
            // the heap — re-read the target from the rooted argument
            // slot afterwards so the local copy is never stale.
            expect_object_value(args.first())?;
            let key = coerce_property_key(interp, stack, context, args.get(1))?;
            let target = expect_object_value(args.first())?;
            let attributes = args.get(2).cloned().unwrap_or(Value::undefined());
            let descriptor = interp.evaluate_to_property_descriptor(stack, context, &attributes)?;
            let ok = interp.define_own_property_value(stack, context, &target, &key, descriptor)?;
            Ok(Value::boolean(ok))
        }
        // §28.1.5 Reflect.deleteProperty(target, propertyKey)
        // <https://tc39.es/ecma262/#sec-reflect.deleteproperty>
        M::DeleteProperty => {
            // Step 1 type check runs before key coercion (observable
            // order), but the key coercion can run user code and move
            // the heap — re-read the target from the rooted argument
            // slot afterwards so the local copy is never stale.
            expect_object_value(args.first())?;
            let key = coerce_property_key(interp, stack, context, args.get(1))?;
            let target = expect_object_value(args.first())?;
            let removed = interp.ordinary_delete_value(stack, context, target, &key, 0)?;
            Ok(Value::boolean(removed))
        }
        // §28.1.6 Reflect.get(target, propertyKey[, receiver])
        // <https://tc39.es/ecma262/#sec-reflect.get>
        M::Get => {
            // Step 1 type check runs before key coercion (observable
            // order), but the key coercion can run user code and move
            // the heap — re-read the target from the rooted argument
            // slot afterwards so the local copy is never stale.
            expect_object_value(args.first())?;
            let key = coerce_property_key(interp, stack, context, args.get(1))?;
            let target = expect_object_value(args.first())?;
            let receiver = args.get(2).cloned().unwrap_or(target);
            match interp.ordinary_get_value(stack, context, target, receiver, &key, 0)? {
                VmGetOutcome::Value(v) => Ok(v),
                VmGetOutcome::InvokeGetter { getter } => {
                    let argv: SmallVec<[Value; 8]> = SmallVec::new();
                    interp.run_callable_sync_rooted(stack, context, &getter, receiver, argv)
                }
            }
        }
        // §28.1.7 Reflect.getOwnPropertyDescriptor(target, propertyKey)
        // <https://tc39.es/ecma262/#sec-reflect.getownpropertydescriptor>
        M::GetOwnPropertyDescriptor => {
            // Step 1 type check runs before key coercion (observable
            // order), but the key coercion can run user code and move
            // the heap — re-read the target from the rooted argument
            // slot afterwards so the local copy is never stale.
            expect_object_value(args.first())?;
            let key = coerce_property_key(interp, stack, context, args.get(1))?;
            let target = expect_object_value(args.first())?;
            match interp
                .ordinary_get_own_property_descriptor_value(stack, context, target, &key, 0)?
            {
                None => Ok(Value::undefined()),
                Some(desc) => interp.with_handle_scope(|interp, scope| {
                    let result = interp.scoped_descriptor_object(scope, &desc)?;
                    Ok(interp.escape_scoped(result))
                }),
            }
        }
        // §28.1.8 Reflect.getPrototypeOf(target)
        // <https://tc39.es/ecma262/#sec-reflect.getprototypeof>
        M::GetPrototypeOf => {
            let target = expect_object_value(args.first())?;
            interp.ordinary_get_prototype_value(stack, context, target, 0)
        }
        // §28.1.9 Reflect.has(target, propertyKey)
        // <https://tc39.es/ecma262/#sec-reflect.has>
        M::Has => {
            // Step 1 type check runs before key coercion (observable
            // order), but the key coercion can run user code and move
            // the heap — re-read the target from the rooted argument
            // slot afterwards so the local copy is never stale.
            expect_object_value(args.first())?;
            let key = coerce_property_key(interp, stack, context, args.get(1))?;
            let target = expect_object_value(args.first())?;
            let present = interp.ordinary_has_property_value(stack, context, target, &key, 0)?;
            Ok(Value::boolean(present))
        }
        // §28.1.10 Reflect.isExtensible(target)
        // <https://tc39.es/ecma262/#sec-reflect.isextensible>
        M::IsExtensible => {
            let target = expect_object_value(args.first())?;
            let ext = interp.is_extensible_value(stack, context, &target)?;
            Ok(Value::boolean(ext))
        }
        // §28.1.11 Reflect.ownKeys(target)
        // <https://tc39.es/ecma262/#sec-reflect.ownkeys>
        M::OwnKeys => {
            let target = expect_object_value(args.first())?;
            let keys = interp.own_property_keys_value(stack, context, &target)?;
            Ok(Value::array(
                interp.alloc_stack_rooted_array_from_values_with_root_slices(
                    stack,
                    keys,
                    &[&target],
                    &[args],
                )?,
            ))
        }
        // §28.1.12 Reflect.preventExtensions(target)
        // <https://tc39.es/ecma262/#sec-reflect.preventextensions>
        M::PreventExtensions => {
            let target = expect_object_value(args.first())?;
            let ok = interp.prevent_extensions_value(stack, context, &target)?;
            Ok(Value::boolean(ok))
        }
        // §28.1.13 Reflect.set(target, propertyKey, V[, receiver])
        // <https://tc39.es/ecma262/#sec-reflect.set>
        M::Set => {
            // Step 1 type check runs before key coercion (observable
            // order), but the key coercion can run user code and move
            // the heap — re-read the target from the rooted argument
            // slot afterwards so the local copy is never stale.
            expect_object_value(args.first())?;
            let key = coerce_property_key(interp, stack, context, args.get(1))?;
            let target = expect_object_value(args.first())?;
            let value = args.get(2).cloned().unwrap_or(Value::undefined());
            let receiver = args.get(3).cloned().unwrap_or(target);
            // §10.1.9 OrdinarySet with receiver semantics for ordinary
            // object targets. Proxy / non-ordinary targets route
            // through `ordinary_set_data_value` (which dispatches the
            // `set` trap + falls through).
            if let Some(obj) = target.as_object() {
                let outcome = {
                    let heap = interp.gc_heap();
                    match &key {
                        VmPropertyKey::Symbol(sym) => {
                            crate::object::resolve_symbol_set(obj, heap, *sym)
                        }
                        _ => crate::object::resolve_set(
                            obj,
                            heap,
                            key.string_name()
                                .expect("non-symbol key has string spelling"),
                        ),
                    }
                };
                match outcome {
                    crate::object::SetOutcome::InvokeSetter { setter } => {
                        if !is_callable(&setter, interp.gc_heap()) {
                            return Ok(Value::boolean(false));
                        }
                        let argv: SmallVec<[Value; 8]> = smallvec::smallvec![value];
                        interp.run_callable_sync_rooted(stack, context, &setter, receiver, argv)?;
                        return Ok(Value::boolean(true));
                    }
                    crate::object::SetOutcome::Reject { .. } => {
                        return Ok(Value::boolean(false));
                    }
                    crate::object::SetOutcome::ExoticParent { parent } => {
                        let ok = interp.ordinary_set_data_value(
                            stack, context, parent, &key, value, receiver, 1,
                        )?;
                        return Ok(Value::boolean(ok));
                    }
                    crate::object::SetOutcome::AssignData => {
                        // §10.1.9.2 step 2.b — `resolve_set` only walks
                        // ordinary prototype links, so a Proxy in the
                        // chain is invisible to it. When `target` has no
                        // own property for `key` and a proxy sits in the
                        // prototype chain, `[[Set]]` belongs to that
                        // proxy (its `set` trap must run with the
                        // original receiver), not a data write on the
                        // receiver.
                        let target_has_own = interp
                            .ordinary_get_own_property_descriptor_value(
                                stack, context, target, &key, 0,
                            )?
                            .is_some();
                        if !target_has_own
                            && let Some(proxy_proto) =
                                interp.first_proxy_in_prototype_chain(target)?
                        {
                            let ok = interp.ordinary_set_data_value(
                                stack,
                                context,
                                proxy_proto,
                                &key,
                                value,
                                receiver,
                                0,
                            )?;
                            return Ok(Value::boolean(ok));
                        }
                        // §10.1.9 step 4 — data path. Honour receiver:
                        // when target ≠ receiver, the data write lands
                        // on receiver, not target.
                        return Ok(Value::boolean(set_data_on_receiver(
                            interp, stack, context, &target, &key, value, &receiver,
                        )?));
                    }
                }
            }
            let ok =
                interp.ordinary_set_data_value(stack, context, target, &key, value, receiver, 0)?;
            Ok(Value::boolean(ok))
        }
        // §28.1.14 Reflect.setPrototypeOf(target, prototype)
        // <https://tc39.es/ecma262/#sec-reflect.setprototypeof>
        M::SetPrototypeOf => {
            let target = expect_object_value(args.first())?;
            // §28.1.14 step 2 — a missing `proto` is `undefined`, which is
            // neither an Object nor null, so it is a TypeError just like an
            // explicit non-object/non-null argument.
            let proto = match args.get(1).copied().unwrap_or(Value::undefined()) {
                v if v.is_object() || v.is_proxy() || v.is_null() => v,
                _ => return Err(VmError::TypeMismatch),
            };
            let ok = interp.set_prototype_value_proxy_aware(stack, context, &target, &proto)?;
            Ok(Value::boolean(ok))
        }
    }
}

/// §10.1.9 OrdinarySet step 5 — data-property write that honours the
/// `receiver` argument. When `receiver` is not an Object, the write
/// is rejected. When the receiver already owns the property, the
/// write goes through `[[DefineOwnProperty]]` with `{value: V}` only
/// (preserving its existing attributes). Otherwise the write creates
/// a fresh data property on the receiver with the spec-default
/// `{writable: true, enumerable: true, configurable: true}`.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-ordinarysetwithowndescriptor>
fn set_data_on_receiver(
    interp: &mut Interpreter,
    stack: &mut ActivationStack,
    context: &ExecutionContext,
    _target: &Value,
    key: &VmPropertyKey,
    value: Value,
    receiver: &Value,
) -> Result<bool, VmError> {
    let _ = _target;
    interp.ordinary_set_on_receiver(stack, context, key, value, receiver)
}

/// `true` when `value` is a member of the spec type Object — anything
/// allocated on the heap. Mirrors §6.1.7. Primitives (Undefined, Null,
/// Boolean, Number, BigInt, String, Symbol, Hole) return `false`.
pub(crate) fn is_type_object_value(value: &Value) -> bool {
    is_type_object(value)
}

fn is_type_object(value: &Value) -> bool {
    !(value.is_undefined()
        || value.is_null()
        || value.is_boolean()
        || value.is_number()
        || value.is_big_int()
        || value.is_string()
        || value.is_symbol()
        || value.is_hole())
}

/// Accept any value of spec type Object (§6.1.7). Used by every
/// Reflect entry point whose step 1 is "If Type(target) is not Object,
/// throw a TypeError exception."
fn expect_object_value(arg: Option<&Value>) -> Result<Value, VmError> {
    match arg {
        Some(v) if is_type_object(v) => Ok(*v),
        _ => Err(VmError::TypeMismatch),
    }
}

/// §7.1.19 ToPropertyKey, invoked observably for non-primitive
/// argument values. Primitive inputs short-circuit to their canonical
/// string form (no JS observable coercion), matching the dispatch
/// ladder's behaviour on simple keys.
fn coerce_property_key(
    interp: &mut Interpreter,
    stack: &mut ActivationStack,
    context: &ExecutionContext,
    arg: Option<&Value>,
) -> Result<VmPropertyKey<'static>, VmError> {
    let Some(v) = arg else {
        return Ok(VmPropertyKey::String("undefined"));
    };
    if let Some(s) = v.as_string(interp.gc_heap()) {
        return Ok(VmPropertyKey::OwnedString(
            s.to_lossy_string(interp.gc_heap()),
        ));
    }
    if let Some(n) = v.as_number() {
        return Ok(VmPropertyKey::OwnedString(n.to_display_string()));
    }
    if let Some(b) = v.as_boolean() {
        return Ok(VmPropertyKey::String(if b { "true" } else { "false" }));
    }
    if v.is_null() {
        return Ok(VmPropertyKey::String("null"));
    }
    if v.is_undefined() {
        return Ok(VmPropertyKey::String("undefined"));
    }
    if let Some(sym) = v.as_symbol(interp.gc_heap()) {
        return Ok(VmPropertyKey::Symbol(sym));
    }
    // Fall through to general coercion path.
    interp.evaluate_to_property_key(stack, context, v)
}

/// §7.3.18 CreateListFromArrayLike for the array-like arguments that
/// `Reflect.apply` / `Reflect.construct` pass through. Accepts:
/// `undefined`, `null` (empty), arrays, and ordinary array-likes
/// with `length` + indexed properties via the shared interpreter helper.
fn create_list_from_array_like(
    interp: &mut Interpreter,
    stack: &mut ActivationStack,
    context: &ExecutionContext,
    arg: Option<&Value>,
) -> Result<SmallVec<[Value; 8]>, VmError> {
    if let Some(arr) = arg.and_then(|v| v.as_array()) {
        // §7.3.18 step 4–5 — substitute holes with `undefined`.
        return Ok(crate::array::with_elements(
            arr,
            interp.gc_heap(),
            |elements| {
                elements
                    .iter()
                    .map(|v| if v.is_hole() { Value::undefined() } else { *v })
                    .collect()
            },
        ));
    }
    if let Some(v) = arg
        && is_type_object(v)
    {
        return interp.create_list_from_array_like(stack, context, *v);
    }
    Err(interp.err_type(("argumentsList must be an object".to_string()).into()))
}

// Static namespace spec installed by bootstrap. Every method is
// configurable, writable, non-enumerable per §28.1 (same descriptor
// shape used by `Object` statics). `length` values come from the
// algorithm headers in the spec.
//
// `REFLECT_SPEC` + `Intrinsic` generated by `holt!`. §28.1 — the
// `Reflect` namespace object's `[[Prototype]]` is
// `%Object.prototype%`, so `link_object_prototype = true` (the
// hand-written installer did this explicitly).
otter_macros::holt! {
    name = "Reflect",
    feature = CORE,
    link_object_prototype = true,
    methods = {
        "apply"                    / 3 => native_apply,
        "construct"                / 2 => native_construct,
        "defineProperty"           / 3 => native_define_property,
        "deleteProperty"           / 2 => native_delete_property,
        "get"                      / 2 => native_get,
        "getOwnPropertyDescriptor" / 2 => native_get_own_property_descriptor,
        "getPrototypeOf"           / 1 => native_get_prototype_of,
        "has"                      / 2 => native_has,
        "isExtensible"             / 1 => native_is_extensible,
        "ownKeys"                  / 1 => native_own_keys,
        "preventExtensions"        / 1 => native_prevent_extensions,
        "set"                      / 3 => native_set,
        "setPrototypeOf"           / 2 => native_set_prototype_of,
    },
}

fn native_apply(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    invoke(ctx, otter_bytecode::method_id::ReflectMethod::Apply, args)
}

fn native_construct(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    invoke(
        ctx,
        otter_bytecode::method_id::ReflectMethod::Construct,
        args,
    )
}

fn native_define_property(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    invoke(
        ctx,
        otter_bytecode::method_id::ReflectMethod::DefineProperty,
        args,
    )
}

fn native_delete_property(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    invoke(
        ctx,
        otter_bytecode::method_id::ReflectMethod::DeleteProperty,
        args,
    )
}

fn native_get(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    invoke(ctx, otter_bytecode::method_id::ReflectMethod::Get, args)
}

fn native_get_own_property_descriptor(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    invoke(
        ctx,
        otter_bytecode::method_id::ReflectMethod::GetOwnPropertyDescriptor,
        args,
    )
}

fn native_get_prototype_of(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    invoke(
        ctx,
        otter_bytecode::method_id::ReflectMethod::GetPrototypeOf,
        args,
    )
}

fn native_has(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    invoke(ctx, otter_bytecode::method_id::ReflectMethod::Has, args)
}

fn native_is_extensible(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    invoke(
        ctx,
        otter_bytecode::method_id::ReflectMethod::IsExtensible,
        args,
    )
}

fn native_own_keys(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    invoke(ctx, otter_bytecode::method_id::ReflectMethod::OwnKeys, args)
}

fn native_prevent_extensions(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    invoke(
        ctx,
        otter_bytecode::method_id::ReflectMethod::PreventExtensions,
        args,
    )
}

fn native_set(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    invoke(ctx, otter_bytecode::method_id::ReflectMethod::Set, args)
}

fn native_set_prototype_of(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    invoke(
        ctx,
        otter_bytecode::method_id::ReflectMethod::SetPrototypeOf,
        args,
    )
}

fn invoke(
    ctx: &mut NativeCtx<'_>,
    method: otter_bytecode::method_id::ReflectMethod,
    args: &[Value],
) -> Result<Value, NativeError> {
    let context = ctx
        .execution_context()
        .cloned()
        .ok_or_else(|| NativeError::TypeError {
            name: "Reflect",
            reason: "no active execution context".to_string(),
        })?;
    let result = ctx.with_turn_parts(|interp, stack| call(interp, stack, &context, method, args));
    match result {
        Ok(v) => Ok(v),
        Err(e) => Err(vm_to_native(ctx.interp_mut(), e)),
    }
}

fn vm_to_native(interp: &mut Interpreter, err: VmError) -> NativeError {
    match err {
        VmError::TypeMismatch => NativeError::TypeError {
            name: "Reflect",
            reason: "type mismatch".to_string(),
        },
        VmError::TypeError => {
            let message = match interp.take_error_detail() {
                Some(crate::run_control::ErrorDetail::Message(m)) => m,
                _ => Default::default(),
            };
            NativeError::TypeError {
                name: "Reflect",
                reason: message.into(),
            }
        }
        VmError::SyntaxError => {
            let message = match interp.take_error_detail() {
                Some(crate::run_control::ErrorDetail::Message(m)) => m,
                _ => Default::default(),
            };
            NativeError::SyntaxError {
                name: "Reflect",
                reason: message.into(),
            }
        }
        VmError::RangeError => {
            let message = match interp.take_error_detail() {
                Some(crate::run_control::ErrorDetail::Message(m)) => m,
                _ => Default::default(),
            };
            NativeError::RangeError {
                name: "Reflect",
                reason: message.into(),
            }
        }
        VmError::NotCallable => NativeError::TypeError {
            name: "Reflect",
            reason: "value is not a function".to_string(),
        },
        VmError::Uncaught => {
            let value = match interp.take_error_detail() {
                Some(crate::run_control::ErrorDetail::Uncaught(m)) => m,
                _ => Default::default(),
            };
            NativeError::Thrown {
                name: "Reflect",
                message: value.into(),
            }
        }
        VmError::OutOfMemory { .. } => NativeError::TypeError {
            name: "Reflect",
            reason: "out of memory".to_string(),
        },
        VmError::Exit { code } => NativeError::Exit { code },
        other => NativeError::TypeError {
            name: "Reflect",
            reason: other.to_string(),
        },
    }
}

fn is_callable(value: &Value, heap: &otter_gc::GcHeap) -> bool {
    if let Some(obj) = value.as_object() {
        crate::object::call_native(obj, heap).is_some_and(|v| v.is_native_function())
    } else {
        crate::abstract_ops::is_callable(value)
    }
}

fn is_constructor(value: &Value, context: &ExecutionContext, heap: &otter_gc::GcHeap) -> bool {
    if let Some(obj) = value.as_object() {
        crate::object::constructor_native(obj, heap).is_some_and(|v| v.is_native_function())
    } else {
        crate::abstract_ops::is_constructor(value, context, heap)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use otter_bytecode::{
        BytecodeModule, Function, Instruction, SourceKind as BcSourceKind, SpanEntry,
        method_id::ReflectMethod,
    };

    fn empty_context() -> ExecutionContext {
        ExecutionContext::from_module(BytecodeModule {
            module: "reflect-test.ts".to_string(),
            template_sites: Vec::new(),
            source_kind: BcSourceKind::TypeScript,
            functions: vec![Function {
                id: 0,
                name: "<main>".to_string(),
                span: (0, 0),
                locals: 0,
                scratch: 0,
                param_count: 0,
                length: 0,
                own_upvalue_count: 0,
                is_strict: false,
                is_arrow: false,
                is_method: false,
                has_rest: false,
                is_async: false,
                is_generator: false,
                is_async_generator: false,
                is_derived_constructor: false,
                is_module: false,
                needs_arguments: false,
                uses_arguments_callee: false,
                arguments_object_kind: crate::ArgumentsObjectKind::Unmapped,
                mapped_argument_bindings: Vec::new(),
                source_text: None,
                source_text_span: None,
                module_url: String::new(),
                direct_eval_bindings: Vec::new(),
                contains_direct_eval: false,
                code: Vec::<Instruction>::new().into(),
                spans: Vec::<SpanEntry>::new(),
                number_hint_sites: Vec::new(),
                class_hint_sites: Vec::new(),
            }],
            constants: Vec::new(),
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        })
    }

    #[test]
    fn get_own_property_descriptor_uses_runtime_rooted_object_allocation() {
        let mut interp = Interpreter::new();
        let mut target =
            crate::object::alloc_object_old_for_fixture(interp.gc_heap_mut()).expect("target");
        crate::object::set(
            &mut target,
            interp.gc_heap_mut(),
            "answer",
            Value::number(crate::NumberValue::from_i32(42)),
        );
        let context = empty_context();
        let key =
            Value::string(crate::JsString::from_str("answer", interp.gc_heap_mut()).expect("key"));
        let before = interp.gc_heap().stats().new_allocated_bytes;
        let mut stack = ActivationStack::new();

        let result = call(
            &mut interp,
            &mut stack,
            &context,
            ReflectMethod::GetOwnPropertyDescriptor,
            &[Value::object(target), key],
        )
        .expect("descriptor");

        let after = interp.gc_heap().stats().new_allocated_bytes;
        assert!(
            after > before,
            "Reflect.getOwnPropertyDescriptor should allocate descriptor objects in young space"
        );
        assert!(result.is_object());
    }

    #[test]
    fn own_keys_uses_runtime_rooted_array_allocation() {
        let mut interp = Interpreter::new();
        let mut target =
            crate::object::alloc_object_old_for_fixture(interp.gc_heap_mut()).expect("target");
        crate::object::set(&mut target, interp.gc_heap_mut(), "a", Value::boolean(true));
        let context = empty_context();
        let before = interp.gc_heap().stats().new_allocated_bytes;
        let mut stack = ActivationStack::new();

        let result = call(
            &mut interp,
            &mut stack,
            &context,
            ReflectMethod::OwnKeys,
            &[Value::object(target)],
        )
        .expect("ownKeys");

        let after = interp.gc_heap().stats().new_allocated_bytes;
        assert!(
            after > before,
            "Reflect.ownKeys should allocate the key array in young space"
        );
        assert!(result.is_array());
    }
}
