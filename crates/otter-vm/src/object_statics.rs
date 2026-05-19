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
    method("groupBy", 2, native_group_by),
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
    method("__defineGetter__", 2, native_prototype_define_getter),
    method("__defineSetter__", 2, native_prototype_define_setter),
    method("__lookupGetter__", 1, native_prototype_lookup_getter),
    method("__lookupSetter__", 1, native_prototype_lookup_setter),
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
        M::GroupBy => native_group_by_rooted(ctx, context, args).map(Some),
        _ => Ok(None),
    }
}

/// §20.1.2.7 `Object.groupBy(items, callbackfn)` — groups iterable
/// `items` into a null-prototype object keyed by the callback's
/// return value. Each value is an Array of `items` in iteration
/// order. The callback receives `(item, index)`.
///
/// Foundation iterates Array operands directly. Non-Array iterables
/// would require the full `GetIterator` ladder; that path falls
/// through to the catch-all below today.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-object.groupby>
fn native_group_by_rooted(
    ctx: &mut NativeCtx<'_>,
    context: Option<&crate::ExecutionContext>,
    args: &[Value],
) -> Result<Value, VmError> {
    let items = args.first().cloned().unwrap_or(Value::Undefined);
    let callback = args.get(1).cloned().unwrap_or(Value::Undefined);
    if matches!(items, Value::Undefined | Value::Null) {
        return Err(VmError::TypeError {
            message: "Object.groupBy: items must be iterable".to_string(),
        });
    }
    if !ctx.cx.interp.is_callable_runtime(&callback) {
        return Err(VmError::TypeError {
            message: "Object.groupBy: callback must be a function".to_string(),
        });
    }
    let exec_ctx = context.cloned().ok_or_else(|| VmError::TypeError {
        message: "Object.groupBy: missing execution context".to_string(),
    })?;
    let result = ctx.alloc_object_with_roots(&[&items, &callback], &[args])?;
    crate::object::set_prototype(result, ctx.heap_mut(), None);

    // Snapshot the iterable's elements. Arrays drain through their
    // dense storage; objects with a `length` data property degrade
    // to `for (let i = 0; i < length; i++)` so spec-classic
    // array-likes are also accepted.
    let items_snapshot: Vec<Value> = match &items {
        Value::Array(arr) => {
            crate::array::with_elements(*arr, ctx.heap(), |elements| elements.to_vec())
        }
        Value::Object(obj) => {
            let length = crate::object::get(*obj, ctx.heap(), "length").unwrap_or(Value::Undefined);
            let length_n = crate::number::to_number_value(&length);
            let length_usize = if length_n.is_nan() || length_n <= 0.0 {
                0
            } else {
                length_n.min(9_007_199_254_740_991.0) as usize
            };
            let mut out: Vec<Value> = Vec::with_capacity(length_usize);
            for i in 0..length_usize {
                let key = i.to_string();
                out.push(crate::object::get(*obj, ctx.heap(), &key).unwrap_or(Value::Undefined));
            }
            out
        }
        _ => {
            return Err(VmError::TypeError {
                message: "Object.groupBy: items is not iterable".to_string(),
            });
        }
    };

    for (idx, item) in items_snapshot.iter().enumerate() {
        let mut cb_args: smallvec::SmallVec<[Value; 8]> = smallvec::SmallVec::new();
        cb_args.push(item.clone());
        cb_args.push(Value::Number(crate::number::NumberValue::from_f64(
            idx as f64,
        )));
        let key =
            ctx.cx
                .interp
                .run_callable_sync(&exec_ctx, &callback, Value::Undefined, cb_args)?;
        let key_pk = ctx.cx.interp.to_property_key_sync(&exec_ctx, key)?;
        let key_str = match key_pk {
            crate::VmPropertyKey::Symbol(_) => {
                // Symbol keys go through `set_symbol`; reuse the
                // existing arm.
                continue;
            }
            crate::VmPropertyKey::Atom(a) => a.name().to_string(),
            crate::VmPropertyKey::String(s) => s.to_string(),
            crate::VmPropertyKey::OwnedString(s) => s,
        };
        let existing = crate::object::get(result, ctx.heap(), &key_str);
        let group = match existing {
            Some(Value::Array(arr)) => arr,
            _ => {
                let arr = ctx.array_from_elements_with_roots(
                    std::iter::empty(),
                    &[&Value::Object(result), item],
                    &[args],
                )?;
                ctx.set_property(result, &key_str, Value::Array(arr))?;
                arr
            }
        };
        let value_root = item.clone();
        let arr_value = Value::Array(group);
        let roots = [&value_root, &arr_value, &Value::Object(result)];
        let mut external_visit = |visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
            for v in &roots {
                v.trace_value_slots(visitor);
            }
        };
        crate::array::push_with_roots(group, ctx.heap_mut(), item.clone(), &mut external_visit)?;
    }
    Ok(Value::Object(result))
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
                set_from_entries_key(result, &key, value, ctx)?;
            }
        }
        Value::Map(map) => {
            for (key, value) in crate::collections::map_entries(map, ctx.heap()) {
                set_from_entries_key(result, &key, value, ctx)?;
            }
        }
        _ => return Err(VmError::TypeMismatch),
    }
    Ok(Value::Object(result))
}

