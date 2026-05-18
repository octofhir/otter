//! ECMA-262 §23.2 TypedArray bootstrap installer.
//!
//! Installs the 11 concrete TypedArray constructors plus a shared
//! abstract `%TypedArray%.prototype` that all per-kind prototypes
//! inherit from. Each per-kind prototype carries
//! `BYTES_PER_ELEMENT`, `constructor`, and `@@toStringTag` —
//! `Uint8Array.prototype[@@toStringTag] === "Uint8Array"`. The
//! 20+ shared prototype methods (`at`, `subarray`, `slice`, …)
//! delegate to the existing
//! [`crate::binary::typed_array_prototype`] intrinsic table.
//!
//! The intrinsic table fast path at `Op::CallMethod` continues to
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
use crate::bootstrap::{alloc_object_with_value_roots, native_constructor_static_with_value_roots};
use crate::intrinsics::IntrinsicArgs;
use crate::js_surface::{Attr, JsSurfaceError, ObjectBuilder};
use crate::native_function::NativeCall;
use crate::number::NumberValue;
use crate::object::{self, JsObject, PartialPropertyDescriptor, PropertyDescriptor};
use crate::{NativeCtx, NativeError, Value, VmError};

const TYPED_ARRAY_METHODS: &[(&str, u8, crate::native_function::NativeFastFn)] = &[
    ("at", 1, ta_at),
    ("subarray", 2, ta_subarray),
    ("slice", 2, ta_slice),
    ("fill", 3, ta_fill),
    ("copyWithin", 3, ta_copy_within),
    ("reverse", 0, ta_reverse),
    ("indexOf", 2, ta_index_of),
    ("lastIndexOf", 2, ta_last_index_of),
    ("includes", 2, ta_includes),
    ("join", 1, ta_join),
    ("toString", 0, ta_to_string),
    ("toLocaleString", 0, ta_to_locale_string),
    ("set", 2, ta_set),
    ("toReversed", 0, ta_to_reversed),
    ("toSorted", 1, ta_to_sorted),
    ("sort", 1, ta_sort),
    ("with", 2, ta_with),
    ("keys", 0, ta_keys),
    ("values", 0, ta_values),
    ("entries", 0, ta_entries),
];

/// Per-kind `<TA>.from` / `<TA>.of` static-method routing table.
/// Installed on each concrete TypedArray constructor after its
/// prototype is wired so the §23.2.6 inherited statics resolve via
/// ordinary property lookup (and not only the call-method
/// intrinsic dispatch path).
const TYPED_ARRAY_STATICS: &[(
    &str,
    crate::native_function::NativeFastFn,
    crate::native_function::NativeFastFn,
)] = &[
    ("Int8Array", from_int8, of_int8),
    ("Uint8Array", from_uint8, of_uint8),
    ("Uint8ClampedArray", from_uint8_clamped, of_uint8_clamped),
    ("Int16Array", from_int16, of_int16),
    ("Uint16Array", from_uint16, of_uint16),
    ("Int32Array", from_int32, of_int32),
    ("Uint32Array", from_uint32, of_uint32),
    ("Float32Array", from_float32, of_float32),
    ("Float64Array", from_float64, of_float64),
    ("BigInt64Array", from_bigint64, of_bigint64),
    ("BigUint64Array", from_biguint64, of_biguint64),
];

/// 11 concrete kinds × (name, length, ctor fn pointer).
const TYPED_ARRAY_CTORS: &[(&str, TypedArrayKind, crate::native_function::NativeFastFn)] = &[
    ("Int8Array", TypedArrayKind::Int8, ctor_int8),
    ("Uint8Array", TypedArrayKind::Uint8, ctor_uint8),
    (
        "Uint8ClampedArray",
        TypedArrayKind::Uint8Clamped,
        ctor_uint8_clamped,
    ),
    ("Int16Array", TypedArrayKind::Int16, ctor_int16),
    ("Uint16Array", TypedArrayKind::Uint16, ctor_uint16),
    ("Int32Array", TypedArrayKind::Int32, ctor_int32),
    ("Uint32Array", TypedArrayKind::Uint32, ctor_uint32),
    ("Float32Array", TypedArrayKind::Float32, ctor_float32),
    ("Float64Array", TypedArrayKind::Float64, ctor_float64),
    ("BigInt64Array", TypedArrayKind::BigInt64, ctor_bigint64),
    ("BigUint64Array", TypedArrayKind::BigUint64, ctor_biguint64),
];

