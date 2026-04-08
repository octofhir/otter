//! %TypedArray% intrinsic and concrete TypedArray constructors.
//!
//! Spec references:
//! - %TypedArray%: <https://tc39.es/ecma262/#sec-%typedarray%-intrinsic-object>
//! - TypedArray constructors: <https://tc39.es/ecma262/#sec-typedarray-constructors>
//! - TypedArray prototype: <https://tc39.es/ecma262/#sec-properties-of-the-%typedarray%-prototype-object>

use crate::builders::ClassBuilder;
use crate::descriptors::{
    JsClassDescriptor, NativeBindingDescriptor, NativeBindingTarget, NativeFunctionDescriptor,
    VmNativeCallError,
};
use crate::object::{
    HeapValueKind, ObjectHandle, PropertyAttributes, PropertyValue, TypedArrayKind,
};
use crate::value::RegisterValue;

use super::{
    IntrinsicsError, VmIntrinsics, WellKnownSymbol,
    install::{IntrinsicInstallContext, IntrinsicInstaller, install_class_plan},
};

// ── Installer ───────────────────────────────────────────────────────

pub(super) static TYPED_ARRAY_INTRINSIC: TypedArrayIntrinsic = TypedArrayIntrinsic;

pub(super) struct TypedArrayIntrinsic;

impl IntrinsicInstaller for TypedArrayIntrinsic {
    fn init(
        &self,
        intrinsics: &mut VmIntrinsics,
        cx: &mut IntrinsicInstallContext<'_>,
    ) -> Result<(), IntrinsicsError> {
        // ── %TypedArray% base constructor + prototype ──────────────
        let base_desc = typed_array_base_class_descriptor();
        let base_plan = ClassBuilder::from_descriptor(&base_desc)
            .expect("TypedArray base class descriptor should normalize")
            .build();

        if let Some(ctor_desc) = base_plan.constructor() {
            let host_id = cx.native_functions.register(ctor_desc.clone());
            intrinsics.typed_array_base_constructor =
                cx.alloc_intrinsic_host_function(host_id, intrinsics.function_prototype())?;
        }

        install_class_plan(
            intrinsics.typed_array_base_prototype,
            intrinsics.typed_array_base_constructor,
            &base_plan,
            intrinsics.function_prototype,
            cx,
        )?;

        // ── Getters on %TypedArray%.prototype ─────────────────────
        install_getter(
            intrinsics.typed_array_base_prototype,
            "buffer",
            typed_array_get_buffer,
            intrinsics,
            cx,
        )?;
        install_getter(
            intrinsics.typed_array_base_prototype,
            "byteLength",
            typed_array_get_byte_length,
            intrinsics,
            cx,
        )?;
        install_getter(
            intrinsics.typed_array_base_prototype,
            "byteOffset",
            typed_array_get_byte_offset,
            intrinsics,
            cx,
        )?;
        install_getter(
            intrinsics.typed_array_base_prototype,
            "length",
            typed_array_get_length,
            intrinsics,
            cx,
        )?;

        // @@toStringTag — a getter that returns the concrete TypedArray name.
        install_symbol_getter(
            intrinsics.typed_array_base_prototype,
            WellKnownSymbol::ToStringTag,
            typed_array_get_to_string_tag,
            intrinsics,
            cx,
        )?;

        // @@iterator = values
        // Install as a separate method that delegates to values.
        let iter_symbol = cx
            .property_names
            .intern_symbol(WellKnownSymbol::Iterator.stable_id());
        let iter_desc = NativeFunctionDescriptor::method("values", 0, typed_array_values);
        let iter_id = cx.native_functions.register(iter_desc);
        let iter_handle =
            cx.alloc_intrinsic_host_function(iter_id, intrinsics.function_prototype())?;
        cx.heap.define_own_property(
            intrinsics.typed_array_base_prototype,
            iter_symbol,
            PropertyValue::data_with_attrs(
                RegisterValue::from_object_handle(iter_handle.0),
                PropertyAttributes::builtin_method(),
            ),
        )?;

        // ── Static methods on %TypedArray% ────────────────────────
        // %TypedArray%.from and %TypedArray%.of are already in the class plan
        // via stat() bindings.

        // ── %TypedArray%[@@species] ───────────────────────────────
        let species_symbol = cx
            .property_names
            .intern_symbol(WellKnownSymbol::Species.stable_id());
        let species_getter_desc =
            NativeFunctionDescriptor::getter("get [Symbol.species]", typed_array_species);
        let species_id = cx.native_functions.register(species_getter_desc);
        let species_getter =
            cx.alloc_intrinsic_host_function(species_id, intrinsics.function_prototype())?;
        cx.heap.define_accessor(
            intrinsics.typed_array_base_constructor,
            species_symbol,
            Some(species_getter),
            None,
        )?;

        // ── Concrete TypedArray constructors ───────────────────────
        for &kind in TypedArrayKind::all() {
            let (constructor_handle, prototype_handle) =
                intrinsics.typed_array_constructor_prototype(kind);

            // Set up prototype chain: ConcreteTA.prototype -> %TypedArray%.prototype
            cx.heap.set_prototype(
                prototype_handle,
                Some(intrinsics.typed_array_base_prototype),
            )?;
            // ConcreteTA constructor -> %TypedArray% constructor
            cx.heap.set_prototype(
                constructor_handle,
                Some(intrinsics.typed_array_base_constructor),
            )?;

            // Register the concrete constructor native function.
            let ctor_fn = make_concrete_constructor(kind);
            let host_id = cx.native_functions.register(ctor_fn);
            let new_ctor =
                cx.alloc_intrinsic_host_function(host_id, intrinsics.function_prototype())?;
            // new_ctor.__proto__ = %TypedArray% (for Int32Array.from/of lookup)
            cx.heap
                .set_prototype(new_ctor, Some(intrinsics.typed_array_base_constructor))?;
            // Copy the new constructor's function identity into the pre-allocated handle.
            // We can't reassign the handle itself, so we wire prototype + constructor links.
            intrinsics.set_typed_array_constructor(kind, new_ctor);

            // constructor.prototype = prototype_handle
            let prototype_prop = cx.property_names.intern("prototype");
            cx.heap.define_own_property(
                new_ctor,
                prototype_prop,
                PropertyValue::data_with_attrs(
                    RegisterValue::from_object_handle(prototype_handle.0),
                    PropertyAttributes::function_prototype(),
                ),
            )?;

            // prototype.constructor = constructor
            let constructor_prop = cx.property_names.intern("constructor");
            cx.heap.define_own_property(
                prototype_handle,
                constructor_prop,
                PropertyValue::data_with_attrs(
                    RegisterValue::from_object_handle(new_ctor.0),
                    PropertyAttributes::constructor_link(),
                ),
            )?;

            // constructor.BYTES_PER_ELEMENT = kind.element_size()
            let bpe_prop = cx.property_names.intern("BYTES_PER_ELEMENT");
            cx.heap.define_own_property(
                new_ctor,
                bpe_prop,
                PropertyValue::data_with_attrs(
                    RegisterValue::from_i32(kind.element_size() as i32),
                    PropertyAttributes::constant(),
                ),
            )?;

            // prototype.BYTES_PER_ELEMENT = kind.element_size()
            cx.heap.define_own_property(
                prototype_handle,
                bpe_prop,
                PropertyValue::data_with_attrs(
                    RegisterValue::from_i32(kind.element_size() as i32),
                    PropertyAttributes::constant(),
                ),
            )?;

            // constructor.name = kind.constructor_name()
            let name_prop = cx.property_names.intern("name");
            let name_str = cx.heap.alloc_string(kind.constructor_name());
            cx.heap.define_own_property(
                new_ctor,
                name_prop,
                PropertyValue::data_with_attrs(
                    RegisterValue::from_object_handle(name_str.0),
                    PropertyAttributes::function_length(),
                ),
            )?;

            // constructor.length = 3 (all TypedArray constructors have length 3)
            let length_prop = cx.property_names.intern("length");
            cx.heap.define_own_property(
                new_ctor,
                length_prop,
                PropertyValue::data_with_attrs(
                    RegisterValue::from_i32(3),
                    PropertyAttributes::function_length(),
                ),
            )?;
        }

        Ok(())
    }

    fn install_on_global(
        &self,
        intrinsics: &VmIntrinsics,
        cx: &mut IntrinsicInstallContext<'_>,
    ) -> Result<(), IntrinsicsError> {
        // Install each concrete TypedArray constructor on the global object.
        for &kind in TypedArrayKind::all() {
            let (ctor, _) = intrinsics.typed_array_constructor_prototype(kind);
            cx.install_global_value(
                intrinsics,
                kind.constructor_name(),
                RegisterValue::from_object_handle(ctor.0),
            )?;
        }
        Ok(())
    }
}

// ── Helpers ─────────────────────────────────────────────────────────

fn proto(
    name: &str,
    arity: u16,
    f: fn(
        &RegisterValue,
        &[RegisterValue],
        &mut crate::interpreter::RuntimeState,
    ) -> Result<RegisterValue, VmNativeCallError>,
) -> NativeBindingDescriptor {
    NativeBindingDescriptor::new(
        NativeBindingTarget::Prototype,
        NativeFunctionDescriptor::method(name, arity, f),
    )
}

fn stat(
    name: &str,
    arity: u16,
    f: fn(
        &RegisterValue,
        &[RegisterValue],
        &mut crate::interpreter::RuntimeState,
    ) -> Result<RegisterValue, VmNativeCallError>,
) -> NativeBindingDescriptor {
    NativeBindingDescriptor::new(
        NativeBindingTarget::Constructor,
        NativeFunctionDescriptor::method(name, arity, f),
    )
}

