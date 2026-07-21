//! Runtime-owned JavaScript surface descriptions and builders.
//!
//! This module is the public runtime facade over the active static JS surface
//! backend. Product crates should import these runtime-owned names instead of
//! depending on VM-shaped descriptor names through hidden adapter modules.
//!
//! # Contents
//! - Runtime-owned aliases for native values, calls, attributes, and specs.
//! - Helper constructors for methods, accessors, constants, classes, and
//!   namespaces.
//! - Host-object helpers for plain Rust state and explicitly traced JS slots.
//!
//! # Invariants
//! - Specs contain only static metadata and native call targets.
//! - Builders are bound to a single native mutator turn and must not be stored.
//! - Plain host-object data stores no JS values. Traced payloads enumerate only
//!   opaque slots and never see a raw collector visitor.
//! - This facade must compile down to the existing static backend without
//!   runtime metadata parsing or hot-path registries.
//!
//! # See also
//! - [JS surface builders](../../../docs/book/src/extensions/js-surface-builders.md)
//! - [Native bindings](../../../docs/book/src/extensions/native-bindings.md)

use std::sync::Arc;

pub use otter_vm::HostAtomInterner;
pub use otter_vm::object::{
    HostDataTracer as RuntimeHostDataTracer, HostObjectData as RuntimeHostObjectData,
    HostValueSlot as RuntimeHostValueSlot, TracedHostObjectData as RuntimeTracedHostObjectData,
};
pub use otter_vm::{HostAtom as RuntimeHostAtom, HostAtomId as RuntimeHostAtomId};

/// Runtime-owned object handle alias.
pub type RuntimeJsObject = otter_vm::JsObject;
/// Runtime-owned JavaScript value alias.
pub type RuntimeValue = otter_vm::Value;
/// Runtime-owned native context alias.
pub type RuntimeNativeCtx<'rt> = otter_vm::NativeCtx<'rt>;
/// Runtime-owned native scope. The collector token is private; contributors
/// allocate and access values only through this mutator-bound view.
pub type RuntimeNativeScope<'s, 'rt> = otter_vm::NativeScope<'s, 'rt>;
/// Runtime-owned local handle. Its lifetime pins it inside the active native
/// scope, so moving collection rewrites remain invisible to contributor code.
pub type RuntimeLocal<'s> = otter_vm::Local<'s>;
/// Runtime-owned native error alias.
pub type RuntimeNativeError = otter_vm::NativeError;
/// Runtime-owned native fast function pointer.
pub type RuntimeNativeFastFn = otter_vm::NativeFastFn;
/// Runtime-owned dynamic native function.
pub type RuntimeNativeFn = otter_vm::NativeFn;
/// Runtime-owned native call target.
pub type RuntimeNativeCall = otter_vm::NativeCall;
/// Runtime-owned property attributes.
pub type RuntimeAttr = otter_vm::Attr;
/// Runtime-owned static primitive constant value.
pub type RuntimeConstValue = otter_vm::ConstValue;
/// Runtime-owned static property spec.
pub type RuntimePropertySpec = otter_vm::PropertySpec;
/// Runtime-owned static constant spec.
pub type RuntimeConstSpec = otter_vm::ConstSpec;
/// Runtime-owned static method spec.
pub type RuntimeMethodSpec = otter_vm::MethodSpec;
/// Runtime-owned static accessor spec.
pub type RuntimeAccessorSpec = otter_vm::AccessorSpec;
/// Runtime-owned constructor/prototype spec.
pub type RuntimeConstructorSpec = otter_vm::ConstructorSpec;
/// Runtime-owned class-shaped spec.
pub type RuntimeClassSpec = otter_vm::ClassSpec;
/// Runtime-owned namespace spec.
pub type RuntimeNamespaceSpec = otter_vm::NamespaceSpec;
/// Runtime-owned number value alias.
pub type RuntimeNumberValue = otter_vm::number::NumberValue;
/// Runtime-owned JS string alias.
pub type RuntimeJsString = otter_vm::string::JsString;
/// Runtime-owned host-object access error.
pub type RuntimeHostObjectError = otter_vm::object::HostObjectError;
/// Runtime-owned JS surface construction error.
pub type RuntimeSurfaceError = otter_vm::JsSurfaceError;

