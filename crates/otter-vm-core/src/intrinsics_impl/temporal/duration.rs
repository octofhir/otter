use crate::context::NativeContext;
use crate::error::VmError;
use crate::gc::GcRef;
use crate::memory::MemoryManager;
use crate::object::{JsObject, PropertyAttributes, PropertyDescriptor, PropertyKey};
use crate::string::JsString;
use crate::value::Value;
use std::sync::Arc;
use temporal_rs::options::{RoundingMode, ToStringRoundingOptions, Unit};
use temporal_rs::parsers::Precision;
use temporal_rs::provider::NeverProvider;

use super::common::*;

/// Install Duration constructor, static methods, and prototype onto `temporal_obj`.
///
/// Returns nothing — wires everything into the provided namespace object.
pub(super) fn install_duration(
    temporal_obj: &GcRef<JsObject>,
    obj_proto: &GcRef<JsObject>,
    fn_proto: &GcRef<JsObject>,
    mm: &Arc<MemoryManager>,
) {
    let proto = GcRef::new(JsObject::new(Value::object(obj_proto.clone()), mm.clone()));
    let ctor_obj = GcRef::new(JsObject::new(Value::object(fn_proto.clone()), mm.clone()));

    ctor_obj.define_property(
        PropertyKey::string("prototype"),
        PropertyDescriptor::data_with_attrs(
            Value::object(proto.clone()),
            PropertyAttributes { writable: false, enumerable: false, configurable: false },
        ),
    );
    ctor_obj.define_property(
        PropertyKey::string("name"),
        PropertyDescriptor::function_length(Value::string(JsString::intern("Duration"))),
    );
    ctor_obj.define_property(
        PropertyKey::string("length"),
        PropertyDescriptor::function_length(Value::number(0.0)),
    );

    // ========================================================================
    // Constructor: new Temporal.Duration(y, mo, w, d, h, mi, s, ms, us, ns)
    // ========================================================================
    let ctor_proto = proto.clone();
    let ctor_mm = mm.clone();
    let ctor_fn: Box<
        dyn Fn(&Value, &[Value], &mut NativeContext<'_>) -> Result<Value, VmError> + Send + Sync,
    > = Box::new(move |_this, args, ncx| {
        // Parse each argument with ToIntegerIfIntegral (defaulting to 0)
        let mut vals = [0f64; 10];
        for (i, field) in DURATION_FIELDS.iter().enumerate() {
            if let Some(val) = args.get(i) {
                if !val.is_undefined() {
                    let n = to_integer_if_integral(ncx, val).map_err(|e| {
                        VmError::range_error(format!("Invalid value for {}: {}", field, e))
                    })?;
                    vals[i] = n;
                }
            }
        }

        // Validate with temporal_rs::Duration::new()
        let dur = temporal_rs::Duration::new(
            vals[0] as i64, vals[1] as i64, vals[2] as i64, vals[3] as i64,
            vals[4] as i64, vals[5] as i64, vals[6] as i64, vals[7] as i64,
            vals[8] as i128, vals[9] as i128,
        ).map_err(temporal_err)?;

        let obj = construct_duration_object(&dur, &ctor_proto, &ctor_mm);
        Ok(Value::object(obj))
    });

    let ctor_value = Value::native_function_with_proto_and_object(
        Arc::from(ctor_fn),
        mm.clone(),
        fn_proto.clone(),
        ctor_obj.clone(),
    );

    // prototype.constructor
    proto.define_property(
        PropertyKey::string("constructor"),
        PropertyDescriptor::data_with_attrs(ctor_value.clone(), PropertyAttributes::constructor_link()),
    );

    // ========================================================================
    // Duration.from()
    // ========================================================================
    let from_proto = proto.clone();
    let from_mm = mm.clone();
    let from_fn = Value::native_function_with_proto_named(
        move |_this, args, ncx| {
            let item = args.first().cloned().unwrap_or(Value::undefined());
            if item.is_string() {
                let s = ncx.to_string_value(&item)?;
                let dur = temporal_rs::Duration::from_utf8(s.as_bytes()).map_err(temporal_err)?;
                let obj = construct_duration_object(&dur, &from_proto, &from_mm);
                return Ok(Value::object(obj));
            }
            if let Some(obj) = item.as_object() {
                let tt = obj.get(&PropertyKey::string(SLOT_TEMPORAL_TYPE))
                    .and_then(|v| v.as_string().map(|s| s.as_str().to_string()));
                if tt.as_deref() == Some("Duration") {
                    let dur = extract_duration_from_slots(&obj)?;
                    let result = construct_duration_object(&dur, &from_proto, &from_mm);
                    return Ok(Value::object(result));
                }
                // Generic property bag
                let field_names_alpha = ["days","hours","microseconds","milliseconds","minutes","months","nanoseconds","seconds","weeks","years"];
                let mut has_any = false;
                let mut vals = [0f64; 10];
                for &f in &field_names_alpha {
                    let v = ncx.get_property(&obj, &PropertyKey::string(f))?;
                    if !v.is_undefined() {
                        has_any = true;
                        let n = to_integer_if_integral(ncx, &v).map_err(|_| {
                            VmError::range_error(format!("{} must be a finite integer", f))
                        })?;
                        let idx = DURATION_FIELDS.iter().position(|&x| x == f).unwrap();
                        vals[idx] = n;
                    }
                }
                if !has_any {
                    return Err(VmError::type_error("duration object must have at least one temporal property"));
                }
                let dur = temporal_rs::Duration::new(
                    vals[0] as i64, vals[1] as i64, vals[2] as i64, vals[3] as i64,
                    vals[4] as i64, vals[5] as i64, vals[6] as i64, vals[7] as i64,
                    vals[8] as i128, vals[9] as i128,
                ).map_err(temporal_err)?;
                let result = construct_duration_object(&dur, &from_proto, &from_mm);
                return Ok(Value::object(result));
            }
            Err(VmError::type_error("invalid argument for Duration.from"))
        },
        mm.clone(), fn_proto.clone(), "from", 1,
    );
    ctor_obj.define_property(
        PropertyKey::string("from"),
        PropertyDescriptor::data_with_attrs(from_fn, PropertyAttributes::builtin_method()),
    );

    // ========================================================================
    // Duration.compare(d1, d2)
    // ========================================================================
    let compare_fn = Value::native_function_with_proto_named(
        |_this, args, _ncx| {
            let d1_obj = args.first().and_then(|v| v.as_object())
                .ok_or_else(|| VmError::type_error("compare: first argument must be a Duration"))?;
            let d2_obj = args.get(1).and_then(|v| v.as_object())
                .ok_or_else(|| VmError::type_error("compare: second argument must be a Duration"))?;
            let d1 = extract_duration_from_slots(&d1_obj)?;
            let d2 = extract_duration_from_slots(&d2_obj)?;
            let ord = d1.compare_with_provider(&d2, None, &NeverProvider::default())
                .map_err(temporal_err)?;
            match ord {
                std::cmp::Ordering::Less => Ok(Value::int32(-1)),
                std::cmp::Ordering::Equal => Ok(Value::int32(0)),
                std::cmp::Ordering::Greater => Ok(Value::int32(1)),
            }
        },
        mm.clone(), fn_proto.clone(), "compare", 2,
    );
    ctor_obj.define_property(
        PropertyKey::string("compare"),
        PropertyDescriptor::data_with_attrs(compare_fn, PropertyAttributes::builtin_method()),
    );

    // ========================================================================
    // Prototype property getters
    // ========================================================================

    // Individual field getters: years, months, weeks, days, hours, minutes,
    // seconds, milliseconds, microseconds, nanoseconds
    // Installed as accessor properties (like PlainDate year/month/day)
    for (slot, field) in DURATION_SLOTS.iter().zip(DURATION_FIELDS.iter()) {
        let slot_name: &'static str = slot;
        let field_name: &'static str = field;
        let getter_fn = Value::native_function_with_proto(
            move |this, _args, _ncx| {
                let obj = this.as_object()
                    .ok_or_else(|| VmError::type_error(format!("{} called on non-Duration", field_name)))?;
                obj.get(&PropertyKey::string(slot_name))
                    .filter(|v| !v.is_undefined())
                    .ok_or_else(|| VmError::type_error(format!("{} called on non-Duration", field_name)))
            },
            mm.clone(), fn_proto.clone(),
        );
        proto.define_property(
            PropertyKey::string(field),
            PropertyDescriptor::Accessor {
                get: Some(getter_fn),
                set: None,
                attributes: PropertyAttributes {
                    writable: false,
                    enumerable: false,
                    configurable: true,
                },
            },
        );
    }

    // .sign getter (accessor)
    let sign_fn = Value::native_function_with_proto(
        |this, _args, _ncx| {
            let obj = this.as_object()
                .ok_or_else(|| VmError::type_error("sign called on non-Duration"))?;
            let dur = extract_duration_from_slots(&obj)?;
            Ok(Value::int32(dur.sign() as i32))
        },
        mm.clone(), fn_proto.clone(),
    );
    proto.define_property(
        PropertyKey::string("sign"),
        PropertyDescriptor::Accessor {
            get: Some(sign_fn),
            set: None,
            attributes: PropertyAttributes { writable: false, enumerable: false, configurable: true },
        },
    );

    // .blank getter (accessor)
    let blank_fn = Value::native_function_with_proto(
        |this, _args, _ncx| {
            let obj = this.as_object()
                .ok_or_else(|| VmError::type_error("blank called on non-Duration"))?;
            let dur = extract_duration_from_slots(&obj)?;
            Ok(Value::boolean(dur.is_zero()))
        },
        mm.clone(), fn_proto.clone(),
    );
    proto.define_property(
        PropertyKey::string("blank"),
        PropertyDescriptor::Accessor {
            get: Some(blank_fn),
            set: None,
            attributes: PropertyAttributes { writable: false, enumerable: false, configurable: true },
        },
    );

    // ========================================================================
    // Prototype methods
    // ========================================================================

    // .negated()
    let neg_proto = proto.clone();
    let neg_mm = mm.clone();
    let negated_fn = Value::native_function_with_proto_named(
        move |this, _args, _ncx| {
            let obj = this.as_object()
                .ok_or_else(|| VmError::type_error("negated called on non-Duration"))?;
            let dur = extract_duration_from_slots(&obj)?;
            let result = dur.negated();
            Ok(Value::object(construct_duration_object(&result, &neg_proto, &neg_mm)))
        },
        mm.clone(), fn_proto.clone(), "negated", 0,
    );
    proto.define_property(PropertyKey::string("negated"), PropertyDescriptor::builtin_method(negated_fn));

    // .abs()
    let abs_proto = proto.clone();
    let abs_mm = mm.clone();
    let abs_fn = Value::native_function_with_proto_named(
        move |this, _args, _ncx| {
            let obj = this.as_object()
                .ok_or_else(|| VmError::type_error("abs called on non-Duration"))?;
            let dur = extract_duration_from_slots(&obj)?;
            let result = dur.abs();
            Ok(Value::object(construct_duration_object(&result, &abs_proto, &abs_mm)))
        },
        mm.clone(), fn_proto.clone(), "abs", 0,
    );
    proto.define_property(PropertyKey::string("abs"), PropertyDescriptor::builtin_method(abs_fn));

    // .add(other)
    let add_proto = proto.clone();
    let add_mm = mm.clone();
    let add_fn = Value::native_function_with_proto_named(
        move |this, args, _ncx| {
            let obj = this.as_object()
                .ok_or_else(|| VmError::type_error("add called on non-Duration"))?;
            let dur = extract_duration_from_slots(&obj)?;
            let other_obj = args.first().and_then(|v| v.as_object())
                .ok_or_else(|| VmError::type_error("add requires a Duration argument"))?;
            let other = extract_duration_from_slots(&other_obj)?;
            let result = dur.add(&other).map_err(temporal_err)?;
            Ok(Value::object(construct_duration_object(&result, &add_proto, &add_mm)))
        },
        mm.clone(), fn_proto.clone(), "add", 1,
    );
    proto.define_property(PropertyKey::string("add"), PropertyDescriptor::builtin_method(add_fn));

    // .subtract(other)
    let sub_proto = proto.clone();
    let sub_mm = mm.clone();
    let subtract_fn = Value::native_function_with_proto_named(
        move |this, args, _ncx| {
            let obj = this.as_object()
                .ok_or_else(|| VmError::type_error("subtract called on non-Duration"))?;
            let dur = extract_duration_from_slots(&obj)?;
            let other_obj = args.first().and_then(|v| v.as_object())
                .ok_or_else(|| VmError::type_error("subtract requires a Duration argument"))?;
            let other = extract_duration_from_slots(&other_obj)?;
            let result = dur.subtract(&other).map_err(temporal_err)?;
            Ok(Value::object(construct_duration_object(&result, &sub_proto, &sub_mm)))
        },
        mm.clone(), fn_proto.clone(), "subtract", 1,
    );
    proto.define_property(PropertyKey::string("subtract"), PropertyDescriptor::builtin_method(subtract_fn));

    // .toString(options?)
    let tostring_fn = Value::native_function_with_proto_named(
        |this, args, ncx| {
            let obj = this.as_object()
                .ok_or_else(|| VmError::type_error("toString called on non-Duration"))?;
            let dur = extract_duration_from_slots(&obj)?;

            let options = parse_duration_to_string_options(ncx, args.first())?;
            let s = dur.as_temporal_string(options).map_err(temporal_err)?;
            Ok(Value::string(JsString::intern(&s)))
        },
        mm.clone(), fn_proto.clone(), "toString", 0,
    );
    proto.define_property(PropertyKey::string("toString"), PropertyDescriptor::builtin_method(tostring_fn));

    // .toJSON()
    let tojson_fn = Value::native_function_with_proto_named(
        |this, _args, _ncx| {
            let obj = this.as_object()
                .ok_or_else(|| VmError::type_error("toJSON called on non-Duration"))?;
            let dur = extract_duration_from_slots(&obj)?;
            let s = dur.as_temporal_string(ToStringRoundingOptions::default()).map_err(temporal_err)?;
            Ok(Value::string(JsString::intern(&s)))
        },
        mm.clone(), fn_proto.clone(), "toJSON", 0,
    );
    proto.define_property(PropertyKey::string("toJSON"), PropertyDescriptor::builtin_method(tojson_fn));

    // .toLocaleString() — falls back to toString() per spec
    let tolocale_fn = Value::native_function_with_proto_named(
        |this, _args, _ncx| {
            let obj = this.as_object()
                .ok_or_else(|| VmError::type_error("toLocaleString called on non-Duration"))?;
            let dur = extract_duration_from_slots(&obj)?;
            let s = dur.as_temporal_string(ToStringRoundingOptions::default()).map_err(temporal_err)?;
            Ok(Value::string(JsString::intern(&s)))
        },
        mm.clone(), fn_proto.clone(), "toLocaleString", 0,
    );
    proto.define_property(PropertyKey::string("toLocaleString"), PropertyDescriptor::builtin_method(tolocale_fn));

    // .valueOf() — always throws TypeError
    let valueof_fn = Value::native_function_with_proto_named(
        |_this, _args, _ncx| {
            Err(VmError::type_error("Temporal.Duration cannot be converted to a primitive value"))
        },
        mm.clone(), fn_proto.clone(), "valueOf", 0,
    );
    proto.define_property(PropertyKey::string("valueOf"), PropertyDescriptor::builtin_method(valueof_fn));

    // .total(options)
    let total_fn = Value::native_function_with_proto_named(
        |this, args, ncx| {
            let obj = this.as_object()
                .ok_or_else(|| VmError::type_error("total called on non-Duration"))?;
            let dur = extract_duration_from_slots(&obj)?;

            let unit_arg = args.first().cloned().unwrap_or(Value::undefined());
            let unit_str = if unit_arg.is_string() {
                ncx.to_string_value(&unit_arg)?
            } else if let Some(opts_obj) = unit_arg.as_object() {
                let u = ncx.get_property(&opts_obj, &PropertyKey::string("unit"))?;
                if u.is_undefined() {
                    return Err(VmError::range_error("unit is required"));
                }
                ncx.to_string_value(&u)?
            } else {
                return Err(VmError::type_error("total requires a unit string or options object"));
            };

            let unit: Unit = unit_str.as_str().parse()
                .map_err(|_| VmError::range_error(format!("{} is not a valid unit", unit_str)))?;

            let result = dur.total_with_provider(unit, None, &NeverProvider::default())
                .map_err(temporal_err)?;
            Ok(Value::number(result.as_inner()))
        },
        mm.clone(), fn_proto.clone(), "total", 1,
    );
    proto.define_property(PropertyKey::string("total"), PropertyDescriptor::builtin_method(total_fn));

    // .round(options)
    let round_proto = proto.clone();
    let round_mm = mm.clone();
    let round_fn = Value::native_function_with_proto_named(
        move |this, args, ncx| {
            let obj = this.as_object()
                .ok_or_else(|| VmError::type_error("round called on non-Duration"))?;
            let dur = extract_duration_from_slots(&obj)?;

            let options = parse_duration_rounding_options(ncx, args.first())?;
            let result = dur.round_with_provider(options, None, &NeverProvider::default())
                .map_err(temporal_err)?;
            Ok(Value::object(construct_duration_object(&result, &round_proto, &round_mm)))
        },
        mm.clone(), fn_proto.clone(), "round", 1,
    );
    proto.define_property(PropertyKey::string("round"), PropertyDescriptor::builtin_method(round_fn));

    // @@toStringTag
    proto.define_property(
        PropertyKey::Symbol(crate::intrinsics::well_known::to_string_tag_symbol()),
        PropertyDescriptor::data_with_attrs(
            Value::string(JsString::intern("Temporal.Duration")),
            PropertyAttributes { writable: false, enumerable: false, configurable: true },
        ),
    );

    // Register on namespace
    temporal_obj.define_property(
        PropertyKey::string("Duration"),
        PropertyDescriptor::data_with_attrs(ctor_value, PropertyAttributes::builtin_method()),
    );
}

// ============================================================================
// Option parsing helpers
// ============================================================================

/// Parse toString options from a JS argument into `ToStringRoundingOptions`.
fn parse_duration_to_string_options(
    ncx: &mut NativeContext<'_>,
    arg: Option<&Value>,
) -> Result<ToStringRoundingOptions, VmError> {
    let arg = match arg {
        Some(v) if !v.is_undefined() => v.clone(),
        _ => return Ok(ToStringRoundingOptions::default()),
    };

    let opts_obj = arg.as_object()
        .ok_or_else(|| VmError::type_error("options must be an object"))?;

    let mut options = ToStringRoundingOptions::default();

    // fractionalSecondDigits
    let fsd = ncx.get_property(&opts_obj, &PropertyKey::string("fractionalSecondDigits"))?;
    if !fsd.is_undefined() {
        if fsd.is_string() {
            let s = ncx.to_string_value(&fsd)?;
            if s.as_str() == "auto" {
                options.precision = Precision::Auto;
            } else {
                return Err(VmError::range_error(format!("Invalid fractionalSecondDigits: {}", s)));
            }
        } else {
            let n = ncx.to_number_value(&fsd)?;
            let digits = n as u8;
            if !(0..=9).contains(&digits) || n != digits as f64 {
                return Err(VmError::range_error("fractionalSecondDigits must be 'auto' or 0-9"));
            }
            options.precision = Precision::Digit(digits);
        }
    }

    // smallestUnit
    let su = ncx.get_property(&opts_obj, &PropertyKey::string("smallestUnit"))?;
    if !su.is_undefined() {
        let s = ncx.to_string_value(&su)?;
        let unit: Unit = s.as_str().parse()
            .map_err(|_| VmError::range_error(format!("{} is not a valid unit", s)))?;
        options.smallest_unit = Some(unit);
    }

    // roundingMode
    let rm = ncx.get_property(&opts_obj, &PropertyKey::string("roundingMode"))?;
    if !rm.is_undefined() {
        let s = ncx.to_string_value(&rm)?;
        let mode: RoundingMode = s.as_str().parse().map_err(temporal_err)?;
        options.rounding_mode = Some(mode);
    }

    Ok(options)
}

/// Parse rounding options from a JS argument into `RoundingOptions`.
fn parse_duration_rounding_options(
    ncx: &mut NativeContext<'_>,
    arg: Option<&Value>,
) -> Result<temporal_rs::options::RoundingOptions, VmError> {
    let arg = match arg {
        Some(v) if !v.is_undefined() => v.clone(),
        _ => return Err(VmError::type_error("round requires options")),
    };

    // If it's a string, treat as shorthand for smallestUnit
    if arg.is_string() {
        let s = ncx.to_string_value(&arg)?;
        let unit: Unit = s.as_str().parse()
            .map_err(|_| VmError::range_error(format!("{} is not a valid unit", s)))?;
        let mut opts = temporal_rs::options::RoundingOptions::default();
        opts.smallest_unit = Some(unit);
        return Ok(opts);
    }

    let opts_obj = arg.as_object()
        .ok_or_else(|| VmError::type_error("options must be an object or string"))?;

    let mut options = temporal_rs::options::RoundingOptions::default();

    // largestUnit
    let lu = ncx.get_property(&opts_obj, &PropertyKey::string("largestUnit"))?;
    if !lu.is_undefined() {
        let s = ncx.to_string_value(&lu)?;
        let unit: Unit = s.as_str().parse()
            .map_err(|_| VmError::range_error(format!("{} is not a valid unit", s)))?;
        options.largest_unit = Some(unit);
    }

    // smallestUnit
    let su = ncx.get_property(&opts_obj, &PropertyKey::string("smallestUnit"))?;
    if !su.is_undefined() {
        let s = ncx.to_string_value(&su)?;
        let unit: Unit = s.as_str().parse()
            .map_err(|_| VmError::range_error(format!("{} is not a valid unit", s)))?;
        options.smallest_unit = Some(unit);
    }

    // roundingMode
    let rm = ncx.get_property(&opts_obj, &PropertyKey::string("roundingMode"))?;
    if !rm.is_undefined() {
        let s = ncx.to_string_value(&rm)?;
        let mode: RoundingMode = s.as_str().parse().map_err(temporal_err)?;
        options.rounding_mode = Some(mode);
    }

    // roundingIncrement
    let ri = ncx.get_property(&opts_obj, &PropertyKey::string("roundingIncrement"))?;
    if !ri.is_undefined() {
        let n = ncx.to_number_value(&ri)?;
        let inc = temporal_rs::options::RoundingIncrement::try_from(n).map_err(temporal_err)?;
        options.increment = Some(inc);
    }

    Ok(options)
}