fn type_error(
    runtime: &mut crate::interpreter::RuntimeState,
    message: &str,
) -> Result<VmNativeCallError, VmNativeCallError> {
    let error = runtime.alloc_type_error(message).map_err(|error| {
        VmNativeCallError::Internal(format!("TypeError allocation failed: {error}").into())
    })?;
    Ok(VmNativeCallError::Thrown(
        RegisterValue::from_object_handle(error.0),
    ))
}

fn range_error(runtime: &mut crate::interpreter::RuntimeState, message: &str) -> VmNativeCallError {
    let prototype = runtime.intrinsics().range_error_prototype;
    let handle = runtime.alloc_object_with_prototype(Some(prototype));
    let msg = runtime.alloc_string(message);
    let msg_prop = runtime.intern_property_name("message");
    runtime
        .objects_mut()
        .set_property(handle, msg_prop, RegisterValue::from_object_handle(msg.0))
        .ok();
    VmNativeCallError::Thrown(RegisterValue::from_object_handle(handle.0))
}

fn install_getter(
    target: ObjectHandle,
    name: &str,
    callback: fn(
        &RegisterValue,
        &[RegisterValue],
        &mut crate::interpreter::RuntimeState,
    ) -> Result<RegisterValue, VmNativeCallError>,
    intrinsics: &VmIntrinsics,
    cx: &mut IntrinsicInstallContext<'_>,
) -> Result<(), IntrinsicsError> {
    let getter_desc = NativeFunctionDescriptor::getter(name, callback);
    let getter_id = cx.native_functions.register(getter_desc);
    let getter_handle =
        cx.alloc_intrinsic_host_function(getter_id, intrinsics.function_prototype())?;
    let property = cx.property_names.intern(name);
    cx.heap
        .define_accessor(target, property, Some(getter_handle), None)?;
    Ok(())
}

fn install_symbol_getter(
    target: ObjectHandle,
    symbol: WellKnownSymbol,
    callback: fn(
        &RegisterValue,
        &[RegisterValue],
        &mut crate::interpreter::RuntimeState,
    ) -> Result<RegisterValue, VmNativeCallError>,
    intrinsics: &VmIntrinsics,
    cx: &mut IntrinsicInstallContext<'_>,
) -> Result<(), IntrinsicsError> {
    let getter_desc = NativeFunctionDescriptor::getter("get [Symbol.toStringTag]", callback);
    let getter_id = cx.native_functions.register(getter_desc);
    let getter_handle =
        cx.alloc_intrinsic_host_function(getter_id, intrinsics.function_prototype())?;
    let property = cx.property_names.intern_symbol(symbol.stable_id());
    cx.heap
        .define_accessor(target, property, Some(getter_handle), None)?;
    Ok(())
}

/// Map InterpreterError to VmNativeCallError.
fn map_interp_err(e: crate::interpreter::InterpreterError) -> VmNativeCallError {
    match e {
        crate::interpreter::InterpreterError::UncaughtThrow(v) => VmNativeCallError::Thrown(v),
        other => VmNativeCallError::Internal(format!("{other}").into()),
    }
}

/// Call a JS function from native code.
fn call_js(
    runtime: &mut crate::interpreter::RuntimeState,
    callable: RegisterValue,
    this_arg: RegisterValue,
    args: &[RegisterValue],
) -> Result<RegisterValue, VmNativeCallError> {
    let Some(handle_raw) = callable.as_object_handle() else {
        return Err(type_error(runtime, "not a function")?);
    };
    runtime.call_callable(ObjectHandle(handle_raw), this_arg, args)
}

/// js_to_number with VmNativeCallError.
fn to_num(
    runtime: &mut crate::interpreter::RuntimeState,
    value: RegisterValue,
) -> Result<f64, VmNativeCallError> {
    runtime.js_to_number(value).map_err(map_interp_err)
}

/// js_to_string with VmNativeCallError.
fn to_str(
    runtime: &mut crate::interpreter::RuntimeState,
    value: RegisterValue,
) -> Result<String, VmNativeCallError> {
    runtime
        .js_to_string(value)
        .map(|s| s.into())
        .map_err(map_interp_err)
}

