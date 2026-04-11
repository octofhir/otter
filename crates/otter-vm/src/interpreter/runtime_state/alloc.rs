//! Heap allocation (objects, arrays, strings, BigInts, RegExps, symbols,
//! host functions), `gc_safepoint`, global / burrow installation, closure
//! allocation, and the `is_ecma_object` / `is_constructible` predicates
//! used for IsCallable / IsConstructor.

use core::any::Any;

use crate::builders::{BurrowBuilder, ObjectMemberPlan};
use crate::descriptors::{NativeFunctionDescriptor, VmNativeCallError};
use crate::host::HostFunctionId;
use crate::module::FunctionIndex;
use crate::object::{
    ClosureFlags as ObjectClosureFlags, HeapValueKind, ObjectHandle, PropertyAttributes,
    PropertyValue,
};
use crate::payload::VmTrace;
use crate::value::RegisterValue;

use super::{InterpreterError, RuntimeState};

impl RuntimeState {
    /// GC safepoint — called at loop back-edges and function call boundaries.
    /// Collects roots from intrinsics and the provided register window,
    /// then triggers collection if memory pressure warrants it.
    pub fn gc_safepoint(&mut self, registers: &[RegisterValue]) {
        let mut roots = self.intrinsics().gc_root_handles();
        // Extract ObjectHandle roots from the current register window.
        for reg in registers {
            if let Some(handle) = reg.as_object_handle() {
                roots.push(ObjectHandle(handle));
            }
        }
        self.objects.maybe_collect_garbage(&roots);
    }

    /// Allocates one ordinary object with the runtime default prototype.
    pub fn alloc_object(&mut self) -> ObjectHandle {
        let prototype = self.intrinsics().object_prototype();
        let handle = self.objects.alloc_object();
        self.objects
            .set_prototype(handle, Some(prototype))
            .expect("ordinary object prototype should exist");
        handle
    }

    /// Allocates one ordinary object with an explicit prototype.
    pub fn alloc_object_with_prototype(&mut self, prototype: Option<ObjectHandle>) -> ObjectHandle {
        let handle = self.objects.alloc_object();
        self.objects
            .set_prototype(handle, prototype)
            .expect("explicit object prototype should be valid");
        handle
    }

    /// Allocates one ordinary object that carries a Rust-owned native payload.
    pub fn alloc_native_object<T>(&mut self, payload: T) -> ObjectHandle
    where
        T: VmTrace + Any,
    {
        let prototype = self.intrinsics().object_prototype();
        self.alloc_native_object_with_prototype(Some(prototype), payload)
    }

    /// Allocates one payload-bearing object with an explicit prototype.
    pub fn alloc_native_object_with_prototype<T>(
        &mut self,
        prototype: Option<ObjectHandle>,
        payload: T,
    ) -> ObjectHandle
    where
        T: VmTrace + Any,
    {
        let payload = self.native_payloads.insert(payload);
        let handle = self.objects.alloc_native_object(payload);
        self.objects
            .set_prototype(handle, prototype)
            .expect("explicit native object prototype should be valid");
        handle
    }

    /// Allocates one dense array with the runtime default prototype.
    pub fn alloc_array(&mut self) -> ObjectHandle {
        let prototype = self.intrinsics().array_prototype();
        let handle = self.objects.alloc_array();
        self.objects
            .set_prototype(handle, Some(prototype))
            .expect("array prototype should exist");
        handle
    }

    /// Allocates an array and populates it with initial elements.
    pub fn alloc_array_with_elements(&mut self, elements: &[RegisterValue]) -> ObjectHandle {
        let handle = self.alloc_array();
        for &elem in elements {
            self.objects
                .push_element(handle, elem)
                .expect("array push should succeed");
        }
        handle
    }

    /// Extracts elements from an array handle into a Vec of RegisterValues.
    pub fn array_to_args(
        &mut self,
        handle: ObjectHandle,
    ) -> Result<Vec<RegisterValue>, VmNativeCallError> {
        self.objects
            .array_elements(handle)
            .map_err(|e| VmNativeCallError::Internal(format!("array_to_args failed: {e:?}").into()))
    }

