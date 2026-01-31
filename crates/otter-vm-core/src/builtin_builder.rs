//! Builder pattern for creating spec-correct builtin constructors and prototypes.
//!
//! Inspired by Boa's `BuiltInBuilder`, this ensures that all builtin methods
//! get the correct property attributes (non-enumerable) and that function
//! objects have proper `length` and `name` properties.
//!
//! ## Usage
//!
//! ```ignore
//! let (ctor, proto) = BuiltInBuilder::new(mm, fn_proto, obj_proto, "Array")
//!     .method("push", array_push_fn, 1)
//!     .method("pop", array_pop_fn, 0)
//!     .static_method("isArray", array_is_array_fn, 1)
//!     .static_method("from", array_from_fn, 1)
//!     .build();
//! ```

use std::sync::Arc;

use crate::error::VmError;
use crate::gc::GcRef;
use crate::memory::MemoryManager;
use crate::object::{JsObject, PropertyAttributes, PropertyDescriptor, PropertyKey};
use crate::string::JsString;
use crate::value::{NativeFn, Value};

/// A deferred property definition to be applied during `build()`.
enum DeferredProperty {
    /// Method on the prototype
    Method {
        name: String,
        func: NativeFn,
        length: u32,
    },
    /// Static method on the constructor
    StaticMethod {
        name: String,
        func: NativeFn,
        length: u32,
    },
    /// Data property on the prototype
    Property {
        key: PropertyKey,
        value: Value,
        attrs: PropertyAttributes,
    },
    /// Data property on the constructor
    StaticProperty {
        key: PropertyKey,
        value: Value,
        attrs: PropertyAttributes,
    },
    /// Getter (and optional setter) on the prototype
    Accessor {
        name: String,
        getter: Option<NativeFn>,
        setter: Option<NativeFn>,
    },
    /// Getter (and optional setter) on the constructor
    StaticAccessor {
        name: String,
        getter: Option<NativeFn>,
        setter: Option<NativeFn>,
    },
    /// Symbol-keyed method on the prototype
    SymbolMethod {
        symbol: Value,
        name: String,
        func: NativeFn,
        length: u32,
    },
}

/// Builder for creating a builtin constructor + prototype pair with
/// spec-correct property attributes.
pub struct BuiltInBuilder {
    mm: Arc<MemoryManager>,
    fn_proto: GcRef<JsObject>,
    constructor: GcRef<JsObject>,
    prototype: GcRef<JsObject>,
    name: String,
    /// Optional parent prototype for the prototype object's [[Prototype]].
    /// Defaults to None (will be set during build if not specified).
    parent_proto: Option<GcRef<JsObject>>,
    /// Optional constructor function implementation.
    /// If None, a default no-op constructor is used.
    ctor_fn: Option<NativeFn>,
    /// Constructor function arity
    ctor_length: u32,
    /// Deferred property definitions
    properties: Vec<DeferredProperty>,
}

impl BuiltInBuilder {
    /// Create a new builder for a builtin with pre-allocated constructor and prototype objects.
    ///
    /// The `constructor` and `prototype` objects should be allocated (possibly empty) before
    /// calling this. This supports the two-stage initialization pattern where objects are
    /// allocated first to break circular dependencies.
    ///
    /// - `mm`: Memory manager
    /// - `fn_proto`: The intrinsic `%Function.prototype%` — used as `[[Prototype]]` for native functions
    /// - `constructor`: Pre-allocated constructor object
    /// - `prototype`: Pre-allocated prototype object
    /// - `name`: Constructor name (e.g., "Array", "Object")
    pub fn new(
        mm: Arc<MemoryManager>,
        fn_proto: GcRef<JsObject>,
        constructor: GcRef<JsObject>,
        prototype: GcRef<JsObject>,
        name: &str,
    ) -> Self {
        Self {
            mm,
            fn_proto,
            constructor,
            prototype,
            name: name.to_string(),
            parent_proto: None,
            ctor_fn: None,
            ctor_length: 0,
            properties: Vec::new(),
        }
    }