fn require_typed_array(
    this: &RegisterValue,
    method_name: &str,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<ObjectHandle, VmNativeCallError> {
    let Some(handle) = this.as_object_handle().map(ObjectHandle) else {
        return Err(type_error(
            runtime,
            &format!("{method_name}: receiver is not a TypedArray"),
        )?);
    };
    if !matches!(
        runtime.objects().kind(handle),
        Ok(HeapValueKind::TypedArray)
    ) {
        return Err(type_error(
            runtime,
            &format!("{method_name}: receiver is not a TypedArray"),
        )?);
    }
    Ok(handle)
}

fn require_not_detached(
    handle: ObjectHandle,
    method_name: &str,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<(), VmNativeCallError> {
    let buf = runtime
        .objects()
        .typed_array_buffer(handle)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    if runtime.objects().kind(buf) == Ok(HeapValueKind::ArrayBuffer)
        && runtime
            .objects()
            .array_buffer_is_detached(buf)
            .unwrap_or(false)
    {
        return Err(type_error(
            runtime,
            &format!("{method_name}: viewed ArrayBuffer is detached"),
        )?);
    }
    Ok(())
}

/// §7.1.22 ToIndex
fn to_index(
    value: RegisterValue,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<usize, VmNativeCallError> {
    if value == RegisterValue::undefined() {
        return Ok(0);
    }
    let number = runtime.js_to_number(value).map_err(map_interp_err)?;
    let integer_index = if number.is_nan() || number == 0.0 {
        0.0
    } else if number.is_infinite() {
        number
    } else {
        number.trunc()
    };
    if integer_index < 0.0 {
        return Err(range_error(runtime, "Invalid typed array length"));
    }
    const MAX_SAFE: f64 = 9_007_199_254_740_991.0;
    let index = if integer_index.is_infinite() {
        MAX_SAFE
    } else {
        integer_index.min(MAX_SAFE)
    };
    if integer_index != index {
        return Err(range_error(runtime, "Invalid typed array length"));
    }
    Ok(index as usize)
}

// ── Class descriptor ────────────────────────────────────────────────

fn typed_array_base_class_descriptor() -> JsClassDescriptor {
    JsClassDescriptor::new("TypedArray")
        .with_constructor(NativeFunctionDescriptor::constructor(
            "TypedArray",
            0,
            typed_array_base_constructor,
        ))
        // §23.2.3 prototype methods
        .with_binding(proto("at", 1, typed_array_at))
        .with_binding(proto("copyWithin", 2, typed_array_copy_within))
        .with_binding(proto("entries", 0, typed_array_entries))
        .with_binding(proto("every", 1, typed_array_every))
        .with_binding(proto("fill", 1, typed_array_fill))
        .with_binding(proto("filter", 1, typed_array_filter))
        .with_binding(proto("find", 1, typed_array_find))
        .with_binding(proto("findIndex", 1, typed_array_find_index))
        .with_binding(proto("findLast", 1, typed_array_find_last))
        .with_binding(proto("findLastIndex", 1, typed_array_find_last_index))
        .with_binding(proto("forEach", 1, typed_array_for_each))
        .with_binding(proto("includes", 1, typed_array_includes))
        .with_binding(proto("indexOf", 1, typed_array_index_of))
        .with_binding(proto("join", 1, typed_array_join))
        .with_binding(proto("keys", 0, typed_array_keys))
        .with_binding(proto("lastIndexOf", 1, typed_array_last_index_of))
        .with_binding(proto("map", 1, typed_array_map))
        .with_binding(proto("reduce", 1, typed_array_reduce))
        .with_binding(proto("reduceRight", 1, typed_array_reduce_right))
        .with_binding(proto("reverse", 0, typed_array_reverse))
        .with_binding(proto("set", 1, typed_array_set))
        .with_binding(proto("slice", 2, typed_array_slice))
        .with_binding(proto("some", 1, typed_array_some))
        .with_binding(proto("sort", 1, typed_array_sort))
        .with_binding(proto("subarray", 2, typed_array_subarray))
        // §23.2.3 ES2023 change-by-copy methods
        .with_binding(proto("toReversed", 0, typed_array_to_reversed))
        .with_binding(proto("toSorted", 1, typed_array_to_sorted))
        .with_binding(proto("with", 2, typed_array_with))
        .with_binding(proto("values", 0, typed_array_values))
        .with_binding(proto("toString", 0, typed_array_to_string))
        .with_binding(proto("toLocaleString", 0, typed_array_to_locale_string))
        // §23.2.2 static methods
        .with_binding(stat("from", 1, typed_array_from))
        .with_binding(stat("of", 0, typed_array_of))
}

fn make_concrete_constructor(kind: TypedArrayKind) -> NativeFunctionDescriptor {
    let ctor_fn: fn(
        &RegisterValue,
        &[RegisterValue],
        &mut crate::interpreter::RuntimeState,
    ) -> Result<RegisterValue, VmNativeCallError> = match kind {
        TypedArrayKind::Int8 => int8_array_constructor,
        TypedArrayKind::Uint8 => uint8_array_constructor,
        TypedArrayKind::Uint8Clamped => uint8_clamped_array_constructor,
        TypedArrayKind::Int16 => int16_array_constructor,
        TypedArrayKind::Uint16 => uint16_array_constructor,
        TypedArrayKind::Int32 => int32_array_constructor,
        TypedArrayKind::Uint32 => uint32_array_constructor,
        TypedArrayKind::Float32 => float32_array_constructor,
        TypedArrayKind::Float64 => float64_array_constructor,
        TypedArrayKind::BigInt64 => bigint64_array_constructor,
        TypedArrayKind::BigUint64 => biguint64_array_constructor,
    };
    let intrinsic = match kind {
        TypedArrayKind::Int8 => crate::intrinsics::IntrinsicKey::Int8ArrayPrototype,
        TypedArrayKind::Uint8 => crate::intrinsics::IntrinsicKey::Uint8ArrayPrototype,
        TypedArrayKind::Uint8Clamped => crate::intrinsics::IntrinsicKey::Uint8ClampedArrayPrototype,
        TypedArrayKind::Int16 => crate::intrinsics::IntrinsicKey::Int16ArrayPrototype,
        TypedArrayKind::Uint16 => crate::intrinsics::IntrinsicKey::Uint16ArrayPrototype,
        TypedArrayKind::Int32 => crate::intrinsics::IntrinsicKey::Int32ArrayPrototype,
        TypedArrayKind::Uint32 => crate::intrinsics::IntrinsicKey::Uint32ArrayPrototype,
        TypedArrayKind::Float32 => crate::intrinsics::IntrinsicKey::Float32ArrayPrototype,
        TypedArrayKind::Float64 => crate::intrinsics::IntrinsicKey::Float64ArrayPrototype,
        TypedArrayKind::BigInt64 => crate::intrinsics::IntrinsicKey::BigInt64ArrayPrototype,
        TypedArrayKind::BigUint64 => crate::intrinsics::IntrinsicKey::BigUint64ArrayPrototype,
    };
    NativeFunctionDescriptor::constructor(kind.constructor_name(), 3, ctor_fn)
        .with_default_intrinsic(intrinsic)
}

// ── %TypedArray% base constructor ───────────────────────────────────

/// §23.2.1.1 %TypedArray% ( )
/// <https://tc39.es/ecma262/#sec-%typedarray%>
///
/// The %TypedArray% intrinsic is not directly callable. It is the base
/// class for all concrete TypedArray constructors.
fn typed_array_base_constructor(
    _this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    Err(type_error(
        runtime,
        "%TypedArray% is not directly constructable",
    )?)
}

// ── Concrete constructors ───────────────────────────────────────────

macro_rules! concrete_typed_array_constructor {
    ($name:ident, $kind:expr) => {
        fn $name(
            _this: &RegisterValue,
            args: &[RegisterValue],
            runtime: &mut crate::interpreter::RuntimeState,
        ) -> Result<RegisterValue, VmNativeCallError> {
            typed_array_construct($kind, args, runtime)
        }
    };
}

concrete_typed_array_constructor!(int8_array_constructor, TypedArrayKind::Int8);
concrete_typed_array_constructor!(uint8_array_constructor, TypedArrayKind::Uint8);
concrete_typed_array_constructor!(
    uint8_clamped_array_constructor,
    TypedArrayKind::Uint8Clamped
);
concrete_typed_array_constructor!(int16_array_constructor, TypedArrayKind::Int16);
concrete_typed_array_constructor!(uint16_array_constructor, TypedArrayKind::Uint16);
concrete_typed_array_constructor!(int32_array_constructor, TypedArrayKind::Int32);
concrete_typed_array_constructor!(uint32_array_constructor, TypedArrayKind::Uint32);
concrete_typed_array_constructor!(float32_array_constructor, TypedArrayKind::Float32);
concrete_typed_array_constructor!(float64_array_constructor, TypedArrayKind::Float64);
concrete_typed_array_constructor!(bigint64_array_constructor, TypedArrayKind::BigInt64);
concrete_typed_array_constructor!(biguint64_array_constructor, TypedArrayKind::BigUint64);

/// §23.2.5 TypedArrayCreate ( constructor, argumentList )
/// <https://tc39.es/ecma262/#sec-typedarray>
///
/// Unified constructor handling all four overload forms:
/// 1. TypedArray() — zero-length
/// 2. TypedArray(length) — new buffer of given length
/// 3. TypedArray(typedArray) — copy from another typed array
/// 4. TypedArray(buffer [, byteOffset [, length]]) — view into buffer
/// 5. TypedArray(iterable/array-like) — from iterable
fn typed_array_construct(
    kind: TypedArrayKind,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    if !runtime.is_current_native_construct_call() {
        return Err(type_error(
            runtime,
            &format!("Constructor {} requires 'new'", kind.constructor_name()),
        )?);
    }

    let first = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);

    // No arguments → zero-length
    if first == RegisterValue::undefined() && args.is_empty() {
        return allocate_typed_array_from_length(kind, 0, runtime);
    }

    // Check if first argument is an object
    if let Some(obj_raw) = first.as_object_handle() {
        let obj = ObjectHandle(obj_raw);
        let obj_kind = runtime
            .objects()
            .kind(obj)
            .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;

        match obj_kind {
            // Form 4: TypedArray(buffer [, byteOffset [, length]])
            HeapValueKind::ArrayBuffer | HeapValueKind::SharedArrayBuffer => {
                return typed_array_from_buffer(kind, obj, obj_kind, args, runtime);
            }
            // Form 3: TypedArray(typedArray)
            HeapValueKind::TypedArray => {
                return typed_array_from_typed_array(kind, obj, runtime);
            }
            // Form 5: TypedArray(iterable/array-like)
            _ => {
                return typed_array_from_array_like(kind, obj, runtime);
            }
        }
    }

    // Form 2: TypedArray(length)
    if let Some(n) = first.as_i32() {
        if n < 0 {
            return Err(range_error(runtime, "Invalid typed array length"));
        }
        return allocate_typed_array_from_length(kind, n as usize, runtime);
    }

    let num = to_num(runtime, first)?;
    if num.is_nan() || num.is_infinite() || num < 0.0 || num != num.floor() {
        return Err(range_error(runtime, "Invalid typed array length"));
    }
    allocate_typed_array_from_length(kind, num as usize, runtime)
}

fn allocate_typed_array_from_length(
    kind: TypedArrayKind,
    length: usize,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let byte_length = length
        .checked_mul(kind.element_size())
        .ok_or_else(|| range_error(runtime, "Invalid typed array length"))?;
    let data = vec![0u8; byte_length];
    let ab_proto = Some(runtime.intrinsics().array_buffer_prototype);
    let buffer = runtime
        .objects_mut()
        .alloc_array_buffer_with_data(data, ab_proto);
    let (_, proto) = runtime.intrinsics().typed_array_constructor_prototype(kind);
    let handle = runtime
        .objects_mut()
        .alloc_typed_array(kind, buffer, 0, length, Some(proto));
    Ok(RegisterValue::from_object_handle(handle.0))
}

/// Form 4: TypedArray(buffer [, byteOffset [, length]])
fn typed_array_from_buffer(
    kind: TypedArrayKind,
    buffer: ObjectHandle,
    buf_kind: HeapValueKind,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let element_size = kind.element_size();

    // Check detached
    if buf_kind == HeapValueKind::ArrayBuffer
        && runtime
            .objects()
            .array_buffer_is_detached(buffer)
            .unwrap_or(false)
    {
        return Err(type_error(
            runtime,
            "Cannot construct TypedArray on detached ArrayBuffer",
        )?);
    }

    let byte_offset = if args.len() > 1 {
        to_index(args[1], runtime)?
    } else {
        0
    };

    if byte_offset % element_size != 0 {
        return Err(range_error(
            runtime,
            "Start offset of TypedArray should be a multiple of element size",
        ));
    }

    let buffer_byte_length = match buf_kind {
        HeapValueKind::ArrayBuffer => runtime
            .objects()
            .array_buffer_byte_length(buffer)
            .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?,
        HeapValueKind::SharedArrayBuffer => runtime
            .objects()
            .shared_array_buffer_byte_length(buffer)
            .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?,
        _ => unreachable!(),
    };

    let (array_length, byte_length) = if args.len() > 2 && args[2] != RegisterValue::undefined() {
        let new_length = to_index(args[2], runtime)?;
        let new_byte_length = new_length * element_size;
        if byte_offset + new_byte_length > buffer_byte_length {
            return Err(range_error(
                runtime,
                "Invalid typed array length: buffer too small",
            ));
        }
        (new_length, new_byte_length)
    } else {
        if byte_offset > buffer_byte_length {
            return Err(range_error(
                runtime,
                "Start offset is outside the bounds of the buffer",
            ));
        }
        let remaining = buffer_byte_length - byte_offset;
        if remaining % element_size != 0 {
            return Err(range_error(
                runtime,
                "Byte length of TypedArray should be a multiple of element size",
            ));
        }
        (remaining / element_size, remaining)
    };

    let _ = byte_length; // used for validation
    let (_, proto) = runtime.intrinsics().typed_array_constructor_prototype(kind);
    let handle = runtime.objects_mut().alloc_typed_array(
        kind,
        buffer,
        byte_offset,
        array_length,
        Some(proto),
    );
    Ok(RegisterValue::from_object_handle(handle.0))
}

/// Form 3: TypedArray(typedArray)
fn typed_array_from_typed_array(
    kind: TypedArrayKind,
    source: ObjectHandle,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    require_not_detached(source, "TypedArray", runtime)?;

    let src_length = runtime
        .objects()
        .typed_array_length(source)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;

    // Read all elements from source as f64
    let mut elements = Vec::with_capacity(src_length);
    for i in 0..src_length {
        let val = runtime
            .objects()
            .typed_array_get_element(source, i)
            .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?
            .unwrap_or(0.0);
        elements.push(val);
    }

    // Allocate new buffer and typed array
    let byte_length = src_length * kind.element_size();
    let data = vec![0u8; byte_length];
    let ab_proto = Some(runtime.intrinsics().array_buffer_prototype);
    let buffer = runtime
        .objects_mut()
        .alloc_array_buffer_with_data(data, ab_proto);
    let (_, proto) = runtime.intrinsics().typed_array_constructor_prototype(kind);
    let handle = runtime
        .objects_mut()
        .alloc_typed_array(kind, buffer, 0, src_length, Some(proto));

    // Write elements
    for (i, val) in elements.into_iter().enumerate() {
        runtime
            .objects_mut()
            .typed_array_set_element(handle, i, val)
            .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    }

    Ok(RegisterValue::from_object_handle(handle.0))
}

/// Form 5: TypedArray(iterable/array-like)
fn typed_array_from_array_like(
    kind: TypedArrayKind,
    source: ObjectHandle,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    // Get array length via "length" property
    let length_prop = runtime.intern_property_name("length");
    let source_val = RegisterValue::from_object_handle(source.0);
    let length_val = runtime.ordinary_get(source, length_prop, source_val)?;
    let length = if length_val == RegisterValue::undefined() {
        0
    } else {
        to_num(runtime, length_val)?.trunc().max(0.0) as usize
    };

    // Create the typed array
    let byte_length = length * kind.element_size();
    let data = vec![0u8; byte_length];
    let ab_proto = Some(runtime.intrinsics().array_buffer_prototype);
    let buffer = runtime
        .objects_mut()
        .alloc_array_buffer_with_data(data, ab_proto);
    let (_, proto) = runtime.intrinsics().typed_array_constructor_prototype(kind);
    let handle = runtime
        .objects_mut()
        .alloc_typed_array(kind, buffer, 0, length, Some(proto));

    // Copy elements via get_index
    for i in 0..length {
        let val = runtime
            .objects_mut()
            .get_index(source, i)
            .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
        if let Some(val) = val {
            let num = to_num(runtime, val)?;
            runtime
                .objects_mut()
                .typed_array_set_element(handle, i, num)
                .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
        }
    }

    Ok(RegisterValue::from_object_handle(handle.0))
}

// ── %TypedArray%.prototype getters ──────────────────────────────────

/// §23.2.3.1 get %TypedArray%.prototype.buffer
fn typed_array_get_buffer(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_typed_array(this, "get %TypedArray%.prototype.buffer", runtime)?;
    let buf = runtime
        .objects()
        .typed_array_buffer(handle)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    Ok(RegisterValue::from_object_handle(buf.0))
}

/// §23.2.3.2 get %TypedArray%.prototype.byteLength
fn typed_array_get_byte_length(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_typed_array(this, "get %TypedArray%.prototype.byteLength", runtime)?;
    let len = runtime
        .objects()
        .typed_array_byte_length(handle)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    Ok(RegisterValue::from_i32(len as i32))
}

/// §23.2.3.3 get %TypedArray%.prototype.byteOffset
fn typed_array_get_byte_offset(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_typed_array(this, "get %TypedArray%.prototype.byteOffset", runtime)?;
    let offset = runtime
        .objects()
        .typed_array_byte_offset(handle)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    Ok(RegisterValue::from_i32(offset as i32))
}

/// §23.2.3.18 get %TypedArray%.prototype.length
fn typed_array_get_length(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_typed_array(this, "get %TypedArray%.prototype.length", runtime)?;
    let len = runtime
        .objects()
        .typed_array_length(handle)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    Ok(RegisterValue::from_i32(len as i32))
}

/// §23.2.3.32 get %TypedArray%.prototype [ @@toStringTag ]
fn typed_array_get_to_string_tag(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let Some(handle_raw) = this.as_object_handle() else {
        return Ok(RegisterValue::undefined());
    };
    let handle = ObjectHandle(handle_raw);
    if !matches!(
        runtime.objects().kind(handle),
        Ok(HeapValueKind::TypedArray)
    ) {
        return Ok(RegisterValue::undefined());
    }
    let kind = runtime
        .objects()
        .typed_array_kind(handle)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    let name = runtime.alloc_string(kind.constructor_name());
    Ok(RegisterValue::from_object_handle(name.0))
}

/// §23.2.2.4 get %TypedArray% [ @@species ]
fn typed_array_species(
    this: &RegisterValue,
    _args: &[RegisterValue],
    _runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    Ok(*this)
}

// ── Prototype methods ───────────────────────────────────────────────

/// Helper: read all elements from a typed array as Vec<f64>.
fn read_all_elements(
    handle: ObjectHandle,
    runtime: &crate::interpreter::RuntimeState,
) -> Result<Vec<f64>, VmNativeCallError> {
    let len = runtime
        .objects()
        .typed_array_length(handle)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    let mut result = Vec::with_capacity(len);
    for i in 0..len {
        let val = runtime
            .objects()
            .typed_array_get_element(handle, i)
            .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?
            .unwrap_or(0.0);
        result.push(val);
    }
    Ok(result)
}

/// Creates a plain Array from a TypedArray's elements (for iterator support).
fn typed_array_to_plain_array(
    handle: ObjectHandle,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<ObjectHandle, VmNativeCallError> {
    let len = runtime
        .objects()
        .typed_array_length(handle)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    let arr = runtime.alloc_array();
    for i in 0..len {
        let val = runtime
            .objects()
            .typed_array_get_element(handle, i)
            .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?
            .unwrap_or(0.0);
        runtime
            .objects_mut()
            .set_index(arr, i, RegisterValue::from_number(val))
            .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    }
    Ok(arr)
}

/// §23.2.3.4 %TypedArray%.prototype.at ( index )
fn typed_array_at(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_typed_array(this, "TypedArray.prototype.at", runtime)?;
    require_not_detached(handle, "TypedArray.prototype.at", runtime)?;
    let len = runtime
        .objects()
        .typed_array_length(handle)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    let relative_index = to_num(
        runtime,
        args.first()
            .copied()
            .unwrap_or_else(RegisterValue::undefined),
    )?
    .trunc() as i64;
    let k = if relative_index >= 0 {
        relative_index as usize
    } else {
        let abs = (-relative_index) as usize;
        if abs > len {
            return Ok(RegisterValue::undefined());
        }
        len - abs
    };
    if k >= len {
        return Ok(RegisterValue::undefined());
    }
    let val = runtime
        .objects()
        .typed_array_get_element(handle, k)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?
        .unwrap_or(0.0);
    Ok(RegisterValue::from_number(val))
}

/// §23.2.3.5 %TypedArray%.prototype.copyWithin ( target, start [ , end ] )
fn typed_array_copy_within(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_typed_array(this, "TypedArray.prototype.copyWithin", runtime)?;
    require_not_detached(handle, "TypedArray.prototype.copyWithin", runtime)?;
    let len = runtime
        .objects()
        .typed_array_length(handle)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))? as i64;

    let target = to_num(
        runtime,
        args.first()
            .copied()
            .unwrap_or_else(RegisterValue::undefined),
    )?
    .trunc() as i64;
    let start = to_num(
        runtime,
        args.get(1)
            .copied()
            .unwrap_or_else(RegisterValue::undefined),
    )?
    .trunc() as i64;
    let end_arg = args
        .get(2)
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let end = if end_arg == RegisterValue::undefined() {
        len
    } else {
        to_num(runtime, end_arg)?.trunc() as i64
    };

    let to = if target < 0 {
        (len + target).max(0)
    } else {
        target.min(len)
    } as usize;
    let from = if start < 0 {
        (len + start).max(0)
    } else {
        start.min(len)
    } as usize;
    let fin = if end < 0 {
        (len + end).max(0)
    } else {
        end.min(len)
    } as usize;
    let count = (fin.saturating_sub(from)).min((len as usize).saturating_sub(to));

    let elements = read_all_elements(handle, runtime)?;
    let mut new_elements = elements.clone();
    if from < to && to < from + count {
        // Copy backwards (overlapping region)
        for i in (0..count).rev() {
            new_elements[to + i] = elements[from + i];
        }
    } else {
        new_elements[to..to + count].copy_from_slice(&elements[from..from + count]);
    }
    for (i, val) in new_elements.into_iter().enumerate() {
        runtime
            .objects_mut()
            .typed_array_set_element(handle, i, val)
            .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    }
    Ok(*this)
}

/// §23.2.3.6 %TypedArray%.prototype.entries ( )
fn typed_array_entries(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_typed_array(this, "TypedArray.prototype.entries", runtime)?;
    require_not_detached(handle, "TypedArray.prototype.entries", runtime)?;
    let arr = typed_array_to_plain_array(handle, runtime)?;
    let iter = runtime
        .objects_mut()
        .alloc_array_iterator(arr, crate::object::ArrayIteratorKind::Entries);
    let proto = runtime.intrinsics().array_iterator_prototype();
    runtime
        .objects_mut()
        .set_prototype(iter, Some(proto))
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    Ok(RegisterValue::from_object_handle(iter.0))
}

/// §23.2.3.7 %TypedArray%.prototype.every ( callbackfn [ , thisArg ] )
fn typed_array_every(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_typed_array(this, "TypedArray.prototype.every", runtime)?;
    require_not_detached(handle, "TypedArray.prototype.every", runtime)?;
    let callback = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let this_arg = args
        .get(1)
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let len = runtime
        .objects()
        .typed_array_length(handle)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    for i in 0..len {
        let val = runtime
            .objects()
            .typed_array_get_element(handle, i)
            .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?
            .unwrap_or(0.0);
        let result = call_js(
            runtime,
            callback,
            this_arg,
            &[
                RegisterValue::from_number(val),
                RegisterValue::from_i32(i as i32),
                *this,
            ],
        )?;
        if !result.is_truthy() {
            return Ok(RegisterValue::from_bool(false));
        }
    }
    Ok(RegisterValue::from_bool(true))
}

/// §23.2.3.8 %TypedArray%.prototype.fill ( value [ , start [ , end ] ] )
fn typed_array_fill(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_typed_array(this, "TypedArray.prototype.fill", runtime)?;
    require_not_detached(handle, "TypedArray.prototype.fill", runtime)?;
    let len = runtime
        .objects()
        .typed_array_length(handle)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))? as i64;
    let fill_value = to_num(
        runtime,
        args.first()
            .copied()
            .unwrap_or_else(RegisterValue::undefined),
    )?;
    let start = if args.len() > 1 {
        to_num(runtime, args[1])?.trunc() as i64
    } else {
        0
    };
    let end = if args.len() > 2 && args[2] != RegisterValue::undefined() {
        to_num(runtime, args[2])?.trunc() as i64
    } else {
        len
    };
    let k = if start < 0 {
        (len + start).max(0)
    } else {
        start.min(len)
    } as usize;
    let fin = if end < 0 {
        (len + end).max(0)
    } else {
        end.min(len)
    } as usize;
    for i in k..fin {
        runtime
            .objects_mut()
            .typed_array_set_element(handle, i, fill_value)
            .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    }
    Ok(*this)
}

