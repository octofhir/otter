//! BigInt constructor and prototype intrinsics.
//!
//! §21.2 BigInt Objects
//! <https://tc39.es/ecma262/#sec-bigint-objects>
//!
//! BigInt is a primitive type — `BigInt()` is a conversion function, NOT a
//! constructor.  `new BigInt()` must throw TypeError.

use crate::descriptors::{
    JsClassDescriptor, NativeBindingDescriptor, NativeBindingTarget, NativeFunctionDescriptor,
    VmNativeCallError,
};
use crate::object::{ObjectHandle, PropertyAttributes, PropertyValue};
use crate::value::RegisterValue;

use super::{
    IntrinsicsError, VmIntrinsics, WellKnownSymbol,
    install::{IntrinsicInstallContext, IntrinsicInstaller, install_class_plan},
};

pub(super) static BIGINT_INTRINSIC: BigIntIntrinsic = BigIntIntrinsic;

pub(super) struct BigIntIntrinsic;

impl IntrinsicInstaller for BigIntIntrinsic {
    fn init(
        &self,
        intrinsics: &mut VmIntrinsics,
        cx: &mut IntrinsicInstallContext<'_>,
    ) -> Result<(), IntrinsicsError> {
        let descriptor = bigint_class_descriptor();
        let plan = crate::builders::ClassBuilder::from_descriptor(&descriptor)
            .expect("BigInt class descriptor should normalize")
            .build();

        if let Some(ctor_desc) = plan.constructor() {
            let host_id = cx.native_functions.register(ctor_desc.clone());
            intrinsics.bigint_constructor =
                cx.alloc_intrinsic_host_function(host_id, intrinsics.function_prototype())?;
        }

        install_class_plan(
            intrinsics.bigint_prototype,
            intrinsics.bigint_constructor,
            &plan,
            intrinsics.function_prototype,
            cx,
        )?;

        // §21.2.3 — @@toStringTag = "BigInt"
        let tag_symbol = cx
            .property_names
            .intern_symbol(WellKnownSymbol::ToStringTag.stable_id());
        let tag_str = cx.heap.alloc_string("BigInt");
        cx.heap.define_own_property(
            intrinsics.bigint_prototype,
            tag_symbol,
            PropertyValue::data_with_attrs(
                RegisterValue::from_object_handle(tag_str.0),
                PropertyAttributes::from_flags(false, false, true),
            ),
        )?;

        Ok(())
    }

    fn install_on_global(
        &self,
        intrinsics: &VmIntrinsics,
        cx: &mut IntrinsicInstallContext<'_>,
    ) -> Result<(), IntrinsicsError> {
        cx.install_global_value(
            intrinsics,
            "BigInt",
            RegisterValue::from_object_handle(intrinsics.bigint_constructor.0),
        )
    }
}

fn proto(
    name: &str,
    arity: u16,
    f: fn(
        &RegisterValue,
        &[RegisterValue],
        &mut crate::interpreter::RuntimeState,
    ) -> Result<RegisterValue, VmNativeCallError>,
) -> NativeBindingDescriptor {
    NativeBindingDescriptor::new(
        NativeBindingTarget::Prototype,
        NativeFunctionDescriptor::method(name, arity, f),
    )
}

fn stat(
    name: &str,
    arity: u16,
    f: fn(
        &RegisterValue,
        &[RegisterValue],
        &mut crate::interpreter::RuntimeState,
    ) -> Result<RegisterValue, VmNativeCallError>,
) -> NativeBindingDescriptor {
    NativeBindingDescriptor::new(
        NativeBindingTarget::Constructor,
        NativeFunctionDescriptor::method(name, arity, f),
    )
}

