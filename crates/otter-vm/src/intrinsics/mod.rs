//! Runtime-owned intrinsic registry for the new VM.
//!
//! This module defines the lifecycle and root ownership model for intrinsic
//! objects. The implementation is intentionally small: it allocates the first
//! stable root set, tracks lifecycle staging, and exposes explicit root
//! enumeration for future GC integration and builder-driven bootstrap.

mod array_class;
mod boolean_class;
mod function_class;
mod install;
mod math;
mod number_class;
mod object_class;
mod reflect;
mod string_class;

use crate::host::NativeFunctionRegistry;
use crate::object::{ObjectError, ObjectHandle, ObjectHeap};
use crate::property::PropertyNameRegistry;
use install::{IntrinsicInstallContext, IntrinsicInstaller};

/// Stable well-known symbol identifiers owned by the intrinsic registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WellKnownSymbol {
    Iterator,
    AsyncIterator,
    ToStringTag,
    Species,
}

impl WellKnownSymbol {
    /// Returns the stable numeric identifier of the symbol.
    #[must_use]
    pub const fn stable_id(self) -> u64 {
        match self {
            Self::Iterator => 1,
            Self::AsyncIterator => 2,
            Self::ToStringTag => 3,
            Self::Species => 4,
        }
    }

    /// Returns the spec-visible description of the symbol.
    #[must_use]
    pub const fn description(self) -> &'static str {
        match self {
            Self::Iterator => "Symbol.iterator",
            Self::AsyncIterator => "Symbol.asyncIterator",
            Self::ToStringTag => "Symbol.toStringTag",
            Self::Species => "Symbol.species",
        }
    }
}

/// One root entry owned by [`VmIntrinsics`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IntrinsicRoot {
    Object(ObjectHandle),
    Symbol(WellKnownSymbol),
}

/// Shared list of ECMAScript globals installed by the new-VM intrinsic bootstrap.
pub const CORE_INTRINSIC_GLOBAL_NAMES: &[&str] = &[
    "Object", "Function", "Array", "String", "Number", "Boolean", "Math", "Reflect",
];

/// Lifecycle stage of the intrinsic registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IntrinsicsStage {
    Allocated,
    Wired,
    Initialized,
    Installed,
}

/// Errors produced while advancing the intrinsic lifecycle.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum IntrinsicsError {
    InvalidLifecycleStage,
    Heap(ObjectError),
    UnsupportedAccessorInstallation { js_name: Box<str> },
}

impl core::fmt::Display for IntrinsicsError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::InvalidLifecycleStage => {
                f.write_str("intrinsics lifecycle advanced in an invalid order")
            }
            Self::Heap(error) => write!(f, "intrinsics heap operation failed: {error:?}"),
            Self::UnsupportedAccessorInstallation { js_name } => write!(
                f,
                "intrinsics bootstrap does not yet install accessor member '{js_name}'"
            ),
        }
    }
}

impl std::error::Error for IntrinsicsError {}

impl From<ObjectError> for IntrinsicsError {
    fn from(value: ObjectError) -> Self {
        Self::Heap(value)
    }
}

/// Runtime-owned handles to intrinsic root objects and well-known symbols.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VmIntrinsics {
    stage: IntrinsicsStage,
    global_object: ObjectHandle,
    math_namespace: Option<ObjectHandle>,
    object_prototype: ObjectHandle,
    function_prototype: ObjectHandle,
    string_constructor: ObjectHandle,
    number_constructor: ObjectHandle,
    boolean_constructor: ObjectHandle,
    object_constructor: ObjectHandle,
    function_constructor: ObjectHandle,
    array_constructor: ObjectHandle,
    array_prototype: ObjectHandle,
    string_prototype: ObjectHandle,
    number_prototype: ObjectHandle,
    boolean_prototype: ObjectHandle,
    namespace_roots: Vec<ObjectHandle>,
    reflect_namespace: Option<ObjectHandle>,
    well_known_symbols: [WellKnownSymbol; 4],
}

