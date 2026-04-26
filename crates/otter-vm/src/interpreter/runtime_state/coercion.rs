//! ECMA-262 §7 abstract operations: ToPrimitive, ToString, ToNumber,
//! ToBoolean, ToInt32/Uint32, OrdinaryToPrimitive, `==` (loose equality),
//! relational comparison, `+` with BigInt fallbacks, `instanceof`,
//! OrdinaryHasInstance, `in`, plus `bigint_*` checked arithmetic and the
//! `js_typeof` / `js_add` operator implementations.

use crate::descriptors::VmNativeCallError;
use crate::intrinsics::{
    WellKnownSymbol, box_boolean_object, box_number_object, box_string_object, box_symbol_object,
};
use crate::object::{HeapValueKind, ObjectError, ObjectHandle, PropertyValue};
use crate::property::PropertyNameId;
use crate::value::RegisterValue;

use super::{
    InterpreterError, RuntimeState, STRING_DATA_SLOT, ToPrimitiveHint, f64_to_int32, f64_to_uint32,
    parse_string_to_number,
};

impl RuntimeState {
    pub(crate) fn invalid_array_length_error(&mut self) -> InterpreterError {
        let prototype = self.intrinsics().range_error_prototype;
        let Ok(handle) = self.alloc_object_with_prototype(Some(prototype)) else {
            return InterpreterError::OutOfMemory;
        };
        // Strategy B: store .message as TAG_PTR_STRING.
        let Ok(message) = self.alloc_string_value("Invalid array length") else {
            return InterpreterError::OutOfMemory;
        };
        let message_prop = self.intern_property_name("message");
        self.objects
            .set_property(handle, message_prop, message)
            .ok();
        InterpreterError::UncaughtThrow(RegisterValue::from_object_handle(handle.0))
    }

    /// §6.1.4 — RangeError("Invalid string length"). Thrown by the C2 lazy
    /// `+` / `concat` / `repeat` when the result would exceed
    /// [`crate::js_string::MAX_STRING_LENGTH`].
    pub(crate) fn invalid_string_length_error(&mut self) -> InterpreterError {
        let prototype = self.intrinsics().range_error_prototype;
        let Ok(handle) = self.alloc_object_with_prototype(Some(prototype)) else {
            return InterpreterError::OutOfMemory;
        };
        // Strategy B: store .message as TAG_PTR_STRING.
        let Ok(message) = self.alloc_string_value("Invalid string length") else {
            return InterpreterError::OutOfMemory;
        };
        let message_prop = self.intern_property_name("message");
        self.objects
            .set_property(handle, message_prop, message)
            .ok();
        InterpreterError::UncaughtThrow(RegisterValue::from_object_handle(handle.0))
    }

    /// C2 helper: returns an `ObjectHandle` to a *primitive* string for any
    /// value, allocating a fresh `SeqTwoByte` from the UTF-8 ToString
    /// conversion when the input is not already a string.
    ///
    /// Used by the lazy `+` path in `js_add` to avoid re-allocating strings
    /// that already live in the heap.
    pub(crate) fn coerce_to_string_handle(
        &mut self,
        value: RegisterValue,
    ) -> Result<ObjectHandle, InterpreterError> {
        if let Some(handle) = value.as_object_handle().map(ObjectHandle) {
            if self.objects.string_value(handle)?.is_some() {
                return Ok(handle);
            }
            // String wrapper object → unwrap to the inner primitive handle.
            if let Some(inner) = self.string_wrapper_data(handle)?
                && self.objects.string_value(inner)?.is_some()
            {
                return Ok(inner);
            }
        }
        // Strategy B: read WTF-16 content via the new path (lossless,
        // preserves lone surrogates), then materialise a legacy
        // `HeapValue::String` handle. This is the bridge consumers of
        // `coerce_to_string_handle` need until they migrate. Once
        // every consumer accepts `RegisterValue` directly, this branch
        // collapses with the rest of the migration in step 2.8.
        if let Some(gc_ref) = value.as_string_ref() {
            let cow = crate::js_string_gc::as_utf16_cow(gc_ref);
            let js = crate::js_string::JsString::from_utf16_vec(cow.into_owned());
            return self.alloc_js_string(js);
        }
        let s = self.js_to_string(value)?;
        let handle = self.alloc_string(s.into_string())?;
        Ok(handle)
    }

    fn own_data_property(
        &mut self,
        handle: ObjectHandle,
        slot_name: &str,
    ) -> Result<Option<RegisterValue>, InterpreterError> {
        let backing = self.intern_property_name(slot_name);
        let Some(lookup) = self.objects.get_property(handle, backing)? else {
            return Ok(None);
        };
        if lookup.owner() != handle {
            return Ok(None);
        }
        let PropertyValue::Data { value, .. } = lookup.value() else {
            return Ok(None);
        };
        Ok(Some(value))
    }

    fn string_wrapper_data(
        &mut self,
        handle: ObjectHandle,
    ) -> Result<Option<ObjectHandle>, InterpreterError> {
        Ok(self
            .own_data_property(handle, STRING_DATA_SLOT)?
            .and_then(|value| value.as_object_handle().map(ObjectHandle)))
    }

    /// §7.2.15 IsLooselyEqual(x, y)
    /// <https://tc39.es/ecma262/#sec-islooselyequal>
    pub(crate) fn js_loose_eq(
        &mut self,
        lhs: RegisterValue,
        rhs: RegisterValue,
    ) -> Result<bool, InterpreterError> {
        if self.objects.strict_eq(lhs, rhs)? {
            return Ok(true);
        }
        if (lhs == RegisterValue::undefined() && rhs == RegisterValue::null())
            || (lhs == RegisterValue::null() && rhs == RegisterValue::undefined())
        {
            return Ok(true);
        }

        // §7.2.15 step 1 delegates same-Type Number pairs to
        // Number::equal. NaN is never loosely equal to anything,
        // including itself; handling that terminal case here also
        // prevents the primitive-coercion fallback from recursing on
        // a still-NaN pair.
        if let (Some(lhs_num), Some(rhs_num)) = (lhs.as_number(), rhs.as_number()) {
            return Ok(!lhs_num.is_nan() && !rhs_num.is_nan() && lhs_num == rhs_num);
        }

        let lhs_is_string = self.value_is_primitive_string(lhs)?;
        let rhs_is_string = self.value_is_primitive_string(rhs)?;

        // Same-Type Object pairs were already checked by strict_eq above.
        // Distinct object references compare false for loose equality;
        // ToPrimitive only applies to Object-vs-primitive pairs. Heap strings
        // are object handles internally, but are primitive strings
        // semantically, so they must still flow through the string cases below.
        if lhs.as_object_handle().is_some()
            && rhs.as_object_handle().is_some()
            && !lhs_is_string
            && !rhs_is_string
        {
            return Ok(false);
        }

        // §7.2.15 step 10-11: BigInt == Number comparison.
        if lhs.is_bigint() && rhs.as_number().is_some() {
            return self.bigint_equals_number(lhs, rhs);
        }
        if lhs.as_number().is_some() && rhs.is_bigint() {
            return self.bigint_equals_number(rhs, lhs);
        }

        // §7.2.15 step 12-13: BigInt == String comparison.
        if lhs.is_bigint() && rhs_is_string {
            let rhs_str = self.js_to_string(rhs)?;
            if let Ok(rhs_payload) =
                crate::bigint_value::BigIntPayload::from_decimal_str(&rhs_str)
            {
                let lhs_payload = self.bigint_payload_for(lhs)?;
                return Ok(lhs_payload == &rhs_payload);
            }
            return Ok(false);
        }
        if lhs_is_string && rhs.is_bigint() {
            let lhs_str = self.js_to_string(lhs)?;
            if let Ok(lhs_payload) =
                crate::bigint_value::BigIntPayload::from_decimal_str(&lhs_str)
            {
                let rhs_payload = self.bigint_payload_for(rhs)?;
                return Ok(rhs_payload == &lhs_payload);
            }
            return Ok(false);
        }

        // §7.2.15 steps 6-7: Number/String pairs compare after ToNumber.
        if lhs.as_number().is_some() && rhs_is_string {
            let rhs_number = RegisterValue::from_number(self.js_to_number(rhs)?);
            return self.js_loose_eq(lhs, rhs_number);
        }
        if lhs_is_string && rhs.as_number().is_some() {
            let lhs_number = RegisterValue::from_number(self.js_to_number(lhs)?);
            return self.js_loose_eq(lhs_number, rhs);
        }

        // §7.2.15 steps 8-9: Boolean compares as ToNumber(boolean).
        if lhs.as_bool().is_some() {
            let lhs_number = RegisterValue::from_number(self.js_to_number(lhs)?);
            return self.js_loose_eq(lhs_number, rhs);
        }
        if rhs.as_bool().is_some() {
            let rhs_number = RegisterValue::from_number(self.js_to_number(rhs)?);
            return self.js_loose_eq(lhs, rhs_number);
        }

        let coerced_lhs = self.coerce_loose_equality_primitive(lhs)?;
        let coerced_rhs = self.coerce_loose_equality_primitive(rhs)?;
        if coerced_lhs == coerced_rhs {
            return Ok(true);
        }
        if coerced_lhs.raw_bits() != lhs.raw_bits() || coerced_rhs.raw_bits() != rhs.raw_bits() {
            return self.js_loose_eq(coerced_lhs, coerced_rhs);
        }

        Ok(false)
    }