/// §23.2.3.9 %TypedArray%.prototype.filter ( callbackfn [ , thisArg ] )
fn typed_array_filter(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_typed_array(this, "TypedArray.prototype.filter", runtime)?;
    require_not_detached(handle, "TypedArray.prototype.filter", runtime)?;
    let callback = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let this_arg = args
        .get(1)
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let kind = runtime
        .objects()
        .typed_array_kind(handle)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    let len = runtime
        .objects()
        .typed_array_length(handle)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    let mut kept = Vec::new();
    for i in 0..len {
        let val = runtime
            .objects()
            .typed_array_get_element(handle, i)
            .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?
            .unwrap_or(0.0);
        let result = call_js(
            runtime,
            callback,
            this_arg,
            &[
                RegisterValue::from_number(val),
                RegisterValue::from_i32(i as i32),
                *this,
            ],
        )?;
        if result.is_truthy() {
            kept.push(val);
        }
    }
    // Create new typed array of same kind
    let byte_length = kept.len() * kind.element_size();
    let data = vec![0u8; byte_length];
    let ab_proto = Some(runtime.intrinsics().array_buffer_prototype);
    let buffer = runtime
        .objects_mut()
        .alloc_array_buffer_with_data(data, ab_proto);
    let (_, proto) = runtime.intrinsics().typed_array_constructor_prototype(kind);
    let result = runtime
        .objects_mut()
        .alloc_typed_array(kind, buffer, 0, kept.len(), Some(proto));
    for (i, val) in kept.into_iter().enumerate() {
        runtime
            .objects_mut()
            .typed_array_set_element(result, i, val)
            .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    }
    Ok(RegisterValue::from_object_handle(result.0))
}