/// §20.1.2.7 step 5.b.iii — `CreateDataPropertyOrThrow(obj, key, value)`.
/// Supports both string-keyed and Symbol-keyed entry pairs so
/// `Object.fromEntries([[Symbol(), v]])` round-trips per
/// `built-ins/Object/fromEntries/supports-symbols.js`.
fn set_from_entries_key(
    target: crate::object::JsObject,
    key: &Value,
    value: Value,
    ctx: &mut NativeCtx<'_>,
) -> Result<(), VmError> {
    match key {
        Value::Symbol(sym) => {
            crate::object::set_symbol(target, ctx.heap_mut(), sym.clone(), value);
            Ok(())
        }
        _ => {
            let key_str = property_key_from_value(key)?;
            ctx.set_property(target, &key_str, value)
        }
    }
}

/// Heap-only variant of [`set_from_entries_key`] used by the
/// context-less `object_statics::call` fallback path.
fn set_from_entries_key_heap(
    target: crate::object::JsObject,
    key: &Value,
    value: Value,
    heap: &mut otter_gc::GcHeap,
) -> Result<(), VmError> {
    match key {
        Value::Symbol(sym) => {
            crate::object::set_symbol(target, heap, sym.clone(), value);
            Ok(())
        }
        _ => {
            let key_str = property_key_from_value(key)?;
            crate::object::set(target, heap, &key_str, value);
            Ok(())
        }
    }
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
        Some(Value::NativeFunction(native)) => match &key {
            PropertyKey::String(key) => native.own_property_descriptor(
                ctx.heap(),
                &ctx.cx.interp.string_heap_clone(),
                key,
            )?,
            PropertyKey::Symbol(sym) => native.own_symbol_property_descriptor(ctx.heap(), sym),
        },
        // §10.4.5.1 IntegerIndexedExoticObject [[GetOwnProperty]] —
        // canonical-numeric-index string keys produce a data
        // descriptor for the live element when in range, otherwise
        // undefined. Symbol / non-numeric keys have no own
        // descriptor on the bare TypedArray exotic.
        // <https://tc39.es/ecma262/#sec-integer-indexed-exotic-objects-getownproperty-p>
        Some(Value::TypedArray(target)) => match &key {
            PropertyKey::String(k) => {
                if let Some(n) = crate::property_dispatch::canonical_numeric_index_string(k) {
                    if target.buffer().is_detached()
                        || !n.is_finite()
                        || n.fract() != 0.0
                        || n < 0.0
                        || (n as usize) >= target.length()
                    {
                        None
                    } else {
                        Some(crate::object::PropertyDescriptor::data(
                            target.get(n as usize),
                            true,
                            true,
                            true,
                        ))
                    }
                } else if let Some(bag) = target.expando() {
                    crate::object::get_own_descriptor(bag, ctx.heap(), k)
                } else {
                    None
                }
            }
            PropertyKey::Symbol(sym) => target
                .expando()
                .and_then(|bag| crate::object::get_own_symbol_descriptor(bag, ctx.heap(), sym)),
        },
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
            ctx.set_property(result, &key, Value::Object(desc_obj))?;
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
            ctx.set_property(result, "value", value.clone())?;
            ctx.set_property(result, "writable", Value::Boolean(desc.writable()))?;
        }
        DescriptorKind::Accessor { getter, setter } => {
            ctx.set_property(result, "get", getter.clone().unwrap_or(Value::Undefined))?;
            ctx.set_property(result, "set", setter.clone().unwrap_or(Value::Undefined))?;
        }
    }
    ctx.set_property(result, "enumerable", Value::Boolean(desc.enumerable()))?;
    ctx.set_property(result, "configurable", Value::Boolean(desc.configurable()))?;
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
native_object_static!(native_group_by, GroupBy);

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

