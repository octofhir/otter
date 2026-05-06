//! Static JavaScript surface specs and mutator-bound builders.
//!
//! This module is the task-96 backend for JavaScript-visible
//! namespaces, functions, constructors, classes, accessors, and data
//! properties. Contributors describe exported names, arity, native
//! call targets, and attributes in static records; builders install
//! those records during a single mutator turn.
//!
//! # Contents
//! - [`Attr`] — explicit property attributes.
//! - [`PropertySpec`], [`MethodSpec`], [`AccessorSpec`],
//!   [`ConstructorSpec`], [`ClassSpec`], and [`NamespaceSpec`] —
//!   static surface records.
//! - [`ObjectBuilder`], [`FunctionBuilder`],
//!   [`ConstructorBuilder`], [`ClassBuilder`], and
//!   [`NamespaceBuilder`] — mutator-bound installers.
//!
//! # Invariants
//! - Spec records contain only static metadata and native call
//!   targets. They never contain `Gc<T>`, `Local<'gc, T>`, VM
//!   frames, or borrowed contexts.
//! - Builders are lifetime-bound to a mutable heap borrow and carry
//!   an `Rc` marker so they remain `!Send + !Sync`.
//! - Every object store goes through [`crate::object`] descriptor
//!   APIs so write barriers fire.
//! - Static builtins use [`crate::NativeCall::Static`] by default;
//!   dynamic closures are opt-in for embedder state.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-property-attributes>
//! - <https://tc39.es/ecma262/#sec-ecmascript-function-objects>
//! - [`docs/new-engine/tasks/96-production-js-surface-builders.md`](
//!     ../../../docs/new-engine/tasks/96-production-js-surface-builders.md
//!   )

use std::marker::PhantomData;
use std::rc::Rc;

use crate::native_function::{NativeCall, NativeFunction};
use crate::number::NumberValue;
use crate::object::{self, JsObject, PropertyDescriptor, PropertyFlags};
use crate::{ClassConstructor, NativeCtx, Value};

/// Explicit JavaScript property attributes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Attr {
    /// `[[Writable]]`.
    pub writable: bool,
    /// `[[Enumerable]]`.
    pub enumerable: bool,
    /// `[[Configurable]]`.
    pub configurable: bool,
}

impl Attr {
    /// Build attributes from individual bits.
    #[must_use]
    pub const fn new(writable: bool, enumerable: bool, configurable: bool) -> Self {
        Self {
            writable,
            enumerable,
            configurable,
        }
    }

    /// Default ordinary data-property attributes.
    #[must_use]
    pub const fn data() -> Self {
        Self::new(true, true, true)
    }

    /// Standard builtin function attributes:
    /// writable/configurable and non-enumerable.
    #[must_use]
    pub const fn builtin_function() -> Self {
        Self::new(true, false, true)
    }

    /// Standard read-only builtin constant attributes:
    /// non-writable, non-enumerable, and non-configurable.
    #[must_use]
    pub const fn read_only() -> Self {
        Self::new(false, false, false)
    }

    /// Standard global binding attributes for writable builtin
    /// namespaces and constructors.
    #[must_use]
    pub const fn global_binding() -> Self {
        Self::new(true, false, true)
    }

    /// Convert to the VM object-model flag representation.
    #[must_use]
    pub const fn to_flags(self) -> PropertyFlags {
        PropertyFlags::new(self.writable, self.enumerable, self.configurable)
    }
}

/// Static primitive value used by property/constant specs.
#[derive(Debug, Clone, Copy)]
pub enum ConstValue {
    /// JavaScript `undefined`.
    Undefined,
    /// JavaScript `null`.
    Null,
    /// JavaScript boolean.
    Boolean(bool),
    /// JavaScript number.
    Number(f64),
}

impl ConstValue {
    fn to_value(self) -> Value {
        match self {
            Self::Undefined => Value::Undefined,
            Self::Null => Value::Null,
            Self::Boolean(v) => Value::Boolean(v),
            Self::Number(v) => Value::Number(NumberValue::from_f64(v)),
        }
    }
}

/// Static data-property spec.
#[derive(Debug, Clone, Copy)]
pub struct PropertySpec {
    /// Exported JavaScript property name.
    pub name: &'static str,
    /// Stored value.
    pub value: ConstValue,
    /// Property attributes.
    pub attrs: Attr,
}

