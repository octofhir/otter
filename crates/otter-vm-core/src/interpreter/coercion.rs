//! Type coercion and conversion methods for the interpreter.
//!
//! Contains implementations of ES2023 abstract operations like ToPrimitive,
//! ToString, ToNumber, ToNumeric, and equality comparison algorithms.

use super::*;

pub(crate) enum Numeric {
    Number(f64),
    BigInt(NumBigInt),
}

/// Maximum recursion depth for abstract equality comparison.
/// Prevents stack overflow from malicious valueOf/toString chains.
const MAX_ABSTRACT_EQUAL_DEPTH: usize = 128;

impl Interpreter {
    /// Convert value to primitive per ES2023 §7.1.1.
    pub(crate) fn to_primitive(
        &self,
        ctx: &mut VmContext,
        value: &Value,
        hint: PreferredType,
    ) -> VmResult<Value> {
        if !value.is_object() {
            return Ok(value.clone());
        }

        // Handle proxy: use proxy_get for property lookups
        if let Some(proxy) = value.as_proxy() {
            // 1. @@toPrimitive
            let to_prim_key =
                PropertyKey::Symbol(crate::intrinsics::well_known::to_primitive_symbol());
            let to_prim_key_value =
                Value::symbol(crate::intrinsics::well_known::to_primitive_symbol());
            let method = {
                let mut ncx = crate::context::NativeContext::new(ctx, self);
                crate::proxy_operations::proxy_get(
                    &mut ncx,
                    proxy,
                    &to_prim_key,
                    to_prim_key_value,
                    value.clone(),
                )?
            };
            if !method.is_undefined() && !method.is_null() {
                if !method.is_callable() {
                    return Err(VmError::type_error(
                        "Cannot convert object to primitive value",
                    ));
                }
                let hint_str = match hint {
                    PreferredType::Default => "default",
                    PreferredType::Number => "number",
                    PreferredType::String => "string",
                };
                let hint_val = Value::string(JsString::intern(hint_str));
                let result = self.call_function(ctx, &method, value.clone(), &[hint_val])?;
                if !result.is_object() {
                    return Ok(result);
                }
                return Err(VmError::type_error(
                    "Cannot convert object to primitive value",
                ));
            }

            // 2. OrdinaryToPrimitive via proxy
            let (first, second) = match hint {
                PreferredType::String => ("toString", "valueOf"),
                _ => ("valueOf", "toString"),
            };
            for name in [first, second] {
                let key = PropertyKey::string(name);
                let key_value = Value::string(JsString::intern(name));
                let method = {
                    let mut ncx = crate::context::NativeContext::new(ctx, self);
                    crate::proxy_operations::proxy_get(
                        &mut ncx,
                        proxy,
                        &key,
                        key_value,
                        value.clone(),
                    )?
                };
                if method.is_callable() {
                    let result = self.call_function(ctx, &method, value.clone(), &[])?;
                    if !result.is_object() {
                        return Ok(result);
                    }
                }
            }

            return Err(VmError::type_error(
                "Cannot convert object to primitive value",
            ));
        }

        let Some(obj) = value.as_object() else {
            return Ok(value.clone());
        };

        // 1. @@toPrimitive
        let to_prim_key = PropertyKey::Symbol(crate::intrinsics::well_known::to_primitive_symbol());
        let method = self.get_property_value(ctx, &obj, &to_prim_key, value)?;
        if !method.is_undefined() && !method.is_null() {
            if !method.is_callable() {
                return Err(VmError::type_error(
                    "Cannot convert object to primitive value",
                ));
            }
            let hint_str = match hint {
                PreferredType::Default => "default",
                PreferredType::Number => "number",
                PreferredType::String => "string",
            };
            let hint_val = Value::string(JsString::intern(hint_str));
            let result = self.call_function(ctx, &method, value.clone(), &[hint_val])?;
            if !result.is_object() {
                return Ok(result);
            }
            return Err(VmError::type_error(
                "Cannot convert object to primitive value",
            ));
        }

        // 2. OrdinaryToPrimitive.
        let (first, second) = match hint {
            PreferredType::String => ("toString", "valueOf"),
            _ => ("valueOf", "toString"),
        };
        for name in [first, second] {
            let method = self.get_property_value(ctx, &obj, &PropertyKey::string(name), value)?;
            if method.is_callable() {
                let result = self.call_function(ctx, &method, value.clone(), &[])?;
                if !result.is_object() {
                    return Ok(result);
                }
            }
        }

        Err(VmError::type_error(
            "Cannot convert object to primitive value",
        ))
    }

