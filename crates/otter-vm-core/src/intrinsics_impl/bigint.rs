//! BigInt constructor and prototype implementation

use std::sync::Arc;

use num_bigint::BigInt as NumBigInt;
use num_bigint::Sign;
use num_traits::{FromPrimitive, One, Zero};
use otter_macros::dive;

use crate::context::NativeContext;
use crate::error::VmError;
use crate::gc::GcRef;
use crate::interpreter::PreferredType;
use crate::intrinsics::well_known;
use crate::memory::MemoryManager;
use crate::object::{JsObject, PropertyAttributes, PropertyDescriptor, PropertyKey};
use crate::string::JsString;
use crate::value::Value;

fn bigint_value_from(this_val: &Value) -> Option<Value> {
    if this_val.is_bigint() {
        return Some(this_val.clone());
    }
    if let Some(obj) = this_val.as_object() {
        if let Some(prim) = obj
            .get(&PropertyKey::string("__value__"))
            .or_else(|| obj.get(&PropertyKey::string("__primitiveValue__")))
        {
            if prim.is_bigint() {
                return Some(prim);
            }
        }
    }
    None
}

fn bigint_from_value(ncx: &mut NativeContext<'_>, this_val: &Value) -> Result<NumBigInt, VmError> {
    let prim = bigint_value_from(this_val).ok_or_else(|| {
        VmError::type_error("BigInt.prototype method requires that 'this' be a BigInt")
    })?;
    if let Some(crate::value::HeapRef::BigInt(b)) = prim.heap_ref() {
        return ncx.parse_bigint_str(&b.value);
    }
    Err(VmError::type_error(
        "BigInt.prototype method requires that 'this' be a BigInt",
    ))
}

fn to_bigint(ncx: &mut NativeContext<'_>, value: &Value) -> Result<NumBigInt, VmError> {
    let prim = if value.is_object() {
        ncx.to_primitive(value, PreferredType::Number)?
    } else {
        value.clone()
    };

    if prim.is_bigint() {
        if let Some(crate::value::HeapRef::BigInt(b)) = prim.heap_ref() {
            return ncx.parse_bigint_str(&b.value);
        }
    }
    if prim.is_symbol() {
        return Err(VmError::type_error(
            "Cannot convert a Symbol value to a BigInt",
        ));
    }
    if prim.is_undefined() {
        return Err(VmError::type_error("Cannot convert undefined to a BigInt"));
    }
    if prim.is_null() {
        return Err(VmError::type_error("Cannot convert null to a BigInt"));
    }
    if let Some(b) = prim.as_boolean() {
        return Ok(if b {
            NumBigInt::one()
        } else {
            NumBigInt::zero()
        });
    }
    if let Some(s) = prim.as_string() {
        let bigint = ncx
            .parse_bigint_str(s.as_str())
            .map_err(|_| VmError::syntax_error("Invalid BigInt"))?;
        return Ok(bigint);
    }
    if prim.as_number().is_some() {
        return Err(VmError::type_error("Cannot convert number to a BigInt"));
    }

    Err(VmError::type_error("Cannot convert value to a BigInt"))
}

fn to_index(ncx: &mut NativeContext<'_>, value: &Value) -> Result<usize, VmError> {
    const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

    let prim = if value.is_object() {
        ncx.to_primitive(value, PreferredType::Number)?
    } else {
        value.clone()
    };
    if prim.is_bigint() {
        return Err(VmError::type_error("Invalid index"));
    }
    let number = ncx.to_number_value(&prim)?;
    if number.is_nan() || number == 0.0 {
        return Ok(0);
    }
    if number.is_infinite() {
        return Err(VmError::range_error("Invalid index"));
    }
    let integer = number.trunc();
    if integer < 0.0 {
        return Err(VmError::range_error("Invalid index"));
    }
    if integer > MAX_SAFE_INTEGER {
        return Err(VmError::range_error("Invalid index"));
    }
    if integer > (usize::MAX as f64) {
        return Err(VmError::range_error("Invalid index"));
    }
    Ok(integer as usize)
}

