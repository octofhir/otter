//! ECMA-262 §23.2 TypedArray bootstrap installer.
//!
//! Installs the 11 concrete TypedArray constructors plus a shared
//! abstract `%TypedArray%.prototype` that all per-kind prototypes
//! inherit from. Each per-kind prototype carries
//! `BYTES_PER_ELEMENT`, `constructor`, and `@@toStringTag` —
//! `Uint8Array.prototype[@@toStringTag] === "Uint8Array"`. The
//! 20+ shared prototype methods (`at`, `subarray`, `slice`, …)
//! delegate to the existing
//! [`crate::binary::typed_array_prototype`] native method table.
//!
//! The method table fast path at `Op::CallMethod` continues to
//! serve `arr.at(...)` / `arr.fill(...)` calls; the installed
//! `NativeFunction` properties are reached by reflective access
//! and by `Function.prototype.call` / user overrides.
//!
//! # Contents
//! - [`install_typed_arrays`] — bootstrap entry that registers
//!   all 11 ctors.
//! - [`install_typed_array_well_knowns_post_bootstrap`] —
//!   `@@iterator` + `@@toStringTag` fixups.
//!
//! # Invariants
//! - Each `new <T>(...)` call routes through the real
//!   `NativeFunction` ctor and delegates to
//!   [`crate::binary::dispatch::typed_array_call_with_roots`] with the
//!   per-kind discriminant.
//! - Bare call (e.g. `Uint8Array(4)` without `new`) throws
//!   `TypeError` per §23.2.5.1 step 2.
//! - Per-kind prototypes link
//!   `<T>.prototype.__proto__ = %TypedArray%.prototype`; the
//!   abstract prototype itself chains to `%Object.prototype%`.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-typedarray-constructors>
//! - <https://tc39.es/ecma262/#sec-properties-of-the-%25typedarrayprototype%25-object>

use otter_bytecode::method_id::TypedArrayMethod;
use smallvec::SmallVec;

use crate::binary::typed_array::TypedArrayKind;
use crate::binary::{dispatch, typed_array_prototype};
use crate::js_surface::JsSurfaceError;
use crate::object::{self, JsObject, PartialPropertyDescriptor, PropertyDescriptor};
use crate::{NativeCtx, NativeError, Value, VmError};

/// Install `@@toStringTag` on each per-kind prototype after the
/// well-known symbol table exists. Also installs `@@iterator =
/// values` on the abstract `%TypedArray%.prototype`.
pub fn install_typed_array_well_knowns_post_bootstrap(
    heap: &mut otter_gc::GcHeap,
    global: JsObject,
    well_known: &crate::symbol::WellKnownSymbols,
) -> Result<(), JsSurfaceError> {
    use crate::symbol::WellKnown;

    let tag_sym = well_known.get(WellKnown::ToStringTag);

    // §22.2.3.{6-11,13,14,15,17,18,21,22} callback prototype
    // methods — install NativeFunction wrappers so reflective
    // access (`TypedArray.prototype.every.length`,
    // `TypedArray.prototype.map.name`) sees the spec property
    // descriptor. The actual dispatch fires via
    // `Interpreter::typed_array_callback_dispatch` on the
    // method-call fast path. The wrappers below cover the
    // `Function.prototype.call` / explicit-invoke fallback by
    // re-entering the same callback loop in the wrapper body.
    if let Some(abstract_proto) = get_abstract_typed_array_prototype(global, heap) {
        let abstract_proto_root = Value::object(abstract_proto);
        let install_method = |heap: &mut otter_gc::GcHeap,
                              name: &'static str,
                              length: u8,
                              call: crate::native_function::NativeFastFn|
         -> Result<(), JsSurfaceError> {
            let f = crate::bootstrap::native_static_with_value_roots(
                heap,
                name,
                length,
                call,
                &[&abstract_proto_root],
            )
            .map_err(|_| JsSurfaceError::OutOfMemory)?;
            object::define_own_property(
                abstract_proto,
                heap,
                name,
                PropertyDescriptor::data(Value::native_function(f), true, false, true),
            );
            Ok(())
        };
        install_method(heap, "forEach", 1, ta_proto_for_each)?;
        install_method(heap, "map", 1, ta_proto_map)?;
        install_method(heap, "filter", 1, ta_proto_filter)?;
        install_method(heap, "find", 1, ta_proto_find)?;
        install_method(heap, "findIndex", 1, ta_proto_find_index)?;
        install_method(heap, "findLast", 1, ta_proto_find_last)?;
        install_method(heap, "findLastIndex", 1, ta_proto_find_last_index)?;
        install_method(heap, "every", 1, ta_proto_every)?;
        install_method(heap, "some", 1, ta_proto_some)?;
        install_method(heap, "reduce", 1, ta_proto_reduce)?;
        install_method(heap, "reduceRight", 1, ta_proto_reduce_right)?;
    }

    // §22.2.6.{1-4} — instance accessor getters live on
    // `%TypedArray%.prototype` (not the per-kind prototypes).
    // Each getter validates the receiver carries a TypedArray
    // internal slot and reads the corresponding field. The
    // setter side is undefined per spec.
    if let Some(abstract_proto) = get_abstract_typed_array_prototype(global, heap) {
        let abstract_proto_root = Value::object(abstract_proto);
        let install_accessor = |heap: &mut otter_gc::GcHeap,
                                name: &'static str,
                                getter_name: &'static str,
                                getter: crate::native_function::NativeFastFn|
         -> Result<(), JsSurfaceError> {
            let f = crate::bootstrap::native_static_with_value_roots(
                heap,
                getter_name,
                0,
                getter,
                &[&abstract_proto_root],
            )
            .map_err(|_| JsSurfaceError::OutOfMemory)?;
            object::define_own_property(
                abstract_proto,
                heap,
                name,
                PropertyDescriptor::accessor(Some(Value::native_function(f)), None, false, true),
            );
            Ok(())
        };
        install_accessor(heap, "buffer", "get buffer", ta_buffer_getter)?;
        install_accessor(heap, "byteLength", "get byteLength", ta_byte_length_getter)?;
        install_accessor(heap, "byteOffset", "get byteOffset", ta_byte_offset_getter)?;
        install_accessor(heap, "length", "get length", ta_length_getter)?;
    }

    // §22.2.6 — `%TypedArray%.prototype[@@toStringTag]` is an
    // accessor on the abstract prototype. The getter returns the
    // receiver's [[TypedArrayName]] (the kind name string) or
    // `undefined` for non-TypedArray receivers. Per-kind
    // prototypes inherit the accessor; per-instance access walks
    // up to %TypedArray%.prototype and triggers the getter.
    if let Some(abstract_proto) = get_abstract_typed_array_prototype(global, heap) {
        let abstract_proto_root = Value::object(abstract_proto);
        let getter = crate::bootstrap::native_static_with_value_roots(
            heap,
            "[Symbol.toStringTag]",
            0,
            tostring_tag_getter,
            &[&abstract_proto_root],
        )
        .map_err(|_| JsSurfaceError::OutOfMemory)?;
        object::define_own_symbol_property_partial(
            abstract_proto,
            heap,
            tag_sym,
            PartialPropertyDescriptor {
                get: Some(Value::native_function(getter)),
                enumerable: Some(false),
                configurable: Some(true),
                ..Default::default()
            },
        );
    }

    // Install `%TypedArray%.prototype[@@iterator] = values`.
    if let Some(abstract_proto) = get_abstract_typed_array_prototype(global, heap)
        && let Some(values_value) = object::get(abstract_proto, heap, "values")
    {
        let iterator_sym = well_known.get(WellKnown::Iterator);
        object::define_own_symbol_property_partial(
            abstract_proto,
            heap,
            iterator_sym,
            PartialPropertyDescriptor {
                value: Some(values_value),
                writable: Some(true),
                enumerable: Some(false),
                configurable: Some(true),
                ..Default::default()
            },
        );
    }
    Ok(())
}

