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
//!   a raw-pointer marker so they remain `!Send + !Sync`.
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

use crate::native_function::{NativeCall, NativeFunction};
use crate::number::NumberValue;
use crate::object::{self, JsObject, PropertyDescriptor, PropertyFlags};
use crate::rooting::RootScopeExt;
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
    /// Getter function name — `"get " + name` per §10.2.2 / §15.x so
    /// `Object.getOwnPropertyDescriptor(O, name).get.name` matches the
    /// spec-mandated `"get <name>"`.
    pub get_name: &'static str,
    /// Setter function name — `"set " + name`.
    pub set_name: &'static str,
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

/// A GC-rooted object handle the builder threads into every allocating call it
/// makes.
///
/// The builder reaches its object *only* through this slot. Each native /
/// object allocation the builder drives is handed [`Self::root`] in its
/// external root set, so the collector rewrites the parked `Value` in place on a
/// move; a later read through [`Self::get`] resolves the object's current
/// location and can never dereference a vacated cell, no matter how the
/// builder's allocations are ordered. There is no bare `JsObject` field to
/// desync — this is the internally-enforced form of the post-alloc "refresh the
/// receiver handle" pattern the builder methods used to bolt on by hand.
struct RootedObject {
    /// The object as a rooted `Value`; kept current by every allocation that
    /// threads [`Self::root`].
    slot: Value,
}

impl RootedObject {
    fn new(object: JsObject) -> Self {
        Self {
            slot: Value::object(object),
        }
    }

    /// The object's current location, resolved through the collector-tracked
    /// slot.
    fn get(&self) -> JsObject {
        self.slot
            .as_object()
            .expect("builder object handle stays an object across allocations")
    }

    /// Re-park the handle an in-place define refreshed. A define roots only its
    /// own receiver (not this slot), so its relocation is reflected back here.
    fn store(&mut self, object: JsObject) {
        self.slot = Value::object(object);
    }
}

/// Mutator-bound builder for an ordinary object.
///
/// The builder object lives in a [`RootedObject`] slot that every allocating
/// method threads into its root set, so the object is always resolved through a
/// collector-tracked slot and no builder method can dereference a stale handle
/// even if a future edit reorders the allocations.
///
/// Sibling values the *caller* holds in raw `Value` locals across builder calls
/// are a different matter: the builder keeps them alive (they are traced as
/// `value_roots`) but cannot rewrite the caller's copies. Native code that
/// assembles a value out of several allocations should prefer
/// [`crate::NativeCtx::scope`] and its `scoped_*` methods, where every
/// intermediate handle is parked in the collector-traced handle arena.
pub struct ObjectBuilder<'rt> {
    heap: &'rt mut otter_gc::GcHeap,
    object: RootedObject,
    raw_roots: Vec<*mut otter_gc::raw::RawGc>,
    value_roots: Vec<Value>,
    _not_send_sync: PhantomData<*mut ()>,
}

