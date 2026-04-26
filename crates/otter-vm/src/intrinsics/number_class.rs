use crate::builders::ClassBuilder;
use crate::descriptors::{
    JsClassDescriptor, NativeBindingDescriptor, NativeBindingTarget, NativeFunctionDescriptor,
    VmNativeCallError,
};
use crate::object::{ObjectHandle, PropertyValue};
use crate::value::RegisterValue;

use super::{
    IntrinsicsError, VmIntrinsics,
    install::{IntrinsicInstallContext, IntrinsicInstaller, install_class_plan},
    string_class::map_interpreter_error,
};

pub(super) static NUMBER_INTRINSIC: NumberIntrinsic = NumberIntrinsic;

const NUMBER_DATA_SLOT: &str = "__otter_number_data__";
const NUMBER_VALUE_OF_ERROR: &str = "Number.prototype.valueOf requires a number receiver";

pub(super) struct NumberIntrinsic;

fn type_error(
    runtime: &mut crate::interpreter::RuntimeState,
    message: &str,
) -> Result<VmNativeCallError, VmNativeCallError> {
    let error = runtime.alloc_type_error(message).map_err(|error| {
        VmNativeCallError::Internal(format!("TypeError allocation failed: {error}").into())
    })?;
    Ok(VmNativeCallError::Thrown(
        RegisterValue::from_object_handle(error.0),
    ))
}

impl IntrinsicInstaller for NumberIntrinsic {
    fn init(
        &self,
        intrinsics: &mut VmIntrinsics,
        cx: &mut IntrinsicInstallContext<'_>,
    ) -> Result<(), IntrinsicsError> {
        let descriptor = number_class_descriptor();
        let plan = ClassBuilder::from_descriptor(&descriptor)
            .expect("Number class descriptors should normalize")
            .build();

        let constructor = if let Some(descriptor) = plan.constructor() {
            let host_function = cx.native_functions.register(descriptor.clone());
            cx.alloc_intrinsic_host_function(host_function, intrinsics.function_prototype())?
        } else {
            cx.alloc_intrinsic_object(Some(intrinsics.object_prototype()))?
        };

        intrinsics.number_constructor = constructor;
        install_class_plan(
            intrinsics.number_prototype(),
            intrinsics.number_constructor(),
            &plan,
            intrinsics.function_prototype(),
            cx,
        )?;
        initialize_number_prototype(intrinsics, cx)?;
        initialize_number_constructor(intrinsics, cx)?;

        Ok(())
    }

    fn install_on_global(
        &self,
        intrinsics: &VmIntrinsics,
        cx: &mut IntrinsicInstallContext<'_>,
    ) -> Result<(), IntrinsicsError> {
        cx.install_global_value(
            intrinsics,
            "Number",
            RegisterValue::from_object_handle(intrinsics.number_constructor().0),
        )
    }
}

fn number_class_descriptor() -> JsClassDescriptor {
    JsClassDescriptor::new("Number")
        .with_constructor(
            NativeFunctionDescriptor::constructor("Number", 1, number_constructor)
                .with_default_intrinsic(crate::intrinsics::IntrinsicKey::NumberPrototype),
        )
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("valueOf", 0, number_value_of),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("toString", 1, number_to_string),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("toLocaleString", 0, number_to_locale_string),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("toFixed", 1, number_to_fixed),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("toPrecision", 1, number_to_precision),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("toExponential", 1, number_to_exponential),
        ))
}