/// Static constant spec.
pub type ConstSpec = PropertySpec;

/// Static method spec.
#[derive(Debug, Clone)]
pub struct MethodSpec {
    /// Exported JavaScript property name.
    pub name: &'static str,
    /// ECMAScript `.length`.
    pub length: u8,
    /// Property attributes for the method property.
    pub attrs: Attr,
    /// Native call target.
    pub call: NativeCall,
}

/// Static accessor spec.
#[derive(Debug, Clone)]
pub struct AccessorSpec {
    /// Exported JavaScript property name.
    pub name: &'static str,
    /// Getter call target, when present.
    pub get: Option<NativeCall>,
    /// Setter call target, when present.
    pub set: Option<NativeCall>,
    /// Accessor property attributes. `writable` is ignored.
    pub attrs: Attr,
}

/// Static constructor/prototype surface spec.
#[derive(Debug, Clone)]
pub struct ConstructorSpec {
    /// Exported constructor name.
    pub name: &'static str,
    /// ECMAScript constructor `.length`.
    pub length: u8,
    /// Constructor call target.
    pub call: NativeCall,
    /// Static constructor methods.
    pub static_methods: &'static [MethodSpec],
    /// Prototype methods.
    pub prototype_methods: &'static [MethodSpec],
    /// Global property attributes for the constructor binding.
    pub attrs: Attr,
}

/// Static class-shaped surface spec.
#[derive(Debug, Clone)]
pub struct ClassSpec {
    /// Constructor/prototype spec.
    pub constructor: ConstructorSpec,
    /// Prototype accessors.
    pub prototype_accessors: &'static [AccessorSpec],
}

/// Static namespace object spec.
#[derive(Debug, Clone)]
pub struct NamespaceSpec {
    /// Exported namespace/global name.
    pub name: &'static str,
    /// Static methods.
    pub methods: &'static [MethodSpec],
    /// Accessors.
    pub accessors: &'static [AccessorSpec],
    /// Constants/data properties.
    pub constants: &'static [ConstSpec],
    /// Global property attributes for installing this namespace.
    pub attrs: Attr,
}

/// Mutator-bound builder for an ordinary object.
pub struct ObjectBuilder<'rt> {
    heap: &'rt mut otter_gc::GcHeap,
    object: JsObject,
    _not_send_sync: PhantomData<Rc<()>>,
}

impl<'rt> ObjectBuilder<'rt> {
    /// Allocate a fresh object and bind the builder to `heap`.
    pub fn new(heap: &'rt mut otter_gc::GcHeap) -> Result<Self, otter_gc::OutOfMemory> {
        let object = object::alloc_object(heap)?;
        Ok(Self {
            heap,
            object,
            _not_send_sync: PhantomData,
        })
    }

    /// Allocate a fresh object through a native context.
    pub fn new_in_ctx<'a>(
        ctx: &'a mut NativeCtx<'_>,
    ) -> Result<ObjectBuilder<'a>, otter_gc::OutOfMemory> {
        ObjectBuilder::<'a>::new(ctx.heap_mut())
    }

    /// Bind a builder to an existing object.
    #[must_use]
    pub fn from_object(heap: &'rt mut otter_gc::GcHeap, object: JsObject) -> Self {
        Self {
            heap,
            object,
            _not_send_sync: PhantomData,
        }
    }

    /// Define a data property.
    pub fn property(
        &mut self,
        name: &'static str,
        value: Value,
        attrs: Attr,
    ) -> Result<&mut Self, JsSurfaceError> {
        define_data(self.object, self.heap, name, value, attrs)?;
        Ok(self)
    }

    /// Define a property from a static spec.
    pub fn property_from_spec(&mut self, spec: &PropertySpec) -> Result<&mut Self, JsSurfaceError> {
        self.property(spec.name, spec.value.to_value(), spec.attrs)
    }

    /// Define a native method.
    pub fn method(
        &mut self,
        name: &'static str,
        length: u8,
        call: NativeCall,
        attrs: Attr,
    ) -> Result<&mut Self, JsSurfaceError> {
        let native = NativeFunction::from_call(self.heap, name, length, call)?;
        define_data(
            self.object,
            self.heap,
            name,
            Value::NativeFunction(native),
            attrs,
        )?;
        Ok(self)
    }

    /// Define a method from a static spec.
    pub fn method_from_spec(&mut self, spec: &MethodSpec) -> Result<&mut Self, JsSurfaceError> {
        self.method(spec.name, spec.length, spec.call.clone(), spec.attrs)
    }

    /// Define an accessor property.
    pub fn accessor_from_spec(&mut self, spec: &AccessorSpec) -> Result<&mut Self, JsSurfaceError> {
        let getter = match &spec.get {
            Some(call) => Some(Value::NativeFunction(NativeFunction::from_call(
                self.heap,
                spec.name,
                0,
                call.clone(),
            )?)),
            None => None,
        };
        let setter = match &spec.set {
            Some(call) => Some(Value::NativeFunction(NativeFunction::from_call(
                self.heap,
                spec.name,
                1,
                call.clone(),
            )?)),
            None => None,
        };
        let descriptor = PropertyDescriptor::accessor(
            getter,
            setter,
            spec.attrs.enumerable,
            spec.attrs.configurable,
        );
        if object::define_own_property(self.object, self.heap, spec.name, descriptor) {
            Ok(self)
        } else {
            Err(JsSurfaceError::DefinePropertyFailed(spec.name))
        }
    }

    /// Finish object construction.
    #[must_use]
    pub fn build(self) -> JsObject {
        self.object
    }
}