/// Drain a JS iterable into a `Vec<Value>` by calling its
/// `[Symbol.iterator]` method and pumping the resulting iterator
/// until completion. Used by the §22.2.4.4 `new TA(iterable)`
/// constructor path.
/// §7.4.2 GetIterator + drain — call the already-fetched `@@iterator`
/// method (one `GetMethod`, per spec) and collect every yielded value.
fn drain_iterable_into_values(
    ctx: &mut NativeCtx<'_>,
    exec_ctx: &crate::ExecutionContext,
    src: &Value,
    iter_method: Value,
) -> Result<Vec<Value>, NativeError> {
    let src_value = *src;
    if !ctx.cx.interp.is_callable_runtime(&iter_method) {
        return Err(NativeError::TypeError {
            name: "TypedArray",
            reason: "source object is not iterable".to_string(),
        });
    }
    let no_args: smallvec::SmallVec<[Value; 8]> = smallvec::SmallVec::new();
    let iter_obj = ctx
        .cx
        .interp
        .run_callable_sync(exec_ctx, &iter_method, src_value, no_args)
        .map_err(|e| NativeError::TypeError {
            name: "TypedArray",
            reason: e.to_string(),
        })?;
    let handle = if let Some(h) = iter_obj.as_iterator() {
        h
    } else if let Some(g) = iter_obj.as_generator() {
        let gen_value = Value::generator(g);
        let state = crate::IteratorState::Generator { handle: g };
        ctx.alloc_iterator_state(state, &[&gen_value], &[])
            .map_err(|_| NativeError::TypeError {
                name: "TypedArray",
                reason: "iterator allocation failed".to_string(),
            })?
    } else {
        let other_root = iter_obj;
        let state = crate::IteratorState::User { iterator: iter_obj };
        ctx.alloc_iterator_state(state, &[&other_root], &[])
            .map_err(|_| NativeError::TypeError {
                name: "TypedArray",
                reason: "iterator allocation failed".to_string(),
            })?
    };
    let mut collected: Vec<Value> = Vec::new();
    loop {
        let (v, done) = ctx
            .cx
            .interp
            .iterator_next_full(exec_ctx, &handle)
            .map_err(|e| NativeError::TypeError {
                name: "TypedArray",
                reason: e.to_string(),
            })?;
        if done {
            break;
        }
        collected.push(v);
    }
    Ok(collected)
}

/// §23.2.5.1 / §22.2.4.4 — convert each collected source value with
/// `ToBigInt` for BigInt element types and `ToNumber` otherwise, so a
/// Symbol / cross-numeric value throws and a `valueOf` / `toString`
/// runs. The per-kind dispatcher narrows the result on store.
fn coerce_values_for_kind(
    ctx: &mut NativeCtx<'_>,
    exec: &crate::ExecutionContext,
    values: Vec<Value>,
    kind: TypedArrayKind,
) -> Result<Vec<Value>, NativeError> {
    let mut out = Vec::with_capacity(values.len());
    for value in values {
        let converted = if kind.is_bigint() {
            let big = crate::coerce::to_big_int_or_throw(ctx.cx.interp, exec, &value)
                .map_err(|e| vm_to_native(e, "TypedArray"))?;
            Value::big_int(big)
        } else {
            let number = crate::coerce::to_number_or_throw(ctx.cx.interp, exec, &value)
                .map_err(|e| vm_to_native(e, "TypedArray"))?;
            Value::number(number)
        };
        out.push(converted);
    }
    Ok(out)
}

/// §7.3.20 LengthOfArrayLike + raw element reads — `Get(source, k)`
/// for each `k < ToLength(Get(source, "length"))`, running getters but
/// **not** numeric-coercing (the caller maps, then converts). Reserves
/// fallibly so a pathological `length` throws `RangeError`.
fn read_array_like_raw(
    ctx: &mut NativeCtx<'_>,
    exec: &crate::ExecutionContext,
    source: Value,
) -> Result<Vec<Value>, NativeError> {
    let len_value = ta_get_via(ctx, exec, source, &crate::VmPropertyKey::String("length"))?;
    let len_number = crate::coerce::to_number_or_throw(ctx.cx.interp, exec, &len_value)
        .map_err(|e| vm_to_native(e, "TypedArray"))?;
    let n = len_number.as_f64();
    let len = if n.is_nan() || n <= 0.0 {
        0
    } else {
        n.trunc().min(9_007_199_254_740_991.0) as usize
    };
    let mut out: Vec<Value> = Vec::new();
    if out.try_reserve_exact(len).is_err() {
        return Err(NativeError::RangeError {
            name: "TypedArray.from",
            reason: "Invalid typed array length".to_string(),
        });
    }
    for i in 0..len {
        out.push(ta_get_via(
            ctx,
            exec,
            source,
            &crate::VmPropertyKey::OwnedString(i.to_string()),
        )?);
    }
    Ok(out)
}

/// §7.3.3 Get + run an accessor, propagating an abrupt completion.
fn ta_get_via(
    ctx: &mut NativeCtx<'_>,
    exec: &crate::ExecutionContext,
    source: Value,
    key: &crate::VmPropertyKey<'_>,
) -> Result<Value, NativeError> {
    let outcome = ctx
        .cx
        .interp
        .ordinary_get_value(exec, source, source, key, 0)
        .map_err(|e| vm_to_native(e, "TypedArray"))?;
    match outcome {
        crate::VmGetOutcome::Value(v) => Ok(v),
        crate::VmGetOutcome::InvokeGetter { getter } => ctx
            .cx
            .interp
            .run_callable_sync(exec, &getter, source, smallvec::SmallVec::new())
            .map_err(|e| vm_to_native(e, "TypedArray")),
    }
}

/// §23.2.5.1 InitializeTypedArrayFromArrayLike — read an array-like
/// object's `length` (`Get` + `ToLength`) and each element (`Get`,
/// running getters, then `ToNumber` / `ToBigInt`), so user side
/// effects run and a Symbol / cross-numeric element throws. Returns
/// the converted elements; the per-kind dispatcher narrows them to the
/// destination representation on store.
fn read_array_like_coerced(
    ctx: &mut NativeCtx<'_>,
    exec: &crate::ExecutionContext,
    source: Value,
    kind: TypedArrayKind,
) -> Result<Vec<Value>, NativeError> {
    let len_value = ta_get_via(ctx, exec, source, &crate::VmPropertyKey::String("length"))?;
    let len_number = crate::coerce::to_number_or_throw(ctx.cx.interp, exec, &len_value)
        .map_err(|e| vm_to_native(e, "TypedArray"))?;
    let n = len_number.as_f64();
    let len = if n.is_nan() || n <= 0.0 {
        0
    } else {
        n.trunc().min(9_007_199_254_740_991.0) as usize
    };
    // §23.2.5.1.1 AllocateTypedArrayBuffer rejects a length the host
    // cannot back with a RangeError; reserve up-front (fallibly) so a
    // pathological `length` (e.g. 2**53) fails cleanly instead of
    // aborting the process on the capacity request.
    let mut out: Vec<Value> = Vec::new();
    if out.try_reserve_exact(len).is_err() {
        return Err(NativeError::RangeError {
            name: "TypedArray",
            reason: "Invalid typed array length".to_string(),
        });
    }
    for i in 0..len {
        let value = ta_get_via(
            ctx,
            exec,
            source,
            &crate::VmPropertyKey::OwnedString(i.to_string()),
        )?;
        let converted = if kind.is_bigint() {
            let big = crate::coerce::to_big_int_or_throw(ctx.cx.interp, exec, &value)
                .map_err(|e| vm_to_native(e, "TypedArray"))?;
            Value::big_int(big)
        } else {
            let number = crate::coerce::to_number_or_throw(ctx.cx.interp, exec, &value)
                .map_err(|e| vm_to_native(e, "TypedArray"))?;
            Value::number(number)
        };
        out.push(converted);
    }
    Ok(out)
}

