//! `Object.<static>` dispatcher — handles the descriptor-shaped
//! surface (`defineProperty`, `getOwnPropertyDescriptor`, `freeze`,
//! `seal`, `preventExtensions`, the `is*` predicates) wired through
//! [`crate::otter_bytecode::Op::ObjectCall`]. Routed by name; unknown
//! names raise [`VmError::UnknownIntrinsic`].
//!
//! Construction-time built-ins (`create`, `getPrototypeOf`,
//! `setPrototypeOf`, `is`) keep their own dedicated opcodes so this
//! file only owns the descriptor / integrity ladder.
//!
//! # Contents
//! - [`call`] — single entry point used by the dispatch loop.
//! - [`coerce_to_descriptor`] — implements §6.2.5.5
//!   `ToPropertyDescriptor` against a JS-side descriptor object.
//!
//! # Invariants
//! - All names match ECMA-262 spelling exactly.
//! - Reads of the descriptor object's `value / writable / enumerable
//!   / configurable / get / set` slots use direct own-data reads
//!   ([`crate::object::lookup_own`]). User-installed accessors / inherited
//!   descriptor fields are intentionally ignored in this slice; the
//!   spec uses the full `[[Get]]` ladder, but the ergonomic surface
//!   (`Object.defineProperty(o, 'k', { value: 1 })`) doesn't depend
//!   on it. Filed against task 60 for full faithfulness.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-properties-of-the-object-constructor>
//! - <https://tc39.es/ecma262/#sec-topropertydescriptor>
//! - <https://tc39.es/ecma262/#sec-setintegritylevel>

use crate::js_surface::{Attr, MethodSpec, NamespaceSpec};
use crate::native_function::NativeCall;
use crate::object::{
    DescriptorKind, JsObject, PartialPropertyDescriptor, PropertyDescriptor, PropertyLookup,
};
use crate::string::{JsString, StringHeap};
use crate::symbol::JsSymbol;
use crate::{NativeCtx, NativeError, Value, VmError};

enum PropertyKey {
    String(String),
    Symbol(JsSymbol),
}

impl PropertyKey {
    fn label(&self) -> String {
        match self {
            Self::String(key) => key.clone(),
            Self::Symbol(sym) => sym.descriptive_string(),
        }
    }
}

/// Static `Object` constructor-shaped surface installed by bootstrap.
///
/// The active compiler still lowers direct `Object.<name>(...)`
/// calls to [`crate::otter_bytecode::Op::ObjectCall`]. This spec
/// installs the same functions as JS-visible properties so `typeof
/// Object.hasOwn`, descriptor helpers, and extracted calls observe a
/// real builtin function value too.
pub static OBJECT_SPEC: NamespaceSpec = NamespaceSpec {
    name: "Object",
    methods: OBJECT_STATIC_METHODS,
    accessors: &[],
    constants: &[],
    attrs: Attr::global_binding(),
};

const OBJECT_STATIC_METHODS: &[MethodSpec] = &[
    method("is", 2, native_is),
    method("getPrototypeOf", 1, native_get_prototype_of),
    method("setPrototypeOf", 2, native_set_prototype_of),
    method("create", 2, native_create),
    method("defineProperty", 3, native_define_property),
    method("defineProperties", 2, native_define_properties),
    method(
        "getOwnPropertyDescriptor",
        2,
        native_get_own_property_descriptor,
    ),
    method(
        "getOwnPropertyDescriptors",
        1,
        native_get_own_property_descriptors,
    ),
    method("freeze", 1, native_freeze),
    method("isFrozen", 1, native_is_frozen),
    method("seal", 1, native_seal),
    method("isSealed", 1, native_is_sealed),
    method("preventExtensions", 1, native_prevent_extensions),
    method("isExtensible", 1, native_is_extensible),
    method("keys", 1, native_keys),
    method("values", 1, native_values),
    method("entries", 1, native_entries),
    method("assign", 2, native_assign),
    method("fromEntries", 1, native_from_entries),
    method("hasOwn", 2, native_has_own),
    method("getOwnPropertyNames", 1, native_get_own_property_names),
    method("getOwnPropertySymbols", 1, native_get_own_property_symbols),
];

/// Static methods installed on `Object.prototype`.
pub static OBJECT_PROTOTYPE_METHODS: &[MethodSpec] = &[
    method("toString", 0, native_prototype_to_string),
    method("toLocaleString", 0, native_prototype_to_locale_string),
    method("valueOf", 0, native_prototype_value_of),
    method("hasOwnProperty", 1, native_prototype_has_own_property),
    method(
        "propertyIsEnumerable",
        1,
        native_prototype_property_is_enumerable,
    ),
    method("isPrototypeOf", 1, native_prototype_is_prototype_of),
];

const fn method(
    name: &'static str,
    length: u8,
    call: for<'rt> fn(&mut NativeCtx<'rt>, &[Value]) -> Result<Value, NativeError>,
) -> MethodSpec {
    MethodSpec {
        name,
        length,
        attrs: Attr::builtin_function(),
        call: NativeCall::Static(call),
    }
}

fn native_call(
    method: otter_bytecode::method_id::ObjectMethod,
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    let context = ctx.execution_context().cloned();
    if let Some(result) = native_rooted_call(method, ctx, context.as_ref(), args)
        .map_err(|err| object_native_error(method.name(), err))?
    {
        return Ok(result);
    }
    if let Some(result) = ctx
        .cx
        .interp
        .try_function_object_static_call(context.as_ref(), None, method, args)
        .map_err(|err| object_native_error(method.name(), err))?
    {
        return Ok(result);
    }
    if let Some(context) = context.as_ref()
        && let Some(result) = ctx
            .cx
            .interp
            .try_proxy_object_static_call(context, None, method, args)
            .map_err(|err| object_native_error(method.name(), err))?
    {
        return Ok(result);
    }
    let string_heap = ctx.cx.interp.string_heap_clone();
    call(method, args, &string_heap, ctx.heap_mut())
        .map_err(|err| object_native_error(method.name(), err))
}

fn native_rooted_call(
    method: otter_bytecode::method_id::ObjectMethod,
    ctx: &mut NativeCtx<'_>,
    context: Option<&crate::ExecutionContext>,
    args: &[Value],
) -> Result<Option<Value>, VmError> {
    use otter_bytecode::method_id::ObjectMethod as M;
    match method {
        M::Create => native_create_rooted(ctx, args).map(Some),
        M::Keys => native_keys_rooted(ctx, context, args).map(Some),
        M::Values => native_values_rooted(ctx, args).map(Some),
        M::Entries => native_entries_rooted(ctx, args).map(Some),
        M::FromEntries => native_from_entries_rooted(ctx, args).map(Some),
        M::GetOwnPropertyDescriptor => {
            native_get_own_property_descriptor_rooted(ctx, context, args).map(Some)
        }
        M::GetOwnPropertyDescriptors => {
            native_get_own_property_descriptors_rooted(ctx, args).map(Some)
        }
        M::GetOwnPropertyNames => {
            native_get_own_property_names_rooted(ctx, context, args).map(Some)
        }
        M::GetOwnPropertySymbols => {
            native_get_own_property_symbols_rooted(ctx, context, args).map(Some)
        }
        _ => Ok(None),
    }
}

