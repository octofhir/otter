//! Proxy traps: get/set/delete/has/apply/construct/getPrototypeOf/setPrototypeOf/
//! isExtensible/preventExtensions/getOwnPropertyDescriptor/defineProperty/ownKeys.
//! All invariant checks from ECMA-262 §10.5 are enforced here.

use crate::descriptors::VmNativeCallError;
use crate::object::{ObjectHandle, PropertyValue};
use crate::property::PropertyNameId;
use crate::value::RegisterValue;

use super::{InterpreterError, RuntimeState};

impl RuntimeState {
    // -----------------------------------------------------------------------
    // Proxy helpers — §10.5 Proxy Object Internal Methods
    // -----------------------------------------------------------------------

    /// Returns `true` if the handle points to a Proxy exotic object.
    pub fn is_proxy(&self, handle: ObjectHandle) -> bool {
        self.objects.is_proxy(handle)
    }

    /// Allocates a JS TypeError and returns it as an `UncaughtThrow` so that
    /// `try/catch` in JS can intercept it.
    fn proxy_type_error(&mut self, message: &str) -> InterpreterError {
        match self.alloc_type_error(message) {
            Ok(error) => {
                InterpreterError::UncaughtThrow(RegisterValue::from_object_handle(error.0))
            }
            Err(_) => InterpreterError::TypeError(message.into()),
        }
    }

    /// Returns `(target, handler)` for a live proxy, or throws TypeError if revoked.
    pub fn proxy_check_revoked(
        &mut self,
        handle: ObjectHandle,
    ) -> Result<(ObjectHandle, ObjectHandle), InterpreterError> {
        if self.objects.is_proxy_revoked(handle) {
            return Err(self.proxy_type_error("Cannot perform operation on a revoked proxy"));
        }
        self.objects
            .proxy_parts(handle)
            .map_err(|e| InterpreterError::NativeCall(format!("proxy_parts: {e:?}").into()))
    }

    /// Looks up a trap method on the handler object.
    /// Returns `Some(callable)` if the trap exists, `None` if undefined/null.
    pub fn proxy_get_trap(
        &mut self,
        handler: ObjectHandle,
        trap_name: &str,
    ) -> Result<Option<ObjectHandle>, InterpreterError> {
        let prop = self.intern_property_name(trap_name);
        let value = self.property_lookup(handler, prop)?;
        match value {
            Some(lookup) => match lookup.value() {
                crate::object::PropertyValue::Data { value, .. } => {
                    if value == RegisterValue::undefined() || value == RegisterValue::null() {
                        Ok(None)
                    } else if let Some(h) = value.as_object_handle().map(ObjectHandle) {
                        Ok(Some(h))
                    } else {
                        Err(self.proxy_type_error(&format!(
                            "proxy trap '{trap_name}' is not a function"
                        )))
                    }
                }
                crate::object::PropertyValue::Accessor { getter, .. } => {
                    // Accessor — call getter to obtain the trap function.
                    let trap_val = self.call_callable_for_accessor(
                        getter,
                        RegisterValue::from_object_handle(handler.0),
                        &[],
                    )?;
                    if trap_val == RegisterValue::undefined() || trap_val == RegisterValue::null() {
                        Ok(None)
                    } else if let Some(h) = trap_val.as_object_handle().map(ObjectHandle) {
                        Ok(Some(h))
                    } else {
                        Err(self.proxy_type_error(&format!(
                            "proxy trap '{trap_name}' is not a function"
                        )))
                    }
                }
            },
            None => Ok(None),
        }
    }

    /// Converts a PropertyNameId to a JS string value for passing to proxy traps.
    pub fn property_name_to_value(
        &mut self,
        property: crate::property::PropertyNameId,
    ) -> Result<RegisterValue, InterpreterError> {
        let name = self
            .property_names()
            .get(property)
            .ok_or_else(|| InterpreterError::NativeCall("property name not found".into()))?
            .to_string();
        let handle = self.alloc_string(name);
        Ok(RegisterValue::from_object_handle(handle.0))
    }

    // -----------------------------------------------------------------------
    // Proxy trap dispatch — §10.5 Proxy Object Internal Methods
    // -----------------------------------------------------------------------

