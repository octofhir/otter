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
//! - Constructor re-entry stays on the current runtime turn. Transient element
//!   buffers are traced in place without a runtime-root snapshot, and each
//!   freshly allocated view is handle-rooted across observable `new.target`
//!   prototype lookup.
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
use crate::{Local, NativeCtx, NativeError, NativeScope, Value, VmError};

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

    // §23.2.2 — the abstract constructor's observable `name` is
    // "TypedArray"; the global slot key keeps the hidden `@@` form.
    if let Some(ctor) =
        object::get(global, heap, ABSTRACT_CTOR_SLOT).and_then(|v| v.as_native_function())
    {
        let name_val = Value::string(
            crate::string::JsString::from_str("TypedArray", heap)
                .map_err(|_| JsSurfaceError::OutOfMemory)?,
        );
        ctor.define_own_property(
            heap,
            "name",
            PropertyDescriptor::data(name_val, false, false, true),
        );
    }

    // §22.2.6 — `%TypedArray%.prototype[@@toStringTag]` is an
    // accessor on the abstract prototype. The getter returns the
    // receiver's [[TypedArrayName]] (the kind name string) or
    // `undefined` for non-TypedArray receivers. Per-kind
    // prototypes inherit the accessor; per-instance access walks
    // up to %TypedArray%.prototype and triggers the getter.
    if let Some(abstract_proto) = get_abstract_typed_array_prototype(global, heap) {
        // §23.2.3.34 — %TypedArray%.prototype.toString is the SAME
        // function object as %Array.prototype.toString%.
        let array_to_string = object::get(global, heap, "Array")
            .and_then(|ctor| {
                if let Some(obj) = ctor.as_object() {
                    object::get(obj, heap, "prototype")
                } else if let Some(nf) = ctor.as_native_function() {
                    nf.own_property_descriptor(heap, "prototype")
                        .ok()
                        .flatten()
                        .and_then(|d| match d.kind {
                            crate::object::DescriptorKind::Data { value } => Some(value),
                            _ => None,
                        })
                } else {
                    None
                }
            })
            .and_then(|proto| proto.as_object())
            .and_then(|proto| object::get(proto, heap, "toString"));
        if let Some(fun) = array_to_string {
            object::define_own_property(
                abstract_proto,
                heap,
                "toString",
                PropertyDescriptor::data(fun, true, false, true),
            );
        }
        let abstract_proto_root = Value::object(abstract_proto);
        let getter = crate::bootstrap::native_static_with_value_roots(
            heap,
            "get [Symbol.toStringTag]",
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
    // §23.2 uint8array-base64 — Uint8Array.{fromBase64,fromHex} statics
    // and the toBase64 / toHex prototype methods.
    crate::uint8_base64::install_uint8_base64(heap, global)?;
    Ok(())
}

/// Drain a JS iterable into a `Vec<Value>` by calling its
/// `[Symbol.iterator]` method and pumping the resulting iterator
/// until completion. Used by the §22.2.4.4 `new TA(iterable)`
/// constructor path.
/// §7.4.2 GetIterator + drain — call the already-fetched `@@iterator`
/// method (one `GetMethod`, per spec) and collect every yielded value.
fn drain_iterable_into_values<'scope>(
    scope: &mut NativeScope<'scope, '_>,
    exec_ctx: &crate::ExecutionContext,
    src: Local<'scope>,
    iter_method: Local<'scope>,
) -> Result<Vec<Local<'scope>>, NativeError> {
    if !scope.is_callable(iter_method) {
        return Err(NativeError::TypeError {
            name: "TypedArray",
            reason: "source object is not iterable".to_string(),
        });
    }
    let iter_obj = scope.call(iter_method, src, &[])?;
    // §7.4.2 GetIteratorFromMethod step 4 — `next` is read off the
    // iterator object as a property, so a user-overridden
    // `%ArrayIteratorPrototype%.next` (or any custom `next`) drives
    // the drain rather than the engine's internal iterator step.
    let next_method = ta_get_via(
        scope,
        exec_ctx,
        iter_obj,
        &crate::VmPropertyKey::String("next"),
    )?;
    if !scope.is_callable(next_method) {
        return Err(NativeError::TypeError {
            name: "TypedArray",
            reason: "iterator.next is not callable".to_string(),
        });
    }
    if scope.raw(iter_obj).as_iterator().is_some()
        && scope
            .raw(next_method)
            .as_native_function()
            .is_some_and(|native| {
                native.is_static_fn(
                    scope.context().heap(),
                    crate::intrinsics::iterator::iterator_proto_next,
                )
            })
    {
        let mut collected = Vec::new();
        loop {
            let handle = scope
                .raw(iter_obj)
                .as_iterator()
                .expect("iterator local changed kind");
            let next = scope.with_turn_parts(|interp, stack| {
                interp.iterator_next_full(exec_ctx, stack, &handle)
            });
            let (value, done) =
                next.map_err(|e| vm_to_native(scope.context().interp_mut(), e, "TypedArray"))?;
            if done {
                break;
            }
            collected.push(scope.value(value));
        }
        return Ok(collected);
    }
    let mut collected = Vec::new();
    loop {
        // §7.4.3 IteratorNext — Call(next, iterator); the result must
        // be an Object, then read `done` / `value` observably.
        let result = scope.call(next_method, iter_obj, &[])?;
        if !crate::reflect::is_type_object_value(&scope.raw(result)) {
            return Err(NativeError::TypeError {
                name: "TypedArray",
                reason: "iterator result is not an object".to_string(),
            });
        }
        let done = ta_get_via(
            scope,
            exec_ctx,
            result,
            &crate::VmPropertyKey::String("done"),
        )?;
        if scope.raw(done).to_boolean(scope.context().heap()) {
            break;
        }
        let value = ta_get_via(
            scope,
            exec_ctx,
            result,
            &crate::VmPropertyKey::String("value"),
        )?;
        collected.push(value);
    }
    Ok(collected)
}