/// ES2024 §21.1.3.3 Number.prototype.toFixed(fractionDigits)
fn number_to_fixed(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let number = this_number_value(*this, runtime)?;
    let digits = args
        .first()
        .copied()
        .and_then(|v| v.as_i32().or_else(|| v.as_number().map(|n| n as i32)))
        .unwrap_or(0);
    if !(0..=100).contains(&digits) {
        return Err(type_error(
            runtime,
            "toFixed() digits argument must be between 0 and 100",
        )?);
    }
    if number.is_nan() {
        let value = runtime
            .alloc_string_value("NaN")
            .map_err(|e| map_interpreter_error(e, runtime))?;
        return Ok(value);
    }
    if number.is_infinite() {
        let s = if number > 0.0 {
            "Infinity"
        } else {
            "-Infinity"
        };
        let value = runtime
            .alloc_string_value(s)
            .map_err(|e| map_interpreter_error(e, runtime))?;
        return Ok(value);
    }
    let text = format!("{number:.prec$}", prec = digits as usize);
    let value = runtime
        .alloc_string_value(&text)
        .map_err(|e| map_interpreter_error(e, runtime))?;
    Ok(value)
}

/// ES2024 §21.1.3.5 Number.prototype.toPrecision(precision)
fn number_to_precision(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let number = this_number_value(*this, runtime)?;
    let precision = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    if precision == RegisterValue::undefined() {
        let text = number_to_decimal_string(number);
        let value = runtime
            .alloc_string_value(&text)
            .map_err(|e| map_interpreter_error(e, runtime))?;
        return Ok(value);
    }
    let p = precision
        .as_i32()
        .or_else(|| precision.as_number().map(|n| n as i32))
        .unwrap_or(0);
    if !(1..=100).contains(&p) {
        return Err(type_error(
            runtime,
            "toPrecision() argument must be between 1 and 100",
        )?);
    }
    if number.is_nan() || number.is_infinite() {
        let text = number_to_decimal_string(number);
        let value = runtime
            .alloc_string_value(&text)
            .map_err(|e| map_interpreter_error(e, runtime))?;
        return Ok(value);
    }
    let text = format!("{number:.prec$e}", prec = (p as usize).saturating_sub(1));
    // Rust uses `e1` for positive exponents; ES requires `e+1`.
    let text = if let Some(pos) = text.find('e') {
        let after_e = &text[pos + 1..];
        if !after_e.starts_with('-') && !after_e.starts_with('+') {
            format!("{}e+{}", &text[..pos], after_e)
        } else {
            text
        }
    } else {
        text
    };
    // Rust's exponential formatting differs from JS — convert to JS-style.
    // For simplicity, use the precision-based formatting for reasonable ranges.
    let text = if number.abs() < 1e-6 || number.abs() >= 1e21 {
        text
    } else {
        format!(
            "{number:.prec$}",
            prec =
                (p as usize).saturating_sub(1 + (number.abs().log10().floor().max(0.0) as usize))
        )
    };
    let value = runtime
        .alloc_string_value(&text)
        .map_err(|e| map_interpreter_error(e, runtime))?;
    Ok(value)
}

/// ES2024 §21.1.3.2 Number.prototype.toExponential(fractionDigits)
fn number_to_exponential(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let number = this_number_value(*this, runtime)?;
    if number.is_nan() || number.is_infinite() {
        let text = number_to_decimal_string(number);
        let value = runtime
            .alloc_string_value(&text)
            .map_err(|e| map_interpreter_error(e, runtime))?;
        return Ok(value);
    }
    let frac = args.first().copied().and_then(|v| {
        if v == RegisterValue::undefined() {
            None
        } else {
            v.as_i32()
        }
    });
    let text = match frac {
        Some(f) => {
            if !(0..=100).contains(&f) {
                return Err(type_error(
                    runtime,
                    "toExponential() argument must be between 0 and 100",
                )?);
            }
            format!("{number:.prec$e}", prec = f as usize)
        }
        None => format!("{number:e}"),
    };
    // Rust uses `e1` for positive exponents; ES requires `e+1`.
    let text = if let Some(pos) = text.find('e') {
        let after_e = &text[pos + 1..];
        if !after_e.starts_with('-') && !after_e.starts_with('+') {
            format!("{}e+{}", &text[..pos], after_e)
        } else {
            text
        }
    } else {
        text
    };
    let value = runtime
        .alloc_string_value(&text)
        .map_err(|e| map_interpreter_error(e, runtime))?;
    Ok(value)
}