    /// Convert value to string per ES2023 §7.1.17.
    pub(crate) fn to_string_value(&self, ctx: &mut VmContext, value: &Value) -> VmResult<String> {
        if value.is_undefined() {
            return Ok("undefined".to_string());
        }
        if value.is_null() {
            return Ok("null".to_string());
        }
        if let Some(b) = value.as_boolean() {
            return Ok(if b { "true" } else { "false" }.to_string());
        }
        if let Some(n) = value.as_number() {
            return Ok(crate::globals::js_number_to_string(n));
        }
        if let Some(s) = value.as_string() {
            return Ok(s.as_str().to_string());
        }
        if let Some(b) = value.as_bigint() {
            return Ok(b.value.clone());
        }
        if value.is_symbol() {
            return Err(VmError::type_error(
                "Cannot convert a Symbol value to a string",
            ));
        }
        if value.is_object() {
            let prim = self.to_primitive(ctx, value, PreferredType::String)?;
            return self.to_string_value(ctx, &prim);
        }
        Ok("[object Object]".to_string())
    }

    /// Convert value to UTF-16 code units of ToString(value), preserving lone surrogates
    /// for existing JS string values.
    pub(super) fn to_string_utf16_units(
        &self,
        ctx: &mut VmContext,
        value: &Value,
    ) -> VmResult<Vec<u16>> {
        if let Some(s) = value.as_string() {
            return Ok(s.as_utf16().to_vec());
        }
        Ok(self.to_string_value(ctx, value)?.encode_utf16().collect())
    }

    /// Convert value to number per ES2023 §7.1.4.
    pub(crate) fn to_number_value(&self, ctx: &mut VmContext, value: &Value) -> VmResult<f64> {
        let prim = if value.is_object() {
            self.to_primitive(ctx, value, PreferredType::Number)?
        } else {
            value.clone()
        };
        if prim.is_symbol() {
            return Err(VmError::type_error(
                "Cannot convert a Symbol value to a number",
            ));
        }
        if prim.is_bigint() {
            return Err(VmError::type_error(
                "Cannot convert a BigInt value to a number",
            ));
        }
        Ok(self.to_number(&prim))
    }

    pub(super) fn parse_string_to_number(&self, input: &str) -> f64 {
        let trimmed = input.trim();
        if trimmed.is_empty() {
            return 0.0;
        }

        let (sign, rest) = if let Some(stripped) = trimmed.strip_prefix('-') {
            (-1.0, stripped)
        } else if let Some(stripped) = trimmed.strip_prefix('+') {
            (1.0, stripped)
        } else {
            (1.0, trimmed)
        };

        if rest == "Infinity" {
            return sign * f64::INFINITY;
        }

        let (radix, digits) = if let Some(rest) = rest.strip_prefix("0x") {
            (16, rest)
        } else if let Some(rest) = rest.strip_prefix("0X") {
            (16, rest)
        } else if let Some(rest) = rest.strip_prefix("0o") {
            (8, rest)
        } else if let Some(rest) = rest.strip_prefix("0O") {
            (8, rest)
        } else if let Some(rest) = rest.strip_prefix("0b") {
            (2, rest)
        } else if let Some(rest) = rest.strip_prefix("0B") {
            (2, rest)
        } else {
            (10, "")
        };

        if radix != 10 {
            if digits.is_empty() {
                return f64::NAN;
            }
            // Numeric separators (_) are only valid in source code literals,
            // not in Number() string conversion (ToNumber)
            if digits.contains('_') {
                return f64::NAN;
            }
            if let Some(bigint) = NumBigInt::parse_bytes(digits.as_bytes(), radix) {
                return bigint.to_f64().unwrap_or(f64::INFINITY) * sign;
            }
            return f64::NAN;
        }

        trimmed.parse::<f64>().unwrap_or(f64::NAN)
    }