impl<'rt> ObjectBuilder<'rt> {
    /// Allocate a fresh object and bind the builder to `heap`.
    pub fn new(heap: &'rt mut otter_gc::GcHeap) -> Result<Self, otter_gc::OutOfMemory> {
        let object = alloc_object_with_roots(heap, &[], &[])?;
        Ok(Self {
            heap,
            object: RootedObject::new(object),
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
            object: RootedObject::new(object),
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
            object: RootedObject::new(object),
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
            object: RootedObject::new(object),
            raw_roots,
            value_roots,
            _not_send_sync: PhantomData,
        }
    }

    /// Allocate a static builtin native function. The caller owns one
    /// [`otter_gc::RootScope`] spanning the allocation and the subsequent
    /// property definition, so this helper only has to publish audited raw
    /// runtime slots that cannot be represented as `Value`s.
    fn alloc_native(
        &mut self,
        name: &'static str,
        length: u8,
        call: NativeCall,
    ) -> Result<NativeFunction, JsSurfaceError> {
        native_from_call_with_raw_roots(
            self.heap,
            name,
            length,
            call,
            self.raw_roots.as_slice(),
            &[],
            &[],
        )
        .map_err(JsSurfaceError::from)
    }

    /// Define a data property.
    pub fn property(
        &mut self,
        name: &'static str,
        value: Value,
        attrs: Attr,
    ) -> Result<&mut Self, JsSurfaceError> {
        let mut value = value;
        let mut roots = otter_gc::RootScope::new(self.heap);
        // SAFETY: every registered field/local remains stationary until the
        // property definition completes and the guard is dropped.
        unsafe {
            roots.add_value(&mut self.object.slot);
            roots.add_value_vec(&mut self.value_roots);
            roots.add_value(&mut value);
        }
        let mut object = self.object.get();
        define_data_in_place(&mut object, self.heap, name, value, attrs)?;
        self.object.store(object);
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
        let mut native_root = Value::undefined();
        let mut roots = otter_gc::RootScope::new(self.heap);
        // SAFETY: the builder fields and local root precede this guard and are
        // not moved until both allocation and definition finish.
        unsafe {
            roots.add_value(&mut self.object.slot);
            roots.add_value_vec(&mut self.value_roots);
            roots.add_value(&mut native_root);
        }
        native_root = Value::native_function(self.alloc_native(name, length, call)?);
        let mut object = self.object.get();
        define_data_in_place(&mut object, self.heap, name, native_root, attrs)?;
        self.object.store(object);
        Ok(self)
    }

    /// Define a method from a static spec.
    pub fn method_from_spec(&mut self, spec: &MethodSpec) -> Result<&mut Self, JsSurfaceError> {
        self.method(spec.name, spec.length, spec.call.clone(), spec.attrs)
    }

    /// Define an accessor property.
    pub fn accessor_from_spec(&mut self, spec: &AccessorSpec) -> Result<&mut Self, JsSurfaceError> {
        let mut getter_root = Value::undefined();
        let mut setter_root = Value::undefined();
        let mut roots = otter_gc::RootScope::new(self.heap);
        // SAFETY: builder fields and both locals precede the guard and remain
        // stationary across getter allocation, setter allocation, and the
        // potentially allocating descriptor installation.
        unsafe {
            roots.add_value(&mut self.object.slot);
            roots.add_value_vec(&mut self.value_roots);
            roots.add_value(&mut getter_root);
            roots.add_value(&mut setter_root);
        }
        if let Some(call) = &spec.get {
            getter_root =
                Value::native_function(self.alloc_native(spec.get_name, 0, call.clone())?);
        }
        if let Some(call) = &spec.set {
            setter_root =
                Value::native_function(self.alloc_native(spec.set_name, 1, call.clone())?);
        }
        let getter = spec.get.as_ref().map(|_| getter_root);
        let setter = spec.set.as_ref().map(|_| setter_root);
        let descriptor = PropertyDescriptor::accessor(
            getter,
            setter,
            spec.attrs.enumerable,
            spec.attrs.configurable,
        );
        let mut object = self.object.get();
        if object::define_own_property_in_place(&mut object, self.heap, spec.name, descriptor) {
            self.object.store(object);
            Ok(self)
        } else {
            Err(JsSurfaceError::DefinePropertyFailed(spec.name))
        }
    }

    /// Finish object construction.
    #[must_use]
    pub fn build(self) -> JsObject {
        self.object.get()
    }
}

/// Mutator-bound builder for constructor-shaped objects.
pub struct ConstructorBuilder<'rt> {
    heap: &'rt mut otter_gc::GcHeap,
    spec: &'static ConstructorSpec,
    raw_roots: Vec<*mut otter_gc::raw::RawGc>,
    value_roots: Vec<Value>,
    _not_send_sync: PhantomData<*mut ()>,
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
        let Self {
            heap,
            spec,
            raw_roots,
            mut value_roots,
            _not_send_sync: _,
        } = self;
        let mut ctor_root = Value::undefined();
        let mut proto_root = Value::undefined();
        let mut function_root = Value::undefined();
        let mut roots = otter_gc::RootScope::new(heap);
        // SAFETY: the owned root vector and canonical build slots all precede
        // the guard and remain stationary until the constructor is complete.
        unsafe {
            roots.add_value_vec(&mut value_roots);
            roots.add_value(&mut ctor_root);
            roots.add_value(&mut proto_root);
            roots.add_value(&mut function_root);
        }
        ctor_root = Value::object(alloc_object_with_raw_roots(
            heap,
            raw_roots.as_slice(),
            &[],
            &[],
        )?);
        proto_root = Value::object(alloc_object_with_raw_roots(
            heap,
            raw_roots.as_slice(),
            &[],
            &[],
        )?);
        function_root = Value::native_function(native_from_call_with_raw_roots(
            heap,
            spec.name,
            spec.length,
            spec.call.clone(),
            raw_roots.as_slice(),
            &[],
            &[],
        )?);
        define_data(
            ctor_root
                .as_object()
                .expect("constructor object stays rooted"),
            heap,
            "call",
            function_root,
            Attr::builtin_function(),
        )?;
        define_data(
            ctor_root
                .as_object()
                .expect("constructor object stays rooted"),
            heap,
            "prototype",
            proto_root,
            Attr::data(),
        )?;