fn number_constructor(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let primitive = if args.is_empty() {
        RegisterValue::from_i32(0)
    } else {
        coerce_to_number(
            args.first()
                .copied()
                .unwrap_or_else(RegisterValue::undefined),
            runtime,
        )?
    };

    if this.as_object_handle().is_some() {
        box_number_object_with_prototype(
            primitive,
            runtime.subclass_prototype_or_default(*this, runtime.intrinsics().number_prototype()),
            runtime,
        )
    } else {
        Ok(primitive)
    }
}

fn number_value_of(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    if this.as_number().is_some() {
        return Ok(*this);
    }
    if let Some(handle) = this.as_object_handle().map(ObjectHandle)
        && let Some(value) = number_data(handle, runtime)?
    {
        return Ok(value);
    }

    Err(VmNativeCallError::Internal(NUMBER_VALUE_OF_ERROR.into()))
}

// ── §19.1.1 Number.prototype.toLocaleString([locales [, options]]) ──────
//
// ECMA-402 §19.1.1: <https://tc39.es/ecma402/#sup-number.prototype.tolocalestring>

fn number_to_locale_string(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    use fixed_decimal::{Decimal as FixedDecimal, FloatPrecision};
    use icu_decimal::DecimalFormatter;

    let number = this_number_value(*this, runtime)?;

    if number.is_nan() {
        let value = runtime
            .alloc_string_value("NaN")
            .map_err(|e| map_interpreter_error(e, runtime))?;
        return Ok(value);
    }
    if number.is_infinite() {
        let s = if number.is_sign_negative() {
            "-Infinity"
        } else {
            "Infinity"
        };
        let value = runtime
            .alloc_string_value(s)
            .map_err(|e| map_interpreter_error(e, runtime))?;
        return Ok(value);
    }

    // Use ICU4X DecimalFormatter for locale-aware number formatting.
    let result = if let Ok(decimal) = FixedDecimal::try_from_f64(number, FloatPrecision::RoundTrip)
    {
        match DecimalFormatter::try_new(Default::default(), Default::default()) {
            Ok(fmt) => fmt.format(&decimal).to_string(),
            Err(_) => ryu::Buffer::new().format(number).to_string(),
        }
    } else {
        ryu::Buffer::new().format(number).to_string()
    };

    let value = runtime
        .alloc_string_value(&result)
        .map_err(|e| map_interpreter_error(e, runtime))?;
    Ok(value)
}

fn number_to_string(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    // 1. Let x be ? ThisNumberValue(this value).
    let number = this_number_value(*this, runtime)?;

    // 2. If radix is undefined, let radixMV be 10.
    let radix = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let radix_mv = if radix == RegisterValue::undefined() {
        10
    } else {
        // 3. Else, let radixMV be ? ToInteger(radix).
        let r = radix
            .as_i32()
            .or_else(|| radix.as_number().map(|n| n as i32))
            .unwrap_or(10);
        // 4. If radixMV < 2 or radixMV > 36, throw a RangeError.
        if !(2..=36).contains(&r) {
            return Err(type_error(
                runtime,
                "toString() radix must be between 2 and 36",
            )?);
        }
        r as u32
    };

    // 5. If radixMV = 10, return Number::toString(x, 10).
    let text = if radix_mv == 10 {
        number_to_decimal_string(number)
    } else {
        number_to_radix_string(number, radix_mv)
    };

    let value = runtime
        .alloc_string_value(&text)
        .map_err(|e| map_interpreter_error(e, runtime))?;
    Ok(value)
}