/// §23.2.5.1 / §22.2.4.4 — convert each collected source value with
/// `ToBigInt` for BigInt element types and `ToNumber` otherwise, so a
/// Symbol / cross-numeric value throws and a `valueOf` / `toString`
/// runs. The per-kind dispatcher narrows the result on store.
fn coerce_values_for_kind<'scope>(
    scope: &mut NativeScope<'scope, '_>,
    exec: &crate::ExecutionContext,
    values: Vec<Local<'scope>>,
    kind: TypedArrayKind,
) -> Result<Vec<Local<'scope>>, NativeError> {
    let mut out = Vec::with_capacity(values.len());
    for value in values {
        let value = scope.raw(value);
        let converted = if kind.is_bigint() {
            let big = scope.with_turn_parts(|interp, stack| {
                crate::coerce::to_big_int_or_throw(interp, stack, exec, &value)
                    .map_err(|e| vm_to_native(interp, e, "TypedArray"))
            })?;
            Value::big_int(big)
        } else {
            let number = scope.with_turn_parts(|interp, stack| {
                crate::coerce::to_number_or_throw(interp, stack, exec, &value)
                    .map_err(|e| vm_to_native(interp, e, "TypedArray"))
            })?;
            Value::number(number)
        };
        out.push(scope.value(converted));
    }
    Ok(out)
}

/// §7.3.20 LengthOfArrayLike + raw element reads — `Get(source, k)`
/// for each `k < ToLength(Get(source, "length"))`, running getters but
/// **not** numeric-coercing (the caller maps, then converts). Reserves
/// fallibly so a pathological `length` throws `RangeError`.
/// §7.3.3 Get + run an accessor, propagating an abrupt completion.
fn ta_get_via<'scope>(
    scope: &mut NativeScope<'scope, '_>,
    exec: &crate::ExecutionContext,
    source: Local<'scope>,
    key: &crate::VmPropertyKey<'_>,
) -> Result<Local<'scope>, NativeError> {
    let source_raw = scope.raw(source);
    let outcome = scope.with_turn_parts(|interp, stack| {
        interp
            .ordinary_get_value(stack, exec, source_raw, source_raw, key, 0)
            .map_err(|e| vm_to_native(interp, e, "TypedArray"))
    })?;
    match outcome {
        crate::VmGetOutcome::Value(value) => Ok(scope.value(value)),
        crate::VmGetOutcome::InvokeGetter { getter } => {
            let getter = scope.value(getter);
            scope.call(getter, source, &[])
        }
    }
}