/// Mutator-bound builder for a native function value.
pub struct FunctionBuilder<'rt> {
    heap: &'rt mut otter_gc::GcHeap,
    name: &'static str,
    length: u8,
    call: NativeCall,
    _not_send_sync: PhantomData<Rc<()>>,
}

impl<'rt> FunctionBuilder<'rt> {
    /// Start a function builder.
    #[must_use]
    pub fn new(
        heap: &'rt mut otter_gc::GcHeap,
        name: &'static str,
        length: u8,
        call: NativeCall,
    ) -> Self {
        Self {
            heap,
            name,
            length,
            call,
            _not_send_sync: PhantomData,
        }
    }

    /// Build the function value.
    pub fn build(self) -> Result<Value, otter_gc::OutOfMemory> {
        Ok(Value::NativeFunction(NativeFunction::from_call(
            self.heap,
            self.name,
            self.length,
            self.call,
        )?))
    }
}

/// Mutator-bound builder for constructor-shaped objects.
pub struct ConstructorBuilder<'rt> {
    heap: &'rt mut otter_gc::GcHeap,
    spec: &'static ConstructorSpec,
    _not_send_sync: PhantomData<Rc<()>>,
}

impl<'rt> ConstructorBuilder<'rt> {
    /// Start from a static constructor spec.
    #[must_use]
    pub fn from_spec(heap: &'rt mut otter_gc::GcHeap, spec: &'static ConstructorSpec) -> Self {
        Self {
            heap,
            spec,
            _not_send_sync: PhantomData,
        }
    }

    /// Build a constructor object with `.prototype` and method bags.
    pub fn build(self) -> Result<JsObject, JsSurfaceError> {
        let ctor = object::alloc_object(self.heap)?;
        let proto = object::alloc_object(self.heap)?;
        let function = NativeFunction::from_call(
            self.heap,
            self.spec.name,
            self.spec.length,
            self.spec.call.clone(),
        )?;
        define_data(
            ctor,
            self.heap,
            "call",
            Value::NativeFunction(function),
            Attr::builtin_function(),
        )?;
        define_data(
            ctor,
            self.heap,
            "prototype",
            Value::Object(proto),
            Attr::data(),
        )?;
        for method in self.spec.static_methods {
            ObjectBuilder::from_object(self.heap, ctor).method_from_spec(method)?;
        }
        for method in self.spec.prototype_methods {
            ObjectBuilder::from_object(self.heap, proto).method_from_spec(method)?;
        }
        Ok(ctor)
    }
}

/// Mutator-bound builder for class-shaped specs.
pub struct ClassBuilder<'rt> {
    heap: &'rt mut otter_gc::GcHeap,
    spec: &'static ClassSpec,
    _not_send_sync: PhantomData<Rc<()>>,
}

impl<'rt> ClassBuilder<'rt> {
    /// Start from a static class spec.
    #[must_use]
    pub fn from_spec(heap: &'rt mut otter_gc::GcHeap, spec: &'static ClassSpec) -> Self {
        Self {
            heap,
            spec,
            _not_send_sync: PhantomData,
        }
    }