    pub(crate) fn non_string_object_handle(
        &self,
        value: RegisterValue,
    ) -> Result<Option<ObjectHandle>, ObjectError> {
        let Some(handle) = value.as_object_handle().map(ObjectHandle) else {
            return Ok(None);
        };
        if matches!(self.objects.kind(handle)?, HeapValueKind::String) {
            return Ok(None);
        }
        Ok(Some(handle))
    }

    pub(crate) fn computed_property_name(
        &mut self,
        key: RegisterValue,
    ) -> Result<PropertyNameId, InterpreterError> {
        self.property_name_from_value(key)
            .map_err(|error| match error {
                VmNativeCallError::Thrown(_) => {
                    InterpreterError::TypeError("property key coercion threw".into())
                }
                VmNativeCallError::Internal(message) => InterpreterError::NativeCall(message),
            })
    }

    /// ES2024 §10.2.9 SetFunctionName — overrides a closure's own `name` data
    /// property based on a runtime property key. Used when installing
    /// computed-key class methods/getters/setters (and object-literal methods)
    /// so their `Function.name` matches the evaluated key.
    ///
    /// For Symbol keys the name becomes `"[desc]"` (or `""` when the symbol
    /// has no description). For string/numeric keys the name is the `ToString`
    /// of the property key. An optional `prefix` (e.g. `"get"` / `"set"`) is
    /// prepended followed by a U+0020 SPACE, matching the spec.
    ///
    /// Spec: <https://tc39.es/ecma262/#sec-setfunctionname>
    pub(crate) fn update_closure_function_name(
        &mut self,
        closure: ObjectHandle,
        key: RegisterValue,
        prefix: Option<&str>,
    ) -> Result<(), InterpreterError> {
        // 1. Derive the base name from the property key.
        let base_name: String = if let Some(sid) = key.as_symbol_id() {
            match self
                .symbol_descriptions
                .get(&sid)
                .and_then(|description| description.as_deref())
            {
                Some(description) => format!("[{description}]"),
                None => String::new(),
            }
        } else if let Some(handle) = key.as_object_handle().map(ObjectHandle) {
            // String heap values (WTF-16). Fall back to stringifying whatever
            // the runtime considers the property key form. C2: walk Cons /
            // Sliced / Thin via the heap-aware helper (read-only).
            match self.objects.string_value(handle) {
                Ok(Some(_)) => self
                    .objects
                    .js_string_to_rust_string(handle)
                    .unwrap_or_default(),
                _ => {
                    // Non-string object keys should have been coerced upstream
                    // by ToPropertyKey; treat them as empty to stay defensive.
                    String::new()
                }
            }
        } else if let Some(i) = key.as_i32() {
            i.to_string()
        } else if let Some(f) = key.as_number() {
            // Numeric keys are rare as computed class-method keys — they would
            // already be stringified by ToPropertyKey upstream. Fall back to
            // Rust's float formatting here; it matches JS Number::toString for
            // the common integer/decimal cases we care about.
            format!("{f}")
        } else {
            String::new()
        };

        // 2. Apply the optional prefix.
        let full_name = match prefix {
            Some(prefix_str) => format!("{prefix_str} {base_name}"),
            None => base_name,
        };

        // 3. Define the "name" own property. The slot installed by
        //    `alloc_closure` is configurable, so re-defining it is legal.
        // Strategy B: store .name as TAG_PTR_STRING.
        let name_property = self.intern_property_name("name");
        let name_value = self.alloc_string_value(&full_name)?;
        self.objects
            .define_own_property(
                closure,
                name_property,
                crate::object::PropertyValue::data_with_attrs(
                    name_value,
                    crate::object::PropertyAttributes::function_length(),
                ),
            )
            .map_err(|_| InterpreterError::TypeError("closure name define failed".into()))?;
        Ok(())
    }

    pub(crate) fn property_base_object_handle(
        &mut self,
        value: RegisterValue,
    ) -> Result<ObjectHandle, InterpreterError> {
        if value == RegisterValue::undefined() || value == RegisterValue::null() {
            return Err(InterpreterError::TypeError(
                "Cannot read properties of null or undefined".into(),
            ));
        }
        if let Some(handle) = value.as_object_handle().map(ObjectHandle) {
            return Ok(handle);
        }
        // Strategy B: GC-managed string ref. Materialise a legacy
        // primitive string handle via WTF-16 round-trip (lossless), then
        // box it into a `String` wrapper so the standard prototype-chain
        // property lookup path applies. Once allocators all switch to
        // TAG_PTR_STRING and the dispatch table understands TAG_PTR_STRING
        // natively, this branch can route directly to String.prototype.
        if let Some(gc_ref) = value.as_string_ref() {
            let cow = crate::js_string_gc::as_utf16_cow(gc_ref);
            let js = crate::js_string::JsString::from_utf16_vec(cow.into_owned());
            let primitive = self.alloc_js_string(js)?;
            let wrapper = box_string_object(primitive, self).map_err(|error| match error {
                VmNativeCallError::Thrown(_) => {
                    InterpreterError::TypeError("string boxing threw".into())
                }
                VmNativeCallError::Internal(message) => InterpreterError::NativeCall(message),
            })?;
            return Ok(ObjectHandle(
                wrapper
                    .as_object_handle()
                    .expect("boxed string should return object handle"),
            ));
        }
        if let Some(boolean) = value.as_bool() {
            let object =
                box_boolean_object(RegisterValue::from_bool(boolean), self).map_err(|error| {
                    match error {
                        VmNativeCallError::Thrown(_) => {
                            InterpreterError::TypeError("boolean boxing threw".into())
                        }
                        VmNativeCallError::Internal(message) => {
                            InterpreterError::NativeCall(message)
                        }
                    }
                })?;
            return Ok(ObjectHandle(
                object
                    .as_object_handle()
                    .expect("boxed boolean should return object handle"),
            ));
        }
        if let Some(number) = value.as_number() {
            let object =
                box_number_object(RegisterValue::from_number(number), self).map_err(|error| {
                    match error {
                        VmNativeCallError::Thrown(_) => {
                            InterpreterError::TypeError("number boxing threw".into())
                        }
                        VmNativeCallError::Internal(message) => {
                            InterpreterError::NativeCall(message)
                        }
                    }
                })?;
            return Ok(ObjectHandle(
                object
                    .as_object_handle()
                    .expect("boxed number should return object handle"),
            ));
        }
        if value.is_bigint() {
            let wrapper =
                self.alloc_object_with_prototype(Some(self.intrinsics().bigint_prototype()))?;
            return Ok(wrapper);
        }
        if value.is_symbol() {
            let object = box_symbol_object(value, self).map_err(|error| match error {
                VmNativeCallError::Thrown(_) => {
                    InterpreterError::TypeError("symbol boxing threw".into())
                }
                VmNativeCallError::Internal(message) => InterpreterError::NativeCall(message),
            })?;
            return Ok(ObjectHandle(
                object
                    .as_object_handle()
                    .expect("boxed symbol should return object handle"),
            ));
        }
        Err(InterpreterError::InvalidObjectValue)
    }