/// Entry point invoked by the per-kind `BuiltinIntrinsic` adapter.
/// Looks up the per-kind ctor for `name` from the static table and
/// routes to the shared install path.
pub(crate) fn install_typed_array_entry(
    name: &'static str,
    heap: &mut otter_gc::GcHeap,
    global: JsObject,
) -> Result<(), JsSurfaceError> {
    let global_root = Value::Object(global);
    // Look up this entry's kind + ctor fn from the static table.
    let (_, kind, ctor_call) = TYPED_ARRAY_CTORS
        .iter()
        .find(|(entry_name, _, _)| *entry_name == name)
        .copied()
        .expect("name must match TYPED_ARRAY_CTORS");

    // Ensure %TypedArray%.prototype exists on the realm (allocated
    // lazily the first time we install a concrete TypedArray).
    let abstract_proto = ensure_abstract_typed_array_prototype(heap, global)?;
    let abstract_proto_root = Value::Object(abstract_proto);

    // Per-kind prototype linked to the abstract.
    let prototype = alloc_object_with_value_roots(heap, &[&global_root, &abstract_proto_root])?;
    let prototype_root = Value::Object(prototype);
    object::set_prototype(prototype, heap, Some(abstract_proto));

    // BYTES_PER_ELEMENT (read-only) on the per-kind prototype.
    let bpe = kind.bytes_per_element() as i32;
    object::define_own_property(
        prototype,
        heap,
        "BYTES_PER_ELEMENT",
        PropertyDescriptor::data(
            Value::Number(NumberValue::from_i32(bpe)),
            false,
            false,
            false,
        ),
    );

    let ctor = native_constructor_static_with_value_roots(
        heap,
        name,
        3,
        ctor_call,
        &[&global_root, &abstract_proto_root, &prototype_root],
    )
    .map_err(|_| JsSurfaceError::OutOfMemory)?;
    let ctor_root = Value::NativeFunction(ctor);
    let string_heap = crate::string::StringHeap::default();
    let proto_desc = PropertyDescriptor::data(Value::Object(prototype), false, false, false);
    if !ctor.define_own_property(heap, &string_heap, "prototype", proto_desc) {
        return Err(JsSurfaceError::DefinePropertyFailed("prototype"));
    }
    // §23.2.6.1: each concrete TypedArray constructor inherits from
    // %TypedArray%. The override is consulted by
    // `Object.getPrototypeOf` / `__proto__` walks.
    let abstract_ctor = ensure_abstract_typed_array_constructor(heap, global)?;
    ctor.set_prototype_override(heap, Some(Value::NativeFunction(abstract_ctor)));
    // Also expose BYTES_PER_ELEMENT on the constructor (§23.2.6.1).
    let bpe_desc = PropertyDescriptor::data(
        Value::Number(NumberValue::from_i32(bpe)),
        false,
        false,
        false,
    );
    let _ = ctor.define_own_property(heap, &string_heap, "BYTES_PER_ELEMENT", bpe_desc);

    // §23.2.2.1 / §23.2.2.2 — per-kind `from` / `of` static methods.
    // Installed as own properties so reflective access
    // (`Int8Array.from`, `Int8Array.of`) returns a callable.
    if let Some((_, from_fn, of_fn)) = TYPED_ARRAY_STATICS
        .iter()
        .copied()
        .find(|(n, _, _)| *n == name)
    {
        let abstract_ctor_value = Value::NativeFunction(abstract_ctor);
        let from_native = crate::bootstrap::native_static_with_value_roots(
            heap,
            "from",
            1,
            from_fn,
            &[&ctor_root, &abstract_ctor_value],
        )
        .map_err(|_| JsSurfaceError::OutOfMemory)?;
        let _ = ctor.define_own_property(
            heap,
            &string_heap,
            "from",
            PropertyDescriptor::data(Value::NativeFunction(from_native), true, false, true),
        );
        let of_native = crate::bootstrap::native_static_with_value_roots(
            heap,
            "of",
            0,
            of_fn,
            &[&ctor_root, &abstract_ctor_value],
        )
        .map_err(|_| JsSurfaceError::OutOfMemory)?;
        let _ = ctor.define_own_property(
            heap,
            &string_heap,
            "of",
            PropertyDescriptor::data(Value::NativeFunction(of_native), true, false, true),
        );
    }

    object::define_own_property(
        prototype,
        heap,
        "constructor",
        PropertyDescriptor::data(ctor_root.clone(), true, false, true),
    );

    crate::bootstrap::define_global_value(global, heap, name, ctor_root);
    Ok(())
}

