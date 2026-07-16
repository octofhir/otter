//! Free functions backing the dispatch loop and builtins glue.
//!
//! # Contents
//! URL resolution for `import.meta.resolve`, register read/write,
//! iterator stepping (`step_iterator`, `GeneratorResumeKind`),
//! callability checks, and small property-key coercions. Re-exported
//! from the crate root where previously public.
#![allow(unused_imports)]
use crate::*;

/// Resolve `specifier` against `referrer`, mirroring the WHATWG URL
/// join semantics used by `import.meta.resolve`. Foundation handles:
///
/// - Absolute URLs (any scheme `xxx://`) and `file://` paths pass
///   through unchanged.
/// - Relative paths (`./foo`, `../bar`, `bar.ts`) join against the
///   referrer's directory.
/// - Bare specifiers without a referrer return as-is so the embedder's
///   resolver can pick them up.
///
/// # See also
/// - <https://html.spec.whatwg.org/multipage/webappapis.html#resolve-a-module-specifier>
pub(crate) fn resolve_relative_url(referrer: Option<&str>, specifier: &str) -> String {
    // Absolute URLs / data: URIs etc. pass through.
    if specifier.contains("://") || specifier.starts_with("data:") {
        return specifier.to_string();
    }
    let Some(referrer) = referrer else {
        return specifier.to_string();
    };
    if referrer.is_empty() {
        return specifier.to_string();
    }
    if specifier.starts_with('/') {
        // Replace path component of referrer.
        if let Some(scheme_end) = referrer.find("://") {
            let after = scheme_end + 3;
            let host_end = referrer[after..]
                .find('/')
                .map(|i| after + i)
                .unwrap_or(referrer.len());
            return format!("{}{}", &referrer[..host_end], specifier);
        }
        return specifier.to_string();
    }
    // Relative path — pop referrer's last path segment and join.
    let dir_end = referrer.rfind('/').unwrap_or(referrer.len());
    let dir = &referrer[..dir_end];
    let mut parts: Vec<&str> = if dir.contains("://") {
        let scheme_end = dir.find("://").map(|i| i + 3).unwrap_or(0);
        let mut acc = vec![&dir[..scheme_end]];
        acc.extend(dir[scheme_end..].split('/'));
        acc
    } else {
        dir.split('/').collect()
    };
    for component in specifier.split('/') {
        match component {
            "" | "." => continue,
            ".." => {
                if parts.last().is_some_and(|s| !s.contains("://")) {
                    parts.pop();
                }
            }
            other => parts.push(other),
        }
    }
    parts.join("/")
}