impl VmIntrinsics {
    /// Allocates the minimal intrinsic root set.
    pub fn allocate(heap: &mut ObjectHeap) -> Self {
        let global_object = heap.alloc_object();
        let object_prototype = heap.alloc_object();
        let function_prototype = heap.alloc_object();
        let string_constructor = heap.alloc_object();
        let number_constructor = heap.alloc_object();
        let boolean_constructor = heap.alloc_object();
        let object_constructor = heap.alloc_object();
        let function_constructor = heap.alloc_object();
        let array_constructor = heap.alloc_object();
        let array_prototype = heap.alloc_object();
        let string_prototype = heap.alloc_object();
        let number_prototype = heap.alloc_object();
        let boolean_prototype = heap.alloc_object();

        Self {
            stage: IntrinsicsStage::Allocated,
            global_object,
            math_namespace: None,
            object_prototype,
            function_prototype,
            string_constructor,
            number_constructor,
            boolean_constructor,
            object_constructor,
            function_constructor,
            array_constructor,
            array_prototype,
            string_prototype,
            number_prototype,
            boolean_prototype,
            namespace_roots: Vec::new(),
            reflect_namespace: None,
            well_known_symbols: [
                WellKnownSymbol::Iterator,
                WellKnownSymbol::AsyncIterator,
                WellKnownSymbol::ToStringTag,
                WellKnownSymbol::Species,
            ],
        }
    }

    /// Performs prototype-chain wiring for the allocated intrinsic objects.
    pub fn wire_prototype_chains(&mut self, heap: &mut ObjectHeap) -> Result<(), IntrinsicsError> {
        if self.stage != IntrinsicsStage::Allocated {
            return Err(IntrinsicsError::InvalidLifecycleStage);
        }
        heap.set_prototype(self.global_object, Some(self.object_prototype))?;
        heap.set_prototype(self.object_prototype, None)?;
        heap.set_prototype(self.function_prototype, Some(self.object_prototype))?;
        heap.set_prototype(self.string_constructor, Some(self.function_prototype))?;
        heap.set_prototype(self.number_constructor, Some(self.function_prototype))?;
        heap.set_prototype(self.boolean_constructor, Some(self.function_prototype))?;
        heap.set_prototype(self.object_constructor, Some(self.function_prototype))?;
        heap.set_prototype(self.function_constructor, Some(self.function_prototype))?;
        heap.set_prototype(self.array_constructor, Some(self.function_prototype))?;
        heap.set_prototype(self.array_prototype, Some(self.object_prototype))?;
        heap.set_prototype(self.string_prototype, Some(self.object_prototype))?;
        heap.set_prototype(self.number_prototype, Some(self.object_prototype))?;
        heap.set_prototype(self.boolean_prototype, Some(self.object_prototype))?;
        self.stage = IntrinsicsStage::Wired;
        Ok(())
    }

    /// Populates intrinsic objects with core methods/properties.
    pub fn init_core(
        &mut self,
        heap: &mut ObjectHeap,
        property_names: &mut PropertyNameRegistry,
        native_functions: &mut NativeFunctionRegistry,
    ) -> Result<(), IntrinsicsError> {
        if self.stage != IntrinsicsStage::Wired {
            return Err(IntrinsicsError::InvalidLifecycleStage);
        }

        let mut cx = IntrinsicInstallContext::new(heap, property_names, native_functions);
        for installer in core_installers() {
            installer.init(self, &mut cx)?;
        }

        self.stage = IntrinsicsStage::Initialized;
        Ok(())
    }

    /// Installs initialized intrinsics on the global object.
    pub fn install_on_global(
        &mut self,
        heap: &mut ObjectHeap,
        property_names: &mut PropertyNameRegistry,
        native_functions: &mut NativeFunctionRegistry,
    ) -> Result<(), IntrinsicsError> {
        if self.stage != IntrinsicsStage::Initialized {
            return Err(IntrinsicsError::InvalidLifecycleStage);
        }

        let mut cx = IntrinsicInstallContext::new(heap, property_names, native_functions);
        for installer in core_installers() {
            installer.install_on_global(self, &mut cx)?;
        }

        self.stage = IntrinsicsStage::Installed;
        Ok(())
    }

    /// Returns the current lifecycle stage.
    #[must_use]
    pub const fn stage(&self) -> IntrinsicsStage {
        self.stage
    }

    /// Returns the global object root.
    #[must_use]
    pub const fn global_object(&self) -> ObjectHandle {
        self.global_object
    }

    /// Returns `%Object.prototype%`.
    #[must_use]
    pub const fn object_prototype(&self) -> ObjectHandle {
        self.object_prototype
    }

    /// Returns `%Function.prototype%`.
    #[must_use]
    pub const fn function_prototype(&self) -> ObjectHandle {
        self.function_prototype
    }