    pub(crate) fn property_set_target_handle(
        &mut self,
        value: RegisterValue,
    ) -> Result<ObjectHandle, InterpreterError> {
        if value == RegisterValue::undefined() || value == RegisterValue::null() {
            return Err(InterpreterError::TypeError(
                "Cannot set properties of null or undefined".into(),
            ));
        }
        if let Some(handle) = value.as_object_handle().map(ObjectHandle) {
            return Ok(handle);
        }
        if value.as_bool().is_some() {
            return Ok(self.intrinsics().boolean_prototype());
        }
        if value.as_number().is_some() {
            return Ok(self.intrinsics().number_prototype());
        }
        if value.is_symbol() {
            return Ok(self.intrinsics().symbol_prototype());
        }
        Err(InterpreterError::InvalidObjectValue)
    }

    pub(crate) fn is_primitive_property_base(
        &self,
        value: RegisterValue,
    ) -> Result<bool, ObjectError> {
        if value.as_bool().is_some() || value.as_number().is_some() || value.is_symbol() {
            return Ok(true);
        }
        let Some(handle) = value.as_object_handle().map(ObjectHandle) else {
            return Ok(false);
        };
        Ok(matches!(self.objects.kind(handle)?, HeapValueKind::String))
    }

    fn ordinary_to_primitive(
        &mut self,
        value: RegisterValue,
        hint: ToPrimitiveHint,
    ) -> Result<RegisterValue, InterpreterError> {
        let Some(handle) = value.as_object_handle().map(ObjectHandle) else {
            return Ok(value);
        };

        let method_names = match hint {
            ToPrimitiveHint::Default | ToPrimitiveHint::Number => ["valueOf", "toString"],
            ToPrimitiveHint::String => ["toString", "valueOf"],
        };

        for method_name in method_names {
            let property = self.intern_property_name(method_name);
            let method =
                self.ordinary_get(handle, property, value)
                    .map_err(|error| match error {
                        VmNativeCallError::Thrown(value) => InterpreterError::UncaughtThrow(value),
                        VmNativeCallError::Internal(message) => {
                            InterpreterError::NativeCall(message)
                        }
                    })?;
            let Some(callable) = method.as_object_handle().map(ObjectHandle) else {
                continue;
            };
            if !self.objects.is_callable(callable) {
                continue;
            }

            let result = self
                .call_callable(callable, value, &[])
                .map_err(|error| match error {
                    VmNativeCallError::Thrown(value) => InterpreterError::UncaughtThrow(value),
                    VmNativeCallError::Internal(message) => InterpreterError::NativeCall(message),
                })?;
            if self.non_string_object_handle(result)?.is_none() {
                return Ok(result);
            }
        }

        Err(InterpreterError::TypeError(
            "Cannot convert object to primitive value".into(),
        ))
    }

    pub(crate) fn js_to_primitive_with_hint(
        &mut self,
        value: RegisterValue,
        hint: ToPrimitiveHint,
    ) -> Result<RegisterValue, InterpreterError> {
        let Some(handle) = value.as_object_handle().map(ObjectHandle) else {
            return Ok(value);
        };

        if self.objects.string_value(handle)?.is_some() {
            return Ok(value);
        }

        let to_primitive =
            self.intern_symbol_property_name(WellKnownSymbol::ToPrimitive.stable_id());
        let exotic =
            self.ordinary_get(handle, to_primitive, value)
                .map_err(|error| match error {
                    VmNativeCallError::Thrown(value) => InterpreterError::UncaughtThrow(value),
                    VmNativeCallError::Internal(message) => InterpreterError::NativeCall(message),
                })?;

        if exotic != RegisterValue::undefined() && exotic != RegisterValue::null() {
            let Some(callable) = exotic.as_object_handle().map(ObjectHandle) else {
                return Err(InterpreterError::TypeError(
                    "@@toPrimitive is not callable".into(),
                ));
            };
            if !self.objects.is_callable(callable) {
                return Err(InterpreterError::TypeError(
                    "@@toPrimitive is not callable".into(),
                ));
            }

            // Strategy B: pass the hint as TAG_PTR_STRING.
            let hint_value = match hint {
                ToPrimitiveHint::Default => self.alloc_string_value("default")?,
                ToPrimitiveHint::String => self.alloc_string_value("string")?,
                ToPrimitiveHint::Number => self.alloc_string_value("number")?,
            };
            let result = self
                .call_callable(callable, value, &[hint_value])
                .map_err(|error| match error {
                    VmNativeCallError::Thrown(value) => InterpreterError::UncaughtThrow(value),
                    VmNativeCallError::Internal(message) => InterpreterError::NativeCall(message),
                })?;
            if self.non_string_object_handle(result)?.is_some() {
                return Err(InterpreterError::TypeError(
                    "@@toPrimitive must return a primitive value".into(),
                ));
            }
            return Ok(result);
        }

        self.ordinary_to_primitive(value, hint)
    }

    fn coerce_loose_equality_primitive(
        &mut self,
        value: RegisterValue,
    ) -> Result<RegisterValue, InterpreterError> {
        let Some(_handle) = value.as_object_handle().map(ObjectHandle) else {
            return Ok(value);
        };
        self.js_to_primitive_with_hint(value, ToPrimitiveHint::Default)
    }

    pub(crate) fn js_to_string(
        &mut self,
        value: RegisterValue,
    ) -> Result<Box<str>, InterpreterError> {
        if value == RegisterValue::undefined() {
            return Ok("undefined".into());
        }
        if value == RegisterValue::null() {
            return Ok("null".into());
        }
        if let Some(boolean) = value.as_bool() {
            return Ok(if boolean { "true" } else { "false" }.into());
        }
        if value.is_symbol() {
            return Err(InterpreterError::TypeError(
                "Cannot convert a Symbol value to a string".into(),
            ));
        }
        // §6.1.6.2.14 BigInt::toString(x)
        if let Some(handle) = value.as_bigint_handle() {
            let text = self
                .objects
                .bigint_value(ObjectHandle(handle))?
                .map(|p| p.to_decimal_string())
                .unwrap_or_else(|| "0".to_string());
            return Ok(text.into_boxed_str());
        }
        // Strategy B: GC-managed string ref. Uses the new `js_string_gc`
        // reader API which handles Latin-1 / UTF-16 / lone surrogates.
        // Cons / Sliced / Thin variants would need flatten before this
        // call, but the migration only allocates flat reprs so far.
        if let Some(gc_ref) = value.as_string_ref() {
            return Ok(crate::js_string_gc::to_rust_string(gc_ref).into_boxed_str());
        }
        if let Some(number) = value.as_number() {
            return Ok(crate::abstract_ops::ecma_number_to_string(number).into_boxed_str());
        }
        if let Some(handle) = value.as_object_handle().map(ObjectHandle) {
            if self.objects.string_value(handle)?.is_some() {
                // C2: flatten Cons / Sliced / Thin before reading the
                // contents — otherwise `to_string` produces a debug
                // placeholder.
                self.objects.flatten_string(handle)?;
                if let Some(string) = self.objects.string_value(handle)? {
                    return Ok(string.to_string().into_boxed_str());
                }
            }
            let primitive = self.js_to_primitive_with_hint(value, ToPrimitiveHint::String)?;
            if primitive != value {
                return self.js_to_string(primitive);
            }
            return Ok("[object Object]".into());
        }

        Ok(String::new().into_boxed_str())
    }