    /// Build the class constructor value.
    pub fn build(self) -> Result<Value, JsSurfaceError> {
        let prototype = object::alloc_object(self.heap)?;
        let statics = object::alloc_object(self.heap)?;
        let constructor = Value::NativeFunction(NativeFunction::from_call(
            self.heap,
            self.spec.constructor.name,
            self.spec.constructor.length,
            self.spec.constructor.call.clone(),
        )?);

        {
            let mut static_builder = ObjectBuilder::from_object(self.heap, statics);
            for method in self.spec.constructor.static_methods {
                static_builder.method_from_spec(method)?;
            }
        }

        {
            let mut prototype_builder = ObjectBuilder::from_object(self.heap, prototype);
            for method in self.spec.constructor.prototype_methods {
                prototype_builder.method_from_spec(method)?;
            }
            for accessor in self.spec.prototype_accessors {
                prototype_builder.accessor_from_spec(accessor)?;
            }
        }

        let class = Value::ClassConstructor(Rc::new(ClassConstructor {
            ctor: constructor,
            prototype,
            statics,
        }));
        define_data(
            prototype,
            self.heap,
            "constructor",
            class.clone(),
            Attr::builtin_function(),
        )?;
        Ok(class)
    }
}

/// Mutator-bound builder for a namespace object.
pub struct NamespaceBuilder<'rt> {
    heap: &'rt mut otter_gc::GcHeap,
    spec: &'static NamespaceSpec,
    object: JsObject,
    _not_send_sync: PhantomData<Rc<()>>,
}

impl<'rt> NamespaceBuilder<'rt> {
    /// Allocate a namespace object from a static spec.
    pub fn from_spec(
        heap: &'rt mut otter_gc::GcHeap,
        spec: &'static NamespaceSpec,
    ) -> Result<Self, otter_gc::OutOfMemory> {
        let object = object::alloc_object(heap)?;
        Ok(Self {
            heap,
            spec,
            object,
            _not_send_sync: PhantomData,
        })
    }

    /// Allocate a namespace object through a native context.
    pub fn from_spec_in_ctx<'a>(
        ctx: &'a mut NativeCtx<'_>,
        spec: &'static NamespaceSpec,
    ) -> Result<NamespaceBuilder<'a>, otter_gc::OutOfMemory> {
        NamespaceBuilder::<'a>::from_spec(ctx.heap_mut(), spec)
    }

    /// Install all constants, methods, and accessors on the object.
    pub fn build(self) -> Result<JsObject, JsSurfaceError> {
        {
            let mut object = ObjectBuilder::from_object(self.heap, self.object);
            for property in self.spec.constants {
                object.property_from_spec(property)?;
            }
            for method in self.spec.methods {
                object.method_from_spec(method)?;
            }
            for accessor in self.spec.accessors {
                object.accessor_from_spec(accessor)?;
            }
        }
        Ok(self.object)
    }
}