/// Build a static native call target.
#[must_use]
pub const fn runtime_native_static(call: RuntimeNativeFastFn) -> RuntimeNativeCall {
    RuntimeNativeCall::Static(call)
}

/// Build a dynamic native call target for rare captured-state cases.
#[must_use]
pub fn runtime_native_dynamic(call: Arc<RuntimeNativeFn>) -> RuntimeNativeCall {
    RuntimeNativeCall::Dynamic(call)
}

/// Build a runtime type error for native bindings.
#[must_use]
pub fn runtime_type_error(name: &'static str, reason: impl Into<String>) -> RuntimeNativeError {
    RuntimeNativeError::TypeError {
        name,
        reason: reason.into(),
    }
}

/// Coerce one argument to a display string, treating missing/undefined as `""`.
#[must_use]
pub fn runtime_arg_to_string(
    args: &[RuntimeValue],
    index: usize,
    heap: &otter_gc::GcHeap,
) -> String {
    match args.get(index) {
        Some(value) => {
            if let Some(s) = value.as_string(heap) {
                s.to_lossy_string(heap)
            } else if value.is_undefined() {
                String::new()
            } else {
                value.display_string(heap)
            }
        }
        None => String::new(),
    }
}

/// Coerce one optional argument to a display string.
#[must_use]
pub fn runtime_optional_arg_to_string(
    args: &[RuntimeValue],
    index: usize,
    heap: &otter_gc::GcHeap,
) -> Option<String> {
    match args.get(index) {
        Some(value) => {
            if let Some(s) = value.as_string(heap) {
                Some(s.to_lossy_string(heap))
            } else if value.is_undefined() {
                None
            } else {
                Some(value.display_string(heap))
            }
        }
        None => None,
    }
}

/// Allocate a JavaScript string value through the native context.
pub fn runtime_string_value(
    ctx: &mut RuntimeNativeCtx<'_>,
    value: &str,
) -> Result<RuntimeValue, RuntimeNativeError> {
    Ok(RuntimeValue::string(
        RuntimeJsString::from_str(value, ctx.heap_mut())
            .map_err(|err| runtime_type_error("string", err.to_string()))?,
    ))
}

/// Return the current receiver as an object or raise a runtime type error.
pub fn runtime_this_object(
    ctx: &RuntimeNativeCtx<'_>,
    name: &'static str,
    expected: &'static str,
) -> Result<RuntimeJsObject, RuntimeNativeError> {
    if let Some(object) = ctx.this_value().as_object() {
        Ok(object)
    } else {
        Err(runtime_type_error(
            name,
            format!("invalid {expected} receiver"),
        ))
    }
}

/// Build a builtin method spec with standard builtin function attributes.
#[must_use]
pub const fn runtime_method(
    name: &'static str,
    length: u8,
    call: RuntimeNativeFastFn,
) -> RuntimeMethodSpec {
    runtime_method_with_attrs(
        name,
        length,
        runtime_native_static(call),
        RuntimeAttr::builtin_function(),
    )
}

/// Build a method spec with explicit attributes and call target.
#[must_use]
pub const fn runtime_method_with_attrs(
    name: &'static str,
    length: u8,
    call: RuntimeNativeCall,
    attrs: RuntimeAttr,
) -> RuntimeMethodSpec {
    RuntimeMethodSpec {
        name,
        length,
        attrs,
        call,
    }
}

/// Build a getter-only accessor spec with standard builtin attributes.
#[must_use]
pub const fn runtime_getter(name: &'static str, get: RuntimeNativeFastFn) -> RuntimeAccessorSpec {
    runtime_accessor(
        name,
        Some(runtime_native_static(get)),
        None,
        RuntimeAttr::builtin_function(),
    )
}