    /// Create a new builder that allocates fresh constructor and prototype objects.
    ///
    /// Use this for simple cases where two-stage init is not needed.
    pub fn with_fresh_objects(
        mm: Arc<MemoryManager>,
        fn_proto: GcRef<JsObject>,
        obj_proto: GcRef<JsObject>,
        name: &str,
    ) -> Self {
        let prototype = GcRef::new(JsObject::new(Some(obj_proto), mm.clone()));
        let constructor = GcRef::new(JsObject::new(Some(fn_proto), mm.clone()));
        Self {
            mm,
            fn_proto,
            constructor,
            prototype,
            name: name.to_string(),
            parent_proto: Some(obj_proto),
            ctor_fn: None,
            ctor_length: 0,
            properties: Vec::new(),
        }
    }

    /// Set the prototype's `[[Prototype]]` (e.g., `Object.prototype` for most builtins).
    pub fn inherits(mut self, parent_proto: GcRef<JsObject>) -> Self {
        self.parent_proto = Some(parent_proto);
        self
    }

    /// Set the constructor function implementation and its arity.
    pub fn constructor_fn<F>(mut self, f: F, length: u32) -> Self
    where
        F: Fn(&Value, &[Value], Arc<MemoryManager>) -> Result<Value, VmError>
            + Send
            + Sync
            + 'static,
    {
        self.ctor_fn = Some(Arc::new(f));
        self.ctor_length = length;
        self
    }

    /// Add a method to the prototype.
    ///
    /// The method will have attributes `{ writable: true, enumerable: false, configurable: true }`.
    /// The function object will have `length` and `name` properties set correctly.
    pub fn method<F>(mut self, name: &str, f: F, length: u32) -> Self
    where
        F: Fn(&Value, &[Value], Arc<MemoryManager>) -> Result<Value, VmError>
            + Send
            + Sync
            + 'static,
    {
        self.properties.push(DeferredProperty::Method {
            name: name.to_string(),
            func: Arc::new(f),
            length,
        });
        self
    }

    /// Add a method using a pre-built `NativeFn` Arc.
    pub fn method_native(mut self, name: &str, func: NativeFn, length: u32) -> Self {
        self.properties.push(DeferredProperty::Method {
            name: name.to_string(),
            func,
            length,
        });
        self
    }

    /// Add a static method to the constructor.
    pub fn static_method<F>(mut self, name: &str, f: F, length: u32) -> Self
    where
        F: Fn(&Value, &[Value], Arc<MemoryManager>) -> Result<Value, VmError>
            + Send
            + Sync
            + 'static,
    {
        self.properties.push(DeferredProperty::StaticMethod {
            name: name.to_string(),
            func: Arc::new(f),
            length,
        });
        self
    }

    /// Add a static method using a pre-built `NativeFn` Arc.
    pub fn static_method_native(mut self, name: &str, func: NativeFn, length: u32) -> Self {
        self.properties.push(DeferredProperty::StaticMethod {
            name: name.to_string(),
            func,
            length,
        });
        self
    }

    /// Add a data property to the prototype with explicit attributes.
    pub fn property(mut self, key: PropertyKey, value: Value, attrs: PropertyAttributes) -> Self {
        self.properties.push(DeferredProperty::Property {
            key,
            value,
            attrs,
        });
        self
    }

    /// Add a data property to the constructor with explicit attributes.
    pub fn static_property(
        mut self,
        key: PropertyKey,
        value: Value,
        attrs: PropertyAttributes,
    ) -> Self {
        self.properties.push(DeferredProperty::StaticProperty {
            key,
            value,
            attrs,
        });
        self
    }

    /// Add a getter (and optional setter) to the prototype.
    pub fn accessor(
        mut self,
        name: &str,
        getter: Option<NativeFn>,
        setter: Option<NativeFn>,
    ) -> Self {
        self.properties.push(DeferredProperty::Accessor {
            name: name.to_string(),
            getter,
            setter,
        });
        self
    }

    /// Add a getter (and optional setter) to the constructor.
    pub fn static_accessor(
        mut self,
        name: &str,
        getter: Option<NativeFn>,
        setter: Option<NativeFn>,
    ) -> Self {
        self.properties.push(DeferredProperty::StaticAccessor {
            name: name.to_string(),
            getter,
            setter,
        });
        self
    }