    /// §10.5.8 [[Get]](P, Receiver)
    /// Spec: <https://tc39.es/ecma262/#sec-proxy-object-internal-methods-and-internal-slots-get-p-receiver>
    pub fn proxy_get(
        &mut self,
        proxy: ObjectHandle,
        property: PropertyNameId,
        receiver: RegisterValue,
    ) -> Result<RegisterValue, InterpreterError> {
        let (target, handler) = self.proxy_check_revoked(proxy)?;
        let trap = self.proxy_get_trap(handler, "get")?;
        match trap {
            Some(trap_fn) => {
                let target_val = RegisterValue::from_object_handle(target.0);
                let prop_val = self.property_name_to_value(property)?;
                let handler_val = RegisterValue::from_object_handle(handler.0);
                self.call_callable_for_accessor(
                    Some(trap_fn),
                    handler_val,
                    &[target_val, prop_val, receiver],
                )
            }
            None => {
                // No trap — forward to target.[[Get]](P, Receiver)
                if self.is_proxy(target) {
                    self.proxy_get(target, property, receiver)
                } else {
                    match self.property_lookup(target, property)? {
                        Some(lookup) => match lookup.value() {
                            PropertyValue::Data { value, .. } => Ok(value),
                            PropertyValue::Accessor { getter, .. } => {
                                self.call_callable_for_accessor(getter, receiver, &[])
                            }
                        },
                        None => Ok(RegisterValue::undefined()),
                    }
                }
            }
        }
    }

    /// §10.5.9 [[Set]](P, V, Receiver)
    /// Spec: <https://tc39.es/ecma262/#sec-proxy-object-internal-methods-and-internal-slots-set-p-v-receiver>
    pub fn proxy_set(
        &mut self,
        proxy: ObjectHandle,
        property: PropertyNameId,
        value: RegisterValue,
        receiver: RegisterValue,
    ) -> Result<bool, InterpreterError> {
        let (target, handler) = self.proxy_check_revoked(proxy)?;
        let trap = self.proxy_get_trap(handler, "set")?;
        match trap {
            Some(trap_fn) => {
                let target_val = RegisterValue::from_object_handle(target.0);
                let prop_val = self.property_name_to_value(property)?;
                let handler_val = RegisterValue::from_object_handle(handler.0);
                let result = self.call_callable_for_accessor(
                    Some(trap_fn),
                    handler_val,
                    &[target_val, prop_val, value, receiver],
                )?;
                Ok(result.is_truthy())
            }
            None => {
                // No trap — forward to target.[[Set]](P, V, Receiver)
                if self.is_proxy(target) {
                    self.proxy_set(target, property, value, receiver)
                } else {
                    self.set_named_property(target, property, value)?;
                    Ok(true)
                }
            }
        }
    }

    /// §10.5.10 [[Delete]](P)
    /// Spec: <https://tc39.es/ecma262/#sec-proxy-object-internal-methods-and-internal-slots-delete-p>
    pub fn proxy_delete_property(
        &mut self,
        proxy: ObjectHandle,
        property: PropertyNameId,
    ) -> Result<bool, InterpreterError> {
        let (target, handler) = self.proxy_check_revoked(proxy)?;
        let trap = self.proxy_get_trap(handler, "deleteProperty")?;
        match trap {
            Some(trap_fn) => {
                let target_val = RegisterValue::from_object_handle(target.0);
                let prop_val = self.property_name_to_value(property)?;
                let handler_val = RegisterValue::from_object_handle(handler.0);
                let result = self.call_callable_for_accessor(
                    Some(trap_fn),
                    handler_val,
                    &[target_val, prop_val],
                )?;
                Ok(result.is_truthy())
            }
            None => {
                // No trap — forward to target.[[Delete]](P)
                if self.is_proxy(target) {
                    self.proxy_delete_property(target, property)
                } else {
                    let deleted = self.delete_named_property(target, property)?;
                    Ok(deleted)
                }
            }
        }
    }