fn set_function_properties(func: &Value, name: &str, length: i32, non_constructor: bool) {
    if let Some(obj) = func.as_object() {
        obj.define_property(
            PropertyKey::string("name"),
            PropertyDescriptor::function_length(Value::string(JsString::intern(name))),
        );
        obj.define_property(
            PropertyKey::string("length"),
            PropertyDescriptor::function_length(Value::int32(length)),
        );
        if non_constructor {
            obj.define_property(
                PropertyKey::string("__non_constructor"),
                PropertyDescriptor::builtin_data(Value::boolean(true)),
            );
        }
    }
}

#[dive(name = "valueOf", length = 0)]
fn bigint_value_of(
    this_val: &Value,
    _args: &[Value],
    _ncx: &mut NativeContext<'_>,
) -> Result<Value, VmError> {
    let prim = bigint_value_from(this_val).ok_or_else(|| {
        VmError::type_error("BigInt.prototype.valueOf requires that 'this' be a BigInt")
    })?;
    Ok(prim)
}

#[dive(name = "toString", length = 0)]
fn bigint_to_string_method(
    this_val: &Value,
    args: &[Value],
    ncx: &mut NativeContext<'_>,
) -> Result<Value, VmError> {
    let bigint = bigint_from_value(ncx, this_val)?;
    let radix = if let Some(arg) = args.first() {
        if arg.is_undefined() {
            10
        } else {
            let num = ncx.to_number_value(arg)?;
            if !num.is_finite() {
                return Err(VmError::range_error(
                    "toString() radix argument must be between 2 and 36",
                ));
            }
            let radix = num.trunc() as i64;
            if !(2..=36).contains(&radix) {
                return Err(VmError::range_error(
                    "toString() radix argument must be between 2 and 36",
                ));
            }
            radix as u32
        }
    } else {
        10
    };
    let s = bigint.to_str_radix(radix);
    Ok(Value::string(JsString::intern(&s)))
}

#[dive(name = "toLocaleString", length = 0)]
fn bigint_to_locale_string_method(
    this_val: &Value,
    _args: &[Value],
    ncx: &mut NativeContext<'_>,
) -> Result<Value, VmError> {
    let bigint = bigint_from_value(ncx, this_val)?;
    let s = bigint.to_str_radix(10);
    Ok(Value::string(JsString::intern(&s)))
}

#[dive(name = "asIntN", length = 2)]
fn bigint_as_int_n(
    _this: &Value,
    args: &[Value],
    ncx: &mut NativeContext<'_>,
) -> Result<Value, VmError> {
    let bits_val = args.first().cloned().unwrap_or(Value::undefined());
    let bigint_val = args.get(1).cloned().unwrap_or(Value::undefined());

    let bits = to_index(ncx, &bits_val)?;
    let bigint = to_bigint(ncx, &bigint_val)?;
    if bits == 0 {
        return Ok(Value::bigint("0".to_string()));
    }

    let modulus = NumBigInt::one() << bits;
    let mut result = bigint % &modulus;
    if result.sign() == Sign::Minus {
        result += &modulus;
    }
    let sign_bit = NumBigInt::one() << (bits - 1);
    if result >= sign_bit {
        result -= &modulus;
    }
    Ok(Value::bigint(result.to_str_radix(10)))
}

#[dive(name = "asUintN", length = 2)]
fn bigint_as_uint_n(
    _this: &Value,
    args: &[Value],
    ncx: &mut NativeContext<'_>,
) -> Result<Value, VmError> {
    let bits_val = args.first().cloned().unwrap_or(Value::undefined());
    let bigint_val = args.get(1).cloned().unwrap_or(Value::undefined());

    let bits = to_index(ncx, &bits_val)?;
    let bigint = to_bigint(ncx, &bigint_val)?;
    if bits == 0 {
        return Ok(Value::bigint("0".to_string()));
    }

    let modulus = NumBigInt::one() << bits;
    let mut result = bigint % &modulus;
    if result.sign() == Sign::Minus {
        result += &modulus;
    }
    Ok(Value::bigint(result.to_str_radix(10)))
}

