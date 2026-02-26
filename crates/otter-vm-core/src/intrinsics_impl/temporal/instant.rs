use crate::context::NativeContext;
use crate::error::VmError;
use crate::gc::GcRef;
use crate::memory::MemoryManager;
use crate::object::{JsObject, PropertyAttributes, PropertyDescriptor, PropertyKey};
use crate::string::JsString;
use crate::temporal_value::TemporalValue;
use crate::value::{HeapRef, Value};
use std::sync::Arc;

use super::common::*;

/// Get the compiled timezone provider from temporal_rs.
fn tz_provider() -> &'static impl temporal_rs::provider::TimeZoneProvider {
    &*temporal_rs::provider::COMPILED_TZ_PROVIDER
}

/// Convert an argument to a temporal_rs::Instant (string or Instant object).
fn to_temporal_instant(
    ncx: &mut NativeContext<'_>,
    val: &Value,
) -> Result<temporal_rs::Instant, VmError> {
    if let Some(obj) = val.as_object() {
        // Try extracting as Instant first
        if let Ok(instant) = extract_instant(&obj) {
            return Ok(instant);
        }
        // Try extracting as ZonedDateTime
        if let Ok(zdt) = extract_zoned_date_time(&obj) {
            let ns = zdt.epoch_nanoseconds().0;
            return temporal_rs::Instant::try_new(ns).map_err(temporal_err);
        }
        // Generic object: call toString, then parse as ISO string
        let s = ncx.to_string_value(val)?;
        return temporal_rs::Instant::from_utf8(s.as_str().as_bytes()).map_err(temporal_err);
    }
    if val.is_string() {
        let s = ncx.to_string_value(val)?;
        return temporal_rs::Instant::from_utf8(s.as_str().as_bytes()).map_err(temporal_err);
    }
    // Try toString for proxies
    if val.as_proxy().is_some() {
        let s = ncx.to_string_value(val)?;
        return temporal_rs::Instant::from_utf8(s.as_str().as_bytes()).map_err(temporal_err);
    }
    Err(VmError::type_error("Cannot convert to Instant"))
}

/// Construct an Instant JS value from a temporal_rs::Instant.
fn construct_instant_value(
    ncx: &mut NativeContext<'_>,
    instant: &temporal_rs::Instant,
) -> Result<Value, VmError> {
    let temporal_ns = ncx
        .ctx
        .get_global("Temporal")
        .ok_or_else(|| VmError::type_error("Temporal not found"))?;
    let temporal_obj = temporal_ns
        .as_object()
        .ok_or_else(|| VmError::type_error("Temporal not found"))?;
    let ctor = temporal_obj
        .get(&PropertyKey::string("Instant"))
        .ok_or_else(|| VmError::type_error("Instant not found"))?;
    let ns = instant.epoch_nanoseconds().0;
    ncx.call_function_construct(&ctor, Value::undefined(), &[Value::bigint(ns.to_string())])
}