    /// Returns `%String%`.
    #[must_use]
    pub const fn string_constructor(&self) -> ObjectHandle {
        self.string_constructor
    }

    /// Returns `%Number%`.
    #[must_use]
    pub const fn number_constructor(&self) -> ObjectHandle {
        self.number_constructor
    }

    /// Returns `%Boolean%`.
    #[must_use]
    pub const fn boolean_constructor(&self) -> ObjectHandle {
        self.boolean_constructor
    }

    /// Returns `%Object%`.
    #[must_use]
    pub const fn object_constructor(&self) -> ObjectHandle {
        self.object_constructor
    }

    /// Returns `%Function%`.
    #[must_use]
    pub const fn function_constructor(&self) -> ObjectHandle {
        self.function_constructor
    }

    /// Returns `%Array%`.
    #[must_use]
    pub const fn array_constructor(&self) -> ObjectHandle {
        self.array_constructor
    }

    /// Returns `%Array.prototype%`.
    #[must_use]
    pub const fn array_prototype(&self) -> ObjectHandle {
        self.array_prototype
    }

    /// Returns `%String.prototype%`.
    #[must_use]
    pub const fn string_prototype(&self) -> ObjectHandle {
        self.string_prototype
    }

    /// Returns `%Number.prototype%`.
    #[must_use]
    pub const fn number_prototype(&self) -> ObjectHandle {
        self.number_prototype
    }

    /// Returns `%Boolean.prototype%`.
    #[must_use]
    pub const fn boolean_prototype(&self) -> ObjectHandle {
        self.boolean_prototype
    }

    /// Registers an additional namespace root owned by the intrinsic registry.
    pub fn register_namespace_root(&mut self, handle: ObjectHandle) {
        self.namespace_roots.push(handle);
    }

    pub(super) fn set_math_namespace(&mut self, handle: ObjectHandle) {
        self.math_namespace = Some(handle);
        self.register_namespace_root(handle);
    }

    pub(super) fn math_namespace(&self) -> Option<ObjectHandle> {
        self.math_namespace
    }

    pub(super) fn set_reflect_namespace(&mut self, handle: ObjectHandle) {
        self.reflect_namespace = Some(handle);
        self.register_namespace_root(handle);
    }

    pub(super) fn reflect_namespace(&self) -> Option<ObjectHandle> {
        self.reflect_namespace
    }

    /// Returns the additional namespace roots.
    #[must_use]
    pub fn namespace_roots(&self) -> &[ObjectHandle] {
        &self.namespace_roots
    }

    /// Returns the stable well-known symbols owned by the intrinsic registry.
    #[must_use]
    pub fn well_known_symbols(&self) -> &[WellKnownSymbol] {
        &self.well_known_symbols
    }

    /// Enumerates all roots owned by the intrinsic registry.
    pub fn trace_roots(&self, tracer: &mut dyn FnMut(IntrinsicRoot)) {
        for handle in [
            self.global_object,
            self.object_prototype,
            self.function_prototype,
            self.string_constructor,
            self.number_constructor,
            self.boolean_constructor,
            self.object_constructor,
            self.function_constructor,
            self.array_constructor,
            self.array_prototype,
            self.string_prototype,
            self.number_prototype,
            self.boolean_prototype,
        ] {
            tracer(IntrinsicRoot::Object(handle));
        }

        for handle in &self.namespace_roots {
            tracer(IntrinsicRoot::Object(*handle));
        }

        for symbol in &self.well_known_symbols {
            tracer(IntrinsicRoot::Symbol(*symbol));
        }
    }
}

fn core_installers() -> [&'static dyn IntrinsicInstaller; 8] {
    [
        &array_class::ARRAY_INTRINSIC as &dyn IntrinsicInstaller,
        &boolean_class::BOOLEAN_INTRINSIC as &dyn IntrinsicInstaller,
        &function_class::FUNCTION_INTRINSIC as &dyn IntrinsicInstaller,
        &math::MATH_INTRINSIC as &dyn IntrinsicInstaller,
        &number_class::NUMBER_INTRINSIC as &dyn IntrinsicInstaller,
        &object_class::OBJECT_INTRINSIC as &dyn IntrinsicInstaller,
        &reflect::REFLECT_INTRINSIC as &dyn IntrinsicInstaller,
        &string_class::STRING_INTRINSIC as &dyn IntrinsicInstaller,
    ]
}