fn bigint_class_descriptor() -> JsClassDescriptor {
    JsClassDescriptor::new("BigInt")
        .with_constructor(
            NativeFunctionDescriptor::constructor("BigInt", 1, bigint_constructor)
                .with_default_intrinsic(crate::intrinsics::IntrinsicKey::BigIntPrototype),
        )
        .with_binding(proto("toString", 0, bigint_to_string))
        .with_binding(proto("toLocaleString", 0, bigint_to_locale_string))
        .with_binding(proto("valueOf", 0, bigint_value_of))
        .with_binding(stat("asIntN", 2, bigint_as_int_n))
        .with_binding(stat("asUintN", 2, bigint_as_uint_n))
}

// ─── Helpers ─────────────────────────────────────────────────────────

fn type_error(runtime: &mut crate::interpreter::RuntimeState, msg: &str) -> VmNativeCallError {
    match runtime.alloc_type_error(msg) {
        Ok(handle) => VmNativeCallError::Thrown(RegisterValue::from_object_handle(handle.0)),
        Err(error) => {
            VmNativeCallError::Internal(format!("TypeError alloc failed: {error}").into())
        }
    }
}

fn range_error(runtime: &mut crate::interpreter::RuntimeState, msg: &str) -> VmNativeCallError {
    let prototype = runtime.intrinsics().range_error_prototype;
    let handle = runtime.alloc_object_with_prototype(Some(prototype));
    let msg_str = runtime.alloc_string(msg);
    let msg_prop = runtime.intern_property_name("message");
    runtime
        .objects_mut()
        .set_property(
            handle,
            msg_prop,
            RegisterValue::from_object_handle(msg_str.0),
        )
        .ok();
    VmNativeCallError::Thrown(RegisterValue::from_object_handle(handle.0))
}

/// Extracts a BigInt value string from a register, or throws TypeError.
fn require_bigint_value<'a>(
    value: &RegisterValue,
    runtime: &'a crate::interpreter::RuntimeState,
) -> Result<&'a str, VmNativeCallError> {
    let handle = value
        .as_bigint_handle()
        .ok_or_else(|| VmNativeCallError::Internal("expected BigInt value".into()))?;
    runtime
        .bigint_value(ObjectHandle(handle))
        .ok_or_else(|| VmNativeCallError::Internal("invalid BigInt handle".into()))
}

/// §7.1.13 ToBigInt(argument)
/// <https://tc39.es/ecma262/#sec-tobigint>
fn to_bigint(
    value: RegisterValue,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    // undefined, null, Number, Symbol → TypeError.
    if value == RegisterValue::undefined() {
        return Err(type_error(runtime, "Cannot convert undefined to a BigInt"));
    }
    if value == RegisterValue::null() {
        return Err(type_error(runtime, "Cannot convert null to a BigInt"));
    }
    if value.as_number().is_some() {
        return Err(type_error(
            runtime,
            "Cannot convert a Number value to a BigInt; use BigInt(int) instead",
        ));
    }
    if value.is_symbol() {
        return Err(type_error(
            runtime,
            "Cannot convert a Symbol value to a BigInt",
        ));
    }

    // Boolean → 0n / 1n.
    if let Some(b) = value.as_bool() {
        let val = if b { "1" } else { "0" };
        let handle = runtime.alloc_bigint(val);
        return Ok(RegisterValue::from_bigint_handle(handle.0));
    }

    // BigInt → identity.
    if value.is_bigint() {
        return Ok(value);
    }

    // String → parse or throw SyntaxError.
    if let Some(handle) = value.as_object_handle().map(ObjectHandle)
        && let Ok(Some(s)) = runtime.objects().string_value(handle)
    {
        let s = s.trim().to_string();
        if let Ok(_val) = s.parse::<num_bigint::BigInt>() {
            let result = runtime.alloc_bigint(&s);
            return Ok(RegisterValue::from_bigint_handle(result.0));
        }
        return Err(type_error(
            runtime,
            &format!("Cannot convert {s} to a BigInt"),
        ));
    }

    Err(type_error(runtime, "Cannot convert value to a BigInt"))
}