/// Foundation §20.1.3 `Object.prototype.<method>` interception for
/// ordinary objects. Returns `Ok(Some(value))` when the call was
/// dispatched here, `Ok(None)` when the method is not one of the
/// prototype names so the caller falls through to the regular lookup.
///
/// Handles: `hasOwnProperty`, `propertyIsEnumerable`,
/// `isPrototypeOf`, `toString`, `valueOf`.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-properties-of-the-object-prototype-object>
pub(crate) fn object_prototype_intercept(
    obj: &object::JsObject,
    name: &str,
    args: &SmallVec<[Value; 8]>,
    gc_heap: &mut otter_gc::GcHeap,
    function_prototype: Option<object::JsObject>,
) -> Result<Option<Value>, VmError> {
    match name {
        // §20.1.3.2 Object.prototype.hasOwnProperty(V)
        // <https://tc39.es/ecma262/#sec-object.prototype.hasownproperty>
        "hasOwnProperty" => {
            let key = property_key_from_arg(args.first(), gc_heap)?;
            let present = !matches!(
                object::lookup_own(*obj, gc_heap, &key),
                object::PropertyLookup::Absent
            );
            Ok(Some(Value::boolean(present)))
        }
        // §20.1.3.4 Object.prototype.propertyIsEnumerable(V)
        // <https://tc39.es/ecma262/#sec-object.prototype.propertyisenumerable>
        "propertyIsEnumerable" => {
            let key = property_key_from_arg(args.first(), gc_heap)?;
            let result = match object::lookup_own(*obj, gc_heap, &key) {
                object::PropertyLookup::Data { flags, .. } => flags.enumerable(),
                object::PropertyLookup::Accessor { flags, .. } => flags.enumerable(),
                object::PropertyLookup::Absent => false,
            };
            Ok(Some(Value::boolean(result)))
        }
        // §20.1.3.3 Object.prototype.isPrototypeOf(V)
        // <https://tc39.es/ecma262/#sec-object.prototype.isprototypeof>
        "isPrototypeOf" => {
            let result = args.first().is_some_and(|value| {
                value_has_prototype_in_chain(value, *obj, gc_heap, function_prototype)
            });
            Ok(Some(Value::boolean(result)))
        }
        // §20.1.3.6 / §20.5.3.4 — `toString()`. Error instances
        // override Object.prototype.toString to return
        // `<name>: <message>`; plain objects fall back to
        // `[object Object]`. The Error path routes through
        // [`error_classes::render_error_to_string`] so the
        // user-facing call and the unwind diagnostic share one
        // implementation.
        // <https://tc39.es/ecma262/#sec-object.prototype.tostring>
        // <https://tc39.es/ecma262/#sec-error.prototype.tostring>
        "toString" => {
            let recv_value = Value::object(*obj);
            let has_error_shape = object::get(*obj, gc_heap, "name").is_some()
                || object::get(*obj, gc_heap, "message").is_some();
            let display = if has_error_shape {
                let rendered = error_classes::render_error_to_string(&recv_value, gc_heap);
                if rendered.is_empty() {
                    "[object Object]".to_string()
                } else {
                    rendered
                }
            } else {
                "[object Object]".to_string()
            };
            let s = JsString::from_str(&display, gc_heap).map_err(|_| VmError::TypeMismatch)?;
            Ok(Some(Value::string(s)))
        }
        // §20.1.3.7 Object.prototype.valueOf() — returns the receiver.
        // <https://tc39.es/ecma262/#sec-object.prototype.valueof>
        "valueOf" => Ok(Some(Value::object(*obj))),
        _ => Ok(None),
    }
}

pub(crate) fn value_has_prototype_in_chain(
    value: &Value,
    target: object::JsObject,
    gc_heap: &otter_gc::GcHeap,
    function_prototype: Option<object::JsObject>,
) -> bool {
    if let Some(obj) = value.as_object() {
        if object_has_construct_slot(value, gc_heap) {
            function_value_has_prototype_in_chain(target, gc_heap, function_prototype)
        } else {
            object::has_in_proto_chain(obj, gc_heap, target)
        }
    } else if value.is_function()
        || value.is_closure()
        || value.is_bound_function()
        || value.is_native_function()
        || value.is_class_constructor()
    {
        function_value_has_prototype_in_chain(target, gc_heap, function_prototype)
    } else {
        false
    }
}

pub(crate) fn function_value_has_prototype_in_chain(
    target: object::JsObject,
    gc_heap: &otter_gc::GcHeap,
    function_prototype: Option<object::JsObject>,
) -> bool {
    let Some(function_prototype) = function_prototype else {
        return false;
    };
    function_prototype == target || object::has_in_proto_chain(function_prototype, gc_heap, target)
}

pub(crate) fn descriptor_value(desc: &crate::object::PropertyDescriptor) -> Value {
    match &desc.kind {
        crate::object::DescriptorKind::Data { value } => *value,
        crate::object::DescriptorKind::Accessor { .. } => Value::undefined(),
    }
}