        let mut ctor_builder = ObjectBuilder::from_object_with_raw_and_value_roots(
            heap,
            ctor_root
                .as_object()
                .expect("constructor object stays rooted"),
            raw_roots.clone(),
            Vec::new(),
        );
        for method in spec.static_methods {
            ctor_builder.method_from_spec(method)?;
        }
        ctor_root = Value::object(ctor_builder.build());

        let mut proto_builder = ObjectBuilder::from_object_with_raw_and_value_roots(
            heap,
            proto_root
                .as_object()
                .expect("constructor prototype stays rooted"),
            raw_roots,
            Vec::new(),
        );
        for method in spec.prototype_methods {
            proto_builder.method_from_spec(method)?;
        }
        let _ = proto_builder.build();
        Ok(ctor_root
            .as_object()
            .expect("constructor object stays rooted through build"))
    }
}

/// Mutator-bound builder for class-shaped specs.
pub struct ClassBuilder<'rt> {
    heap: &'rt mut otter_gc::GcHeap,
    spec: &'static ClassSpec,
    raw_roots: Vec<*mut otter_gc::raw::RawGc>,
    value_roots: Vec<Value>,
    _not_send_sync: PhantomData<*mut ()>,
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
        let Self {
            heap,
            spec,
            raw_roots,
            mut value_roots,
            _not_send_sync: _,
        } = self;
        let mut prototype_root = Value::undefined();
        let mut statics_root = Value::undefined();
        let mut constructor_root = Value::undefined();
        let mut class_root = Value::undefined();
        let mut roots = otter_gc::RootScope::new(heap);
        // SAFETY: the owned roots and all canonical build slots precede the
        // guard and remain stationary until the class graph is complete.
        unsafe {
            roots.add_value_vec(&mut value_roots);
            roots.add_value(&mut prototype_root);
            roots.add_value(&mut statics_root);
            roots.add_value(&mut constructor_root);
            roots.add_value(&mut class_root);
        }
        prototype_root = Value::object(alloc_object_with_raw_roots(
            heap,
            raw_roots.as_slice(),
            &[],
            &[],
        )?);
        statics_root = Value::object(alloc_object_with_raw_roots(
            heap,
            raw_roots.as_slice(),
            &[],
            &[],
        )?);
        constructor_root = Value::native_function(native_from_call_with_raw_roots(
            heap,
            spec.constructor.name,
            spec.constructor.length,
            spec.constructor.call.clone(),
            raw_roots.as_slice(),
            &[],
            &[],
        )?);

        {
            let mut static_builder = ObjectBuilder::from_object_with_raw_and_value_roots(
                heap,
                statics_root.as_object().expect("class statics stay rooted"),
                raw_roots.clone(),
                Vec::new(),
            );
            for method in spec.constructor.static_methods {
                static_builder.method_from_spec(method)?;
            }
            statics_root = Value::object(static_builder.build());
        }

        {
            let mut prototype_builder = ObjectBuilder::from_object_with_raw_and_value_roots(
                heap,
                prototype_root
                    .as_object()
                    .expect("class prototype stays rooted"),
                raw_roots.clone(),
                Vec::new(),
            );
            for method in spec.constructor.prototype_methods {
                prototype_builder.method_from_spec(method)?;
            }
            for accessor in spec.prototype_accessors {
                prototype_builder.accessor_from_spec(accessor)?;
            }
            prototype_root = Value::object(prototype_builder.build());
        }

        let mut external_visit = |visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
            for &slot in &raw_roots {
                visitor(slot);
            }
        };
        class_root = Value::class_constructor(ClassConstructor::new_with_roots(
            heap,
            constructor_root,
            prototype_root
                .as_object()
                .expect("class prototype stays rooted"),
            statics_root.as_object().expect("class statics stay rooted"),
            &mut external_visit,
        )?);
        define_data(
            prototype_root
                .as_object()
                .expect("class prototype stays rooted"),
            heap,
            "constructor",
            class_root,
            Attr::builtin_function(),
        )?;
        Ok(class_root)
    }
}