/// §22.2.3 TypedArray callback prototype method wrappers — used
/// for reflective access and `Function.prototype.call` style
/// re-entries; the method-call fast path bypasses these into
/// `Interpreter::typed_array_callback_dispatch` directly.
fn ta_callback_receiver(
    ctx: &NativeCtx<'_>,
    method: &'static str,
) -> Result<crate::binary::typed_array::JsTypedArray, NativeError> {
    ctx.this_value()
        .as_typed_array(ctx.heap())
        .ok_or_else(|| NativeError::TypeError {
            name: method,
            reason: "this is not a TypedArray".to_string(),
        })
}

fn ta_callback_snapshot(
    t: &crate::binary::typed_array::JsTypedArray,
    heap: &mut otter_gc::GcHeap,
) -> Result<Vec<Value>, otter_gc::OutOfMemory> {
    let len = t.length(heap);
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        out.push(t.get(heap, i)?);
    }
    Ok(out)
}

fn ta_oom_to_native(method: &'static str) -> impl Fn(otter_gc::OutOfMemory) -> NativeError {
    move |err: otter_gc::OutOfMemory| NativeError::TypeError {
        name: method,
        reason: format!(
            "out of memory: requested {} bytes, heap limit {}",
            err.requested_bytes(),
            err.heap_limit_bytes(),
        ),
    }
}

fn ta_require_callable(args: &[Value], method: &'static str) -> Result<Value, NativeError> {
    let cb = args.first().cloned().unwrap_or(Value::undefined());
    if !crate::abstract_ops::is_callable(&cb) {
        return Err(NativeError::TypeError {
            name: method,
            reason: "callback is not callable".to_string(),
        });
    }
    Ok(cb)
}

fn ta_invoke_callback(
    ctx: &mut NativeCtx<'_>,
    callee: &Value,
    this_arg: &Value,
    value: &Value,
    index: usize,
    ta: &Value,
) -> Result<Value, NativeError> {
    let exec_ctx = ctx
        .execution_context()
        .cloned()
        .ok_or_else(|| NativeError::TypeError {
            name: "TypedArray callback",
            reason: "missing execution context".to_string(),
        })?;
    let mut cb_args: smallvec::SmallVec<[Value; 8]> = smallvec::SmallVec::new();
    cb_args.push(*value);
    cb_args.push(Value::number(crate::number::NumberValue::from_i32(
        index as i32,
    )));
    cb_args.push(*ta);
    ctx.cx
        .interp
        .run_callable_sync(&exec_ctx, callee, *this_arg, cb_args)
        .map_err(|e| NativeError::TypeError {
            name: "TypedArray callback",
            reason: e.to_string(),
        })
}

fn ta_proto_for_each(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let t = ta_callback_receiver(ctx, "TypedArray.prototype.forEach")?;
    let callee = ta_require_callable(args, "TypedArray.prototype.forEach")?;
    let this_arg = args.get(1).cloned().unwrap_or(Value::undefined());
    let ta_value = Value::typed_array(t);
    let elements = ta_callback_snapshot(&t, ctx.interp_mut().gc_heap_mut())
        .map_err(ta_oom_to_native("TypedArray callback"))?;
    for (i, v) in elements.into_iter().enumerate() {
        ta_invoke_callback(ctx, &callee, &this_arg, &v, i, &ta_value)?;
    }
    Ok(Value::undefined())
}

fn ta_proto_map(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let t = ta_callback_receiver(ctx, "TypedArray.prototype.map")?;
    let callee = ta_require_callable(args, "TypedArray.prototype.map")?;
    let this_arg = args.get(1).cloned().unwrap_or(Value::undefined());
    let ta_value = Value::typed_array(t);
    // §23.2.3.20 step 3 — capture `len` before any user callback runs.
    let len = t.length(ctx.heap());
    // Step 5 — `A = ? TypedArraySpeciesCreate(O, « len »)` runs before
    // the per-element callback loop, so a throwing species constructor
    // is observed before any callback is invoked.
    let a = ta_species_create(ctx, t, len, "TypedArray.prototype.map")?;
    let target_kind = a.kind();
    // Step 6 — read each source element live (so callbacks that mutate
    // the receiver mid-iteration are observed per `Get(O, k)`), invoke
    // the callback, then `Set(A, k, mappedValue)` through the target
    // kind's element coercion.
    for i in 0..len {
        let kvalue = t
            .get(ctx.interp_mut().gc_heap_mut(), i)
            .map_err(ta_oom_to_native("TypedArray.prototype.map"))?;
        let mapped = ta_invoke_callback(ctx, &callee, &this_arg, &kvalue, i, &ta_value)?;
        let coerced = crate::binary::dispatch::coerce_element_for_store(
            ctx.interp_mut().gc_heap_mut(),
            target_kind,
            &mapped,
        )
        .map_err(|e| vm_to_native(e, "TypedArray.prototype.map"))?;
        a.set(ctx.interp_mut().gc_heap_mut(), i, &coerced);
    }
    Ok(Value::typed_array(a))
}

fn ta_proto_filter(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let t = ta_callback_receiver(ctx, "TypedArray.prototype.filter")?;
    let callee = ta_require_callable(args, "TypedArray.prototype.filter")?;
    let this_arg = args.get(1).cloned().unwrap_or(Value::undefined());
    let ta_value = Value::typed_array(t);
    // §23.2.3.10 — run the predicate over every element first, reading
    // each live so mid-iteration receiver mutations are observed, and
    // collect the kept values. The species constructor is only invoked
    // afterwards with the kept count (step 9).
    let len = t.length(ctx.heap());
    let mut kept: Vec<Value> = Vec::new();
    for i in 0..len {
        let kvalue = t
            .get(ctx.interp_mut().gc_heap_mut(), i)
            .map_err(ta_oom_to_native("TypedArray.prototype.filter"))?;
        let selected = ta_invoke_callback(ctx, &callee, &this_arg, &kvalue, i, &ta_value)?;
        if selected.to_boolean(ctx.interp_mut().gc_heap()) {
            kept.push(kvalue);
        }
    }
    let a = ta_species_create(ctx, t, kept.len(), "TypedArray.prototype.filter")?;
    let target_kind = a.kind();
    for (i, v) in kept.iter().enumerate() {
        let coerced = crate::binary::dispatch::coerce_element_for_store(
            ctx.interp_mut().gc_heap_mut(),
            target_kind,
            v,
        )
        .map_err(|e| vm_to_native(e, "TypedArray.prototype.filter"))?;
        a.set(ctx.interp_mut().gc_heap_mut(), i, &coerced);
    }
    Ok(Value::typed_array(a))
}

fn ta_proto_find(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let t = ta_callback_receiver(ctx, "TypedArray.prototype.find")?;
    let callee = ta_require_callable(args, "TypedArray.prototype.find")?;
    let this_arg = args.get(1).cloned().unwrap_or(Value::undefined());
    let ta_value = Value::typed_array(t);
    let elements = ta_callback_snapshot(&t, ctx.interp_mut().gc_heap_mut())
        .map_err(ta_oom_to_native("TypedArray callback"))?;
    for (i, v) in elements.into_iter().enumerate() {
        let hit = ta_invoke_callback(ctx, &callee, &this_arg, &v, i, &ta_value)?;
        if hit.to_boolean(ctx.interp_mut().gc_heap()) {
            return Ok(v);
        }
    }
    Ok(Value::undefined())
}

fn ta_proto_find_index(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let t = ta_callback_receiver(ctx, "TypedArray.prototype.findIndex")?;
    let callee = ta_require_callable(args, "TypedArray.prototype.findIndex")?;
    let this_arg = args.get(1).cloned().unwrap_or(Value::undefined());
    let ta_value = Value::typed_array(t);
    let elements = ta_callback_snapshot(&t, ctx.interp_mut().gc_heap_mut())
        .map_err(ta_oom_to_native("TypedArray callback"))?;
    for (i, v) in elements.into_iter().enumerate() {
        let hit = ta_invoke_callback(ctx, &callee, &this_arg, &v, i, &ta_value)?;
        if hit.to_boolean(ctx.interp_mut().gc_heap()) {
            return Ok(Value::number(crate::number::NumberValue::from_i32(
                i as i32,
            )));
        }
    }
    Ok(Value::number(crate::number::NumberValue::from_i32(-1)))
}