    /// Create a JavaScript Promise object from an internal promise
    /// This creates an object with _internal field and copies methods from Promise.prototype
    pub(super) fn create_js_promise(&self, ctx: &VmContext, internal: GcRef<JsPromise>) -> Value {
        let obj = GcRef::new(JsObject::new(Value::null()));

        // Set _internal to the raw promise
        let _ = obj.set(PropertyKey::string("_internal"), Value::promise(internal));

        // Try to get Promise.prototype and copy its methods
        if let Some(promise_ctor) = ctx.get_global("Promise").and_then(|v| v.as_object()) {
            if let Some(proto) = promise_ctor
                .get(&PropertyKey::string("prototype"))
                .and_then(|v| v.as_object())
            {
                // Copy then, catch, finally from prototype
                if let Some(then_fn) = proto.get(&PropertyKey::string("then")) {
                    let _ = obj.set(PropertyKey::string("then"), then_fn);
                }
                if let Some(catch_fn) = proto.get(&PropertyKey::string("catch")) {
                    let _ = obj.set(PropertyKey::string("catch"), catch_fn);
                }
                if let Some(finally_fn) = proto.get(&PropertyKey::string("finally")) {
                    let _ = obj.set(PropertyKey::string("finally"), finally_fn);
                }

                // Set prototype for proper inheritance
                obj.set_prototype(Value::object(proto));
            }
        }

        Value::object(obj)
    }

    /// Convert object to primitive using number hint.
    pub(super) fn to_primitive_number(
        &self,
        ctx: &mut VmContext,
        value: &Value,
    ) -> VmResult<Value> {
        self.to_primitive(ctx, value, PreferredType::Number)
    }

    /// Convert primitive value to number (small ToNumber subset).
    /// Does NOT handle objects - for objects, use `to_number_value()` which
    /// invokes ToPrimitive first per ES2023 §7.1.4.
    pub(super) fn to_number(&self, value: &Value) -> f64 {
        if let Some(n) = value.as_number() {
            return n;
        }
        if value.is_undefined() {
            return f64::NAN;
        }
        if value.is_null() {
            return 0.0;
        }
        if let Some(b) = value.as_boolean() {
            return if b { 1.0 } else { 0.0 };
        }
        if let Some(s) = value.as_string() {
            return self.parse_string_to_number(s.as_str());
        }
        // Objects should be converted via to_number_value() which calls ToPrimitive
        f64::NAN
    }

    /// ES2023 §7.1.6 ToInt32 — convert f64 to 32-bit signed integer
    pub(super) fn to_int32_from(&self, n: f64) -> i32 {
        if n.is_nan() || n.is_infinite() || n == 0.0 {
            return 0;
        }
        // Truncate to integer, then wrap to i32 via u32
        let i = n.trunc() as i64;
        (i as u32) as i32
    }

    /// ES2023 §7.1.7 ToUint32 — convert f64 to 32-bit unsigned integer
    pub(super) fn to_uint32_from(&self, n: f64) -> u32 {
        if n.is_nan() || n.is_infinite() || n == 0.0 {
            return 0;
        }
        let i = n.trunc() as i64;
        i as u32
    }

    pub(super) fn make_error(&self, ctx: &VmContext, name: &str, message: &str) -> Value {
        let ctor_value = ctx.get_global(name);
        let proto = ctor_value
            .as_ref()
            .and_then(|v| v.as_object())
            .and_then(|obj| obj.get(&PropertyKey::string("prototype")))
            .and_then(|v| v.as_object());

        let obj = GcRef::new(JsObject::new(
            proto.map(Value::object).unwrap_or_else(Value::null),
        ));
        let _ = obj.set(
            PropertyKey::string("name"),
            Value::string(JsString::intern(name)),
        );
        let _ = obj.set(
            PropertyKey::string("message"),
            Value::string(JsString::intern(message)),
        );
        let stack = if message.is_empty() {
            name.to_string()
        } else {
            format!("{}: {}", name, message)
        };
        let _ = obj.set(
            PropertyKey::string("stack"),
            Value::string(JsString::intern(&stack)),
        );
        let _ = obj.set(PropertyKey::string("__is_error__"), Value::boolean(true));
        let _ = obj.set(
            PropertyKey::string("__error_type__"),
            Value::string(JsString::intern(name)),
        );
        if let Some(ctor) = ctor_value {
            let _ = obj.set(PropertyKey::string("constructor"), ctor);
        }

        Value::object(obj)
    }