    /// §10.5.7 [[HasProperty]](P)
    /// Spec: <https://tc39.es/ecma262/#sec-proxy-object-internal-methods-and-internal-slots-hasproperty-p>
    pub fn proxy_has(
        &mut self,
        proxy: ObjectHandle,
        property: PropertyNameId,
    ) -> Result<bool, InterpreterError> {
        let (target, handler) = self.proxy_check_revoked(proxy)?;
        let trap = self.proxy_get_trap(handler, "has")?;
        match trap {
            Some(trap_fn) => {
                let target_val = RegisterValue::from_object_handle(target.0);
                let prop_val = self.property_name_to_value(property)?;
                let handler_val = RegisterValue::from_object_handle(handler.0);
                let result = self.call_callable_for_accessor(
                    Some(trap_fn),
                    handler_val,
                    &[target_val, prop_val],
                )?;
                Ok(result.is_truthy())
            }
            None => {
                // No trap — forward to target.[[HasProperty]](P)
                if self.is_proxy(target) {
                    self.proxy_has(target, property)
                } else {
                    self.has_property(target, property)
                        .map_err(InterpreterError::from)
                }
            }
        }
    }

    /// §10.5.12 [[Call]](thisArgument, argumentsList)
    /// Spec: <https://tc39.es/ecma262/#sec-proxy-object-internal-methods-and-internal-slots-call-thisargument-argumentslist>
    pub fn proxy_apply(
        &mut self,
        proxy: ObjectHandle,
        this_arg: RegisterValue,
        arguments: &[RegisterValue],
    ) -> Result<RegisterValue, InterpreterError> {
        let (target, handler) = self.proxy_check_revoked(proxy)?;
        let trap = self.proxy_get_trap(handler, "apply")?;
        match trap {
            Some(trap_fn) => {
                let target_val = RegisterValue::from_object_handle(target.0);
                let handler_val = RegisterValue::from_object_handle(handler.0);
                let args_array = self.alloc_array_with_elements(arguments);
                let args_val = RegisterValue::from_object_handle(args_array.0);
                self.call_callable_for_accessor(
                    Some(trap_fn),
                    handler_val,
                    &[target_val, this_arg, args_val],
                )
            }
            None => {
                // No trap — forward to target.[[Call]](thisArgument, argumentsList)
                self.call_callable_for_accessor(Some(target), this_arg, arguments)
            }
        }
    }