pub(crate) fn value_kind_name(value: &Value) -> &'static str {
    if value.is_undefined() || value.is_hole() {
        "undefined"
    } else if value.is_null() {
        "null"
    } else if value.is_boolean() {
        "boolean"
    } else if value.is_number() {
        "number"
    } else if value.is_string() {
        "string"
    } else if value.is_symbol() {
        "symbol"
    } else if value.is_big_int() {
        "bigint"
    } else if value.is_object() {
        "object"
    } else if value.is_array() {
        "array"
    } else if value.is_function()
        || value.is_closure()
        || value.is_native_function()
        || value.is_bound_function()
    {
        "function"
    } else if value.is_class_constructor() {
        "class constructor"
    } else if value.is_regexp() {
        "regexp"
    } else if value.is_promise() {
        "promise"
    } else if value.is_proxy() {
        "proxy"
    } else if value.is_map() {
        "map"
    } else if value.is_set() {
        "set"
    } else if value.is_weak_map() {
        "weakmap"
    } else if value.is_weak_set() {
        "weakset"
    } else if value.is_weak_ref() {
        "weakref"
    } else if value.is_finalization_registry() {
        "finalization registry"
    } else if value.is_generator() {
        "generator"
    } else if value.is_iterator() {
        "iterator"
    } else if value.is_temporal() {
        "temporal"
    } else if value.is_intl() {
        "intl"
    } else if value.is_array_buffer() {
        "arraybuffer"
    } else if value.is_data_view() {
        "dataview"
    } else if value.is_typed_array() {
        "typedarray"
    } else {
        "unknown"
    }
}

/// §7.1.19 ToPropertyKey for a single optional argument used by
/// `Object.prototype.hasOwnProperty` / `propertyIsEnumerable`.
pub(crate) fn property_key_from_arg(
    arg: Option<&Value>,
    heap: &otter_gc::GcHeap,
) -> Result<String, VmError> {
    let Some(v) = arg else {
        return Ok("undefined".to_string());
    };
    if let Some(s) = v.as_string(heap) {
        Ok(s.to_lossy_string(heap))
    } else if let Some(n) = v.as_number() {
        Ok(n.to_display_string())
    } else if let Some(b) = v.as_boolean() {
        Ok((if b { "true" } else { "false" }).to_string())
    } else if v.is_null() {
        Ok("null".to_string())
    } else if v.is_undefined() {
        Ok("undefined".to_string())
    } else {
        Err(VmError::TypeMismatch)
    }
}

pub(crate) fn to_length(value: &Value, heap: &otter_gc::GcHeap) -> Result<usize, VmError> {
    if value.is_symbol() || value.is_big_int() {
        return Err(VmError::TypeMismatch);
    }
    let n = number::to_number_value(value, heap);
    if n.is_nan() || n <= 0.0 {
        return Ok(0);
    }
    if n.is_infinite() {
        return Ok(9_007_199_254_740_991);
    }
    let len = n.trunc().min(9_007_199_254_740_991.0);
    if len > usize::MAX as f64 {
        Ok(usize::MAX)
    } else {
        Ok(len as usize)
    }
}

/// Validate that the first callback argument to an Array method is
/// callable per ECMA-262 §23.1.3 step 3 (CheckObjectCoercible +
/// IsCallable). Returns the callable value cloned out for the
/// dispatch loop.
pub(crate) fn require_callable(arg: Option<&Value>) -> Result<Value, VmError> {
    match arg {
        Some(v) if abstract_ops::is_callable(v) => Ok(*v),
        _ => Err(VmError::NotCallable),
    }
}

pub(crate) fn read_register(frame: &Frame, idx: u16) -> Result<&Value, VmError> {
    frame
        .registers
        .get(idx as usize)
        .ok_or(VmError::InvalidOperand)
}

pub(crate) fn write_register(frame: &mut Frame, idx: u16, value: Value) -> Result<(), VmError> {
    crate::ActiveFrameMut::materialized(frame).write(idx, value)
}