    /// Infallible ToString — returns "" on any error.
    pub fn js_to_string_infallible(&mut self, value: RegisterValue) -> Box<str> {
        self.js_to_string(value).unwrap_or_default()
    }

    /// ES spec 7.1.4 ToNumber — converts a value to its numeric representation.
    /// <https://tc39.es/ecma262/#sec-tonumber>
    pub fn js_to_number(&mut self, value: RegisterValue) -> Result<f64, InterpreterError> {
        if value == RegisterValue::undefined() {
            return Ok(f64::NAN);
        }
        if value == RegisterValue::null() {
            return Ok(0.0);
        }
        if let Some(boolean) = value.as_bool() {
            return Ok(if boolean { 1.0 } else { 0.0 });
        }
        if value.is_symbol() {
            return Err(InterpreterError::TypeError(
                "Cannot convert a Symbol value to a number".into(),
            ));
        }
        // §7.1.4 step 1.e: BigInt → throw TypeError.
        if value.is_bigint() {
            return Err(InterpreterError::TypeError(
                "Cannot convert a BigInt value to a number".into(),
            ));
        }
        // Strategy B: GC-managed string ref. §7.1.4 step 1.f: ToNumber on
        // a string parses it via StringToNumber (`parse_string_to_number`).
        if let Some(gc_ref) = value.as_string_ref() {
            let s = crate::js_string_gc::to_rust_string(gc_ref);
            return Ok(parse_string_to_number(&s));
        }
        if let Some(number) = value.as_number() {
            return Ok(number);
        }
        if let Some(handle) = value.as_object_handle().map(ObjectHandle) {
            if self.objects.string_value(handle)?.is_some() {
                // C2: flatten before reading content (Cons / Sliced / Thin).
                self.objects.flatten_string(handle)?;
                if let Some(string) = self.objects.string_value(handle)? {
                    return Ok(parse_string_to_number(&string.to_rust_string()));
                }
            }
            let primitive = self.js_to_primitive_with_hint(value, ToPrimitiveHint::Number)?;
            if primitive != value {
                return self.js_to_number(primitive);
            }
            return Ok(f64::NAN);
        }
        Ok(f64::NAN)
    }

    /// ES spec 7.1.6 ToInt32 — converts a value to a signed 32-bit integer.
    pub fn js_to_int32(&mut self, value: RegisterValue) -> Result<i32, InterpreterError> {
        let n = self.js_to_number(value)?;
        Ok(f64_to_int32(n))
    }

    /// ES spec 7.1.7 ToUint32 — converts a value to an unsigned 32-bit integer.
    pub fn js_to_uint32(&mut self, value: RegisterValue) -> Result<u32, InterpreterError> {
        let n = self.js_to_number(value)?;
        Ok(f64_to_uint32(n))
    }

    /// ES spec 7.1.1 ToPrimitive with hint Number — converts an object to
    /// a primitive value.  Returns the value unchanged for non-objects.
    fn js_to_primitive_number(
        &mut self,
        value: RegisterValue,
    ) -> Result<RegisterValue, InterpreterError> {
        self.js_to_primitive_with_hint(value, ToPrimitiveHint::Number)
    }

    /// ES spec 7.2.13 Abstract Relational Comparison.
    /// <https://tc39.es/ecma262/#sec-abstract-relational-comparison>
    /// Returns `Some(true)` for less-than, `Some(false)` for not less-than,
    /// `None` for undefined (NaN involved).
    pub(crate) fn js_abstract_relational_comparison(
        &mut self,
        lhs: RegisterValue,
        rhs: RegisterValue,
        left_first: bool,
    ) -> Result<Option<bool>, InterpreterError> {
        // 1-2. ToPrimitive with hint Number.
        let (px, py) = if left_first {
            let px = self.js_to_primitive_number(lhs)?;
            let py = self.js_to_primitive_number(rhs)?;
            (px, py)
        } else {
            let py = self.js_to_primitive_number(rhs)?;
            let px = self.js_to_primitive_number(lhs)?;
            (px, py)
        };

        // 3. If both are strings, compare lexicographically.
        let px_is_string = self.value_is_string(px)?;
        let py_is_string = self.value_is_string(py)?;
        if px_is_string && py_is_string {
            let sx = self.js_to_string(px)?;
            let sy = self.js_to_string(py)?;
            return Ok(Some(sx.as_ref() < sy.as_ref()));
        }

        // §7.2.13 step 3.a: If both are BigInt, use BigInt::lessThan.
        if px.is_bigint() && py.is_bigint() {
            return self.bigint_less_than(px, py);
        }

        // §7.2.13 step 3.b: Mixed BigInt/Number comparison.
        if px.is_bigint() && py.as_number().is_some() {
            return self.bigint_number_less_than(px, py);
        }
        if px.as_number().is_some() && py.is_bigint() {
            // number < bigint ≡ !(bigint < number) && !(bigint == number)
            // But spec says: reverse roles in step 3.c.
            return self.number_bigint_less_than(px, py);
        }

        // §7.2.13 step 3.d: Mixed BigInt + String comparison.
        if px.is_bigint() && py_is_string {
            let sy = self.js_to_string(py)?;
            if let Ok(ny) = crate::bigint_value::BigIntPayload::from_decimal_str(&sy) {
                let lhs_payload = self.bigint_payload_for(px)?;
                return Ok(Some(lhs_payload.cmp(&ny) == std::cmp::Ordering::Less));
            }
            return Ok(None);
        }
        if px_is_string && py.is_bigint() {
            let sx = self.js_to_string(px)?;
            if let Ok(nx) = crate::bigint_value::BigIntPayload::from_decimal_str(&sx) {
                let rhs_payload = self.bigint_payload_for(py)?;
                return Ok(Some(nx.cmp(rhs_payload) == std::cmp::Ordering::Less));
            }
            return Ok(None);
        }

        // 4. Otherwise, coerce both to numbers.
        let nx = self.js_to_number(px)?;
        let ny = self.js_to_number(py)?;
        // NaN comparisons return undefined (None).
        if nx.is_nan() || ny.is_nan() {
            return Ok(None);
        }
        Ok(Some(nx < ny))
    }

    /// Parse the BigInt value from a register into a `num_bigint::BigInt`.
    fn bigint_payload_for(
        &self,
        value: RegisterValue,
    ) -> Result<&crate::bigint_value::BigIntPayload, InterpreterError> {
        let handle = ObjectHandle(
            value
                .as_bigint_handle()
                .ok_or_else(|| InterpreterError::TypeError("expected BigInt".into()))?,
        );
        self.objects
            .bigint_value(handle)?
            .ok_or(InterpreterError::InvalidHeapValueKind)
    }