fn ta_proto_find_last(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let t = ta_callback_receiver(ctx, "TypedArray.prototype.findLast")?;
    let callee = ta_require_callable(args, "TypedArray.prototype.findLast")?;
    let this_arg = args.get(1).cloned().unwrap_or(Value::undefined());
    let ta_value = Value::typed_array(t);
    let elements = ta_callback_snapshot(&t, ctx.interp_mut().gc_heap_mut())
        .map_err(ta_oom_to_native("TypedArray callback"))?;
    for i in (0..elements.len()).rev() {
        let v = elements[i];
        let hit = ta_invoke_callback(ctx, &callee, &this_arg, &v, i, &ta_value)?;
        if hit.to_boolean(ctx.interp_mut().gc_heap()) {
            return Ok(v);
        }
    }
    Ok(Value::undefined())
}

fn ta_proto_find_last_index(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let t = ta_callback_receiver(ctx, "TypedArray.prototype.findLastIndex")?;
    let callee = ta_require_callable(args, "TypedArray.prototype.findLastIndex")?;
    let this_arg = args.get(1).cloned().unwrap_or(Value::undefined());
    let ta_value = Value::typed_array(t);
    let elements = ta_callback_snapshot(&t, ctx.interp_mut().gc_heap_mut())
        .map_err(ta_oom_to_native("TypedArray callback"))?;
    for i in (0..elements.len()).rev() {
        let v = elements[i];
        let hit = ta_invoke_callback(ctx, &callee, &this_arg, &v, i, &ta_value)?;
        if hit.to_boolean(ctx.interp_mut().gc_heap()) {
            return Ok(Value::number(crate::number::NumberValue::from_i32(
                i as i32,
            )));
        }
    }
    Ok(Value::number(crate::number::NumberValue::from_i32(-1)))
}

fn ta_proto_every(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let t = ta_callback_receiver(ctx, "TypedArray.prototype.every")?;
    let callee = ta_require_callable(args, "TypedArray.prototype.every")?;
    let this_arg = args.get(1).cloned().unwrap_or(Value::undefined());
    let ta_value = Value::typed_array(t);
    let elements = ta_callback_snapshot(&t, ctx.interp_mut().gc_heap_mut())
        .map_err(ta_oom_to_native("TypedArray callback"))?;
    for (i, v) in elements.into_iter().enumerate() {
        let hit = ta_invoke_callback(ctx, &callee, &this_arg, &v, i, &ta_value)?;
        if !hit.to_boolean(ctx.interp_mut().gc_heap()) {
            return Ok(Value::boolean(false));
        }
    }
    Ok(Value::boolean(true))
}

fn ta_proto_some(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let t = ta_callback_receiver(ctx, "TypedArray.prototype.some")?;
    let callee = ta_require_callable(args, "TypedArray.prototype.some")?;
    let this_arg = args.get(1).cloned().unwrap_or(Value::undefined());
    let ta_value = Value::typed_array(t);
    let elements = ta_callback_snapshot(&t, ctx.interp_mut().gc_heap_mut())
        .map_err(ta_oom_to_native("TypedArray callback"))?;
    for (i, v) in elements.into_iter().enumerate() {
        let hit = ta_invoke_callback(ctx, &callee, &this_arg, &v, i, &ta_value)?;
        if hit.to_boolean(ctx.interp_mut().gc_heap()) {
            return Ok(Value::boolean(true));
        }
    }
    Ok(Value::boolean(false))
}

fn ta_proto_reduce(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    ta_proto_reduce_dir(ctx, args, false)
}

fn ta_proto_reduce_right(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    ta_proto_reduce_dir(ctx, args, true)
}

fn ta_proto_reduce_dir(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    reverse: bool,
) -> Result<Value, NativeError> {
    let method = if reverse {
        "TypedArray.prototype.reduceRight"
    } else {
        "TypedArray.prototype.reduce"
    };
    let t = ta_callback_receiver(ctx, method)?;
    let callee = ta_require_callable(args, method)?;
    let elements = ta_callback_snapshot(&t, ctx.interp_mut().gc_heap_mut())
        .map_err(ta_oom_to_native("TypedArray callback"))?;
    let len = elements.len();
    let has_init = args.len() >= 2;
    if len == 0 && !has_init {
        return Err(NativeError::TypeError {
            name: method,
            reason: "reduce of empty array with no initial value".to_string(),
        });
    }
    let ta_value = Value::typed_array(t);
    let exec_ctx = ctx
        .execution_context()
        .cloned()
        .ok_or_else(|| NativeError::TypeError {
            name: method,
            reason: "missing execution context".to_string(),
        })?;
    let step: i64 = if reverse { -1 } else { 1 };
    let (mut acc, start_idx) = if has_init {
        (args[1], if reverse { len as i64 - 1 } else { 0 })
    } else {
        let seed = if reverse { len - 1 } else { 0 };
        (elements[seed], seed as i64 + step)
    };
    let mut i = start_idx;
    while i >= 0 && (i as usize) < len {
        let value = elements[i as usize];
        let mut cb_args: smallvec::SmallVec<[Value; 8]> = smallvec::SmallVec::new();
        cb_args.push(acc);
        cb_args.push(value);
        cb_args.push(Value::number(crate::number::NumberValue::from_i32(
            i as i32,
        )));
        cb_args.push(ta_value);
        acc = ctx
            .cx
            .interp
            .run_callable_sync(&exec_ctx, &callee, Value::undefined(), cb_args)
            .map_err(|e| NativeError::TypeError {
                name: method,
                reason: e.to_string(),
            })?;
        i += step;
    }
    Ok(acc)
}

/// §23.2.4.1 `TypedArraySpeciesCreate(exemplar, argumentList)` for the
/// single-`length` argument case used by `map` / `filter` / `slice` /
/// `subarray`.
///
/// Resolves `SpeciesConstructor(exemplar, %DefaultConstructor%)`
/// (§7.3.22) — observing a user `constructor` / `Symbol.species`
/// override — then performs `TypedArrayCreate(constructor, « length »)`
/// (§23.2.4.2): constructs the result, then verifies it is a
/// TypedArray, its buffer is not detached, and its length is at least
/// `length`.
fn ta_species_create(
    ctx: &mut NativeCtx<'_>,
    exemplar: crate::binary::typed_array::JsTypedArray,
    length: usize,
    method: &'static str,
) -> Result<crate::binary::typed_array::JsTypedArray, NativeError> {
    let exemplar_value = Value::typed_array(exemplar);
    let default_name = exemplar.kind().name();
    let default_ctor = {
        let interp = &ctx.cx.interp;
        crate::object::get(interp.global_this, &interp.gc_heap, default_name).ok_or_else(|| {
            NativeError::TypeError {
                name: method,
                reason: format!("%{default_name}% intrinsic missing"),
            }
        })?
    };
    let constructor = crate::regexp_prototype::species_constructor_runtime(
        ctx,
        &exemplar_value,
        &default_ctor,
        method,
    )?;

    let mut argv: SmallVec<[Value; 8]> = SmallVec::new();
    argv.push(Value::number(crate::number::NumberValue::from_f64(
        length as f64,
    )));
    let (interp, exec) = ctx.interp_mut_and_context();
    let exec = exec.ok_or_else(|| NativeError::TypeError {
        name: method,
        reason: "missing execution context".to_string(),
    })?;
    let result = interp
        .run_construct_sync(&exec, &constructor, constructor, argv)
        .map_err(|e| vm_to_native(e, method))?;

    let Some(new_ta) = result.as_typed_array(&interp.gc_heap) else {
        return Err(NativeError::TypeError {
            name: method,
            reason: "Species constructor did not return a TypedArray".to_string(),
        });
    };
    if new_ta.buffer(&interp.gc_heap).is_detached(&interp.gc_heap) {
        return Err(NativeError::TypeError {
            name: method,
            reason: "Species constructor returned a TypedArray with a detached buffer".to_string(),
        });
    }
    if new_ta.length(&interp.gc_heap) < length {
        return Err(NativeError::TypeError {
            name: method,
            reason: "Species constructor returned a TypedArray smaller than the source".to_string(),
        });
    }
    Ok(new_ta)
}

