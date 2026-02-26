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
            PropertyAttributes {
                writable: false,
                enumerable: false,
                configurable: false,
            },
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
    let ctor_proto_check = proto.clone();
    let ctor_mm = mm.clone();
    let ctor_fn: Box<
        dyn Fn(&Value, &[Value], &mut NativeContext<'_>) -> Result<Value, VmError> + Send + Sync,
    > = Box::new(move |this, args, ncx| {
        // Step 1: If NewTarget is undefined, throw TypeError
        let is_new_target = if let Some(obj) = this.as_object() {
            obj.prototype()
                .as_object()
                .map_or(false, |p| p.as_ptr() == ctor_proto_check.as_ptr())
        } else {
            false
        };
        if !is_new_target {
            return Err(VmError::type_error(
                "Temporal.Duration constructor requires 'new'",
            ));
        }
        // Parse each argument with ToIntegerIfIntegral (defaulting to 0)
        let mut vals = [0f64; 10];
        for (i, field) in DURATION_FIELDS.iter().enumerate() {
            if let Some(val) = args.get(i) {
                if !val.is_undefined() {
                    let n = to_integer_if_integral(ncx, val)?;
                    vals[i] = n;
                }
            }
        }

        // Validate with temporal_rs::Duration::new()
        let dur = temporal_rs::Duration::new(
            vals[0] as i64,
            vals[1] as i64,
            vals[2] as i64,
            vals[3] as i64,
            vals[4] as i64,
            vals[5] as i64,
            vals[6] as i64,
            vals[7] as i64,
            vals[8] as i128,
            vals[9] as i128,
        )
        .map_err(temporal_err)?;

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
        PropertyDescriptor::data_with_attrs(
            ctor_value.clone(),
            PropertyAttributes::constructor_link(),
        ),
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
            // Handle objects AND proxies
            if item.as_object().is_some() || item.as_proxy().is_some() {
                // Check if it's a real Duration object via TemporalValue
                if let Some(obj) = item.as_object() {
                    let tt = obj
                        .get(&PropertyKey::string(SLOT_TEMPORAL_TYPE))
                        .and_then(|v| v.as_string().map(|s| s.as_str().to_string()));
                    if tt.as_deref() == Some("Duration") {
                        let dur = extract_duration(&obj)?;
                        let result = construct_duration_object(&dur, &from_proto, &from_mm);
                        return Ok(Value::object(result));
                    }
                }
                // Generic property bag (supports Proxy via get_property_of_value)
                let field_names_alpha = [
                    "days",
                    "hours",
                    "microseconds",
                    "milliseconds",
                    "minutes",
                    "months",
                    "nanoseconds",
                    "seconds",
                    "weeks",
                    "years",
                ];
                let mut has_any = false;
                let mut vals = [0f64; 10];
                for &f in &field_names_alpha {
                    let v = ncx.get_property_of_value(&item, &PropertyKey::string(f))?;
                    if !v.is_undefined() {
                        has_any = true;
                        let n = to_integer_if_integral(ncx, &v)?;
                        let idx = DURATION_FIELDS.iter().position(|&x| x == f).unwrap();
                        vals[idx] = n;
                    }
                }
                if !has_any {
                    return Err(VmError::type_error(
                        "duration object must have at least one temporal property",
                    ));
                }
                let dur = temporal_rs::Duration::new(
                    vals[0] as i64,
                    vals[1] as i64,
                    vals[2] as i64,
                    vals[3] as i64,
                    vals[4] as i64,
                    vals[5] as i64,
                    vals[6] as i64,
                    vals[7] as i64,
                    vals[8] as i128,
                    vals[9] as i128,
                )
                .map_err(temporal_err)?;
                let result = construct_duration_object(&dur, &from_proto, &from_mm);
                return Ok(Value::object(result));
            }
            Err(VmError::type_error("invalid argument for Duration.from"))
        },
        mm.clone(),
        fn_proto.clone(),
        "from",
        1,
    );
    ctor_obj.define_property(
        PropertyKey::string("from"),
        PropertyDescriptor::data_with_attrs(from_fn, PropertyAttributes::builtin_method()),
    );

    // ========================================================================
    // Duration.compare(d1, d2)
    // ========================================================================
    let compare_fn = Value::native_function_with_proto_named(
        |_this, args, ncx| {
            let d1_arg = args.first().cloned().unwrap_or(Value::undefined());
            let d2_arg = args.get(1).cloned().unwrap_or(Value::undefined());
            let d1 = to_temporal_duration(ncx, &d1_arg)?;
            let d2 = to_temporal_duration(ncx, &d2_arg)?;
            // Parse relativeTo from options (3rd argument)
            let options_val = args.get(2).cloned().unwrap_or(Value::undefined());
            get_options_object(&options_val)?;
            let relative_to = if !options_val.is_undefined() {
                parse_relative_to(ncx, &options_val)?
            } else {
                None
            };
            let ord = d1
                .compare_with_provider(&d2, relative_to, tz_provider())
                .map_err(temporal_err)?;
            match ord {
                std::cmp::Ordering::Less => Ok(Value::int32(-1)),
                std::cmp::Ordering::Equal => Ok(Value::int32(0)),
                std::cmp::Ordering::Greater => Ok(Value::int32(1)),
            }
        },
        mm.clone(),
        fn_proto.clone(),
        "compare",
        2,
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
    for field in DURATION_FIELDS.iter() {
        let field_name: &'static str = field;
        let getter_fn = Value::native_function_with_proto(
            move |this, _args, _ncx| {
                let obj = this.as_object().ok_or_else(|| {
                    VmError::type_error(format!("{} called on non-Duration", field_name))
                })?;
                let dur = extract_duration(&obj)?;
                let val = match field_name {
                    "years" => dur.years() as f64,
                    "months" => dur.months() as f64,
                    "weeks" => dur.weeks() as f64,
                    "days" => dur.days() as f64,
                    "hours" => dur.hours() as f64,
                    "minutes" => dur.minutes() as f64,
                    "seconds" => dur.seconds() as f64,
                    "milliseconds" => dur.milliseconds() as f64,
                    "microseconds" => dur.microseconds() as f64,
                    "nanoseconds" => dur.nanoseconds() as f64,
                    _ => unreachable!(),
                };
                Ok(Value::number(val))
            },
            mm.clone(),
            fn_proto.clone(),
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
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("sign called on non-Duration"))?;
            let dur = extract_duration(&obj)?;
            Ok(Value::int32(dur.sign() as i32))
        },
        mm.clone(),
        fn_proto.clone(),
    );
    proto.define_property(
        PropertyKey::string("sign"),
        PropertyDescriptor::Accessor {
            get: Some(sign_fn),
            set: None,
            attributes: PropertyAttributes {
                writable: false,
                enumerable: false,
                configurable: true,
            },
        },
    );

    // .blank getter (accessor)
    let blank_fn = Value::native_function_with_proto(
        |this, _args, _ncx| {
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("blank called on non-Duration"))?;
            let dur = extract_duration(&obj)?;
            Ok(Value::boolean(dur.is_zero()))
        },
        mm.clone(),
        fn_proto.clone(),
    );
    proto.define_property(
        PropertyKey::string("blank"),
        PropertyDescriptor::Accessor {
            get: Some(blank_fn),
            set: None,
            attributes: PropertyAttributes {
                writable: false,
                enumerable: false,
                configurable: true,
            },
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
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("negated called on non-Duration"))?;
            let dur = extract_duration(&obj)?;
            let result = dur.negated();
            Ok(Value::object(construct_duration_object(
                &result, &neg_proto, &neg_mm,
            )))
        },
        mm.clone(),
        fn_proto.clone(),
        "negated",
        0,
    );
    proto.define_property(
        PropertyKey::string("negated"),
        PropertyDescriptor::builtin_method(negated_fn),
    );

    // .abs()
    let abs_proto = proto.clone();
    let abs_mm = mm.clone();
    let abs_fn = Value::native_function_with_proto_named(
        move |this, _args, _ncx| {
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("abs called on non-Duration"))?;
            let dur = extract_duration(&obj)?;
            let result = dur.abs();
            Ok(Value::object(construct_duration_object(
                &result, &abs_proto, &abs_mm,
            )))
        },
        mm.clone(),
        fn_proto.clone(),
        "abs",
        0,
    );
    proto.define_property(
        PropertyKey::string("abs"),
        PropertyDescriptor::builtin_method(abs_fn),
    );

    // .with(durationLike)
    let with_fn = Value::native_function_with_proto_named(
        |this, args, ncx| {
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("with called on non-Duration"))?;
            let dur = extract_duration(&obj)?;
            let item = args.first().cloned().unwrap_or(Value::undefined());
            // Accept both objects and proxies
            if item.as_object().is_none() && item.as_proxy().is_none() {
                return Err(VmError::type_error("with argument must be an object"));
            }

            // Read optional fields from the property bag in alphabetical order (spec)
            // Uses get_property_of_value for Proxy support
            let field_names_alpha = [
                "days",
                "hours",
                "microseconds",
                "milliseconds",
                "minutes",
                "months",
                "nanoseconds",
                "seconds",
                "weeks",
                "years",
            ];
            let mut has_any = false;
            let mut field_vals: std::collections::HashMap<&str, f64> =
                std::collections::HashMap::new();
            for &name in &field_names_alpha {
                let v = ncx.get_property_of_value(&item, &PropertyKey::string(name))?;
                if !v.is_undefined() {
                    has_any = true;
                    let n = ncx.to_number_value(&v)?;
                    if n.is_nan() || n.is_infinite() {
                        return Err(VmError::range_error(format!(
                            "{} must be a finite number",
                            name
                        )));
                    }
                    if n != n.trunc() {
                        return Err(VmError::range_error(format!("{} must be an integer", name)));
                    }
                    field_vals.insert(name, n);
                }
            }

            let years = field_vals.get("years").copied();
            let months = field_vals.get("months").copied();
            let weeks = field_vals.get("weeks").copied();
            let days = field_vals.get("days").copied();
            let hours = field_vals.get("hours").copied();
            let minutes = field_vals.get("minutes").copied();
            let seconds = field_vals.get("seconds").copied();
            let milliseconds = field_vals.get("milliseconds").copied();
            let microseconds = field_vals.get("microseconds").copied();
            let nanoseconds = field_vals.get("nanoseconds").copied();

            if !has_any {
                return Err(VmError::type_error(
                    "with argument must have at least one duration property",
                ));
            }

            // Create new Duration, replacing provided fields, keeping existing ones
            let result = temporal_rs::Duration::new(
                years.map(|n| n as i64).unwrap_or_else(|| dur.years()),
                months.map(|n| n as i64).unwrap_or_else(|| dur.months()),
                weeks.map(|n| n as i64).unwrap_or_else(|| dur.weeks()),
                days.map(|n| n as i64).unwrap_or_else(|| dur.days()),
                hours.map(|n| n as i64).unwrap_or_else(|| dur.hours()),
                minutes.map(|n| n as i64).unwrap_or_else(|| dur.minutes()),
                seconds.map(|n| n as i64).unwrap_or_else(|| dur.seconds()),
                milliseconds
                    .map(|n| n as i64)
                    .unwrap_or_else(|| dur.milliseconds()),
                microseconds
                    .map(|n| n as i128)
                    .unwrap_or_else(|| dur.microseconds()),
                nanoseconds
                    .map(|n| n as i128)
                    .unwrap_or_else(|| dur.nanoseconds()),
            )
            .map_err(temporal_err)?;
            construct_duration_value(ncx, &result)
        },
        mm.clone(),
        fn_proto.clone(),
        "with",
        1,
    );
    proto.define_property(
        PropertyKey::string("with"),
        PropertyDescriptor::builtin_method(with_fn),
    );

    // .add(other)
    let add_fn = Value::native_function_with_proto_named(
        |this, args, ncx| {
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("add called on non-Duration"))?;
            let dur = extract_duration(&obj)?;
            let other_arg = args.first().cloned().unwrap_or(Value::undefined());
            let other = to_temporal_duration(ncx, &other_arg)?;
            let result = dur.add(&other).map_err(temporal_err)?;
            construct_duration_value(ncx, &result)
        },
        mm.clone(),
        fn_proto.clone(),
        "add",
        1,
    );
    proto.define_property(
        PropertyKey::string("add"),
        PropertyDescriptor::builtin_method(add_fn),
    );

    // .subtract(other)
    let subtract_fn = Value::native_function_with_proto_named(
        |this, args, ncx| {
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("subtract called on non-Duration"))?;
            let dur = extract_duration(&obj)?;
            let other_arg = args.first().cloned().unwrap_or(Value::undefined());
            let other = to_temporal_duration(ncx, &other_arg)?;
            let result = dur.subtract(&other).map_err(temporal_err)?;
            construct_duration_value(ncx, &result)
        },
        mm.clone(),
        fn_proto.clone(),
        "subtract",
        1,
    );
    proto.define_property(
        PropertyKey::string("subtract"),
        PropertyDescriptor::builtin_method(subtract_fn),
    );

    // .toString(options?)
    let tostring_fn = Value::native_function_with_proto_named(
        |this, args, ncx| {
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("toString called on non-Duration"))?;
            let dur = extract_duration(&obj)?;

            let options = parse_duration_to_string_options(ncx, args.first())?;
            let s = dur.as_temporal_string(options).map_err(temporal_err)?;
            Ok(Value::string(JsString::intern(&s)))
        },
        mm.clone(),
        fn_proto.clone(),
        "toString",
        0,
    );
    proto.define_property(
        PropertyKey::string("toString"),
        PropertyDescriptor::builtin_method(tostring_fn),
    );

    // .toJSON()
    let tojson_fn = Value::native_function_with_proto_named(
        |this, _args, _ncx| {
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("toJSON called on non-Duration"))?;
            let dur = extract_duration(&obj)?;
            let s = dur
                .as_temporal_string(ToStringRoundingOptions::default())
                .map_err(temporal_err)?;
            Ok(Value::string(JsString::intern(&s)))
        },
        mm.clone(),
        fn_proto.clone(),
        "toJSON",
        0,
    );
    proto.define_property(
        PropertyKey::string("toJSON"),
        PropertyDescriptor::builtin_method(tojson_fn),
    );

    // .toLocaleString() — falls back to toString() per spec
    let tolocale_fn = Value::native_function_with_proto_named(
        |this, _args, _ncx| {
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("toLocaleString called on non-Duration"))?;
            let dur = extract_duration(&obj)?;
            let s = dur
                .as_temporal_string(ToStringRoundingOptions::default())
                .map_err(temporal_err)?;
            Ok(Value::string(JsString::intern(&s)))
        },
        mm.clone(),
        fn_proto.clone(),
        "toLocaleString",
        0,
    );
    proto.define_property(
        PropertyKey::string("toLocaleString"),
        PropertyDescriptor::builtin_method(tolocale_fn),
    );

    // .valueOf() — always throws TypeError
    let valueof_fn = Value::native_function_with_proto_named(
        |_this, _args, _ncx| {
            Err(VmError::type_error(
                "Temporal.Duration cannot be converted to a primitive value",
            ))
        },
        mm.clone(),
        fn_proto.clone(),
        "valueOf",
        0,
    );
    proto.define_property(
        PropertyKey::string("valueOf"),
        PropertyDescriptor::builtin_method(valueof_fn),
    );

    // .total(options)
    let total_fn = Value::native_function_with_proto_named(
        |this, args, ncx| {
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("total called on non-Duration"))?;
            let dur = extract_duration(&obj)?;

            let unit_arg = args.first().cloned().unwrap_or(Value::undefined());
            let (unit_str, relative_to) = if unit_arg.is_string() {
                (ncx.to_string_value(&unit_arg)?, None)
            } else if unit_arg.is_undefined()
                || unit_arg.is_null()
                || unit_arg.is_boolean()
                || unit_arg.is_number()
                || unit_arg.is_bigint()
                || unit_arg.as_symbol().is_some()
            {
                return Err(VmError::type_error(
                    "total requires a unit string or options object",
                ));
            } else {
                // Object or Proxy — read in spec order: relativeTo (+ all its fields), THEN unit
                let rt_val =
                    ncx.get_property_of_value(&unit_arg, &PropertyKey::string("relativeTo"))?;
                let relative_to = if !rt_val.is_undefined() {
                    parse_relative_to_value(ncx, &rt_val)?
                } else {
                    None
                };
                let u = ncx.get_property_of_value(&unit_arg, &PropertyKey::string("unit"))?;
                if u.is_undefined() {
                    return Err(VmError::range_error("unit is required"));
                }
                (ncx.to_string_value(&u)?, relative_to)
            };

            let unit: Unit = unit_str
                .as_str()
                .parse()
                .map_err(|_| VmError::range_error(format!("{} is not a valid unit", unit_str)))?;

            let result = dur
                .total_with_provider(unit, relative_to, tz_provider())
                .map_err(temporal_err)?;
            Ok(Value::number(result.as_inner()))
        },
        mm.clone(),
        fn_proto.clone(),
        "total",
        1,
    );
    proto.define_property(
        PropertyKey::string("total"),
        PropertyDescriptor::builtin_method(total_fn),
    );

    // .round(options)
    let round_fn = Value::native_function_with_proto_named(
        |this, args, ncx| {
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("round called on non-Duration"))?;
            let dur = extract_duration(&obj)?;

            let arg = args.first().cloned().unwrap_or(Value::undefined());
            let (options, relative_to) =
                parse_duration_rounding_options_with_relative(ncx, args.first())?;
            let result = dur
                .round_with_provider(options, relative_to, tz_provider())
                .map_err(temporal_err)?;
            construct_duration_value(ncx, &result)
        },
        mm.clone(),
        fn_proto.clone(),
        "round",
        1,
    );
    proto.define_property(
        PropertyKey::string("round"),
        PropertyDescriptor::builtin_method(round_fn),
    );

    // @@toStringTag
    proto.define_property(
        PropertyKey::Symbol(crate::intrinsics::well_known::to_string_tag_symbol()),
        PropertyDescriptor::data_with_attrs(
            Value::string(JsString::intern("Temporal.Duration")),
            PropertyAttributes {
                writable: false,
                enumerable: false,
                configurable: true,
            },
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

    // Reject primitives
    if arg.is_null()
        || arg.is_boolean()
        || arg.is_number()
        || arg.is_string()
        || arg.is_bigint()
        || arg.as_symbol().is_some()
    {
        return Err(VmError::type_error("options must be an object"));
    }

    let mut options = ToStringRoundingOptions::default();

    // Per spec: read ALL options in alphabetical order via get_property_of_value (handles Proxy)
    // 1. fractionalSecondDigits
    let fsd = ncx.get_property_of_value(&arg, &PropertyKey::string("fractionalSecondDigits"))?;
    if !fsd.is_undefined() {
        if fsd.is_number() {
            let n = fsd.as_number().unwrap();
            if n.is_nan() || n.is_infinite() {
                return Err(VmError::range_error(
                    "fractionalSecondDigits must be 'auto' or 0-9",
                ));
            }
            let floored = n.floor();
            if floored < 0.0 || floored > 9.0 {
                return Err(VmError::range_error(
                    "fractionalSecondDigits must be 'auto' or 0-9",
                ));
            }
            options.precision = Precision::Digit(floored as u8);
        } else {
            let s = ncx.to_string_value(&fsd)?;
            if s.as_str() == "auto" {
                options.precision = Precision::Auto;
            } else {
                return Err(VmError::range_error(format!(
                    "Invalid fractionalSecondDigits: {}",
                    s
                )));
            }
        }
    }

    // 2. roundingMode (alphabetically before smallestUnit)
    let rm = ncx.get_property_of_value(&arg, &PropertyKey::string("roundingMode"))?;
    if !rm.is_undefined() {
        let s = ncx.to_string_value(&rm)?;
        let mode: RoundingMode = s.as_str().parse().map_err(temporal_err)?;
        options.rounding_mode = Some(mode);
    }

    // 3. smallestUnit
    let su = ncx.get_property_of_value(&arg, &PropertyKey::string("smallestUnit"))?;
    if !su.is_undefined() {
        let s = ncx.to_string_value(&su)?;
        let unit: Unit = s
            .as_str()
            .parse()
            .map_err(|_| VmError::range_error(format!("{} is not a valid unit", s)))?;
        options.smallest_unit = Some(unit);
    }

    Ok(options)
}

/// Parse rounding options from a JS argument into `RoundingOptions`.
fn parse_duration_rounding_options_with_relative(
    ncx: &mut NativeContext<'_>,
    arg: Option<&Value>,
) -> Result<
    (
        temporal_rs::options::RoundingOptions,
        Option<temporal_rs::options::RelativeTo>,
    ),
    VmError,
> {
    let arg = match arg {
        Some(v) if !v.is_undefined() => v.clone(),
        _ => return Err(VmError::type_error("round requires options")),
    };

    // If it's a string, treat as shorthand for smallestUnit
    if arg.is_string() {
        let s = ncx.to_string_value(&arg)?;
        let unit: Unit = s
            .as_str()
            .parse()
            .map_err(|_| VmError::range_error(format!("{} is not a valid unit", s)))?;
        let mut opts = temporal_rs::options::RoundingOptions::default();
        opts.smallest_unit = Some(unit);
        return Ok((opts, None));
    }

    // Reject primitives
    if arg.is_null()
        || arg.is_boolean()
        || arg.is_number()
        || arg.is_bigint()
        || arg.as_symbol().is_some()
    {
        return Err(VmError::type_error("options must be an object or string"));
    }

    let mut options = temporal_rs::options::RoundingOptions::default();

    // Per spec: read ALL options in alphabetical order via get_property_of_value (handles Proxy)
    // 1. largestUnit
    let lu = ncx.get_property_of_value(&arg, &PropertyKey::string("largestUnit"))?;
    if !lu.is_undefined() {
        let s = ncx.to_string_value(&lu)?;
        let unit: Unit = s
            .as_str()
            .parse()
            .map_err(|_| VmError::range_error(format!("{} is not a valid unit", s)))?;
        options.largest_unit = Some(unit);
    }

    // 2. relativeTo — read once, then parse (no double-read)
    let rt_val = ncx.get_property_of_value(&arg, &PropertyKey::string("relativeTo"))?;
    let relative_to = if !rt_val.is_undefined() {
        parse_relative_to_value(ncx, &rt_val)?
    } else {
        None
    };

    // 3. roundingIncrement
    let ri = ncx.get_property_of_value(&arg, &PropertyKey::string("roundingIncrement"))?;
    if !ri.is_undefined() {
        let n = ncx.to_number_value(&ri)?;
        let inc = temporal_rs::options::RoundingIncrement::try_from(n).map_err(temporal_err)?;
        options.increment = Some(inc);
    }

    // 4. roundingMode
    let rm = ncx.get_property_of_value(&arg, &PropertyKey::string("roundingMode"))?;
    if !rm.is_undefined() {
        let s = ncx.to_string_value(&rm)?;
        let mode: RoundingMode = s.as_str().parse().map_err(temporal_err)?;
        options.rounding_mode = Some(mode);
    }

    // 5. smallestUnit
    let su = ncx.get_property_of_value(&arg, &PropertyKey::string("smallestUnit"))?;
    if !su.is_undefined() {
        let s = ncx.to_string_value(&su)?;
        let unit: Unit = s
            .as_str()
            .parse()
            .map_err(|_| VmError::range_error(format!("{} is not a valid unit", s)))?;
        options.smallest_unit = Some(unit);
    }

    // Per spec: If smallestUnit and largestUnit are both absent, throw RangeError (after reading all options)
    if lu.is_undefined() && su.is_undefined() {
        return Err(VmError::range_error(
            "at least one of smallestUnit or largestUnit is required",
        ));
    }

    Ok((options, relative_to))
}