/// §23.2.5.1 InitializeTypedArrayFromArrayLike — read an array-like
/// object's `length` (`Get` + `ToLength`) and each element (`Get`,
/// running getters, then `ToNumber` / `ToBigInt`), so user side
/// effects run and a Symbol / cross-numeric element throws. Returns
/// the converted elements; the per-kind dispatcher narrows them to the
/// destination representation on store.
fn read_array_like_coerced<'scope>(
    scope: &mut NativeScope<'scope, '_>,
    exec: &crate::ExecutionContext,
    source: Local<'scope>,
    kind: TypedArrayKind,
) -> Result<Vec<Local<'scope>>, NativeError> {
    let len_value = ta_get_via(scope, exec, source, &crate::VmPropertyKey::String("length"))?;
    let len_value_raw = scope.raw(len_value);
    let len_number = scope.with_turn_parts(|interp, stack| {
        crate::coerce::to_number_or_throw(interp, stack, exec, &len_value_raw)
            .map_err(|e| vm_to_native(interp, e, "TypedArray"))
    })?;
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
    let mut out: Vec<Local<'scope>> = Vec::new();
    if out.try_reserve_exact(len).is_err() {
        return Err(NativeError::RangeError {
            name: "TypedArray",
            reason: "Invalid typed array length".to_string(),
        });
    }
    for i in 0..len {
        let value = ta_get_via(
            scope,
            exec,
            source,
            &crate::VmPropertyKey::OwnedString(i.to_string()),
        )?;
        let value = scope.raw(value);
        let converted = if kind.is_bigint() {
            let big = scope.with_turn_parts(|interp, stack| {
                crate::coerce::to_big_int_or_throw(interp, stack, exec, &value)
                    .map_err(|e| vm_to_native(interp, e, "TypedArray"))
            })?;
            Value::big_int(big)
        } else {
            let number = scope.with_turn_parts(|interp, stack| {
                crate::coerce::to_number_or_throw(interp, stack, exec, &value)
                    .map_err(|e| vm_to_native(interp, e, "TypedArray"))
            })?;
            Value::number(number)
        };
        out.push(scope.value(converted));
    }
    Ok(out)
}

fn ta_proto_for_each(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    ta_callback_dispatch(ctx, args, "forEach")
}

fn ta_proto_map(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    ta_callback_dispatch(ctx, args, "map")
}

fn ta_proto_filter(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    ta_callback_dispatch(ctx, args, "filter")
}

fn ta_proto_find(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    ta_callback_dispatch(ctx, args, "find")
}

fn ta_proto_find_index(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    ta_callback_dispatch(ctx, args, "findIndex")
}

fn ta_proto_find_last(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    ta_callback_dispatch(ctx, args, "findLast")
}

fn ta_proto_find_last_index(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    ta_callback_dispatch(ctx, args, "findLastIndex")
}

fn ta_proto_every(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    ta_callback_dispatch(ctx, args, "every")
}

fn ta_proto_some(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    ta_callback_dispatch(ctx, args, "some")
}

fn ta_proto_reduce(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    ta_callback_dispatch(ctx, args, "reduce")
}