/// §22.2.6.1 `get %TypedArray%.prototype.buffer` — return the
/// receiver's [[ViewedArrayBuffer]] or raise TypeError on
/// non-TypedArray receivers.
fn ta_buffer_getter(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let t = ctx
        .this_value()
        .as_typed_array(ctx.heap())
        .ok_or_else(|| NativeError::TypeError {
            name: "TypedArray.prototype.buffer",
            reason: "this is not a TypedArray".to_string(),
        })?;
    Ok(Value::array_buffer(t.buffer(ctx.heap())))
}

/// §22.2.6.2 `get %TypedArray%.prototype.byteLength`.
fn ta_byte_length_getter(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let t = ctx
        .this_value()
        .as_typed_array(ctx.heap())
        .ok_or_else(|| NativeError::TypeError {
            name: "TypedArray.prototype.byteLength",
            reason: "this is not a TypedArray".to_string(),
        })?;
    let n = t.byte_length(ctx.heap());
    Ok(Value::number(crate::number::NumberValue::from_f64(
        n as f64,
    )))
}

/// §22.2.6.3 `get %TypedArray%.prototype.byteOffset`.
fn ta_byte_offset_getter(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let t = ctx
        .this_value()
        .as_typed_array(ctx.heap())
        .ok_or_else(|| NativeError::TypeError {
            name: "TypedArray.prototype.byteOffset",
            reason: "this is not a TypedArray".to_string(),
        })?;
    let n = t.byte_offset(ctx.heap());
    Ok(Value::number(crate::number::NumberValue::from_f64(
        n as f64,
    )))
}

/// §22.2.6.18 `get %TypedArray%.prototype.length`.
fn ta_length_getter(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let t = ctx
        .this_value()
        .as_typed_array(ctx.heap())
        .ok_or_else(|| NativeError::TypeError {
            name: "TypedArray.prototype.length",
            reason: "this is not a TypedArray".to_string(),
        })?;
    let n = t.length(ctx.heap());
    Ok(Value::number(crate::number::NumberValue::from_f64(
        n as f64,
    )))
}

/// §22.2.6.15 `get %TypedArray%.prototype [ @@toStringTag ]` — return
/// the receiver's element-kind name (`"Int8Array"`, …), or undefined
/// if the receiver is not a TypedArray.
///
/// <https://tc39.es/ecma262/#sec-get-%typedarray%.prototype-%symbol.tostringtag%>
fn tostring_tag_getter(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let this_value = *ctx.this_value();
    let Some(t) = this_value.as_typed_array(ctx.heap()) else {
        return Ok(Value::undefined());
    };
    let kind_name = t.kind().name();

    Ok(Value::string(
        crate::string::JsString::from_str(kind_name, ctx.heap_mut()).map_err(|_| {
            NativeError::TypeError {
                name: "TypedArray.prototype[@@toStringTag]",
                reason: "out of memory".to_string(),
            }
        })?,
    ))
}

// ---------------------------------------------------------------
// Per-kind constructor wrappers
// ---------------------------------------------------------------

macro_rules! ta_ctor {
    ($name:ident, $kind:expr) => {
        fn $name(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
            ta_ctor_dispatch(ctx, args, $kind)
        }
    };
}

/// Build a per-kind `from` static for the concrete TypedArray
/// constructor. Mirrors §23.2.2.1 `%TypedArray%.from(source [,
/// mapfn [, thisArg]])` for the common cases (no `mapfn` and the
/// receiver is a known concrete constructor). The basic shape
/// (`Int8Array.from([1,2,3])`, `Int8Array.from(otherTA)`,
/// `Int8Array.from(arrayLike)`) routes through the existing
/// `typed_array_call_with_roots` dispatch under `M::From`.
macro_rules! ta_static_from {
    ($name:ident, $kind:expr) => {
        fn $name(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
            ta_from_dispatch(ctx, args, $kind)
        }
    };
}

macro_rules! ta_static_of {
    ($name:ident, $kind:expr) => {
        fn $name(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
            ta_of_dispatch(ctx, args, $kind)
        }
    };
}

ta_static_from!(from_int8, TypedArrayKind::Int8);
ta_static_from!(from_uint8, TypedArrayKind::Uint8);
ta_static_from!(from_uint8_clamped, TypedArrayKind::Uint8Clamped);
ta_static_from!(from_int16, TypedArrayKind::Int16);
ta_static_from!(from_uint16, TypedArrayKind::Uint16);
ta_static_from!(from_int32, TypedArrayKind::Int32);
ta_static_from!(from_uint32, TypedArrayKind::Uint32);
ta_static_from!(from_float32, TypedArrayKind::Float32);
ta_static_from!(from_float64, TypedArrayKind::Float64);
ta_static_from!(from_bigint64, TypedArrayKind::BigInt64);
ta_static_from!(from_biguint64, TypedArrayKind::BigUint64);

ta_static_of!(of_int8, TypedArrayKind::Int8);
ta_static_of!(of_uint8, TypedArrayKind::Uint8);
ta_static_of!(of_uint8_clamped, TypedArrayKind::Uint8Clamped);
ta_static_of!(of_int16, TypedArrayKind::Int16);
ta_static_of!(of_uint16, TypedArrayKind::Uint16);
ta_static_of!(of_int32, TypedArrayKind::Int32);
ta_static_of!(of_uint32, TypedArrayKind::Uint32);
ta_static_of!(of_float32, TypedArrayKind::Float32);
ta_static_of!(of_float64, TypedArrayKind::Float64);
ta_static_of!(of_bigint64, TypedArrayKind::BigInt64);
ta_static_of!(of_biguint64, TypedArrayKind::BigUint64);

fn ta_from_dispatch(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    kind: TypedArrayKind,
) -> Result<Value, NativeError> {
    // §23.2.2.1 %TypedArray%.from(source, mapfn, thisArg).
    let name = typed_array_name(kind);
    let exec = ctx
        .execution_context()
        .cloned()
        .ok_or_else(|| NativeError::TypeError {
            name,
            reason: "missing execution context".to_string(),
        })?;
    let source = args.first().cloned().unwrap_or(Value::undefined());
    let mapfn = args.get(1).cloned().unwrap_or(Value::undefined());
    let mapping = !mapfn.is_undefined();
    if mapping && !ctx.cx.interp.is_callable_runtime(&mapfn) {
        return Err(NativeError::TypeError {
            name,
            reason: "mapfn is not a function".to_string(),
        });
    }
    let this_arg = args.get(2).cloned().unwrap_or(Value::undefined());
    // §23.2.2.1 step 4 — GetMethod(source, @@iterator). `GetV` on a
    // null / undefined source throws a TypeError before that.
    if source.is_null() || source.is_undefined() {
        return Err(NativeError::TypeError {
            name,
            reason: "cannot create a TypedArray from null or undefined".to_string(),
        });
    }
    let iter_sym = ctx
        .cx
        .interp
        .well_known_symbols()
        .get(crate::symbol::WellKnown::Iterator);
    let iter_method = ta_get_via(ctx, &exec, source, &crate::VmPropertyKey::Symbol(iter_sym))?;
    let raw = if ctx.cx.interp.is_callable_runtime(&iter_method) {
        drain_iterable_into_values(ctx, &exec, &source, iter_method)?
    } else {
        read_array_like_raw(ctx, &exec, source)?
    };
    let mut out: Vec<Value> = Vec::new();
    if out.try_reserve_exact(raw.len()).is_err() {
        return Err(NativeError::RangeError {
            name,
            reason: "Invalid typed array length".to_string(),
        });
    }
    for (k, value) in raw.into_iter().enumerate() {
        // §23.2.2.1 step 6.e — map the source value (mapfn(value, k))
        // before the numeric conversion, then convert with
        // ToNumber / ToBigInt so a Symbol / cross-type element throws.
        let mapped = if mapping {
            let index = Value::number(crate::number::NumberValue::from_f64(k as f64));
            ctx.cx
                .interp
                .run_callable_sync(&exec, &mapfn, this_arg, smallvec::smallvec![value, index])
                .map_err(|e| vm_to_native(e, name))?
        } else {
            value
        };
        let converted = if kind.is_bigint() {
            let big = crate::coerce::to_big_int_or_throw(ctx.cx.interp, &exec, &mapped)
                .map_err(|e| vm_to_native(e, name))?;
            Value::big_int(big)
        } else {
            let number = crate::coerce::to_number_or_throw(ctx.cx.interp, &exec, &mapped)
                .map_err(|e| vm_to_native(e, name))?;
            Value::number(number)
        };
        out.push(converted);
    }
    let arr = ctx
        .array_from_elements(out)
        .map_err(|_| NativeError::TypeError {
            name,
            reason: "out of memory while allocating array".to_string(),
        })?;
    let arr_value = Value::array(arr);
    let roots = ctx.collect_native_roots();
    let this_value = *ctx.this_value();
    let arr_slice = [arr_value];
    let mut external_visit = |visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
        crate::runtime_cx::visit_native_roots(
            visitor,
            &roots,
            &this_value,
            None,
            &[],
            &[&arr_slice],
        );
    };
    dispatch::typed_array_call_with_roots(
        kind,
        TypedArrayMethod::From,
        std::slice::from_ref(&arr_value),
        ctx.heap_mut(),
        &mut external_visit,
    )
    .map_err(|e| vm_to_native(e, name))
}