    /// Add a symbol-keyed method to the prototype (e.g., `[Symbol.iterator]`).
    pub fn symbol_method<F>(
        mut self,
        symbol: Value,
        name: &str,
        f: F,
        length: u32,
    ) -> Self
    where
        F: Fn(&Value, &[Value], Arc<MemoryManager>) -> Result<Value, VmError>
            + Send
            + Sync
            + 'static,
    {
        self.properties.push(DeferredProperty::SymbolMethod {
            symbol,
            name: name.to_string(),
            func: Arc::new(f),
            length,
        });
        self
    }

    /// Build the constructor + prototype pair.
    ///
    /// This:
    /// 1. Sets `prototype.[[Prototype]]` to the parent prototype (if specified)
    /// 2. Adds all deferred properties with correct attributes
    /// 3. Wires `constructor.prototype = prototype`
    /// 4. Wires `prototype.constructor = constructor` (non-enumerable)
    /// 5. Sets `constructor.length` and `constructor.name`
    ///
    /// Returns the constructor as a `Value`.
    pub fn build(self) -> Value {
        let BuiltInBuilder {
            mm,
            fn_proto,
            constructor,
            prototype,
            name,
            parent_proto,
            ctor_fn,
            ctor_length,
            properties,
        } = self;

        // 1. Set prototype's [[Prototype]] if specified
        if let Some(parent) = &parent_proto {
            prototype.set_prototype(Some(*parent));
        }

        // 2. Apply all deferred properties
        for prop in properties {
            match prop {
                DeferredProperty::Method {
                    name: method_name,
                    func,
                    length,
                } => {
                    let fn_val = make_native_fn(&mm, fn_proto, func, &method_name, length);
                    prototype.define_property(
                        PropertyKey::string(&method_name),
                        PropertyDescriptor::builtin_method(fn_val),
                    );
                }
                DeferredProperty::StaticMethod {
                    name: method_name,
                    func,
                    length,
                } => {
                    let fn_val = make_native_fn(&mm, fn_proto, func, &method_name, length);
                    constructor.define_property(
                        PropertyKey::string(&method_name),
                        PropertyDescriptor::builtin_method(fn_val),
                    );
                }
                DeferredProperty::Property { key, value, attrs } => {
                    prototype
                        .define_property(key, PropertyDescriptor::data_with_attrs(value, attrs));
                }
                DeferredProperty::StaticProperty { key, value, attrs } => {
                    constructor
                        .define_property(key, PropertyDescriptor::data_with_attrs(value, attrs));
                }
                DeferredProperty::Accessor {
                    name: acc_name,
                    getter,
                    setter,
                } => {
                    let get_val =
                        getter.map(|g| make_native_fn(&mm, fn_proto, g, &format!("get {acc_name}"), 0));
                    let set_val =
                        setter.map(|s| make_native_fn(&mm, fn_proto, s, &format!("set {acc_name}"), 1));
                    prototype.define_property(
                        PropertyKey::string(&acc_name),
                        PropertyDescriptor::Accessor {
                            get: get_val,
                            set: set_val,
                            attributes: PropertyAttributes::builtin_accessor(),
                        },
                    );
                }
                DeferredProperty::StaticAccessor {
                    name: acc_name,
                    getter,
                    setter,
                } => {
                    let get_val =
                        getter.map(|g| make_native_fn(&mm, fn_proto, g, &format!("get {acc_name}"), 0));
                    let set_val =
                        setter.map(|s| make_native_fn(&mm, fn_proto, s, &format!("set {acc_name}"), 1));
                    constructor.define_property(
                        PropertyKey::string(&acc_name),
                        PropertyDescriptor::Accessor {
                            get: get_val,
                            set: set_val,
                            attributes: PropertyAttributes::builtin_accessor(),
                        },
                    );
                }
                DeferredProperty::SymbolMethod {
                    symbol,
                    name: sym_name,
                    func,
                    length,
                } => {
                    let fn_val =
                        make_native_fn(&mm, fn_proto, func, &format!("[{sym_name}]"), length);
                    if let Some(sym) = symbol.as_symbol() {
                        prototype.define_property(
                            PropertyKey::Symbol(sym.id),
                            PropertyDescriptor::builtin_method(fn_val),
                        );
                    }
                }
            }
        }

        // 3. Wire constructor.prototype = prototype (non-enumerable, non-configurable per spec)
        constructor.define_property(
            PropertyKey::string("prototype"),
            PropertyDescriptor::data_with_attrs(
                Value::object(prototype),
                PropertyAttributes {
                    writable: false,
                    enumerable: false,
                    configurable: false,
                },
            ),
        );

        // 4. Wire prototype.constructor = constructor (non-enumerable)
        let ctor_value = if let Some(ctor_fn_impl) = ctor_fn {
            Value::native_function_with_proto_and_object(
                ctor_fn_impl,
                mm.clone(),
                fn_proto,
                constructor,
            )
        } else {
            Value::object(constructor)
        };

        prototype.define_property(
            PropertyKey::string("constructor"),
            PropertyDescriptor::data_with_attrs(
                ctor_value.clone(),
                PropertyAttributes::constructor_link(),
            ),
        );

        // 5. Set constructor.name and constructor.length
        if let Some(ctor_obj) = ctor_value.as_object() {
            ctor_obj.define_property(
                PropertyKey::string("length"),
                PropertyDescriptor::function_length(Value::number(ctor_length as f64)),
            );
            ctor_obj.define_property(
                PropertyKey::string("name"),
                PropertyDescriptor::function_length(Value::string(JsString::intern(&name))),
            );
        }

        ctor_value
    }
}