fn ta_proto_reduce_right(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    ta_callback_dispatch(ctx, args, "reduceRight")
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
    // §23.2.3.3 — IsTypedArrayOutOfBounds → +0; otherwise [[ByteOffset]].
    let n = if t.is_out_of_bounds(ctx.heap()) {
        0
    } else {
        t.byte_offset(ctx.heap())
    };
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
ta_ctor!(ctor_float16, TypedArrayKind::Float16);

/// §23.2.2.1 `%TypedArray%.from(source [, mapfn [, thisArg]])` —
/// generic over `this`: the receiver is ANY constructor and the
/// result comes from TypedArrayCreate(this, len), so subclasses and
/// custom constructors observe spec ordering (create AFTER the
/// source is materialized, element writes through detach-safe
/// IntegerIndexedElementSet).
fn ta_from(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let name = "TypedArray.from";
    let exec = ctx
        .execution_context()
        .cloned()
        .ok_or_else(|| NativeError::TypeError {
            name,
            reason: "missing execution context".to_string(),
        })?;
    ctx.scope(|mut scope| {
        let receiver = scope.this();
        let receiver_value = scope.raw(receiver);
        if !crate::abstract_ops::is_constructor(&receiver_value, &exec, scope.context().heap()) {
            return Err(NativeError::TypeError {
                name,
                reason: "this is not a constructor".to_string(),
            });
        }
        let source = scope.argument(args, 0);
        let mapfn = scope.argument(args, 1);
        let mapping = !scope.is_undefined(mapfn);
        if mapping && !scope.is_callable(mapfn) {
            return Err(NativeError::TypeError {
                name,
                reason: "mapfn is not a function".to_string(),
            });
        }
        let this_arg = scope.argument(args, 2);
        if scope.is_null(source) || scope.is_undefined(source) {
            return Err(NativeError::TypeError {
                name,
                reason: "cannot create a TypedArray from null or undefined".to_string(),
            });
        }
        // §7.3.10 GetMethod(source, @@iterator) — non-callable,
        // non-nullish answers throw before anything else runs.
        let iter_sym = scope
            .context()
            .interp_mut()
            .well_known_symbols()
            .get(crate::symbol::WellKnown::Iterator);
        let iter_method = ta_get_via(
            &mut scope,
            &exec,
            source,
            &crate::VmPropertyKey::Symbol(iter_sym),
        )?;
        let use_iterator = if scope.is_undefined(iter_method) || scope.is_null(iter_method) {
            false
        } else if scope.is_callable(iter_method) {
            true
        } else {
            return Err(NativeError::TypeError {
                name,
                reason: "@@iterator is not callable".to_string(),
            });
        };
        if use_iterator {
            // §23.2.2.1 step 6 — IteratorToList first, THEN create.
            let values = drain_iterable_into_values(&mut scope, &exec, source, iter_method)?;
            let target = ta_create_from_constructor(&mut scope, receiver, values.len(), name)?;
            for (k, value) in values.into_iter().enumerate() {
                ta_from_store(
                    &mut scope, &exec, target, k, value, mapping, mapfn, this_arg, name,
                )?;
            }
            return Ok(scope.finish(target));
        }
        // §23.2.2.1 step 7 — array-like: LengthOfArrayLike, create,
        // then per-index Get / map / Set in order.
        let len_value = ta_get_via(
            &mut scope,
            &exec,
            source,
            &crate::VmPropertyKey::String("length"),
        )?;
        let len_value = scope.raw(len_value);
        let len = scope.with_turn_parts(|interp, stack| {
            crate::coerce::to_length_or_throw(interp, stack, &exec, &len_value)
                .map_err(|e| vm_to_native(interp, e, name))
        })?;
        let target = ta_create_from_constructor(&mut scope, receiver, len, name)?;
        for k in 0..len {
            let value = ta_get_via(
                &mut scope,
                &exec,
                source,
                &crate::VmPropertyKey::OwnedString(k.to_string()),
            )?;
            ta_from_store(
                &mut scope, &exec, target, k, value, mapping, mapfn, this_arg, name,
            )?;
        }
        Ok(scope.finish(target))
    })
}

/// §23.2.2.2 `%TypedArray%.of(...items)` — generic over `this`.
fn ta_of(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let name = "TypedArray.of";
    let exec = ctx
        .execution_context()
        .cloned()
        .ok_or_else(|| NativeError::TypeError {
            name,
            reason: "missing execution context".to_string(),
        })?;
    ctx.scope(|mut scope| {
        let receiver = scope.this();
        let receiver_value = scope.raw(receiver);
        if !crate::abstract_ops::is_constructor(&receiver_value, &exec, scope.context().heap()) {
            return Err(NativeError::TypeError {
                name,
                reason: "this is not a constructor".to_string(),
            });
        }
        let values: SmallVec<[Local<'_>; 4]> =
            args.iter().map(|value| scope.value(*value)).collect();
        let target = ta_create_from_constructor(&mut scope, receiver, values.len(), name)?;
        let undefined = scope.undefined();
        for (k, value) in values.into_iter().enumerate() {
            ta_from_store(
                &mut scope, &exec, target, k, value, false, undefined, undefined, name,
            )?;
        }
        Ok(scope.finish(target))
    })
}

/// §23.2.4.2 TypedArrayCreate — `Construct(C, [len])`, then
/// ValidateTypedArray on the result plus the length floor.
fn ta_create_from_constructor<'scope>(
    scope: &mut NativeScope<'scope, '_>,
    ctor: Local<'scope>,
    len: usize,
    name: &'static str,
) -> Result<Local<'scope>, NativeError> {
    let len_arg = scope.value(Value::number(crate::number::NumberValue::from_f64(
        len as f64,
    )));
    let result = scope
        .construct(ctor, &[len_arg])
        .map_err(|error| match error {
            NativeError::TypeError { reason, .. } => NativeError::TypeError { name, reason },
            NativeError::RangeError { reason, .. } => NativeError::RangeError { name, reason },
            other => other,
        })?;
    let Some(target) = scope.raw(result).as_typed_array(scope.context().heap()) else {
        return Err(NativeError::TypeError {
            name,
            reason: "constructor did not return a TypedArray".to_string(),
        });
    };
    if target
        .buffer(scope.context().heap())
        .is_detached(scope.context().heap())
    {
        return Err(NativeError::TypeError {
            name,
            reason: "constructor returned a detached TypedArray".to_string(),
        });
    }
    if target.length(scope.context().heap()) < len {
        return Err(NativeError::TypeError {
            name,
            reason: "constructor returned a TypedArray that is too small".to_string(),
        });
    }
    Ok(result)
}