/// Build the native callable that `arr[Symbol.iterator]` evaluates
/// to. Invoking the returned function (with any `this`) yields a
/// fresh iterator over the captured array — matching the
/// surface of `Array.prototype[@@iterator]` from
/// [ECMA-262 §23.1.5.1](https://tc39.es/ecma262/#sec-array.prototype-@@iterator).
///
/// # Invariants
/// - Capturing the array by handle means the iterator observes
///   subsequent in-place mutations through the same `JsArray`,
///   matching real-engine `Array.prototype[Symbol.iterator]`
///   semantics.
///
/// `String.prototype[Symbol.iterator]()` — receiver-dispatched
/// shim that materialises a string iterator from the calling
/// `this` value. Installed as the realm's iterator method per
/// §22.1.3.34.
pub(crate) fn string_proto_iterator(
    ctx: &mut NativeCtx<'_>,
    _args: &[Value],
) -> Result<Value, NativeError> {
    const NAME: &str = "String.prototype[Symbol.iterator]";
    let this = *ctx.this_value();
    // §22.1.3.34 — RequireObjectCoercible(this), then `S = ?
    // ToString(this)`: the method is generic, so a plain-object
    // receiver runs its own `toString` / `valueOf` / `@@toPrimitive`
    // (and an abrupt completion from there propagates).
    if this.is_nullish() {
        return Err(NativeError::TypeError {
            name: NAME,
            reason: "called on null or undefined".to_string(),
        });
    }
    let string = if let Some(s) = this.as_string(ctx.heap()) {
        s
    } else if let Some(obj) = this.as_object()
        && let Some(s) = crate::object::string_data(obj, ctx.heap())
    {
        s
    } else {
        let (interp, exec) = ctx.interp_mut_and_context();
        let exec = exec.ok_or_else(|| NativeError::TypeError {
            name: NAME,
            reason: "missing execution context".to_string(),
        })?;
        let text = interp
            .coerce_to_string(&exec, &this)
            .map_err(|e| crate::native_function::vm_to_native_error(interp, e, NAME))?;
        JsString::from_str(&text, ctx.heap_mut()).map_err(|_| NativeError::TypeError {
            name: NAME,
            reason: "out of memory".to_string(),
        })?
    };
    let state = IteratorState::String { string, index: 0 };
    Ok(Value::iterator(ctx.alloc_iterator_state(
        state,
        &[],
        &[],
    )?))
}

/// Install `String.prototype[Symbol.iterator]` per §22.1.3.34.
pub(crate) fn install_string_iterator_post_bootstrap(
    heap: &mut otter_gc::GcHeap,
    global: crate::object::JsObject,
    well_known: &symbol::WellKnownSymbols,
) -> Result<(), crate::js_surface::JsSurfaceError> {
    let Some(string_ctor) = crate::object::get(global, heap, "String") else {
        return Ok(());
    };
    let prototype = if let Some(string_ctor) = string_ctor.as_object() {
        crate::object::get(string_ctor, heap, "prototype").and_then(|v| v.as_object())
    } else if let Some(string_ctor) = string_ctor.as_native_function() {
        string_ctor
            .own_property_descriptor(heap, "prototype")
            .ok()
            .flatten()
            .and_then(|desc| match desc.kind {
                crate::object::DescriptorKind::Data { value } => value.as_object(),
                crate::object::DescriptorKind::Accessor { .. } => None,
            })
    } else {
        None
    };
    let Some(prototype) = prototype else {
        return Ok(());
    };
    let global_root = Value::object(global);
    let prototype_root = Value::object(prototype);
    let getter = crate::bootstrap::native_static_with_value_roots(
        heap,
        "[Symbol.iterator]",
        0,
        string_proto_iterator,
        &[&global_root, &prototype_root],
    )
    .map_err(|_| crate::js_surface::JsSurfaceError::OutOfMemory)?;
    let sym = well_known.get(symbol::WellKnown::Iterator);
    crate::object::define_own_symbol_property_partial(
        prototype,
        heap,
        sym,
        crate::object::PartialPropertyDescriptor {
            value: Some(Value::native_function(getter)),
            writable: Some(true),
            enumerable: Some(false),
            configurable: Some(true),
            ..Default::default()
        },
    );
    Ok(())
}

#[cfg(test)]
pub(crate) fn make_array_iterator_factory(
    array: JsArray,
    heap: &mut otter_gc::GcHeap,
) -> Result<Value, otter_gc::OutOfMemory> {
    native_value_with_captures(
        heap,
        "Array[Symbol.iterator]",
        smallvec::smallvec![Value::array(array)],
        array_iterator_factory_call,
    )
}

#[cfg(test)]
pub(crate) fn array_iterator_factory_call(
    ctx: &mut NativeCtx<'_>,
    _: &[Value],
    captures: &[Value],
) -> Result<Value, NativeError> {
    let Some(array) = captures.first().and_then(|v| v.as_array()) else {
        return Err(NativeError::TypeError {
            name: "Array[Symbol.iterator]",
            reason: "missing traced array capture".to_string(),
        });
    };
    let state = IteratorState::Array {
        array,
        index: 0,
        origin: BuiltinIteratorOrigin::Array,
    };
    Ok(Value::iterator(ctx.alloc_iterator_state(
        state,
        &[],
        &[],
    )?))
}