    pub(super) fn coerce_number(&self, ctx: &mut VmContext, value: Value) -> VmResult<f64> {
        self.to_number_value(ctx, &value)
    }

    pub(super) fn bigint_value(&self, value: &Value) -> VmResult<Option<NumBigInt>> {
        if let Some(b) = value.as_bigint() {
            let bigint = self.parse_bigint_str(&b.value)?;
            return Ok(Some(bigint));
        }
        Ok(None)
    }

    pub(super) fn to_numeric(&self, ctx: &mut VmContext, value: &Value) -> VmResult<Numeric> {
        let prim = if value.is_object() {
            self.to_primitive(ctx, value, PreferredType::Number)?
        } else {
            value.clone()
        };
        if let Some(bigint) = self.bigint_value(&prim)? {
            return Ok(Numeric::BigInt(bigint));
        }
        if prim.is_symbol() {
            return Err(VmError::type_error("Cannot convert to number"));
        }
        Ok(Numeric::Number(self.to_number(&prim)))
    }

    pub(super) fn numeric_compare(
        &self,
        left: Numeric,
        right: Numeric,
    ) -> VmResult<Option<Ordering>> {
        match (left, right) {
            (Numeric::Number(left), Numeric::Number(right)) => {
                if left.is_nan() || right.is_nan() {
                    Ok(None)
                } else {
                    Ok(left.partial_cmp(&right))
                }
            }
            (Numeric::BigInt(left), Numeric::BigInt(right)) => Ok(Some(left.cmp(&right))),
            (Numeric::BigInt(left), Numeric::Number(right)) => {
                Ok(self.compare_bigint_number(&left, right))
            }
            (Numeric::Number(left), Numeric::BigInt(right)) => Ok(self
                .compare_bigint_number(&right, left)
                .map(|ordering| ordering.reverse())),
        }
    }

    pub(super) fn compare_bigint_number(
        &self,
        bigint: &NumBigInt,
        number: f64,
    ) -> Option<Ordering> {
        if number.is_nan() {
            return None;
        }
        if number.is_infinite() {
            return Some(if number.is_sign_positive() {
                Ordering::Less
            } else {
                Ordering::Greater
            });
        }
        let (numerator, denominator) = self.f64_to_ratio(number);
        let scaled = bigint * denominator;
        Some(scaled.cmp(&numerator))
    }

    pub(crate) fn parse_bigint_str(&self, value: &str) -> VmResult<NumBigInt> {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            return Ok(num_bigint::BigInt::from(0));
        }

        let (sign, digits, had_sign) = if let Some(rest) = trimmed.strip_prefix('-') {
            (true, rest, true)
        } else if let Some(rest) = trimmed.strip_prefix('+') {
            (false, rest, true)
        } else {
            (false, trimmed, false)
        };

        if had_sign {
            if digits.starts_with("0x")
                || digits.starts_with("0X")
                || digits.starts_with("0o")
                || digits.starts_with("0O")
                || digits.starts_with("0b")
                || digits.starts_with("0B")
            {
                return Err(VmError::syntax_error("Invalid BigInt"));
            }
        }

        let (radix, digits) = if let Some(rest) = digits.strip_prefix("0x") {
            (16, rest)
        } else if let Some(rest) = digits.strip_prefix("0X") {
            (16, rest)
        } else if let Some(rest) = digits.strip_prefix("0o") {
            (8, rest)
        } else if let Some(rest) = digits.strip_prefix("0O") {
            (8, rest)
        } else if let Some(rest) = digits.strip_prefix("0b") {
            (2, rest)
        } else if let Some(rest) = digits.strip_prefix("0B") {
            (2, rest)
        } else {
            (10, digits)
        };