/// One `from` / `of` element step: optional mapfn call, the
/// target-kind numeric coercion (full user ToNumber / ToBigInt),
/// then the detach-safe §10.4.5.16 IntegerIndexedElementSet write.
#[allow(clippy::too_many_arguments)]
fn ta_from_store<'scope>(
    scope: &mut NativeScope<'scope, '_>,
    exec: &crate::ExecutionContext,
    target: Local<'scope>,
    k: usize,
    value: Local<'scope>,
    mapping: bool,
    mapfn: Local<'scope>,
    this_arg: Local<'scope>,
    name: &'static str,
) -> Result<(), NativeError> {
    let mapped = if mapping {
        let index = scope.value(Value::number(crate::number::NumberValue::from_f64(
            k as f64,
        )));
        scope.call(mapfn, this_arg, &[value, index])?
    } else {
        value
    };
    let target_kind = scope
        .raw(target)
        .as_typed_array(scope.context().heap())
        .ok_or_else(|| NativeError::TypeError {
            name,
            reason: "constructor result is no longer a TypedArray".to_string(),
        })?
        .kind();
    let mapped = scope.raw(mapped);
    let converted = if target_kind.is_bigint() {
        let big = scope.with_turn_parts(|interp, stack| {
            crate::coerce::to_big_int_or_throw(interp, stack, exec, &mapped)
                .map_err(|e| vm_to_native(interp, e, name))
        })?;
        Value::big_int(big)
    } else {
        let number = scope.with_turn_parts(|interp, stack| {
            crate::coerce::to_number_or_throw(interp, stack, exec, &mapped)
                .map_err(|e| vm_to_native(interp, e, name))
        })?;
        Value::number(number)
    };
    let converted = scope.value(converted);
    let target = scope
        .raw(target)
        .as_typed_array(scope.context().heap())
        .ok_or_else(|| NativeError::TypeError {
            name,
            reason: "constructor result is no longer a TypedArray".to_string(),
        })?;
    let converted = scope.raw(converted);
    target.set(scope.context().heap_mut(), k, &converted);
    Ok(())
}
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
    let exec = ctx.execution_context().cloned();
    ctx.scope(|mut scope| {
        let mut rooted_args: SmallVec<[Local<'_>; 4]> =
            args.iter().map(|value| scope.value(*value)).collect();

        // §23.2.5.1 — any Object source other than an ArrayBuffer or a
        // TypedArray initializes from @@iterator / array-like reads. Keep the
        // source, iterator methods, yielded values, and conversions in this
        // one handle range while every observable hook re-enters the VM.
        if let (Some(source), Some(exec)) = (rooted_args.first().copied(), exec.as_ref()) {
            let source_value = scope.raw(source);
            if source_value.is_object_type()
                && !source_value.is_array_buffer()
                && !source_value.is_typed_array()
            {
                let iter_sym = scope
                    .context()
                    .interp_mut()
                    .well_known_symbols()
                    .get(crate::symbol::WellKnown::Iterator);
                let iter_method = ta_get_via(
                    &mut scope,
                    exec,
                    source,
                    &crate::VmPropertyKey::Symbol(iter_sym),
                )?;
                if !(scope.is_undefined(iter_method)
                    || scope.is_null(iter_method)
                    || scope.is_callable(iter_method))
                {
                    return Err(NativeError::TypeError {
                        name: typed_array_name(kind),
                        reason: "@@iterator is not callable".to_string(),
                    });
                }
                let values = if scope.is_callable(iter_method) {
                    let values = drain_iterable_into_values(&mut scope, exec, source, iter_method)?;
                    coerce_values_for_kind(&mut scope, exec, values, kind)?
                } else {
                    read_array_like_coerced(&mut scope, exec, source, kind)?
                };
                let values: SmallVec<[Value; 4]> =
                    values.iter().map(|value| scope.raw(*value)).collect();
                let values_slice = values.as_slice();
                let mut external_visit = |visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
                    for value in values_slice {
                        value.trace_value_slots(visitor);
                    }
                };
                let value = dispatch::typed_array_from_values_with_roots(
                    kind,
                    values_slice,
                    scope.context().interp_mut(),
                    &mut external_visit,
                )
                .map_err(|error| {
                    vm_to_native(scope.context().interp_mut(), error, typed_array_name(kind))
                })?;
                let value = scope.value(value);
                let value = apply_typed_array_new_target_proto(&mut scope, kind, value)?;
                return Ok(scope.finish(value));
            }
        }

        // §22.2.4.5 `TypedArray(buffer [, byteOffset [, length]])` —
        // pre-coerce object offsets through ToPrimitive(Number). Replacing a
        // vector lane swaps Local identities; the old arena slot remains
        // harmless and the new one is collector-rewritten in place.
        if rooted_args
            .first()
            .is_some_and(|value| scope.raw(*value).is_array_buffer())
            && let Some(exec) = exec.as_ref()
        {
            for idx in 1..=2 {
                let Some(value) = rooted_args.get(idx).copied() else {
                    continue;
                };
                let value = scope.raw(value);
                if !value.is_object_type() {
                    continue;
                }
                let primitive = scope.with_turn_parts(|interp, stack| {
                    interp
                        .evaluate_to_primitive(
                            stack,
                            exec,
                            &value,
                            crate::abstract_ops::ToPrimitiveHint::Number,
                        )
                        .map_err(|error| vm_to_native(interp, error, typed_array_name(kind)))
                })?;
                rooted_args[idx] = scope.value(primitive);
            }
        }

        let coerced: SmallVec<[Value; 4]> =
            rooted_args.iter().map(|value| scope.raw(*value)).collect();
        let coerced_slice = coerced.as_slice();
        // §23.2.5.1 step 6.b ToIndex(length) — a negative or infinite
        // numeric length throws RangeError before allocation.
        if let Some(first) = coerced_slice.first()
            && !first.is_object_type()
            && let Some(number) = first.as_number()
        {
            let value = number.as_f64();
            let integer = if value.is_nan() { 0.0 } else { value.trunc() };
            if integer < 0.0 || integer.is_infinite() {
                return Err(NativeError::RangeError {
                    name: typed_array_name(kind),
                    reason: "Invalid typed array length".to_string(),
                });
            }
        }
        let mut external_visit = |visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
            for value in coerced_slice {
                value.trace_value_slots(visitor);
            }
        };
        let value = dispatch::typed_array_call_with_roots(
            kind,
            TypedArrayMethod::Construct,
            coerced_slice,
            scope.context().interp_mut(),
            &mut external_visit,
        )
        .map_err(|error| {
            vm_to_native(scope.context().interp_mut(), error, typed_array_name(kind))
        })?;
        let value = scope.value(value);
        // §10.1.13 GetPrototypeFromConstructor — derived `super()`
        // construction forwards `new.target`.
        let value = apply_typed_array_new_target_proto(&mut scope, kind, value)?;
        Ok(scope.finish(value))
    })
}