#[cfg(test)]
mod tests {
    use super::{IntrinsicRoot, IntrinsicsStage, VmIntrinsics, WellKnownSymbol};
    use crate::host::NativeFunctionRegistry;
    use crate::object::{HeapValueKind, PropertyValue};
    use crate::property::PropertyNameRegistry;

    #[test]
    fn intrinsics_bootstrap_advances_through_lifecycle() {
        let mut heap = crate::object::ObjectHeap::new();
        let mut intrinsics = VmIntrinsics::allocate(&mut heap);
        let mut property_names = PropertyNameRegistry::new();
        let mut native_functions = NativeFunctionRegistry::new();
        assert_eq!(intrinsics.stage(), IntrinsicsStage::Allocated);

        intrinsics
            .wire_prototype_chains(&mut heap)
            .expect("wiring should succeed");
        assert_eq!(intrinsics.stage(), IntrinsicsStage::Wired);

        intrinsics
            .init_core(&mut heap, &mut property_names, &mut native_functions)
            .expect("init should succeed");
        assert_eq!(intrinsics.stage(), IntrinsicsStage::Initialized);

        intrinsics
            .install_on_global(&mut heap, &mut property_names, &mut native_functions)
            .expect("install should succeed");
        assert_eq!(intrinsics.stage(), IntrinsicsStage::Installed);

        for handle in [
            intrinsics.global_object(),
            intrinsics.object_prototype(),
            intrinsics.function_prototype(),
            intrinsics.array_prototype(),
            intrinsics.string_prototype(),
            intrinsics.number_prototype(),
            intrinsics.boolean_prototype(),
        ] {
            assert_eq!(heap.kind(handle), Ok(HeapValueKind::Object));
        }
        assert_eq!(
            heap.kind(intrinsics.string_constructor()),
            Ok(HeapValueKind::HostFunction)
        );
        assert_eq!(
            heap.kind(intrinsics.number_constructor()),
            Ok(HeapValueKind::HostFunction)
        );
        assert_eq!(
            heap.kind(intrinsics.boolean_constructor()),
            Ok(HeapValueKind::HostFunction)
        );
        assert_eq!(
            heap.kind(intrinsics.object_constructor()),
            Ok(HeapValueKind::HostFunction)
        );
        assert_eq!(
            heap.kind(intrinsics.function_constructor()),
            Ok(HeapValueKind::HostFunction)
        );
        assert_eq!(
            heap.kind(intrinsics.array_constructor()),
            Ok(HeapValueKind::HostFunction)
        );

        assert_eq!(intrinsics.namespace_roots().len(), 2);
        assert_eq!(native_functions.len(), 23);
        assert_eq!(
            heap.get_prototype(intrinsics.global_object()),
            Ok(Some(intrinsics.object_prototype()))
        );
        assert_eq!(heap.get_prototype(intrinsics.object_prototype()), Ok(None));
        assert_eq!(
            heap.get_prototype(intrinsics.function_prototype()),
            Ok(Some(intrinsics.object_prototype()))
        );
        assert_eq!(
            heap.get_prototype(intrinsics.array_constructor()),
            Ok(Some(intrinsics.function_prototype()))
        );
        assert_eq!(
            heap.get_prototype(intrinsics.string_constructor()),
            Ok(Some(intrinsics.function_prototype()))
        );

        let math_property = property_names.intern("Math");
        let math_namespace = heap
            .get_property(intrinsics.global_object(), math_property)
            .expect("global Math lookup should succeed")
            .expect("Math namespace should be installed");
        let math_namespace = math_namespace.value();
        let PropertyValue::Data(math_namespace) = math_namespace else {
            panic!("expected Math to be a data property");
        };
        let math_namespace = math_namespace
            .as_object_handle()
            .map(crate::object::ObjectHandle)
            .expect("Math namespace should be an object");
        assert_eq!(heap.kind(math_namespace), Ok(HeapValueKind::Object));

        let abs_property = property_names.intern("abs");
        let abs = heap
            .get_property(math_namespace, abs_property)
            .expect("Math.abs lookup should succeed")
            .expect("Math.abs should be installed");
        let abs = abs.value();
        let PropertyValue::Data(abs) = abs else {
            panic!("expected Math.abs to be a data property");
        };
        let abs = abs
            .as_object_handle()
            .map(crate::object::ObjectHandle)
            .expect("Math.abs should be an object");
        assert_eq!(heap.kind(abs), Ok(HeapValueKind::HostFunction));
        assert_eq!(
            heap.get_prototype(abs),
            Ok(Some(intrinsics.function_prototype()))
        );
        let to_string_property = property_names.intern("toString");
        let to_string = heap
            .get_property(abs, to_string_property)
            .expect("Function.prototype.toString lookup should succeed")
            .expect("Function.prototype.toString should be inherited");
        assert_eq!(to_string.owner(), intrinsics.function_prototype());

        let object_property = property_names.intern("Object");
        let object_constructor = heap
            .get_property(intrinsics.global_object(), object_property)
            .expect("global Object lookup should succeed")
            .expect("Object constructor should be installed");
        let object_constructor = object_constructor.value();
        let PropertyValue::Data(object_constructor) = object_constructor else {
            panic!("expected Object to be a data property");
        };
        let object_constructor = object_constructor
            .as_object_handle()
            .map(crate::object::ObjectHandle)
            .expect("Object should be an object");
        assert_eq!(
            object_constructor,
            intrinsics.object_constructor(),
            "global Object should point at the intrinsic constructor handle"
        );

        let prototype_property = property_names.intern("prototype");
        let prototype = heap
            .get_property(object_constructor, prototype_property)
            .expect("Object.prototype lookup should succeed")
            .expect("Object.prototype should be installed");
        let prototype = prototype.value();
        let PropertyValue::Data(prototype) = prototype else {
            panic!("expected Object.prototype to be a data property");
        };
        let prototype = prototype
            .as_object_handle()
            .map(crate::object::ObjectHandle)
            .expect("Object.prototype should be an object");
        assert_eq!(prototype, intrinsics.object_prototype());

        let function_property = property_names.intern("Function");
        let function_constructor = heap
            .get_property(intrinsics.global_object(), function_property)
            .expect("global Function lookup should succeed")
            .expect("Function constructor should be installed");
        let function_constructor = function_constructor.value();
        let PropertyValue::Data(function_constructor) = function_constructor else {
            panic!("expected Function to be a data property");
        };
        let function_constructor = function_constructor
            .as_object_handle()
            .map(crate::object::ObjectHandle)
            .expect("Function should be an object");
        assert_eq!(
            function_constructor,
            intrinsics.function_constructor(),
            "global Function should point at the intrinsic constructor handle"
        );

        let function_prototype = heap
            .get_property(function_constructor, prototype_property)
            .expect("Function.prototype lookup should succeed")
            .expect("Function.prototype should be installed");
        let function_prototype = function_prototype.value();
        let PropertyValue::Data(function_prototype) = function_prototype else {
            panic!("expected Function.prototype to be a data property");
        };
        let function_prototype = function_prototype
            .as_object_handle()
            .map(crate::object::ObjectHandle)
            .expect("Function.prototype should be an object");
        assert_eq!(function_prototype, intrinsics.function_prototype());

        let array_property = property_names.intern("Array");
        let array_constructor = heap
            .get_property(intrinsics.global_object(), array_property)
            .expect("global Array lookup should succeed")
            .expect("Array constructor should be installed");
        let array_constructor = array_constructor.value();
        let PropertyValue::Data(array_constructor) = array_constructor else {
            panic!("expected Array to be a data property");
        };
        let array_constructor = array_constructor
            .as_object_handle()
            .map(crate::object::ObjectHandle)
            .expect("Array should be an object");
        assert_eq!(array_constructor, intrinsics.array_constructor());

        let array_prototype = heap
            .get_property(array_constructor, prototype_property)
            .expect("Array.prototype lookup should succeed")
            .expect("Array.prototype should be installed");
        let array_prototype = array_prototype.value();
        let PropertyValue::Data(array_prototype) = array_prototype else {
            panic!("expected Array.prototype to be a data property");
        };
        let array_prototype = array_prototype
            .as_object_handle()
            .map(crate::object::ObjectHandle)
            .expect("Array.prototype should be an object");
        assert_eq!(array_prototype, intrinsics.array_prototype());

        let push_property = property_names.intern("push");
        let push = heap
            .get_property(array_prototype, push_property)
            .expect("Array.prototype.push lookup should succeed")
            .expect("Array.prototype.push should be installed");
        let push = push.value();
        let PropertyValue::Data(push) = push else {
            panic!("expected Array.prototype.push to be a data property");
        };
        let push = push
            .as_object_handle()
            .map(crate::object::ObjectHandle)
            .expect("Array.prototype.push should be an object");
        assert_eq!(heap.kind(push), Ok(HeapValueKind::HostFunction));

        let string_property = property_names.intern("String");
        let string_constructor = heap
            .get_property(intrinsics.global_object(), string_property)
            .expect("global String lookup should succeed")
            .expect("String constructor should be installed");
        let string_constructor = string_constructor.value();
        let PropertyValue::Data(string_constructor) = string_constructor else {
            panic!("expected String to be a data property");
        };
        let string_constructor = string_constructor
            .as_object_handle()
            .map(crate::object::ObjectHandle)
            .expect("String should be an object");
        assert_eq!(string_constructor, intrinsics.string_constructor());

        let string_prototype = heap
            .get_property(string_constructor, prototype_property)
            .expect("String.prototype lookup should succeed")
            .expect("String.prototype should be installed");
        let string_prototype = string_prototype.value();
        let PropertyValue::Data(string_prototype) = string_prototype else {
            panic!("expected String.prototype to be a data property");
        };
        let string_prototype = string_prototype
            .as_object_handle()
            .map(crate::object::ObjectHandle)
            .expect("String.prototype should be an object");
        assert_eq!(string_prototype, intrinsics.string_prototype());

        let string_value_of_property = property_names.intern("valueOf");
        let string_value_of = heap
            .get_property(string_prototype, string_value_of_property)
            .expect("String.prototype.valueOf lookup should succeed")
            .expect("String.prototype.valueOf should be installed");
        assert_eq!(string_value_of.owner(), intrinsics.string_prototype());

        let number_property = property_names.intern("Number");
        let number_constructor = heap
            .get_property(intrinsics.global_object(), number_property)
            .expect("global Number lookup should succeed")
            .expect("Number constructor should be installed");
        let number_constructor = number_constructor.value();
        let PropertyValue::Data(number_constructor) = number_constructor else {
            panic!("expected Number to be a data property");
        };
        let number_constructor = number_constructor
            .as_object_handle()
            .map(crate::object::ObjectHandle)
            .expect("Number should be an object");
        assert_eq!(number_constructor, intrinsics.number_constructor());

        let boolean_property = property_names.intern("Boolean");
        let boolean_constructor = heap
            .get_property(intrinsics.global_object(), boolean_property)
            .expect("global Boolean lookup should succeed")
            .expect("Boolean constructor should be installed");
        let boolean_constructor = boolean_constructor.value();
        let PropertyValue::Data(boolean_constructor) = boolean_constructor else {
            panic!("expected Boolean to be a data property");
        };
        let boolean_constructor = boolean_constructor
            .as_object_handle()
            .map(crate::object::ObjectHandle)
            .expect("Boolean should be an object");
        assert_eq!(boolean_constructor, intrinsics.boolean_constructor());

        let reflect_property = property_names.intern("Reflect");
        let reflect_namespace = heap
            .get_property(intrinsics.global_object(), reflect_property)
            .expect("global Reflect lookup should succeed")
            .expect("Reflect namespace should be installed");
        let reflect_namespace = reflect_namespace.value();
        let PropertyValue::Data(reflect_namespace) = reflect_namespace else {
            panic!("expected Reflect to be a data property");
        };
        let reflect_namespace = reflect_namespace
            .as_object_handle()
            .map(crate::object::ObjectHandle)
            .expect("Reflect should be an object");
        assert_eq!(heap.kind(reflect_namespace), Ok(HeapValueKind::Object));
    }

    #[test]
    fn intrinsics_trace_roots_covers_objects_and_symbols() {
        let mut heap = crate::object::ObjectHeap::new();
        let mut intrinsics = VmIntrinsics::allocate(&mut heap);
        let namespace = heap.alloc_object();
        intrinsics.register_namespace_root(namespace);

        let mut seen = Vec::new();
        intrinsics.trace_roots(&mut |root| seen.push(root));

        assert!(seen.contains(&IntrinsicRoot::Object(intrinsics.global_object())));
        assert!(seen.contains(&IntrinsicRoot::Object(namespace)));
        assert!(seen.contains(&IntrinsicRoot::Symbol(WellKnownSymbol::Iterator)));
        assert!(seen.contains(&IntrinsicRoot::Symbol(WellKnownSymbol::AsyncIterator)));
        assert!(seen.contains(&IntrinsicRoot::Symbol(WellKnownSymbol::ToStringTag)));
        assert!(seen.contains(&IntrinsicRoot::Symbol(WellKnownSymbol::Species)));
    }
}
