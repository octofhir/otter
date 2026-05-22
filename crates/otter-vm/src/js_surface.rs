//! Static JavaScript surface specs and mutator-bound builders.
//!
//! This module is the backend for JavaScript-visible
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
//! - [`ObjectBuilder`], [`ConstructorBuilder`], [`ClassBuilder`],
//!   and [`NamespaceBuilder`] — mutator-bound installers.
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
//! - [JS surface builders](../../../docs/book/src/extensions/js-surface-builders.md)

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
            Self::Undefined => Value::undefined(),
            Self::Null => Value::null(),
            Self::Boolean(v) => Value::boolean(v),
            Self::Number(v) => Value::number(NumberValue::from_f64(v)),
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

fn visit_value_roots(
    visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc),
    value_roots: &[&Value],
    slice_roots: &[&[Value]],
) {
    for value in value_roots {
        value.trace_value_slots(visitor);
    }
    for slice in slice_roots {
        for value in *slice {
            value.trace_value_slots(visitor);
        }
    }
}

fn visit_raw_and_value_roots(
    visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc),
    raw_roots: &[*mut otter_gc::raw::RawGc],
    value_roots: &[&Value],
    slice_roots: &[&[Value]],
) {
    for &slot in raw_roots {
        visitor(slot);
    }
    visit_value_roots(visitor, value_roots, slice_roots);
}

fn alloc_object_with_roots(
    heap: &mut otter_gc::GcHeap,
    value_roots: &[&Value],
    slice_roots: &[&[Value]],
) -> Result<JsObject, otter_gc::OutOfMemory> {
    let mut external_visit = |visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
        visit_value_roots(visitor, value_roots, slice_roots);
    };
    object::alloc_object_with_roots(heap, &mut external_visit)
}

fn alloc_object_with_raw_roots(
    heap: &mut otter_gc::GcHeap,
    raw_roots: &[*mut otter_gc::raw::RawGc],
    value_roots: &[&Value],
    slice_roots: &[&[Value]],
) -> Result<JsObject, otter_gc::OutOfMemory> {
    let mut external_visit = |visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
        visit_raw_and_value_roots(visitor, raw_roots, value_roots, slice_roots);
    };
    object::alloc_object_with_roots(heap, &mut external_visit)
}

fn native_from_call_with_raw_roots(
    heap: &mut otter_gc::GcHeap,
    name: &'static str,
    length: u8,
    call: NativeCall,
    raw_roots: &[*mut otter_gc::raw::RawGc],
    value_roots: &[&Value],
    slice_roots: &[&[Value]],
) -> Result<NativeFunction, otter_gc::OutOfMemory> {
    let mut external_visit = |visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
        visit_raw_and_value_roots(visitor, raw_roots, value_roots, slice_roots);
    };
    NativeFunction::from_call_with_roots(heap, name, length, call, &mut external_visit)
}

/// Mutator-bound builder for an ordinary object.
pub struct ObjectBuilder<'rt> {
    heap: &'rt mut otter_gc::GcHeap,
    object: JsObject,
    raw_roots: Vec<*mut otter_gc::raw::RawGc>,
    value_roots: Vec<Value>,
    _not_send_sync: PhantomData<Rc<()>>,
}

impl<'rt> ObjectBuilder<'rt> {
    /// Allocate a fresh object and bind the builder to `heap`.
    pub fn new(heap: &'rt mut otter_gc::GcHeap) -> Result<Self, otter_gc::OutOfMemory> {
        let object = alloc_object_with_roots(heap, &[], &[])?;
        Ok(Self {
            heap,
            object,
            raw_roots: Vec::new(),
            value_roots: Vec::new(),
            _not_send_sync: PhantomData,
        })
    }