/// Install `@@toStringTag` on each per-kind prototype after the
/// well-known symbol table exists. Also installs `@@iterator =
/// values` on the abstract `%TypedArray%.prototype`.
pub fn install_typed_array_well_knowns_post_bootstrap(
    heap: &mut otter_gc::GcHeap,
    string_heap: &crate::string::StringHeap,
    global: JsObject,
    well_known: &crate::symbol::WellKnownSymbols,
) -> Result<(), JsSurfaceError> {
    use crate::symbol::WellKnown;

    let tag_sym = well_known.get(WellKnown::ToStringTag);
    for (ctor_name, _kind, _call) in TYPED_ARRAY_CTORS {
        let Some(prototype) = ctor_prototype(global, heap, string_heap, ctor_name) else {
            continue;
        };
        let tag = crate::string::JsString::from_str(ctor_name, string_heap)
            .map_err(|_| JsSurfaceError::OutOfMemory)?;
        object::define_own_symbol_property_partial(
            prototype,
            heap,
            &tag_sym,
            PartialPropertyDescriptor {
                value: Some(Value::String(tag)),
                writable: Some(false),
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
            &iterator_sym,
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

// ---------------------------------------------------------------
// Abstract %TypedArray%.prototype lifecycle
// ---------------------------------------------------------------

/// Sentinel-named property on `globalThis` that holds
/// `%TypedArray%.prototype`. Hidden by a leading symbol-style
/// `@@` prefix to avoid colliding with any user-visible global.
const ABSTRACT_PROTO_SLOT: &str = "@@%TypedArrayPrototype%";

/// Sentinel slot that holds the abstract `%TypedArray%` constructor
/// function. Spec §23.2.1: this constructor is the [[Prototype]] of
/// every concrete TypedArray constructor (Int8Array, Uint8Array …).
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

fn ensure_abstract_typed_array_constructor(
    heap: &mut otter_gc::GcHeap,
    global: JsObject,
) -> Result<crate::native_function::NativeFunction, JsSurfaceError> {
    if let Some(Value::NativeFunction(nf)) = object::get(global, heap, ABSTRACT_CTOR_SLOT) {
        return Ok(nf);
    }
    let abstract_proto = ensure_abstract_typed_array_prototype(heap, global)?;
    let global_root = Value::Object(global);
    let abstract_proto_root = Value::Object(abstract_proto);
    let ctor = native_constructor_static_with_value_roots(
        heap,
        "TypedArray",
        0,
        abstract_typed_array_call,
        &[&global_root, &abstract_proto_root],
    )
    .map_err(|_| JsSurfaceError::OutOfMemory)?;
    let string_heap = crate::string::StringHeap::default();
    // §23.2.2.3 %TypedArray%.prototype is non-writable,
    // non-enumerable, non-configurable.
    let proto_desc =
        PropertyDescriptor::data(Value::Object(abstract_proto), false, false, false);
    if !ctor.define_own_property(heap, &string_heap, "prototype", proto_desc) {
        return Err(JsSurfaceError::DefinePropertyFailed("prototype"));
    }
    // §23.2.3.2 %TypedArray%.prototype.constructor — writable,
    // non-enumerable, configurable.
    object::define_own_property(
        abstract_proto,
        heap,
        "constructor",
        PropertyDescriptor::data(Value::NativeFunction(ctor), true, false, true),
    );
    // Hide the abstract ctor under a non-enumerable global slot.
    object::define_own_property(
        global,
        heap,
        ABSTRACT_CTOR_SLOT,
        PropertyDescriptor::data(Value::NativeFunction(ctor), false, false, false),
    );
    Ok(ctor)
}

fn ensure_abstract_typed_array_prototype(
    heap: &mut otter_gc::GcHeap,
    global: JsObject,
) -> Result<JsObject, JsSurfaceError> {
    if let Some(Value::Object(obj)) = object::get(global, heap, ABSTRACT_PROTO_SLOT) {
        return Ok(obj);
    }
    let global_root = Value::Object(global);
    let proto = alloc_object_with_value_roots(heap, &[&global_root])?;
    // Chain to %Object.prototype% per §23.2.3.
    if let Some(Value::Object(object_ctor)) = object::get(global, heap, "Object")
        && let Some(Value::Object(object_proto)) = object::get(object_ctor, heap, "prototype")
    {
        object::set_prototype(proto, heap, Some(object_proto));
    }
    {
        let mut builder =
            ObjectBuilder::from_object_with_value_roots(heap, proto, vec![global_root.clone()]);
        for (name, length, call) in TYPED_ARRAY_METHODS {
            builder.method(
                name,
                *length,
                NativeCall::Static(*call),
                Attr::builtin_function(),
            )?;
        }
    }
    // Hide the slot itself from enumeration.
    object::define_own_property(
        global,
        heap,
        ABSTRACT_PROTO_SLOT,
        PropertyDescriptor::data(Value::Object(proto), false, false, false),
    );
    Ok(proto)
}

fn get_abstract_typed_array_prototype(
    global: JsObject,
    heap: &otter_gc::GcHeap,
) -> Option<JsObject> {
    match object::get(global, heap, ABSTRACT_PROTO_SLOT) {
        Some(Value::Object(obj)) => Some(obj),
        _ => None,
    }
}

fn ctor_prototype(
    global: JsObject,
    heap: &otter_gc::GcHeap,
    string_heap: &crate::string::StringHeap,
    ctor_name: &str,
) -> Option<JsObject> {
    let Some(Value::NativeFunction(f)) = object::get(global, heap, ctor_name) else {
        return None;
    };
    let descriptor = f
        .own_property_descriptor(heap, string_heap, "prototype")
        .ok()
        .flatten()?;
    match descriptor.kind {
        crate::object::DescriptorKind::Data {
            value: Value::Object(p),
        } => Some(p),
        _ => None,
    }
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
    let roots = ctx.collect_native_roots();
    let this_value = ctx.this_value().clone();
    let mut external_visit = |visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
        crate::runtime_cx::visit_native_roots(visitor, &roots, &this_value, None, &[], &[args]);
    };
    dispatch::typed_array_call_with_roots(
        kind,
        TypedArrayMethod::From,
        args,
        ctx.heap_mut(),
        &mut external_visit,
    )
    .map_err(|e| vm_to_native(e, typed_array_name(kind)))
}

fn ta_of_dispatch(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    kind: TypedArrayKind,
) -> Result<Value, NativeError> {
    let roots = ctx.collect_native_roots();
    let this_value = ctx.this_value().clone();
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
    let coerced: SmallVec<[Value; 4]> = if matches!(args.first(), Some(Value::ArrayBuffer(_))) {
        if let Some(exec) = &exec {
            let mut out: SmallVec<[Value; 4]> = args.iter().cloned().collect();
            for idx in 1..=2 {
                let Some(slot) = out.get_mut(idx) else {
                    continue;
                };
                if !matches!(
                    slot,
                    Value::Object(_)
                        | Value::Array(_)
                        | Value::Function { .. }
                        | Value::Closure { .. }
                        | Value::NativeFunction(_)
                        | Value::BoundFunction(_)
                        | Value::ClassConstructor(_)
                        | Value::Proxy(_)
                        | Value::RegExp(_)
                ) {
                    continue;
                }
                let interp = ctx.interp_mut();
                let primitive = interp
                    .evaluate_to_primitive(
                        exec,
                        slot,
                        crate::abstract_ops::ToPrimitiveHint::Number,
                    )
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
    let this_value = ctx.this_value().clone();
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
    dispatch::typed_array_call_with_roots(
        kind,
        TypedArrayMethod::Construct,
        coerced_slice,
        ctx.heap_mut(),
        &mut external_visit,
    )
    .map_err(|e| vm_to_native(e, typed_array_name(kind)))
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
// Prototype method wrappers — all delegate to the intrinsic table
// ---------------------------------------------------------------

macro_rules! ta_proto_method {
    ($name:ident, $method:expr) => {
        fn $name(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
            ta_proto_dispatch(ctx, args, $method)
        }
    };
}

ta_proto_method!(ta_at, "at");
ta_proto_method!(ta_subarray, "subarray");
ta_proto_method!(ta_slice, "slice");
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
    let entry =
        typed_array_prototype::lookup(method_name).ok_or_else(|| NativeError::TypeError {
            name: "TypedArray.prototype",
            reason: format!("method {method_name} missing"),
        })?;
    let receiver = ctx.this_value().clone();
    let small_args: SmallVec<[Value; 4]> = args.iter().cloned().collect();
    let (string_heap, allocation_roots) = {
        let interp = ctx.interp_mut();
        (interp.string_heap_clone(), interp.collect_runtime_roots())
    };
    let gc_heap = ctx.heap_mut();
    let mut intrinsic_args = IntrinsicArgs {
        receiver: &receiver,
        args: &small_args,
        string_heap: &string_heap,
        gc_heap,
        allocation_roots: allocation_roots.as_slice(),
    };
    (entry.impl_fn)(&mut intrinsic_args).map_err(|e| NativeError::TypeError {
        name: "TypedArray.prototype",
        reason: e.to_string(),
    })
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
// BuiltinIntrinsic adapters — one zero-sized struct per TypedArray
// variant.
// ---------------------------------------------------------------

/// Generate per-kind `BuiltinIntrinsic` adapter types. Each ZST
/// pins its JS name and dispatches through
/// [`install_typed_array_entry`].
macro_rules! typed_array_intrinsic {
    ($($ty:ident => $name:literal),* $(,)?) => {
        $(
            #[doc = concat!("`BuiltinIntrinsic` adapter for the `", $name, "` constructor.")]
            pub struct $ty;
            impl crate::intrinsic_install::BuiltinIntrinsic for $ty {
                const NAME: &'static str = $name;
                const FEATURE: crate::bootstrap::BootstrapFeatures =
                    crate::bootstrap::BootstrapFeatures::CORE;
                fn install(
                    heap: &mut otter_gc::GcHeap,
                    global: JsObject,
                ) -> Result<(), JsSurfaceError> {
                    install_typed_array_entry(Self::NAME, heap, global)
                }
            }
        )*
    };
}

typed_array_intrinsic!(
    Int8ArrayIntrinsic         => "Int8Array",
    Uint8ArrayIntrinsic        => "Uint8Array",
    Uint8ClampedArrayIntrinsic => "Uint8ClampedArray",
    Int16ArrayIntrinsic        => "Int16Array",
    Uint16ArrayIntrinsic       => "Uint16Array",
    Int32ArrayIntrinsic        => "Int32Array",
    Uint32ArrayIntrinsic       => "Uint32Array",
    Float32ArrayIntrinsic      => "Float32Array",
    Float64ArrayIntrinsic      => "Float64Array",
    BigInt64ArrayIntrinsic     => "BigInt64Array",
    BigUint64ArrayIntrinsic    => "BigUint64Array",
);