/// ES2024 §21.1.3.6 step 1 — thisNumberValue(value).
fn this_number_value(
    value: RegisterValue,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<f64, VmNativeCallError> {
    if let Some(n) = value.as_number() {
        return Ok(n);
    }
    if let Some(n) = value.as_i32() {
        return Ok(n as f64);
    }
    if let Some(handle) = value.as_object_handle().map(ObjectHandle)
        && let Some(prim) = number_data(handle, runtime)?
    {
        if let Some(n) = prim.as_number() {
            return Ok(n);
        }
        if let Some(n) = prim.as_i32() {
            return Ok(n as f64);
        }
    }
    Err(VmNativeCallError::Internal(NUMBER_VALUE_OF_ERROR.into()))
}

/// Formats a number as a decimal string per ES2024 §6.1.6.1.20 Number::toString.
fn number_to_decimal_string(number: f64) -> String {
    crate::abstract_ops::ecma_number_to_string(number)
}

/// Formats a number as a string in the given radix (2..=36).
///
/// §6.1.6.1.20 Number::toString with a non-10 radix. Ported from
/// V8's `DoubleToRadixCString` (src/numbers/conversions.cc). Handles
/// integer values past 2^53 (previously truncated through `as u64`)
/// and fractional values (previously returned decimal / exponential
/// notation instead of the binary/hex/etc. expansion).
fn number_to_radix_string(number: f64, radix: u32) -> String {
    if number.is_nan() {
        return "NaN".to_string();
    }
    if number.is_infinite() {
        return if number.is_sign_positive() {
            "Infinity".to_string()
        } else {
            "-Infinity".to_string()
        };
    }
    if number == 0.0 {
        return "0".to_string();
    }

    const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    // Cap per V8 — guards against pathological delta underflow loops.
    const MAX_FRACTION_DIGITS: usize = 1024;

    let negative = number < 0.0;
    let value = number.abs();
    let radix_f = radix as f64;

    let mut integer_part = value.floor();
    let mut fraction_part = value - integer_part;

    // C11: integer extraction via f64 repeated division — works past
    // 2^53 with the usual Number precision tradeoff (per spec).
    let mut integer_digits: Vec<u8> = Vec::new();
    if integer_part == 0.0 {
        integer_digits.push(b'0');
    } else {
        while integer_part > 0.0 && integer_digits.len() < MAX_FRACTION_DIGITS {
            let q = (integer_part / radix_f).floor();
            // Clamp to guard against off-by-one from f64 rounding at
            // extreme magnitudes (e.g. `(radix^n).toString(radix)`).
            let digit = ((integer_part - q * radix_f) as i64).clamp(0, radix as i64 - 1) as usize;
            integer_digits.push(DIGITS[digit]);
            integer_part = q;
        }
        integer_digits.reverse();
    }

    // Fraction: delta-bounded repeated multiplication. `delta` is half
    // the gap to the next representable f64; once `fraction < delta`,
    // further digits can't distinguish between two adjacent doubles.
    let mut delta = 0.5 * (next_representable_up(value) - value);
    if delta <= 0.0 {
        delta = f64::from_bits(1); // smallest positive subnormal
    }

    let mut fraction_digits: Vec<u8> = Vec::new();
    if fraction_part >= delta {
        fraction_digits.push(b'.');
        while fraction_part >= delta && fraction_digits.len() < MAX_FRACTION_DIGITS + 1 {
            delta *= radix_f;
            fraction_part *= radix_f;
            let mut digit = fraction_part as u32;
            if digit >= radix {
                digit = radix - 1;
            }
            fraction_digits.push(DIGITS[digit as usize]);
            fraction_part -= digit as f64;

            // Round-up condition from V8: if remaining fraction + delta
            // would spill over into the next digit, carry instead of
            // continuing.
            if (fraction_part > 0.5 || (fraction_part == 0.5 && (digit & 1) == 1))
                && fraction_part + delta > 1.0
            {
                round_up_radix_digits(&mut integer_digits, &mut fraction_digits, radix);
                break;
            }
        }
    }

    let mut buf =
        Vec::with_capacity(integer_digits.len() + fraction_digits.len() + usize::from(negative));
    if negative {
        buf.push(b'-');
    }
    buf.extend_from_slice(&integer_digits);
    buf.extend_from_slice(&fraction_digits);
    String::from_utf8(buf).unwrap_or_else(|_| number.to_string())
}

/// Propagate a round-up through the fractional digits, then into the
/// integer digits if the fraction is all radix-1. Drops fraction on
/// full carry (matches V8).
fn round_up_radix_digits(
    integer_digits: &mut Vec<u8>,
    fraction_digits: &mut Vec<u8>,
    radix: u32,
) {
    const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    // Scan fractional from the right.
    while let Some(&last) = fraction_digits.last() {
        if last == b'.' {
            // Carry into integer part.
            fraction_digits.pop(); // drop '.'
            let mut i = integer_digits.len();
            let mut carry = true;
            while i > 0 && carry {
                i -= 1;
                let d = radix_digit_index(integer_digits[i]);
                if d + 1 < radix as usize {
                    integer_digits[i] = DIGITS[d + 1];
                    carry = false;
                } else {
                    integer_digits[i] = b'0';
                }
            }
            if carry {
                integer_digits.insert(0, b'1');
            }
            return;
        }
        let d = radix_digit_index(last);
        if d + 1 < radix as usize {
            *fraction_digits.last_mut().unwrap() = DIGITS[d + 1];
            return;
        }
        // All-max digit rolls over: drop and propagate.
        fraction_digits.pop();
    }
}

fn radix_digit_index(byte: u8) -> usize {
    match byte {
        b'0'..=b'9' => (byte - b'0') as usize,
        b'a'..=b'z' => (byte - b'a') as usize + 10,
        _ => 0,
    }
}

/// Portable `f64::next_up` (stabilised in Rust 1.86 — kept local so the
/// MSRV doesn't have to move for this one call site). Returns the
/// smallest representable value strictly greater than `x`.
fn next_representable_up(x: f64) -> f64 {
    if x.is_nan() || x == f64::INFINITY {
        return x;
    }
    if x == 0.0 {
        return f64::from_bits(1);
    }
    let bits = x.to_bits();
    let next_bits = if x.is_sign_positive() {
        bits + 1
    } else {
        bits - 1
    };
    f64::from_bits(next_bits)
}

fn coerce_to_number(
    value: RegisterValue,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    runtime
        .js_to_number(value)
        .map(RegisterValue::from_number)
        .map_err(|error| match error {
            crate::interpreter::InterpreterError::UncaughtThrow(value) => {
                VmNativeCallError::Thrown(value)
            }
            crate::interpreter::InterpreterError::TypeError(message) => {
                match type_error(runtime, &message) {
                    Ok(error) => error,
                    Err(error) => error,
                }
            }
            other => VmNativeCallError::Internal(format!("{other}").into()),
        })
}

fn initialize_number_prototype(
    intrinsics: &VmIntrinsics,
    cx: &mut IntrinsicInstallContext<'_>,
) -> Result<(), IntrinsicsError> {
    let backing = cx.property_names.intern(NUMBER_DATA_SLOT);
    cx.heap.define_own_property(
        intrinsics.number_prototype(),
        backing,
        crate::object::PropertyValue::data_with_attrs(
            RegisterValue::from_i32(0),
            crate::object::PropertyAttributes::from_flags(true, false, true),
        ),
    )?;
    Ok(())
}

/// ES2024 §21.1.2 Properties of the Number Constructor
fn initialize_number_constructor(
    intrinsics: &VmIntrinsics,
    cx: &mut IntrinsicInstallContext<'_>,
) -> Result<(), IntrinsicsError> {
    let ctor = intrinsics.number_constructor();

    // §21.1.2.1  Number.EPSILON
    // §21.1.2.2  Number.isFinite
    // §21.1.2.3  Number.isInteger
    // §21.1.2.4  Number.isNaN
    // §21.1.2.5  Number.isSafeInteger
    // §21.1.2.6  Number.MAX_SAFE_INTEGER
    // §21.1.2.7  Number.MAX_VALUE
    // §21.1.2.8  Number.MIN_SAFE_INTEGER
    // §21.1.2.9  Number.MIN_VALUE
    // §21.1.2.10 Number.NaN
    // §21.1.2.11 Number.NEGATIVE_INFINITY
    // §21.1.2.12 Number.parseFloat
    // §21.1.2.13 Number.parseInt
    // §21.1.2.14 Number.POSITIVE_INFINITY

    const CONSTANTS: &[(&str, f64)] = &[
        ("EPSILON", f64::EPSILON),
        ("MAX_SAFE_INTEGER", 9_007_199_254_740_991.0), // 2^53 - 1
        ("MAX_VALUE", f64::MAX),
        ("MIN_SAFE_INTEGER", -9_007_199_254_740_991.0), // -(2^53 - 1)
        ("MIN_VALUE", f64::MIN_POSITIVE),               // smallest positive subnormal ≈ 5e-324
        ("NaN", f64::NAN),
        ("NEGATIVE_INFINITY", f64::NEG_INFINITY),
        ("POSITIVE_INFINITY", f64::INFINITY),
    ];

    // ES2024 §21.1.2: Number constructor value properties are {W:false, E:false, C:false}.
    for &(name, value) in CONSTANTS {
        let prop = cx.property_names.intern(name);
        cx.heap.define_own_property(
            ctor,
            prop,
            crate::object::PropertyValue::data_with_attrs(
                RegisterValue::from_number(value),
                crate::object::PropertyAttributes::constant(),
            ),
        )?;
    }

    // Static methods.
    let static_methods: &[(&str, u16, crate::descriptors::VmNativeFunction)] = &[
        ("isFinite", 1, number_is_finite),
        ("isInteger", 1, number_is_integer),
        ("isNaN", 1, number_is_nan),
        ("isSafeInteger", 1, number_is_safe_integer),
        ("parseFloat", 1, number_parse_float),
        ("parseInt", 2, number_parse_int),
    ];

    for &(name, length, callback) in static_methods {
        let descriptor = NativeFunctionDescriptor::method(name, length, callback);
        let host_fn = cx.native_functions.register(descriptor);
        let handle = cx.alloc_intrinsic_host_function(host_fn, intrinsics.function_prototype())?;
        let prop = cx.property_names.intern(name);
        cx.heap
            .set_property(ctor, prop, RegisterValue::from_object_handle(handle.0))?;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Number static methods (ES2024 §21.1.2)
// ---------------------------------------------------------------------------

/// §21.1.2.2 Number.isFinite(number)
fn number_is_finite(
    _this: &RegisterValue,
    args: &[RegisterValue],
    _runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let arg = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    // If Type(number) is not Number, return false.
    let Some(n) = arg.as_number() else {
        return Ok(RegisterValue::from_bool(false));
    };
    Ok(RegisterValue::from_bool(n.is_finite()))
}

/// §21.1.2.3 Number.isInteger(number)
fn number_is_integer(
    _this: &RegisterValue,
    args: &[RegisterValue],
    _runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let arg = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let Some(n) = arg.as_number() else {
        return Ok(RegisterValue::from_bool(false));
    };
    Ok(RegisterValue::from_bool(n.is_finite() && n.trunc() == n))
}

/// §21.1.2.4 Number.isNaN(number)
fn number_is_nan(
    _this: &RegisterValue,
    args: &[RegisterValue],
    _runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let arg = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let Some(n) = arg.as_number() else {
        return Ok(RegisterValue::from_bool(false));
    };
    Ok(RegisterValue::from_bool(n.is_nan()))
}

/// §21.1.2.5 Number.isSafeInteger(number)
fn number_is_safe_integer(
    _this: &RegisterValue,
    args: &[RegisterValue],
    _runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let arg = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let Some(n) = arg.as_number() else {
        return Ok(RegisterValue::from_bool(false));
    };
    Ok(RegisterValue::from_bool(
        n.is_finite() && n.trunc() == n && n.abs() <= 9_007_199_254_740_991.0,
    ))
}

/// §21.1.2.12 Number.parseFloat(string)
fn number_parse_float(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let arg = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let s = runtime.js_to_string_infallible(arg);
    let trimmed = s.trim_start();
    if trimmed.is_empty() {
        return Ok(RegisterValue::from_number(f64::NAN));
    }
    // parseFloat accepts a prefix — find the longest valid float prefix.
    let result = parse_float_prefix(trimmed);
    Ok(RegisterValue::from_number(result))
}

/// §21.1.2.13 Number.parseInt(string, radix)
fn number_parse_int(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let arg = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let s = runtime.js_to_string_infallible(arg);
    let radix_arg = args
        .get(1)
        .copied()
        .unwrap_or_else(RegisterValue::undefined);

    let radix = if radix_arg == RegisterValue::undefined() {
        0 // auto-detect
    } else {
        runtime
            .js_to_int32(radix_arg)
            .map_err(|e| VmNativeCallError::Internal(format!("parseInt radix: {e}").into()))?
    };

    let result = parse_int_impl(&s, radix);
    Ok(RegisterValue::from_number(result))
}

/// §19.2 Global function bindings: isNaN, isFinite, parseFloat, parseInt.
/// These are the *global* versions (coerce to Number first, unlike Number.isNaN).
pub(super) fn global_number_function_bindings() -> Vec<NativeFunctionDescriptor> {
    vec![
        NativeFunctionDescriptor::method("isNaN", 1, global_is_nan),
        NativeFunctionDescriptor::method("isFinite", 1, global_is_finite),
        NativeFunctionDescriptor::method("parseFloat", 1, number_parse_float),
        NativeFunctionDescriptor::method("parseInt", 2, number_parse_int),
    ]
}

/// §19.2.3 globalThis.isNaN(number) — coerces to Number first.
fn global_is_nan(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let arg = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let n = runtime
        .js_to_number(arg)
        .map_err(|e| VmNativeCallError::Internal(format!("isNaN: {e}").into()))?;
    Ok(RegisterValue::from_bool(n.is_nan()))
}

/// §19.2.2 globalThis.isFinite(number) — coerces to Number first.
fn global_is_finite(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let arg = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let n = runtime
        .js_to_number(arg)
        .map_err(|e| VmNativeCallError::Internal(format!("isFinite: {e}").into()))?;
    Ok(RegisterValue::from_bool(n.is_finite()))
}

/// ES spec parseFloat prefix parsing — finds longest valid float prefix.
fn parse_float_prefix(s: &str) -> f64 {
    match s {
        _ if s.starts_with("Infinity") || s.starts_with("+Infinity") => f64::INFINITY,
        _ if s.starts_with("-Infinity") => f64::NEG_INFINITY,
        _ => {
            // Find the longest parseable prefix.
            let mut end = 0;
            let bytes = s.as_bytes();
            let len = bytes.len();

            // Optional sign.
            if end < len && (bytes[end] == b'+' || bytes[end] == b'-') {
                end += 1;
            }
            // Integer part.
            while end < len && bytes[end].is_ascii_digit() {
                end += 1;
            }
            // Decimal point + fraction.
            if end < len && bytes[end] == b'.' {
                end += 1;
                while end < len && bytes[end].is_ascii_digit() {
                    end += 1;
                }
            }
            // Exponent.
            if end < len && (bytes[end] == b'e' || bytes[end] == b'E') {
                let save = end;
                end += 1;
                if end < len && (bytes[end] == b'+' || bytes[end] == b'-') {
                    end += 1;
                }
                if end < len && bytes[end].is_ascii_digit() {
                    while end < len && bytes[end].is_ascii_digit() {
                        end += 1;
                    }
                } else {
                    end = save; // no exponent digits — revert
                }
            }

            if end == 0 || (end == 1 && (bytes[0] == b'+' || bytes[0] == b'-')) {
                f64::NAN
            } else {
                s[..end].parse::<f64>().unwrap_or(f64::NAN)
            }
        }
    }
}

/// ES spec parseInt implementation.
fn parse_int_impl(input: &str, radix: i32) -> f64 {
    let s = input.trim_start();
    if s.is_empty() {
        return f64::NAN;
    }

    let mut chars = s.chars().peekable();
    let sign: f64 = if chars.peek() == Some(&'-') {
        chars.next();
        -1.0
    } else if chars.peek() == Some(&'+') {
        chars.next();
        1.0
    } else {
        1.0
    };

    let radix = if radix == 0 {
        // Auto-detect: 0x → 16, else 10
        if chars
            .clone()
            .take(2)
            .collect::<String>()
            .eq_ignore_ascii_case("0x")
        {
            chars.next(); // skip '0'
            chars.next(); // skip 'x'
            16
        } else {
            10
        }
    } else if !(2..=36).contains(&radix) {
        return f64::NAN;
    } else {
        if radix == 16 {
            // Strip 0x prefix if present
            let rest: String = chars.clone().take(2).collect();
            if rest.eq_ignore_ascii_case("0x") {
                chars.next();
                chars.next();
            }
        }
        radix as u32
    };

    let mut result: f64 = 0.0;
    let mut found_digit = false;

    for ch in chars {
        let digit = match ch.to_ascii_lowercase().to_digit(radix) {
            Some(d) => d,
            None => break, // stop at first invalid char
        };
        found_digit = true;
        result = result * (radix as f64) + (digit as f64);
    }

    if !found_digit {
        f64::NAN
    } else {
        sign * result
    }
}

fn set_number_data(
    receiver: ObjectHandle,
    primitive: RegisterValue,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<(), VmNativeCallError> {
    let backing = runtime.intern_property_name(NUMBER_DATA_SLOT);
    runtime
        .objects_mut()
        .define_own_property(
            receiver,
            backing,
            crate::object::PropertyValue::data_with_attrs(
                primitive,
                crate::object::PropertyAttributes::from_flags(true, false, true),
            ),
        )
        .map_err(|error| {
            VmNativeCallError::Internal(
                format!("Number constructor backing store failed: {error:?}").into(),
            )
        })?;
    Ok(())
}

fn number_data(
    handle: ObjectHandle,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<Option<RegisterValue>, VmNativeCallError> {
    let backing = runtime.intern_property_name(NUMBER_DATA_SLOT);
    let Some(lookup) = runtime
        .objects()
        .get_property(handle, backing)
        .map_err(|error| {
            VmNativeCallError::Internal(format!("Number data lookup failed: {error:?}").into())
        })?
    else {
        return Ok(None);
    };

    let PropertyValue::Data { value, .. } = lookup.value() else {
        return Ok(None);
    };

    Ok(Some(value))
}

pub(crate) fn box_number_object(
    primitive: RegisterValue,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let proto = runtime.intrinsics().number_prototype();
    box_number_object_with_prototype(primitive, proto, runtime)
}

/// Variant of `box_number_object` that installs a caller-specified prototype.
/// Used by the `Number` constructor to honour `newTarget.prototype` via
/// §10.1.13 OrdinaryCreateFromConstructor.
pub(crate) fn box_number_object_with_prototype(
    primitive: RegisterValue,
    prototype: ObjectHandle,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let wrapper = runtime.alloc_object_with_prototype(Some(prototype))?;
    set_number_data(wrapper, primitive, runtime)?;
    Ok(RegisterValue::from_object_handle(wrapper.0))
}