    pub fn list_from_array_like(
        &mut self,
        handle: ObjectHandle,
    ) -> Result<Vec<RegisterValue>, VmNativeCallError> {
        let length_key = self.intern_property_name("length");
        let receiver = RegisterValue::from_object_handle(handle.0);
        let length_value = self.ordinary_get(handle, length_key, receiver)?;
        let length = usize::try_from(self.js_to_uint32(length_value).map_err(
            |error| match error {
                InterpreterError::UncaughtThrow(value) => VmNativeCallError::Thrown(value),
                InterpreterError::NativeCall(message) | InterpreterError::TypeError(message) => {
                    VmNativeCallError::Internal(message)
                }
                other => VmNativeCallError::Internal(format!("{other}").into()),
            },
        )?)
        .unwrap_or(usize::MAX);

        let mut values = Vec::with_capacity(length);
        for index in 0..length {
            let property = self.intern_property_name(&index.to_string());
            let value = self.ordinary_get(handle, property, receiver)?;
            values.push(value);
        }
        Ok(values)
    }

    /// Allocates one string object with the runtime default prototype.
    pub fn alloc_string(&mut self, value: impl Into<Box<str>>) -> ObjectHandle {
        let prototype = self.intrinsics().string_prototype();
        let handle = self.objects.alloc_string(value);
        self.objects
            .set_prototype(handle, Some(prototype))
            .expect("string prototype should exist");
        handle
    }

    /// Allocates a string from a WTF-16 `JsString` with the runtime default prototype.
    ///
    /// Preserves lone surrogates as-is.
    pub fn alloc_js_string(&mut self, value: crate::js_string::JsString) -> ObjectHandle {
        let prototype = self.intrinsics().string_prototype();
        let handle = self.objects.alloc_js_string(value);
        self.objects
            .set_prototype(handle, Some(prototype))
            .expect("string prototype should exist");
        handle
    }

    /// Allocates one BigInt heap value (no prototype — BigInt is a primitive type).
    ///
    /// §6.1.6.2 The BigInt Type
    /// <https://tc39.es/ecma262/#sec-ecmascript-language-types-bigint-type>
    pub fn alloc_bigint(&mut self, value: &str) -> ObjectHandle {
        self.objects.alloc_bigint(value)
    }

    /// Allocates a fully-initialized RegExp instance with the spec-mandated
    /// own `lastIndex` property.
    ///
    /// §22.2.3.1 RegExpCreate / §22.2.3.1.1 RegExpAlloc steps 4-5 require the
    /// object to expose `lastIndex` as a data property with attributes
    /// `{ [[Writable]]: true, [[Enumerable]]: false, [[Configurable]]: false }`
    /// and value 0. Defining it up front (instead of letting the first write
    /// create a writable/enumerable/configurable slot) is what lets
    /// `/./.lastIndex === 0`, `verifyProperty` checks, and `delete re.lastIndex`
    /// behave per spec.
    ///
    /// Spec: <https://tc39.es/ecma262/#sec-regexpcreate>
    pub fn alloc_regexp(
        &mut self,
        pattern: &str,
        flags: &str,
        prototype: Option<ObjectHandle>,
    ) -> ObjectHandle {
        let handle = self.objects.alloc_regexp(pattern, flags, prototype);
        let last_index = self.intern_property_name("lastIndex");
        let descriptor = crate::object::PropertyValue::data_with_attrs(
            RegisterValue::from_i32(0),
            crate::object::PropertyAttributes::from_flags(true, false, false),
        );
        self.objects
            .define_own_property(handle, last_index, descriptor)
            .ok();
        handle
    }

    /// Returns the decimal string backing a BigInt handle.
    ///
    /// §6.1.6.2 The BigInt Type
    /// <https://tc39.es/ecma262/#sec-ecmascript-language-types-bigint-type>
    pub fn bigint_value(&self, handle: ObjectHandle) -> Option<&str> {
        self.objects.bigint_value(handle).ok().flatten()
    }

    /// Allocates one fresh symbol primitive with a VM-wide stable identifier.
    pub fn alloc_symbol(&mut self) -> RegisterValue {
        self.alloc_symbol_with_description(None)
    }

    /// Allocates one fresh symbol primitive and records its optional description.
    pub fn alloc_symbol_with_description(
        &mut self,
        description: Option<Box<str>>,
    ) -> RegisterValue {
        let symbol_id = self.next_symbol_id;
        self.next_symbol_id = self
            .next_symbol_id
            .checked_add(1)
            .expect("symbol identifier space exhausted");
        self.symbol_descriptions.insert(symbol_id, description);
        RegisterValue::from_symbol_id(symbol_id)
    }