/// §23.2.3.10 %TypedArray%.prototype.find ( predicate [ , thisArg ] )
fn typed_array_find(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_typed_array(this, "TypedArray.prototype.find", runtime)?;
    require_not_detached(handle, "TypedArray.prototype.find", runtime)?;
    let callback = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let this_arg = args
        .get(1)
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let len = runtime
        .objects()
        .typed_array_length(handle)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    for i in 0..len {
        let val = runtime
            .objects()
            .typed_array_get_element(handle, i)
            .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?
            .unwrap_or(0.0);
        let result = call_js(
            runtime,
            callback,
            this_arg,
            &[
                RegisterValue::from_number(val),
                RegisterValue::from_i32(i as i32),
                *this,
            ],
        )?;
        if result.is_truthy() {
            return Ok(RegisterValue::from_number(val));
        }
    }
    Ok(RegisterValue::undefined())
}

/// §23.2.3.11 %TypedArray%.prototype.findIndex ( predicate [ , thisArg ] )
fn typed_array_find_index(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_typed_array(this, "TypedArray.prototype.findIndex", runtime)?;
    require_not_detached(handle, "TypedArray.prototype.findIndex", runtime)?;
    let callback = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let this_arg = args
        .get(1)
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let len = runtime
        .objects()
        .typed_array_length(handle)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    for i in 0..len {
        let val = runtime
            .objects()
            .typed_array_get_element(handle, i)
            .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?
            .unwrap_or(0.0);
        let result = call_js(
            runtime,
            callback,
            this_arg,
            &[
                RegisterValue::from_number(val),
                RegisterValue::from_i32(i as i32),
                *this,
            ],
        )?;
        if result.is_truthy() {
            return Ok(RegisterValue::from_i32(i as i32));
        }
    }
    Ok(RegisterValue::from_i32(-1))
}

/// §23.2.3.12 %TypedArray%.prototype.findLast ( predicate [ , thisArg ] )
fn typed_array_find_last(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_typed_array(this, "TypedArray.prototype.findLast", runtime)?;
    require_not_detached(handle, "TypedArray.prototype.findLast", runtime)?;
    let callback = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let this_arg = args
        .get(1)
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let len = runtime
        .objects()
        .typed_array_length(handle)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    for i in (0..len).rev() {
        let val = runtime
            .objects()
            .typed_array_get_element(handle, i)
            .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?
            .unwrap_or(0.0);
        let result = call_js(
            runtime,
            callback,
            this_arg,
            &[
                RegisterValue::from_number(val),
                RegisterValue::from_i32(i as i32),
                *this,
            ],
        )?;
        if result.is_truthy() {
            return Ok(RegisterValue::from_number(val));
        }
    }
    Ok(RegisterValue::undefined())
}

/// §23.2.3.13 %TypedArray%.prototype.findLastIndex ( predicate [ , thisArg ] )
fn typed_array_find_last_index(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_typed_array(this, "TypedArray.prototype.findLastIndex", runtime)?;
    require_not_detached(handle, "TypedArray.prototype.findLastIndex", runtime)?;
    let callback = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let this_arg = args
        .get(1)
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let len = runtime
        .objects()
        .typed_array_length(handle)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    for i in (0..len).rev() {
        let val = runtime
            .objects()
            .typed_array_get_element(handle, i)
            .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?
            .unwrap_or(0.0);
        let result = call_js(
            runtime,
            callback,
            this_arg,
            &[
                RegisterValue::from_number(val),
                RegisterValue::from_i32(i as i32),
                *this,
            ],
        )?;
        if result.is_truthy() {
            return Ok(RegisterValue::from_i32(i as i32));
        }
    }
    Ok(RegisterValue::from_i32(-1))
}

/// §23.2.3.14 %TypedArray%.prototype.forEach ( callbackfn [ , thisArg ] )
fn typed_array_for_each(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_typed_array(this, "TypedArray.prototype.forEach", runtime)?;
    require_not_detached(handle, "TypedArray.prototype.forEach", runtime)?;
    let callback = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let this_arg = args
        .get(1)
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let len = runtime
        .objects()
        .typed_array_length(handle)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    for i in 0..len {
        let val = runtime
            .objects()
            .typed_array_get_element(handle, i)
            .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?
            .unwrap_or(0.0);
        call_js(
            runtime,
            callback,
            this_arg,
            &[
                RegisterValue::from_number(val),
                RegisterValue::from_i32(i as i32),
                *this,
            ],
        )?;
    }
    Ok(RegisterValue::undefined())
}

/// §23.2.3.15 %TypedArray%.prototype.includes ( searchElement [ , fromIndex ] )
fn typed_array_includes(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_typed_array(this, "TypedArray.prototype.includes", runtime)?;
    require_not_detached(handle, "TypedArray.prototype.includes", runtime)?;
    let len = runtime
        .objects()
        .typed_array_length(handle)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))? as i64;
    if len == 0 {
        return Ok(RegisterValue::from_bool(false));
    }
    let search = to_num(
        runtime,
        args.first()
            .copied()
            .unwrap_or_else(RegisterValue::undefined),
    )?;
    let from = if args.len() > 1 {
        to_num(runtime, args[1])?.trunc() as i64
    } else {
        0
    };
    let k = if from >= 0 {
        from.min(len) as usize
    } else {
        (len + from).max(0) as usize
    };
    for i in k..len as usize {
        let val = runtime
            .objects()
            .typed_array_get_element(handle, i)
            .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?
            .unwrap_or(0.0);
        // SameValueZero
        if val == search || (val.is_nan() && search.is_nan()) {
            return Ok(RegisterValue::from_bool(true));
        }
    }
    Ok(RegisterValue::from_bool(false))
}