/// JS surface builder failure.
#[derive(Debug, Clone, thiserror::Error)]
#[non_exhaustive]
pub enum JsSurfaceError {
    /// GC allocation failed.
    #[error("out of memory while installing JS surface")]
    OutOfMemory,
    /// OrdinaryDefineOwnProperty rejected a descriptor.
    #[error("failed to define JS property {0}")]
    DefinePropertyFailed(&'static str),
}

impl From<otter_gc::OutOfMemory> for JsSurfaceError {
    fn from(_: otter_gc::OutOfMemory) -> Self {
        Self::OutOfMemory
    }
}

fn define_data(
    object: JsObject,
    heap: &mut otter_gc::GcHeap,
    name: &'static str,
    value: Value,
    attrs: Attr,
) -> Result<(), JsSurfaceError> {
    let descriptor = PropertyDescriptor {
        kind: crate::object::DescriptorKind::Data { value },
        flags: attrs.to_flags(),
    };
    if object::define_own_property(object, heap, name, descriptor) {
        Ok(())
    } else {
        Err(JsSurfaceError::DefinePropertyFailed(name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{NativeError, native_function::NativeCallTarget};

    fn one(_: &mut NativeCtx<'_>, _: &[Value]) -> Result<Value, NativeError> {
        Ok(Value::Number(NumberValue::from_i32(1)))
    }

    fn construct(_: &mut NativeCtx<'_>, _: &[Value]) -> Result<Value, NativeError> {
        Ok(Value::Undefined)
    }

    static METHODS: &[MethodSpec] = &[MethodSpec {
        name: "one",
        length: 0,
        attrs: Attr::builtin_function(),
        call: NativeCall::Static(one),
    }];

    static CONSTANTS: &[ConstSpec] = &[ConstSpec {
        name: "PI",
        value: ConstValue::Number(3.0),
        attrs: Attr::read_only(),
    }];

    static SPEC: NamespaceSpec = NamespaceSpec {
        name: "Test",
        methods: METHODS,
        accessors: &[],
        constants: CONSTANTS,
        attrs: Attr::global_binding(),
    };

    static CLASS_STATIC_METHODS: &[MethodSpec] = &[MethodSpec {
        name: "from",
        length: 1,
        attrs: Attr::builtin_function(),
        call: NativeCall::Static(one),
    }];

    static CLASS_PROTOTYPE_METHODS: &[MethodSpec] = &[MethodSpec {
        name: "valueOf",
        length: 0,
        attrs: Attr::builtin_function(),
        call: NativeCall::Static(one),
    }];

    static CLASS_PROTOTYPE_ACCESSORS: &[AccessorSpec] = &[AccessorSpec {
        name: "answer",
        get: Some(NativeCall::Static(one)),
        set: None,
        attrs: Attr::new(false, false, true),
    }];

    static CLASS_SPEC: ClassSpec = ClassSpec {
        constructor: ConstructorSpec {
            name: "Widget",
            length: 1,
            call: NativeCall::Static(construct),
            static_methods: CLASS_STATIC_METHODS,
            prototype_methods: CLASS_PROTOTYPE_METHODS,
            attrs: Attr::global_binding(),
        },
        prototype_accessors: CLASS_PROTOTYPE_ACCESSORS,
    };

    static_assertions::assert_not_impl_any!(ObjectBuilder<'static>: Send, Sync);
    static_assertions::assert_not_impl_any!(NamespaceBuilder<'static>: Send, Sync);
    static_assertions::assert_not_impl_any!(ClassBuilder<'static>: Send, Sync);

    #[test]
    fn installs_static_attrs_and_static_native_call() {
        let mut heap = otter_gc::GcHeap::new().expect("heap");
        let ns = NamespaceBuilder::from_spec(&mut heap, &SPEC)
            .expect("builder")
            .build()
            .expect("build");

        let pi = object::get_own_descriptor(ns, &heap, "PI").expect("PI");
        assert!(!pi.writable());
        assert!(!pi.enumerable());
        assert!(!pi.configurable());

        let method = object::get(ns, &heap, "one").expect("one");
        let Value::NativeFunction(native) = method else {
            panic!("method should be native")
        };
        assert!(native.is_static_call(&heap));
        assert_eq!(native.length(&heap), 0);
        assert!(matches!(
            native.call_target(&heap),
            NativeCallTarget::Static(_)
        ));
    }

    #[test]
    fn class_builder_installs_spec_shaped_constructor_value() {
        let mut heap = otter_gc::GcHeap::new().expect("heap");
        let class = ClassBuilder::from_spec(&mut heap, &CLASS_SPEC)
            .build()
            .expect("build");
        let Value::ClassConstructor(class) = class else {
            panic!("class builder should produce a class constructor value")
        };

        let Value::NativeFunction(ctor) = &class.ctor else {
            panic!("class constructor should use a native function")
        };
        assert!(ctor.is_static_call(&heap));
        assert_eq!(ctor.length(&heap), 1);

        let Value::NativeFunction(static_method) =
            object::get(class.statics, &heap, "from").expect("static method")
        else {
            panic!("static method should be native")
        };
        assert!(static_method.is_static_call(&heap));
        assert_eq!(static_method.length(&heap), 1);

        let Value::NativeFunction(proto_method) =
            object::get(class.prototype, &heap, "valueOf").expect("prototype method")
        else {
            panic!("prototype method should be native")
        };
        assert!(proto_method.is_static_call(&heap));
        assert_eq!(proto_method.length(&heap), 0);

        let constructor = object::get(class.prototype, &heap, "constructor")
            .expect("prototype constructor backlink");
        assert!(matches!(constructor, Value::ClassConstructor(_)));
        let accessor = object::get_own_descriptor(class.prototype, &heap, "answer")
            .expect("prototype accessor");
        assert!(accessor.is_accessor());
        assert!(!accessor.enumerable());
        assert!(accessor.configurable());
    }
}