    /// §6.1.6.2.12 BigInt::lessThan(x, y)
    /// <https://tc39.es/ecma262/#sec-numeric-types-bigint-lessThan>
    fn bigint_less_than(
        &self,
        lhs: RegisterValue,
        rhs: RegisterValue,
    ) -> Result<Option<bool>, InterpreterError> {
        let lhs_pay = self.bigint_payload_for(lhs)?;
        let rhs_pay = self.bigint_payload_for(rhs)?;
        Ok(Some(lhs_pay.cmp(rhs_pay) == std::cmp::Ordering::Less))
    }

    /// §7.2.13 step 3.b: BigInt < Number comparison.
    fn bigint_number_less_than(
        &self,
        bigint_val: RegisterValue,
        number_val: RegisterValue,
    ) -> Result<Option<bool>, InterpreterError> {
        let n = number_val.as_number().unwrap();
        if n.is_nan() || n.is_infinite() {
            return Ok(if n.is_nan() {
                None
            } else if n.is_sign_positive() {
                Some(true) // bigint < +Infinity
            } else {
                Some(false) // bigint < -Infinity
            });
        }
        let bv_payload = self.bigint_payload_for(bigint_val)?;
        let bv = bv_payload.as_bigint();
        let n_int = num_bigint::BigInt::from(n as i64);
        if bv.as_ref() < &n_int {
            Ok(Some(true))
        } else if bv.as_ref() > &n_int {
            Ok(Some(false))
        } else {
            // bv == n_int, but n may have fractional part
            use num_traits::ToPrimitive;
            let n_int_f = n_int.to_f64().unwrap_or(0.0);
            Ok(Some(n_int_f < n))
        }
    }

    /// §7.2.13 step 3.c: Number < BigInt comparison.
    fn number_bigint_less_than(
        &self,
        number_val: RegisterValue,
        bigint_val: RegisterValue,
    ) -> Result<Option<bool>, InterpreterError> {
        let n = number_val.as_number().unwrap();
        if n.is_nan() || n.is_infinite() {
            return Ok(if n.is_nan() {
                None
            } else if n.is_sign_positive() {
                Some(false) // +Infinity < bigint → false
            } else {
                Some(true) // -Infinity < bigint → true
            });
        }
        let bv_payload = self.bigint_payload_for(bigint_val)?;
        let bv = bv_payload.as_bigint();
        let n_int = num_bigint::BigInt::from(n as i64);
        if &n_int < bv.as_ref() {
            Ok(Some(true))
        } else if &n_int > bv.as_ref() {
            Ok(Some(false))
        } else {
            // n_int == bv, but n may have fractional part
            use num_traits::ToPrimitive;
            let n_int_f = n_int.to_f64().unwrap_or(0.0);
            Ok(Some(n < n_int_f))
        }
    }

    /// §7.2.15 BigInt == Number comparison.
    /// <https://tc39.es/ecma262/#sec-islooselyequal>
    fn bigint_equals_number(
        &self,
        bigint_val: RegisterValue,
        number_val: RegisterValue,
    ) -> Result<bool, InterpreterError> {
        let n = number_val.as_number().unwrap();
        if n.is_nan() || n.is_infinite() {
            return Ok(false);
        }
        // If n has a fractional part, it can never equal a BigInt.
        if n.fract() != 0.0 {
            return Ok(false);
        }
        let bv_payload = self.bigint_payload_for(bigint_val)?;
        let bv = bv_payload.as_bigint();
        let n_int = num_bigint::BigInt::from(n as i64);
        Ok(bv.as_ref() == &n_int)
    }

    /// ES spec 7.1.2 ToBoolean — runtime-aware truthiness check.
    /// <https://tc39.es/ecma262/#sec-toboolean>
    /// Unlike `RegisterValue::is_truthy()`, this correctly handles heap strings
    /// (empty string "" is falsy) and BigInt (0n is falsy).
    pub(crate) fn js_to_boolean(&mut self, value: RegisterValue) -> Result<bool, InterpreterError> {
        // §7.1.2 step 7: BigInt — 0n is falsy, all others truthy.
        if let Some(handle) = value.as_bigint_handle() {
            let is_zero = self
                .objects
                .bigint_value(ObjectHandle(handle))?
                .map(|p| p.is_zero())
                .unwrap_or(true);
            return Ok(!is_zero);
        }
        // Fast path: non-object values use the NaN-box check.
        let Some(handle) = value.as_object_handle().map(ObjectHandle) else {
            return Ok(value.is_truthy());
        };
        // Heap strings: empty string is falsy, non-empty is truthy.
        if let Some(s) = self.objects.string_value(handle)? {
            return Ok(!s.is_empty());
        }
        // All other objects are truthy.
        Ok(true)
    }

    /// ES spec §7.3.21 OrdinaryHasInstance — `value instanceof constructor`.
    /// ES2024 §7.3.22 InstanceofOperator(V, target).
    pub(crate) fn js_instance_of(
        &mut self,
        value: RegisterValue,
        constructor: RegisterValue,
    ) -> Result<bool, InterpreterError> {
        // 1. If target is not an Object, throw a TypeError.
        let Some(ctor_handle) = constructor.as_object_handle().map(ObjectHandle) else {
            return Err(InterpreterError::TypeError(
                "Right-hand side of instanceof is not an object".into(),
            ));
        };

        // 2. Let instOfHandler be ? GetMethod(target, @@hasInstance).
        let has_instance_sym =
            self.intern_symbol_property_name(WellKnownSymbol::HasInstance.stable_id());
        let handler = self
            .ordinary_get(ctor_handle, has_instance_sym, constructor)
            .map_err(|error| match error {
                VmNativeCallError::Thrown(v) => InterpreterError::UncaughtThrow(v),
                VmNativeCallError::Internal(m) => InterpreterError::NativeCall(m),
            })?;

        // 3. If instOfHandler is not undefined, then
        if handler != RegisterValue::undefined() && handler != RegisterValue::null() {
            let Some(handler_handle) = handler.as_object_handle().map(ObjectHandle) else {
                return Err(InterpreterError::TypeError(
                    "@@hasInstance is not callable".into(),
                ));
            };
            if !self.objects.is_callable(handler_handle) {
                return Err(InterpreterError::TypeError(
                    "@@hasInstance is not callable".into(),
                ));
            }
            // a. Return ! ToBoolean(? Call(instOfHandler, target, « V »)).
            let result = self
                .call_callable(handler_handle, constructor, &[value])
                .map_err(|error| match error {
                    VmNativeCallError::Thrown(v) => InterpreterError::UncaughtThrow(v),
                    VmNativeCallError::Internal(m) => InterpreterError::NativeCall(m),
                })?;
            return self.js_to_boolean(result);
        }

        // 4. If IsCallable(target) is false, throw a TypeError.
        if !self.objects.is_callable(ctor_handle) {
            return Err(InterpreterError::TypeError(
                "Right-hand side of instanceof is not callable".into(),
            ));
        }

        // 5. Return ? OrdinaryHasInstance(target, V).
        self.ordinary_has_instance(value, ctor_handle)
    }

