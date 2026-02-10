//! Registration context for extension installation.
//!
//! `RegistrationContext` provides a safe, scoped API for extensions to register
//! native functions, constructors, namespace objects, and module exports during
//! the bootstrap sequence.

use std::sync::Arc;

use otter_vm_core::builtin_builder::{BuiltInBuilder, NamespaceBuilder};
use otter_vm_core::gc::GcRef;
use otter_vm_core::intrinsics::Intrinsics;
use otter_vm_core::memory::MemoryManager;
use otter_vm_core::object::{JsObject, PropertyDescriptor, PropertyKey};
use otter_vm_core::string::JsString;
use otter_vm_core::value::{NativeFn, Value};

use crate::extension_state::ExtensionState;

/// Context passed to extensions during the `allocate()` and `install()` phases.
///
/// Provides access to intrinsics, the global object, memory manager, and
/// extension state, plus builder methods for registering native functions.
pub struct RegistrationContext<'a> {
    intrinsics: &'a Intrinsics,
    global: GcRef<JsObject>,
    mm: Arc<MemoryManager>,
    state: &'a mut ExtensionState,
}

impl<'a> RegistrationContext<'a> {
    /// Create a new registration context.
    pub fn new(
        intrinsics: &'a Intrinsics,
        global: GcRef<JsObject>,
        mm: Arc<MemoryManager>,
        state: &'a mut ExtensionState,
    ) -> Self {
        Self {
            intrinsics,
            global,
            mm,
            state,
        }
    }

    // -----------------------------------------------------------------------
    // Accessors
    // -----------------------------------------------------------------------

    /// Get the intrinsics registry.
    pub fn intrinsics(&self) -> &Intrinsics {
        self.intrinsics
    }

    /// Get the global object.
    pub fn global(&self) -> GcRef<JsObject> {
        self.global
    }

    /// Get the memory manager.
    pub fn mm(&self) -> &Arc<MemoryManager> {
        &self.mm
    }

    /// Get `%Function.prototype%` for creating native function objects.
    pub fn fn_proto(&self) -> GcRef<JsObject> {
        self.intrinsics.function_prototype
    }

    /// Get `%Object.prototype%` for creating plain objects.
    pub fn obj_proto(&self) -> GcRef<JsObject> {
        self.intrinsics.object_prototype
    }

    /// Get extension state (immutable).
    pub fn state(&self) -> &ExtensionState {
        self.state
    }

    /// Get extension state (mutable).
    pub fn state_mut(&mut self) -> &mut ExtensionState {
        self.state
    }

    // -----------------------------------------------------------------------
    // Builder methods
    // -----------------------------------------------------------------------

    /// Create a `BuiltInBuilder` for a constructor/prototype pair.
    ///
    /// Use this for classes like `Buffer`, `EventEmitter`, etc.
    pub fn builtin(
        &self,
        constructor: GcRef<JsObject>,
        prototype: GcRef<JsObject>,
        name: &str,
    ) -> BuiltInBuilder {
        BuiltInBuilder::new(
            self.mm.clone(),
            self.fn_proto(),
            constructor,
            prototype,
            name,
        )
    }

    /// Create a `BuiltInBuilder` with freshly allocated constructor/prototype objects.
    pub fn builtin_fresh(&self, name: &str) -> BuiltInBuilder {
        BuiltInBuilder::with_fresh_objects(self.mm.clone(), self.fn_proto(), self.obj_proto(), name)
    }

    /// Create a `NamespaceBuilder` for a namespace object (like `Math`, `JSON`).
    pub fn namespace(&self, obj: GcRef<JsObject>) -> NamespaceBuilder {
        NamespaceBuilder::new(self.mm.clone(), self.fn_proto(), obj)
    }

    /// Create a fresh plain object for use as a module namespace.
    pub fn new_object(&self) -> GcRef<JsObject> {
        GcRef::new(JsObject::new(
            Value::object(self.obj_proto()),
            self.mm.clone(),
        ))
    }

    /// Create a `ModuleNamespaceBuilder` for building module exports.
    pub fn module_namespace(&self) -> ModuleNamespaceBuilder {
        let obj = self.new_object();
        ModuleNamespaceBuilder {
            mm: self.mm.clone(),
            fn_proto: self.fn_proto(),
            object: obj,
        }
    }

    /// Register a global function on `globalThis`.
    pub fn global_fn(&self, name: &str, f: NativeFn, length: u32) {
        let fn_val = make_module_fn(&self.mm, self.fn_proto(), f, name, length);
        let _ = self.global.set(PropertyKey::string(name), fn_val);
    }

    /// Set a global value on `globalThis`.
    pub fn global_value(&self, name: &str, value: Value) {
        let _ = self.global.set(PropertyKey::string(name), value);
    }
}

/// Builder for constructing module namespace objects.
///
/// A module namespace is a plain object whose properties are the module's exports.
/// Used by `OtterExtension::load_module()` to return a native module.
pub struct ModuleNamespaceBuilder {
    mm: Arc<MemoryManager>,
    fn_proto: GcRef<JsObject>,
    object: GcRef<JsObject>,
}

impl ModuleNamespaceBuilder {
    /// Add a function export.
    pub fn function(self, name: &str, f: NativeFn, length: u32) -> Self {
        let fn_val = make_module_fn(&self.mm, self.fn_proto, f, name, length);
        self.object.define_property(
            PropertyKey::string(name),
            PropertyDescriptor::builtin_method(fn_val),
        );
        self
    }

    /// Add a value export.
    pub fn property(self, name: &str, value: Value) -> Self {
        let _ = self.object.set(PropertyKey::string(name), value);
        self
    }

    /// Build the namespace object, returning the GcRef.
    pub fn build(self) -> GcRef<JsObject> {
        self.object
    }
}

/// Create a native function value with correct `length` and `name` properties.
fn make_module_fn(
    mm: &Arc<MemoryManager>,
    fn_proto: GcRef<JsObject>,
    func: NativeFn,
    name: &str,
    length: u32,
) -> Value {
    let fn_obj = GcRef::new(JsObject::new(Value::object(fn_proto), mm.clone()));

    fn_obj.define_property(
        PropertyKey::string("length"),
        PropertyDescriptor::function_length(Value::number(length as f64)),
    );

    fn_obj.define_property(
        PropertyKey::string("name"),
        PropertyDescriptor::function_length(Value::string(JsString::intern(name))),
    );

    Value::native_function_with_proto_and_object(func, mm.clone(), fn_proto, fn_obj)
}