/// Generator resume entry per ECMA-262 §27.5.3.
#[derive(Debug, Clone)]
pub enum GeneratorResumeKind {
    /// `gen.next(arg)`.
    Next(Value),
    /// `gen.return(arg)` — foundation closes the generator without
    /// running additional finally blocks.
    Return(Value),
    /// `gen.throw(reason)` — re-enters the body and unwinds.
    Throw(Value),
}

/// Drive an iterator one step. Returns `(value, done)`. Once an
/// iterator hands back `done = true`, its state transitions to
/// `Exhausted` so subsequent calls are stable no-ops (matches the
/// spec rule "an iterator never produces values after it has
/// produced `done: true`"; §7.4.2 step 6).
pub(crate) fn step_iterator(
    iter: IteratorHandle,
    gc_heap: &mut otter_gc::GcHeap,
) -> Result<(Value, bool), VmError> {
    enum FastIteratorSnapshot {
        Array(JsArray, usize),
        ArrayKey(JsArray, usize),
        ArrayEntry(JsArray, usize),
        ArrayLike(Value, usize, crate::iterator_state::ArrayIterKind),
        TypedArray(
            crate::binary::typed_array::JsTypedArray,
            usize,
            crate::iterator_state::ArrayIterKind,
        ),
        String(JsString, u32),
        MapCollection(JsMap, usize, MapIteratorKind),
        SetCollection(JsSet, usize, SetIteratorKind),
        Exhausted,
        Slow,
    }

    let snapshot = gc_heap.read_payload(iter, |state| match state {
        IteratorState::Array { array, index, .. } => FastIteratorSnapshot::Array(*array, *index),
        IteratorState::ArrayKey { array, index } => FastIteratorSnapshot::ArrayKey(*array, *index),
        IteratorState::ArrayEntry { array, index } => {
            FastIteratorSnapshot::ArrayEntry(*array, *index)
        }
        IteratorState::TypedArray {
            typed_array,
            index,
            kind,
        } => FastIteratorSnapshot::TypedArray(*typed_array, *index, *kind),
        IteratorState::String { string, index } => FastIteratorSnapshot::String(*string, *index),
        IteratorState::MapCollection { map, index, kind } => {
            FastIteratorSnapshot::MapCollection(*map, *index, *kind)
        }
        IteratorState::SetCollection { set, index, kind } => {
            FastIteratorSnapshot::SetCollection(*set, *index, *kind)
        }
        IteratorState::ArrayLike {
            object,
            index,
            kind,
        } => FastIteratorSnapshot::ArrayLike(*object, *index, *kind),
        IteratorState::Exhausted { .. } => FastIteratorSnapshot::Exhausted,
        IteratorState::User { .. }
        | IteratorState::RegExpString { .. }
        | IteratorState::Generator { .. }
        | IteratorState::Map { .. }
        | IteratorState::Filter { .. }
        | IteratorState::Take { .. }
        | IteratorState::Drop { .. }
        | IteratorState::FlatMap { .. } => FastIteratorSnapshot::Slow,
    });

    let outcome = match snapshot {
        FastIteratorSnapshot::Array(array, index) => {
            if index >= crate::array::len(array, gc_heap) {
                None
            } else {
                let v = crate::array::get(array, gc_heap, index);
                gc_heap.with_payload(iter, |state| {
                    if let IteratorState::Array { index, .. } = state {
                        *index += 1;
                    }
                });
                Some(v)
            }
        }
        FastIteratorSnapshot::ArrayKey(array, index) => {
            if index >= crate::array::len(array, gc_heap) {
                None
            } else {
                gc_heap.with_payload(iter, |state| {
                    if let IteratorState::ArrayKey { index, .. } = state {
                        *index += 1;
                    }
                });
                Some(Value::number(crate::number::NumberValue::from_f64(
                    index as f64,
                )))
            }
        }
        FastIteratorSnapshot::ArrayEntry(array, index) => {
            if index >= crate::array::len(array, gc_heap) {
                None
            } else {
                let v = crate::array::get(array, gc_heap, index);
                let index_val = Value::number(crate::number::NumberValue::from_f64(index as f64));
                // Materialise [index, value] dense array. Roots both
                // operands via the visitor so a GC during allocation
                // sees them.
                let pair = {
                    let array_root = Value::array(array);
                    let mut visitor = |visit: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
                        array_root.trace_value_slots(visit);
                        index_val.trace_value_slots(visit);
                        v.trace_value_slots(visit);
                    };
                    crate::array::alloc_array_with_roots(gc_heap, &mut visitor)
                        .map_err(|_| VmError::TypeMismatch)?
                };
                crate::array::with_elements_mut(pair, gc_heap, |elements| {
                    elements.push(index_val);
                    elements.push(v);
                });
                gc_heap.with_payload(iter, |state| {
                    if let IteratorState::ArrayEntry { index, .. } = state {
                        *index += 1;
                    }
                });
                Some(Value::array(pair))
            }
        }
        FastIteratorSnapshot::ArrayLike(object, index, kind) => {
            // §23.1.5.2.1 %ArrayIteratorPrototype%.next over a generic
            // array-like object: re-read `length` and the element each
            // step so a mutation between calls is observed. Reads go
            // through the object's own data slots (`arguments`-style
            // array-likes), matching the other heap-only fast iterators.
            let len = match object
                .as_object()
                .and_then(|obj| crate::object::get(obj, gc_heap, "length"))
            {
                Some(v) => to_length(&v, gc_heap)?,
                None => 0,
            };
            if index >= len {
                None
            } else {
                let advance = |gc_heap: &mut otter_gc::GcHeap| {
                    gc_heap.with_payload(iter, |state| {
                        if let IteratorState::ArrayLike { index, .. } = state {
                            *index += 1;
                        }
                    });
                };
                match kind {
                    ArrayIterKind::Key => {
                        advance(gc_heap);
                        Some(Value::number(crate::number::NumberValue::from_f64(
                            index as f64,
                        )))
                    }
                    ArrayIterKind::Value => {
                        let v = object
                            .as_object()
                            .and_then(|obj| crate::object::get(obj, gc_heap, &index.to_string()))
                            .unwrap_or_else(Value::undefined);
                        advance(gc_heap);
                        Some(v)
                    }
                    ArrayIterKind::Entry => {
                        let element = object
                            .as_object()
                            .and_then(|obj| crate::object::get(obj, gc_heap, &index.to_string()))
                            .unwrap_or_else(Value::undefined);
                        let index_val =
                            Value::number(crate::number::NumberValue::from_f64(index as f64));
                        let pair = {
                            let mut visitor = |visit: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
                                index_val.trace_value_slots(visit);
                                element.trace_value_slots(visit);
                            };
                            crate::array::alloc_array_with_roots(gc_heap, &mut visitor)
                                .map_err(|_| VmError::TypeMismatch)?
                        };
                        crate::array::with_elements_mut(pair, gc_heap, |elements| {
                            elements.push(index_val);
                            elements.push(element);
                        });
                        advance(gc_heap);
                        Some(Value::array(pair))
                    }
                }
            }
        }
        FastIteratorSnapshot::TypedArray(typed_array, index, kind) => {
            // §23.1.5.1 CreateArrayIterator step — for a typed array the
            // closure rebuilds a buffer-witness record each step and
            // throws a TypeError when the array is out of bounds (a
            // shrunk resizable buffer or a detached one); otherwise it
            // reads the live element and terminates at the live length.
            if typed_array.is_out_of_bounds(gc_heap) {
                return Err(VmError::TypeError);
            }
            if index >= typed_array.length(gc_heap) {
                None
            } else {
                let element = typed_array
                    .get(gc_heap, index)
                    .map_err(|_| VmError::TypeMismatch)?;
                let advance = |gc_heap: &mut otter_gc::GcHeap| {
                    gc_heap.with_payload(iter, |state| {
                        if let IteratorState::TypedArray { index, .. } = state {
                            *index += 1;
                        }
                    });
                };
                match kind {
                    ArrayIterKind::Key => {
                        advance(gc_heap);
                        Some(Value::number(crate::number::NumberValue::from_f64(
                            index as f64,
                        )))
                    }
                    ArrayIterKind::Value => {
                        advance(gc_heap);
                        Some(element)
                    }
                    ArrayIterKind::Entry => {
                        let index_val =
                            Value::number(crate::number::NumberValue::from_f64(index as f64));
                        let pair = {
                            let mut visitor = |visit: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
                                index_val.trace_value_slots(visit);
                                element.trace_value_slots(visit);
                            };
                            crate::array::alloc_array_with_roots(gc_heap, &mut visitor)
                                .map_err(|_| VmError::TypeMismatch)?
                        };
                        crate::array::with_elements_mut(pair, gc_heap, |elements| {
                            elements.push(index_val);
                            elements.push(element);
                        });
                        advance(gc_heap);
                        Some(Value::array(pair))
                    }
                }
            }
        }
        FastIteratorSnapshot::String(string, index) => {
            // §22.1.5.1 `%StringIteratorPrototype%.next`.
            if let Some(unit) = string.char_code_at(index, gc_heap) {
                let next_unit = string.char_code_at(index + 1, gc_heap);
                let is_pair = (0xD800..=0xDBFF).contains(&unit)
                    && matches!(next_unit, Some(low) if (0xDC00..=0xDFFF).contains(&low));
                let (s, advance) = if is_pair {
                    let pair = [unit, next_unit.unwrap()];
                    (JsString::from_utf16_units(&pair, gc_heap)?, 2)
                } else {
                    (JsString::from_utf16_units(&[unit], gc_heap)?, 1)
                };
                gc_heap.with_payload(iter, |state| {
                    if let IteratorState::String { index, .. } = state {
                        *index += advance;
                    }
                });
                Some(Value::string(s))
            } else {
                None
            }
        }
        FastIteratorSnapshot::MapCollection(map, index, kind) => {
            let raw_len = crate::collections::map_raw_len(map, gc_heap);
            let mut next_index = index;
            let mut next_entry = None;
            while next_index < raw_len {
                let probe_index = next_index;
                next_index += 1;
                if let Some(entry) = crate::collections::map_entry_at(map, gc_heap, probe_index) {
                    next_entry = Some(entry);
                    break;
                }
            }
            if let Some((key, value)) = next_entry {
                gc_heap.with_payload(iter, |state| {
                    if let IteratorState::MapCollection { index, .. } = state {
                        *index = next_index;
                    }
                });
                Some(match kind {
                    MapIteratorKind::Key => key,
                    MapIteratorKind::Value => value,
                    MapIteratorKind::Entry => {
                        let pair = {
                            let map_root = Value::map(map);
                            let mut visitor = |visit: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
                                map_root.trace_value_slots(visit);
                                key.trace_value_slots(visit);
                                value.trace_value_slots(visit);
                            };
                            crate::array::alloc_array_with_roots(gc_heap, &mut visitor)
                                .map_err(|_| VmError::TypeMismatch)?
                        };
                        crate::array::with_elements_mut(pair, gc_heap, |elements| {
                            elements.push(key);
                            elements.push(value);
                        });
                        Value::array(pair)
                    }
                })
            } else {
                None
            }
        }
        FastIteratorSnapshot::SetCollection(set, index, kind) => {
            let raw_len = crate::collections::set_raw_len(set, gc_heap);
            let mut next_index = index;
            let mut next_value = None;
            while next_index < raw_len {
                let probe_index = next_index;
                next_index += 1;
                if let Some(value) = crate::collections::set_value_at(set, gc_heap, probe_index) {
                    next_value = Some(value);
                    break;
                }
            }
            if let Some(value) = next_value {
                gc_heap.with_payload(iter, |state| {
                    if let IteratorState::SetCollection { index, .. } = state {
                        *index = next_index;
                    }
                });
                Some(match kind {
                    SetIteratorKind::Value => value,
                    SetIteratorKind::Entry => {
                        let pair = {
                            let set_root = Value::set(set);
                            let mut visitor = |visit: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
                                set_root.trace_value_slots(visit);
                                value.trace_value_slots(visit);
                            };
                            crate::array::alloc_array_with_roots(gc_heap, &mut visitor)
                                .map_err(|_| VmError::TypeMismatch)?
                        };
                        crate::array::with_elements_mut(pair, gc_heap, |elements| {
                            elements.push(value);
                            elements.push(value);
                        });
                        Value::array(pair)
                    }
                })
            } else {
                None
            }
        }
        FastIteratorSnapshot::Exhausted => None,
        FastIteratorSnapshot::Slow => return Err(VmError::TypeMismatch),
    };
    match outcome {
        Some(value) => Ok((value, false)),
        None => {
            gc_heap.with_payload(iter, |state| state.exhaust());
            Ok((Value::undefined(), true))
        }
    }
}