    /// ES2024 §7.3.21 OrdinaryHasInstance(C, O).
    fn ordinary_has_instance(
        &mut self,
        value: RegisterValue,
        constructor: ObjectHandle,
    ) -> Result<bool, InterpreterError> {
        // 1. If IsCallable(C) is false, return false.
        if !self.objects.is_callable(constructor) {
            return Ok(false);
        }

        // 2. If C has a [[BoundTargetFunction]] internal slot, unwrap.
        let mut effective_ctor = constructor;
        while matches!(
            self.objects.kind(effective_ctor),
            Ok(HeapValueKind::BoundFunction)
        ) {
            let (target, _, _) = self.objects.bound_function_parts(effective_ctor)?;
            effective_ctor = target;
        }

        // 3. If Type(O) is not Object, return false.
        let Some(obj_handle) = value.as_object_handle().map(ObjectHandle) else {
            return Ok(false);
        };

        // 4. Let P be ? Get(C, "prototype").
        let proto_prop = self.intern_property_name("prototype");
        let proto_value = self
            .ordinary_get(
                effective_ctor,
                proto_prop,
                RegisterValue::from_object_handle(effective_ctor.0),
            )
            .map_err(|error| match error {
                VmNativeCallError::Thrown(v) => InterpreterError::UncaughtThrow(v),
                VmNativeCallError::Internal(m) => InterpreterError::NativeCall(m),
            })?;

        // 5. If Type(P) is not Object, throw a TypeError.
        let Some(proto_handle) = proto_value.as_object_handle().map(ObjectHandle) else {
            return Err(InterpreterError::TypeError(
                "Function has non-object prototype in instanceof check".into(),
            ));
        };

        // 6. Repeat: walk the prototype chain of O.
        let mut current = self.objects.get_prototype(obj_handle)?;
        let mut depth = 0;
        while let Some(p) = current {
            if p == proto_handle {
                return Ok(true);
            }
            depth += 1;
            if depth > 45 {
                break;
            }
            current = self.objects.get_prototype(p)?;
        }
        Ok(false)
    }

    /// ES2024 §13.10.1 The `in` Operator — `HasProperty(object, ToPropertyKey(key))`.
    pub(crate) fn js_has_property(
        &mut self,
        key: RegisterValue,
        object: RegisterValue,
    ) -> Result<bool, InterpreterError> {
        let Some(obj_handle) = self.non_string_object_handle(object)? else {
            return Err(InterpreterError::TypeError(
                "Cannot use 'in' operator to search for property in non-object".into(),
            ));
        };
        let property = self.computed_property_name(key)?;
        // §10.5.7 — Proxy [[HasProperty]] trap
        if self.is_proxy(obj_handle) {
            return self.proxy_has(obj_handle, property);
        }
        self.has_property(obj_handle, property)
            .map_err(InterpreterError::from)
    }

    /// Allocate an error object with the correct prototype chain.
    pub(crate) fn alloc_reference_error(
        &mut self,
        message: &str,
    ) -> Result<ObjectHandle, InterpreterError> {
        let prototype = self.intrinsics().reference_error_prototype;
        let handle = self.alloc_object_with_prototype(Some(prototype))?;
        // Strategy B: store .message as TAG_PTR_STRING.
        let msg_value = self.alloc_string_value(message)?;
        let msg_prop = self.intern_property_name("message");
        self.objects.set_property(handle, msg_prop, msg_value)?;
        Ok(handle)
    }

    /// Allocate a TypeError object with the correct prototype chain.
    pub fn alloc_type_error(&mut self, message: &str) -> Result<ObjectHandle, InterpreterError> {
        let prototype = self.intrinsics().type_error_prototype;
        let handle = self.alloc_object_with_prototype(Some(prototype))?;
        // Strategy B: store .message as TAG_PTR_STRING.
        let msg_value = self.alloc_string_value(message)?;
        let msg_prop = self.intern_property_name("message");
        self.objects.set_property(handle, msg_prop, msg_value)?;
        Ok(handle)
    }

    /// Allocates one RangeError instance with the given message.
    pub fn alloc_range_error(&mut self, message: &str) -> Result<ObjectHandle, InterpreterError> {
        let prototype = self.intrinsics().range_error_prototype;
        let handle = self.alloc_object_with_prototype(Some(prototype))?;
        // Strategy B: store .message as TAG_PTR_STRING.
        let msg_value = self.alloc_string_value(message)?;
        let msg_prop = self.intern_property_name("message");
        self.objects.set_property(handle, msg_prop, msg_value)?;
        Ok(handle)
    }

    /// Creates a { status: "...", [value_key]: value } object for Promise.allSettled.
    /// ES2024 §27.2.4.2.1–2
    pub fn alloc_settled_result_object(
        &mut self,
        status: &str,
        value_key: &str,
        value: RegisterValue,
    ) -> Result<ObjectHandle, InterpreterError> {
        let obj = self.alloc_object()?;
        let status_prop = self.intern_property_name("status");
        // Strategy B: status is TAG_PTR_STRING.
        let status_value = self.alloc_string_value(status)?;
        let _ = self.objects.set_property(obj, status_prop, status_value);
        let value_prop = self.intern_property_name(value_key);
        let _ = self.objects.set_property(obj, value_prop, value);
        Ok(obj)
    }

    /// §19.2.1 Step 1: If x is not a String, return None.
    /// Extracts the string content if `value` is a string primitive.
    /// Does NOT coerce — returns None for non-string values.
    ///
    /// Spec: <https://tc39.es/ecma262/#sec-eval-x>
    pub fn value_as_string(&self, value: RegisterValue) -> Option<String> {
        // Strategy B path: native GC-managed string ref.
        if let Some(gc_ref) = value.as_string_ref() {
            return Some(crate::js_string_gc::to_rust_string(gc_ref));
        }
        // Legacy path: HeapValue::String behind an ObjectHandle.
        let handle = value.as_object_handle().map(ObjectHandle)?;
        // C2: Cons / Sliced / Thin require walking. `js_string_to_rust_string`
        // does that via `&self` (no mutation).
        self.objects.string_value(handle).ok().flatten()?;
        self.objects.js_string_to_rust_string(handle).ok()
    }

    /// Checks whether a value is a string type (heap string or string wrapper).
    fn value_is_string(&mut self, value: RegisterValue) -> Result<bool, InterpreterError> {
        // Strategy B path.
        if value.is_string_ref() {
            return Ok(true);
        }
        let Some(handle) = value.as_object_handle().map(ObjectHandle) else {
            return Ok(false);
        };
        if self.objects.string_value(handle)?.is_some() {
            return Ok(true);
        }
        if let Some(inner) = self.string_wrapper_data(handle)?
            && self.objects.string_value(inner)?.is_some()
        {
            return Ok(true);
        }
        Ok(false)
    }

    /// Checks for ECMAScript Type(String). String wrappers are Object values
    /// and must not match this in loose equality dispatch.
    fn value_is_primitive_string(&self, value: RegisterValue) -> Result<bool, InterpreterError> {
        // Strategy B: TAG_PTR_STRING is always a primitive string.
        if value.is_string_ref() {
            return Ok(true);
        }
        let Some(handle) = value.as_object_handle().map(ObjectHandle) else {
            return Ok(false);
        };
        Ok(self.objects.string_value(handle)?.is_some())
    }