fn native_create_rooted(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, VmError> {
    let proto = args.first().cloned().unwrap_or(Value::Undefined);
    let proto_obj = match proto {
        Value::Object(object) => Some(object),
        Value::Null => None,
        _ => return Err(VmError::TypeMismatch),
    };
    let obj = ctx.alloc_object_with_roots(&[&proto], &[args])?;
    crate::object::set_prototype(obj, ctx.heap_mut(), proto_obj);
    if let Some(props_arg) = args.get(1)
        && !matches!(props_arg, Value::Undefined)
    {
        let props = match props_arg {
            Value::Object(object) => *object,
            _ => return Err(VmError::TypeMismatch),
        };
        let entries: Vec<(String, Value)> =
            crate::object::with_properties(props, ctx.heap(), |p| {
                p.enumerable_data_iter()
                    .map(|(key, value)| (key.to_string(), value))
                    .collect()
            });
        for (key, desc_value) in entries {
            let desc_obj = match desc_value {
                Value::Object(object) => object,
                _ => return Err(VmError::TypeMismatch),
            };
            let descriptor = coerce_to_descriptor(&desc_obj, ctx.heap())?;
            if !crate::object::define_own_property_partial(obj, ctx.heap_mut(), &key, descriptor) {
                return Err(VmError::TypeMismatch);
            }
        }
    }
    Ok(Value::Object(obj))
}

fn native_keys_rooted(
    ctx: &mut NativeCtx<'_>,
    context: Option<&crate::ExecutionContext>,
    args: &[Value],
) -> Result<Value, VmError> {
    let target = args.first().cloned().ok_or(VmError::TypeMismatch)?;
    if matches!(
        target,
        Value::Proxy(_)
            | Value::Array(_)
            | Value::RegExp(_)
            | Value::Function { .. }
            | Value::Closure { .. }
            | Value::BoundFunction(_)
            | Value::NativeFunction(_)
    ) {
        let Some(context) = context else {
            return Err(VmError::InvalidOperand);
        };
        let values = if matches!(target, Value::Proxy(_)) {
            let string_heap = ctx.cx.interp.string_heap_clone();
            let trap_keys =
                ctx.cx
                    .interp
                    .own_property_keys_value(context, &target, &string_heap)?;
            let mut values = Vec::with_capacity(trap_keys.len());
            for key in trap_keys {
                let Value::String(_) = &key else { continue };
                let vm_key = match &key {
                    Value::String(s) => crate::VmPropertyKey::OwnedString(s.to_lossy_string()),
                    Value::Symbol(sym) => crate::VmPropertyKey::Symbol(sym.clone()),
                    _ => return Err(VmError::TypeMismatch),
                };
                let desc = ctx
                    .cx
                    .interp
                    .ordinary_get_own_property_descriptor_value_runtime_rooted(
                        context,
                        target.clone(),
                        &vm_key,
                        0,
                        &[&target],
                        &[args],
                    )?;
                if desc.as_ref().is_some_and(PropertyDescriptor::enumerable) {
                    values.push(key);
                }
            }
            values
        } else {
            let keys =
                ctx.cx
                    .interp
                    .enumerable_own_string_keys_for_value(context, target.clone(), 0)?;
            let string_heap = ctx.cx.interp.string_heap_clone();
            let mut values = Vec::with_capacity(keys.len());
            for key in keys {
                values.push(string_value(&key, &string_heap)?);
            }
            values
        };
        let array = ctx.array_from_elements_with_roots(values, &[&target], &[args])?;
        return Ok(Value::Array(array));
    }

    let owned: Vec<String> = match args.first() {
        Some(Value::Object(target)) => crate::object::with_properties(*target, ctx.heap(), |p| {
            p.enumerable_keys().map(|k| k.to_string()).collect()
        }),
        Some(Value::NativeFunction(native)) => native
            .enumerable_own_property_keys(ctx.heap())
            .into_iter()
            .collect(),
        Some(Value::BoundFunction(bound)) => {
            crate::function_metadata::bound_enumerable_own_property_keys(bound, ctx.heap())
                .into_iter()
                .collect()
        }
        _ => return Err(VmError::TypeMismatch),
    };
    let string_heap = ctx.cx.interp.string_heap_clone();
    let mut names = Vec::with_capacity(owned.len());
    for key in owned {
        names.push(string_value(&key, &string_heap)?);
    }
    Ok(Value::Array(ctx.array_from_elements_with_roots(
        names,
        &[],
        &[args],
    )?))
}

fn native_values_rooted(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, VmError> {
    let target = expect_object(args.first())?;
    let values: Vec<Value> = crate::object::with_properties(target, ctx.heap(), |p| {
        p.enumerable_data_iter().map(|(_, value)| value).collect()
    });
    let target_root = Value::Object(target);
    Ok(Value::Array(ctx.array_from_elements_with_roots(
        values,
        &[&target_root],
        &[args],
    )?))
}

fn native_entries_rooted(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, VmError> {
    let target = expect_object(args.first())?;
    let raw: Vec<(String, Value)> = crate::object::with_properties(target, ctx.heap(), |p| {
        p.enumerable_data_iter()
            .map(|(key, value)| (key.to_string(), value))
            .collect()
    });
    let string_heap = ctx.cx.interp.string_heap_clone();
    let target_root = Value::Object(target);
    let mut pairs = Vec::with_capacity(raw.len());
    for (key, value) in raw {
        let key_value = string_value(&key, &string_heap)?;
        let pair = ctx.array_from_elements_with_roots(
            [key_value, value],
            &[&target_root],
            &[args, pairs.as_slice()],
        )?;
        pairs.push(Value::Array(pair));
    }
    Ok(Value::Array(ctx.array_from_elements_with_roots(
        pairs,
        &[&target_root],
        &[args],
    )?))
}

fn native_from_entries_rooted(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, VmError> {
    let iter = args.first().cloned().unwrap_or(Value::Undefined);
    let result = ctx.alloc_object_with_roots(&[], &[args])?;
    match iter {
        Value::Array(arr) => {
            let snapshot: Vec<Value> =
                crate::array::with_elements(arr, ctx.heap(), |elements| elements.to_vec());
            for entry in snapshot {
                let (key, value) = read_entry_pair(ctx, &entry)?;
                let key_str = property_key_from_value(&key)?;
                crate::object::set(result, ctx.heap_mut(), &key_str, value);
            }
        }
        Value::Map(map) => {
            for (key, value) in crate::collections::map_entries(map, ctx.heap()) {
                let key_str = property_key_from_value(&key)?;
                crate::object::set(result, ctx.heap_mut(), &key_str, value);
            }
        }
        _ => return Err(VmError::TypeMismatch),
    }
    Ok(Value::Object(result))
}

/// §20.1.2.7 step 5.b — read indices `"0"` and `"1"` from an entry
/// candidate via the spec `[[Get]]`. Heap-only variant for the
/// context-less `object_statics::call` path. Accepts Array pairs,
/// ordinary Objects with indexed keys, and String / String-wrapper
/// entries.
fn read_entry_pair_heap(
    entry: &Value,
    heap: &otter_gc::GcHeap,
    string_heap: &StringHeap,
) -> Result<(Value, Value), VmError> {
    match entry {
        Value::Array(pair) => Ok((
            crate::array::get(*pair, heap, 0),
            crate::array::get(*pair, heap, 1),
        )),
        Value::Object(obj) => {
            if let Some(s) = crate::object::string_data(*obj, heap) {
                let units = s.to_utf16_vec();
                let zero = units.first().copied().map_or(Value::Undefined, |u| {
                    crate::string::JsString::from_utf16_units(&[u], string_heap)
                        .map(Value::String)
                        .unwrap_or(Value::Undefined)
                });
                let one = units.get(1).copied().map_or(Value::Undefined, |u| {
                    crate::string::JsString::from_utf16_units(&[u], string_heap)
                        .map(Value::String)
                        .unwrap_or(Value::Undefined)
                });
                return Ok((zero, one));
            }
            let key = crate::object::get(*obj, heap, "0").unwrap_or(Value::Undefined);
            let value = crate::object::get(*obj, heap, "1").unwrap_or(Value::Undefined);
            Ok((key, value))
        }
        Value::String(s) => {
            let units = s.to_utf16_vec();
            let zero = units.first().copied().map_or(Value::Undefined, |u| {
                crate::string::JsString::from_utf16_units(&[u], string_heap)
                    .map(Value::String)
                    .unwrap_or(Value::Undefined)
            });
            let one = units.get(1).copied().map_or(Value::Undefined, |u| {
                crate::string::JsString::from_utf16_units(&[u], string_heap)
                    .map(Value::String)
                    .unwrap_or(Value::Undefined)
            });
            Ok((zero, one))
        }
        _ => Err(VmError::TypeMismatch),
    }
}

fn read_entry_pair(ctx: &NativeCtx<'_>, entry: &Value) -> Result<(Value, Value), VmError> {
    match entry {
        Value::Array(pair) => Ok((
            crate::array::get(*pair, ctx.heap(), 0),
            crate::array::get(*pair, ctx.heap(), 1),
        )),
        Value::Object(obj) => {
            // Wrapper String `Object("ab")` carries `[[StringData]]`
            // — read its code-unit slots directly so `Object.fromEntries
            // ([Object("ab")])` yields `{a: "b"}`.
            if let Some(s) = crate::object::string_data(*obj, ctx.heap()) {
                let units = s.to_utf16_vec();
                let zero = units.first().copied().map_or(Value::Undefined, |u| {
                    crate::string::JsString::from_utf16_units(
                        &[u],
                        &ctx.cx.interp.string_heap_clone(),
                    )
                    .map(Value::String)
                    .unwrap_or(Value::Undefined)
                });
                let one = units.get(1).copied().map_or(Value::Undefined, |u| {
                    crate::string::JsString::from_utf16_units(
                        &[u],
                        &ctx.cx.interp.string_heap_clone(),
                    )
                    .map(Value::String)
                    .unwrap_or(Value::Undefined)
                });
                return Ok((zero, one));
            }
            let key = crate::object::get(*obj, ctx.heap(), "0").unwrap_or(Value::Undefined);
            let value = crate::object::get(*obj, ctx.heap(), "1").unwrap_or(Value::Undefined);
            Ok((key, value))
        }
        Value::String(s) => {
            let units = s.to_utf16_vec();
            let zero = units.first().copied().map_or(Value::Undefined, |u| {
                crate::string::JsString::from_utf16_units(&[u], &ctx.cx.interp.string_heap_clone())
                    .map(Value::String)
                    .unwrap_or(Value::Undefined)
            });
            let one = units.get(1).copied().map_or(Value::Undefined, |u| {
                crate::string::JsString::from_utf16_units(&[u], &ctx.cx.interp.string_heap_clone())
                    .map(Value::String)
                    .unwrap_or(Value::Undefined)
            });
            Ok((zero, one))
        }
        _ => Err(VmError::TypeMismatch),
    }
}

fn native_get_own_property_descriptor_rooted(
    ctx: &mut NativeCtx<'_>,
    context: Option<&crate::ExecutionContext>,
    args: &[Value],
) -> Result<Value, VmError> {
    let target = args.first().cloned().ok_or(VmError::TypeMismatch)?;
    if matches!(
        target,
        Value::Proxy(_)
            | Value::Array(_)
            | Value::RegExp(_)
            | Value::Function { .. }
            | Value::Closure { .. }
            | Value::BoundFunction(_)
            | Value::NativeFunction(_)
    ) {
        let Some(context) = context else {
            return Err(VmError::InvalidOperand);
        };
        let desc = ctx.cx.interp.get_own_property_descriptor_for_value(
            context,
            target.clone(),
            args.get(1),
        )?;
        return match desc {
            Some(desc) => Ok(Value::Object(native_descriptor_to_object_rooted(
                ctx,
                &desc,
                &[&target],
                args,
            )?)),
            None => Ok(Value::Undefined),
        };
    }

    let key = expect_property_key(args.get(1))?;
    let desc = match args.first() {
        Some(Value::Object(target)) => match &key {
            PropertyKey::String(key) => crate::object::get_own_descriptor(*target, ctx.heap(), key),
            PropertyKey::Symbol(sym) => {
                crate::object::get_own_symbol_descriptor(*target, ctx.heap(), sym)
            }
        },
        Some(Value::ClassConstructor(class)) => match &key {
            PropertyKey::String(key) => {
                crate::object::get_own_descriptor(class.statics(ctx.heap()), ctx.heap(), key)
            }
            PropertyKey::Symbol(sym) => {
                crate::object::get_own_symbol_descriptor(class.statics(ctx.heap()), ctx.heap(), sym)
            }
        },
        Some(Value::NativeFunction(native)) => {
            let PropertyKey::String(key) = &key else {
                return Ok(Value::Undefined);
            };
            native.own_property_descriptor(ctx.heap(), &ctx.cx.interp.string_heap_clone(), key)?
        }
        // §20.1.2.7 — primitive operands are coerced via ToObject;
        // the wrapper carries no own data property for arbitrary
        // keys (except String which exposes indexed characters and
        // `length`, handled in the dedicated arms above). Returning
        // `Undefined` matches the spec's "no such own property"
        // path without materialising a transient wrapper.
        Some(
            Value::Boolean(_)
            | Value::Number(_)
            | Value::String(_)
            | Value::Symbol(_)
            | Value::BigInt(_),
        ) => None,
        Some(Value::Null) | Some(Value::Undefined) | None => {
            return Err(VmError::TypeError {
                message: "Object.getOwnPropertyDescriptor: cannot convert null/undefined to object"
                    .to_string(),
            });
        }
        _ => {
            return Err(VmError::TypeError {
                message: "Object.getOwnPropertyDescriptor target must be an object".to_string(),
            });
        }
    };
    match desc {
        Some(desc) => Ok(Value::Object(native_descriptor_to_object_rooted(
            ctx,
            &desc,
            &[],
            args,
        )?)),
        None => Ok(Value::Undefined),
    }
}

fn native_get_own_property_descriptors_rooted(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, VmError> {
    let target = expect_object(args.first())?;
    let target_root = Value::Object(target);
    let result = ctx.alloc_object_with_roots(&[&target_root], &[args])?;
    let result_root = Value::Object(result);
    let (keys, symbols): (Vec<String>, Vec<JsSymbol>) =
        crate::object::with_properties(target, ctx.heap(), |p| {
            (
                p.keys().map(|s| s.to_string()).collect(),
                p.symbol_keys().collect(),
            )
        });
    for key in keys {
        if let Some(desc) = crate::object::get_own_descriptor(target, ctx.heap(), &key) {
            let desc_obj = native_descriptor_to_object_rooted(
                ctx,
                &desc,
                &[&target_root, &result_root],
                args,
            )?;
            crate::object::set(result, ctx.heap_mut(), &key, Value::Object(desc_obj));
        }
    }
    for sym in symbols {
        if let Some(desc) = crate::object::get_own_symbol_descriptor(target, ctx.heap(), &sym) {
            let desc_obj = native_descriptor_to_object_rooted(
                ctx,
                &desc,
                &[&target_root, &result_root],
                args,
            )?;
            if !crate::object::set_symbol(result, ctx.heap_mut(), sym, Value::Object(desc_obj)) {
                return Err(VmError::TypeMismatch);
            }
        }
    }
    Ok(Value::Object(result))
}

fn native_get_own_property_names_rooted(
    ctx: &mut NativeCtx<'_>,
    context: Option<&crate::ExecutionContext>,
    args: &[Value],
) -> Result<Value, VmError> {
    if let Some(target @ Value::Proxy(_)) = args.first() {
        let Some(context) = context else {
            return Err(VmError::InvalidOperand);
        };
        let target = target.clone();
        let string_heap = ctx.cx.interp.string_heap_clone();
        let trap_keys = ctx
            .cx
            .interp
            .own_property_keys_value(context, &target, &string_heap)?;
        let values: Vec<Value> = trap_keys
            .into_iter()
            .filter(|v| matches!(v, Value::String(_)))
            .collect();
        return Ok(Value::Array(ctx.array_from_elements_with_roots(
            values,
            &[&target],
            &[args],
        )?));
    }
    let owned: Vec<String> = match args.first() {
        Some(Value::Object(target)) => crate::object::with_properties(*target, ctx.heap(), |p| {
            p.keys().map(|k| k.to_string()).collect()
        }),
        Some(Value::NativeFunction(native)) => {
            native.own_property_keys(ctx.heap()).into_iter().collect()
        }
        Some(Value::BoundFunction(bound)) => {
            crate::function_metadata::bound_own_property_keys(bound, ctx.heap())
                .into_iter()
                .collect()
        }
        Some(Value::Function { function_id }) | Some(Value::Closure { function_id, .. }) => {
            let Some(context) = context else {
                return Err(VmError::InvalidOperand);
            };
            ctx.cx
                .interp
                .ordinary_function_own_property_keys(context, *function_id)
        }
        Some(Value::ClassConstructor(class)) => {
            // §15.7.13 — class constructors expose `prototype` as
            // an own property in addition to anything installed on
            // their static-bag object.
            let mut keys: Vec<String> =
                crate::object::with_properties(class.statics(ctx.heap()), ctx.heap(), |p| {
                    p.keys().map(|k| k.to_string()).collect()
                });
            if !keys.iter().any(|k| k == "prototype") {
                keys.push("prototype".to_string());
            }
            keys
        }
        Some(Value::Boolean(_) | Value::Number(_) | Value::Symbol(_)) => Vec::new(),
        Some(Value::String(s)) => {
            let mut keys: Vec<String> = (0..s.len()).map(|idx| idx.to_string()).collect();
            keys.push("length".to_string());
            keys
        }
        _ => return Err(VmError::TypeMismatch),
    };
    let string_heap = ctx.cx.interp.string_heap_clone();
    let mut names = Vec::with_capacity(owned.len());
    for key in owned {
        names.push(string_value(&key, &string_heap)?);
    }
    Ok(Value::Array(ctx.array_from_elements_with_roots(
        names,
        &[],
        &[args],
    )?))
}

fn native_get_own_property_symbols_rooted(
    ctx: &mut NativeCtx<'_>,
    context: Option<&crate::ExecutionContext>,
    args: &[Value],
) -> Result<Value, VmError> {
    if let Some(target @ Value::Proxy(_)) = args.first() {
        let Some(context) = context else {
            return Err(VmError::InvalidOperand);
        };
        let target = target.clone();
        let string_heap = ctx.cx.interp.string_heap_clone();
        let trap_keys = ctx
            .cx
            .interp
            .own_property_keys_value(context, &target, &string_heap)?;
        let values: Vec<Value> = trap_keys
            .into_iter()
            .filter(|v| matches!(v, Value::Symbol(_)))
            .collect();
        return Ok(Value::Array(ctx.array_from_elements_with_roots(
            values,
            &[&target],
            &[args],
        )?));
    }
    let target = expect_object(args.first())?;
    let syms: Vec<Value> = crate::object::with_properties(target, ctx.heap(), |p| {
        p.symbol_keys().map(Value::Symbol).collect()
    });
    let target_root = Value::Object(target);
    Ok(Value::Array(ctx.array_from_elements_with_roots(
        syms,
        &[&target_root],
        &[args],
    )?))
}

fn native_descriptor_to_object_rooted(
    ctx: &mut NativeCtx<'_>,
    desc: &PropertyDescriptor,
    value_roots: &[&Value],
    args: &[Value],
) -> Result<JsObject, VmError> {
    let mut roots = Vec::with_capacity(value_roots.len() + 2);
    roots.extend_from_slice(value_roots);
    match &desc.kind {
        DescriptorKind::Data { value } => roots.push(value),
        DescriptorKind::Accessor { getter, setter } => {
            if let Some(getter) = getter {
                roots.push(getter);
            }
            if let Some(setter) = setter {
                roots.push(setter);
            }
        }
    }
    let result = ctx.alloc_object_with_roots(roots.as_slice(), &[args])?;
    match &desc.kind {
        DescriptorKind::Data { value } => {
            crate::object::set(result, ctx.heap_mut(), "value", value.clone());
            crate::object::set(
                result,
                ctx.heap_mut(),
                "writable",
                Value::Boolean(desc.writable()),
            );
        }
        DescriptorKind::Accessor { getter, setter } => {
            crate::object::set(
                result,
                ctx.heap_mut(),
                "get",
                getter.clone().unwrap_or(Value::Undefined),
            );
            crate::object::set(
                result,
                ctx.heap_mut(),
                "set",
                setter.clone().unwrap_or(Value::Undefined),
            );
        }
    }
    crate::object::set(
        result,
        ctx.heap_mut(),
        "enumerable",
        Value::Boolean(desc.enumerable()),
    );
    crate::object::set(
        result,
        ctx.heap_mut(),
        "configurable",
        Value::Boolean(desc.configurable()),
    );
    Ok(result)
}

fn object_native_error(name: &'static str, err: VmError) -> NativeError {
    NativeError::TypeError {
        name,
        reason: err.to_string(),
    }
}

macro_rules! native_object_static {
    ($fn_name:ident, $variant:ident) => {
        fn $fn_name(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
            native_call(otter_bytecode::method_id::ObjectMethod::$variant, ctx, args)
        }
    };
}

native_object_static!(native_create, Create);
native_object_static!(native_define_property, DefineProperty);
native_object_static!(native_define_properties, DefineProperties);
native_object_static!(native_get_own_property_descriptor, GetOwnPropertyDescriptor);
native_object_static!(
    native_get_own_property_descriptors,
    GetOwnPropertyDescriptors
);
native_object_static!(native_freeze, Freeze);
native_object_static!(native_is_frozen, IsFrozen);
native_object_static!(native_seal, Seal);
native_object_static!(native_is_sealed, IsSealed);
native_object_static!(native_prevent_extensions, PreventExtensions);
native_object_static!(native_is_extensible, IsExtensible);
native_object_static!(native_keys, Keys);
native_object_static!(native_values, Values);
native_object_static!(native_entries, Entries);
native_object_static!(native_assign, Assign);
native_object_static!(native_from_entries, FromEntries);
native_object_static!(native_has_own, HasOwn);
native_object_static!(native_get_own_property_names, GetOwnPropertyNames);
native_object_static!(native_get_own_property_symbols, GetOwnPropertySymbols);

/// §20.1.2.13 `Object.is(value1, value2)` — direct §7.2.11 SameValue.
///
/// Mirrors the compile-time `Op::SameValue` lowering so callers that
/// read the property as a value (e.g.
/// `Object.getOwnPropertyDescriptor(Object, "is").value`) and then
/// invoke it through `.call` / `Reflect.apply` see the spec result.
fn native_is(_ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let a = args.first().cloned().unwrap_or(Value::Undefined);
    let b = args.get(1).cloned().unwrap_or(Value::Undefined);
    Ok(Value::Boolean(crate::abstract_ops::same_value(&a, &b)))
}

/// §20.1.2.12 `Object.getPrototypeOf(O)` — `[[Prototype]]` of `O`
/// after ToObject coercion. Primitive operands resolve to their
/// respective `%X.prototype%` per §7.1.18.
fn native_get_prototype_of(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let target = args.first().cloned().unwrap_or(Value::Undefined);
    let interp = ctx.interp_mut();
    interp.get_prototype_for_op(&target).map_err(|err| {
        object_native_error(
            otter_bytecode::method_id::ObjectMethod::PreventExtensions.name(),
            err,
        )
    })
}

/// §20.1.2.21 `Object.setPrototypeOf(O, proto)` — assigns the
/// `[[Prototype]]` of `O` to `proto` (which must be Object or Null)
/// and returns `O` after ToObject coercion checks.
fn native_set_prototype_of(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let target = args.first().cloned().unwrap_or(Value::Undefined);
    let proto = args.get(1).cloned().unwrap_or(Value::Undefined);
    match (&target, &proto) {
        (Value::Null | Value::Undefined, _) => {
            return Err(NativeError::TypeError {
                name: "Object.setPrototypeOf",
                reason: "Object.setPrototypeOf called on null or undefined".to_string(),
            });
        }
        (_, Value::Object(_) | Value::Null) => {}
        _ => {
            return Err(NativeError::TypeError {
                name: "Object.setPrototypeOf",
                reason: "Object.setPrototypeOf prototype must be an Object or null".to_string(),
            });
        }
    }
    match &target {
        Value::Object(obj) => {
            let proto_obj = if let Value::Object(p) = &proto {
                Some(*p)
            } else {
                None
            };
            crate::object::set_prototype(*obj, ctx.heap_mut(), proto_obj);
            Ok(target)
        }
        // Primitive operands: ToObject would wrap but spec §20.1.2.21
        // step 5 says "Return O" unchanged when ToObject would
        // produce a transient wrapper. We mirror that and skip the
        // prototype write — the wrapper would be unreachable anyway.
        _ => Ok(target),
    }
}

fn native_prototype_to_string(
    ctx: &mut NativeCtx<'_>,
    _args: &[Value],
) -> Result<Value, NativeError> {
    let tag = object_to_string_tag(ctx);
    let display = format!("[object {tag}]");
    let string_heap = ctx.cx.interp.string_heap_clone();
    Ok(Value::String(
        JsString::from_str(&display, &string_heap).map_err(|_| NativeError::TypeError {
            name: "toString",
            reason: "out of memory while allocating string".to_string(),
        })?,
    ))
}

fn native_prototype_value_of(
    ctx: &mut NativeCtx<'_>,
    _args: &[Value],
) -> Result<Value, NativeError> {
    Ok(ctx.this_value().clone())
}

/// §20.1.3.5 `Object.prototype.toLocaleString` — foundation form.
///
/// Spec algorithm: `return ? Invoke(O, "toString")`. We forward to
/// `Object.prototype.toString` directly so the result matches the
/// `[object <tag>]` shape the spec mandates. Once user code overrides
/// `Symbol.toPrimitive` / locale-aware `toString` overloads we'll
/// route this through the standard `Invoke` ladder.
fn native_prototype_to_locale_string(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    native_prototype_to_string(ctx, args)
}

fn native_prototype_has_own_property(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    let this_value = ctx.this_value().clone();
    if let Some(context) = ctx.execution_context().cloned() {
        let desc = ctx
            .cx
            .interp
            .get_own_property_descriptor_for_value(&context, this_value.clone(), args.first())
            .map_err(|err| object_native_error("hasOwnProperty", err))?;
        return Ok(Value::Boolean(desc.is_some()));
    }
    let present = match ctx.this_value() {
        Value::Object(obj) => has_own_property(*obj, ctx.heap(), args.first())
            .map_err(|err| object_native_error("hasOwnProperty", err))?,
        Value::NativeFunction(native) => native_function_has_own(native, ctx.heap(), args.first()),
        Value::BoundFunction(bound) => bound_function_has_own(bound, ctx.heap(), args.first()),
        Value::ClassConstructor(class) => {
            // The own-property surface for a `ClassConstructor` is
            // its `statics` object plus the spec-mandated
            // `prototype` property. Mirror the property-load path
            // (which falls through to `statics`) so spec checks
            // like `Number.hasOwnProperty("EPSILON")` see the
            // installed statics.
            let key = args.first();
            if matches!(key, Some(Value::String(s)) if s.to_lossy_string() == "prototype") {
                true
            } else {
                has_own_property(class.statics(ctx.heap()), ctx.heap(), key)
                    .map_err(|err| object_native_error("hasOwnProperty", err))?
            }
        }
        _ => false,
    };
    Ok(Value::Boolean(present))
}

fn native_prototype_property_is_enumerable(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    let this_value = ctx.this_value().clone();
    if let Some(context) = ctx.execution_context().cloned() {
        let desc = ctx
            .cx
            .interp
            .get_own_property_descriptor_for_value(&context, this_value, args.first())
            .map_err(|err| object_native_error("propertyIsEnumerable", err))?;
        return Ok(Value::Boolean(
            desc.as_ref().is_some_and(PropertyDescriptor::enumerable),
        ));
    }
    let enumerable = match ctx.this_value() {
        Value::Object(obj) => {
            let key = expect_property_key(args.first())
                .map_err(|err| object_native_error("propertyIsEnumerable", err))?;
            match key {
                PropertyKey::String(key) => {
                    match crate::object::lookup_own(*obj, ctx.heap(), &key) {
                        PropertyLookup::Data { flags, .. }
                        | PropertyLookup::Accessor { flags, .. } => flags.enumerable(),
                        PropertyLookup::Absent => false,
                    }
                }
                PropertyKey::Symbol(sym) => {
                    match crate::object::lookup_own_symbol(*obj, ctx.heap(), &sym) {
                        PropertyLookup::Data { flags, .. }
                        | PropertyLookup::Accessor { flags, .. } => flags.enumerable(),
                        PropertyLookup::Absent => false,
                    }
                }
            }
        }
        Value::NativeFunction(native) => {
            let _ = native;
            false
        }
        Value::BoundFunction(bound) => {
            let key = expect_property_key(args.first())
                .map_err(|err| object_native_error("propertyIsEnumerable", err))?;
            match key {
                PropertyKey::String(key) => {
                    crate::function_metadata::bound_own_property_is_enumerable(
                        bound,
                        ctx.heap(),
                        &key,
                    )
                }
                PropertyKey::Symbol(_) => false,
            }
        }
        _ => false,
    };
    Ok(Value::Boolean(enumerable))
}

fn native_prototype_is_prototype_of(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    let function_prototype = ctx.cx.interp.function_prototype_object().ok();
    let this_value = ctx.this_value().clone();
    let target = args.first().cloned();
    let result = match (this_value, target) {
        (Value::Object(proto), Some(value)) => {
            value_has_prototype_in_chain(&value, proto, ctx.heap(), function_prototype)
        }
        _ => false,
    };
    Ok(Value::Boolean(result))
}

fn value_has_prototype_in_chain(
    value: &Value,
    target: JsObject,
    heap: &otter_gc::GcHeap,
    function_prototype: Option<JsObject>,
) -> bool {
    match value {
        Value::Object(obj) if constructor_object_has_function_prototype(*obj, heap) => {
            function_value_has_prototype_in_chain(target, heap, function_prototype)
        }
        Value::Object(obj) => crate::object::has_in_proto_chain(*obj, heap, target),
        Value::Function { .. }
        | Value::Closure { .. }
        | Value::BoundFunction(_)
        | Value::NativeFunction(_)
        | Value::ClassConstructor(_) => {
            function_value_has_prototype_in_chain(target, heap, function_prototype)
        }
        _ => false,
    }
}

fn function_value_has_prototype_in_chain(
    target: JsObject,
    heap: &otter_gc::GcHeap,
    function_prototype: Option<JsObject>,
) -> bool {
    let Some(function_prototype) = function_prototype else {
        return false;
    };
    function_prototype == target
        || crate::object::has_in_proto_chain(function_prototype, heap, target)
}

fn constructor_object_has_function_prototype(obj: JsObject, heap: &otter_gc::GcHeap) -> bool {
    matches!(
        crate::object::constructor_native(obj, heap),
        Some(Value::NativeFunction(_))
    )
}

fn object_to_string_tag(ctx: &NativeCtx<'_>) -> String {
    if let Some(tag) = explicit_to_string_tag(ctx) {
        return tag;
    }
    match ctx.this_value() {
        Value::Undefined | Value::Hole => "Undefined",
        Value::Null => "Null",
        Value::Boolean(_) => "Boolean",
        Value::Number(_) => "Number",
        Value::BigInt(_) => "BigInt",
        Value::String(_) => "String",
        Value::Symbol(_) => "Symbol",
        Value::Function { .. }
        | Value::Closure { .. }
        | Value::BoundFunction(_)
        | Value::NativeFunction(_)
        | Value::ClassConstructor(_) => "Function",
        Value::Array(_) => "Array",
        Value::RegExp(_) => "RegExp",
        Value::Promise(_) => "Promise",
        Value::Map(_) => "Map",
        Value::Set(_) => "Set",
        Value::WeakMap(_) => "WeakMap",
        Value::WeakSet(_) => "WeakSet",
        Value::WeakRef(_) => "WeakRef",
        Value::FinalizationRegistry(_) => "FinalizationRegistry",
        Value::Generator(_) => "Generator",
        Value::Iterator(_) => "Iterator",
        Value::Temporal(_) => "Temporal",
        Value::Intl(_) => "Intl",
        Value::ArrayBuffer(_) => "ArrayBuffer",
        Value::DataView(_) => "DataView",
        Value::TypedArray(_) => "TypedArray",
        Value::Object(obj) if crate::object::call_native(*obj, ctx.heap()).is_some() => "Function",
        Value::Object(_) | Value::Proxy(_) => "Object",
    }
    .to_string()
}

fn explicit_to_string_tag(ctx: &NativeCtx<'_>) -> Option<String> {
    let tag_symbol = ctx
        .cx
        .interp
        .well_known_symbols()
        .get(crate::symbol::WellKnown::ToStringTag);
    let value = match ctx.this_value() {
        Value::Object(obj) => crate::object::get_symbol(*obj, ctx.heap(), &tag_symbol),
        _ => None,
    }?;
    match value {
        Value::String(s) => Some(s.to_lossy_string()),
        _ => None,
    }
}

fn native_function_has_own(
    native: &crate::NativeFunction,
    gc_heap: &otter_gc::GcHeap,
    key: Option<&Value>,
) -> bool {
    match expect_property_key(key) {
        Ok(PropertyKey::String(key)) => native
            .own_property_descriptor(gc_heap, &StringHeap::default(), &key)
            .ok()
            .flatten()
            .is_some(),
        Ok(PropertyKey::Symbol(_)) | Err(_) => false,
    }
}

fn bound_function_has_own(
    bound: &crate::BoundFunction,
    gc_heap: &otter_gc::GcHeap,
    key: Option<&Value>,
) -> bool {
    match expect_property_key(key) {
        Ok(PropertyKey::String(key)) => {
            crate::function_metadata::bound_has_own_property(bound, gc_heap, &key)
        }
        Ok(PropertyKey::Symbol(_)) | Err(_) => false,
    }
}

/// Single entry point for `Object.<method>(args...)` dispatch.
///
/// Routes the typed [`ObjectMethod`] emitted by the compiler — no
/// per-call name match.
///
/// # Errors
/// - [`VmError::TypeMismatch`] when an argument has the wrong shape
///   (e.g., the receiver of `defineProperty` is not an Object).
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-properties-of-the-object-constructor>
pub fn call(
    method: otter_bytecode::method_id::ObjectMethod,
    args: &[Value],
    string_heap: &StringHeap,
    gc_heap: &mut otter_gc::GcHeap,
) -> Result<Value, VmError> {
    use otter_bytecode::method_id::ObjectMethod as M;
    match method {
        // §20.1.2.2 Object.create(O, Properties)
        // <https://tc39.es/ecma262/#sec-object.create>
        M::Create => {
            let proto = args.first().cloned().unwrap_or(Value::Undefined);
            let proto_obj = match proto {
                Value::Object(o) => Some(o),
                Value::Null => None,
                _ => return Err(VmError::TypeMismatch),
            };
            let obj = rooted_object(gc_heap, &[&proto], &[args])?;
            crate::object::set_prototype(obj, gc_heap, proto_obj);
            if let Some(props_arg) = args.get(1)
                && !matches!(props_arg, Value::Undefined)
            {
                let props = match props_arg {
                    Value::Object(o) => *o,
                    _ => return Err(VmError::TypeMismatch),
                };
                let entries: Vec<(String, Value)> =
                    crate::object::with_properties(props, gc_heap, |p| {
                        p.enumerable_data_iter()
                            .map(|(k, v)| (k.to_string(), v))
                            .collect()
                    });
                for (key, desc_value) in entries {
                    let desc_obj = match desc_value {
                        Value::Object(o) => o,
                        _ => return Err(VmError::TypeMismatch),
                    };
                    let descriptor = coerce_to_descriptor(&desc_obj, gc_heap)?;
                    if !crate::object::define_own_property_partial(obj, gc_heap, &key, descriptor) {
                        return Err(VmError::TypeMismatch);
                    }
                }
            }
            Ok(Value::Object(obj))
        }
        // §20.1.2.4 Object.defineProperty(O, P, Attributes)
        // <https://tc39.es/ecma262/#sec-object.defineproperty>
        M::DefineProperty => {
            let key = expect_property_key(args.get(1))?;
            let desc_obj = expect_object(args.get(2))?;
            let descriptor = coerce_to_descriptor(&desc_obj, gc_heap)?;
            match args.first() {
                Some(Value::Object(target)) => {
                    let ok = match &key {
                        PropertyKey::String(key) => crate::object::define_own_property_partial(
                            *target, gc_heap, key, descriptor,
                        ),
                        PropertyKey::Symbol(sym) => {
                            crate::object::define_own_symbol_property_partial(
                                *target, gc_heap, sym, descriptor,
                            )
                        }
                    };
                    if !ok {
                        return Err(VmError::TypeError {
                            message: format!("Cannot define property '{}'", key.label()),
                        });
                    }
                    Ok(Value::Object(*target))
                }
                Some(Value::ClassConstructor(class)) => {
                    let ok = match &key {
                        PropertyKey::String(key) => crate::object::define_own_property_partial(
                            class.statics(gc_heap),
                            gc_heap,
                            key,
                            descriptor,
                        ),
                        PropertyKey::Symbol(sym) => {
                            crate::object::define_own_symbol_property_partial(
                                class.statics(gc_heap),
                                gc_heap,
                                sym,
                                descriptor,
                            )
                        }
                    };
                    if !ok {
                        return Err(VmError::TypeError {
                            message: format!("Cannot define property '{}'", key.label()),
                        });
                    }
                    Ok(Value::ClassConstructor(*class))
                }
                Some(Value::NativeFunction(native)) => {
                    let PropertyKey::String(key) = &key else {
                        return Err(VmError::TypeError {
                            message: format!(
                                "Cannot define property '{}' on function {}",
                                key.label(),
                                native.name(gc_heap)
                            ),
                        });
                    };
                    let completed = descriptor.complete_for_new_property();
                    if !native.define_own_property(gc_heap, string_heap, key, completed) {
                        return Err(VmError::TypeError {
                            message: format!(
                                "Cannot define property '{key}' on function {}",
                                native.name(gc_heap)
                            ),
                        });
                    }
                    Ok(Value::NativeFunction(*native))
                }
                _ => Err(VmError::TypeError {
                    message: "Object.defineProperty target must be an object".to_string(),
                }),
            }
        }
        // §20.1.2.5 Object.defineProperties(O, Properties)
        // <https://tc39.es/ecma262/#sec-object.defineproperties>
        M::DefineProperties => {
            let target = expect_object(args.first())?;
            let props = expect_object(args.get(1))?;
            // Walk enumerable own keys of `props`. Each value is a
            // descriptor object that we coerce + apply.
            let entries: Vec<(String, Value)> =
                crate::object::with_properties(props, gc_heap, |p| {
                    p.enumerable_data_iter()
                        .map(|(k, v)| (k.to_string(), v))
                        .collect()
                });
            for (key, desc_value) in entries {
                let desc_obj = match desc_value {
                    Value::Object(o) => o,
                    _ => return Err(VmError::TypeMismatch),
                };
                let descriptor = coerce_to_descriptor(&desc_obj, gc_heap)?;
                if !crate::object::define_own_property_partial(target, gc_heap, &key, descriptor) {
                    return Err(VmError::TypeMismatch);
                }
            }
            Ok(Value::Object(target))
        }
        // §20.1.2.10 Object.getOwnPropertyDescriptor(O, P)
        // <https://tc39.es/ecma262/#sec-object.getownpropertydescriptor>
        M::GetOwnPropertyDescriptor => {
            let key = expect_property_key(args.get(1))?;
            match args.first() {
                Some(Value::Object(target)) => match &key {
                    PropertyKey::String(key) => {
                        match crate::object::get_own_descriptor(*target, gc_heap, key) {
                            Some(desc) => Ok(Value::Object(descriptor_to_object_with_roots(
                                &desc,
                                gc_heap,
                                &[],
                                &[args],
                            )?)),
                            None => Ok(Value::Undefined),
                        }
                    }
                    PropertyKey::Symbol(sym) => {
                        match crate::object::get_own_symbol_descriptor(*target, gc_heap, sym) {
                            Some(desc) => Ok(Value::Object(descriptor_to_object_with_roots(
                                &desc,
                                gc_heap,
                                &[],
                                &[args],
                            )?)),
                            None => Ok(Value::Undefined),
                        }
                    }
                },
                Some(Value::ClassConstructor(class)) => match &key {
                    PropertyKey::String(key) => {
                        match crate::object::get_own_descriptor(
                            class.statics(gc_heap),
                            gc_heap,
                            key,
                        ) {
                            Some(desc) => Ok(Value::Object(descriptor_to_object_with_roots(
                                &desc,
                                gc_heap,
                                &[],
                                &[args],
                            )?)),
                            None => Ok(Value::Undefined),
                        }
                    }
                    PropertyKey::Symbol(sym) => {
                        match crate::object::get_own_symbol_descriptor(
                            class.statics(gc_heap),
                            gc_heap,
                            sym,
                        ) {
                            Some(desc) => Ok(Value::Object(descriptor_to_object_with_roots(
                                &desc,
                                gc_heap,
                                &[],
                                &[args],
                            )?)),
                            None => Ok(Value::Undefined),
                        }
                    }
                },
                Some(Value::NativeFunction(native)) => {
                    let PropertyKey::String(key) = &key else {
                        return Ok(Value::Undefined);
                    };
                    match native.own_property_descriptor(gc_heap, string_heap, key)? {
                        Some(desc) => Ok(Value::Object(descriptor_to_object_with_roots(
                            &desc,
                            gc_heap,
                            &[],
                            &[args],
                        )?)),
                        None => Ok(Value::Undefined),
                    }
                }
                // §20.1.2.7 Object.getOwnPropertyDescriptor performs
                // `obj = ? ToObject(O)` first. Primitive Boolean /
                // Number / String / Symbol / BigInt coerce to their
                // wrapper, which carries no own data properties
                // matching arbitrary keys (other than indexed chars
                // and `length` on String). Returning `Undefined` for
                // the common "no such own property" case matches
                // spec without materialising a transient wrapper.
                Some(
                    Value::Boolean(_)
                    | Value::Number(_)
                    | Value::String(_)
                    | Value::Symbol(_)
                    | Value::BigInt(_),
                ) => Ok(Value::Undefined),
                Some(Value::Null) | Some(Value::Undefined) | None => Err(VmError::TypeError {
                    message:
                        "Object.getOwnPropertyDescriptor: cannot convert null/undefined to object"
                            .to_string(),
                }),
                _ => Err(VmError::TypeError {
                    message: "Object.getOwnPropertyDescriptor target must be an object".to_string(),
                }),
            }
        }
        // §20.1.2.11 Object.getOwnPropertyDescriptors(O)
        // <https://tc39.es/ecma262/#sec-object.getownpropertydescriptors>
        M::GetOwnPropertyDescriptors => {
            let target = expect_object(args.first())?;
            let target_root = Value::Object(target);
            let result = rooted_object(gc_heap, &[&target_root], &[args])?;
            let result_root = Value::Object(result);
            let (keys, symbols): (Vec<String>, Vec<JsSymbol>) =
                crate::object::with_properties(target, gc_heap, |p| {
                    (
                        p.keys().map(|s| s.to_string()).collect(),
                        p.symbol_keys().collect(),
                    )
                });
            for key in keys {
                if let Some(desc) = crate::object::get_own_descriptor(target, gc_heap, &key) {
                    let value = Value::Object(descriptor_to_object_with_roots(
                        &desc,
                        gc_heap,
                        &[&target_root, &result_root],
                        &[args],
                    )?);
                    crate::object::set(result, gc_heap, &key, value);
                }
            }
            for sym in symbols {
                if let Some(desc) = crate::object::get_own_symbol_descriptor(target, gc_heap, &sym)
                {
                    let value = Value::Object(descriptor_to_object_with_roots(
                        &desc,
                        gc_heap,
                        &[&target_root, &result_root],
                        &[args],
                    )?);
                    if !crate::object::set_symbol(result, gc_heap, sym, value) {
                        return Err(VmError::TypeMismatch);
                    }
                }
            }
            Ok(Value::Object(result))
        }
        // §20.1.2.6 Object.freeze(O)
        // <https://tc39.es/ecma262/#sec-object.freeze>
        M::Freeze => {
            let arg = args.first().cloned().unwrap_or(Value::Undefined);
            if let Value::Object(o) = &arg {
                crate::object::freeze(*o, gc_heap);
            }
            // Spec: returns the argument unchanged (non-objects pass
            // through).
            Ok(arg)
        }
        // §20.1.2.20 Object.seal(O)
        M::Seal => {
            let arg = args.first().cloned().unwrap_or(Value::Undefined);
            if let Value::Object(o) = &arg {
                crate::object::seal(*o, gc_heap);
            }
            Ok(arg)
        }
        // §20.1.2.18 Object.preventExtensions(O)
        M::PreventExtensions => {
            let arg = args.first().cloned().unwrap_or(Value::Undefined);
            if let Value::Object(o) = &arg {
                crate::object::prevent_extensions(*o, gc_heap);
            }
            Ok(arg)
        }
        // §20.1.2.15 Object.isFrozen(O)
        M::IsFrozen => {
            let arg = args.first().cloned().unwrap_or(Value::Undefined);
            // Per spec, `Object.isFrozen(non_object) === true`.
            let result = match arg {
                Value::Object(o) => crate::object::is_frozen(o, gc_heap),
                _ => true,
            };
            Ok(Value::Boolean(result))
        }
        // §20.1.2.16 Object.isSealed(O)
        M::IsSealed => {
            let arg = args.first().cloned().unwrap_or(Value::Undefined);
            let result = match arg {
                Value::Object(o) => crate::object::is_sealed(o, gc_heap),
                _ => true,
            };
            Ok(Value::Boolean(result))
        }
        // §20.1.2.14 Object.isExtensible(O)
        M::IsExtensible => {
            let arg = args.first().cloned().unwrap_or(Value::Undefined);
            // Spec: `Object.isExtensible(non_object) === false`.
            let result = match arg {
                Value::Object(o) => crate::object::is_extensible(o, gc_heap),
                Value::Function { .. }
                | Value::Closure { .. }
                | Value::BoundFunction(_)
                | Value::NativeFunction(_)
                | Value::ClassConstructor(_) => true,
                _ => false,
            };
            Ok(Value::Boolean(result))
        }
        // §20.1.2.17 Object.keys(O) — enumerable own string keys.
        // <https://tc39.es/ecma262/#sec-object.keys>
        M::Keys => {
            let owned: Vec<String> = match args.first() {
                Some(Value::Object(target)) => {
                    crate::object::with_properties(*target, gc_heap, |p| {
                        p.enumerable_keys().map(|k| k.to_string()).collect()
                    })
                }
                Some(Value::NativeFunction(native)) => native
                    .enumerable_own_property_keys(gc_heap)
                    .into_iter()
                    .collect(),
                Some(Value::BoundFunction(bound)) => {
                    crate::function_metadata::bound_enumerable_own_property_keys(bound, gc_heap)
                        .into_iter()
                        .collect()
                }
                _ => return Err(VmError::TypeMismatch),
            };
            let mut names = Vec::with_capacity(owned.len());
            for k in owned {
                names.push(string_value(&k, string_heap)?);
            }
            Ok(Value::Array(rooted_array_from_elements(
                gc_heap,
                names,
                &[],
                &[args],
            )?))
        }
        // §20.1.2.22 Object.values(O) — enumerable own data values.
        // <https://tc39.es/ecma262/#sec-object.values>
        M::Values => {
            let target = expect_object(args.first())?;
            let values: Vec<Value> = crate::object::with_properties(target, gc_heap, |p| {
                p.enumerable_data_iter().map(|(_, v)| v).collect()
            });
            let target_root = Value::Object(target);
            Ok(Value::Array(rooted_array_from_elements(
                gc_heap,
                values,
                &[&target_root],
                &[args],
            )?))
        }
        // §20.1.2.5 Object.entries(O) — `[key, value]` pairs in
        // insertion order.
        // <https://tc39.es/ecma262/#sec-object.entries>
        M::Entries => {
            let target = expect_object(args.first())?;
            let raw: Vec<(String, Value)> = crate::object::with_properties(target, gc_heap, |p| {
                p.enumerable_data_iter()
                    .map(|(k, v)| (k.to_string(), v))
                    .collect()
            });
            let mut pairs: Vec<Value> = Vec::with_capacity(raw.len());
            for (k, v) in raw {
                let key = string_value(&k, string_heap)?;
                let pair: smallvec::SmallVec<[Value; 4]> = smallvec::smallvec![key, v];
                let target_root = Value::Object(target);
                pairs.push(Value::Array(rooted_array_from_elements(
                    gc_heap,
                    pair,
                    &[&target_root],
                    &[args, pairs.as_slice()],
                )?));
            }
            let target_root = Value::Object(target);
            Ok(Value::Array(rooted_array_from_elements(
                gc_heap,
                pairs,
                &[&target_root],
                &[args],
            )?))
        }
        // §20.1.2.1 Object.assign(target, ...sources). Copies own
        // enumerable string-keyed data properties from each source
        // into `target` using `[[Set]]` (so existing accessors on
        // target invoke their setters). Foundation simplifies the
        // [[Set]] step: we use the `set()` construction helper since
        // the spec's full ladder is filed against the dispatch layer.
        // Symbol-keyed properties + non-enumerable + accessor sources
        // are left to follow-ups.
        // <https://tc39.es/ecma262/#sec-object.assign>
        M::Assign => {
            let target = expect_object(args.first())?;
            for src in args.iter().skip(1) {
                match src {
                    // Per spec, `null` / `undefined` sources are
                    // skipped silently.
                    Value::Undefined | Value::Null => continue,
                    Value::Object(o) => {
                        let entries: Vec<(String, Value)> =
                            crate::object::with_properties(*o, gc_heap, |p| {
                                p.enumerable_data_iter()
                                    .map(|(k, v)| (k.to_string(), v))
                                    .collect()
                            });
                        for (k, v) in entries {
                            crate::object::set(target, gc_heap, &k, v);
                        }
                    }
                    _ => return Err(VmError::TypeMismatch),
                }
            }
            Ok(Value::Object(target))
        }
        // §20.1.2.7 Object.fromEntries(iterable). Foundation accepts
        // an array of `[k, v]` pairs (the most common shape) and a
        // `Value::Map`; arbitrary iterables route through the user
        // iterator protocol once it lands here too — filed.
        // <https://tc39.es/ecma262/#sec-object.fromentries>
        M::FromEntries => {
            let iter = args.first().cloned().unwrap_or(Value::Undefined);
            let iter_root = iter.clone();
            let result = rooted_object(gc_heap, &[&iter_root], &[args])?;
            match iter {
                Value::Array(arr) => {
                    // Snapshot to avoid holding the array's RefCell
                    // borrow while we recurse into per-pair work.
                    let snapshot: Vec<Value> =
                        crate::array::with_elements(arr, gc_heap, |elements| elements.to_vec());
                    for entry in snapshot {
                        let (key, value) = read_entry_pair_heap(&entry, gc_heap, string_heap)?;
                        let key_str = property_key_from_value(&key)?;
                        crate::object::set(result, gc_heap, &key_str, value);
                    }
                }
                Value::Map(m) => {
                    for (key, value) in crate::collections::map_entries(m, gc_heap) {
                        let key_str = property_key_from_value(&key)?;
                        crate::object::set(result, gc_heap, &key_str, value);
                    }
                }
                _ => return Err(VmError::TypeMismatch),
            }
            Ok(Value::Object(result))
        }
        // §20.1.2.13 Object.hasOwn(O, P) — Stage 4 ergonomic
        // alternative to `Object.prototype.hasOwnProperty.call`.
        // <https://tc39.es/ecma262/#sec-object.hasown>
        M::HasOwn => {
            let target = match args.first() {
                Some(Value::Object(target)) => *target,
                Some(Value::ClassConstructor(class)) => class.statics(gc_heap),
                _ => return Err(VmError::TypeMismatch),
            };
            let present = has_own_property(target, gc_heap, args.get(1))?;
            Ok(Value::Boolean(present))
        }
        // §20.1.2.12 Object.getOwnPropertyNames(O) — every own
        // string-keyed property, regardless of enumerability.
        // <https://tc39.es/ecma262/#sec-object.getownpropertynames>
        M::GetOwnPropertyNames => {
            let owned: Vec<String> = match args.first() {
                Some(Value::Object(target)) => {
                    crate::object::with_properties(*target, gc_heap, |p| {
                        p.keys().map(|k| k.to_string()).collect()
                    })
                }
                Some(Value::NativeFunction(native)) => {
                    native.own_property_keys(gc_heap).into_iter().collect()
                }
                Some(Value::BoundFunction(bound)) => {
                    crate::function_metadata::bound_own_property_keys(bound, gc_heap)
                        .into_iter()
                        .collect()
                }
                // Ordinary functions / closures — without an
                // `ExecutionContext` we cannot honor the arrow-vs-
                // constructor branch in
                // [`Interpreter::ordinary_function_own_property_keys`].
                // The context-aware paths
                // ([`super::run_object_static_call_operands`] +
                // [`native_get_own_property_names_rooted`]) reach
                // this branch only after exhausting their own
                // handlers, so signal "no context" here and let the
                // caller fall through to the array shape it expects
                // (the realistic fast paths already produced a
                // result before landing here).
                Some(Value::Function { .. }) | Some(Value::Closure { .. }) => {
                    return Err(VmError::InvalidOperand);
                }
                Some(Value::ClassConstructor(class)) => {
                    let mut keys: Vec<String> =
                        crate::object::with_properties(class.statics(gc_heap), gc_heap, |p| {
                            p.keys().map(|k| k.to_string()).collect()
                        });
                    if !keys.iter().any(|k| k == "prototype") {
                        keys.push("prototype".to_string());
                    }
                    keys
                }
                Some(Value::Boolean(_) | Value::Number(_) | Value::Symbol(_)) => Vec::new(),
                Some(Value::String(s)) => {
                    let mut keys: Vec<String> = (0..s.len()).map(|idx| idx.to_string()).collect();
                    keys.push("length".to_string());
                    keys
                }
                _ => return Err(VmError::TypeMismatch),
            };
            let mut names: Vec<Value> = Vec::with_capacity(owned.len());
            for k in owned {
                names.push(string_value(&k, string_heap)?);
            }
            Ok(Value::Array(rooted_array_from_elements(
                gc_heap,
                names,
                &[],
                &[args],
            )?))
        }
        // §20.1.2.13 Object.getOwnPropertySymbols(O) — every own
        // symbol-keyed property. Foundation property bag is
        // string-keyed today; symbol keys are tracked in a parallel
        // table inside JsObject.
        // <https://tc39.es/ecma262/#sec-object.getownpropertysymbols>
        M::GetOwnPropertySymbols => {
            let target = expect_object(args.first())?;
            let syms: Vec<Value> = crate::object::with_properties(target, gc_heap, |p| {
                p.symbol_keys().map(Value::Symbol).collect()
            });
            let target_root = Value::Object(target);
            Ok(Value::Array(rooted_array_from_elements(
                gc_heap,
                syms,
                &[&target_root],
                &[args],
            )?))
        }
    }
}

fn string_value(s: &str, heap: &StringHeap) -> Result<Value, VmError> {
    Ok(Value::String(
        JsString::from_str(s, heap).map_err(|_| VmError::TypeMismatch)?,
    ))
}

/// Implement §6.2.5.5 ToPropertyDescriptor against `desc_obj`.
///
/// Returns a [`PartialPropertyDescriptor`] that tracks which fields
/// were present on the source object, matching the V8 / JSC /
/// SpiderMonkey descriptor-coercion shape so
/// `ValidateAndApplyPropertyDescriptor` can distinguish "absent" from
/// "present with `false`".
///
/// # Algorithm
/// - Read `value`, `writable`, `enumerable`, `configurable`, `get`,
///   `set` from the descriptor object as own data properties.
/// - Mixing accessor + data fields rejects with `TypeMismatch` per
///   step 17.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-topropertydescriptor>
pub fn coerce_to_descriptor(
    desc_obj: &JsObject,
    gc_heap: &otter_gc::GcHeap,
) -> Result<PartialPropertyDescriptor, VmError> {
    // Direct own-data probes — accessors on the descriptor object
    // itself are ignored for the slice.
    let value = crate::object::lookup_own(*desc_obj, gc_heap, "value");
    let writable = crate::object::lookup_own(*desc_obj, gc_heap, "writable");
    let enumerable = crate::object::lookup_own(*desc_obj, gc_heap, "enumerable");
    let configurable = crate::object::lookup_own(*desc_obj, gc_heap, "configurable");
    let getter = crate::object::lookup_own(*desc_obj, gc_heap, "get");
    let setter = crate::object::lookup_own(*desc_obj, gc_heap, "set");

    let has_value = !matches!(value, PropertyLookup::Absent);
    let has_writable = !matches!(writable, PropertyLookup::Absent);
    let has_get = !matches!(getter, PropertyLookup::Absent);
    let has_set = !matches!(setter, PropertyLookup::Absent);

    if (has_get || has_set) && (has_value || has_writable) {
        // §6.2.5.5 step 17 — cannot mix data and accessor fields.
        return Err(VmError::TypeMismatch);
    }

    let mut descriptor = PartialPropertyDescriptor::default();
    if has_value {
        descriptor.value = Some(match value {
            PropertyLookup::Data { value, .. } => value,
            _ => Value::Undefined,
        });
    }
    if has_writable {
        descriptor.writable = lookup_to_optional_bool(&writable);
    }
    descriptor.enumerable = lookup_to_optional_bool(&enumerable);
    descriptor.configurable = lookup_to_optional_bool(&configurable);
    if has_get {
        descriptor.get = Some(lookup_to_optional_value(&getter)?.unwrap_or(Value::Undefined));
    }
    if has_set {
        descriptor.set = Some(lookup_to_optional_value(&setter)?.unwrap_or(Value::Undefined));
    }
    Ok(descriptor)
}

fn rooted_object(
    gc_heap: &mut otter_gc::GcHeap,
    value_roots: &[&Value],
    slice_roots: &[&[Value]],
) -> Result<JsObject, VmError> {
    let mut external_visit = |visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
        for value in value_roots {
            value.trace_value_slots(visitor);
        }
        for slice in slice_roots {
            for value in *slice {
                value.trace_value_slots(visitor);
            }
        }
    };
    crate::object::alloc_object_with_roots(gc_heap, &mut external_visit).map_err(VmError::from)
}

fn rooted_array_from_elements<I>(
    gc_heap: &mut otter_gc::GcHeap,
    values: I,
    value_roots: &[&Value],
    slice_roots: &[&[Value]],
) -> Result<crate::array::JsArray, VmError>
where
    I: IntoIterator<Item = Value>,
{
    let mut external_visit = |visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
        for value in value_roots {
            value.trace_value_slots(visitor);
        }
        for slice in slice_roots {
            for value in *slice {
                value.trace_value_slots(visitor);
            }
        }
    };
    crate::array::from_elements_with_roots(gc_heap, values, &mut external_visit)
        .map_err(VmError::from)
}

fn descriptor_to_object_with_roots(
    desc: &PropertyDescriptor,
    gc_heap: &mut otter_gc::GcHeap,
    value_roots: &[&Value],
    slice_roots: &[&[Value]],
) -> Result<JsObject, VmError> {
    let mut roots = Vec::with_capacity(value_roots.len() + 2);
    roots.extend_from_slice(value_roots);
    match &desc.kind {
        DescriptorKind::Data { value } => roots.push(value),
        DescriptorKind::Accessor { getter, setter } => {
            if let Some(getter) = getter {
                roots.push(getter);
            }
            if let Some(setter) = setter {
                roots.push(setter);
            }
        }
    }
    let result = rooted_object(gc_heap, &roots, slice_roots)?;
    match &desc.kind {
        DescriptorKind::Data { value } => {
            crate::object::set(result, gc_heap, "value", value.clone());
            crate::object::set(result, gc_heap, "writable", Value::Boolean(desc.writable()));
        }
        DescriptorKind::Accessor { getter, setter } => {
            crate::object::set(
                result,
                gc_heap,
                "get",
                getter.clone().unwrap_or(Value::Undefined),
            );
            crate::object::set(
                result,
                gc_heap,
                "set",
                setter.clone().unwrap_or(Value::Undefined),
            );
        }
    }
    crate::object::set(
        result,
        gc_heap,
        "enumerable",
        Value::Boolean(desc.enumerable()),
    );
    crate::object::set(
        result,
        gc_heap,
        "configurable",
        Value::Boolean(desc.configurable()),
    );
    Ok(result)
}

fn lookup_to_optional_bool(lookup: &PropertyLookup) -> Option<bool> {
    match lookup {
        PropertyLookup::Absent => None,
        PropertyLookup::Data { value, .. } => Some(value.to_boolean()),
        // An accessor on the descriptor object would fire its getter
        // per spec; we treat as absent in the slice.
        PropertyLookup::Accessor { .. } => None,
    }
}

fn lookup_to_optional_value(lookup: &PropertyLookup) -> Result<Option<Value>, VmError> {
    match lookup {
        PropertyLookup::Absent => Ok(None),
        PropertyLookup::Data { value, .. } => match value {
            Value::Undefined => Ok(None),
            v => Ok(Some(v.clone())),
        },
        PropertyLookup::Accessor { .. } => Ok(None),
    }
}

fn expect_object(arg: Option<&Value>) -> Result<JsObject, VmError> {
    match arg {
        Some(Value::Object(o)) => Ok(*o),
        _ => Err(VmError::TypeMismatch),
    }
}

fn expect_property_key(arg: Option<&Value>) -> Result<PropertyKey, VmError> {
    match arg {
        Some(Value::String(s)) => Ok(PropertyKey::String(s.to_lossy_string())),
        Some(Value::Number(n)) => Ok(PropertyKey::String(n.to_display_string())),
        Some(Value::Boolean(b)) => Ok(PropertyKey::String(
            (if *b { "true" } else { "false" }).to_string(),
        )),
        Some(Value::Null) => Ok(PropertyKey::String("null".to_string())),
        Some(Value::Undefined) | None => Ok(PropertyKey::String("undefined".to_string())),
        Some(Value::Symbol(sym)) => Ok(PropertyKey::Symbol(sym.clone())),
        _ => Err(VmError::TypeMismatch),
    }
}

fn has_own_property(
    target: JsObject,
    gc_heap: &otter_gc::GcHeap,
    key: Option<&Value>,
) -> Result<bool, VmError> {
    match expect_property_key(key)? {
        PropertyKey::Symbol(sym) => Ok(crate::object::has_own_symbol(target, gc_heap, &sym)),
        PropertyKey::String(key) => Ok(!matches!(
            crate::object::lookup_own(target, gc_heap, &key),
            PropertyLookup::Absent
        )),
    }
}

/// §7.1.19 ToPropertyKey for a free-standing `Value`. Foundation
/// accepts `String` and `Number` operands; symbol keys take a
/// dedicated path (object_statics here is string-key only).
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-topropertykey>
fn property_key_from_value(value: &Value) -> Result<String, VmError> {
    match value {
        Value::String(s) => Ok(s.to_lossy_string()),
        Value::Number(n) => Ok(n.to_display_string()),
        Value::Boolean(b) => Ok((if *b { "true" } else { "false" }).to_string()),
        Value::Null => Ok("null".to_string()),
        Value::Undefined => Ok("undefined".to_string()),
        _ => Err(VmError::TypeMismatch),
    }
}