/// Initialize BigInt.prototype with valueOf and toString.
pub fn init_bigint_prototype(
    bigint_proto: GcRef<JsObject>,
    _fn_proto: GcRef<JsObject>,
    mm: &Arc<MemoryManager>,
) {
    let methods: &[(&str, crate::value::NativeFn, u32)] = &[
        bigint_value_of_decl(),
        bigint_to_string_method_decl(),
        bigint_to_locale_string_method_decl(),
    ];
    for (name, native_fn, length) in methods {
        let fn_val = Value::native_function_from_decl(name, native_fn.clone(), *length, mm.clone());
        bigint_proto.define_property(
            PropertyKey::string(name),
            PropertyDescriptor::builtin_method(fn_val),
        );
    }

    // BigInt.prototype[Symbol.toStringTag] = "BigInt"
    bigint_proto.define_property(
        PropertyKey::Symbol(well_known::to_string_tag_symbol()),
        PropertyDescriptor::data_with_attrs(
            Value::string(JsString::intern("BigInt")),
            PropertyAttributes::function_length(),
        ),
    );
}

/// Install static methods on the BigInt constructor.
pub fn install_bigint_statics(
    bigint_ctor: GcRef<JsObject>,
    _fn_proto: GcRef<JsObject>,
    mm: &Arc<MemoryManager>,
) {
    let methods: &[(&str, crate::value::NativeFn, u32)] =
        &[bigint_as_int_n_decl(), bigint_as_uint_n_decl()];
    for (name, native_fn, length) in methods {
        let fn_val = Value::native_function_from_decl(name, native_fn.clone(), *length, mm.clone());
        bigint_ctor.define_property(
            PropertyKey::string(name),
            PropertyDescriptor::builtin_method(fn_val),
        );
    }

    // BigInt is not a constructor
    let ctor_val = Value::object(bigint_ctor);
    set_function_properties(&ctor_val, "BigInt", 1, false);
}

/// Create BigInt constructor function (callable, not constructable).
pub fn create_bigint_constructor()
-> Box<dyn Fn(&Value, &[Value], &mut NativeContext<'_>) -> Result<Value, VmError> + Send + Sync> {
    Box::new(|_this, args, ncx| {
        if ncx.is_construct() {
            return Err(VmError::type_error("BigInt is not a constructor"));
        }

        let arg = args.first().cloned().unwrap_or(Value::undefined());
        let prim = ncx.to_primitive(&arg, PreferredType::Number)?;

        if prim.is_bigint() {
            return Ok(prim);
        }
        if prim.is_symbol() {
            return Err(VmError::type_error(
                "Cannot convert a Symbol value to a BigInt",
            ));
        }
        if prim.is_undefined() {
            return Err(VmError::type_error("Cannot convert undefined to a BigInt"));
        }
        if prim.is_null() {
            return Ok(Value::bigint("0".to_string()));
        }
        if let Some(b) = prim.as_boolean() {
            return Ok(Value::bigint(if b {
                "1".to_string()
            } else {
                "0".to_string()
            }));
        }
        if let Some(s) = prim.as_string() {
            let bigint = ncx
                .parse_bigint_str(s.as_str())
                .map_err(|_| VmError::syntax_error("Invalid BigInt"))?;
            return Ok(Value::bigint(bigint.to_str_radix(10)));
        }
        if let Some(n) = prim.as_number() {
            if !n.is_finite() || n.fract() != 0.0 {
                return Err(VmError::range_error("Cannot convert number to a BigInt"));
            }
            let bigint = NumBigInt::from_f64(n)
                .ok_or_else(|| VmError::range_error("Cannot convert number to a BigInt"))?;
            return Ok(Value::bigint(bigint.to_str_radix(10)));
        }

        Err(VmError::type_error("Cannot convert value to a BigInt"))
    })
}