    /// §6.1.6.2 BigInt arithmetic helper — performs a binary operation on two
    /// BigInt register values and returns the result as a new BigInt.
    ///
    /// The `op` closure receives the structured [`BigIntPayload`] payloads
    /// directly, so the i64 inline fast path inside `BigIntPayload::add` /
    /// `sub` / `mul` skips the previous decimal-string parse + stringify
    /// round-trip on every call. (C1 in `PRODUCTION_READINESS_PLAN.md`.)
    /// <https://tc39.es/ecma262/#sec-numeric-types-bigint-add>
    pub(crate) fn bigint_binary_op(
        &mut self,
        lhs: RegisterValue,
        rhs: RegisterValue,
        op: impl FnOnce(
            &crate::bigint_value::BigIntPayload,
            &crate::bigint_value::BigIntPayload,
        ) -> crate::bigint_value::BigIntPayload,
    ) -> Result<RegisterValue, InterpreterError> {
        let lhs_handle = ObjectHandle(
            lhs.as_bigint_handle()
                .ok_or_else(|| InterpreterError::TypeError("expected BigInt".into()))?,
        );
        let rhs_handle = ObjectHandle(
            rhs.as_bigint_handle()
                .ok_or_else(|| InterpreterError::TypeError("expected BigInt".into()))?,
        );

        let result = {
            let lhs_pay = self
                .objects
                .bigint_value(lhs_handle)?
                .ok_or(InterpreterError::InvalidHeapValueKind)?;
            let rhs_pay = self
                .objects
                .bigint_value(rhs_handle)?
                .ok_or(InterpreterError::InvalidHeapValueKind)?;
            op(lhs_pay, rhs_pay)
        };

        let handle = self.alloc_bigint(result)?;
        Ok(RegisterValue::from_bigint_handle(handle.0))
    }

    /// §6.1.6.2.10 BigInt::divide — truncating division, RangeError on zero divisor.
    /// <https://tc39.es/ecma262/#sec-numeric-types-bigint-divide>
    pub(crate) fn bigint_checked_div(
        &mut self,
        lhs: RegisterValue,
        rhs: RegisterValue,
    ) -> Result<RegisterValue, InterpreterError> {
        // Spec step 1: throw RangeError when the divisor is 0n. Resolve the
        // payload up front so the closure does not have to re-borrow.
        let rhs_handle = ObjectHandle(
            rhs.as_bigint_handle()
                .ok_or_else(|| InterpreterError::TypeError("expected BigInt".into()))?,
        );
        let rhs_is_zero = self
            .objects
            .bigint_value(rhs_handle)?
            .ok_or(InterpreterError::InvalidHeapValueKind)?
            .is_zero();
        if rhs_is_zero {
            return Err(InterpreterError::TypeError("Division by zero".into()));
        }
        self.bigint_binary_op(lhs, rhs, |a, b| a.div_trunc(b))
    }

    /// §6.1.6.2.11 BigInt::remainder — RangeError on zero divisor.
    /// <https://tc39.es/ecma262/#sec-numeric-types-bigint-remainder>
    pub(crate) fn bigint_checked_rem(
        &mut self,
        lhs: RegisterValue,
        rhs: RegisterValue,
    ) -> Result<RegisterValue, InterpreterError> {
        let rhs_handle = ObjectHandle(
            rhs.as_bigint_handle()
                .ok_or_else(|| InterpreterError::TypeError("expected BigInt".into()))?,
        );
        let rhs_is_zero = self
            .objects
            .bigint_value(rhs_handle)?
            .ok_or(InterpreterError::InvalidHeapValueKind)?
            .is_zero();
        if rhs_is_zero {
            return Err(InterpreterError::TypeError("Division by zero".into()));
        }
        self.bigint_binary_op(lhs, rhs, |a, b| a.rem_trunc(b))
    }

    /// §12.8.3 The Addition Operator ( + )
    /// <https://tc39.es/ecma262/#sec-addition-operator-plus>
    pub(crate) fn js_add(
        &mut self,
        lhs: RegisterValue,
        rhs: RegisterValue,
    ) -> Result<RegisterValue, InterpreterError> {
        // i32 fast-path: both operands are already i32 → skip ToPrimitive
        // and all type checks entirely. This is the common case in loops
        // with integer arithmetic like `(acc + i) | 0`.
        if let (Some(l), Some(r)) = (lhs.as_i32(), rhs.as_i32()) {
            return match l.checked_add(r) {
                Some(sum) => Ok(RegisterValue::from_i32(sum)),
                None => Ok(RegisterValue::from_number(l as f64 + r as f64)),
            };
        }

        // f64 fast-path: both operands are already numbers.
        if let (Some(l), Some(r)) = (lhs.as_number(), rhs.as_number()) {
            return Ok(RegisterValue::from_number(l + r));
        }

        // §13.15.3 ApplyStringOrNumericBinaryOperator — step 1-4: ToPrimitive first.
        let lprim = self.js_to_primitive_with_hint(lhs, ToPrimitiveHint::Number)?;
        let rprim = self.js_to_primitive_with_hint(rhs, ToPrimitiveHint::Number)?;

        // §13.15.3 step 5: If either is a String, do string concatenation.
        let lhs_is_string = self.value_is_string(lprim)?;
        let rhs_is_string = self.value_is_string(rprim)?;
        if lhs_is_string || rhs_is_string {
            // C2: lazy `+` — produce a Cons handle for non-trivial inputs
            // instead of eagerly UTF-8 round-tripping. This is the
            // O(n²) → O(n log n) win for `s += piece` loops.
            let lhandle = self.coerce_to_string_handle(lprim)?;
            let rhandle = self.coerce_to_string_handle(rprim)?;
            let proto = Some(self.intrinsics().string_prototype());
            match self.objects.concat_strings(lhandle, rhandle, proto) {
                Ok(result) => return Ok(RegisterValue::from_object_handle(result.0)),
                Err(crate::object::ObjectError::InvalidArrayLength) => {
                    // §6.1.4 + V8 cap: RangeError("Invalid string length").
                    return Err(self.invalid_string_length_error());
                }
                Err(other) => return Err(other.into()),
            }
        }

        // §6.1.6.2.7 BigInt::add — both operands BigInt.
        if lprim.is_bigint() && rprim.is_bigint() {
            return self.bigint_binary_op(lprim, rprim, |a, b| a.add(b));
        }
        // Mixed BigInt + non-BigInt → TypeError (§12.15.3 step 6).
        if lprim.is_bigint() || rprim.is_bigint() {
            return Err(InterpreterError::TypeError(
                "Cannot mix BigInt and other types, use explicit conversions".into(),
            ));
        }

        // General case: coerce to Number (ToNumber). Undefined → NaN,
        // null → 0, bool → 0/1.
        let lhs_num = self.js_to_number(lprim)?;
        let rhs_num = self.js_to_number(rprim)?;
        Ok(RegisterValue::from_number(lhs_num + rhs_num))
    }

    /// §13.15.3 ApplyStringOrNumericBinaryOperator for `-`. Same
    /// ToNumeric pipeline as `/` / `%` / `**`; never string-
    /// concatenates (that's `+`'s special case only).
    pub(crate) fn js_subtract(
        &mut self,
        lhs: RegisterValue,
        rhs: RegisterValue,
    ) -> Result<RegisterValue, InterpreterError> {
        if let (Some(l), Some(r)) = (lhs.as_number(), rhs.as_number()) {
            return Ok(RegisterValue::from_number(l - r));
        }
        self.js_numeric_binop_fallback(lhs, rhs, |l, r| l - r, "-", |a, b| Ok(a.sub(b)))
    }

    /// §13.15.3 ApplyStringOrNumericBinaryOperator for `*`.
    pub(crate) fn js_multiply(
        &mut self,
        lhs: RegisterValue,
        rhs: RegisterValue,
    ) -> Result<RegisterValue, InterpreterError> {
        if let (Some(l), Some(r)) = (lhs.as_number(), rhs.as_number()) {
            return Ok(RegisterValue::from_number(l * r));
        }
        self.js_numeric_binop_fallback(lhs, rhs, |l, r| l * r, "*", |a, b| Ok(a.mul(b)))
    }