/// Build an accessor spec with explicit getter, setter, and attributes.
#[must_use]
pub const fn runtime_accessor(
    name: &'static str,
    get: Option<RuntimeNativeCall>,
    set: Option<RuntimeNativeCall>,
    attrs: RuntimeAttr,
) -> RuntimeAccessorSpec {
    RuntimeAccessorSpec {
        name,
        // A `const fn` cannot concatenate `"get "`/`"set "` onto a
        // runtime `&'static str`; runtime-built accessors fall back to
        // the bare name. The `couch!` / `raft!` macros, which own the
        // builtin accessor surface, emit the spec-correct
        // `"get <name>"` / `"set <name>"` from string literals.
        get_name: name,
        set_name: name,
        get,
        set,
        attrs,
    }
}

/// Build a read-only builtin constant spec.
#[must_use]
pub const fn runtime_constant(name: &'static str, value: RuntimeConstValue) -> RuntimeConstSpec {
    runtime_property(name, value, RuntimeAttr::read_only())
}

/// Build a static data property spec.
#[must_use]
pub const fn runtime_property(
    name: &'static str,
    value: RuntimeConstValue,
    attrs: RuntimeAttr,
) -> RuntimePropertySpec {
    RuntimePropertySpec { name, value, attrs }
}

/// Build a constructor/prototype surface spec.
#[must_use]
pub const fn runtime_constructor(
    name: &'static str,
    length: u8,
    call: RuntimeNativeFastFn,
    static_methods: &'static [RuntimeMethodSpec],
    prototype_methods: &'static [RuntimeMethodSpec],
    attrs: RuntimeAttr,
) -> RuntimeConstructorSpec {
    RuntimeConstructorSpec {
        name,
        length,
        call: runtime_native_static(call),
        static_methods,
        prototype_methods,
        attrs,
    }
}

/// Build a class-shaped surface spec.
#[must_use]
pub const fn runtime_class(
    constructor: RuntimeConstructorSpec,
    prototype_accessors: &'static [RuntimeAccessorSpec],
) -> RuntimeClassSpec {
    RuntimeClassSpec {
        constructor,
        prototype_accessors,
    }
}

/// Build a namespace surface spec.
#[must_use]
pub const fn runtime_namespace(
    name: &'static str,
    methods: &'static [RuntimeMethodSpec],
    accessors: &'static [RuntimeAccessorSpec],
    constants: &'static [RuntimeConstSpec],
    attrs: RuntimeAttr,
) -> RuntimeNamespaceSpec {
    RuntimeNamespaceSpec {
        name,
        methods,
        accessors,
        constants,
        attrs,
    }
}

/// Allocate a fresh ordinary object through the native context.
pub fn runtime_alloc_object(
    ctx: &mut RuntimeNativeCtx<'_>,
) -> Result<RuntimeJsObject, otter_gc::OutOfMemory> {
    ctx.alloc_object()
}

/// Read typed host data from a receiver object.
pub fn runtime_with_host_data<T, R>(
    ctx: &RuntimeNativeCtx<'_>,
    object: RuntimeJsObject,
    f: impl FnOnce(&T) -> R,
) -> Result<R, RuntimeHostObjectError>
where
    T: RuntimeHostObjectData,
{
    otter_vm::object::with_host_data::<T, R>(object, ctx.heap(), f)
}

/// Mutably borrow typed host data from a receiver object.
pub fn runtime_with_host_data_mut<T, R>(
    ctx: &mut RuntimeNativeCtx<'_>,
    object: RuntimeJsObject,
    f: impl FnOnce(&mut T) -> R,
) -> Result<R, RuntimeHostObjectError>
where
    T: RuntimeHostObjectData,
{
    otter_vm::object::with_host_data_mut::<T, R>(object, ctx.interp_mut().gc_heap_mut(), f)
}

/// Set a string-keyed property on an object through ordinary descriptor
/// assignment.
pub fn runtime_set_property(
    ctx: &mut RuntimeNativeCtx<'_>,
    object: RuntimeJsObject,
    key: &str,
    value: RuntimeValue,
) -> bool {
    otter_vm::object::ordinary_set_data_property(object, ctx.interp_mut().gc_heap_mut(), key, value)
}

/// Build an array from already-created JS values.
pub fn runtime_array_from_elements(
    ctx: &mut RuntimeNativeCtx<'_>,
    values: Vec<RuntimeValue>,
) -> Result<otter_vm::JsArray, otter_gc::OutOfMemory> {
    ctx.array_from_elements(values)
}