    /// Allocate a fresh host/runtime-owned object through interpreter runtime
    /// roots and bind a builder to it.
    pub fn new_runtime_rooted(
        interp: &'rt mut crate::Interpreter,
    ) -> Result<Self, otter_gc::OutOfMemory> {
        let raw_roots = interp.collect_runtime_roots();
        let object = interp.alloc_host_object_with_roots(&[], &[])?;
        Ok(ObjectBuilder::<'rt>::from_object_with_raw_and_value_roots(
            interp.gc_heap_mut(),
            object,
            raw_roots,
            Vec::new(),
        ))
    }

    /// Allocate a fresh object through a native context.
    pub fn new_in_ctx<'a>(
        ctx: &'a mut NativeCtx<'_>,
    ) -> Result<ObjectBuilder<'a>, otter_gc::OutOfMemory> {
        let raw_roots = ctx.collect_native_roots();
        let mut value_roots = vec![*ctx.this_value()];
        if let Some(new_target) = ctx.new_target() {
            value_roots.push(*new_target);
        }
        let object = ctx.alloc_object()?;
        Ok(ObjectBuilder::<'a>::from_object_with_raw_and_value_roots(
            ctx.heap_mut(),
            object,
            raw_roots,
            value_roots,
        ))
    }

    /// Bind a builder to an existing object through a native context,
    /// preserving the native root set for later method/accessor allocation.
    #[must_use]
    pub fn from_object_in_ctx<'a>(
        ctx: &'a mut NativeCtx<'_>,
        object: JsObject,
    ) -> ObjectBuilder<'a> {
        let raw_roots = ctx.collect_native_roots();
        let mut value_roots = vec![*ctx.this_value()];
        if let Some(new_target) = ctx.new_target() {
            value_roots.push(*new_target);
        }
        ObjectBuilder::<'a>::from_object_with_raw_and_value_roots(
            ctx.heap_mut(),
            object,
            raw_roots,
            value_roots,
        )
    }

    /// Bind a builder to an existing object.
    #[must_use]
    pub fn from_object(heap: &'rt mut otter_gc::GcHeap, object: JsObject) -> Self {
        Self {
            heap,
            object,
            raw_roots: Vec::new(),
            value_roots: Vec::new(),
            _not_send_sync: PhantomData,
        }
    }

    /// Bind a builder to an existing object while carrying additional
    /// bootstrap/runtime roots across method allocation.
    #[must_use]
    pub fn from_object_with_value_roots(
        heap: &'rt mut otter_gc::GcHeap,
        object: JsObject,
        value_roots: Vec<Value>,
    ) -> Self {
        Self {
            heap,
            object,
            raw_roots: Vec::new(),
            value_roots,
            _not_send_sync: PhantomData,
        }
    }

    /// Bind a builder to an existing object while carrying raw runtime root
    /// slots and owned value roots across method/accessor allocation.
    #[must_use]
    pub fn from_object_with_raw_and_value_roots(
        heap: &'rt mut otter_gc::GcHeap,
        object: JsObject,
        raw_roots: Vec<*mut otter_gc::raw::RawGc>,
        value_roots: Vec<Value>,
    ) -> Self {
        Self {
            heap,
            object,
            raw_roots,
            value_roots,
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
        let object_root = Value::object(self.object);
        let mut roots = Vec::with_capacity(self.value_roots.len() + 1);
        roots.push(&object_root);
        roots.extend(self.value_roots.iter());
        let native = native_from_call_with_raw_roots(
            self.heap,
            name,
            length,
            call,
            self.raw_roots.as_slice(),
            roots.as_slice(),
            &[],
        )?;
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
        let object_root = Value::object(self.object);
        let mut roots = Vec::with_capacity(self.value_roots.len() + 1);
        roots.push(&object_root);
        roots.extend(self.value_roots.iter());
        let getter = match &spec.get {
            Some(call) => Some(Value::NativeFunction(native_from_call_with_raw_roots(
                self.heap,
                spec.name,
                0,
                call.clone(),
                self.raw_roots.as_slice(),
                roots.as_slice(),
                &[],
            )?)),
            None => None,
        };
        let getter_root = getter.unwrap_or(Value::undefined());
        let mut setter_roots = Vec::with_capacity(self.value_roots.len() + 2);
        setter_roots.push(&object_root);
        setter_roots.push(&getter_root);
        setter_roots.extend(self.value_roots.iter());
        let setter = match &spec.set {
            Some(call) => Some(Value::NativeFunction(native_from_call_with_raw_roots(
                self.heap,
                spec.name,
                1,
                call.clone(),
                self.raw_roots.as_slice(),
                setter_roots.as_slice(),
                &[],
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

/// Mutator-bound builder for constructor-shaped objects.
pub struct ConstructorBuilder<'rt> {
    heap: &'rt mut otter_gc::GcHeap,
    spec: &'static ConstructorSpec,
    raw_roots: Vec<*mut otter_gc::raw::RawGc>,
    value_roots: Vec<Value>,
    _not_send_sync: PhantomData<Rc<()>>,
}

impl<'rt> ConstructorBuilder<'rt> {
    /// Start from a static constructor spec.
    #[must_use]
    pub fn from_spec(heap: &'rt mut otter_gc::GcHeap, spec: &'static ConstructorSpec) -> Self {
        Self {
            heap,
            spec,
            raw_roots: Vec::new(),
            value_roots: Vec::new(),
            _not_send_sync: PhantomData,
        }
    }

    /// Start from a static constructor spec while carrying roots through
    /// object/native-function allocation.
    #[must_use]
    pub fn from_spec_with_raw_and_value_roots(
        heap: &'rt mut otter_gc::GcHeap,
        spec: &'static ConstructorSpec,
        raw_roots: Vec<*mut otter_gc::raw::RawGc>,
        value_roots: Vec<Value>,
    ) -> Self {
        Self {
            heap,
            spec,
            raw_roots,
            value_roots,
            _not_send_sync: PhantomData,
        }
    }

    /// Build a constructor object with `.prototype` and method bags.
    pub fn build(self) -> Result<JsObject, JsSurfaceError> {
        let root_refs: Vec<&Value> = self.value_roots.iter().collect();
        let ctor = alloc_object_with_raw_roots(
            self.heap,
            self.raw_roots.as_slice(),
            root_refs.as_slice(),
            &[],
        )?;
        let ctor_root = Value::object(ctor);
        let mut proto_roots = Vec::with_capacity(root_refs.len() + 1);
        proto_roots.push(&ctor_root);
        proto_roots.extend(root_refs.iter().copied());
        let proto = alloc_object_with_raw_roots(
            self.heap,
            self.raw_roots.as_slice(),
            proto_roots.as_slice(),
            &[],
        )?;
        let proto_root = Value::object(proto);
        let mut function_roots = Vec::with_capacity(root_refs.len() + 2);
        function_roots.push(&ctor_root);
        function_roots.push(&proto_root);
        function_roots.extend(root_refs.iter().copied());
        let function = native_from_call_with_raw_roots(
            self.heap,
            self.spec.name,
            self.spec.length,
            self.spec.call.clone(),
            self.raw_roots.as_slice(),
            function_roots.as_slice(),
            &[],
        )?;
        let function_root = Value::native_function(function);
        define_data(
            ctor,
            self.heap,
            "call",
            function_root,
            Attr::builtin_function(),
        )?;
        define_data(
            ctor,
            self.heap,
            "prototype",
            Value::Object(proto),
            Attr::data(),
        )?;
        let mut builder_roots = Vec::with_capacity(self.value_roots.len() + 3);
        builder_roots.push(ctor_root);
        builder_roots.push(proto_root);
        builder_roots.push(function_root);
        builder_roots.extend(self.value_roots);
        for method in self.spec.static_methods {
            ObjectBuilder::from_object_with_raw_and_value_roots(
                self.heap,
                ctor,
                self.raw_roots.clone(),
                builder_roots.clone(),
            )
            .method_from_spec(method)?;
        }
        for method in self.spec.prototype_methods {
            ObjectBuilder::from_object_with_raw_and_value_roots(
                self.heap,
                proto,
                self.raw_roots.clone(),
                builder_roots.clone(),
            )
            .method_from_spec(method)?;
        }
        Ok(ctor)
    }
}

/// Mutator-bound builder for class-shaped specs.
pub struct ClassBuilder<'rt> {
    heap: &'rt mut otter_gc::GcHeap,
    spec: &'static ClassSpec,
    raw_roots: Vec<*mut otter_gc::raw::RawGc>,
    value_roots: Vec<Value>,
    _not_send_sync: PhantomData<Rc<()>>,
}

impl<'rt> ClassBuilder<'rt> {
    /// Start from a static class spec.
    #[must_use]
    pub fn from_spec(heap: &'rt mut otter_gc::GcHeap, spec: &'static ClassSpec) -> Self {
        Self {
            heap,
            spec,
            raw_roots: Vec::new(),
            value_roots: Vec::new(),
            _not_send_sync: PhantomData,
        }
    }

    /// Start from a static class spec while carrying roots through
    /// constructor/prototype/static method allocation.
    #[must_use]
    pub fn from_spec_with_raw_and_value_roots(
        heap: &'rt mut otter_gc::GcHeap,
        spec: &'static ClassSpec,
        raw_roots: Vec<*mut otter_gc::raw::RawGc>,
        value_roots: Vec<Value>,
    ) -> Self {
        Self {
            heap,
            spec,
            raw_roots,
            value_roots,
            _not_send_sync: PhantomData,
        }
    }

    /// Build the class constructor value.
    pub fn build(self) -> Result<Value, JsSurfaceError> {
        let root_refs: Vec<&Value> = self.value_roots.iter().collect();
        let prototype = alloc_object_with_raw_roots(
            self.heap,
            self.raw_roots.as_slice(),
            root_refs.as_slice(),
            &[],
        )?;
        let prototype_root = Value::object(prototype);
        let mut statics_roots = Vec::with_capacity(root_refs.len() + 1);
        statics_roots.push(&prototype_root);
        statics_roots.extend(root_refs.iter().copied());
        let statics = alloc_object_with_raw_roots(
            self.heap,
            self.raw_roots.as_slice(),
            statics_roots.as_slice(),
            &[],
        )?;
        let statics_root = Value::object(statics);
        let mut constructor_roots = Vec::with_capacity(root_refs.len() + 2);
        constructor_roots.push(&prototype_root);
        constructor_roots.push(&statics_root);
        constructor_roots.extend(root_refs.iter().copied());
        let constructor = Value::native_function(native_from_call_with_raw_roots(
            self.heap,
            self.spec.constructor.name,
            self.spec.constructor.length,
            self.spec.constructor.call.clone(),
            self.raw_roots.as_slice(),
            constructor_roots.as_slice(),
            &[],
        )?);
        let mut builder_roots = Vec::with_capacity(self.value_roots.len() + 3);
        builder_roots.push(prototype_root);
        builder_roots.push(statics_root);
        builder_roots.push(constructor);
        builder_roots.extend(self.value_roots);

        {
            let mut static_builder = ObjectBuilder::from_object_with_raw_and_value_roots(
                self.heap,
                statics,
                self.raw_roots.clone(),
                builder_roots.clone(),
            );
            for method in self.spec.constructor.static_methods {
                static_builder.method_from_spec(method)?;
            }
        }

        {
            let mut prototype_builder = ObjectBuilder::from_object_with_raw_and_value_roots(
                self.heap,
                prototype,
                self.raw_roots.clone(),
                builder_roots.clone(),
            );
            for method in self.spec.constructor.prototype_methods {
                prototype_builder.method_from_spec(method)?;
            }
            for accessor in self.spec.prototype_accessors {
                prototype_builder.accessor_from_spec(accessor)?;
            }
        }

        let class_roots: Vec<&Value> = builder_roots.iter().collect();
        let mut external_visit = |visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
            visit_raw_and_value_roots(
                visitor,
                self.raw_roots.as_slice(),
                class_roots.as_slice(),
                &[],
            );
        };
        let class = Value::class_constructor(ClassConstructor::new_with_roots(
            self.heap,
            constructor,
            prototype,
            statics,
            &mut external_visit,
        )?);
        define_data(
            prototype,
            self.heap,
            "constructor",
            class,
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
    raw_roots: Vec<*mut otter_gc::raw::RawGc>,
    value_roots: Vec<Value>,
    _not_send_sync: PhantomData<Rc<()>>,
}

impl<'rt> NamespaceBuilder<'rt> {
    /// Allocate a namespace object from a static spec.
    pub fn from_spec(
        heap: &'rt mut otter_gc::GcHeap,
        spec: &'static NamespaceSpec,
    ) -> Result<Self, otter_gc::OutOfMemory> {
        let object = alloc_object_with_roots(heap, &[], &[])?;
        Ok(Self {
            heap,
            spec,
            object,
            raw_roots: Vec::new(),
            value_roots: Vec::new(),
            _not_send_sync: PhantomData,
        })
    }

    /// Allocate a namespace object from a static spec while carrying
    /// bootstrap/runtime roots across method/accessor allocation.
    pub fn from_spec_with_value_roots(
        heap: &'rt mut otter_gc::GcHeap,
        spec: &'static NamespaceSpec,
        value_roots: Vec<Value>,
    ) -> Result<Self, otter_gc::OutOfMemory> {
        let root_refs: Vec<&Value> = value_roots.iter().collect();
        let object = alloc_object_with_roots(heap, root_refs.as_slice(), &[])?;
        Ok(Self {
            heap,
            spec,
            object,
            raw_roots: Vec::new(),
            value_roots,
            _not_send_sync: PhantomData,
        })
    }

    /// Allocate a namespace object through a native context.
    pub fn from_spec_in_ctx<'a>(
        ctx: &'a mut NativeCtx<'_>,
        spec: &'static NamespaceSpec,
    ) -> Result<NamespaceBuilder<'a>, otter_gc::OutOfMemory> {
        let raw_roots = ctx.collect_native_roots();
        let mut value_roots = vec![*ctx.this_value()];
        if let Some(new_target) = ctx.new_target() {
            value_roots.push(*new_target);
        }
        let object = ctx.alloc_object()?;
        Ok(NamespaceBuilder {
            heap: ctx.heap_mut(),
            spec,
            object,
            raw_roots,
            value_roots,
            _not_send_sync: PhantomData,
        })
    }

    /// Install all constants, methods, and accessors on the object.
    pub fn build(self) -> Result<JsObject, JsSurfaceError> {
        {
            let mut object = ObjectBuilder::from_object_with_raw_and_value_roots(
                self.heap,
                self.object,
                self.raw_roots,
                self.value_roots,
            );
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
        Ok(Value::number(NumberValue::from_i32(1)))
    }

    fn construct(_: &mut NativeCtx<'_>, _: &[Value]) -> Result<Value, NativeError> {
        Ok(Value::undefined())
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

        let Value::NativeFunction(ctor) = &class.ctor(&heap) else {
            panic!("class constructor should use a native function")
        };
        assert!(ctor.is_static_call(&heap));
        assert_eq!(ctor.length(&heap), 1);

        let Value::NativeFunction(static_method) =
            object::get(class.statics(&heap), &heap, "from").expect("static method")
        else {
            panic!("static method should be native")
        };
        assert!(static_method.is_static_call(&heap));
        assert_eq!(static_method.length(&heap), 1);

        let Value::NativeFunction(proto_method) =
            object::get(class.prototype(&heap), &heap, "valueOf").expect("prototype method")
        else {
            panic!("prototype method should be native")
        };
        assert!(proto_method.is_static_call(&heap));
        assert_eq!(proto_method.length(&heap), 0);

        let constructor = object::get(class.prototype(&heap), &heap, "constructor")
            .expect("prototype constructor backlink");
        assert!(matches!(constructor, Value::ClassConstructor(_)));
        let accessor = object::get_own_descriptor(class.prototype(&heap), &heap, "answer")
            .expect("prototype accessor");
        assert!(accessor.is_accessor());
        assert!(!accessor.enumerable());
        assert!(accessor.configurable());
    }
}