/// Install Instant constructor and prototype onto `temporal_obj`.
pub(super) fn install_instant(
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
        PropertyDescriptor::function_length(Value::string(JsString::intern("Instant"))),
    );
    ctor_obj.define_property(
        PropertyKey::string("length"),
        PropertyDescriptor::function_length(Value::number(1.0)),
    );

    // Constructor: new Temporal.Instant(epochNanoseconds)
    let proto_for_ctor = proto.clone();
    let ctor_fn: Box<
        dyn Fn(&Value, &[Value], &mut NativeContext<'_>) -> Result<Value, VmError> + Send + Sync,
    > = Box::new(move |this, args, _ncx| {
        // Step 1: If NewTarget is undefined, throw TypeError
        let is_new_target = if let Some(obj) = this.as_object() {
            obj.prototype()
                .as_object()
                .is_some_and(|p| p.as_ptr() == proto_for_ctor.as_ptr())
        } else {
            false
        };
        if !is_new_target {
            return Err(VmError::type_error(
                "Temporal.Instant constructor requires 'new'",
            ));
        }

        let epoch_ns_val = args.first().cloned().unwrap_or(Value::undefined());

        // Per spec: ToBigInt(epochNanoseconds)
        // Accepts: BigInt, boolean (true→1n, false→0n), string (parse as BigInt)
        // Rejects: undefined→TypeError, null→TypeError, number→TypeError, symbol→TypeError
        let ns: i128 = if epoch_ns_val.is_bigint() {
            match epoch_ns_val.heap_ref() {
                Some(HeapRef::BigInt(b)) => b
                    .value
                    .parse::<i128>()
                    .map_err(|_| VmError::range_error("epoch nanoseconds out of range"))?,
                _ => {
                    return Err(VmError::type_error(
                        "Temporal.Instant requires a BigInt argument",
                    ));
                }
            }
        } else if let Some(b) = epoch_ns_val.as_boolean() {
            if b { 1i128 } else { 0i128 }
        } else if epoch_ns_val.is_string() {
            let s = epoch_ns_val.as_string().unwrap().as_str().to_string();
            let trimmed = s.trim();
            if trimmed.is_empty() {
                return Err(VmError::syntax_error(
                    "Cannot convert empty string to BigInt",
                ));
            }
            trimmed
                .parse::<i128>()
                .map_err(|_| VmError::syntax_error(format!("Cannot convert {} to a BigInt", s)))?
        } else if epoch_ns_val.is_number() {
            return Err(VmError::type_error(
                "Cannot convert a Number value to a BigInt",
            ));
        } else if epoch_ns_val.is_undefined() {
            return Err(VmError::type_error("Cannot convert undefined to a BigInt"));
        } else if epoch_ns_val.is_null() {
            return Err(VmError::type_error("Cannot convert null to a BigInt"));
        } else if epoch_ns_val.as_symbol().is_some() {
            return Err(VmError::type_error(
                "Cannot convert a Symbol value to a BigInt",
            ));
        } else {
            return Err(VmError::type_error(
                "Temporal.Instant requires a BigInt argument",
            ));
        };

        // Validate via temporal_rs (handles range check)
        let instant = temporal_rs::Instant::try_new(ns).map_err(temporal_err)?;

        if let Some(obj) = this.as_object() {
            store_temporal_inner(&obj, TemporalValue::Instant(instant));
        }

        Ok(Value::undefined())
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

    // epochMilliseconds getter
    let epoch_ms_getter = Value::native_function_with_proto_named(
        |this, _args, _ncx| {
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("not an Instant"))?;
            let instant = extract_instant(&obj)?;
            Ok(Value::number(instant.epoch_milliseconds() as f64))
        },
        mm.clone(),
        fn_proto.clone(),
        "get epochMilliseconds",
        0,
    );
    proto.define_property(
        PropertyKey::string("epochMilliseconds"),
        PropertyDescriptor::getter(epoch_ms_getter),
    );

    // epochNanoseconds getter (returns BigInt)
    let epoch_ns_getter = Value::native_function_with_proto_named(
        |this, _args, _ncx| {
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("not an Instant"))?;
            let instant = extract_instant(&obj)?;
            Ok(Value::bigint(instant.epoch_nanoseconds().0.to_string()))
        },
        mm.clone(),
        fn_proto.clone(),
        "get epochNanoseconds",
        0,
    );
    proto.define_property(
        PropertyKey::string("epochNanoseconds"),
        PropertyDescriptor::getter(epoch_ns_getter),
    );

    // equals(other)
    let equals_fn = Value::native_function_with_proto_named(
        |this, args, ncx| {
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("not an Instant"))?;
            let instant = extract_instant(&obj)?;
            let other_val = args.first().cloned().unwrap_or(Value::undefined());
            let other = to_temporal_instant(ncx, &other_val)?;
            Ok(Value::boolean(
                instant.epoch_nanoseconds().0 == other.epoch_nanoseconds().0,
            ))
        },
        mm.clone(),
        fn_proto.clone(),
        "equals",
        1,
    );
    proto.define_property(
        PropertyKey::string("equals"),
        PropertyDescriptor::builtin_method(equals_fn),
    );

    // add(duration)
    let add_fn = Value::native_function_with_proto_named(
        |this, args, ncx| {
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("not an Instant"))?;
            let instant = extract_instant(&obj)?;
            let dur_val = args.first().cloned().unwrap_or(Value::undefined());
            let dur = to_temporal_duration(ncx, &dur_val)?;
            let result = instant.add(&dur).map_err(temporal_err)?;
            construct_instant_value(ncx, &result)
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

    // subtract(duration)
    let subtract_fn = Value::native_function_with_proto_named(
        |this, args, ncx| {
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("not an Instant"))?;
            let instant = extract_instant(&obj)?;
            let dur_val = args.first().cloned().unwrap_or(Value::undefined());
            let dur = to_temporal_duration(ncx, &dur_val)?;
            let result = instant.subtract(&dur).map_err(temporal_err)?;
            construct_instant_value(ncx, &result)
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

    // until(other [, options])
    let until_fn = Value::native_function_with_proto_named(
        |this, args, ncx| {
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("not an Instant"))?;
            let instant = extract_instant(&obj)?;
            let other_val = args.first().cloned().unwrap_or(Value::undefined());
            let other = to_temporal_instant(ncx, &other_val)?;
            let options_val = args.get(1).cloned().unwrap_or(Value::undefined());
            let settings = parse_instant_difference_options(ncx, &options_val)?;
            let dur = instant.until(&other, settings).map_err(temporal_err)?;
            construct_duration_value(ncx, &dur)
        },
        mm.clone(),
        fn_proto.clone(),
        "until",
        1,
    );
    proto.define_property(
        PropertyKey::string("until"),
        PropertyDescriptor::builtin_method(until_fn),
    );

    // since(other [, options])
    let since_fn = Value::native_function_with_proto_named(
        |this, args, ncx| {
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("not an Instant"))?;
            let instant = extract_instant(&obj)?;
            let other_val = args.first().cloned().unwrap_or(Value::undefined());
            let other = to_temporal_instant(ncx, &other_val)?;
            let options_val = args.get(1).cloned().unwrap_or(Value::undefined());
            let settings = parse_instant_difference_options(ncx, &options_val)?;
            let dur = instant.since(&other, settings).map_err(temporal_err)?;
            construct_duration_value(ncx, &dur)
        },
        mm.clone(),
        fn_proto.clone(),
        "since",
        1,
    );
    proto.define_property(
        PropertyKey::string("since"),
        PropertyDescriptor::builtin_method(since_fn),
    );

    // round(options)
    let round_fn = Value::native_function_with_proto_named(
        |this, args, ncx| {
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("not an Instant"))?;
            let instant = extract_instant(&obj)?;
            let options_val = args.first().cloned().unwrap_or(Value::undefined());
            let round_opts = parse_instant_rounding_options(ncx, &options_val)?;
            let result = instant.round(round_opts).map_err(temporal_err)?;
            construct_instant_value(ncx, &result)
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

    // toZonedDateTimeISO(timeZone)
    let to_zdt_iso_fn = Value::native_function_with_proto_named(
        |this, args, ncx| {
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("not an Instant"))?;
            let instant = extract_instant(&obj)?;
            let tz_val = args.first().cloned().unwrap_or(Value::undefined());
            // Validate timezone via temporal_rs (TypeError for non-string, RangeError for invalid)
            if tz_val.is_undefined() {
                return Err(VmError::type_error(
                    "toZonedDateTimeISO requires a timeZone argument",
                ));
            }
            let tz = to_temporal_timezone_identifier(ncx, &tz_val)?;
            let zdt = instant
                .to_zoned_date_time_iso_with_provider(tz, tz_provider())
                .map_err(temporal_err)?;

            // Build ZDT JS value via common helper
            construct_zoned_date_time_value(ncx, &zdt)
        },
        mm.clone(),
        fn_proto.clone(),
        "toZonedDateTimeISO",
        1,
    );
    proto.define_property(
        PropertyKey::string("toZonedDateTimeISO"),
        PropertyDescriptor::builtin_method(to_zdt_iso_fn),
    );

    // toString([options])
    let to_string_fn = Value::native_function_with_proto_named(
        |this, args, ncx| {
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("not an Instant"))?;
            let instant = extract_instant(&obj)?;
            let options_val = args.first().cloned().unwrap_or(Value::undefined());
            let (tz, rounding_opts) = parse_instant_to_string_options(ncx, &options_val)?;
            let s = instant
                .to_ixdtf_string_with_provider(tz, rounding_opts, tz_provider())
                .map_err(temporal_err)?;
            Ok(Value::string(JsString::intern(&s)))
        },
        mm.clone(),
        fn_proto.clone(),
        "toString",
        0,
    );
    proto.define_property(
        PropertyKey::string("toString"),
        PropertyDescriptor::builtin_method(to_string_fn),
    );

    // toJSON()
    let to_json_fn = Value::native_function_with_proto_named(
        |this, _args, _ncx| {
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("not an Instant"))?;
            let instant = extract_instant(&obj)?;
            let s = instant
                .to_ixdtf_string_with_provider(
                    None,
                    temporal_rs::options::ToStringRoundingOptions::default(),
                    tz_provider(),
                )
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
        PropertyDescriptor::builtin_method(to_json_fn),
    );

    // toLocaleString()
    let to_locale_string_fn = Value::native_function_with_proto_named(
        |this, _args, _ncx| {
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("not an Instant"))?;
            let instant = extract_instant(&obj)?;
            let s = instant
                .to_ixdtf_string_with_provider(
                    None,
                    temporal_rs::options::ToStringRoundingOptions::default(),
                    tz_provider(),
                )
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
        PropertyDescriptor::builtin_method(to_locale_string_fn),
    );

    // valueOf() — always throws
    let value_of_fn = Value::native_function_with_proto_named(
        |_this, _args, _ncx| {
            Err(VmError::type_error(
                "use compare() or equals() to compare Temporal.Instant",
            ))
        },
        mm.clone(),
        fn_proto.clone(),
        "valueOf",
        0,
    );
    proto.define_property(
        PropertyKey::string("valueOf"),
        PropertyDescriptor::builtin_method(value_of_fn),
    );

    // @@toStringTag
    proto.define_property(
        PropertyKey::Symbol(crate::intrinsics::well_known::to_string_tag_symbol()),
        PropertyDescriptor::data_with_attrs(
            Value::string(JsString::intern("Temporal.Instant")),
            PropertyAttributes {
                writable: false,
                enumerable: false,
                configurable: true,
            },
        ),
    );

    // === Static methods ===

    // Instant.from(item)
    let from_ctor = ctor_value.clone();
    let from_fn = Value::native_function_with_proto_named(
        move |_this, args, ncx| {
            let item = args.first().cloned().unwrap_or(Value::undefined());

            if let Some(obj) = item.as_object() {
                // Try extracting as Instant (creates a copy)
                if let Ok(instant) = extract_instant(&obj) {
                    let ns = instant.epoch_nanoseconds().0;
                    return ncx.call_function_construct(
                        &from_ctor,
                        Value::undefined(),
                        &[Value::bigint(ns.to_string())],
                    );
                }
                // Try extracting as ZonedDateTime
                if let Ok(zdt) = extract_zoned_date_time(&obj) {
                    let ns = zdt.epoch_nanoseconds().0;
                    return ncx.call_function_construct(
                        &from_ctor,
                        Value::undefined(),
                        &[Value::bigint(ns.to_string())],
                    );
                }
                // Generic object: call toString, then parse as ISO string
                let s = ncx.to_string_value(&item)?;
                let instant =
                    temporal_rs::Instant::from_utf8(s.as_str().as_bytes()).map_err(temporal_err)?;
                let ns = instant.epoch_nanoseconds().0;
                return ncx.call_function_construct(
                    &from_ctor,
                    Value::undefined(),
                    &[Value::bigint(ns.to_string())],
                );
            }

            // String: parse ISO 8601
            if item.is_string() {
                let s = ncx.to_string_value(&item)?;
                let instant =
                    temporal_rs::Instant::from_utf8(s.as_str().as_bytes()).map_err(temporal_err)?;
                let ns = instant.epoch_nanoseconds().0;
                return ncx.call_function_construct(
                    &from_ctor,
                    Value::undefined(),
                    &[Value::bigint(ns.to_string())],
                );
            }

            Err(VmError::type_error("Cannot convert to Instant"))
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

    // Instant.compare(one, two)
    let compare_fn = Value::native_function_with_proto_named(
        |_this, args, ncx| {
            let one_val = args.first().cloned().unwrap_or(Value::undefined());
            let two_val = args.get(1).cloned().unwrap_or(Value::undefined());
            let one = to_temporal_instant(ncx, &one_val)?;
            let two = to_temporal_instant(ncx, &two_val)?;
            let ns1 = one.epoch_nanoseconds().0;
            let ns2 = two.epoch_nanoseconds().0;
            Ok(Value::number(if ns1 < ns2 {
                -1.0
            } else if ns1 > ns2 {
                1.0
            } else {
                0.0
            }))
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

    // Instant.fromEpochMilliseconds(epochMilliseconds)
    let from_ms_ctor = ctor_value.clone();
    let from_epoch_ms_fn = Value::native_function_with_proto_named(
        move |_this, args, ncx| {
            let ms_val = args.first().cloned().unwrap_or(Value::undefined());
            let ms = ncx.to_number_value(&ms_val)?;
            if ms.is_nan() || ms.is_infinite() || ms != ms.trunc() {
                return Err(VmError::range_error(
                    "epochMilliseconds must be a finite integer",
                ));
            }
            let ns = (ms as i128) * 1_000_000;
            const MAX_NS: i128 = 8_640_000_000_000_000_000_000;
            if ns < -MAX_NS || ns > MAX_NS {
                return Err(VmError::range_error("epoch nanoseconds out of range"));
            }
            ncx.call_function_construct(
                &from_ms_ctor,
                Value::undefined(),
                &[Value::bigint(ns.to_string())],
            )
        },
        mm.clone(),
        fn_proto.clone(),
        "fromEpochMilliseconds",
        1,
    );
    ctor_obj.define_property(
        PropertyKey::string("fromEpochMilliseconds"),
        PropertyDescriptor::data_with_attrs(from_epoch_ms_fn, PropertyAttributes::builtin_method()),
    );

    // Instant.fromEpochNanoseconds(epochNanoseconds)
    let from_ns_ctor = ctor_value.clone();
    let from_epoch_ns_fn = Value::native_function_with_proto_named(
        move |_this, args, ncx| {
            let ns_val = args.first().cloned().unwrap_or(Value::undefined());
            if !ns_val.is_bigint() {
                return Err(VmError::type_error("epochNanoseconds must be a BigInt"));
            }
            ncx.call_function_construct(&from_ns_ctor, Value::undefined(), &[ns_val])
        },
        mm.clone(),
        fn_proto.clone(),
        "fromEpochNanoseconds",
        1,
    );
    ctor_obj.define_property(
        PropertyKey::string("fromEpochNanoseconds"),
        PropertyDescriptor::data_with_attrs(from_epoch_ns_fn, PropertyAttributes::builtin_method()),
    );

    // Instant.fromEpochSeconds(epochSeconds)
    let from_s_ctor = ctor_value.clone();
    let from_epoch_s_fn = Value::native_function_with_proto_named(
        move |_this, args, ncx| {
            let s_val = args.first().cloned().unwrap_or(Value::undefined());
            let s = ncx.to_number_value(&s_val)?;
            if s.is_nan() || s.is_infinite() || s != s.trunc() {
                return Err(VmError::range_error(
                    "epochSeconds must be a finite integer",
                ));
            }
            let ns = (s as i128) * 1_000_000_000;
            const MAX_NS: i128 = 8_640_000_000_000_000_000_000;
            if ns < -MAX_NS || ns > MAX_NS {
                return Err(VmError::range_error("epoch nanoseconds out of range"));
            }
            ncx.call_function_construct(
                &from_s_ctor,
                Value::undefined(),
                &[Value::bigint(ns.to_string())],
            )
        },
        mm.clone(),
        fn_proto.clone(),
        "fromEpochSeconds",
        1,
    );
    ctor_obj.define_property(
        PropertyKey::string("fromEpochSeconds"),
        PropertyDescriptor::data_with_attrs(from_epoch_s_fn, PropertyAttributes::builtin_method()),
    );

    // Register on namespace
    temporal_obj.define_property(
        PropertyKey::string("Instant"),
        PropertyDescriptor::data_with_attrs(ctor_value, PropertyAttributes::builtin_method()),
    );
}

/// Parse difference options for Instant.until/since.
fn parse_instant_difference_options(
    ncx: &mut NativeContext<'_>,
    options_val: &Value,
) -> Result<temporal_rs::options::DifferenceSettings, VmError> {
    if options_val.is_undefined() {
        return Ok(temporal_rs::options::DifferenceSettings::default());
    }

    let _obj = get_options_object(options_val)?;

    let lu_val = get_option_value(ncx, options_val, "largestUnit")?;
    let largest_unit = if !lu_val.is_undefined() {
        let s = ncx.to_string_value(&lu_val)?;
        if s.as_str() == "auto" {
            None
        } else {
            Some(parse_temporal_unit(s.as_str())?)
        }
    } else {
        None
    };

    let ri_val = get_option_value(ncx, options_val, "roundingIncrement")?;
    let increment = if !ri_val.is_undefined() {
        let n = ncx.to_number_value(&ri_val)?;
        let n_u32 = n.trunc() as u32;
        Some(temporal_rs::options::RoundingIncrement::try_new(n_u32).map_err(temporal_err)?)
    } else {
        None
    };

    let rm_val = get_option_value(ncx, options_val, "roundingMode")?;
    let rounding_mode = if !rm_val.is_undefined() {
        let s = ncx.to_string_value(&rm_val)?;
        Some(
            s.as_str()
                .parse::<temporal_rs::options::RoundingMode>()
                .map_err(|_| VmError::range_error(format!("Invalid roundingMode: {}", s)))?,
        )
    } else {
        None
    };

    let su_val = get_option_value(ncx, options_val, "smallestUnit")?;
    let smallest_unit = if !su_val.is_undefined() {
        let s = ncx.to_string_value(&su_val)?;
        if s.as_str() == "auto" {
            None
        } else {
            Some(parse_temporal_unit(s.as_str())?)
        }
    } else {
        None
    };

    let mut settings = temporal_rs::options::DifferenceSettings::default();
    settings.largest_unit = largest_unit;
    settings.smallest_unit = smallest_unit;
    settings.rounding_mode = rounding_mode;
    settings.increment = increment;
    Ok(settings)
}

/// Parse rounding options for Instant.round().
fn parse_instant_rounding_options(
    ncx: &mut NativeContext<'_>,
    options_val: &Value,
) -> Result<temporal_rs::options::RoundingOptions, VmError> {
    if options_val.is_undefined() {
        return Err(VmError::type_error("round requires an options argument"));
    }

    // String shorthand: treated as smallestUnit
    if options_val.is_string() {
        let s = ncx.to_string_value(options_val)?;
        let unit = parse_temporal_unit(s.as_str())?;
        let mut opts = temporal_rs::options::RoundingOptions::default();
        opts.smallest_unit = Some(unit);
        return Ok(opts);
    }

    let _obj = get_options_object(options_val)?;

    let ri_val = get_option_value(ncx, options_val, "roundingIncrement")?;
    let increment = if !ri_val.is_undefined() {
        let n = ncx.to_number_value(&ri_val)?;
        let n_u32 = n.trunc() as u32;
        Some(temporal_rs::options::RoundingIncrement::try_new(n_u32).map_err(temporal_err)?)
    } else {
        None
    };

    let rm_val = get_option_value(ncx, options_val, "roundingMode")?;
    let rounding_mode = if !rm_val.is_undefined() {
        let s = ncx.to_string_value(&rm_val)?;
        Some(
            s.as_str()
                .parse::<temporal_rs::options::RoundingMode>()
                .map_err(|_| VmError::range_error(format!("Invalid roundingMode: {}", s)))?,
        )
    } else {
        None
    };

    let su_val = get_option_value(ncx, options_val, "smallestUnit")?;
    let smallest_unit = if !su_val.is_undefined() {
        let s = ncx.to_string_value(&su_val)?;
        Some(parse_temporal_unit(s.as_str())?)
    } else {
        None
    };

    let mut opts = temporal_rs::options::RoundingOptions::default();
    opts.smallest_unit = smallest_unit;
    opts.rounding_mode = rounding_mode;
    opts.increment = increment;
    Ok(opts)
}

/// Parse toString options for Instant.
fn parse_instant_to_string_options(
    ncx: &mut NativeContext<'_>,
    options_val: &Value,
) -> Result<
    (
        Option<temporal_rs::TimeZone>,
        temporal_rs::options::ToStringRoundingOptions,
    ),
    VmError,
> {
    if options_val.is_undefined() {
        return Ok((
            None,
            temporal_rs::options::ToStringRoundingOptions::default(),
        ));
    }

    let _obj = get_options_object(options_val)?;

    // Per spec ordering: fractionalSecondDigits, roundingMode, smallestUnit, timeZone

    // fractionalSecondDigits
    let fsd_val = get_option_value(ncx, options_val, "fractionalSecondDigits")?;
    let precision = if fsd_val.is_undefined() {
        temporal_rs::parsers::Precision::Auto
    } else if fsd_val.is_number() {
        let n = fsd_val.as_number().unwrap();
        if n.is_nan() {
            return Err(VmError::range_error(
                "fractionalSecondDigits must not be NaN",
            ));
        }
        let d = n.floor();
        if d < 0.0 || d > 9.0 || !d.is_finite() {
            return Err(VmError::range_error(
                "fractionalSecondDigits must be 0-9 or 'auto'",
            ));
        }
        temporal_rs::parsers::Precision::Digit(d as u8)
    } else {
        let s = ncx.to_string_value(&fsd_val)?;
        if s.as_str() == "auto" {
            temporal_rs::parsers::Precision::Auto
        } else {
            return Err(VmError::range_error(format!(
                "Invalid fractionalSecondDigits: {}",
                s
            )));
        }
    };

    // roundingMode
    let rm_val = get_option_value(ncx, options_val, "roundingMode")?;
    let rounding_mode = if rm_val.is_undefined() {
        None
    } else {
        let s = ncx.to_string_value(&rm_val)?;
        Some(
            s.as_str()
                .parse::<temporal_rs::options::RoundingMode>()
                .map_err(|_| VmError::range_error(format!("Invalid roundingMode: {}", s)))?,
        )
    };

    // smallestUnit
    let su_val = get_option_value(ncx, options_val, "smallestUnit")?;
    let smallest_unit = if su_val.is_undefined() {
        None
    } else {
        let s = ncx.to_string_value(&su_val)?;
        Some(parse_temporal_unit(s.as_str())?)
    };

    // timeZone — read LAST per spec ordering
    let tz_val = get_option_value(ncx, options_val, "timeZone")?;
    let tz = if !tz_val.is_undefined() {
        // Use to_temporal_timezone_identifier for type validation (TypeError for non-strings)
        Some(to_temporal_timezone_identifier(ncx, &tz_val)?)
    } else {
        None
    };

    Ok((
        tz,
        temporal_rs::options::ToStringRoundingOptions {
            precision,
            smallest_unit,
            rounding_mode,
        },
    ))
}

/// Get an options object, validating the type.
fn get_options_object(val: &Value) -> Result<Value, VmError> {
    if val.is_undefined() {
        return Ok(Value::undefined());
    }
    if val.is_null()
        || val.is_boolean()
        || val.is_number()
        || val.is_bigint()
        || val.is_string()
        || val.as_symbol().is_some()
    {
        return Err(VmError::type_error("Options must be an object"));
    }
    Ok(val.clone())
}

/// Get a single option value from an options object.
fn get_option_value(
    ncx: &mut NativeContext<'_>,
    options: &Value,
    name: &str,
) -> Result<Value, VmError> {
    if options.is_undefined() {
        return Ok(Value::undefined());
    }
    ncx.get_property_of_value(options, &PropertyKey::string(name))
}
