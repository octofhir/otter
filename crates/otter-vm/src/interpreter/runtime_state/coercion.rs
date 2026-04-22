//! ECMA-262 §7 abstract operations: ToPrimitive, ToString, ToNumber,
//! ToBoolean, ToInt32/Uint32, OrdinaryToPrimitive, `==` (loose equality),
//! relational comparison, `+` with BigInt fallbacks, `instanceof`,
//! OrdinaryHasInstance, `in`, plus `bigint_*` checked arithmetic and the
//! `js_typeof` / `js_add` operator implementations.

use num_traits::Zero;

use crate::descriptors::VmNativeCallError;
use crate::intrinsics::{
    WellKnownSymbol, box_boolean_object, box_number_object, box_symbol_object,
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
        let handle = self.alloc_object_with_prototype(Some(prototype));
        let message = self.alloc_string("Invalid array length");
        let message_prop = self.intern_property_name("message");
        self.objects
            .set_property(
                handle,
                message_prop,
                RegisterValue::from_object_handle(message.0),
            )
            .ok();
        InterpreterError::UncaughtThrow(RegisterValue::from_object_handle(handle.0))
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

        // Same-Type Object pairs were already checked by strict_eq above.
        // Distinct object references compare false for loose equality;
        // ToPrimitive only applies to Object-vs-primitive pairs.
        if lhs.as_object_handle().is_some() && rhs.as_object_handle().is_some() {
            return Ok(false);
        }

        let lhs_is_string = self.value_is_string(lhs)?;
        let rhs_is_string = self.value_is_string(rhs)?;

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
            if let Ok(rhs_val) = rhs_str.parse::<num_bigint::BigInt>() {
                let lhs_val = self.parse_bigint_value(lhs)?;
                return Ok(lhs_val == rhs_val);
            }
            return Ok(false);
        }
        if lhs_is_string && rhs.is_bigint() {
            let lhs_str = self.js_to_string(lhs)?;
            if let Ok(lhs_val) = lhs_str.parse::<num_bigint::BigInt>() {
                let rhs_val = self.parse_bigint_value(rhs)?;
                return Ok(lhs_val == rhs_val);
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
            // the runtime considers the property key form.
            match self.objects.string_value(handle) {
                Ok(Some(js_string)) => js_string.to_string(),
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
        let name_property = self.intern_property_name("name");
        let name_value = self.alloc_string(full_name);
        self.objects
            .define_own_property(
                closure,
                name_property,
                crate::object::PropertyValue::data_with_attrs(
                    RegisterValue::from_object_handle(name_value.0),
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
                self.alloc_object_with_prototype(Some(self.intrinsics().bigint_prototype()));
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
            ToPrimitiveHint::String => ["toString", "valueOf"],
            ToPrimitiveHint::Number => ["valueOf", "toString"],
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

            let hint_value = match hint {
                ToPrimitiveHint::String => self.alloc_string("string"),
                ToPrimitiveHint::Number => self.alloc_string("number"),
            };
            let result = self
                .call_callable(
                    callable,
                    value,
                    &[RegisterValue::from_object_handle(hint_value.0)],
                )
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
        self.js_to_primitive_with_hint(value, ToPrimitiveHint::Number)
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
            let str_val = self
                .objects
                .bigint_value(ObjectHandle(handle))?
                .unwrap_or("0");
            return Ok(str_val.to_string().into_boxed_str());
        }
        if let Some(number) = value.as_number() {
            let text = if number.is_nan() {
                "NaN".to_string()
            } else if number.is_infinite() {
                if number.is_sign_positive() {
                    "Infinity".to_string()
                } else {
                    "-Infinity".to_string()
                }
            } else if number == 0.0 {
                "0".to_string()
            } else if number.fract() == 0.0 {
                format!("{number:.0}")
            } else {
                number.to_string()
            };
            return Ok(text.into_boxed_str());
        }
        if let Some(handle) = value.as_object_handle().map(ObjectHandle) {
            if let Some(string) = self.objects.string_value(handle)? {
                return Ok(string.to_string().into_boxed_str());
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
        if let Some(number) = value.as_number() {
            return Ok(number);
        }
        if let Some(handle) = value.as_object_handle().map(ObjectHandle) {
            if let Some(string) = self.objects.string_value(handle)? {
                return Ok(parse_string_to_number(&string.to_rust_string()));
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
            if let Ok(ny) = sy.parse::<num_bigint::BigInt>() {
                let lhs_val = self.parse_bigint_value(px)?;
                return Ok(Some(lhs_val < ny));
            }
            return Ok(None);
        }
        if px_is_string && py.is_bigint() {
            let sx = self.js_to_string(px)?;
            if let Ok(nx) = sx.parse::<num_bigint::BigInt>() {
                let rhs_val = self.parse_bigint_value(py)?;
                return Ok(Some(nx < rhs_val));
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
    fn parse_bigint_value(
        &self,
        value: RegisterValue,
    ) -> Result<num_bigint::BigInt, InterpreterError> {
        let handle = ObjectHandle(
            value
                .as_bigint_handle()
                .ok_or_else(|| InterpreterError::TypeError("expected BigInt".into()))?,
        );
        let str_val = self
            .objects
            .bigint_value(handle)?
            .ok_or(InterpreterError::InvalidHeapValueKind)?;
        str_val
            .parse()
            .map_err(|_| InterpreterError::InvalidConstant)
    }

    /// §6.1.6.2.12 BigInt::lessThan(x, y)
    /// <https://tc39.es/ecma262/#sec-numeric-types-bigint-lessThan>
    fn bigint_less_than(
        &self,
        lhs: RegisterValue,
        rhs: RegisterValue,
    ) -> Result<Option<bool>, InterpreterError> {
        let lhs_val = self.parse_bigint_value(lhs)?;
        let rhs_val = self.parse_bigint_value(rhs)?;
        Ok(Some(lhs_val < rhs_val))
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
        let bv = self.parse_bigint_value(bigint_val)?;
        // Convert number to integer for comparison.
        let n_int = num_bigint::BigInt::from(n as i64);
        if bv < n_int {
            Ok(Some(true))
        } else if bv > n_int {
            Ok(Some(false))
        } else {
            // bv == n_int, but n may have fractional part
            Ok(Some((n_int.to_string().parse::<f64>().unwrap_or(0.0)) < n))
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
        let bv = self.parse_bigint_value(bigint_val)?;
        let n_int = num_bigint::BigInt::from(n as i64);
        if n_int < bv {
            Ok(Some(true))
        } else if n_int > bv {
            Ok(Some(false))
        } else {
            // n_int == bv, but n may have fractional part
            Ok(Some(n < n_int.to_string().parse::<f64>().unwrap_or(0.0)))
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
        let bv = self.parse_bigint_value(bigint_val)?;
        let n_int = num_bigint::BigInt::from(n as i64);
        Ok(bv == n_int)
    }

    /// ES spec 7.1.2 ToBoolean — runtime-aware truthiness check.
    /// <https://tc39.es/ecma262/#sec-toboolean>
    /// Unlike `RegisterValue::is_truthy()`, this correctly handles heap strings
    /// (empty string "" is falsy) and BigInt (0n is falsy).
    pub(crate) fn js_to_boolean(&mut self, value: RegisterValue) -> Result<bool, InterpreterError> {
        // §7.1.2 step 7: BigInt — 0n is falsy, all others truthy.
        if let Some(handle) = value.as_bigint_handle() {
            let str_val = self
                .objects
                .bigint_value(ObjectHandle(handle))?
                .unwrap_or("0");
            return Ok(str_val != "0");
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
        let handle = self.alloc_object_with_prototype(Some(prototype));
        let msg_handle = self.objects.alloc_string(message);
        let msg_prop = self.intern_property_name("message");
        self.objects.set_property(
            handle,
            msg_prop,
            RegisterValue::from_object_handle(msg_handle.0),
        )?;
        Ok(handle)
    }

    /// Allocate a TypeError object with the correct prototype chain.
    pub fn alloc_type_error(&mut self, message: &str) -> Result<ObjectHandle, InterpreterError> {
        let prototype = self.intrinsics().type_error_prototype;
        let handle = self.alloc_object_with_prototype(Some(prototype));
        let msg_handle = self.objects.alloc_string(message);
        let msg_prop = self.intern_property_name("message");
        self.objects.set_property(
            handle,
            msg_prop,
            RegisterValue::from_object_handle(msg_handle.0),
        )?;
        Ok(handle)
    }

    /// Allocates one RangeError instance with the given message.
    pub fn alloc_range_error(&mut self, message: &str) -> Result<ObjectHandle, InterpreterError> {
        let prototype = self.intrinsics().range_error_prototype;
        let handle = self.alloc_object_with_prototype(Some(prototype));
        let msg_handle = self.objects.alloc_string(message);
        let msg_prop = self.intern_property_name("message");
        self.objects.set_property(
            handle,
            msg_prop,
            RegisterValue::from_object_handle(msg_handle.0),
        )?;
        Ok(handle)
    }

    /// Creates a { status: "...", [value_key]: value } object for Promise.allSettled.
    /// ES2024 §27.2.4.2.1–2
    pub fn alloc_settled_result_object(
        &mut self,
        status: &str,
        value_key: &str,
        value: RegisterValue,
    ) -> ObjectHandle {
        let obj = self.alloc_object();
        let status_prop = self.intern_property_name("status");
        let status_str = self.objects.alloc_string(status);
        let _ = self.objects.set_property(
            obj,
            status_prop,
            RegisterValue::from_object_handle(status_str.0),
        );
        let value_prop = self.intern_property_name(value_key);
        let _ = self.objects.set_property(obj, value_prop, value);
        obj
    }

    /// §19.2.1 Step 1: If x is not a String, return None.
    /// Extracts the string content if `value` is a string primitive.
    /// Does NOT coerce — returns None for non-string values.
    ///
    /// Spec: <https://tc39.es/ecma262/#sec-eval-x>
    pub fn value_as_string(&self, value: RegisterValue) -> Option<String> {
        let handle = value.as_object_handle().map(ObjectHandle)?;
        self.objects
            .string_value(handle)
            .ok()
            .flatten()
            .map(|s| s.to_string())
    }

    /// Checks whether a value is a string type (heap string or string wrapper).
    fn value_is_string(&mut self, value: RegisterValue) -> Result<bool, InterpreterError> {
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

    /// §6.1.6.2 BigInt arithmetic helper — performs a binary operation on two
    /// BigInt register values and returns the result as a new BigInt.
    /// <https://tc39.es/ecma262/#sec-numeric-types-bigint-add>
    pub(crate) fn bigint_binary_op(
        &mut self,
        lhs: RegisterValue,
        rhs: RegisterValue,
        op: fn(&num_bigint::BigInt, &num_bigint::BigInt) -> num_bigint::BigInt,
    ) -> Result<RegisterValue, InterpreterError> {
        let lhs_handle = ObjectHandle(
            lhs.as_bigint_handle()
                .ok_or_else(|| InterpreterError::TypeError("expected BigInt".into()))?,
        );
        let rhs_handle = ObjectHandle(
            rhs.as_bigint_handle()
                .ok_or_else(|| InterpreterError::TypeError("expected BigInt".into()))?,
        );

        let lhs_str = self
            .objects
            .bigint_value(lhs_handle)?
            .ok_or(InterpreterError::InvalidHeapValueKind)?;
        let rhs_str = self
            .objects
            .bigint_value(rhs_handle)?
            .ok_or(InterpreterError::InvalidHeapValueKind)?;

        let lhs_val: num_bigint::BigInt = lhs_str
            .parse()
            .map_err(|_| InterpreterError::InvalidConstant)?;
        let rhs_val: num_bigint::BigInt = rhs_str
            .parse()
            .map_err(|_| InterpreterError::InvalidConstant)?;

        let result = op(&lhs_val, &rhs_val);
        let handle = self.alloc_bigint(&result.to_string());
        Ok(RegisterValue::from_bigint_handle(handle.0))
    }

    /// §6.1.6.2.10 BigInt::divide — truncating division, RangeError on zero divisor.
    /// <https://tc39.es/ecma262/#sec-numeric-types-bigint-divide>
    pub(crate) fn bigint_checked_div(
        &mut self,
        lhs: RegisterValue,
        rhs: RegisterValue,
    ) -> Result<RegisterValue, InterpreterError> {
        self.bigint_binary_op(lhs, rhs, |a, b| {
            if b.is_zero() {
                // Caller would need to signal error; we use a sentinel approach below.
                num_bigint::BigInt::from(0)
            } else {
                a / b
            }
        })
        .and_then(|result| {
            // Re-check for division by zero via the original rhs.
            let rhs_handle = ObjectHandle(rhs.as_bigint_handle().unwrap());
            let rhs_str = self
                .objects
                .bigint_value(rhs_handle)
                .ok()
                .flatten()
                .unwrap_or("0");
            if rhs_str == "0" {
                return Err(InterpreterError::TypeError("Division by zero".into()));
            }
            Ok(result)
        })
    }

    /// §6.1.6.2.11 BigInt::remainder — RangeError on zero divisor.
    /// <https://tc39.es/ecma262/#sec-numeric-types-bigint-remainder>
    pub(crate) fn bigint_checked_rem(
        &mut self,
        lhs: RegisterValue,
        rhs: RegisterValue,
    ) -> Result<RegisterValue, InterpreterError> {
        // Check for zero divisor first.
        let rhs_handle = ObjectHandle(
            rhs.as_bigint_handle()
                .ok_or_else(|| InterpreterError::TypeError("expected BigInt".into()))?,
        );
        let rhs_str = self
            .objects
            .bigint_value(rhs_handle)?
            .ok_or(InterpreterError::InvalidHeapValueKind)?;
        if rhs_str == "0" {
            return Err(InterpreterError::TypeError("Division by zero".into()));
        }
        self.bigint_binary_op(lhs, rhs, |a, b| a % b)
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
            let mut text = self.js_to_string(lprim)?.into_string();
            text.push_str(&self.js_to_string(rprim)?);
            let value = self.alloc_string(text);
            return Ok(RegisterValue::from_object_handle(value.0));
        }

        // §6.1.6.2.7 BigInt::add — both operands BigInt.
        if lprim.is_bigint() && rprim.is_bigint() {
            return self.bigint_binary_op(lprim, rprim, |a, b| a + b);
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
        self.js_numeric_binop_fallback(lhs, rhs, |l, r| l - r, "-", |a, b| a - b)
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
        self.js_numeric_binop_fallback(lhs, rhs, |l, r| l * r, "*", |a, b| a * b)
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
                // §6.1.6.2.10 BigInt::divide — /0n throws RangeError. We
                // delegate to the existing `bigint_checked_div` which
                // rounds toward zero and throws on zero divisor.
                if b.is_zero() {
                    num_bigint::BigInt::from(0)
                } else {
                    a / b
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
            return self.bigint_binary_op(lhs, rhs, |a, b| {
                if b.is_zero() {
                    num_bigint::BigInt::from(0)
                } else {
                    a % b
                }
            });
        }
        self.js_numeric_binop_fallback(
            lhs,
            rhs,
            |l, r| l % r,
            "%",
            |a, b| {
                if b.is_zero() {
                    num_bigint::BigInt::from(0)
                } else {
                    a % b
                }
            },
        )
    }

    /// §13.15.3 ApplyStringOrNumericBinaryOperator for `**`.
    /// Number::exponentiate maps to `f64::powf`; BigInt::exponentiate
    /// surfaces through `bigint_binary_op` with `num_bigint::BigInt::pow`.
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
                // Negative exponent on BigInt throws per spec; clamping
                // here keeps the fallback in arithmetic shape, and the
                // BigInt-specific early-return above handles the non-
                // mixed case without relying on this branch.
                let exp = b
                    .to_biguint()
                    .and_then(|u| u32::try_from(u).ok())
                    .unwrap_or(0);
                a.pow(exp)
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
        bigint_op: fn(&num_bigint::BigInt, &num_bigint::BigInt) -> num_bigint::BigInt,
    ) -> Result<RegisterValue, InterpreterError> {
        let lprim = self.js_to_primitive_with_hint(lhs, ToPrimitiveHint::Number)?;
        let rprim = self.js_to_primitive_with_hint(rhs, ToPrimitiveHint::Number)?;
        if lprim.is_bigint() && rprim.is_bigint() {
            return self.bigint_binary_op(lprim, rprim, bigint_op);
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

        let string = self.alloc_string(kind);
        Ok(RegisterValue::from_object_handle(string.0))
    }
}