fn apply_typed_array_new_target_proto<'scope>(
    scope: &mut NativeScope<'scope, '_>,
    kind: TypedArrayKind,
    value: Local<'scope>,
) -> Result<Local<'scope>, NativeError> {
    let needs_proto_override = !scope
        .context()
        .new_target()
        .is_some_and(|target| target.is_native_function());
    if needs_proto_override
        && let Some(proto) =
            crate::bootstrap::native_new_target_prototype(scope.context(), typed_array_name(kind))?
    {
        let current = scope.raw(value);
        scope
            .context()
            .interp_mut()
            .set_non_gc_exotic_prototype_override(&current, Some(proto));
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
        TypedArrayKind::Float16 => "Float16Array",
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
    ctx.scope(|mut scope| {
        let receiver = scope.this();
        let mut arg_handles: SmallVec<[crate::Local<'_>; 4]> =
            args.iter().map(|value| scope.value(*value)).collect();

        // Some relative-index operands are coerced in the wrapper before
        // calling shared implementations. Methods whose spec first reads
        // TypedArray length, such as `fill`, `copyWithin`, `includes`,
        // `indexOf`, and `lastIndexOf`, do their coercions inside the impl
        // so user side effects cannot pre-empt the length snapshot.
        let int_coerce: &[usize] = match method_name {
            // §23.2.3.1 / §23.2.3.36 / §23.2.3.27/.28 — relative-index
            // operands run ToIntegerOrInfinity (firing valueOf /
            // toString) before the impl reads them as numbers.
            // `at` coerces its index inside impl_at AFTER ValidateTypedArray
            // so a resize during ToIntegerOrInfinity is observed in the
            // correct order (it must not throw, only read out of range).
            "with" => &[0],
            "slice" | "subarray" => &[0, 1],
            _ => &[],
        };
        if !int_coerce.is_empty() {
            // §23.2.4.4 ValidateTypedArray runs BEFORE the argument
            // coercions for these methods — a non-TypedArray or detached
            // receiver throws before any user valueOf fires. `subarray`
            // (§23.2.3.30) only requires the internal slot and operates
            // on detached views.
            match scope.raw(receiver).as_typed_array(scope.context().heap()) {
                None => {
                    return Err(NativeError::TypeError {
                        name: NAME,
                        reason: "method called on a non-TypedArray receiver".to_string(),
                    });
                }
                Some(t)
                    if method_name != "subarray"
                        && t.buffer(scope.context().heap())
                            .is_detached(scope.context().heap()) =>
                {
                    return Err(NativeError::TypeError {
                        name: NAME,
                        reason: "expected non-detached typedarray".to_string(),
                    });
                }
                Some(_) => {}
            }
            if let Some(context) = scope.context().execution_context().cloned() {
                for &idx in int_coerce {
                    let Some(value_handle) = arg_handles.get(idx).copied() else {
                        continue;
                    };
                    let value = scope.raw(value_handle);
                    if value.is_number() || value.is_undefined() {
                        continue;
                    }
                    let number = scope.with_turn_parts(|interp, stack| {
                        interp
                            .coerce_to_number(stack, &context, &value)
                            .map_err(|error| {
                                crate::native_function::vm_to_native_error(interp, error, NAME)
                            })
                    })?;
                    arg_handles[idx] = scope.value(Value::number(number));
                }
            }
        }

        let current_args: SmallVec<[Value; 4]> =
            arg_handles.iter().map(|value| scope.raw(*value)).collect();
        let result = impl_fn(scope.context(), &current_args)?;
        let result = scope.value(result);
        Ok(scope.finish(result))
    })
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
    let context = ctx
        .execution_context()
        .cloned()
        .ok_or_else(|| NativeError::TypeError {
            name: method_name,
            reason: "missing execution context".to_string(),
        })?;
    let result = ctx.with_turn_parts(|interp, stack| {
        if method_name == "slice" {
            interp.typed_array_slice_value_dispatch(stack, &context, &t, args)
        } else {
            interp.typed_array_subarray_value_dispatch(stack, &context, &t, args)
        }
    });
    result.map_err(|err| {
        crate::native_function::vm_to_native_error(ctx.interp_mut(), err, method_name)
    })
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
    let context = ctx
        .execution_context()
        .cloned()
        .ok_or_else(|| NativeError::TypeError {
            name: method_name,
            reason: "missing execution context".to_string(),
        })?;
    ctx.with_turn_parts(|interp, stack| {
        interp
            .typed_array_callback_value_dispatch(stack, &context, &t, method_name, args)
            .map_err(|err| crate::native_function::vm_to_native_error(interp, err, method_name))
    })
}

// ---------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------

fn vm_to_native(interp: &crate::Interpreter, err: VmError, name: &'static str) -> NativeError {
    // Delegate to the canonical mapping so a thrown JS exception
    // (`VmError::Uncaught`) keeps its identity as `NativeError::Thrown`
    // rather than collapsing into a generic TypeError — array-like
    // `length` getters / element `valueOf` hooks re-throw user errors
    // (Test262Error, RangeError, …) that must propagate unchanged.
    crate::native_function::vm_to_native_error(interp, err, name)
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
    statics = {
        "from" / 1 => ta_from,
        "of"   / 0 => ta_of,
    },
    prototype = {
        methods = {
            "at"             / 1 => ta_at,
            "subarray"       / 2 => ta_subarray,
            "slice"          / 2 => ta_slice,
            "fill"           / 1 => ta_fill,
            "copyWithin"     / 2 => ta_copy_within,
            "reverse"        / 0 => ta_reverse,
            "indexOf"        / 1 => ta_index_of,
            "lastIndexOf"    / 1 => ta_last_index_of,
            "includes"       / 1 => ta_includes,
            "join"           / 1 => ta_join,
            "toString"       / 0 => ta_to_string,
            "toLocaleString" / 0 => ta_to_locale_string,
            "set"            / 1 => ta_set,
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
    ($name:literal, $intrinsic:ident, $bpe:expr, $ctor:ident) => {
        otter_macros::couch! {
            name = $name,
            feature = CORE,
            intrinsic = $intrinsic,
            constructor = (length = 3, call = $ctor),
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

typed_array_kind!("Int8Array", Int8ArrayIntrinsic, 1.0, ctor_int8);
typed_array_kind!("Uint8Array", Uint8ArrayIntrinsic, 1.0, ctor_uint8);
typed_array_kind!(
    "Uint8ClampedArray",
    Uint8ClampedArrayIntrinsic,
    1.0,
    ctor_uint8_clamped
);
typed_array_kind!("Int16Array", Int16ArrayIntrinsic, 2.0, ctor_int16);
typed_array_kind!("Uint16Array", Uint16ArrayIntrinsic, 2.0, ctor_uint16);
typed_array_kind!("Int32Array", Int32ArrayIntrinsic, 4.0, ctor_int32);
typed_array_kind!("Uint32Array", Uint32ArrayIntrinsic, 4.0, ctor_uint32);
typed_array_kind!("Float16Array", Float16ArrayIntrinsic, 2.0, ctor_float16);
typed_array_kind!("Float32Array", Float32ArrayIntrinsic, 4.0, ctor_float32);
typed_array_kind!("Float64Array", Float64ArrayIntrinsic, 8.0, ctor_float64);
typed_array_kind!("BigInt64Array", BigInt64ArrayIntrinsic, 8.0, ctor_bigint64);
typed_array_kind!(
    "BigUint64Array",
    BigUint64ArrayIntrinsic,
    8.0,
    ctor_biguint64
);