/// Create a native function value with correct `length` and `name` properties,
/// using `fn_proto` as `[[Prototype]]`.
fn make_native_fn(
    mm: &Arc<MemoryManager>,
    fn_proto: GcRef<JsObject>,
    func: NativeFn,
    name: &str,
    length: u32,
) -> Value {
    let fn_obj = GcRef::new(JsObject::new(Some(fn_proto), mm.clone()));

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

/// Builder for creating namespace objects (like `Math`, `JSON`, `Reflect`).
///
/// Namespace objects are NOT constructors — they are plain objects with methods.
pub struct NamespaceBuilder {
    mm: Arc<MemoryManager>,
    fn_proto: GcRef<JsObject>,
    object: GcRef<JsObject>,
    properties: Vec<DeferredProperty>,
}

impl NamespaceBuilder {
    /// Create a new namespace builder.
    pub fn new(
        mm: Arc<MemoryManager>,
        fn_proto: GcRef<JsObject>,
        object: GcRef<JsObject>,
    ) -> Self {
        Self {
            mm,
            fn_proto,
            object,
            properties: Vec::new(),
        }
    }

    /// Add a method to the namespace object.
    pub fn method<F>(mut self, name: &str, f: F, length: u32) -> Self
    where
        F: Fn(&Value, &[Value], Arc<MemoryManager>) -> Result<Value, VmError>
            + Send
            + Sync
            + 'static,
    {
        self.properties.push(DeferredProperty::Method {
            name: name.to_string(),
            func: Arc::new(f),
            length,
        });
        self
    }

    /// Add a data property to the namespace object.
    pub fn property(mut self, key: PropertyKey, value: Value, attrs: PropertyAttributes) -> Self {
        self.properties.push(DeferredProperty::Property {
            key,
            value,
            attrs,
        });
        self
    }

    /// Build the namespace object, returning it as a `Value`.
    pub fn build(self) -> Value {
        let NamespaceBuilder {
            mm,
            fn_proto,
            object,
            properties,
        } = self;

        for prop in properties {
            match prop {
                DeferredProperty::Method { name, func, length } => {
                    let fn_val = make_native_fn(&mm, fn_proto, func, &name, length);
                    object.define_property(
                        PropertyKey::string(&name),
                        PropertyDescriptor::builtin_method(fn_val),
                    );
                }
                DeferredProperty::Property { key, value, attrs } => {
                    object
                        .define_property(key, PropertyDescriptor::data_with_attrs(value, attrs));
                }
                _ => {} // Namespace objects only support methods and properties
            }
        }
        Value::object(object)
    }
}
