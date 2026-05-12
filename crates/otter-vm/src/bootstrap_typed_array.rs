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
//!   [`crate::binary::dispatch::typed_array_call`] with the
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

use std::cell::RefCell;

use otter_bytecode::method_id::TypedArrayMethod;
use smallvec::SmallVec;

use crate::binary::typed_array::TypedArrayKind;
use crate::binary::{dispatch, typed_array_prototype};
use crate::bootstrap::BootstrapEntry;
use crate::intrinsics::IntrinsicArgs;
use crate::js_surface::{Attr, JsSurfaceError, ObjectBuilder};
use crate::native_function::{NativeCall, NativeFunction};
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

/// 11 concrete kinds × (name, length, ctor fn pointer).
const TYPED_ARRAY_CTORS: &[(&str, TypedArrayKind, crate::native_function::NativeFastFn)] = &[
    ("Int8Array", TypedArrayKind::Int8, ctor_int8),
    ("Uint8Array", TypedArrayKind::Uint8, ctor_uint8),
    ("Uint8ClampedArray", TypedArrayKind::Uint8Clamped, ctor_uint8_clamped),
    ("Int16Array", TypedArrayKind::Int16, ctor_int16),
    ("Uint16Array", TypedArrayKind::Uint16, ctor_uint16),
    ("Int32Array", TypedArrayKind::Int32, ctor_int32),
    ("Uint32Array", TypedArrayKind::Uint32, ctor_uint32),
    ("Float32Array", TypedArrayKind::Float32, ctor_float32),
    ("Float64Array", TypedArrayKind::Float64, ctor_float64),
    ("BigInt64Array", TypedArrayKind::BigInt64, ctor_bigint64),
    ("BigUint64Array", TypedArrayKind::BigUint64, ctor_biguint64),
];

/// Entry point used by [`crate::bootstrap::BOOTSTRAP_ENTRIES`].
/// Looks up the per-kind ctor for `entry.name` from the static
/// table and routes to the shared install path.
pub(crate) fn install_typed_array_entry(
    entry: &BootstrapEntry,
    heap: &mut otter_gc::GcHeap,
    global: JsObject,
) -> Result<(), JsSurfaceError> {
    // Look up this entry's kind + ctor fn from the static table.
    let (_, kind, ctor_call) = TYPED_ARRAY_CTORS
        .iter()
        .find(|(name, _, _)| *name == entry.name)
        .copied()
        .expect("entry name must match TYPED_ARRAY_CTORS");

    // Ensure %TypedArray%.prototype exists on the realm (allocated
    // lazily the first time we install a concrete TypedArray).
    let abstract_proto = ensure_abstract_typed_array_prototype(heap, global)?;

    // Per-kind prototype linked to the abstract.
    let prototype = object::alloc_object(heap)?;
    object::set_prototype(prototype, heap, Some(abstract_proto));

    // BYTES_PER_ELEMENT (read-only) on the per-kind prototype.
    let bpe = kind.bytes_per_element() as i32;
    object::define_own_property(
        prototype,
        heap,
        "BYTES_PER_ELEMENT",
        PropertyDescriptor::data(Value::Number(NumberValue::from_i32(bpe)), false, false, false),
    );

    let ctor = NativeFunction::new_constructor_static(heap, entry.name, 3, ctor_call)
        .map_err(|_| JsSurfaceError::OutOfMemory)?;
    let string_heap = crate::string::StringHeap::default();
    let proto_desc = PropertyDescriptor::data(Value::Object(prototype), false, false, false);
    if !ctor.define_own_property(heap, &string_heap, "prototype", proto_desc) {
        return Err(JsSurfaceError::DefinePropertyFailed("prototype"));
    }
    // Also expose BYTES_PER_ELEMENT on the constructor (§23.2.6.1).
    let bpe_desc = PropertyDescriptor::data(
        Value::Number(NumberValue::from_i32(bpe)),
        false,
        false,
        false,
    );
    let _ = ctor.define_own_property(heap, &string_heap, "BYTES_PER_ELEMENT", bpe_desc);

    object::define_own_property(
        prototype,
        heap,
        "constructor",
        PropertyDescriptor::data(Value::NativeFunction(ctor), true, false, true),
    );

    crate::bootstrap::define_global_value(global, heap, entry.name, Value::NativeFunction(ctor));
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

fn ensure_abstract_typed_array_prototype(
    heap: &mut otter_gc::GcHeap,
    global: JsObject,
) -> Result<JsObject, JsSurfaceError> {
    if let Some(Value::Object(obj)) = object::get(global, heap, ABSTRACT_PROTO_SLOT) {
        return Ok(obj);
    }
    let proto = object::alloc_object(heap)?;
    // Chain to %Object.prototype% per §23.2.3.
    if let Some(Value::Object(object_ctor)) = object::get(global, heap, "Object")
        && let Some(Value::Object(object_proto)) = object::get(object_ctor, heap, "prototype")
    {
        object::set_prototype(proto, heap, Some(object_proto));
    }
    {
        let mut builder = ObjectBuilder::from_object(heap, proto);
        for (name, length, call) in TYPED_ARRAY_METHODS {
            builder.method(name, *length, NativeCall::Static(*call), Attr::builtin_function())?;
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
    dispatch::typed_array_call(kind, TypedArrayMethod::Construct, args, ctx.heap())
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
    let entry = typed_array_prototype::lookup(method_name).ok_or_else(|| {
        NativeError::TypeError {
            name: "TypedArray.prototype",
            reason: format!("method {method_name} missing"),
        }
    })?;
    let receiver = ctx.this_value().clone();
    let small_args: SmallVec<[Value; 4]> = args.iter().cloned().collect();
    let string_heap = ctx.interp_mut().string_heap_clone();
    let gc_heap = ctx.heap_mut();
    let intrinsic_args = IntrinsicArgs {
        receiver: &receiver,
        args: &small_args,
        string_heap: &string_heap,
        gc_heap: RefCell::new(gc_heap),
    };
    (entry.impl_fn)(&intrinsic_args).map_err(|e| NativeError::TypeError {
        name: "TypedArray.prototype",
        reason: e.to_string(),
    })
}

// ---------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------

fn vm_to_native(err: VmError, name: &'static str) -> NativeError {
    match err {
        VmError::TypeError { message } => NativeError::TypeError { name, reason: message },
        VmError::TypeMismatch => NativeError::TypeError {
            name,
            reason: "type mismatch".to_string(),
        },
        VmError::RangeError { message } => NativeError::RangeError { name, reason: message },
        other => NativeError::TypeError {
            name,
            reason: other.to_string(),
        },
    }
}
