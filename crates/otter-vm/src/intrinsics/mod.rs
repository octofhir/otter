//! Runtime-owned intrinsic registry for the new VM.
//!
//! This module defines the lifecycle and root ownership model for intrinsic
//! objects. The implementation is intentionally small: it allocates the first
//! stable root set, tracks lifecycle staging, and exposes explicit root
//! enumeration for future GC integration and builder-driven bootstrap.

mod function_class;
mod install;
mod math;
mod object_class;

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
    object_constructor: ObjectHandle,
    function_constructor: ObjectHandle,
    array_prototype: ObjectHandle,
    string_prototype: ObjectHandle,
    namespace_roots: Vec<ObjectHandle>,
    well_known_symbols: [WellKnownSymbol; 4],
}

impl VmIntrinsics {
    /// Allocates the minimal intrinsic root set.
    pub fn allocate(heap: &mut ObjectHeap) -> Self {
        let global_object = heap.alloc_object();
        let object_prototype = heap.alloc_object();
        let function_prototype = heap.alloc_object();
        let object_constructor = heap.alloc_object();
        let function_constructor = heap.alloc_object();
        let array_prototype = heap.alloc_object();
        let string_prototype = heap.alloc_object();

        Self {
            stage: IntrinsicsStage::Allocated,
            global_object,
            math_namespace: None,
            object_prototype,
            function_prototype,
            object_constructor,
            function_constructor,
            array_prototype,
            string_prototype,
            namespace_roots: Vec::new(),
            well_known_symbols: [
                WellKnownSymbol::Iterator,
                WellKnownSymbol::AsyncIterator,
                WellKnownSymbol::ToStringTag,
                WellKnownSymbol::Species,
            ],
        }
    }

    /// Performs prototype-chain wiring for the allocated intrinsic objects.
    pub fn wire_prototype_chains(&mut self, _heap: &mut ObjectHeap) -> Result<(), IntrinsicsError> {
        if self.stage != IntrinsicsStage::Allocated {
            return Err(IntrinsicsError::InvalidLifecycleStage);
        }
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
            self.object_constructor,
            self.function_constructor,
            self.array_prototype,
            self.string_prototype,
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

fn core_installers() -> [&'static dyn IntrinsicInstaller; 3] {
    [
        &function_class::FUNCTION_INTRINSIC as &dyn IntrinsicInstaller,
        &math::MATH_INTRINSIC as &dyn IntrinsicInstaller,
        &object_class::OBJECT_INTRINSIC as &dyn IntrinsicInstaller,
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
        ] {
            assert_eq!(heap.kind(handle), Ok(HeapValueKind::Object));
        }
        assert_eq!(
            heap.kind(intrinsics.object_constructor()),
            Ok(HeapValueKind::HostFunction)
        );
        assert_eq!(
            heap.kind(intrinsics.function_constructor()),
            Ok(HeapValueKind::HostFunction)
        );

        assert_eq!(intrinsics.namespace_roots().len(), 1);
        assert_eq!(native_functions.len(), 9);

        let math_property = property_names.intern("Math");
        let math_namespace = heap
            .get_property(intrinsics.global_object(), math_property)
            .expect("global Math lookup should succeed")
            .expect("Math namespace should be installed")
            .0;
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
            .expect("Math.abs should be installed")
            .0;
        let PropertyValue::Data(abs) = abs else {
            panic!("expected Math.abs to be a data property");
        };
        let abs = abs
            .as_object_handle()
            .map(crate::object::ObjectHandle)
            .expect("Math.abs should be an object");
        assert_eq!(heap.kind(abs), Ok(HeapValueKind::HostFunction));

        let object_property = property_names.intern("Object");
        let object_constructor = heap
            .get_property(intrinsics.global_object(), object_property)
            .expect("global Object lookup should succeed")
            .expect("Object constructor should be installed")
            .0;
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
            .expect("Object.prototype should be installed")
            .0;
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
            .expect("Function constructor should be installed")
            .0;
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
            .expect("Function.prototype should be installed")
            .0;
        let PropertyValue::Data(function_prototype) = function_prototype else {
            panic!("expected Function.prototype to be a data property");
        };
        let function_prototype = function_prototype
            .as_object_handle()
            .map(crate::object::ObjectHandle)
            .expect("Function.prototype should be an object");
        assert_eq!(function_prototype, intrinsics.function_prototype());
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