    /// §10.5.13 [[Construct]](argumentsList, newTarget)
    /// Spec: <https://tc39.es/ecma262/#sec-proxy-object-internal-methods-and-internal-slots-construct-argumentslist-newtarget>
    pub fn proxy_construct(
        &mut self,
        proxy: ObjectHandle,
        arguments: &[RegisterValue],
        new_target: ObjectHandle,
    ) -> Result<RegisterValue, InterpreterError> {
        let (target, handler) = self.proxy_check_revoked(proxy)?;
        let trap = self.proxy_get_trap(handler, "construct")?;
        match trap {
            Some(trap_fn) => {
                let target_val = RegisterValue::from_object_handle(target.0);
                let handler_val = RegisterValue::from_object_handle(handler.0);
                let new_target_val = RegisterValue::from_object_handle(new_target.0);
                let args_array = self.alloc_array_with_elements(arguments);
                let args_val = RegisterValue::from_object_handle(args_array.0);
                let result = self.call_callable_for_accessor(
                    Some(trap_fn),
                    handler_val,
                    &[target_val, args_val, new_target_val],
                )?;
                // §10.5.13 step 10: the result of [[Construct]] must be an object
                if result.as_object_handle().is_none() {
                    return Err(
                        self.proxy_type_error("'construct' on proxy: trap returned non-Object")
                    );
                }
                Ok(result)
            }
            None => {
                // No trap — forward to target.[[Construct]](argumentsList, newTarget)
                match self.construct_callable(target, arguments, new_target) {
                    Ok(value) => Ok(value),
                    Err(VmNativeCallError::Thrown(value)) => {
                        Err(InterpreterError::UncaughtThrow(value))
                    }
                    Err(VmNativeCallError::Internal(message)) => {
                        Err(InterpreterError::NativeCall(message))
                    }
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // §10.5.1 [[GetPrototypeOf]]()
    // Spec: <https://tc39.es/ecma262/#sec-proxy-object-internal-methods-and-internal-slots-getprototypeof>
    // -----------------------------------------------------------------------
    pub fn proxy_get_prototype_of(
        &mut self,
        proxy: ObjectHandle,
    ) -> Result<Option<ObjectHandle>, InterpreterError> {
        let (target, handler) = self.proxy_check_revoked(proxy)?;
        let trap = self.proxy_get_trap(handler, "getPrototypeOf")?;
        match trap {
            Some(trap_fn) => {
                let target_val = RegisterValue::from_object_handle(target.0);
                let handler_val = RegisterValue::from_object_handle(handler.0);
                let result =
                    self.call_callable_for_accessor(Some(trap_fn), handler_val, &[target_val])?;
                // Step 5: If Type(handlerProto) is neither Object nor Null, throw TypeError.
                if result == RegisterValue::null() {
                    // §10.5.1 step 8: invariant — if target is non-extensible, trap must
                    // return the same value as target.[[GetPrototypeOf]]().
                    let target_extensible = self
                        .objects
                        .is_extensible(target)
                        .map_err(|e| InterpreterError::NativeCall(format!("{e:?}").into()))?;
                    if !target_extensible {
                        let target_proto = self
                            .objects
                            .get_prototype(target)
                            .map_err(|e| InterpreterError::NativeCall(format!("{e:?}").into()))?;
                        if target_proto.is_some() {
                            return Err(self.proxy_type_error(
                                "'getPrototypeOf' on proxy: proxy target is non-extensible but the trap returned a prototype different from the target's prototype",
                            ));
                        }
                    }
                    Ok(None)
                } else if let Some(h) = result.as_object_handle().map(ObjectHandle) {
                    // §10.5.1 step 8: invariant check
                    let target_extensible = self
                        .objects
                        .is_extensible(target)
                        .map_err(|e| InterpreterError::NativeCall(format!("{e:?}").into()))?;
                    if !target_extensible {
                        let target_proto = self
                            .objects
                            .get_prototype(target)
                            .map_err(|e| InterpreterError::NativeCall(format!("{e:?}").into()))?;
                        if target_proto != Some(h) {
                            return Err(self.proxy_type_error(
                                "'getPrototypeOf' on proxy: proxy target is non-extensible but the trap returned a prototype different from the target's prototype",
                            ));
                        }
                    }
                    Ok(Some(h))
                } else {
                    Err(self.proxy_type_error(
                        "'getPrototypeOf' on proxy: trap returned neither object nor null",
                    ))
                }
            }
            None => {
                // No trap — forward to target.[[GetPrototypeOf]]()
                if self.is_proxy(target) {
                    self.proxy_get_prototype_of(target)
                } else {
                    self.objects
                        .get_prototype(target)
                        .map_err(|e| InterpreterError::NativeCall(format!("{e:?}").into()))
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // §10.5.2 [[SetPrototypeOf]](V)
    // Spec: <https://tc39.es/ecma262/#sec-proxy-object-internal-methods-and-internal-slots-setprototypeof-v>
    // -----------------------------------------------------------------------
    pub fn proxy_set_prototype_of(
        &mut self,
        proxy: ObjectHandle,
        prototype: Option<ObjectHandle>,
    ) -> Result<bool, InterpreterError> {
        let (target, handler) = self.proxy_check_revoked(proxy)?;
        let trap = self.proxy_get_trap(handler, "setPrototypeOf")?;
        match trap {
            Some(trap_fn) => {
                let target_val = RegisterValue::from_object_handle(target.0);
                let handler_val = RegisterValue::from_object_handle(handler.0);
                let proto_val = prototype
                    .map(|h| RegisterValue::from_object_handle(h.0))
                    .unwrap_or_else(RegisterValue::null);
                let result = self.call_callable_for_accessor(
                    Some(trap_fn),
                    handler_val,
                    &[target_val, proto_val],
                )?;
                let boolean_trap_result = result.is_truthy();
                if !boolean_trap_result {
                    return Ok(false);
                }
                // §10.5.2 step 12: invariant — if target is non-extensible, V must be
                // SameValue as target.[[GetPrototypeOf]]().
                let target_extensible = self
                    .objects
                    .is_extensible(target)
                    .map_err(|e| InterpreterError::NativeCall(format!("{e:?}").into()))?;
                if !target_extensible {
                    let target_proto = self
                        .objects
                        .get_prototype(target)
                        .map_err(|e| InterpreterError::NativeCall(format!("{e:?}").into()))?;
                    if target_proto != prototype {
                        return Err(self.proxy_type_error(
                            "'setPrototypeOf' on proxy: trap returned truish but the proxy target is non-extensible and the new prototype is different from the current one",
                        ));
                    }
                }
                Ok(true)
            }
            None => {
                // No trap — forward to target.[[SetPrototypeOf]](V)
                if self.is_proxy(target) {
                    self.proxy_set_prototype_of(target, prototype)
                } else {
                    self.objects
                        .set_prototype(target, prototype)
                        .map_err(|e| InterpreterError::NativeCall(format!("{e:?}").into()))
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // §10.5.3 [[IsExtensible]]()
    // Spec: <https://tc39.es/ecma262/#sec-proxy-object-internal-methods-and-internal-slots-isextensible>
    // -----------------------------------------------------------------------
    pub fn proxy_is_extensible(&mut self, proxy: ObjectHandle) -> Result<bool, InterpreterError> {
        let (target, handler) = self.proxy_check_revoked(proxy)?;
        let trap = self.proxy_get_trap(handler, "isExtensible")?;
        match trap {
            Some(trap_fn) => {
                let target_val = RegisterValue::from_object_handle(target.0);
                let handler_val = RegisterValue::from_object_handle(handler.0);
                let result =
                    self.call_callable_for_accessor(Some(trap_fn), handler_val, &[target_val])?;
                let boolean_trap_result = result.is_truthy();
                // §10.5.3 step 8: invariant — must agree with target.[[IsExtensible]]()
                let target_extensible = self
                    .objects
                    .is_extensible(target)
                    .map_err(|e| InterpreterError::NativeCall(format!("{e:?}").into()))?;
                if boolean_trap_result != target_extensible {
                    return Err(self.proxy_type_error(
                        "'isExtensible' on proxy: trap result does not reflect extensibility of proxy target",
                    ));
                }
                Ok(boolean_trap_result)
            }
            None => {
                // No trap — forward to target.[[IsExtensible]]()
                if self.is_proxy(target) {
                    self.proxy_is_extensible(target)
                } else {
                    self.objects
                        .is_extensible(target)
                        .map_err(|e| InterpreterError::NativeCall(format!("{e:?}").into()))
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // §10.5.4 [[PreventExtensions]]()
    // Spec: <https://tc39.es/ecma262/#sec-proxy-object-internal-methods-and-internal-slots-preventextensions>
    // -----------------------------------------------------------------------
    pub fn proxy_prevent_extensions(
        &mut self,
        proxy: ObjectHandle,
    ) -> Result<bool, InterpreterError> {
        let (target, handler) = self.proxy_check_revoked(proxy)?;
        let trap = self.proxy_get_trap(handler, "preventExtensions")?;
        match trap {
            Some(trap_fn) => {
                let target_val = RegisterValue::from_object_handle(target.0);
                let handler_val = RegisterValue::from_object_handle(handler.0);
                let result =
                    self.call_callable_for_accessor(Some(trap_fn), handler_val, &[target_val])?;
                let boolean_trap_result = result.is_truthy();
                // §10.5.4 step 8: if trap returns true, target must be non-extensible.
                if boolean_trap_result {
                    let target_extensible = self
                        .objects
                        .is_extensible(target)
                        .map_err(|e| InterpreterError::NativeCall(format!("{e:?}").into()))?;
                    if target_extensible {
                        return Err(self.proxy_type_error(
                            "'preventExtensions' on proxy: trap returned truish but the proxy target is extensible",
                        ));
                    }
                }
                Ok(boolean_trap_result)
            }
            None => {
                // No trap — forward to target.[[PreventExtensions]]()
                if self.is_proxy(target) {
                    self.proxy_prevent_extensions(target)
                } else {
                    self.objects
                        .prevent_extensions(target)
                        .map_err(|e| InterpreterError::NativeCall(format!("{e:?}").into()))
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // §10.5.5 [[GetOwnProperty]](P)
    // Spec: <https://tc39.es/ecma262/#sec-proxy-object-internal-methods-and-internal-slots-getownproperty-p>
    // -----------------------------------------------------------------------
    pub fn proxy_get_own_property_descriptor(
        &mut self,
        proxy: ObjectHandle,
        property: PropertyNameId,
    ) -> Result<Option<PropertyValue>, InterpreterError> {
        let (target, handler) = self.proxy_check_revoked(proxy)?;
        let trap = self.proxy_get_trap(handler, "getOwnPropertyDescriptor")?;
        match trap {
            Some(trap_fn) => {
                let target_val = RegisterValue::from_object_handle(target.0);
                let prop_val = self.property_name_to_value(property)?;
                let handler_val = RegisterValue::from_object_handle(handler.0);
                let result = self.call_callable_for_accessor(
                    Some(trap_fn),
                    handler_val,
                    &[target_val, prop_val],
                )?;
                // Step 9: If Type(trapResultObj) is neither Object nor Undefined, throw TypeError.
                if result == RegisterValue::undefined() {
                    // §10.5.5 step 14: If targetDesc is not undefined and targetDesc.[[Configurable]]
                    // is false, throw TypeError.
                    let target_desc = self
                        .own_property_descriptor(target, property)
                        .map_err(|e| InterpreterError::NativeCall(format!("{e:?}").into()))?;
                    if let Some(td) = target_desc {
                        if !td.attributes().configurable() {
                            return Err(self.proxy_type_error(
                                "'getOwnPropertyDescriptor' on proxy: trap returned undefined for a non-configurable property",
                            ));
                        }
                        // §10.5.5 step 15: if target is non-extensible and property exists, cannot report as non-existent
                        let target_extensible = self
                            .objects
                            .is_extensible(target)
                            .map_err(|e| InterpreterError::NativeCall(format!("{e:?}").into()))?;
                        if !target_extensible {
                            return Err(self.proxy_type_error(
                                "'getOwnPropertyDescriptor' on proxy: trap returned undefined for an existing property on a non-extensible target",
                            ));
                        }
                    }
                    Ok(None)
                } else if let Some(desc_handle) = result.as_object_handle().map(ObjectHandle) {
                    // Convert the trap result to a PropertyDescriptor via ToPropertyDescriptor.
                    let desc = crate::abstract_ops::to_property_descriptor(Some(desc_handle), self)
                        .map_err(|e| match e {
                            VmNativeCallError::Thrown(v) => InterpreterError::UncaughtThrow(v),
                            VmNativeCallError::Internal(m) => InterpreterError::NativeCall(m),
                        })?;
                    // Convert PropertyDescriptor to PropertyValue using the descriptor's apply logic.
                    let pv = desc.to_property_value();
                    Ok(Some(pv))
                } else {
                    Err(self.proxy_type_error(
                        "'getOwnPropertyDescriptor' on proxy: trap returned neither object nor undefined",
                    ))
                }
            }
            None => {
                // No trap — forward to target.[[GetOwnProperty]](P)
                if self.is_proxy(target) {
                    self.proxy_get_own_property_descriptor(target, property)
                } else {
                    self.own_property_descriptor(target, property)
                        .map_err(|e| InterpreterError::NativeCall(format!("{e:?}").into()))
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // §10.5.6 [[DefineOwnProperty]](P, Desc)
    // Spec: <https://tc39.es/ecma262/#sec-proxy-object-internal-methods-and-internal-slots-defineownproperty-p-desc>
    // -----------------------------------------------------------------------
    pub fn proxy_define_own_property(
        &mut self,
        proxy: ObjectHandle,
        property: PropertyNameId,
        desc_value: RegisterValue,
    ) -> Result<bool, InterpreterError> {
        let (target, handler) = self.proxy_check_revoked(proxy)?;
        let trap = self.proxy_get_trap(handler, "defineProperty")?;
        match trap {
            Some(trap_fn) => {
                let target_val = RegisterValue::from_object_handle(target.0);
                let prop_val = self.property_name_to_value(property)?;
                let handler_val = RegisterValue::from_object_handle(handler.0);
                let result = self.call_callable_for_accessor(
                    Some(trap_fn),
                    handler_val,
                    &[target_val, prop_val, desc_value],
                )?;
                let boolean_trap_result = result.is_truthy();
                if !boolean_trap_result {
                    return Ok(false);
                }
                // §10.5.6 step 15: invariant — cannot define non-configurable property on
                // extensible target that doesn't have it, or change configurable→non-configurable.
                let target_desc = self
                    .own_property_descriptor(target, property)
                    .map_err(|e| InterpreterError::NativeCall(format!("{e:?}").into()))?;
                let target_extensible = self
                    .objects
                    .is_extensible(target)
                    .map_err(|e| InterpreterError::NativeCall(format!("{e:?}").into()))?;
                if target_desc.is_none() && !target_extensible {
                    return Err(self.proxy_type_error(
                        "'defineProperty' on proxy: trap returned truish for adding property to non-extensible target",
                    ));
                }
                Ok(true)
            }
            None => {
                // No trap — forward to target.[[DefineOwnProperty]](P, Desc)
                if self.is_proxy(target) {
                    self.proxy_define_own_property(target, property, desc_value)
                } else {
                    // Convert desc_value to PropertyDescriptor and apply.
                    let desc_handle = desc_value.as_object_handle().map(ObjectHandle);
                    let desc = crate::abstract_ops::to_property_descriptor(desc_handle, self)
                        .map_err(|e| match e {
                            VmNativeCallError::Thrown(v) => InterpreterError::UncaughtThrow(v),
                            VmNativeCallError::Internal(m) => InterpreterError::NativeCall(m),
                        })?;
                    let property_names = self.property_names().clone();
                    self.objects
                        .define_own_property_from_descriptor_with_registry(
                            target,
                            property,
                            desc,
                            &property_names,
                        )
                        .map_err(|e| InterpreterError::NativeCall(format!("{e:?}").into()))
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // §10.5.11 [[OwnPropertyKeys]]()
    // Spec: <https://tc39.es/ecma262/#sec-proxy-object-internal-methods-and-internal-slots-ownpropertykeys>
    // -----------------------------------------------------------------------
    pub fn proxy_own_keys(
        &mut self,
        proxy: ObjectHandle,
    ) -> Result<Vec<PropertyNameId>, InterpreterError> {
        let (target, handler) = self.proxy_check_revoked(proxy)?;
        let trap = self.proxy_get_trap(handler, "ownKeys")?;
        match trap {
            Some(trap_fn) => {
                let target_val = RegisterValue::from_object_handle(target.0);
                let handler_val = RegisterValue::from_object_handle(handler.0);
                let result =
                    self.call_callable_for_accessor(Some(trap_fn), handler_val, &[target_val])?;
                // Step 7: CreateListFromArrayLike — the result must be an array-like
                // whose elements are Strings or Symbols.
                let Some(arr_handle) = result.as_object_handle().map(ObjectHandle) else {
                    return Err(
                        self.proxy_type_error("'ownKeys' on proxy: trap result is not an object")
                    );
                };
                let length_prop = self.intern_property_name("length");
                let length_val = self
                    .own_property_value(arr_handle, length_prop)
                    .map_err(|e| InterpreterError::NativeCall(format!("{e:?}").into()))?;
                let length = length_val.as_number().map(|n| n as usize).unwrap_or(0);
                let mut keys = Vec::with_capacity(length);
                for i in 0..length {
                    let index_key = self.intern_property_name(&i.to_string());
                    let elem = self
                        .own_property_value(arr_handle, index_key)
                        .map_err(|e| InterpreterError::NativeCall(format!("{e:?}").into()))?;
                    // Each element must be a string (or symbol).
                    let key_id = self.property_name_from_value(elem).map_err(|e| match e {
                        VmNativeCallError::Thrown(v) => InterpreterError::UncaughtThrow(v),
                        VmNativeCallError::Internal(m) => InterpreterError::NativeCall(m),
                    })?;
                    keys.push(key_id);
                }
                Ok(keys)
            }
            None => {
                // No trap — forward to target.[[OwnPropertyKeys]]()
                if self.is_proxy(target) {
                    self.proxy_own_keys(target)
                } else {
                    self.own_property_keys(target)
                        .map_err(|e| InterpreterError::NativeCall(format!("{e:?}").into()))
                }
            }
        }
    }
}