/// §23.2.3.16 %TypedArray%.prototype.indexOf ( searchElement [ , fromIndex ] )
fn typed_array_index_of(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_typed_array(this, "TypedArray.prototype.indexOf", runtime)?;
    require_not_detached(handle, "TypedArray.prototype.indexOf", runtime)?;
    let len = runtime
        .objects()
        .typed_array_length(handle)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))? as i64;
    if len == 0 {
        return Ok(RegisterValue::from_i32(-1));
    }
    let search = to_num(
        runtime,
        args.first()
            .copied()
            .unwrap_or_else(RegisterValue::undefined),
    )?;
    let from = if args.len() > 1 {
        to_num(runtime, args[1])?.trunc() as i64
    } else {
        0
    };
    let k = if from >= 0 {
        from.min(len)
    } else {
        (len + from).max(0)
    } as usize;
    for i in k..len as usize {
        let val = runtime
            .objects()
            .typed_array_get_element(handle, i)
            .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?
            .unwrap_or(0.0);
        // Strict equality
        if val == search {
            return Ok(RegisterValue::from_i32(i as i32));
        }
    }
    Ok(RegisterValue::from_i32(-1))
}

/// §23.2.3.17 %TypedArray%.prototype.join ( separator )
fn typed_array_join(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_typed_array(this, "TypedArray.prototype.join", runtime)?;
    require_not_detached(handle, "TypedArray.prototype.join", runtime)?;
    let len = runtime
        .objects()
        .typed_array_length(handle)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    let sep_arg = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let sep = if sep_arg == RegisterValue::undefined() {
        ",".to_string()
    } else {
        to_str(runtime, sep_arg)?
    };
    let mut result = String::new();
    for i in 0..len {
        if i > 0 {
            result.push_str(&sep);
        }
        let val = runtime
            .objects()
            .typed_array_get_element(handle, i)
            .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?
            .unwrap_or(0.0);
        result.push_str(&format_number(val));
    }
    let s = runtime.alloc_string(result);
    Ok(RegisterValue::from_object_handle(s.0))
}

/// §23.2.3.19 %TypedArray%.prototype.keys ( )
fn typed_array_keys(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_typed_array(this, "TypedArray.prototype.keys", runtime)?;
    require_not_detached(handle, "TypedArray.prototype.keys", runtime)?;
    let arr = typed_array_to_plain_array(handle, runtime)?;
    let iter = runtime
        .objects_mut()
        .alloc_array_iterator(arr, crate::object::ArrayIteratorKind::Keys);
    let proto = runtime.intrinsics().array_iterator_prototype();
    runtime
        .objects_mut()
        .set_prototype(iter, Some(proto))
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    Ok(RegisterValue::from_object_handle(iter.0))
}

/// §23.2.3.20 %TypedArray%.prototype.lastIndexOf ( searchElement [ , fromIndex ] )
fn typed_array_last_index_of(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_typed_array(this, "TypedArray.prototype.lastIndexOf", runtime)?;
    require_not_detached(handle, "TypedArray.prototype.lastIndexOf", runtime)?;
    let len = runtime
        .objects()
        .typed_array_length(handle)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))? as i64;
    if len == 0 {
        return Ok(RegisterValue::from_i32(-1));
    }
    let search = to_num(
        runtime,
        args.first()
            .copied()
            .unwrap_or_else(RegisterValue::undefined),
    )?;
    let from = if args.len() > 1 {
        to_num(runtime, args[1])?.trunc() as i64
    } else {
        len - 1
    };
    let k = if from >= 0 {
        from.min(len - 1)
    } else {
        len + from
    } as i64;
    for i in (0..=k).rev() {
        let val = runtime
            .objects()
            .typed_array_get_element(handle, i as usize)
            .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?
            .unwrap_or(0.0);
        if val == search {
            return Ok(RegisterValue::from_i32(i as i32));
        }
    }
    Ok(RegisterValue::from_i32(-1))
}

/// §23.2.3.21 %TypedArray%.prototype.map ( callbackfn [ , thisArg ] )
fn typed_array_map(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_typed_array(this, "TypedArray.prototype.map", runtime)?;
    require_not_detached(handle, "TypedArray.prototype.map", runtime)?;
    let callback = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let this_arg = args
        .get(1)
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let kind = runtime
        .objects()
        .typed_array_kind(handle)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    let len = runtime
        .objects()
        .typed_array_length(handle)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;

    let byte_length = len * kind.element_size();
    let data = vec![0u8; byte_length];
    let ab_proto = Some(runtime.intrinsics().array_buffer_prototype);
    let buffer = runtime
        .objects_mut()
        .alloc_array_buffer_with_data(data, ab_proto);
    let (_, proto) = runtime.intrinsics().typed_array_constructor_prototype(kind);
    let result = runtime
        .objects_mut()
        .alloc_typed_array(kind, buffer, 0, len, Some(proto));

    for i in 0..len {
        let val = runtime
            .objects()
            .typed_array_get_element(handle, i)
            .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?
            .unwrap_or(0.0);
        let mapped = call_js(
            runtime,
            callback,
            this_arg,
            &[
                RegisterValue::from_number(val),
                RegisterValue::from_i32(i as i32),
                *this,
            ],
        )?;
        let num = to_num(runtime, mapped)?;
        runtime
            .objects_mut()
            .typed_array_set_element(result, i, num)
            .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    }
    Ok(RegisterValue::from_object_handle(result.0))
}

/// §23.2.3.22 %TypedArray%.prototype.reduce ( callbackfn [ , initialValue ] )
fn typed_array_reduce(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_typed_array(this, "TypedArray.prototype.reduce", runtime)?;
    require_not_detached(handle, "TypedArray.prototype.reduce", runtime)?;
    let callback = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let len = runtime
        .objects()
        .typed_array_length(handle)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    let mut k = 0;
    let mut accumulator = if args.len() > 1 {
        args[1]
    } else {
        if len == 0 {
            return Err(type_error(
                runtime,
                "Reduce of empty array with no initial value",
            )?);
        }
        let val = runtime
            .objects()
            .typed_array_get_element(handle, 0)
            .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?
            .unwrap_or(0.0);
        k = 1;
        RegisterValue::from_number(val)
    };
    for i in k..len {
        let val = runtime
            .objects()
            .typed_array_get_element(handle, i)
            .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?
            .unwrap_or(0.0);
        accumulator = call_js(
            runtime,
            callback,
            RegisterValue::undefined(),
            &[
                accumulator,
                RegisterValue::from_number(val),
                RegisterValue::from_i32(i as i32),
                *this,
            ],
        )?;
    }
    Ok(accumulator)
}

/// §23.2.3.23 %TypedArray%.prototype.reduceRight ( callbackfn [ , initialValue ] )
fn typed_array_reduce_right(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_typed_array(this, "TypedArray.prototype.reduceRight", runtime)?;
    require_not_detached(handle, "TypedArray.prototype.reduceRight", runtime)?;
    let callback = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let len = runtime
        .objects()
        .typed_array_length(handle)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    let mut k = len;
    let mut accumulator = if args.len() > 1 {
        args[1]
    } else {
        if len == 0 {
            return Err(type_error(
                runtime,
                "Reduce of empty array with no initial value",
            )?);
        }
        let val = runtime
            .objects()
            .typed_array_get_element(handle, len - 1)
            .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?
            .unwrap_or(0.0);
        k = len - 1;
        RegisterValue::from_number(val)
    };
    for i in (0..k).rev() {
        let val = runtime
            .objects()
            .typed_array_get_element(handle, i)
            .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?
            .unwrap_or(0.0);
        accumulator = call_js(
            runtime,
            callback,
            RegisterValue::undefined(),
            &[
                accumulator,
                RegisterValue::from_number(val),
                RegisterValue::from_i32(i as i32),
                *this,
            ],
        )?;
    }
    Ok(accumulator)
}

/// §23.2.3.24 %TypedArray%.prototype.reverse ( )
fn typed_array_reverse(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_typed_array(this, "TypedArray.prototype.reverse", runtime)?;
    require_not_detached(handle, "TypedArray.prototype.reverse", runtime)?;
    let _len = runtime
        .objects()
        .typed_array_length(handle)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    let mut elements = read_all_elements(handle, runtime)?;
    elements.reverse();
    for (i, val) in elements.into_iter().enumerate() {
        runtime
            .objects_mut()
            .typed_array_set_element(handle, i, val)
            .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    }
    Ok(*this)
}

/// §23.2.3.25 %TypedArray%.prototype.set ( source [ , offset ] )
fn typed_array_set(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_typed_array(this, "TypedArray.prototype.set", runtime)?;
    require_not_detached(handle, "TypedArray.prototype.set", runtime)?;
    let target_offset = if args.len() > 1 {
        to_num(runtime, args[1])?.trunc().max(0.0) as usize
    } else {
        0
    };
    let target_length = runtime
        .objects()
        .typed_array_length(handle)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;

    let source = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let Some(src_raw) = source.as_object_handle() else {
        return Err(type_error(
            runtime,
            "TypedArray.prototype.set: source is not an object",
        )?);
    };
    let src = ObjectHandle(src_raw);
    let src_kind = runtime
        .objects()
        .kind(src)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;

    if src_kind == HeapValueKind::TypedArray {
        let src_length = runtime
            .objects()
            .typed_array_length(src)
            .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
        if target_offset + src_length > target_length {
            return Err(range_error(
                runtime,
                "TypedArray.prototype.set: source is too large",
            ));
        }
        let elements = read_all_elements(src, runtime)?;
        for (i, val) in elements.into_iter().enumerate() {
            runtime
                .objects_mut()
                .typed_array_set_element(handle, target_offset + i, val)
                .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
        }
    } else {
        // Array-like source
        let length_prop = runtime.intern_property_name("length");
        let src_val = RegisterValue::from_object_handle(src.0);
        let len_val = runtime.ordinary_get(src, length_prop, src_val)?;
        let src_length = if len_val == RegisterValue::undefined() {
            0
        } else {
            to_num(runtime, len_val)?.trunc().max(0.0) as usize
        };
        if target_offset + src_length > target_length {
            return Err(range_error(
                runtime,
                "TypedArray.prototype.set: source is too large",
            ));
        }
        for i in 0..src_length {
            let val = runtime
                .objects_mut()
                .get_index(src, i)
                .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?
                .unwrap_or_else(RegisterValue::undefined);
            let num = to_num(runtime, val)?;
            runtime
                .objects_mut()
                .typed_array_set_element(handle, target_offset + i, num)
                .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
        }
    }

    Ok(RegisterValue::undefined())
}

