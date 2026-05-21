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
//! - [`RuntimeObjectBuilder`] for mutator-bound object construction.
//! - Host-object helper functions for Rust-owned receiver state.
//!
//! # Invariants
//! - Specs contain only static metadata and native call targets.
//! - Builders are bound to a single native mutator turn and must not be stored.
//! - Host-object data must not store JS values, handles, contexts, or futures.
//! - This facade must compile down to the existing static backend without
//!   runtime metadata parsing or hot-path registries.
//!
//! # See also
//! - [JS surface builders](../../../docs/book/src/extensions/js-surface-builders.md)
//! - [Native bindings](../../../docs/book/src/extensions/native-bindings.md)

use std::sync::Arc;

pub use otter_vm::object::HostObjectData as RuntimeHostObjectData;

/// Runtime-owned object handle alias.
pub type RuntimeJsObject = otter_vm::JsObject;
/// Runtime-owned JavaScript value alias.
pub type RuntimeValue = otter_vm::Value;
/// Runtime-owned native context alias.
pub type RuntimeNativeCtx<'rt> = otter_vm::NativeCtx<'rt>;
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
        Some(RuntimeValue::String(value)) => value.to_lossy_string(),
        Some(RuntimeValue::Undefined) | None => String::new(),
        Some(value) => value.display_string(heap),
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
        Some(RuntimeValue::String(value)) => Some(value.to_lossy_string()),
        Some(RuntimeValue::Undefined) | None => None,
        Some(value) => Some(value.display_string(heap)),
    }
}

/// Allocate a JavaScript string value through the native context.
pub fn runtime_string_value(
    ctx: &mut RuntimeNativeCtx<'_>,
    value: &str,
) -> Result<RuntimeValue, RuntimeNativeError> {
    let heap = ctx.interp_mut().string_heap_clone();
    Ok(RuntimeValue::String(
        RuntimeJsString::from_str(value, &heap)
            .map_err(|err| runtime_type_error("string", err.to_string()))?,
    ))
}

/// Return the current receiver as an object or raise a runtime type error.
pub fn runtime_this_object(
    ctx: &RuntimeNativeCtx<'_>,
    name: &'static str,
    expected: &'static str,
) -> Result<RuntimeJsObject, RuntimeNativeError> {
    match ctx.this_value().clone() {
        RuntimeValue::Object(object) => Ok(object),
        _ => Err(runtime_type_error(
            name,
            format!("invalid {expected} receiver"),
        )),
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

/// Mutator-bound runtime object builder.
pub struct RuntimeObjectBuilder<'rt> {
    inner: otter_vm::ObjectBuilder<'rt>,
}

impl<'rt> RuntimeObjectBuilder<'rt> {
    pub(crate) fn new_in_interpreter(
        interp: &'rt mut otter_vm::Interpreter,
    ) -> Result<Self, otter_gc::OutOfMemory> {
        Ok(Self {
            inner: otter_vm::ObjectBuilder::new_runtime_rooted(interp)?,
        })
    }

    /// Allocate a fresh ordinary object through the current native context.
    pub fn new<'a>(
        ctx: &'a mut RuntimeNativeCtx<'_>,
    ) -> Result<RuntimeObjectBuilder<'a>, otter_gc::OutOfMemory> {
        Self::new_in_ctx(ctx)
    }

    /// Allocate a fresh object through the current native context.
    pub fn new_in_ctx<'a>(
        ctx: &'a mut RuntimeNativeCtx<'_>,
    ) -> Result<RuntimeObjectBuilder<'a>, otter_gc::OutOfMemory> {
        Ok(RuntimeObjectBuilder {
            inner: otter_vm::ObjectBuilder::new_in_ctx(ctx)?,
        })
    }

    /// Allocate a fresh object with Rust-owned host data and bind a builder to
    /// it. Contributor code can use this to create receiver-backed objects
    /// without touching VM heap/object internals.
    pub fn from_host_data<'a, T: RuntimeHostObjectData>(
        ctx: &'a mut RuntimeNativeCtx<'_>,
        data: T,
    ) -> Result<RuntimeObjectBuilder<'a>, otter_gc::OutOfMemory> {
        let object = ctx.alloc_host_object(data)?;
        Ok(Self::from_object(ctx, object))
    }

    /// Bind a builder to an existing object through the current native context.
    #[must_use]
    pub fn from_object<'a>(
        ctx: &'a mut RuntimeNativeCtx<'_>,
        object: RuntimeJsObject,
    ) -> RuntimeObjectBuilder<'a> {
        RuntimeObjectBuilder {
            inner: otter_vm::ObjectBuilder::from_object_in_ctx(ctx, object),
        }
    }

    /// Define a data property.
    pub fn property(
        &mut self,
        name: &'static str,
        value: RuntimeValue,
        attrs: RuntimeAttr,
    ) -> Result<&mut Self, RuntimeSurfaceError> {
        self.inner.property(name, value, attrs)?;
        Ok(self)
    }

    /// Define an ordinary data property.
    pub fn data_property(
        &mut self,
        name: &'static str,
        value: RuntimeValue,
    ) -> Result<&mut Self, RuntimeSurfaceError> {
        self.property(name, value, RuntimeAttr::data())
    }

    /// Define a read-only builtin data property.
    pub fn readonly_property(
        &mut self,
        name: &'static str,
        value: RuntimeValue,
    ) -> Result<&mut Self, RuntimeSurfaceError> {
        self.property(name, value, RuntimeAttr::read_only())
    }

    /// Define a native method.
    pub fn method(
        &mut self,
        name: &'static str,
        length: u8,
        call: RuntimeNativeCall,
        attrs: RuntimeAttr,
    ) -> Result<&mut Self, RuntimeSurfaceError> {
        self.inner.method(name, length, call, attrs)?;
        Ok(self)
    }

    /// Define a builtin native method using the static function-pointer path.
    pub fn builtin_method(
        &mut self,
        name: &'static str,
        length: u8,
        call: RuntimeNativeFastFn,
    ) -> Result<&mut Self, RuntimeSurfaceError> {
        self.method(
            name,
            length,
            runtime_native_static(call),
            RuntimeAttr::builtin_function(),
        )
    }

    /// Define a method from a runtime method spec.
    pub fn method_from_spec(
        &mut self,
        spec: &RuntimeMethodSpec,
    ) -> Result<&mut Self, RuntimeSurfaceError> {
        self.inner.method_from_spec(spec)?;
        Ok(self)
    }

    /// Finish object construction.
    #[must_use]
    pub fn build(self) -> RuntimeJsObject {
        self.inner.build()
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