    /// Returns the recorded description for a symbol value, if any.
    #[must_use]
    pub fn symbol_description(&self, value: RegisterValue) -> Option<&str> {
        let symbol_id = value.as_symbol_id()?;
        self.symbol_descriptions
            .get(&symbol_id)
            .and_then(|description| description.as_deref())
    }

    /// Interns a global-registry symbol key and returns the canonical symbol value.
    pub fn intern_global_symbol(&mut self, key: Box<str>) -> RegisterValue {
        if let Some(&symbol_id) = self.global_symbol_registry.get(key.as_ref()) {
            return RegisterValue::from_symbol_id(symbol_id);
        }

        let symbol = self.alloc_symbol_with_description(Some(key.clone()));
        let symbol_id = symbol
            .as_symbol_id()
            .expect("allocated symbol should expose a symbol id");
        self.global_symbol_registry.insert(key.clone(), symbol_id);
        self.global_symbol_registry_reverse.insert(symbol_id, key);
        symbol
    }

    /// Returns the registry key for a symbol value, if it was created via `Symbol.for`.
    #[must_use]
    pub fn symbol_registry_key(&self, value: RegisterValue) -> Option<&str> {
        let symbol_id = value.as_symbol_id()?;
        self.global_symbol_registry_reverse
            .get(&symbol_id)
            .map(Box::as_ref)
    }

    /// Allocates a new symbol from a JS-visible description value.
    pub fn create_symbol_from_value(
        &mut self,
        description: RegisterValue,
    ) -> Result<RegisterValue, InterpreterError> {
        if description == RegisterValue::undefined() {
            return Ok(self.alloc_symbol_with_description(None));
        }
        let description = self.coerce_symbol_string(description)?;
        Ok(self.alloc_symbol_with_description(Some(description)))
    }

    /// Resolves `Symbol.for(key)` using the runtime-wide global symbol registry.
    pub fn symbol_for_value(
        &mut self,
        key: RegisterValue,
    ) -> Result<RegisterValue, InterpreterError> {
        let key = self.coerce_symbol_string(key)?;
        Ok(self.intern_global_symbol(key))
    }

    fn coerce_symbol_string(&mut self, value: RegisterValue) -> Result<Box<str>, InterpreterError> {
        self.js_to_string(value)
    }

    /// Allocates one host-callable function with the runtime default prototype.
    /// The function is bound to the runtime's currently-active realm.
    pub fn alloc_host_function(&mut self, function: HostFunctionId) -> ObjectHandle {
        let prototype = self.intrinsics().function_prototype();
        let realm = self.current_realm;
        let handle = self.objects.alloc_host_function(function, realm);
        self.objects
            .set_prototype(handle, Some(prototype))
            .expect("function prototype should exist");
        handle
    }

    /// Allocates one host function from descriptor metadata and installs `.name` / `.length`.
    pub fn alloc_host_function_from_descriptor(
        &mut self,
        descriptor: NativeFunctionDescriptor,
    ) -> Result<ObjectHandle, VmNativeCallError> {
        let js_name = descriptor.js_name().to_string();
        let length = descriptor.length();
        let host_function = self.register_native_function(descriptor);
        let handle = self.alloc_host_function(host_function);
        self.install_host_function_length_name(handle, length, &js_name)?;
        Ok(handle)
    }