/// `true` when `value` is a `JsObject` whose internal native
/// call slot carries a native function, i.e. it is
/// callable even though it is not a plain function value.
pub(crate) fn object_has_call_slot(value: &Value, heap: &otter_gc::GcHeap) -> bool {
    let Some(obj) = value.as_object() else {
        return false;
    };
    crate::object::call_native(obj, heap).is_some_and(|v| v.is_native_function())
}

/// `true` when `value` is a VM constructor. This is intentionally
/// stricter than `IsCallable`: callable ordinary objects such as
/// `Function.prototype` must reject `new`.
pub(crate) fn is_constructor_runtime(
    value: &Value,
    context: &ExecutionContext,
    heap: &otter_gc::GcHeap,
) -> bool {
    if let Some(bound) = value.as_bound_function() {
        let (target, _, _) = bound.parts(heap);
        is_constructor_runtime(&target, context, heap)
    } else {
        abstract_ops::is_constructor(value, context, heap) || object_has_construct_slot(value, heap)
    }
}

/// `true` when `value` is a `JsObject` whose internal native
/// constructor slot carries a native function, i.e. it is
/// admissible as a `new` callee even though it is not a plain
/// function value.
pub(crate) fn object_has_construct_slot(value: &Value, heap: &otter_gc::GcHeap) -> bool {
    let Some(obj) = value.as_object() else {
        return false;
    };
    crate::object::constructor_native(obj, heap).is_some_and(|v| v.is_native_function())
}