/// §23.2.3.26 %TypedArray%.prototype.slice ( start, end )
fn typed_array_slice(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_typed_array(this, "TypedArray.prototype.slice", runtime)?;
    require_not_detached(handle, "TypedArray.prototype.slice", runtime)?;
    let kind = runtime
        .objects()
        .typed_array_kind(handle)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    let len = runtime
        .objects()
        .typed_array_length(handle)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))? as i64;

    let start = to_num(
        runtime,
        args.first()
            .copied()
            .unwrap_or_else(RegisterValue::undefined),
    )?
    .trunc() as i64;
    let end_arg = args
        .get(1)
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let end = if end_arg == RegisterValue::undefined() {
        len
    } else {
        to_num(runtime, end_arg)?.trunc() as i64
    };

    let k = if start < 0 {
        (len + start).max(0)
    } else {
        start.min(len)
    } as usize;
    let fin = if end < 0 {
        (len + end).max(0)
    } else {
        end.min(len)
    } as usize;
    let count = fin.saturating_sub(k);

    let byte_length = count * kind.element_size();
    let data = vec![0u8; byte_length];
    let ab_proto = Some(runtime.intrinsics().array_buffer_prototype);
    let buffer = runtime
        .objects_mut()
        .alloc_array_buffer_with_data(data, ab_proto);
    let (_, proto) = runtime.intrinsics().typed_array_constructor_prototype(kind);
    let result = runtime
        .objects_mut()
        .alloc_typed_array(kind, buffer, 0, count, Some(proto));

    for i in 0..count {
        let val = runtime
            .objects()
            .typed_array_get_element(handle, k + i)
            .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?
            .unwrap_or(0.0);
        runtime
            .objects_mut()
            .typed_array_set_element(result, i, val)
            .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    }
    Ok(RegisterValue::from_object_handle(result.0))
}

/// §23.2.3.27 %TypedArray%.prototype.some ( callbackfn [ , thisArg ] )
fn typed_array_some(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_typed_array(this, "TypedArray.prototype.some", runtime)?;
    require_not_detached(handle, "TypedArray.prototype.some", runtime)?;
    let callback = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let this_arg = args
        .get(1)
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let len = runtime
        .objects()
        .typed_array_length(handle)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    for i in 0..len {
        let val = runtime
            .objects()
            .typed_array_get_element(handle, i)
            .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?
            .unwrap_or(0.0);
        let result = call_js(
            runtime,
            callback,
            this_arg,
            &[
                RegisterValue::from_number(val),
                RegisterValue::from_i32(i as i32),
                *this,
            ],
        )?;
        if result.is_truthy() {
            return Ok(RegisterValue::from_bool(true));
        }
    }
    Ok(RegisterValue::from_bool(false))
}

/// §23.2.3.28 %TypedArray%.prototype.sort ( comparefn )
fn typed_array_sort(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_typed_array(this, "TypedArray.prototype.sort", runtime)?;
    require_not_detached(handle, "TypedArray.prototype.sort", runtime)?;
    let comparefn = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let mut elements = read_all_elements(handle, runtime)?;

    if comparefn == RegisterValue::undefined() {
        // Default: sort numerically
        elements.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    } else {
        // Use comparefn — we need a simple sort that calls the callback
        // Insertion sort to avoid complex error handling in Rust closures
        for i in 1..elements.len() {
            let key = elements[i];
            let mut j = i;
            while j > 0 {
                let cmp_result = call_js(
                    runtime,
                    comparefn,
                    RegisterValue::undefined(),
                    &[
                        RegisterValue::from_number(elements[j - 1]),
                        RegisterValue::from_number(key),
                    ],
                )?;
                let cmp = to_num(runtime, cmp_result).unwrap_or(0.0);
                if cmp <= 0.0 {
                    break;
                }
                elements[j] = elements[j - 1];
                j -= 1;
            }
            elements[j] = key;
        }
    }

    for (i, val) in elements.into_iter().enumerate() {
        runtime
            .objects_mut()
            .typed_array_set_element(handle, i, val)
            .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    }
    Ok(*this)
}

/// §23.2.3.29 %TypedArray%.prototype.subarray ( begin, end )
fn typed_array_subarray(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_typed_array(this, "TypedArray.prototype.subarray", runtime)?;
    let kind = runtime
        .objects()
        .typed_array_kind(handle)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    let buffer = runtime
        .objects()
        .typed_array_buffer(handle)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    let src_length = runtime
        .objects()
        .typed_array_length(handle)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?
        as i64;
    let src_byte_offset = runtime
        .objects()
        .typed_array_byte_offset(handle)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;

    let begin = to_num(
        runtime,
        args.first()
            .copied()
            .unwrap_or_else(RegisterValue::undefined),
    )?
    .trunc() as i64;
    let end_arg = args
        .get(1)
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let end = if end_arg == RegisterValue::undefined() {
        src_length
    } else {
        to_num(runtime, end_arg)?.trunc() as i64
    };

    let begin_index = if begin < 0 {
        (src_length + begin).max(0)
    } else {
        begin.min(src_length)
    } as usize;
    let end_index = if end < 0 {
        (src_length + end).max(0)
    } else {
        end.min(src_length)
    } as usize;
    let new_length = end_index.saturating_sub(begin_index);
    let element_size = kind.element_size();
    let new_byte_offset = src_byte_offset + begin_index * element_size;

    let (_, proto) = runtime.intrinsics().typed_array_constructor_prototype(kind);
    let result = runtime.objects_mut().alloc_typed_array(
        kind,
        buffer,
        new_byte_offset,
        new_length,
        Some(proto),
    );
    Ok(RegisterValue::from_object_handle(result.0))
}

/// §23.2.3.30 %TypedArray%.prototype.values ( )
fn typed_array_values(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_typed_array(this, "TypedArray.prototype.values", runtime)?;
    require_not_detached(handle, "TypedArray.prototype.values", runtime)?;
    let arr = typed_array_to_plain_array(handle, runtime)?;
    let iter = runtime
        .objects_mut()
        .alloc_array_iterator(arr, crate::object::ArrayIteratorKind::Values);
    let proto = runtime.intrinsics().array_iterator_prototype();
    runtime
        .objects_mut()
        .set_prototype(iter, Some(proto))
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    Ok(RegisterValue::from_object_handle(iter.0))
}

/// %TypedArray%.prototype.toString — delegates to Array.prototype.join
fn typed_array_to_string(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    typed_array_join(this, &[], runtime)
}

/// %TypedArray%.prototype.toLocaleString
fn typed_array_to_locale_string(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    typed_array_join(this, &[], runtime)
}

// ── Static methods ──────────────────────────────────────────────────

/// §23.2.2.1 %TypedArray%.from ( source [ , mapfn [ , thisArg ] ] )
fn typed_array_from(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    // `this` is the constructor (e.g., Uint8Array)
    let source = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let mapfn = args
        .get(1)
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let this_arg = args
        .get(2)
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let has_map = mapfn != RegisterValue::undefined();

    let Some(src_raw) = source.as_object_handle() else {
        return Err(type_error(
            runtime,
            "%TypedArray%.from: source is not an object",
        )?);
    };
    let src = ObjectHandle(src_raw);

    // Get length
    let length_prop = runtime.intern_property_name("length");
    let src_val = RegisterValue::from_object_handle(src.0);
    let len_val = runtime.ordinary_get(src, length_prop, src_val)?;
    let len = if len_val == RegisterValue::undefined() {
        0
    } else {
        to_num(runtime, len_val)?.trunc().max(0.0) as usize
    };

    // Determine target kind from `this` (the constructor)
    let Some(ctor_raw) = this.as_object_handle() else {
        return Err(type_error(
            runtime,
            "%TypedArray%.from requires a TypedArray constructor as this",
        )?);
    };
    let kind = find_kind_from_constructor(ObjectHandle(ctor_raw), runtime)?;

    let byte_length = len * kind.element_size();
    let data = vec![0u8; byte_length];
    let ab_proto = Some(runtime.intrinsics().array_buffer_prototype);
    let buffer = runtime
        .objects_mut()
        .alloc_array_buffer_with_data(data, ab_proto);
    let (_, proto) = runtime.intrinsics().typed_array_constructor_prototype(kind);
    let result = runtime
        .objects_mut()
        .alloc_typed_array(kind, buffer, 0, len, Some(proto));

    for i in 0..len {
        let val = runtime
            .objects_mut()
            .get_index(src, i)
            .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?
            .unwrap_or_else(RegisterValue::undefined);
        let mapped = if has_map {
            call_js(
                runtime,
                mapfn,
                this_arg,
                &[val, RegisterValue::from_i32(i as i32)],
            )?
        } else {
            val
        };
        let num = to_num(runtime, mapped)?;
        runtime
            .objects_mut()
            .typed_array_set_element(result, i, num)
            .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    }

    Ok(RegisterValue::from_object_handle(result.0))
}