    /// Installs descriptor-driven members onto one existing host-owned object.
    pub fn install_burrow(
        &mut self,
        target: ObjectHandle,
        descriptors: &[NativeFunctionDescriptor],
    ) -> Result<(), VmNativeCallError> {
        let plan = BurrowBuilder::from_descriptors(descriptors)
            .map(BurrowBuilder::build)
            .map_err(|error| {
                VmNativeCallError::Internal(
                    format!("failed to normalize host object surface: {error}").into(),
                )
            })?;

        for member in plan.members() {
            match member {
                ObjectMemberPlan::Method(function) => {
                    let host_function = self.register_native_function(function.clone());
                    let handle = self.alloc_host_function(host_function);
                    self.install_host_function_length_name(
                        handle,
                        function.length(),
                        function.js_name(),
                    )?;
                    let property = self.intern_property_name(function.js_name());
                    self.objects
                        .define_own_property(
                            target,
                            property,
                            PropertyValue::data_with_attrs(
                                RegisterValue::from_object_handle(handle.0),
                                PropertyAttributes::builtin_method(),
                            ),
                        )
                        .map_err(|error| {
                            VmNativeCallError::Internal(
                                format!(
                                    "failed to install host object method '{}': {error:?}",
                                    function.js_name()
                                )
                                .into(),
                            )
                        })?;
                }
                ObjectMemberPlan::Accessor(accessor) => {
                    let getter = accessor
                        .getter()
                        .cloned()
                        .map(|descriptor| {
                            let function = self.register_native_function(descriptor);
                            Ok(self.alloc_host_function(function))
                        })
                        .transpose()?;
                    let setter = accessor
                        .setter()
                        .cloned()
                        .map(|descriptor| {
                            let function = self.register_native_function(descriptor);
                            Ok(self.alloc_host_function(function))
                        })
                        .transpose()?;
                    let property = self.intern_property_name(accessor.js_name());
                    self.objects
                        .define_accessor(target, property, getter, setter)
                        .map_err(|error| {
                            VmNativeCallError::Internal(
                                format!(
                                    "failed to install host object accessor '{}': {error:?}",
                                    accessor.js_name()
                                )
                                .into(),
                            )
                        })?;
                }
            }
        }

        Ok(())
    }

    /// Registers a native function and installs it as a property on the global object.
    ///
    /// This is the primary API for embedders to inject host-provided globals
    /// (e.g., `print`, `$DONE`, `$262`) into the runtime.
    pub fn install_native_global(
        &mut self,
        descriptor: crate::descriptors::NativeFunctionDescriptor,
    ) -> ObjectHandle {
        let host_fn = self.native_functions.register(descriptor);
        let handle = self.alloc_host_function(host_fn);
        let global = self.intrinsics().global_object();
        let prop = self.property_names.intern(
            self.native_functions
                .get(host_fn)
                .expect("just registered")
                .js_name(),
        );
        self.objects
            .set_property(global, prop, RegisterValue::from_object_handle(handle.0))
            .expect("global property installation should succeed");
        handle
    }

    /// Installs a value property on the global object.
    pub fn install_global_value(&mut self, name: &str, value: RegisterValue) {
        let global = self.intrinsics().global_object();
        let prop = self.property_names.intern(name);
        self.objects
            .set_property(global, prop, value)
            .expect("global property installation should succeed");
    }

    fn install_host_function_length_name(
        &mut self,
        handle: ObjectHandle,
        length: u16,
        name: &str,
    ) -> Result<(), VmNativeCallError> {
        let length_prop = self.intern_property_name("length");
        self.objects
            .define_own_property(
                handle,
                length_prop,
                PropertyValue::data_with_attrs(
                    RegisterValue::from_i32(i32::from(length)),
                    PropertyAttributes::function_length(),
                ),
            )
            .map_err(|error| {
                VmNativeCallError::Internal(
                    format!("failed to install function length for '{name}': {error:?}").into(),
                )
            })?;

        let name_prop = self.intern_property_name("name");
        let name_handle = self.alloc_string(name);
        self.objects
            .define_own_property(
                handle,
                name_prop,
                PropertyValue::data_with_attrs(
                    RegisterValue::from_object_handle(name_handle.0),
                    PropertyAttributes::function_length(),
                ),
            )
            .map_err(|error| {
                VmNativeCallError::Internal(
                    format!("failed to install function name for '{name}': {error:?}").into(),
                )
            })?;

        Ok(())
    }