fn ta_of_dispatch(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    kind: TypedArrayKind,
) -> Result<Value, NativeError> {
    let roots = ctx.collect_native_roots();
    let this_value = *ctx.this_value();
    let mut external_visit = |visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
        crate::runtime_cx::visit_native_roots(visitor, &roots, &this_value, None, &[], &[args]);
    };
    dispatch::typed_array_call_with_roots(
        kind,
        TypedArrayMethod::Of,
        args,
        ctx.heap_mut(),
        &mut external_visit,
    )
    .map_err(|e| vm_to_native(e, typed_array_name(kind)))
}

ta_ctor!(ctor_int8, TypedArrayKind::Int8);
ta_ctor!(ctor_uint8, TypedArrayKind::Uint8);
ta_ctor!(ctor_uint8_clamped, TypedArrayKind::Uint8Clamped);
ta_ctor!(ctor_int16, TypedArrayKind::Int16);
ta_ctor!(ctor_uint16, TypedArrayKind::Uint16);
ta_ctor!(ctor_int32, TypedArrayKind::Int32);
ta_ctor!(ctor_uint32, TypedArrayKind::Uint32);
ta_ctor!(ctor_float32, TypedArrayKind::Float32);
ta_ctor!(ctor_float64, TypedArrayKind::Float64);
ta_ctor!(ctor_bigint64, TypedArrayKind::BigInt64);
ta_ctor!(ctor_biguint64, TypedArrayKind::BigUint64);

fn ta_ctor_dispatch(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    kind: TypedArrayKind,
) -> Result<Value, NativeError> {
    if !ctx.is_construct_call() {
        return Err(NativeError::TypeError {
            name: typed_array_name(kind),
            reason: "constructor requires 'new'".to_string(),
        });
    }
    // §22.2.4.5 `TypedArray(buffer [, byteOffset [, length]])` —
    // ToIndex(byteOffset) / ToIndex(length) per spec invoke
    // ToPrimitive(Number) → ToIntegerOrInfinity. The dispatch
    // helper only handles primitive operands; pre-coerce non-
    // primitive Object args here so user `@@toPrimitive` /
    // `valueOf` / `toString` hooks fire.
    // <https://tc39.es/ecma262/#sec-typedarray-buffer-byteoffset-length>
    let exec = ctx.execution_context().cloned();
    // §22.2.4.4 — when the source argument is an Object with
    // `@@iterator`, drain that iterator into an Array up-front so
    // the per-kind dispatcher's array-like path collects the
    // yielded values rather than reading the (probably-undefined)
    // `length` own slot.
    let iter_pre: Option<SmallVec<[Value; 4]>> = if let (Some(src_obj), Some(exec)) =
        (args.first().and_then(|v| v.as_object()), exec.as_ref())
    {
        let iter_sym = ctx
            .cx
            .interp
            .well_known_symbols()
            .get(crate::symbol::WellKnown::Iterator);
        let src_value = Value::object(src_obj);
        // §23.2.5.1 — GetMethod(source, @@iterator): an Object source
        // with a callable `@@iterator` initializes from the drained
        // iterator; otherwise it is an array-like read with observable
        // `[[Get]]` + `ToNumber` / `ToBigInt`. Either way the values are
        // coerced up-front so the per-kind dispatcher only narrows.
        let iter_method = ta_get_via(
            ctx,
            exec,
            src_value,
            &crate::VmPropertyKey::Symbol(iter_sym),
        )?;
        let drained = if ctx.cx.interp.is_callable_runtime(&iter_method) {
            // §22.2.4.4 — IterableToList collects raw values, then each
            // is converted (ToNumber / ToBigInt) when stored.
            let raw = drain_iterable_into_values(ctx, exec, &src_value, iter_method)?;
            coerce_values_for_kind(ctx, exec, raw, kind)?
        } else {
            read_array_like_coerced(ctx, exec, src_value, kind)?
        };
        let arr = ctx
            .array_from_elements(drained)
            .map_err(|_| NativeError::TypeError {
                name: typed_array_name(kind),
                reason: "out of memory while allocating array".to_string(),
            })?;
        let mut out: SmallVec<[Value; 4]> = SmallVec::new();
        out.push(Value::array(arr));
        for v in args.iter().skip(1) {
            out.push(*v);
        }
        Some(out)
    } else {
        None
    };
    let coerced: SmallVec<[Value; 4]> = if let Some(pre) = iter_pre {
        pre
    } else if args.first().is_some_and(|v| v.is_array_buffer()) {
        if let Some(exec) = &exec {
            let mut out: SmallVec<[Value; 4]> = args.iter().cloned().collect();
            for idx in 1..=2 {
                let Some(slot) = out.get_mut(idx) else {
                    continue;
                };
                let object_like = slot.is_object()
                    || slot.is_array()
                    || slot.is_function()
                    || slot.is_closure()
                    || slot.is_native_function()
                    || slot.is_bound_function()
                    || slot.is_class_constructor()
                    || slot.is_proxy()
                    || slot.is_regexp();
                if !object_like {
                    continue;
                }
                let interp = ctx.interp_mut();
                let primitive = interp
                    .evaluate_to_primitive(exec, slot, crate::abstract_ops::ToPrimitiveHint::Number)
                    .map_err(|e| NativeError::TypeError {
                        name: typed_array_name(kind),
                        reason: e.to_string(),
                    })?;
                *slot = primitive;
            }
            out
        } else {
            args.iter().cloned().collect()
        }
    } else {
        args.iter().cloned().collect()
    };
    let coerced_slice: &[Value] = coerced.as_slice();
    let roots = ctx.collect_native_roots();
    let this_value = *ctx.this_value();
    let new_target = ctx.new_target().cloned();
    let mut external_visit = |visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
        crate::runtime_cx::visit_native_roots(
            visitor,
            &roots,
            &this_value,
            new_target.as_ref(),
            &[],
            &[coerced_slice],
        );
    };
    let value = dispatch::typed_array_call_with_roots(
        kind,
        TypedArrayMethod::Construct,
        coerced_slice,
        ctx.heap_mut(),
        &mut external_visit,
    )
    .map_err(|e| vm_to_native(e, typed_array_name(kind)))?;
    // §10.1.13 GetPrototypeFromConstructor — derived `super()`
    // construction forwards `new.target`, so the allocated typed
    // array receives `Subclass.prototype` as its observable
    // [[Prototype]].
    // <https://tc39.es/ecma262/#sec-getprototypefromconstructor>
    let needs_proto_override = !ctx.new_target().is_some_and(|v| v.is_native_function());
    if needs_proto_override
        && let Some(proto) =
            crate::bootstrap::native_new_target_prototype(ctx, typed_array_name(kind))?
    {
        ctx.interp_mut()
            .set_non_gc_exotic_prototype_override(&value, Some(proto));
    }
    Ok(value)
}