/// §20.1.3.5 `Object.prototype.toLocaleString ( [ reserved1 [ , reserved2 ] ] )`.
///
/// 1. Let `O` be the this value.
/// 2. Return `? Invoke(O, "toString")`.
///
/// Routes through the `Invoke` ladder so user-installed
/// `Boolean.prototype.toString` / `Number.prototype.toString` /
/// other receiver-side overrides are observable. Falls back to
/// `Object.prototype.toString` when no execution context is wired
/// (sync-only fast path used by some embedders).
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-object.prototype.tolocalestring>
fn native_prototype_to_locale_string(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    let this_value = ctx.this_value().clone();
    if let Some(context) = ctx.execution_context().cloned() {
        let callee = ctx
            .cx
            .interp
            .get_property_value_for_call(&context, this_value.clone(), "toString")
            .map_err(|err| object_native_error("toLocaleString", err))?;
        if crate::is_callable_value(&callee) {
            let result = ctx
                .cx
                .interp
                .run_callable_sync(&context, &callee, this_value, smallvec::SmallVec::new())
                .map_err(|err| object_native_error("toLocaleString", err))?;
            return Ok(result);
        }
    }
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
            let key = expect_property_key(args.first())
                .map_err(|err| object_native_error("propertyIsEnumerable", err))?;
            let desc = match key {
                PropertyKey::String(key) => native
                    .own_property_descriptor(ctx.heap(), &ctx.cx.interp.string_heap_clone(), &key)
                    .map_err(|err| object_native_error("propertyIsEnumerable", err.into()))?,
                PropertyKey::Symbol(sym) => native.own_symbol_property_descriptor(ctx.heap(), &sym),
            };
            desc.as_ref().is_some_and(PropertyDescriptor::enumerable)
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

/// §B.2.2.1.1 `get Object.prototype.__proto__` — returns the
/// receiver's `[[Prototype]]`.
///
/// 1. Let `O` be `? ToObject(this value)`.
/// 2. Return `? O.[[GetPrototypeOf]]()`.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-get-object.prototype.__proto__>
pub fn native_prototype_proto_get(
    ctx: &mut NativeCtx<'_>,
    _args: &[Value],
) -> Result<Value, NativeError> {
    let this_value = ctx.this_value().clone();
    let obj = match this_value {
        Value::Object(o) => o,
        Value::Null | Value::Undefined => {
            return Err(NativeError::TypeError {
                name: "get __proto__",
                reason: "cannot convert null or undefined to object".to_string(),
            });
        }
        // Primitives ToObject-coerce; the wrapper's prototype is the
        // constructor's `%Prototype%`. The Object intrinsic gives us
        // that resolution path directly without materialising the
        // wrapper.
        _ => {
            let name = match ctx.this_value() {
                Value::Boolean(_) => "Boolean",
                Value::Number(_) => "Number",
                Value::String(_) => "String",
                Value::Symbol(_) => "Symbol",
                Value::BigInt(_) => "BigInt",
                _ => return Ok(Value::Null),
            };
            return Ok(ctx
                .cx
                .interp
                .constructor_prototype_value(name)
                .unwrap_or(Value::Null));
        }
    };
    Ok(crate::object::prototype_value(obj, ctx.heap()).unwrap_or(Value::Null))
}

/// §B.2.2.1.2 `set Object.prototype.__proto__` — installs a new
/// `[[Prototype]]`.
///
/// 1. Let `O` be `? RequireObjectCoercible(this value)`.
/// 2. If `Type(proto)` is neither Object nor Null, return undefined.
/// 3. If `Type(O)` is not Object, return undefined.
/// 4. Let `status` be `? O.[[SetPrototypeOf]](proto)`.
/// 5. If `status` is false, throw a TypeError.
/// 6. Return undefined.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-set-object.prototype.__proto__>
pub fn native_prototype_proto_set(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    let this_value = ctx.this_value().clone();
    if matches!(this_value, Value::Null | Value::Undefined) {
        return Err(NativeError::TypeError {
            name: "set __proto__",
            reason: "cannot convert null or undefined to object".to_string(),
        });
    }
    let obj = match this_value {
        Value::Object(o) => o,
        _ => return Ok(Value::Undefined),
    };
    let proto_value = args.first().cloned().unwrap_or(Value::Undefined);
    let proto_arg = match &proto_value {
        Value::Object(_) | Value::Null | Value::Proxy(_) => Some(proto_value.clone()),
        _ => return Ok(Value::Undefined),
    };
    let ok = crate::object::set_prototype_value(obj, ctx.heap_mut(), proto_arg);
    if !ok {
        return Err(NativeError::TypeError {
            name: "set __proto__",
            reason: "cyclic or non-extensible prototype chain".to_string(),
        });
    }
    Ok(Value::Undefined)
}

/// §B.2.2.2 `Object.prototype.__defineGetter__(P, getter)`.
///
/// 1. Let `O` be `? ToObject(this value)`.
/// 2. If `IsCallable(getter)` is false, throw a TypeError.
/// 3. Let `desc` be `PropertyDescriptor { [[Get]]: getter, [[Enumerable]]: true,
///    [[Configurable]]: true }`.
/// 4. Let `key` be `? ToPropertyKey(P)`.
/// 5. Perform `? DefinePropertyOrThrow(O, key, desc)`.
/// 6. Return undefined.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-object.prototype.__defineGetter__>
fn native_prototype_define_getter(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    define_accessor_helper(ctx, args, /* is_setter */ false, "__defineGetter__")
}

/// §B.2.2.3 `Object.prototype.__defineSetter__(P, setter)`.
///
/// Mirror of [`native_prototype_define_getter`] for `[[Set]]`.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-object.prototype.__defineSetter__>
fn native_prototype_define_setter(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    define_accessor_helper(ctx, args, /* is_setter */ true, "__defineSetter__")
}

fn define_accessor_helper(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    is_setter: bool,
    method_name: &'static str,
) -> Result<Value, NativeError> {
    let this_value = ctx.this_value().clone();
    let target = match &this_value {
        Value::Object(o) => *o,
        Value::Null | Value::Undefined => {
            return Err(NativeError::TypeError {
                name: method_name,
                reason: "cannot convert null or undefined to object".to_string(),
            });
        }
        _ => {
            // §7.1.18 ToObject — primitives wrap. The accessor lands
            // on the transient wrapper which is discarded once the
            // call returns, mirroring V8/JSC. Tests against
            // Object.prototype.__defineGetter__ use plain objects.
            return Ok(Value::Undefined);
        }
    };
    let callable = args.get(1).cloned().unwrap_or(Value::Undefined);
    if !crate::is_callable_value(&callable) {
        return Err(NativeError::TypeError {
            name: method_name,
            reason: "argument is not a function".to_string(),
        });
    }
    let key =
        expect_property_key(args.first()).map_err(|err| object_native_error(method_name, err))?;
    let desc = if is_setter {
        PropertyDescriptor::accessor(None, Some(callable), true, true)
    } else {
        PropertyDescriptor::accessor(Some(callable), None, true, true)
    };
    let ok = match key {
        PropertyKey::String(name) => {
            crate::object::define_own_property(target, ctx.heap_mut(), &name, desc)
        }
        PropertyKey::Symbol(sym) => {
            crate::object::define_own_symbol_property(target, ctx.heap_mut(), &sym, desc)
        }
    };
    if !ok {
        return Err(NativeError::TypeError {
            name: method_name,
            reason: "cannot redefine property".to_string(),
        });
    }
    Ok(Value::Undefined)
}

/// §B.2.2.4 `Object.prototype.__lookupGetter__(P)`.
///
/// 1. Let `O` be `? ToObject(this value)`.
/// 2. Let `key` be `? ToPropertyKey(P)`.
/// 3. Repeat:
///    a. Let `desc` be `? O.[[GetOwnProperty]](key)`.
///    b. If `desc` is not undefined, then
///       i. If `IsAccessorDescriptor(desc)` is true, return `desc.[[Get]]`.
///       ii. Return undefined.
///    c. Let `O` be `? O.[[GetPrototypeOf]]()`.
///    d. If `O` is null, return undefined.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-object.prototype.__lookupGetter__>
fn native_prototype_lookup_getter(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    lookup_accessor_helper(
        ctx,
        args,
        /* lookup_setter */ false,
        "__lookupGetter__",
    )
}

/// §B.2.2.5 `Object.prototype.__lookupSetter__(P)`. Mirror for `[[Set]]`.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-object.prototype.__lookupSetter__>
fn native_prototype_lookup_setter(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    lookup_accessor_helper(ctx, args, /* lookup_setter */ true, "__lookupSetter__")
}

fn lookup_accessor_helper(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    lookup_setter: bool,
    method_name: &'static str,
) -> Result<Value, NativeError> {
    let this_value = ctx.this_value().clone();
    let mut current = match this_value {
        Value::Object(o) => Some(o),
        Value::Null | Value::Undefined => {
            return Err(NativeError::TypeError {
                name: method_name,
                reason: "cannot convert null or undefined to object".to_string(),
            });
        }
        _ => return Ok(Value::Undefined),
    };
    let key =
        expect_property_key(args.first()).map_err(|err| object_native_error(method_name, err))?;
    while let Some(obj) = current {
        let lookup = match &key {
            PropertyKey::String(name) => crate::object::lookup_own(obj, ctx.heap(), name),
            PropertyKey::Symbol(sym) => crate::object::lookup_own_symbol(obj, ctx.heap(), sym),
        };
        match lookup {
            PropertyLookup::Accessor { getter, setter, .. } => {
                let value = if lookup_setter { setter } else { getter };
                return Ok(value.unwrap_or(Value::Undefined));
            }
            PropertyLookup::Data { .. } => return Ok(Value::Undefined),
            PropertyLookup::Absent => {
                current = crate::object::prototype(obj, ctx.heap());
            }
        }
    }
    Ok(Value::Undefined)
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
        // §22.2.6.15: `%TypedArray%.prototype[@@toStringTag]`
        // is an accessor that yields the receiver's
        // [[TypedArrayName]] — the kind-specific name string —
        // not the generic `"TypedArray"` family tag.
        Value::TypedArray(t) => t.kind().name(),
        Value::Object(obj) if crate::object::date_data(*obj, ctx.heap()).is_some() => "Date",
        Value::Object(obj) if crate::object::call_native(*obj, ctx.heap()).is_some() => "Function",
        // §20.1.3.6 step 14.b — if `O` has an `[[ErrorData]]` internal
        // slot, the built-in tag is `"Error"`. Otter does not carry
        // an explicit slot; treat any ordinary object whose prototype
        // chain reaches one of the realm error prototypes as having
        // the slot.
        Value::Object(obj) if object_has_error_data(ctx, *obj) => "Error",
        Value::Object(_) | Value::Proxy(_) => "Object",
    }
    .to_string()
}

