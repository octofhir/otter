//! Runtime-owned intrinsic registry for the new VM.
//!
//! This module defines the lifecycle and root ownership model for intrinsic
//! objects. The implementation is intentionally small: it allocates the first
//! stable root set, tracks lifecycle staging, and exposes explicit root
//! enumeration for future GC integration and builder-driven bootstrap.

use crate::object::{ObjectError, ObjectHandle, ObjectHeap};

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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IntrinsicsError {
    InvalidLifecycleStage,
    Heap(ObjectError),
}

impl core::fmt::Display for IntrinsicsError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::InvalidLifecycleStage => {
                f.write_str("intrinsics lifecycle advanced in an invalid order")
            }
            Self::Heap(error) => write!(f, "intrinsics heap operation failed: {error:?}"),
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
    pub fn init_core(&mut self, _heap: &mut ObjectHeap) -> Result<(), IntrinsicsError> {
        if self.stage != IntrinsicsStage::Wired {
            return Err(IntrinsicsError::InvalidLifecycleStage);
        }
        self.stage = IntrinsicsStage::Initialized;
        Ok(())
    }

    /// Installs initialized intrinsics on the global object.
    pub fn install_on_global(&mut self, _heap: &mut ObjectHeap) -> Result<(), IntrinsicsError> {
        if self.stage != IntrinsicsStage::Initialized {
            return Err(IntrinsicsError::InvalidLifecycleStage);
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

#[cfg(test)]
mod tests {
    use crate::object::HeapValueKind;

    use super::{IntrinsicRoot, IntrinsicsStage, VmIntrinsics, WellKnownSymbol};

    #[test]
    fn intrinsics_bootstrap_advances_through_lifecycle() {
        let mut heap = crate::object::ObjectHeap::new();
        let mut intrinsics = VmIntrinsics::allocate(&mut heap);
        assert_eq!(intrinsics.stage(), IntrinsicsStage::Allocated);

        intrinsics
            .wire_prototype_chains(&mut heap)
            .expect("wiring should succeed");
        assert_eq!(intrinsics.stage(), IntrinsicsStage::Wired);

        intrinsics
            .init_core(&mut heap)
            .expect("init should succeed");
        assert_eq!(intrinsics.stage(), IntrinsicsStage::Initialized);

        intrinsics
            .install_on_global(&mut heap)
            .expect("install should succeed");
        assert_eq!(intrinsics.stage(), IntrinsicsStage::Installed);

        for handle in [
            intrinsics.global_object(),
            intrinsics.object_prototype(),
            intrinsics.function_prototype(),
            intrinsics.object_constructor(),
            intrinsics.function_constructor(),
            intrinsics.array_prototype(),
            intrinsics.string_prototype(),
        ] {
            assert_eq!(heap.kind(handle), Ok(HeapValueKind::Object));
        }
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