        let cleaned: String = digits.chars().filter(|c| *c != '_').collect();
        if cleaned.is_empty() {
            return Err(VmError::syntax_error("Invalid BigInt"));
        }
        let mut bigint = NumBigInt::parse_bytes(cleaned.as_bytes(), radix)
            .ok_or_else(|| VmError::syntax_error("Invalid BigInt"))?;
        if sign {
            bigint = -bigint;
        }
        Ok(bigint)
    }

    pub(super) fn f64_to_ratio(&self, number: f64) -> (NumBigInt, NumBigInt) {
        if number == 0.0 {
            return (NumBigInt::zero(), NumBigInt::one());
        }

        let bits = number.to_bits();
        let sign = (bits >> 63) != 0;
        let exponent = ((bits >> 52) & 0x7ff) as i32;
        let mantissa = bits & 0x000f_ffff_ffff_ffff;

        let (mut numerator, denominator) = if exponent == 0 {
            let exp2 = 1 - 1023 - 52;
            let mut num = NumBigInt::from(mantissa);
            let mut den = NumBigInt::one();
            if exp2 >= 0 {
                num <<= exp2 as usize;
            } else {
                den <<= (-exp2) as usize;
            }
            (num, den)
        } else {
            let significand = (1u64 << 52) | mantissa;
            let exp2 = exponent - 1023 - 52;
            let mut num = NumBigInt::from(significand);
            let mut den = NumBigInt::one();
            if exp2 >= 0 {
                num <<= exp2 as usize;
            } else {
                den <<= (-exp2) as usize;
            }
            (num, den)
        };

        if sign {
            numerator = -numerator;
        }

        (numerator, denominator)
    }

    /// Convert a Value to a PropertyKey for object property access
    pub(super) fn value_to_property_key(
        &self,
        ctx: &mut VmContext,
        value: &Value,
    ) -> VmResult<PropertyKey> {
        if let Some(sym) = value.as_symbol() {
            return Ok(PropertyKey::Symbol(sym));
        }
        let prim = if value.is_object() {
            self.to_primitive(ctx, value, PreferredType::String)?
        } else {
            value.clone()
        };
        if let Some(sym) = prim.as_symbol() {
            return Ok(PropertyKey::Symbol(sym));
        }
        let key_str = self.to_string_value(ctx, &prim)?;
        if let Ok(n) = key_str.parse::<u32>() {
            if n.to_string() == key_str {
                return Ok(PropertyKey::Index(n));
            }
        }
        Ok(PropertyKey::string(&key_str))
    }

    /// Abstract equality comparison (==) per ES2023 §7.2.14 IsLooselyEqual
    ///
    /// # NaN Handling
    /// NaN == NaN returns false (IEEE 754 semantics via f64 comparison)
    ///
    /// # Recursion Protection
    /// Depth-limited to MAX_ABSTRACT_EQUAL_DEPTH to prevent stack overflow
    /// from malicious valueOf/toString implementations.
    pub(super) fn abstract_equal(
        &self,
        ctx: &mut VmContext,
        left: &Value,
        right: &Value,
    ) -> VmResult<bool> {
        self.abstract_equal_impl(ctx, left, right, 0)
    }

    /// Internal implementation with depth tracking
    fn abstract_equal_impl(
        &self,
        ctx: &mut VmContext,
        left: &Value,
        right: &Value,
        depth: usize,
    ) -> VmResult<bool> {
        // Prevent stack overflow from malicious valueOf/toString chains
        if depth > MAX_ABSTRACT_EQUAL_DEPTH {
            return Err(VmError::range_error(
                "Maximum recursion depth exceeded in equality comparison",
            ));
        }

        // Same type fast paths
        if left.is_undefined() && right.is_undefined() {
            return Ok(true);
        }
        if left.is_null() && right.is_null() {
            return Ok(true);
        }
        if left.is_number() && right.is_number() {
            let a = left.as_number().unwrap();
            let b = right.as_number().unwrap();
            // NaN == NaN returns false per IEEE 754
            return Ok(a == b);
        }
        if let (Some(a), Some(b)) = (left.as_string(), right.as_string()) {
            return Ok(a == b);
        }
        if let (Some(a), Some(b)) = (left.as_boolean(), right.as_boolean()) {
            return Ok(a == b);
        }
        if left.is_bigint() && right.is_bigint() {
            let left_bigint = self.bigint_value(left)?.unwrap_or_else(NumBigInt::zero);
            let right_bigint = self.bigint_value(right)?.unwrap_or_else(NumBigInt::zero);
            return Ok(left_bigint == right_bigint);
        }
        if left.is_symbol() && right.is_symbol() {
            return Ok(left == right);
        }
        if left.is_object() && right.is_object() {
            return Ok(self.strict_equal(left, right));
        }

        // null == undefined
        if (left.is_null() && right.is_undefined()) || (left.is_undefined() && right.is_null()) {
            return Ok(true);
        }

        // [[IsHTMLDDA]] == null/undefined (Annex B)
        // IsHTMLDDA objects are treated as null/undefined for abstract equality.
        if left.is_htmldda() && (right.is_null() || right.is_undefined()) {
            return Ok(true);
        }
        if right.is_htmldda() && (left.is_null() || left.is_undefined()) {
            return Ok(true);
        }

        // Number <-> String
        if left.is_number() && right.is_string() {
            let right_num = self.to_number(right);
            let left_num = left.as_number().unwrap();
            return Ok(left_num == right_num);
        }
        if left.is_string() && right.is_number() {
            let left_num = self.to_number(left);
            let right_num = right.as_number().unwrap();
            return Ok(left_num == right_num);
        }

        // BigInt <-> String
        if left.is_bigint() && right.is_string() {
            let right_str = right.as_string().unwrap();
            if let Ok(parsed) = self.parse_bigint_str(right_str.as_str()) {
                let left_bigint = self.bigint_value(left)?.unwrap_or_else(NumBigInt::zero);
                return Ok(left_bigint == parsed);
            }
            return Ok(false);
        }
        if left.is_string() && right.is_bigint() {
            let left_str = left.as_string().unwrap();
            if let Ok(parsed) = self.parse_bigint_str(left_str.as_str()) {
                let right_bigint = self.bigint_value(right)?.unwrap_or_else(NumBigInt::zero);
                return Ok(parsed == right_bigint);
            }
            return Ok(false);
        }

        // BigInt <-> Number
        if left.is_bigint() && right.is_number() {
            let right_num = right.as_number().unwrap();
            let left_bigint = self.bigint_value(left)?.unwrap_or_else(NumBigInt::zero);
            return Ok(matches!(
                self.compare_bigint_number(&left_bigint, right_num),
                Some(Ordering::Equal)
            ));
        }
        if left.is_number() && right.is_bigint() {
            let left_num = left.as_number().unwrap();
            let right_bigint = self.bigint_value(right)?.unwrap_or_else(NumBigInt::zero);
            return Ok(matches!(
                self.compare_bigint_number(&right_bigint, left_num),
                Some(Ordering::Equal)
            ));
        }

        // Boolean -> ToNumber, recurse
        if let Some(b) = left.as_boolean() {
            let num = if b { 1.0 } else { 0.0 };
            return self.abstract_equal_impl(ctx, &Value::number(num), right, depth + 1);
        }
        if let Some(b) = right.as_boolean() {
            let num = if b { 1.0 } else { 0.0 };
            return self.abstract_equal_impl(ctx, left, &Value::number(num), depth + 1);
        }

        // Object <-> Primitive: ToPrimitive, recurse
        if left.is_object() && !right.is_object() {
            let prim = self.to_primitive(ctx, left, PreferredType::Default)?;
            return self.abstract_equal_impl(ctx, &prim, right, depth + 1);
        }
        if right.is_object() && !left.is_object() {
            let prim = self.to_primitive(ctx, right, PreferredType::Default)?;
            return self.abstract_equal_impl(ctx, left, &prim, depth + 1);
        }

        // Symbol with non-symbol
        if left.is_symbol() || right.is_symbol() {
            return Ok(false);
        }

        Ok(false)
    }

    /// Strict equality comparison (===)
    #[inline(always)]
    pub(super) fn strict_equal(&self, left: &Value, right: &Value) -> bool {
        // Value::PartialEq already matches strict-equality behavior:
        // - same-tag primitives compare by bits/number value
        // - object/function/symbol identity compares by pointer bits
        // - different kinds fall through to false
        left == right
    }
}