/// Walk `obj`'s `[[Prototype]]` chain and return `true` when any
/// realm error prototype is reached. Used as a substitute for the
/// spec's `[[ErrorData]]` internal slot, which Otter does not carry
/// on ordinary object instances.
fn object_has_error_data(ctx: &NativeCtx<'_>, obj: crate::object::JsObject) -> bool {
    use crate::ErrorKind;
    let heap = ctx.heap();
    let registry = &ctx.cx.interp.error_classes;
    let mut current = Some(obj);
    let mut hops = 0;
    while let Some(o) = current {
        if hops >= crate::object::PROTO_CHAIN_HARD_CAP {
            return false;
        }
        hops += 1;
        for kind in [
            ErrorKind::Error,
            ErrorKind::TypeError,
            ErrorKind::RangeError,
            ErrorKind::SyntaxError,
            ErrorKind::ReferenceError,
            ErrorKind::URIError,
            ErrorKind::EvalError,
            ErrorKind::AggregateError,
        ] {
            if registry.prototype(kind) == o {
                return true;
            }
        }
        current = crate::object::prototype(o, heap);
    }
    false
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
        Ok(PropertyKey::Symbol(sym)) => native
            .own_symbol_property_descriptor(gc_heap, &sym)
            .is_some(),
        Err(_) => false,
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
                    let ok = match &key {
                        PropertyKey::String(key) => native.define_own_property(
                            gc_heap,
                            string_heap,
                            key,
                            descriptor.complete_for_new_property(),
                        ),
                        PropertyKey::Symbol(sym) => {
                            native.define_own_symbol_property(gc_heap, sym, descriptor)
                        }
                    };
                    if !ok {
                        return Err(VmError::TypeError {
                            message: format!(
                                "Cannot define property '{}' on function {}",
                                key.label(),
                                native.name(gc_heap)
                            ),
                        });
                    }
                    Ok(Value::NativeFunction(*native))
                }
                // RegExp instances expose `lastIndex` and the
                // expando bag for ordinary defineProperty.
                Some(Value::RegExp(r)) => {
                    let r = *r;
                    let bag = crate::property_dispatch::regexp_ensure_expando_pub(gc_heap, &r)?;
                    let ok = match &key {
                        PropertyKey::String(k) => {
                            crate::object::define_own_property_partial(bag, gc_heap, k, descriptor)
                        }
                        PropertyKey::Symbol(sym) => {
                            crate::object::define_own_symbol_property_partial(
                                bag, gc_heap, sym, descriptor,
                            )
                        }
                    };
                    if !ok {
                        return Err(VmError::TypeError {
                            message: format!("Cannot define property '{}'", key.label()),
                        });
                    }
                    Ok(Value::RegExp(r))
                }
                // Promise instances also expose the lazy expando
                // bag through Object.defineProperty.
                Some(Value::Promise(p)) => {
                    let p = *p;
                    let bag = crate::property_dispatch::promise_ensure_expando_pub(gc_heap, &p)?;
                    let ok = match &key {
                        PropertyKey::String(k) => {
                            crate::object::define_own_property_partial(bag, gc_heap, k, descriptor)
                        }
                        PropertyKey::Symbol(sym) => {
                            crate::object::define_own_symbol_property_partial(
                                bag, gc_heap, sym, descriptor,
                            )
                        }
                    };
                    if !ok {
                        return Err(VmError::TypeError {
                            message: format!("Cannot define property '{}'", key.label()),
                        });
                    }
                    Ok(Value::Promise(p))
                }
                // §10.4.5.3 IntegerIndexedExoticObject
                // [[DefineOwnProperty]] — canonical-numeric-index
                // keys verify the index against the live element
                // (writable / enumerable / configurable / value);
                // everything else falls through to OrdinaryDefine
                // against the lazy expando bag.
                Some(Value::TypedArray(t)) => {
                    let t = t.clone();
                    match &key {
                        PropertyKey::String(k) => {
                            if let Some(n) =
                                crate::property_dispatch::canonical_numeric_index_string(k)
                            {
                                if t.buffer().is_detached()
                                    || !n.is_finite()
                                    || n.fract() != 0.0
                                    || n < 0.0
                                    || (n as usize) >= t.length()
                                    || descriptor.configurable == Some(false)
                                    || descriptor.enumerable == Some(false)
                                    || descriptor.writable == Some(false)
                                    || descriptor.is_accessor()
                                {
                                    return Err(VmError::TypeError {
                                        message: format!(
                                            "Cannot define property '{}'",
                                            key.label()
                                        ),
                                    });
                                }
                                if let Some(value) = descriptor.value.clone() {
                                    let coerced =
                                        crate::binary::dispatch::coerce_element_for_store(
                                            t.kind(),
                                            &value,
                                        )?;
                                    t.set(n as usize, &coerced);
                                }
                            } else {
                                let bag = crate::property_dispatch::typed_array_ensure_expando_pub(
                                    gc_heap, &t,
                                )?;
                                if !crate::object::define_own_property_partial(
                                    bag, gc_heap, k, descriptor,
                                ) {
                                    return Err(VmError::TypeError {
                                        message: format!(
                                            "Cannot define property '{}'",
                                            key.label()
                                        ),
                                    });
                                }
                            }
                        }
                        PropertyKey::Symbol(sym) => {
                            let bag = crate::property_dispatch::typed_array_ensure_expando_pub(
                                gc_heap, &t,
                            )?;
                            if !crate::object::define_own_symbol_property_partial(
                                bag, gc_heap, sym, descriptor,
                            ) {
                                return Err(VmError::TypeError {
                                    message: format!("Cannot define property '{}'", key.label()),
                                });
                            }
                        }
                    }
                    Ok(Value::TypedArray(t))
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
                        set_from_entries_key_heap(result, &key, value, gc_heap)?;
                    }
                }
                Value::Map(m) => {
                    for (key, value) in crate::collections::map_entries(m, gc_heap) {
                        set_from_entries_key_heap(result, &key, value, gc_heap)?;
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
        // §20.1.2.7 `Object.groupBy(items, callbackfn)` — the
        // context-less fallback path can't run the callback, so it
        // routes through the rooted entrypoint above. Reaching this
        // arm means the call site bypassed `native_rooted_call`
        // (e.g. through `Reflect.apply` without a live execution
        // context); surface as a TypeError so the caller learns the
        // method needs a JS frame.
        M::GroupBy => Err(VmError::TypeError {
            message: "Object.groupBy requires an active execution context".to_string(),
        }),
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