// ─── Constructor ─────────────────────────────────────────────────────

/// §21.2.1.1 BigInt(value)
/// <https://tc39.es/ecma262/#sec-bigint-constructor-number-value>
///
/// BigInt is NOT constructable — `new BigInt()` throws TypeError.
/// As a function, it converts the argument to a BigInt.
fn bigint_constructor(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    // §21.2.1.1 step 1: If NewTarget is not undefined, throw TypeError.
    if runtime.is_current_native_construct_call() {
        return Err(type_error(runtime, "BigInt is not a constructor"));
    }

    let value = args.first().copied().unwrap_or(RegisterValue::undefined());

    // §21.2.1.1 step 2: Let prim be ? ToPrimitive(value, number).
    // §21.2.1.1 step 3: If Type(prim) is Number, return ? NumberToBigInt(prim).
    if let Some(n) = value.as_number() {
        return number_to_bigint(n, runtime);
    }
    if value.as_i32().is_some() {
        let n = value.as_i32().unwrap();
        let handle = runtime.alloc_bigint(&n.to_string());
        return Ok(RegisterValue::from_bigint_handle(handle.0));
    }

    // §21.2.1.1 step 4: Otherwise, return ? ToBigInt(prim).
    to_bigint(value, runtime)
}

/// §21.2.1.1.1 NumberToBigInt(number)
/// <https://tc39.es/ecma262/#sec-numbertobigint>
fn number_to_bigint(
    n: f64,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    if !n.is_finite() || n.fract() != 0.0 {
        return Err(range_error(
            runtime,
            &format!("The number {n} cannot be converted to a BigInt because it is not an integer"),
        ));
    }
    let val = n as i64;
    let handle = runtime.alloc_bigint(&val.to_string());
    Ok(RegisterValue::from_bigint_handle(handle.0))
}

// ─── Prototype methods ───────────────────────────────────────────────

/// §21.2.3.2 BigInt.prototype.toString([radix])
/// <https://tc39.es/ecma262/#sec-bigint.prototype.tostring>
fn bigint_to_string(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let value_str = require_bigint_value(this, runtime)?.to_string();
    let parsed: num_bigint::BigInt = value_str
        .parse()
        .map_err(|_| VmNativeCallError::Internal("invalid BigInt value".into()))?;

    let radix = if let Some(r) = args.first() {
        if *r == RegisterValue::undefined() {
            10
        } else {
            let n = r
                .as_number()
                .or_else(|| r.as_i32().map(f64::from))
                .unwrap_or(10.0);
            let n = n as u32;
            if !(2..=36).contains(&n) {
                return Err(range_error(
                    runtime,
                    "toString() radix must be between 2 and 36",
                ));
            }
            n
        }
    } else {
        10
    };

    let text = if radix == 10 {
        parsed.to_string()
    } else {
        bigint_to_radix_string(&parsed, radix)
    };

    let handle = runtime.alloc_string(text);
    Ok(RegisterValue::from_object_handle(handle.0))
}

/// §21.2.3.1 BigInt.prototype.toLocaleString([locales [, options]])
/// <https://tc39.es/ecma262/#sec-bigint.prototype.tolocalestring>
/// ECMA-402 §19.1.1: <https://tc39.es/ecma402/#sup-number.prototype.tolocalestring>
fn bigint_to_locale_string(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    use fixed_decimal::Decimal;
    use icu_decimal::DecimalFormatter;

    let value_str = require_bigint_value(this, runtime)?.to_string();

    // Parse the BigInt decimal string into a FixedDecimal for locale formatting.
    let result = if let Ok(decimal) = value_str.parse::<Decimal>() {
        match DecimalFormatter::try_new(Default::default(), Default::default()) {
            Ok(fmt) => fmt.format(&decimal).to_string(),
            Err(_) => value_str,
        }
    } else {
        value_str
    };

    let handle = runtime.alloc_string(result);
    Ok(RegisterValue::from_object_handle(handle.0))
}