    /// Allocates one bytecode closure with the runtime default function prototype.
    /// The closure is bound to the runtime's currently-active realm.
    pub fn alloc_closure(
        &mut self,
        callee: FunctionIndex,
        upvalues: Vec<ObjectHandle>,
        flags: ObjectClosureFlags,
    ) -> ObjectHandle {
        // Generator functions should have %GeneratorFunction.prototype%
        // as their [[Prototype]], not %Function.prototype%.
        let prototype = if flags.is_generator() {
            self.intrinsics().generator_function_prototype()
        } else {
            self.intrinsics().function_prototype()
        };
        let module = self
            .current_module
            .clone()
            .expect("closure allocation requires active module context");
        let realm = self.current_realm;
        let handle = self
            .objects
            .alloc_closure(module, callee, upvalues, flags, realm);
        self.objects
            .set_prototype(handle, Some(prototype))
            .expect("function prototype should exist");
        let closure_length = self
            .current_module
            .as_ref()
            .and_then(|module| module.function(callee))
            .map(|function| function.length())
            .unwrap_or(0);
        let closure_name = self
            .current_module
            .as_ref()
            .and_then(|module| module.function(callee))
            .and_then(|function| function.name())
            .unwrap_or("")
            .to_string();
        let length_property = self.intern_property_name("length");
        self.objects
            .define_own_property(
                handle,
                length_property,
                PropertyValue::data_with_attrs(
                    RegisterValue::from_i32(i32::from(closure_length)),
                    PropertyAttributes::function_length(),
                ),
            )
            .expect("closure length should install");
        let name_property = self.intern_property_name("name");
        let name_handle = self.alloc_string(closure_name);
        self.objects
            .define_own_property(
                handle,
                name_property,
                PropertyValue::data_with_attrs(
                    RegisterValue::from_object_handle(name_handle.0),
                    PropertyAttributes::function_length(),
                ),
            )
            .expect("closure name should install");
        // §10.2.6 MakeConstructor + §27.3.3 — Constructable closures AND
        // generator functions get a `.prototype` own property. Generators
        // are not constructable but still get `.prototype` per §27.3.3.
        if flags.is_constructable() || flags.is_generator() {
            let prototype_property = self.intern_property_name("prototype");
            let constructor_property = self.intern_property_name("constructor");
            let instance_prototype = self.alloc_object();
            self.objects
                .define_own_property(
                    handle,
                    prototype_property,
                    PropertyValue::data_with_attrs(
                        RegisterValue::from_object_handle(instance_prototype.0),
                        PropertyAttributes::function_prototype(),
                    ),
                )
                .expect("closure prototype object should install");
            // §27.3.3 — Generator function prototypes do NOT get a
            // `.constructor` back-link. Only regular constructors do.
            if !flags.is_generator() {
                self.objects
                    .define_own_property(
                        instance_prototype,
                        constructor_property,
                        PropertyValue::data_with_attrs(
                            RegisterValue::from_object_handle(handle.0),
                            PropertyAttributes::constructor_link(),
                        ),
                    )
                    .expect("closure prototype.constructor should install");
            }
        }

        handle
    }

    /// ES2024 §7.2.1 Type — returns `true` when the value is an ECMAScript
    /// Object (not a primitive). In our VM, strings and BigInts are heap-
    /// allocated but are still primitives per the spec.
    pub fn is_ecma_object(&self, value: RegisterValue) -> bool {
        let Some(handle) = value.as_object_handle().map(ObjectHandle) else {
            return false;
        };
        !matches!(
            self.objects.kind(handle),
            Ok(HeapValueKind::String | HeapValueKind::BigInt)
        )
    }

    /// ES2024 §7.2.4 IsConstructor — checks if a value has `[[Construct]]`.
    pub fn is_constructible(&self, handle: ObjectHandle) -> bool {
        match self.objects.kind(handle) {
            Ok(HeapValueKind::HostFunction) => {
                // Host functions are constructors only if registered with Constructor slot kind.
                if let Ok(Some(host_fn_id)) = self.objects.host_function(handle) {
                    self.native_functions.get(host_fn_id).is_some_and(|desc| {
                        desc.slot_kind() == crate::descriptors::NativeSlotKind::Constructor
                    })
                } else {
                    false
                }
            }
            Ok(HeapValueKind::Closure) => self
                .objects
                .closure_flags(handle)
                .is_ok_and(|f| f.is_constructable()),
            Ok(HeapValueKind::BoundFunction) => self
                .objects
                .bound_function_parts(handle)
                .is_ok_and(|(target, _, _)| self.is_constructible(target)),
            Ok(HeapValueKind::Proxy) => {
                // A proxy is constructible if its target is constructible.
                self.objects
                    .proxy_parts(handle)
                    .is_ok_and(|(target, _)| self.is_constructible(target))
            }
            _ => false,
        }
    }
}