pub(crate) fn is_restricted_function_property(name: &str) -> bool {
    matches!(name, "caller" | "arguments")
}

/// Pick the property name for the current
/// [`ToPrimitiveStage`] under ECMA-262 §7.1.1.1
/// `OrdinaryToPrimitive`.
///
/// - `Default` / `Number` → first slot is `"valueOf"`, second is
///   `"toString"`.
/// - `String` → first slot is `"toString"`, second is `"valueOf"`.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-ordinarytoprimitive>
pub(crate) fn ordinary_method_for(
    hint: abstract_ops::ToPrimitiveHint,
    stage: ToPrimitiveStage,
) -> &'static str {
    let (first, second) = match hint {
        abstract_ops::ToPrimitiveHint::String => ("toString", "valueOf"),
        abstract_ops::ToPrimitiveHint::Default | abstract_ops::ToPrimitiveHint::Number => {
            ("valueOf", "toString")
        }
    };
    match stage {
        ToPrimitiveStage::OrdinaryFirst => first,
        ToPrimitiveStage::OrdinarySecond => second,
        ToPrimitiveStage::SymbolToPrim
        | ToPrimitiveStage::SymbolResult
        | ToPrimitiveStage::Exhausted => "",
    }
}

/// `true` when `value` is one of the call-site shapes the dispatcher
/// can invoke. Thin wrapper over [`abstract_ops::is_callable`]
/// (ECMA-262 §7.2.3) — kept under the same name so existing call
/// sites do not change.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-iscallable>
pub(crate) fn is_callable(value: &Value) -> bool {
    abstract_ops::is_callable(value)
}

/// Public re-export of [`is_callable`] for crate-external dispatch
/// helpers (e.g. [`crate::promise_dispatch`]).
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-iscallable>
#[must_use]
pub fn is_callable_value(value: &Value) -> bool {
    abstract_ops::is_callable(value)
}