const fn typed_array_name(kind: TypedArrayKind) -> &'static str {
    match kind {
        TypedArrayKind::Int8 => "Int8Array",
        TypedArrayKind::Uint8 => "Uint8Array",
        TypedArrayKind::Uint8Clamped => "Uint8ClampedArray",
        TypedArrayKind::Int16 => "Int16Array",
        TypedArrayKind::Uint16 => "Uint16Array",
        TypedArrayKind::Int32 => "Int32Array",
        TypedArrayKind::Uint32 => "Uint32Array",
        TypedArrayKind::Float32 => "Float32Array",
        TypedArrayKind::Float64 => "Float64Array",
        TypedArrayKind::BigInt64 => "BigInt64Array",
        TypedArrayKind::BigUint64 => "BigUint64Array",
    }
}

// ---------------------------------------------------------------
// Prototype method wrappers — pure methods delegate to the shared
// typed-array implementation module.
// ---------------------------------------------------------------

macro_rules! ta_proto_method {
    ($name:ident, $method:expr) => {
        fn $name(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
            ta_proto_dispatch(ctx, args, $method)
        }
    };
}

ta_proto_method!(ta_at, "at");
ta_proto_method!(ta_fill, "fill");
ta_proto_method!(ta_copy_within, "copyWithin");
ta_proto_method!(ta_reverse, "reverse");
ta_proto_method!(ta_index_of, "indexOf");
ta_proto_method!(ta_last_index_of, "lastIndexOf");
ta_proto_method!(ta_includes, "includes");
ta_proto_method!(ta_join, "join");
ta_proto_method!(ta_to_string, "toString");
ta_proto_method!(ta_to_locale_string, "toLocaleString");
ta_proto_method!(ta_set, "set");
ta_proto_method!(ta_to_reversed, "toReversed");
ta_proto_method!(ta_to_sorted, "toSorted");
ta_proto_method!(ta_sort, "sort");
ta_proto_method!(ta_with, "with");
ta_proto_method!(ta_keys, "keys");
ta_proto_method!(ta_values, "values");
ta_proto_method!(ta_entries, "entries");

fn ta_proto_dispatch(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    method_name: &str,
) -> Result<Value, NativeError> {
    const NAME: &str = "TypedArray.prototype";
    let impl_fn =
        typed_array_prototype::method_impl(method_name).ok_or_else(|| NativeError::TypeError {
            name: NAME,
            reason: format!("method {method_name} missing"),
        })?;
    let receiver = *ctx.this_value();
    let mut small_args: SmallVec<[Value; 4]> = args.iter().cloned().collect();

    // §23.2.3.{8,5} — `fill` / `copyWithin` open with `ToNumber` /
    // `ToIntegerOrInfinity` on their operands (and `fill` coerces its
    // value first, as a BigInt for bigint element kinds). The intrinsic
    // impl reads raw `Value`s, so coerce here in spec order.
    //
    // `includes` / `indexOf` / `lastIndexOf` coerce their `fromIndex`
    // inside the impl instead: §23.2.3.16 runs ToIntegerOrInfinity only
    // after ValidateTypedArray + the length read, so a `valueOf` that
    // detaches the buffer must not pre-empt that ordering.
    let int_coerce: &[usize] = match method_name {
        "fill" => &[1, 2],
        "copyWithin" => &[0, 1, 2],
        _ => &[],
    };
    if method_name == "fill" || !int_coerce.is_empty() {
        let is_bigint = receiver
            .as_typed_array(ctx.heap())
            .is_some_and(|t| t.kind().is_bigint());
        let (interp, ctx_opt) = ctx.interp_mut_and_context();
        if let Some(context) = ctx_opt {
            if method_name == "fill"
                && let Some(value) = small_args.first().copied()
            {
                if is_bigint {
                    let b = crate::coerce::to_big_int_or_throw(interp, &context, &value)
                        .map_err(|e| crate::native_function::vm_to_native_error(e, NAME))?;
                    small_args[0] = Value::big_int(b);
                } else if !value.is_number() {
                    let n = interp
                        .coerce_to_number(&context, &value)
                        .map_err(|e| crate::native_function::vm_to_native_error(e, NAME))?;
                    small_args[0] = Value::number(n);
                }
            }
            for &idx in int_coerce {
                let Some(value) = small_args.get(idx).copied() else {
                    continue;
                };
                if value.is_number() || value.is_undefined() {
                    continue;
                }
                let n = interp
                    .coerce_to_number(&context, &value)
                    .map_err(|e| crate::native_function::vm_to_native_error(e, NAME))?;
                small_args[idx] = Value::number(n);
            }
        }
    }

    impl_fn(ctx, &small_args)
}

// §23.2.3 callback-driven prototype methods re-enter the interpreter to
// drive synchronous callbacks and `TypedArraySpeciesCreate`.

macro_rules! ta_cb_method {
    ($name:ident, $method:expr) => {
        fn $name(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
            ta_callback_dispatch(ctx, args, $method)
        }
    };
}

ta_cb_method!(ta_map, "map");
ta_cb_method!(ta_filter, "filter");
ta_cb_method!(ta_for_each, "forEach");
ta_cb_method!(ta_every, "every");
ta_cb_method!(ta_some, "some");
ta_cb_method!(ta_find, "find");
ta_cb_method!(ta_find_index, "findIndex");
ta_cb_method!(ta_find_last, "findLast");
ta_cb_method!(ta_find_last_index, "findLastIndex");
ta_cb_method!(ta_reduce, "reduce");
ta_cb_method!(ta_reduce_right, "reduceRight");

/// §23.2.3.26 / §23.2.3.27 `slice` / `subarray` — both run
/// `TypedArraySpeciesCreate` and `ToIntegerOrInfinity` operand coercion.
fn ta_slice(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    ta_species_dispatch(ctx, args, "slice")
}

fn ta_subarray(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    ta_species_dispatch(ctx, args, "subarray")
}

fn ta_species_dispatch(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    method_name: &'static str,
) -> Result<Value, NativeError> {
    let receiver = *ctx.this_value();
    let Some(t) = receiver.as_typed_array(ctx.heap()) else {
        return Err(NativeError::TypeError {
            name: method_name,
            reason: "method called on a non-TypedArray receiver".to_string(),
        });
    };
    let (interp, ctx_opt) = ctx.interp_mut_and_context();
    let context = ctx_opt.ok_or(NativeError::TypeError {
        name: method_name,
        reason: "missing execution context".to_string(),
    })?;
    let result = if method_name == "slice" {
        interp.typed_array_slice_value_dispatch(&context, &t, args)
    } else {
        interp.typed_array_subarray_value_dispatch(&context, &t, args)
    };
    result.map_err(|err| crate::native_function::vm_to_native_error(err, method_name))
}

fn ta_callback_dispatch(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    method_name: &'static str,
) -> Result<Value, NativeError> {
    let receiver = *ctx.this_value();
    let Some(t) = receiver.as_typed_array(ctx.heap()) else {
        return Err(NativeError::TypeError {
            name: method_name,
            reason: "method called on a non-TypedArray receiver".to_string(),
        });
    };
    let (interp, ctx_opt) = ctx.interp_mut_and_context();
    let context = ctx_opt.ok_or(NativeError::TypeError {
        name: method_name,
        reason: "missing execution context".to_string(),
    })?;
    interp
        .typed_array_callback_value_dispatch(&context, &t, method_name, args)
        .map_err(|err| crate::native_function::vm_to_native_error(err, method_name))
}

// ---------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------

fn vm_to_native(err: VmError, name: &'static str) -> NativeError {
    match err {
        VmError::TypeError { message } => NativeError::TypeError {
            name,
            reason: message,
        },
        VmError::TypeMismatch => NativeError::TypeError {
            name,
            reason: "type mismatch".to_string(),
        },
        VmError::RangeError { message } => NativeError::RangeError {
            name,
            reason: message,
        },
        other => NativeError::TypeError {
            name,
            reason: other.to_string(),
        },
    }
}