/// Mutator-bound builder for a namespace object.
pub struct NamespaceBuilder<'rt> {
    heap: &'rt mut otter_gc::GcHeap,
    spec: &'static NamespaceSpec,
    object: JsObject,
    raw_roots: Vec<*mut otter_gc::raw::RawGc>,
    value_roots: Vec<Value>,
    _not_send_sync: PhantomData<*mut ()>,
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
        // Return the builder's refreshed handle, not `self.object`: the
        // installs above can relocate the namespace object, and the inner
        // builder tracked the move while `self.object` stayed at the old offset.
        Ok(object.build())
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
    let mut object = object;
    define_data_in_place(&mut object, heap, name, value, attrs)
}

/// [`define_data`] that reflects any relocation the write drove back into
/// `object`, so a builder chaining several properties never writes through a
/// stale handle when an intermediate allocation moves the receiver.
fn define_data_in_place(
    object: &mut JsObject,
    heap: &mut otter_gc::GcHeap,
    name: &'static str,
    value: Value,
    attrs: Attr,
) -> Result<(), JsSurfaceError> {
    let descriptor = PropertyDescriptor {
        kind: crate::object::DescriptorKind::Data { value },
        flags: attrs.to_flags(),
    };
    if object::define_own_property_in_place(object, heap, name, descriptor) {
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
        get_name: "get answer",
        set_name: "set answer",
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
        let native = method
            .as_native_function()
            .expect("method should be native");
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
        let class = class
            .as_class_constructor()
            .expect("class builder should produce a class constructor value");

        let ctor = class
            .ctor(&heap)
            .as_native_function()
            .expect("class constructor should use a native function");
        assert!(ctor.is_static_call(&heap));
        assert_eq!(ctor.length(&heap), 1);

        let static_method = object::get(class.statics(&heap), &heap, "from")
            .expect("static method")
            .as_native_function()
            .expect("static method should be native");
        assert!(static_method.is_static_call(&heap));
        assert_eq!(static_method.length(&heap), 1);

        let proto_method = object::get(class.prototype(&heap), &heap, "valueOf")
            .expect("prototype method")
            .as_native_function()
            .expect("prototype method should be native");
        assert!(proto_method.is_static_call(&heap));
        assert_eq!(proto_method.length(&heap), 0);

        let constructor = object::get(class.prototype(&heap), &heap, "constructor")
            .expect("prototype constructor backlink");
        assert!(constructor.is_class_constructor());
        let accessor = object::get_own_descriptor(class.prototype(&heap), &heap, "answer")
            .expect("prototype accessor");
        assert!(accessor.is_accessor());
        assert!(!accessor.enumerable());
        assert!(accessor.configurable());
    }
}