/// §21.2.3.3 BigInt.prototype.valueOf()
/// <https://tc39.es/ecma262/#sec-bigint.prototype.valueof>
fn bigint_value_of(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    if this.is_bigint() {
        return Ok(*this);
    }
    Err(type_error(
        runtime,
        "BigInt.prototype.valueOf requires a BigInt",
    ))
}

// ─── Static methods ──────────────────────────────────────────────────

/// §21.2.2.1 BigInt.asIntN(bits, bigint)
/// <https://tc39.es/ecma262/#sec-bigint.asintn>
fn bigint_as_int_n(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let bits_val = args.first().copied().unwrap_or(RegisterValue::undefined());
    let bigint_val = args.get(1).copied().unwrap_or(RegisterValue::undefined());

    let bits = bits_val
        .as_number()
        .or_else(|| bits_val.as_i32().map(f64::from))
        .unwrap_or(0.0) as u32;

    let value_str = require_bigint_value(&bigint_val, runtime)?;
    let parsed: num_bigint::BigInt = value_str
        .parse()
        .map_err(|_| VmNativeCallError::Internal("invalid BigInt value".into()))?;

    // §21.2.2.1 step 4: Let mod = n modulo 2^bits.
    let modulus = num_bigint::BigInt::from(1) << bits;
    let result = &parsed % &modulus;
    // §21.2.2.1 step 5: If mod >= 2^(bits-1), return mod - 2^bits.
    let half = &modulus >> 1;
    let result = if bits > 0 && result >= half {
        result - modulus
    } else {
        result
    };

    let handle = runtime.alloc_bigint(&result.to_string());
    Ok(RegisterValue::from_bigint_handle(handle.0))
}

/// §21.2.2.2 BigInt.asUintN(bits, bigint)
/// <https://tc39.es/ecma262/#sec-bigint.asuintn>
fn bigint_as_uint_n(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let bits_val = args.first().copied().unwrap_or(RegisterValue::undefined());
    let bigint_val = args.get(1).copied().unwrap_or(RegisterValue::undefined());

    let bits = bits_val
        .as_number()
        .or_else(|| bits_val.as_i32().map(f64::from))
        .unwrap_or(0.0) as u32;

    let value_str = require_bigint_value(&bigint_val, runtime)?;
    let parsed: num_bigint::BigInt = value_str
        .parse()
        .map_err(|_| VmNativeCallError::Internal("invalid BigInt value".into()))?;

    // §21.2.2.2 step 4: Return n modulo 2^bits.
    let modulus = num_bigint::BigInt::from(1) << bits;
    let mut result = &parsed % &modulus;
    if result < num_bigint::BigInt::from(0) {
        result += &modulus;
    }

    let handle = runtime.alloc_bigint(&result.to_string());
    Ok(RegisterValue::from_bigint_handle(handle.0))
}

// ─── Radix conversion ────────────────────────────────────────────────

/// Converts a BigInt to a string in the given radix (2-36).
fn bigint_to_radix_string(value: &num_bigint::BigInt, radix: u32) -> String {
    use num_bigint::Sign;
    use num_traits::Zero;

    if value.is_zero() {
        return "0".to_string();
    }

    let (sign, mut abs) = (value.sign(), value.magnitude().clone());
    let radix_big = num_bigint::BigUint::from(radix);
    let mut digits = Vec::new();

    while !abs.is_zero() {
        let remainder = &abs % &radix_big;
        let digit = remainder.to_u32_digits().first().copied().unwrap_or(0);
        digits.push(std::char::from_digit(digit, radix).unwrap_or('?'));
        abs /= &radix_big;
    }

    digits.reverse();
    let mut result: String = digits.into_iter().collect();
    if sign == Sign::Minus {
        result.insert(0, '-');
    }
    result
}