// ---------------------------------------------------------------
// Abstract %TypedArray% + per-kind couch!-driven installers.
// ---------------------------------------------------------------

/// Sentinel-named property on `globalThis` that holds the abstract
/// `%TypedArray%` constructor. Hidden by a leading `@@` prefix to
/// avoid colliding with any user-visible global. The matching
/// abstract `%TypedArray%.prototype` is reached through
/// `<abstract>.prototype` (couch! emits the standard prototype data
/// property when the prototype block is non-empty).
const ABSTRACT_CTOR_SLOT: &str = "@@%TypedArray%";

/// §23.2.1.1 — calling `%TypedArray%` directly always throws a
/// `TypeError`. The abstract constructor is never observably
/// instantiated.
fn abstract_typed_array_call(
    _ctx: &mut NativeCtx<'_>,
    _args: &[Value],
) -> Result<Value, NativeError> {
    Err(NativeError::TypeError {
        name: "TypedArray",
        reason: "Abstract class TypedArray not directly constructable".to_string(),
    })
}

otter_macros::couch! {
    name = "@@%TypedArray%",
    feature = CORE,
    intrinsic = AbstractTypedArrayIntrinsic,
    constructor = (length = 0, call = abstract_typed_array_call),
    prototype = {
        methods = {
            "at"             / 1 => ta_at,
            "subarray"       / 2 => ta_subarray,
            "slice"          / 2 => ta_slice,
            "fill"           / 3 => ta_fill,
            "copyWithin"     / 3 => ta_copy_within,
            "reverse"        / 0 => ta_reverse,
            "indexOf"        / 1 => ta_index_of,
            "lastIndexOf"    / 1 => ta_last_index_of,
            "includes"       / 1 => ta_includes,
            "join"           / 1 => ta_join,
            "toString"       / 0 => ta_to_string,
            "toLocaleString" / 0 => ta_to_locale_string,
            "set"            / 2 => ta_set,
            "toReversed"     / 0 => ta_to_reversed,
            "toSorted"       / 1 => ta_to_sorted,
            "sort"           / 1 => ta_sort,
            "with"           / 2 => ta_with,
            "keys"           / 0 => ta_keys,
            "values"         / 0 => ta_values,
            "entries"        / 0 => ta_entries,
            "map"            / 1 => ta_map,
            "filter"         / 1 => ta_filter,
            "forEach"        / 1 => ta_for_each,
            "every"          / 1 => ta_every,
            "some"           / 1 => ta_some,
            "find"           / 1 => ta_find,
            "findIndex"      / 1 => ta_find_index,
            "findLast"       / 1 => ta_find_last,
            "findLastIndex"  / 1 => ta_find_last_index,
            "reduce"         / 1 => ta_reduce,
            "reduceRight"    / 1 => ta_reduce_right,
        },
    },
}

/// Safe `Option` variant of [`abstract_typed_array_proto_lookup`]
/// for callers that run before the abstract ctor is guaranteed to
/// exist (e.g. post-bootstrap well-known fixups that walk every
/// per-kind prototype regardless of installation order).
fn get_abstract_typed_array_prototype(
    global: JsObject,
    heap: &mut otter_gc::GcHeap,
) -> Option<JsObject> {
    let ctor = object::get(global, heap, ABSTRACT_CTOR_SLOT)?.as_native_function()?;
    let desc = ctor
        .own_property_descriptor(heap, "prototype")
        .ok()
        .flatten()?;
    match desc.kind {
        crate::object::DescriptorKind::Data { value } => value.as_object(),
        _ => None,
    }
}

/// Resolve `%TypedArray%.prototype` for per-kind couch! invocations
/// via `prototype.parent`. Panics if `AbstractTypedArrayIntrinsic`
/// has not yet run — bootstrap enforces declaration order, so this
/// is unreachable under the supported install path.
fn abstract_typed_array_proto_lookup(global: JsObject, heap: &mut otter_gc::GcHeap) -> JsObject {
    let ctor = object::get(global, heap, ABSTRACT_CTOR_SLOT)
        .and_then(|v| v.as_native_function())
        .expect("abstract %TypedArray% ctor must be installed before per-kind installers");
    let desc = ctor
        .own_property_descriptor(heap, "prototype")
        .ok()
        .flatten()
        .expect("abstract %TypedArray%.prototype must exist");
    match desc.kind {
        crate::object::DescriptorKind::Data { value } => value
            .as_object()
            .expect("abstract %TypedArray%.prototype must be an object"),
        _ => panic!("abstract %TypedArray%.prototype must be a data descriptor"),
    }
}

/// Resolve `%TypedArray%` as a `Value` for per-kind couch! ctors via
/// `ctor_parent`. Per §23.2.6.1 each concrete TypedArray constructor
/// inherits from `%TypedArray%`.
fn abstract_typed_array_ctor_lookup(global: JsObject, heap: &mut otter_gc::GcHeap) -> Value {
    object::get(global, heap, ABSTRACT_CTOR_SLOT)
        .expect("abstract %TypedArray% ctor must be installed before per-kind installers")
}

/// Declarative wrapper that emits one `couch!` invocation for a
/// concrete TypedArray kind. Each kind pins its `BYTES_PER_ELEMENT`
/// on both ctor and prototype, chains the prototype to
/// `%TypedArray%.prototype`, and overrides the ctor's `[[Prototype]]`
/// to `%TypedArray%`.
macro_rules! typed_array_kind {
    ($name:literal, $intrinsic:ident, $bpe:expr, $ctor:ident, $from:ident, $of:ident) => {
        otter_macros::couch! {
            name = $name,
            feature = CORE,
            intrinsic = $intrinsic,
            constructor = (length = 3, call = $ctor),
            statics = {
                "from" / 1 => $from,
                "of"   / 0 => $of,
            },
            static_constants = [
                ("BYTES_PER_ELEMENT", Number($bpe)),
            ],
            prototype = {
                parent = abstract_typed_array_proto_lookup,
            },
            prototype_constants = [
                ("BYTES_PER_ELEMENT", Number($bpe)),
            ],
            ctor_parent = abstract_typed_array_ctor_lookup,
        }
    };
}

typed_array_kind!(
    "Int8Array",
    Int8ArrayIntrinsic,
    1.0,
    ctor_int8,
    from_int8,
    of_int8
);
typed_array_kind!(
    "Uint8Array",
    Uint8ArrayIntrinsic,
    1.0,
    ctor_uint8,
    from_uint8,
    of_uint8
);
typed_array_kind!(
    "Uint8ClampedArray",
    Uint8ClampedArrayIntrinsic,
    1.0,
    ctor_uint8_clamped,
    from_uint8_clamped,
    of_uint8_clamped
);
typed_array_kind!(
    "Int16Array",
    Int16ArrayIntrinsic,
    2.0,
    ctor_int16,
    from_int16,
    of_int16
);
typed_array_kind!(
    "Uint16Array",
    Uint16ArrayIntrinsic,
    2.0,
    ctor_uint16,
    from_uint16,
    of_uint16
);
typed_array_kind!(
    "Int32Array",
    Int32ArrayIntrinsic,
    4.0,
    ctor_int32,
    from_int32,
    of_int32
);
typed_array_kind!(
    "Uint32Array",
    Uint32ArrayIntrinsic,
    4.0,
    ctor_uint32,
    from_uint32,
    of_uint32
);
typed_array_kind!(
    "Float32Array",
    Float32ArrayIntrinsic,
    4.0,
    ctor_float32,
    from_float32,
    of_float32
);
typed_array_kind!(
    "Float64Array",
    Float64ArrayIntrinsic,
    8.0,
    ctor_float64,
    from_float64,
    of_float64
);
typed_array_kind!(
    "BigInt64Array",
    BigInt64ArrayIntrinsic,
    8.0,
    ctor_bigint64,
    from_bigint64,
    of_bigint64
);
typed_array_kind!(
    "BigUint64Array",
    BigUint64ArrayIntrinsic,
    8.0,
    ctor_biguint64,
    from_biguint64,
    of_biguint64
);
