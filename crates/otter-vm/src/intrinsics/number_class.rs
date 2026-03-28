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
};

pub(super) static NUMBER_INTRINSIC: NumberIntrinsic = NumberIntrinsic;

const NUMBER_DATA_SLOT: &str = "__otter_number_data__";
const NUMBER_VALUE_OF_ERROR: &str = "Number.prototype.valueOf requires a number receiver";

pub(super) struct NumberIntrinsic;

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
        .with_constructor(NativeFunctionDescriptor::constructor(
            "Number",
            1,
            number_constructor,
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("valueOf", 0, number_value_of),
        ))
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
        )
    };

    if this.as_object_handle().is_some() {
        box_number_object(primitive, runtime)
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

fn coerce_to_number(
    value: RegisterValue,
    runtime: &mut crate::interpreter::RuntimeState,
) -> RegisterValue {
    if value == RegisterValue::undefined() {
        return RegisterValue::from_number(f64::NAN);
    }
    if value == RegisterValue::null() {
        return RegisterValue::from_i32(0);
    }
    if let Some(number) = value.as_number() {
        return RegisterValue::from_number(number);
    }
    if let Some(boolean) = value.as_bool() {
        return RegisterValue::from_i32(if boolean { 1 } else { 0 });
    }
    if let Some(handle) = value.as_object_handle().map(ObjectHandle) {
        if let Some(string) = runtime.objects().string_value(handle).ok().flatten() {
            let trimmed = string.trim();
            if trimmed.is_empty() {
                return RegisterValue::from_i32(0);
            }
            if let Ok(parsed) = trimmed.parse::<f64>() {
                return RegisterValue::from_number(parsed);
            }
            return RegisterValue::from_number(f64::NAN);
        }
        if let Ok(Some(value)) = number_data(handle, runtime) {
            return value;
        }
    }

    RegisterValue::from_number(f64::NAN)
}

fn initialize_number_prototype(
    intrinsics: &VmIntrinsics,
    cx: &mut IntrinsicInstallContext<'_>,
) -> Result<(), IntrinsicsError> {
    let backing = cx.property_names.intern(NUMBER_DATA_SLOT);
    cx.heap.set_property(
        intrinsics.number_prototype(),
        backing,
        RegisterValue::from_i32(0),
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
        .set_property(receiver, backing, primitive)
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
    let wrapper =
        runtime.alloc_object_with_prototype(Some(runtime.intrinsics().number_prototype()));
    set_number_data(wrapper, primitive, runtime)?;
    Ok(RegisterValue::from_object_handle(wrapper.0))
}