    /// §13.15.3 ApplyStringOrNumericBinaryOperator for `/`. Spec:
    /// ToNumeric both operands, then BigInt::divide if both BigInt,
    /// else Number::divide. Never string-concatenates.
    pub(crate) fn js_divide(
        &mut self,
        lhs: RegisterValue,
        rhs: RegisterValue,
    ) -> Result<RegisterValue, InterpreterError> {
        if let (Some(l), Some(r)) = (lhs.as_number(), rhs.as_number()) {
            return Ok(RegisterValue::from_number(l / r));
        }
        if lhs.is_bigint() && rhs.is_bigint() {
            return self.bigint_checked_div(lhs, rhs);
        }
        self.js_numeric_binop_fallback(
            lhs,
            rhs,
            |l, r| l / r,
            "/",
            |a, b| {
                // §6.1.6.2.10 BigInt::divide — /0n throws RangeError per spec.
                // The pure-bigint branch above handles the common path; the
                // fallback only fires after a string→bigint coercion that
                // already guarantees both operands are BigInt-typed.
                if b.is_zero() {
                    Err(InterpreterError::TypeError("Division by zero".into()))
                } else {
                    Ok(a.div_trunc(b))
                }
            },
        )
    }

    /// §13.15.3 ApplyStringOrNumericBinaryOperator for `%`. Same
    /// ToNumeric pipeline as `/`; Number::remainder matches JS's
    /// `%` operator (sign follows the dividend).
    pub(crate) fn js_remainder(
        &mut self,
        lhs: RegisterValue,
        rhs: RegisterValue,
    ) -> Result<RegisterValue, InterpreterError> {
        if let (Some(l), Some(r)) = (lhs.as_number(), rhs.as_number()) {
            return Ok(RegisterValue::from_number(l % r));
        }
        if lhs.is_bigint() && rhs.is_bigint() {
            return self.bigint_checked_rem(lhs, rhs);
        }
        self.js_numeric_binop_fallback(
            lhs,
            rhs,
            |l, r| l % r,
            "%",
            |a, b| {
                if b.is_zero() {
                    Err(InterpreterError::TypeError("Division by zero".into()))
                } else {
                    Ok(a.rem_trunc(b))
                }
            },
        )
    }

    /// §13.15.3 ApplyStringOrNumericBinaryOperator for `**`.
    /// Number::exponentiate maps to `f64::powf`; BigInt::exponentiate
    /// surfaces through `BigIntPayload::pow` with the i64 inline fast path.
    pub(crate) fn js_exponentiate(
        &mut self,
        lhs: RegisterValue,
        rhs: RegisterValue,
    ) -> Result<RegisterValue, InterpreterError> {
        if let (Some(l), Some(r)) = (lhs.as_number(), rhs.as_number()) {
            return Ok(RegisterValue::from_number(l.powf(r)));
        }
        self.js_numeric_binop_fallback(
            lhs,
            rhs,
            |l, r| l.powf(r),
            "**",
            |a, b| {
                // §6.1.6.2.12 BigInt::exponentiate — RangeError on negative
                // exponent. `BigIntPayload::pow` returns `None` for that,
                // which we surface as a TypeError carrying the spec message
                // (the runtime layer wraps it as RangeError).
                a.pow(b).ok_or_else(|| {
                    InterpreterError::TypeError("Exponent must be non-negative".into())
                })
            },
        )
    }

    /// Helper: shared ToNumeric + Number/BigInt dispatch for
    /// `/`, `%`, `**`, `-`, `*`. Returns `RangeError`-shaped
    /// errors only via the built-in BigInt paths; otherwise
    /// f64 arithmetic always succeeds (NaN / Infinity on
    /// degenerate inputs, per spec).
    fn js_numeric_binop_fallback(
        &mut self,
        lhs: RegisterValue,
        rhs: RegisterValue,
        number_op: fn(f64, f64) -> f64,
        op_name: &'static str,
        bigint_op: impl Fn(
            &crate::bigint_value::BigIntPayload,
            &crate::bigint_value::BigIntPayload,
        ) -> Result<
            crate::bigint_value::BigIntPayload,
            InterpreterError,
        >,
    ) -> Result<RegisterValue, InterpreterError> {
        let lprim = self.js_to_primitive_with_hint(lhs, ToPrimitiveHint::Number)?;
        let rprim = self.js_to_primitive_with_hint(rhs, ToPrimitiveHint::Number)?;
        if lprim.is_bigint() && rprim.is_bigint() {
            // Borrow the payloads, run the closure, then alloc the result.
            let result = {
                let lhs_handle = ObjectHandle(lprim.as_bigint_handle().unwrap());
                let rhs_handle = ObjectHandle(rprim.as_bigint_handle().unwrap());
                let lpay = self
                    .objects
                    .bigint_value(lhs_handle)?
                    .ok_or(InterpreterError::InvalidHeapValueKind)?;
                let rpay = self
                    .objects
                    .bigint_value(rhs_handle)?
                    .ok_or(InterpreterError::InvalidHeapValueKind)?;
                bigint_op(lpay, rpay)?
            };
            let handle = self.alloc_bigint(result)?;
            return Ok(RegisterValue::from_bigint_handle(handle.0));
        }
        if lprim.is_bigint() || rprim.is_bigint() {
            return Err(InterpreterError::TypeError(
                format!("Cannot mix BigInt and other types with '{op_name}'").into(),
            ));
        }
        let l = self.js_to_number(lprim)?;
        let r = self.js_to_number(rprim)?;
        Ok(RegisterValue::from_number(number_op(l, r)))
    }

    pub(crate) fn js_typeof(
        &mut self,
        value: RegisterValue,
    ) -> Result<RegisterValue, InterpreterError> {
        let kind = if value == RegisterValue::undefined() {
            "undefined"
        } else if value == RegisterValue::null() {
            "object"
        } else if value.as_bool().is_some() {
            "boolean"
        } else if value.is_symbol() {
            "symbol"
        } else if value.is_bigint() {
            "bigint"
        } else if value.is_string_ref() {
            // Strategy B: TAG_PTR_STRING is a string primitive.
            "string"
        } else if value.as_number().is_some() {
            "number"
        } else if let Some(handle) = value.as_object_handle().map(ObjectHandle) {
            match self.objects.kind(handle)? {
                HeapValueKind::String => "string",
                HeapValueKind::HostFunction
                | HeapValueKind::Closure
                | HeapValueKind::BoundFunction
                | HeapValueKind::PromiseCapabilityFunction
                | HeapValueKind::PromiseCombinatorElement
                | HeapValueKind::PromiseFinallyFunction
                | HeapValueKind::PromiseValueThunk => "function",
                HeapValueKind::Object
                | HeapValueKind::Array
                | HeapValueKind::UpvalueCell
                | HeapValueKind::Iterator
                | HeapValueKind::Promise
                | HeapValueKind::Map
                | HeapValueKind::Set
                | HeapValueKind::MapIterator
                | HeapValueKind::SetIterator
                | HeapValueKind::WeakMap
                | HeapValueKind::WeakSet
                | HeapValueKind::WeakRef
                | HeapValueKind::FinalizationRegistry
                | HeapValueKind::Generator
                | HeapValueKind::AsyncGenerator
                | HeapValueKind::ArrayBuffer
                | HeapValueKind::SharedArrayBuffer
                | HeapValueKind::RegExp
                | HeapValueKind::Proxy
                | HeapValueKind::TypedArray
                | HeapValueKind::DataView
                | HeapValueKind::ErrorStackFrames => "object",
                HeapValueKind::BigInt => "bigint",
            }
        } else {
            "undefined"
        };

        // Strategy B: typeof always returns one of a small set of static
        // strings — emit them as TAG_PTR_STRING values.
        let value = self.alloc_string_value(kind)?;
        Ok(value)
    }
}