/// §23.2.2.2 %TypedArray%.of ( ...items )
fn typed_array_of(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let Some(ctor_raw) = this.as_object_handle() else {
        return Err(type_error(
            runtime,
            "%TypedArray%.of requires a TypedArray constructor as this",
        )?);
    };
    let kind = find_kind_from_constructor(ObjectHandle(ctor_raw), runtime)?;
    let len = args.len();

    let byte_length = len * kind.element_size();
    let data = vec![0u8; byte_length];
    let ab_proto = Some(runtime.intrinsics().array_buffer_prototype);
    let buffer = runtime
        .objects_mut()
        .alloc_array_buffer_with_data(data, ab_proto);
    let (_, proto) = runtime.intrinsics().typed_array_constructor_prototype(kind);
    let result = runtime
        .objects_mut()
        .alloc_typed_array(kind, buffer, 0, len, Some(proto));

    for (i, arg) in args.iter().enumerate() {
        let num = to_num(runtime, *arg)?;
        runtime
            .objects_mut()
            .typed_array_set_element(result, i, num)
            .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    }

    Ok(RegisterValue::from_object_handle(result.0))
}

/// Try to determine which TypedArrayKind the given constructor handle corresponds to.
fn find_kind_from_constructor(
    ctor: ObjectHandle,
    runtime: &crate::interpreter::RuntimeState,
) -> Result<TypedArrayKind, VmNativeCallError> {
    for &kind in TypedArrayKind::all() {
        let (c, _) = runtime.intrinsics().typed_array_constructor_prototype(kind);
        if c == ctor {
            return Ok(kind);
        }
    }
    // Default to Uint8Array if we can't determine
    Ok(TypedArrayKind::Uint8)
}

// ─── ES2023 change-by-copy methods ──────────────────────────────────────────

/// §23.2.3.30 %TypedArray%.prototype.toReversed ( )
/// <https://tc39.es/ecma262/#sec-%typedarray%.prototype.toreversed>
///
/// Returns a new TypedArray of the same kind with elements in reverse order.
fn typed_array_to_reversed(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_typed_array(this, "%TypedArray%.prototype.toReversed", runtime)?;
    require_not_detached(handle, "%TypedArray%.prototype.toReversed", runtime)?;
    let kind = runtime
        .objects()
        .typed_array_kind(handle)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    let len = runtime
        .objects()
        .typed_array_length(handle)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;

    // Read elements in reverse.
    let mut elements = Vec::with_capacity(len);
    for i in (0..len).rev() {
        let val = runtime
            .objects()
            .typed_array_get_element(handle, i)
            .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?
            .unwrap_or(0.0);
        elements.push(val);
    }

    // Allocate new TypedArray with same kind.
    let result = alloc_typed_array_from_elements(kind, &elements, runtime)?;
    Ok(RegisterValue::from_object_handle(result.0))
}

/// §23.2.3.31 %TypedArray%.prototype.toSorted ( comparefn )
/// <https://tc39.es/ecma262/#sec-%typedarray%.prototype.tosorted>
///
/// Returns a new TypedArray of the same kind with elements sorted.
fn typed_array_to_sorted(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_typed_array(this, "%TypedArray%.prototype.toSorted", runtime)?;
    require_not_detached(handle, "%TypedArray%.prototype.toSorted", runtime)?;
    let kind = runtime
        .objects()
        .typed_array_kind(handle)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;

    let comparefn = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);

    // §23.2.3.31 step 2: If comparefn is not undefined and not callable, throw TypeError.
    if comparefn != RegisterValue::undefined() {
        if let Some(h) = comparefn.as_object_handle().map(ObjectHandle) {
            if !runtime.objects().is_callable(h) {
                return Err(type_error(
                    runtime,
                    "%TypedArray%.prototype.toSorted: comparefn is not callable",
                )?);
            }
        } else {
            return Err(type_error(
                runtime,
                "%TypedArray%.prototype.toSorted: comparefn is not callable",
            )?);
        }
    }

    let mut elements = read_all_elements(handle, runtime)?;

    if comparefn == RegisterValue::undefined() {
        // Default: sort numerically (§23.2.3.31 step 8).
        elements.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    } else {
        // Insertion sort with user comparefn.
        for i in 1..elements.len() {
            let key = elements[i];
            let mut j = i;
            while j > 0 {
                let cmp_result = call_js(
                    runtime,
                    comparefn,
                    RegisterValue::undefined(),
                    &[
                        RegisterValue::from_number(elements[j - 1]),
                        RegisterValue::from_number(key),
                    ],
                )?;
                let cmp = to_num(runtime, cmp_result).unwrap_or(0.0);
                if cmp <= 0.0 {
                    break;
                }
                elements[j] = elements[j - 1];
                j -= 1;
            }
            elements[j] = key;
        }
    }

    let result = alloc_typed_array_from_elements(kind, &elements, runtime)?;
    Ok(RegisterValue::from_object_handle(result.0))
}

/// §23.2.3.37 %TypedArray%.prototype.with ( index, value )
/// <https://tc39.es/ecma262/#sec-%typedarray%.prototype.with>
///
/// Returns a new TypedArray identical to the original except the element at
/// `index` is replaced with `value`.
fn typed_array_with(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_typed_array(this, "%TypedArray%.prototype.with", runtime)?;
    require_not_detached(handle, "%TypedArray%.prototype.with", runtime)?;
    let kind = runtime
        .objects()
        .typed_array_kind(handle)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    let len = runtime
        .objects()
        .typed_array_length(handle)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))? as i64;

    // Step 3: Let relativeIndex be ? ToIntegerOrInfinity(index).
    let raw_index = to_num(
        runtime,
        args.first()
            .copied()
            .unwrap_or_else(RegisterValue::undefined),
    )?
    .trunc() as i64;

    let actual_index = if raw_index < 0 {
        len + raw_index
    } else {
        raw_index
    };

    // Step 7: If actualIndex < 0 or actualIndex ≥ len, throw RangeError.
    if actual_index < 0 || actual_index >= len {
        let err = runtime
            .alloc_range_error("TypedArray.prototype.with: index out of range")
            .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
        return Err(VmNativeCallError::Thrown(
            RegisterValue::from_object_handle(err.0),
        ));
    }
    let actual_index = actual_index as usize;

    // Step 5-6: Coerce value based on content type.
    let new_value_arg = args
        .get(1)
        .copied()
        .unwrap_or_else(RegisterValue::undefined);

    // Read all elements, replace at index.
    let mut elements = read_all_elements(handle, runtime)?;

    if kind.is_bigint_kind() {
        // For BigInt typed arrays, the value must be a BigInt.
        let Some(bigint_handle) = new_value_arg.as_bigint_handle() else {
            return Err(type_error(
                runtime,
                "%TypedArray%.prototype.with: BigInt typed arrays require BigInt values",
            )?);
        };
        let bigint_str = match runtime.objects().bigint_value(ObjectHandle(bigint_handle)) {
            Ok(Some(s)) => s.to_string(),
            _ => {
                return Err(type_error(
                    runtime,
                    "%TypedArray%.prototype.with: invalid BigInt",
                )?);
            }
        };
        let n: f64 = match kind {
            TypedArrayKind::BigInt64 => {
                let v: i64 = bigint_str.parse().unwrap_or(0);
                v as f64
            }
            TypedArrayKind::BigUint64 => {
                let v: u64 = bigint_str.parse().unwrap_or(0);
                v as f64
            }
            _ => unreachable!(),
        };
        elements[actual_index] = n;
    } else {
        // For numeric typed arrays, coerce to Number.
        let n = to_num(runtime, new_value_arg)?;
        elements[actual_index] = n;
    }

    let result = alloc_typed_array_from_elements(kind, &elements, runtime)?;
    Ok(RegisterValue::from_object_handle(result.0))
}

/// Helper: allocate a new TypedArray of the given kind from a slice of f64 elements.
fn alloc_typed_array_from_elements(
    kind: TypedArrayKind,
    elements: &[f64],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<ObjectHandle, VmNativeCallError> {
    let byte_length = elements.len() * kind.element_size();
    let data = vec![0u8; byte_length];
    let ab_proto = Some(runtime.intrinsics().array_buffer_prototype);
    let buffer = runtime
        .objects_mut()
        .alloc_array_buffer_with_data(data, ab_proto);
    let (_, proto) = runtime.intrinsics().typed_array_constructor_prototype(kind);
    let result =
        runtime
            .objects_mut()
            .alloc_typed_array(kind, buffer, 0, elements.len(), Some(proto));
    for (i, &val) in elements.iter().enumerate() {
        runtime
            .objects_mut()
            .typed_array_set_element(result, i, val)
            .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    }
    Ok(result)
}

/// Format a number for join/toString.
fn format_number(n: f64) -> String {
    if n == 0.0 && n.is_sign_positive() {
        "0".to_string()
    } else if n.is_nan() {
        "NaN".to_string()
    } else if n.is_infinite() {
        if n.is_sign_positive() {
            "Infinity".to_string()
        } else {
            "-Infinity".to_string()
        }
    } else if n == n.trunc() && n.abs() < 1e20 {
        // Integer-like values
        format!("{}", n as i64)
    } else {
        format!("{}", n)
    }
}
