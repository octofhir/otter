use crate::context::NativeContext;
use crate::error::VmError;
use crate::gc::GcRef;
use crate::memory::MemoryManager;
use crate::object::{JsObject, PropertyAttributes, PropertyDescriptor, PropertyKey};
use crate::string::JsString;
use crate::temporal_value::TemporalValue;
use crate::value::Value;
use std::sync::Arc;

use super::common::*;

/// Get the compiled timezone provider from temporal_rs.
fn tz_provider() -> &'static impl temporal_rs::provider::TimeZoneProvider {
    &*temporal_rs::provider::COMPILED_TZ_PROVIDER
}

/// Create a ZonedDateTime JS value from a temporal_rs::ZonedDateTime.
fn construct_zdt_value(
    ncx: &mut NativeContext<'_>,
    zdt: &temporal_rs::ZonedDateTime,
) -> Result<Value, VmError> {
    let temporal_ns = ncx
        .ctx
        .get_global("Temporal")
        .ok_or_else(|| VmError::type_error("Temporal namespace not found"))?;
    let temporal_obj = temporal_ns
        .as_object()
        .ok_or_else(|| VmError::type_error("Temporal namespace not found"))?;
    let ctor = temporal_obj
        .get(&PropertyKey::string("ZonedDateTime"))
        .ok_or_else(|| VmError::type_error("ZonedDateTime constructor not found"))?;

    let epoch_ns_str = zdt.epoch_nanoseconds().0.to_string();
    let tz_id = zdt
        .time_zone()
        .identifier_with_provider(tz_provider())
        .unwrap_or_else(|_| "UTC".to_string());
    let cal_id = zdt.calendar().identifier().to_string();

    let epoch_bigint = Value::bigint(epoch_ns_str);

    ncx.call_function_construct(
        &ctor,
        Value::undefined(),
        &[
            epoch_bigint,
            Value::string(JsString::intern(&tz_id)),
            Value::string(JsString::intern(&cal_id)),
        ],
    )
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
        return Err(VmError::type_error(
            "options must be an object or undefined",
        ));
    }
    Ok(val.clone())
}

/// Get a property from options (handles proxy).
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

/// Parse the full set of ZonedDateTime.from options: disambiguation, offset, overflow.
fn parse_zdt_from_options(
    ncx: &mut NativeContext<'_>,
    options_val: &Value,
) -> Result<
    (
        temporal_rs::options::Disambiguation,
        temporal_rs::options::OffsetDisambiguation,
        temporal_rs::options::Overflow,
    ),
    VmError,
> {
    if options_val.is_undefined() {
        return Ok((
            temporal_rs::options::Disambiguation::Compatible,
            temporal_rs::options::OffsetDisambiguation::Reject,
            temporal_rs::options::Overflow::Constrain,
        ));
    }
    let _obj = get_options_object(options_val)?;

    // Read options in alphabetical order per spec
    let dis_val = get_option_value(ncx, options_val, "disambiguation")?;
    let disambiguation = if dis_val.is_undefined() {
        temporal_rs::options::Disambiguation::Compatible
    } else {
        let s = ncx.to_string_value(&dis_val)?;
        s.as_str()
            .parse::<temporal_rs::options::Disambiguation>()
            .map_err(|_| VmError::range_error(format!("Invalid disambiguation: {}", s)))?
    };

    let off_val = get_option_value(ncx, options_val, "offset")?;
    let offset = if off_val.is_undefined() {
        temporal_rs::options::OffsetDisambiguation::Reject
    } else {
        let s = ncx.to_string_value(&off_val)?;
        s.as_str()
            .parse::<temporal_rs::options::OffsetDisambiguation>()
            .map_err(|_| VmError::range_error(format!("Invalid offset option: {}", s)))?
    };

    let ov_val = get_option_value(ncx, options_val, "overflow")?;
    let overflow = if ov_val.is_undefined() {
        temporal_rs::options::Overflow::Constrain
    } else {
        let s = ncx.to_string_value(&ov_val)?;
        match s.as_str() {
            "constrain" => temporal_rs::options::Overflow::Constrain,
            "reject" => temporal_rs::options::Overflow::Reject,
            _ => return Err(VmError::range_error(format!("Invalid overflow: {}", s))),
        }
    };

    Ok((disambiguation, offset, overflow))
}

/// Resolve a timezone string/value into a canonical timezone.
/// Per spec, objects that aren't ZonedDateTime throw TypeError (ToTemporalTimeZoneIdentifier).
/// `allow_iso_strings`: if true, also parse ISO datetime strings to extract timezone (for property bags).
/// For the constructor, this should be false.
fn resolve_timezone(
    ncx: &mut NativeContext<'_>,
    tz_val: &Value,
    allow_iso_strings: bool,
) -> Result<temporal_rs::TimeZone, VmError> {
    if tz_val.is_undefined() {
        return Err(VmError::type_error("timeZone is required"));
    }

    if tz_val.is_null()
        || tz_val.is_boolean()
        || tz_val.is_number()
        || tz_val.is_bigint()
        || tz_val.as_symbol().is_some()
    {
        return Err(VmError::type_error(format!(
            "Cannot convert {} to a time zone",
            tz_val.type_of()
        )));
    }

    // Objects: only ZonedDateTime is allowed (extracts its timezone).
    // All other objects throw TypeError per ToTemporalTimeZoneIdentifier.
    if tz_val.as_object().is_some() || tz_val.as_proxy().is_some() {
        if let Some(obj) = tz_val.as_object() {
            if let Ok(zdt) = extract_zoned_date_time(&obj) {
                return Ok(zdt.time_zone().clone());
            }
        }
        return Err(VmError::type_error(format!(
            "{} is not a valid time zone",
            tz_val.type_of()
        )));
    }

    let s = ncx.to_string_value(tz_val)?;
    let s = s.as_str();

    if allow_iso_strings {
        // Property bag path: try identifier first, then ISO string parsing
        temporal_rs::TimeZone::try_from_identifier_str_with_provider(s, tz_provider())
            .or_else(|_| temporal_rs::TimeZone::try_from_str_with_provider(s, tz_provider()))
            .map_err(temporal_err)
    } else {
        // Constructor path: only accept timezone identifiers
        temporal_rs::TimeZone::try_from_identifier_str_with_provider(s, tz_provider())
            .map_err(temporal_err)
    }
}

/// Convert a JS value to BigInt i128 (spec ToBigInt).
fn value_to_bigint_i128(ncx: &mut NativeContext<'_>, val: &Value) -> Result<i128, VmError> {
    // If it's already a BigInt, extract value
    if val.is_bigint() {
        let s = ncx.to_string_value(val)?;
        let s = s.as_str().trim_end_matches('n');
        return s
            .parse::<i128>()
            .map_err(|_| VmError::range_error("epoch nanoseconds out of range"));
    }

    // ToBigInt conversion per spec
    if val.is_undefined() {
        return Err(VmError::type_error("Cannot convert undefined to a BigInt"));
    }
    if val.is_null() {
        return Err(VmError::type_error("Cannot convert null to a BigInt"));
    }
    if val.is_number() {
        return Err(VmError::type_error("Cannot convert a number to a BigInt"));
    }
    if val.as_symbol().is_some() {
        return Err(VmError::type_error(
            "Cannot convert a Symbol value to a BigInt",
        ));
    }
    if val.is_boolean() {
        let b = val.as_boolean().unwrap_or(false);
        return Ok(if b { 1 } else { 0 });
    }
    if val.is_string() {
        let s = ncx.to_string_value(val)?;
        let s = s.as_str().trim();
        if s.is_empty() {
            return Err(VmError::syntax_error(
                "Cannot convert empty string to a BigInt",
            ));
        }
        return s
            .parse::<i128>()
            .map_err(|_| VmError::syntax_error(format!("Cannot convert \"{}\" to a BigInt", s)));
    }

    // For objects/proxies, ToPrimitive then retry
    if val.as_object().is_some() || val.as_proxy().is_some() {
        let prim = ncx.to_primitive(val, crate::interpreter::PreferredType::Number)?;
        return value_to_bigint_i128(ncx, &prim);
    }

    Err(VmError::type_error("Cannot convert to BigInt"))
}

/// Install ZonedDateTime constructor and prototype onto `temporal_obj`.
pub(super) fn install_zoned_date_time(
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
        PropertyDescriptor::function_length(Value::string(JsString::intern("ZonedDateTime"))),
    );
    // length = 2 (epochNanoseconds, timeZone)
    ctor_obj.define_property(
        PropertyKey::string("length"),
        PropertyDescriptor::function_length(Value::number(2.0)),
    );

    // Constructor: new Temporal.ZonedDateTime(epochNanoseconds, timeZone [, calendar])
    let proto_for_ctor = proto.clone();
    let ctor_fn: Box<
        dyn Fn(&Value, &[Value], &mut NativeContext<'_>) -> Result<Value, VmError> + Send + Sync,
    > = Box::new(move |this, args, ncx| {
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
                "Temporal.ZonedDateTime constructor requires 'new'",
            ));
        }

        // Step 2: epochNanoseconds = ToBigInt(epochNanoseconds)
        let epoch_ns_arg = args.first().cloned().unwrap_or(Value::undefined());
        let epoch_ns = value_to_bigint_i128(ncx, &epoch_ns_arg)?;

        // Step 3: IsValidEpochNanoseconds
        let max_ns: i128 = 864 * 10i128.pow(19);
        if epoch_ns > max_ns || epoch_ns < -max_ns {
            return Err(VmError::range_error("epoch nanoseconds out of range"));
        }

        // Step 4: timeZone (constructor: no ISO strings)
        let tz_arg = args.get(1).cloned().unwrap_or(Value::undefined());
        let tz = resolve_timezone(ncx, &tz_arg, false)?;

        // Step 5: calendar (optional, default "iso8601")
        let cal_arg = args.get(2).cloned().unwrap_or(Value::undefined());
        let cal = if cal_arg.is_undefined() {
            temporal_rs::Calendar::default()
        } else {
            let cal_str = validate_calendar_arg_standalone(ncx, &cal_arg)?;
            temporal_rs::Calendar::try_from_utf8(cal_str.as_bytes()).map_err(temporal_err)?
        };

        // Create the temporal_rs::ZonedDateTime for validation
        let zdt =
            temporal_rs::ZonedDateTime::try_new_with_provider(epoch_ns, tz, cal, tz_provider())
                .map_err(temporal_err)?;

        // Store in internal slots using the new TemporalValue approach
        if let Some(obj) = this.as_object() {
            store_temporal_inner(&obj, TemporalValue::ZonedDateTime(zdt));
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

    // ========================================================================
    // ZonedDateTime.from() static method
    // ========================================================================
    let from_fn = Value::native_function_with_proto_named(
        move |_this, args, ncx| {
            let item = args.first().cloned().unwrap_or(Value::undefined());
            let options_val = args.get(1).cloned().unwrap_or(Value::undefined());

            // If it's a string, validate structure first (before options per spec).
            // temporal_rs handles Z→exact and no-offset→wall internally.
            if item.is_string() {
                let s = ncx.to_string_value(&item)?;
                let s_str = s.as_str();

                // Step 1: Validate string is well-formed by parsing with safe defaults.
                // Use OffsetDisambiguation::Use (never fails due to offset mismatch).
                // Any failure here means the string itself is invalid → throw before options.
                let initial_zdt = temporal_rs::ZonedDateTime::from_utf8_with_provider(
                    s_str.as_bytes(),
                    temporal_rs::options::Disambiguation::Compatible,
                    temporal_rs::options::OffsetDisambiguation::Use,
                    tz_provider(),
                )
                .map_err(temporal_err)?;

                // Step 2: Read and validate options (string was valid)
                let (disambiguation, offset_option, _overflow) =
                    parse_zdt_from_options(ncx, &options_val)?;

                // Step 3: Re-parse with actual options if they differ from safe defaults
                let zdt = if disambiguation != temporal_rs::options::Disambiguation::Compatible
                    || offset_option != temporal_rs::options::OffsetDisambiguation::Use
                {
                    temporal_rs::ZonedDateTime::from_utf8_with_provider(
                        s_str.as_bytes(),
                        disambiguation,
                        offset_option,
                        tz_provider(),
                    )
                    .map_err(temporal_err)?
                } else {
                    initial_zdt
                };

                return construct_zdt_value(ncx, &zdt);
            }

            // Check for proxy or object
            let is_proxy = item.as_proxy().is_some();
            if item.as_object().is_some() || is_proxy {
                // Check if it's an existing ZonedDateTime
                if let Some(obj) = item.as_object() {
                    if let Ok(zdt) = extract_zoned_date_time(&obj) {
                        let (_disambiguation, _offset_option, _overflow) =
                            parse_zdt_from_options(ncx, &options_val)?;
                        return construct_zdt_value(ncx, &zdt);
                    }
                }

                // Property bag — spec order: calendar, then fields alphabetically,
                // then offset, then timeZone, then options
                let get_field =
                    |ncx: &mut NativeContext<'_>, name: &str| -> Result<Value, VmError> {
                        ncx.get_property_of_value(&item, &PropertyKey::string(name))
                    };

                // 1. Read calendar — lenient validation (accepts ISO strings)
                let calendar_val = get_field(ncx, "calendar")?;
                let cal = if !calendar_val.is_undefined() {
                    resolve_calendar_from_property(ncx, &calendar_val)?;
                    temporal_rs::Calendar::default() // We only support iso8601
                } else {
                    temporal_rs::Calendar::default()
                };

                // 2. Read fields in alphabetical order (PrepareTemporalFields)
                let day_val = get_field(ncx, "day")?;
                let day = if !day_val.is_undefined() {
                    Some(to_integer_with_truncation(ncx, &day_val)? as i32)
                } else {
                    None
                };

                let hour_val = get_field(ncx, "hour")?;
                let hour = if !hour_val.is_undefined() {
                    to_integer_with_truncation(ncx, &hour_val)? as u8
                } else {
                    0
                };

                let microsecond_val = get_field(ncx, "microsecond")?;
                let microsecond = if !microsecond_val.is_undefined() {
                    to_integer_with_truncation(ncx, &microsecond_val)? as u16
                } else {
                    0
                };

                let millisecond_val = get_field(ncx, "millisecond")?;
                let millisecond = if !millisecond_val.is_undefined() {
                    to_integer_with_truncation(ncx, &millisecond_val)? as u16
                } else {
                    0
                };

                let minute_val = get_field(ncx, "minute")?;
                let minute = if !minute_val.is_undefined() {
                    to_integer_with_truncation(ncx, &minute_val)? as u8
                } else {
                    0
                };

                let month_val = get_field(ncx, "month")?;
                let month_num = if !month_val.is_undefined() {
                    Some(to_integer_with_truncation(ncx, &month_val)? as i32)
                } else {
                    None
                };

                let month_code_val = get_field(ncx, "monthCode")?;
                let mc_str = if !month_code_val.is_undefined() {
                    if !month_code_val.is_string() {
                        if month_code_val.as_object().is_some()
                            || month_code_val.as_proxy().is_some()
                        {
                            let prim = ncx.to_primitive(
                                &month_code_val,
                                crate::interpreter::PreferredType::String,
                            )?;
                            if !prim.is_string() {
                                return Err(VmError::type_error("monthCode must be a string"));
                            }
                            let mc = prim.as_string().unwrap().as_str().to_string();
                            // Syntax validation at read time
                            validate_month_code_syntax(&mc)?;
                            Some(mc)
                        } else {
                            return Err(VmError::type_error("monthCode must be a string"));
                        }
                    } else {
                        let mc = month_code_val.as_string().unwrap().as_str().to_string();
                        // Syntax validation at read time
                        validate_month_code_syntax(&mc)?;
                        Some(mc)
                    }
                } else {
                    None
                };

                let nanosecond_val = get_field(ncx, "nanosecond")?;
                let nanosecond = if !nanosecond_val.is_undefined() {
                    to_integer_with_truncation(ncx, &nanosecond_val)? as u16
                } else {
                    0
                };

                // 3. Read offset (after fields, before timeZone per spec)
                let offset_val = get_field(ncx, "offset")?;
                let offset_str = if !offset_val.is_undefined() {
                    if offset_val.as_symbol().is_some() {
                        return Err(VmError::type_error(
                            "Cannot convert a Symbol value to a string",
                        ));
                    }
                    if offset_val.is_null()
                        || offset_val.is_boolean()
                        || offset_val.is_number()
                        || offset_val.is_bigint()
                    {
                        return Err(VmError::type_error(format!(
                            "offset must be a string, got {}",
                            offset_val.type_of()
                        )));
                    }
                    let s = ncx.to_string_value(&offset_val)?;
                    let os = s.as_str().to_string();
                    // Validate offset syntax immediately
                    parse_offset_string_to_ns(&os)?;
                    Some(os)
                } else {
                    None
                };

                let second_val = get_field(ncx, "second")?;
                let second = if !second_val.is_undefined() {
                    to_integer_with_truncation(ncx, &second_val)? as u8
                } else {
                    0
                };

                // 4. Read timeZone (after offset per spec)
                // Property bag: allow ISO strings for timezone
                let tz_val = get_field(ncx, "timeZone")?;
                let tz = resolve_timezone(ncx, &tz_val, true)?;

                let year_val = get_field(ncx, "year")?;
                let year = if !year_val.is_undefined() {
                    Some(to_integer_with_truncation(ncx, &year_val)? as i32)
                } else {
                    None
                };

                // 5. Read options AFTER all fields
                let (disambiguation, offset_option, overflow) =
                    parse_zdt_from_options(ncx, &options_val)?;

                // Validate required fields
                let year = year.ok_or_else(|| VmError::type_error("year is required"))?;
                if month_num.is_none() && mc_str.is_none() {
                    return Err(VmError::type_error("month or monthCode is required"));
                }
                let day = day.ok_or_else(|| VmError::type_error("day is required"))?;

                // Validate non-negative (PrepareTemporalFields uses ToPositiveInteger)
                if let Some(m) = month_num {
                    if m < 1 {
                        return Err(VmError::range_error(format!(
                            "month must be >= 1, got {}",
                            m
                        )));
                    }
                }
                if day < 1 {
                    return Err(VmError::range_error(format!(
                        "day must be >= 1, got {}",
                        day
                    )));
                }

                // Resolve month — monthCode suitability validated AFTER options reading
                let month = if let Some(ref mc) = mc_str {
                    let mc_month = validate_month_code_iso_suitability(mc)? as i32;
                    if let Some(m) = month_num {
                        if m != mc_month {
                            return Err(VmError::range_error("month and monthCode must agree"));
                        }
                    }
                    mc_month
                } else {
                    month_num.unwrap()
                };

                // Build using from_partial_with_provider (handles month clamping via overflow)
                let offset_utc = if let Some(ref os) = offset_str {
                    let ns = parse_offset_string_to_ns(os)?;
                    let minutes = (ns / 60_000_000_000) as i16;
                    Some(temporal_rs::UtcOffset::from_minutes(minutes))
                } else {
                    None
                };

                // Clamp month if overflow=constrain (for valid values > 12, not for negatives)
                let month_u8 = if overflow == temporal_rs::options::Overflow::Constrain {
                    (month.clamp(1, 12)) as u8
                } else {
                    if month > 12 {
                        return Err(VmError::range_error(format!(
                            "month must be 1-12, got {}",
                            month
                        )));
                    }
                    month as u8
                };

                let mut partial = temporal_rs::partial::PartialZonedDateTime::new()
                    .with_calendar_fields(temporal_rs::fields::CalendarFields {
                        year: Some(year),
                        month: Some(month_u8),
                        month_code: None,
                        day: Some(day as u8),
                        era: None,
                        era_year: None,
                    })
                    .with_time(temporal_rs::partial::PartialTime {
                        hour: Some(hour),
                        minute: Some(minute),
                        second: Some(second),
                        millisecond: Some(millisecond),
                        microsecond: Some(microsecond),
                        nanosecond: Some(nanosecond),
                    })
                    .with_timezone(Some(tz));

                if let Some(offset) = offset_utc {
                    partial = partial.with_offset(offset);
                }

                let zdt = temporal_rs::ZonedDateTime::from_partial_with_provider(
                    partial,
                    Some(overflow),
                    Some(disambiguation),
                    if offset_str.is_some() {
                        Some(offset_option)
                    } else {
                        None
                    },
                    tz_provider(),
                )
                .map_err(temporal_err)?;

                // Override calendar if specified
                let zdt = if cal != temporal_rs::Calendar::default() {
                    zdt.with_calendar(cal)
                } else {
                    zdt
                };

                return construct_zdt_value(ncx, &zdt);
            }

            Err(VmError::type_error(
                "ZonedDateTime.from requires a string or property bag",
            ))
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
    // ZonedDateTime.compare() static method
    // ========================================================================
    let compare_fn = Value::native_function_with_proto_named(
        |_this, args, ncx| {
            let one_arg = args.first().cloned().unwrap_or(Value::undefined());
            let two_arg = args.get(1).cloned().unwrap_or(Value::undefined());
            let one = to_temporal_zdt(ncx, &one_arg)?;
            let two = to_temporal_zdt(ncx, &two_arg)?;
            match one.compare_instant(&two) {
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
    // Prototype getters
    // ========================================================================

    macro_rules! define_getter {
        ($proto:expr, $name:expr, $fn_proto:expr, $mm:expr, $extract:expr) => {
            let getter_fn = Value::native_function_with_proto_named(
                |this, _args, _ncx| {
                    let obj = this
                        .as_object()
                        .ok_or_else(|| VmError::type_error("not a ZonedDateTime"))?;
                    let zdt = extract_zoned_date_time(&obj)?;
                    #[allow(clippy::redundant_closure_call)]
                    Ok($extract(&zdt))
                },
                $mm.clone(),
                $fn_proto.clone(),
                concat!("get ", $name),
                0,
            );
            $proto.define_property(
                PropertyKey::string($name),
                PropertyDescriptor::getter(getter_fn),
            );
        };
    }

    define_getter!(
        proto,
        "calendarId",
        fn_proto,
        mm,
        |zdt: &temporal_rs::ZonedDateTime| {
            Value::string(JsString::intern(zdt.calendar().identifier()))
        }
    );

    define_getter!(
        proto,
        "timeZoneId",
        fn_proto,
        mm,
        |zdt: &temporal_rs::ZonedDateTime| {
            let tz_id = zdt
                .time_zone()
                .identifier_with_provider(tz_provider())
                .unwrap_or_else(|_| "UTC".to_string());
            Value::string(JsString::intern(&tz_id))
        }
    );

    define_getter!(
        proto,
        "year",
        fn_proto,
        mm,
        |zdt: &temporal_rs::ZonedDateTime| { Value::int32(zdt.year()) }
    );

    define_getter!(
        proto,
        "month",
        fn_proto,
        mm,
        |zdt: &temporal_rs::ZonedDateTime| { Value::int32(zdt.month() as i32) }
    );

    define_getter!(
        proto,
        "monthCode",
        fn_proto,
        mm,
        |zdt: &temporal_rs::ZonedDateTime| {
            Value::string(JsString::intern(zdt.month_code().as_str()))
        }
    );

    define_getter!(
        proto,
        "day",
        fn_proto,
        mm,
        |zdt: &temporal_rs::ZonedDateTime| { Value::int32(zdt.day() as i32) }
    );

    define_getter!(
        proto,
        "hour",
        fn_proto,
        mm,
        |zdt: &temporal_rs::ZonedDateTime| { Value::int32(zdt.hour() as i32) }
    );

    define_getter!(
        proto,
        "minute",
        fn_proto,
        mm,
        |zdt: &temporal_rs::ZonedDateTime| { Value::int32(zdt.minute() as i32) }
    );

    define_getter!(
        proto,
        "second",
        fn_proto,
        mm,
        |zdt: &temporal_rs::ZonedDateTime| { Value::int32(zdt.second() as i32) }
    );

    define_getter!(
        proto,
        "millisecond",
        fn_proto,
        mm,
        |zdt: &temporal_rs::ZonedDateTime| { Value::int32(zdt.millisecond() as i32) }
    );

    define_getter!(
        proto,
        "microsecond",
        fn_proto,
        mm,
        |zdt: &temporal_rs::ZonedDateTime| { Value::int32(zdt.microsecond() as i32) }
    );

    define_getter!(
        proto,
        "nanosecond",
        fn_proto,
        mm,
        |zdt: &temporal_rs::ZonedDateTime| { Value::int32(zdt.nanosecond() as i32) }
    );

    define_getter!(
        proto,
        "epochMilliseconds",
        fn_proto,
        mm,
        |zdt: &temporal_rs::ZonedDateTime| { Value::number(zdt.epoch_milliseconds() as f64) }
    );

    // epochNanoseconds - returns BigInt
    {
        let getter_fn = Value::native_function_with_proto_named(
            |this, _args, _ncx| {
                let obj = this
                    .as_object()
                    .ok_or_else(|| VmError::type_error("not a ZonedDateTime"))?;
                let zdt = extract_zoned_date_time(&obj)?;
                let ns_str = zdt.epoch_nanoseconds().0.to_string();
                Ok(Value::bigint(ns_str))
            },
            mm.clone(),
            fn_proto.clone(),
            "get epochNanoseconds",
            0,
        );
        proto.define_property(
            PropertyKey::string("epochNanoseconds"),
            PropertyDescriptor::getter(getter_fn),
        );
    }

    define_getter!(
        proto,
        "dayOfWeek",
        fn_proto,
        mm,
        |zdt: &temporal_rs::ZonedDateTime| { Value::int32(zdt.day_of_week() as i32) }
    );

    define_getter!(
        proto,
        "dayOfYear",
        fn_proto,
        mm,
        |zdt: &temporal_rs::ZonedDateTime| { Value::int32(zdt.day_of_year() as i32) }
    );

    define_getter!(
        proto,
        "weekOfYear",
        fn_proto,
        mm,
        |zdt: &temporal_rs::ZonedDateTime| {
            match zdt.week_of_year() {
                Some(w) => Value::int32(w as i32),
                None => Value::undefined(),
            }
        }
    );

    define_getter!(
        proto,
        "yearOfWeek",
        fn_proto,
        mm,
        |zdt: &temporal_rs::ZonedDateTime| {
            match zdt.year_of_week() {
                Some(y) => Value::int32(y),
                None => Value::undefined(),
            }
        }
    );

    define_getter!(
        proto,
        "daysInWeek",
        fn_proto,
        mm,
        |zdt: &temporal_rs::ZonedDateTime| { Value::int32(zdt.days_in_week() as i32) }
    );

    define_getter!(
        proto,
        "daysInMonth",
        fn_proto,
        mm,
        |zdt: &temporal_rs::ZonedDateTime| { Value::int32(zdt.days_in_month() as i32) }
    );

    define_getter!(
        proto,
        "daysInYear",
        fn_proto,
        mm,
        |zdt: &temporal_rs::ZonedDateTime| { Value::int32(zdt.days_in_year() as i32) }
    );

    define_getter!(
        proto,
        "monthsInYear",
        fn_proto,
        mm,
        |zdt: &temporal_rs::ZonedDateTime| { Value::int32(zdt.months_in_year() as i32) }
    );

    define_getter!(
        proto,
        "inLeapYear",
        fn_proto,
        mm,
        |zdt: &temporal_rs::ZonedDateTime| { Value::boolean(zdt.in_leap_year()) }
    );

    define_getter!(
        proto,
        "offset",
        fn_proto,
        mm,
        |zdt: &temporal_rs::ZonedDateTime| { Value::string(JsString::intern(&zdt.offset())) }
    );

    define_getter!(
        proto,
        "offsetNanoseconds",
        fn_proto,
        mm,
        |zdt: &temporal_rs::ZonedDateTime| { Value::number(zdt.offset_nanoseconds() as f64) }
    );

    define_getter!(
        proto,
        "era",
        fn_proto,
        mm,
        |zdt: &temporal_rs::ZonedDateTime| {
            match zdt.era() {
                Some(e) => Value::string(JsString::intern(e.as_str())),
                None => Value::undefined(),
            }
        }
    );

    define_getter!(
        proto,
        "eraYear",
        fn_proto,
        mm,
        |zdt: &temporal_rs::ZonedDateTime| {
            match zdt.era_year() {
                Some(y) => Value::int32(y),
                None => Value::undefined(),
            }
        }
    );

    // hoursInDay (needs provider)
    {
        let getter_fn = Value::native_function_with_proto_named(
            |this, _args, _ncx| {
                let obj = this
                    .as_object()
                    .ok_or_else(|| VmError::type_error("not a ZonedDateTime"))?;
                let zdt = extract_zoned_date_time(&obj)?;
                let hours = zdt
                    .hours_in_day_with_provider(tz_provider())
                    .map_err(temporal_err)?;
                Ok(Value::number(hours))
            },
            mm.clone(),
            fn_proto.clone(),
            "get hoursInDay",
            0,
        );
        proto.define_property(
            PropertyKey::string("hoursInDay"),
            PropertyDescriptor::getter(getter_fn),
        );
    }

    // ========================================================================
    // Prototype methods
    // ========================================================================

    // toString([options])
    let to_string_fn = Value::native_function_with_proto_named(
        |this, args, ncx| {
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("not a ZonedDateTime"))?;
            let zdt = extract_zoned_date_time(&obj)?;

            let options_val = args.first().cloned().unwrap_or(Value::undefined());
            let (display_offset, display_tz, display_cal, rounding_opts) =
                parse_zdt_to_string_options(ncx, &options_val)?;

            let s = zdt
                .to_ixdtf_string_with_provider(
                    display_offset,
                    display_tz,
                    display_cal,
                    rounding_opts,
                    tz_provider(),
                )
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
        PropertyDescriptor::data_with_attrs(to_string_fn, PropertyAttributes::builtin_method()),
    );

    // toJSON()
    let to_json_fn = Value::native_function_with_proto_named(
        |this, _args, _ncx| {
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("not a ZonedDateTime"))?;
            let zdt = extract_zoned_date_time(&obj)?;
            let s = zdt
                .to_string_with_provider(tz_provider())
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
        PropertyDescriptor::data_with_attrs(to_json_fn, PropertyAttributes::builtin_method()),
    );

    // toLocaleString()
    let to_locale_string_fn = Value::native_function_with_proto_named(
        |this, _args, _ncx| {
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("not a ZonedDateTime"))?;
            let zdt = extract_zoned_date_time(&obj)?;
            let s = zdt
                .to_string_with_provider(tz_provider())
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
        PropertyDescriptor::data_with_attrs(
            to_locale_string_fn,
            PropertyAttributes::builtin_method(),
        ),
    );

    // valueOf()
    let value_of_fn = Value::native_function_with_proto_named(
        |_this, _args, _ncx| {
            Err(VmError::type_error(
                "ZonedDateTime.prototype.valueOf is not allowed, use compare or equals instead",
            ))
        },
        mm.clone(),
        fn_proto.clone(),
        "valueOf",
        0,
    );
    proto.define_property(
        PropertyKey::string("valueOf"),
        PropertyDescriptor::data_with_attrs(value_of_fn, PropertyAttributes::builtin_method()),
    );

    // startOfDay()
    let start_of_day_fn = Value::native_function_with_proto_named(
        |this, _args, ncx| {
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("not a ZonedDateTime"))?;
            let zdt = extract_zoned_date_time(&obj)?;
            let sod = zdt
                .start_of_day_with_provider(tz_provider())
                .map_err(temporal_err)?;
            construct_zdt_value(ncx, &sod)
        },
        mm.clone(),
        fn_proto.clone(),
        "startOfDay",
        0,
    );
    proto.define_property(
        PropertyKey::string("startOfDay"),
        PropertyDescriptor::data_with_attrs(start_of_day_fn, PropertyAttributes::builtin_method()),
    );

    // toInstant()
    let to_instant_fn = Value::native_function_with_proto_named(
        |this, _args, ncx| {
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("not a ZonedDateTime"))?;
            let zdt = extract_zoned_date_time(&obj)?;
            let instant = zdt.to_instant();
            let ns_str = instant.epoch_nanoseconds().0.to_string();
            let temporal_ns = ncx
                .ctx
                .get_global("Temporal")
                .ok_or_else(|| VmError::type_error("Temporal namespace not found"))?;
            let temporal_obj = temporal_ns
                .as_object()
                .ok_or_else(|| VmError::type_error("Temporal namespace not found"))?;
            let ctor = temporal_obj
                .get(&PropertyKey::string("Instant"))
                .ok_or_else(|| VmError::type_error("Instant constructor not found"))?;
            let epoch_bigint = Value::bigint(ns_str);
            ncx.call_function_construct(&ctor, Value::undefined(), &[epoch_bigint])
        },
        mm.clone(),
        fn_proto.clone(),
        "toInstant",
        0,
    );
    proto.define_property(
        PropertyKey::string("toInstant"),
        PropertyDescriptor::data_with_attrs(to_instant_fn, PropertyAttributes::builtin_method()),
    );

    // toPlainDate()
    let to_plain_date_fn = Value::native_function_with_proto_named(
        |this, _args, ncx| {
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("not a ZonedDateTime"))?;
            let zdt = extract_zoned_date_time(&obj)?;
            let pd = zdt.to_plain_date();
            construct_plain_date_value(ncx, pd.year(), pd.month() as i32, pd.day() as i32)
        },
        mm.clone(),
        fn_proto.clone(),
        "toPlainDate",
        0,
    );
    proto.define_property(
        PropertyKey::string("toPlainDate"),
        PropertyDescriptor::data_with_attrs(to_plain_date_fn, PropertyAttributes::builtin_method()),
    );

    // toPlainTime()
    let to_plain_time_fn = Value::native_function_with_proto_named(
        |this, _args, ncx| {
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("not a ZonedDateTime"))?;
            let zdt = extract_zoned_date_time(&obj)?;
            let pt = zdt.to_plain_time();
            let temporal_ns = ncx
                .ctx
                .get_global("Temporal")
                .ok_or_else(|| VmError::type_error("Temporal namespace not found"))?;
            let temporal_obj = temporal_ns
                .as_object()
                .ok_or_else(|| VmError::type_error("Temporal namespace not found"))?;
            let ctor = temporal_obj
                .get(&PropertyKey::string("PlainTime"))
                .ok_or_else(|| VmError::type_error("PlainTime constructor not found"))?;
            ncx.call_function_construct(
                &ctor,
                Value::undefined(),
                &[
                    Value::int32(pt.hour() as i32),
                    Value::int32(pt.minute() as i32),
                    Value::int32(pt.second() as i32),
                    Value::int32(pt.millisecond() as i32),
                    Value::int32(pt.microsecond() as i32),
                    Value::int32(pt.nanosecond() as i32),
                ],
            )
        },
        mm.clone(),
        fn_proto.clone(),
        "toPlainTime",
        0,
    );
    proto.define_property(
        PropertyKey::string("toPlainTime"),
        PropertyDescriptor::data_with_attrs(to_plain_time_fn, PropertyAttributes::builtin_method()),
    );

    // toPlainDateTime()
    let to_plain_date_time_fn = Value::native_function_with_proto_named(
        |this, _args, ncx| {
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("not a ZonedDateTime"))?;
            let zdt = extract_zoned_date_time(&obj)?;
            let pdt = zdt.to_plain_date_time();
            let temporal_ns = ncx
                .ctx
                .get_global("Temporal")
                .ok_or_else(|| VmError::type_error("Temporal namespace not found"))?;
            let temporal_obj = temporal_ns
                .as_object()
                .ok_or_else(|| VmError::type_error("Temporal namespace not found"))?;
            let ctor = temporal_obj
                .get(&PropertyKey::string("PlainDateTime"))
                .ok_or_else(|| VmError::type_error("PlainDateTime constructor not found"))?;
            ncx.call_function_construct(
                &ctor,
                Value::undefined(),
                &[
                    Value::int32(pdt.year()),
                    Value::int32(pdt.month() as i32),
                    Value::int32(pdt.day() as i32),
                    Value::int32(pdt.hour() as i32),
                    Value::int32(pdt.minute() as i32),
                    Value::int32(pdt.second() as i32),
                    Value::int32(pdt.millisecond() as i32),
                    Value::int32(pdt.microsecond() as i32),
                    Value::int32(pdt.nanosecond() as i32),
                ],
            )
        },
        mm.clone(),
        fn_proto.clone(),
        "toPlainDateTime",
        0,
    );
    proto.define_property(
        PropertyKey::string("toPlainDateTime"),
        PropertyDescriptor::data_with_attrs(
            to_plain_date_time_fn,
            PropertyAttributes::builtin_method(),
        ),
    );

    // equals(other)
    let equals_fn = Value::native_function_with_proto_named(
        |this, args, ncx| {
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("not a ZonedDateTime"))?;
            let zdt = extract_zoned_date_time(&obj)?;
            let other_val = args.first().cloned().unwrap_or(Value::undefined());
            let other = to_temporal_zdt(ncx, &other_val)?;
            let eq = zdt
                .equals_with_provider(&other, tz_provider())
                .map_err(temporal_err)?;
            Ok(Value::boolean(eq))
        },
        mm.clone(),
        fn_proto.clone(),
        "equals",
        1,
    );
    proto.define_property(
        PropertyKey::string("equals"),
        PropertyDescriptor::data_with_attrs(equals_fn, PropertyAttributes::builtin_method()),
    );

    // withCalendar(calendar)
    let with_calendar_fn = Value::native_function_with_proto_named(
        |this, args, ncx| {
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("not a ZonedDateTime"))?;
            let zdt = extract_zoned_date_time(&obj)?;
            let cal_arg = args.first().cloned().unwrap_or(Value::undefined());
            if cal_arg.is_undefined() {
                return Err(VmError::type_error("calendar argument is required"));
            }
            resolve_calendar_from_property(ncx, &cal_arg)?;
            let cal = temporal_rs::Calendar::default(); // We only support iso8601
            let new_zdt = zdt.with_calendar(cal);
            construct_zdt_value(ncx, &new_zdt)
        },
        mm.clone(),
        fn_proto.clone(),
        "withCalendar",
        1,
    );
    proto.define_property(
        PropertyKey::string("withCalendar"),
        PropertyDescriptor::data_with_attrs(with_calendar_fn, PropertyAttributes::builtin_method()),
    );

    // withTimeZone(timeZone)
    let with_time_zone_fn = Value::native_function_with_proto_named(
        |this, args, ncx| {
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("not a ZonedDateTime"))?;
            let zdt = extract_zoned_date_time(&obj)?;
            let tz_arg = args.first().cloned().unwrap_or(Value::undefined());
            let tz = resolve_timezone(ncx, &tz_arg, true)?;
            let new_zdt = zdt
                .with_time_zone_with_provider(tz, tz_provider())
                .map_err(temporal_err)?;
            construct_zdt_value(ncx, &new_zdt)
        },
        mm.clone(),
        fn_proto.clone(),
        "withTimeZone",
        1,
    );
    proto.define_property(
        PropertyKey::string("withTimeZone"),
        PropertyDescriptor::data_with_attrs(
            with_time_zone_fn,
            PropertyAttributes::builtin_method(),
        ),
    );

    // withPlainTime([plainTime])
    let with_plain_time_fn = Value::native_function_with_proto_named(
        |this, args, ncx| {
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("not a ZonedDateTime"))?;
            let zdt = extract_zoned_date_time(&obj)?;
            let pt_arg = args.first().cloned().unwrap_or(Value::undefined());
            let pt = if pt_arg.is_undefined() {
                None
            } else {
                Some(to_temporal_plain_time(ncx, &pt_arg)?)
            };
            let new_zdt = zdt
                .with_plain_time_and_provider(pt, tz_provider())
                .map_err(temporal_err)?;
            construct_zdt_value(ncx, &new_zdt)
        },
        mm.clone(),
        fn_proto.clone(),
        "withPlainTime",
        0,
    );
    proto.define_property(
        PropertyKey::string("withPlainTime"),
        PropertyDescriptor::data_with_attrs(
            with_plain_time_fn,
            PropertyAttributes::builtin_method(),
        ),
    );

    // add(duration [, options])
    let add_fn = Value::native_function_with_proto_named(
        |this, args, ncx| {
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("not a ZonedDateTime"))?;
            let zdt = extract_zoned_date_time(&obj)?;
            let dur_val = args.first().cloned().unwrap_or(Value::undefined());
            let dur = to_temporal_duration(ncx, &dur_val)?;
            let options_val = args.get(1).cloned().unwrap_or(Value::undefined());
            let overflow = if !options_val.is_undefined() {
                Some(parse_overflow_option(ncx, &options_val)?)
            } else {
                None
            };
            let new_zdt = zdt
                .add_with_provider(&dur, overflow, tz_provider())
                .map_err(temporal_err)?;
            construct_zdt_value(ncx, &new_zdt)
        },
        mm.clone(),
        fn_proto.clone(),
        "add",
        1,
    );
    proto.define_property(
        PropertyKey::string("add"),
        PropertyDescriptor::data_with_attrs(add_fn, PropertyAttributes::builtin_method()),
    );

    // subtract(duration [, options])
    let subtract_fn = Value::native_function_with_proto_named(
        |this, args, ncx| {
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("not a ZonedDateTime"))?;
            let zdt = extract_zoned_date_time(&obj)?;
            let dur_val = args.first().cloned().unwrap_or(Value::undefined());
            let dur = to_temporal_duration(ncx, &dur_val)?;
            let options_val = args.get(1).cloned().unwrap_or(Value::undefined());
            let overflow = if !options_val.is_undefined() {
                Some(parse_overflow_option(ncx, &options_val)?)
            } else {
                None
            };
            let new_zdt = zdt
                .subtract_with_provider(&dur, overflow, tz_provider())
                .map_err(temporal_err)?;
            construct_zdt_value(ncx, &new_zdt)
        },
        mm.clone(),
        fn_proto.clone(),
        "subtract",
        1,
    );
    proto.define_property(
        PropertyKey::string("subtract"),
        PropertyDescriptor::data_with_attrs(subtract_fn, PropertyAttributes::builtin_method()),
    );

    // until(other [, options])
    let until_fn = Value::native_function_with_proto_named(
        |this, args, ncx| {
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("not a ZonedDateTime"))?;
            let zdt = extract_zoned_date_time(&obj)?;
            let other_val = args.first().cloned().unwrap_or(Value::undefined());
            let other = to_temporal_zdt(ncx, &other_val)?;
            let options_val = args.get(1).cloned().unwrap_or(Value::undefined());
            let settings = parse_zdt_difference_options(ncx, &options_val)?;
            let dur = zdt
                .until_with_provider(&other, settings, tz_provider())
                .map_err(temporal_err)?;
            construct_duration_value(ncx, &dur)
        },
        mm.clone(),
        fn_proto.clone(),
        "until",
        1,
    );
    proto.define_property(
        PropertyKey::string("until"),
        PropertyDescriptor::data_with_attrs(until_fn, PropertyAttributes::builtin_method()),
    );

    // since(other [, options])
    let since_fn = Value::native_function_with_proto_named(
        |this, args, ncx| {
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("not a ZonedDateTime"))?;
            let zdt = extract_zoned_date_time(&obj)?;
            let other_val = args.first().cloned().unwrap_or(Value::undefined());
            let other = to_temporal_zdt(ncx, &other_val)?;
            let options_val = args.get(1).cloned().unwrap_or(Value::undefined());
            let settings = parse_zdt_difference_options(ncx, &options_val)?;
            let dur = zdt
                .since_with_provider(&other, settings, tz_provider())
                .map_err(temporal_err)?;
            construct_duration_value(ncx, &dur)
        },
        mm.clone(),
        fn_proto.clone(),
        "since",
        1,
    );
    proto.define_property(
        PropertyKey::string("since"),
        PropertyDescriptor::data_with_attrs(since_fn, PropertyAttributes::builtin_method()),
    );

    // with(fields [, options])
    let with_fn = Value::native_function_with_proto_named(
        |this, args, ncx| {
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("not a ZonedDateTime"))?;
            let zdt = extract_zoned_date_time(&obj)?;
            let fields_val = args.first().cloned().unwrap_or(Value::undefined());
            if fields_val.as_object().is_none() && fields_val.as_proxy().is_none() {
                return Err(VmError::type_error("fields must be an object"));
            }
            // RejectTemporalLikeObject:
            // 1. Check for Temporal internal slots (any Temporal type)
            if let Some(fobj) = fields_val.as_object() {
                if extract_temporal_inner(&fobj).is_ok() {
                    return Err(VmError::type_error(
                        "with() does not accept a Temporal object",
                    ));
                }
            }
            // 2. Check calendar property on plain objects
            let calendar_check =
                ncx.get_property_of_value(&fields_val, &PropertyKey::string("calendar"))?;
            if !calendar_check.is_undefined() {
                return Err(VmError::type_error(
                    "with() does not accept a calendar property",
                ));
            }
            // 3. Check timeZone property on plain objects
            let timezone_check =
                ncx.get_property_of_value(&fields_val, &PropertyKey::string("timeZone"))?;
            if !timezone_check.is_undefined() {
                return Err(VmError::type_error(
                    "with() does not accept a timeZone property",
                ));
            }
            // Read and validate fields (before options, per spec order)
            let fields = parse_zdt_fields(ncx, &fields_val)?;
            let options_val = args.get(1).cloned().unwrap_or(Value::undefined());
            let (disambiguation, offset_option, overflow) =
                parse_zdt_from_options(ncx, &options_val)?;
            let new_zdt = zdt
                .with_with_provider(
                    fields,
                    Some(disambiguation),
                    Some(offset_option),
                    Some(overflow),
                    tz_provider(),
                )
                .map_err(temporal_err)?;
            construct_zdt_value(ncx, &new_zdt)
        },
        mm.clone(),
        fn_proto.clone(),
        "with",
        1,
    );
    proto.define_property(
        PropertyKey::string("with"),
        PropertyDescriptor::data_with_attrs(with_fn, PropertyAttributes::builtin_method()),
    );

    // round(options)
    let round_fn = Value::native_function_with_proto_named(
        |this, args, ncx| {
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("not a ZonedDateTime"))?;
            let zdt = extract_zoned_date_time(&obj)?;
            let options_val = args.first().cloned().unwrap_or(Value::undefined());
            let round_opts = parse_zdt_rounding_options(ncx, &options_val)?;
            let new_zdt = zdt
                .round_with_provider(round_opts, tz_provider())
                .map_err(temporal_err)?;
            construct_zdt_value(ncx, &new_zdt)
        },
        mm.clone(),
        fn_proto.clone(),
        "round",
        1,
    );
    proto.define_property(
        PropertyKey::string("round"),
        PropertyDescriptor::data_with_attrs(round_fn, PropertyAttributes::builtin_method()),
    );

    // getTimeZoneTransition(direction)
    let get_tz_transition_fn = Value::native_function_with_proto_named(
        |this, args, ncx| {
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("not a ZonedDateTime"))?;
            let zdt = extract_zoned_date_time(&obj)?;
            let dir_val = args.first().cloned().unwrap_or(Value::undefined());
            let direction = parse_transition_direction(ncx, &dir_val)?;
            let result = zdt
                .get_time_zone_transition_with_provider(direction, tz_provider())
                .map_err(temporal_err)?;
            match result {
                Some(new_zdt) => construct_zdt_value(ncx, &new_zdt),
                None => Ok(Value::null()),
            }
        },
        mm.clone(),
        fn_proto.clone(),
        "getTimeZoneTransition",
        1,
    );
    proto.define_property(
        PropertyKey::string("getTimeZoneTransition"),
        PropertyDescriptor::data_with_attrs(
            get_tz_transition_fn,
            PropertyAttributes::builtin_method(),
        ),
    );

    // toPlainYearMonth()
    let to_plain_year_month_fn = Value::native_function_with_proto_named(
        |this, _args, ncx| {
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("not a ZonedDateTime"))?;
            let zdt = extract_zoned_date_time(&obj)?;
            construct_plain_year_month_value(ncx, zdt.year(), zdt.month() as i32)
        },
        mm.clone(),
        fn_proto.clone(),
        "toPlainYearMonth",
        0,
    );
    proto.define_property(
        PropertyKey::string("toPlainYearMonth"),
        PropertyDescriptor::data_with_attrs(
            to_plain_year_month_fn,
            PropertyAttributes::builtin_method(),
        ),
    );

    // toPlainMonthDay()
    let to_plain_month_day_fn = Value::native_function_with_proto_named(
        |this, _args, ncx| {
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("not a ZonedDateTime"))?;
            let zdt = extract_zoned_date_time(&obj)?;
            construct_plain_month_day_value(ncx, zdt.month() as i32, zdt.day() as i32)
        },
        mm.clone(),
        fn_proto.clone(),
        "toPlainMonthDay",
        0,
    );
    proto.define_property(
        PropertyKey::string("toPlainMonthDay"),
        PropertyDescriptor::data_with_attrs(
            to_plain_month_day_fn,
            PropertyAttributes::builtin_method(),
        ),
    );

    // toInstant()
    let to_instant_fn = Value::native_function_with_proto_named(
        |this, _args, ncx| {
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("not a ZonedDateTime"))?;
            let zdt = extract_zoned_date_time(&obj)?;
            let epoch_ns = zdt.epoch_nanoseconds().0;

            // Get Temporal.Instant constructor and create an instance
            let temporal_ns = ncx
                .ctx
                .get_global("Temporal")
                .ok_or_else(|| VmError::type_error("Temporal not found"))?;
            let temporal_obj = temporal_ns
                .as_object()
                .ok_or_else(|| VmError::type_error("Temporal not found"))?;
            let instant_ctor = temporal_obj
                .get(&PropertyKey::string("Instant"))
                .ok_or_else(|| VmError::type_error("Instant not found"))?;

            let epoch_bigint = Value::bigint(epoch_ns.to_string());
            ncx.call_function_construct(&instant_ctor, Value::undefined(), &[epoch_bigint])
        },
        mm.clone(),
        fn_proto.clone(),
        "toInstant",
        0,
    );
    proto.define_property(
        PropertyKey::string("toInstant"),
        PropertyDescriptor::data_with_attrs(to_instant_fn, PropertyAttributes::builtin_method()),
    );

    // getISOFields()
    let get_iso_fields_fn = Value::native_function_with_proto_named(
        |this, _args, ncx| {
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("not a ZonedDateTime"))?;
            let zdt = extract_zoned_date_time(&obj)?;
            let result = GcRef::new(JsObject::new(
                Value::undefined(),
                ncx.ctx.memory_manager().clone(),
            ));
            result.define_property(
                PropertyKey::string("calendar"),
                PropertyDescriptor::data(Value::string(JsString::intern(
                    zdt.calendar().identifier(),
                ))),
            );
            result.define_property(
                PropertyKey::string("isoDay"),
                PropertyDescriptor::data(Value::int32(zdt.day() as i32)),
            );
            result.define_property(
                PropertyKey::string("isoHour"),
                PropertyDescriptor::data(Value::int32(zdt.hour() as i32)),
            );
            result.define_property(
                PropertyKey::string("isoMicrosecond"),
                PropertyDescriptor::data(Value::int32(zdt.microsecond() as i32)),
            );
            result.define_property(
                PropertyKey::string("isoMillisecond"),
                PropertyDescriptor::data(Value::int32(zdt.millisecond() as i32)),
            );
            result.define_property(
                PropertyKey::string("isoMinute"),
                PropertyDescriptor::data(Value::int32(zdt.minute() as i32)),
            );
            result.define_property(
                PropertyKey::string("isoMonth"),
                PropertyDescriptor::data(Value::int32(zdt.month() as i32)),
            );
            result.define_property(
                PropertyKey::string("isoNanosecond"),
                PropertyDescriptor::data(Value::int32(zdt.nanosecond() as i32)),
            );
            result.define_property(
                PropertyKey::string("isoSecond"),
                PropertyDescriptor::data(Value::int32(zdt.second() as i32)),
            );
            result.define_property(
                PropertyKey::string("isoYear"),
                PropertyDescriptor::data(Value::int32(zdt.year())),
            );
            let offset_str = zdt.offset();
            result.define_property(
                PropertyKey::string("offset"),
                PropertyDescriptor::data(Value::string(JsString::intern(&offset_str))),
            );
            let tz_id = zdt
                .time_zone()
                .identifier_with_provider(tz_provider())
                .unwrap_or_else(|_| "UTC".to_string());
            result.define_property(
                PropertyKey::string("timeZone"),
                PropertyDescriptor::data(Value::string(JsString::intern(&tz_id))),
            );
            Ok(Value::object(result))
        },
        mm.clone(),
        fn_proto.clone(),
        "getISOFields",
        0,
    );
    proto.define_property(
        PropertyKey::string("getISOFields"),
        PropertyDescriptor::data_with_attrs(
            get_iso_fields_fn,
            PropertyAttributes::builtin_method(),
        ),
    );

    // @@toStringTag
    proto.define_property(
        PropertyKey::Symbol(crate::intrinsics::well_known::to_string_tag_symbol()),
        PropertyDescriptor::data_with_attrs(
            Value::string(JsString::intern("Temporal.ZonedDateTime")),
            PropertyAttributes {
                writable: false,
                enumerable: false,
                configurable: true,
            },
        ),
    );

    // Register on namespace
    temporal_obj.define_property(
        PropertyKey::string("ZonedDateTime"),
        PropertyDescriptor::data_with_attrs(ctor_value, PropertyAttributes::builtin_method()),
    );
}

// ============================================================================
// Helpers
// ============================================================================

fn to_temporal_zdt(
    ncx: &mut NativeContext<'_>,
    val: &Value,
) -> Result<temporal_rs::ZonedDateTime, VmError> {
    // 1. If object with ZonedDateTime internal slot, extract directly
    if let Some(obj) = val.as_object() {
        if let Ok(zdt) = extract_zoned_date_time(&obj) {
            return Ok(zdt);
        }
    }

    // 2. If object (not ZDT), treat as property bag
    if val.as_object().is_some() || val.as_proxy().is_some() {
        let get = |ncx: &mut NativeContext<'_>, name: &str| -> Result<Value, VmError> {
            ncx.get_property_of_value(val, &PropertyKey::string(name))
        };

        // Read calendar — lenient validation (accepts ISO strings)
        let calendar_val = get(ncx, "calendar")?;
        let cal = if !calendar_val.is_undefined() {
            resolve_calendar_from_property(ncx, &calendar_val)?;
            temporal_rs::Calendar::default() // We only support iso8601
        } else {
            temporal_rs::Calendar::default()
        };

        // Read fields
        let day_val = get(ncx, "day")?;
        let day = if !day_val.is_undefined() {
            Some(to_integer_with_truncation(ncx, &day_val)? as u8)
        } else {
            None
        };
        let hour_val = get(ncx, "hour")?;
        let hour = if !hour_val.is_undefined() {
            to_integer_with_truncation(ncx, &hour_val)? as u8
        } else {
            0
        };
        let microsecond_val = get(ncx, "microsecond")?;
        let microsecond = if !microsecond_val.is_undefined() {
            to_integer_with_truncation(ncx, &microsecond_val)? as u16
        } else {
            0
        };
        let millisecond_val = get(ncx, "millisecond")?;
        let millisecond = if !millisecond_val.is_undefined() {
            to_integer_with_truncation(ncx, &millisecond_val)? as u16
        } else {
            0
        };
        let minute_val = get(ncx, "minute")?;
        let minute = if !minute_val.is_undefined() {
            to_integer_with_truncation(ncx, &minute_val)? as u8
        } else {
            0
        };
        let month_val = get(ncx, "month")?;
        let month_num = if !month_val.is_undefined() {
            Some(to_integer_with_truncation(ncx, &month_val)? as i32)
        } else {
            None
        };
        let month_code_val = get(ncx, "monthCode")?;
        let mc_str = if !month_code_val.is_undefined() {
            let s = ncx.to_string_value(&month_code_val)?;
            Some(s.as_str().to_string())
        } else {
            None
        };
        let nanosecond_val = get(ncx, "nanosecond")?;
        let nanosecond = if !nanosecond_val.is_undefined() {
            to_integer_with_truncation(ncx, &nanosecond_val)? as u16
        } else {
            0
        };

        // Read offset
        let offset_val = get(ncx, "offset")?;
        let offset_utc = if !offset_val.is_undefined() {
            // Offset must be a string or object (ToString for objects)
            let s = if offset_val.is_string() {
                offset_val.as_string().unwrap().as_str().to_string()
            } else if offset_val.as_object().is_some() || offset_val.as_proxy().is_some() {
                ncx.to_string_value(&offset_val)?.as_str().to_string()
            } else {
                return Err(VmError::type_error("offset must be a string"));
            };
            let ns = parse_offset_string_to_ns(&s)?;
            let minutes = (ns / 60_000_000_000) as i16;
            Some(temporal_rs::UtcOffset::from_minutes(minutes))
        } else {
            None
        };

        let second_val = get(ncx, "second")?;
        let second = if !second_val.is_undefined() {
            to_integer_with_truncation(ncx, &second_val)? as u8
        } else {
            0
        };

        // Read timeZone (allow ISO strings for property bags)
        let tz_val = get(ncx, "timeZone")?;
        let tz = resolve_timezone(ncx, &tz_val, true)?;

        let year_val = get(ncx, "year")?;
        let year = if !year_val.is_undefined() {
            to_integer_with_truncation(ncx, &year_val)? as i32
        } else {
            return Err(VmError::type_error("year is required"));
        };

        // Resolve month
        let month = if let Some(ref mc) = mc_str {
            let mc_month = validate_month_code_iso_suitability(mc)? as i32;
            if let Some(m) = month_num {
                if m != mc_month {
                    return Err(VmError::range_error("month and monthCode must agree"));
                }
            }
            mc_month as u8
        } else if let Some(m) = month_num {
            if m < 1 || m > 12 {
                return Err(VmError::range_error(format!("month out of range: {}", m)));
            }
            m as u8
        } else {
            return Err(VmError::type_error("month or monthCode is required"));
        };

        let day = day.ok_or_else(|| VmError::type_error("day is required"))?;

        let mut partial = temporal_rs::partial::PartialZonedDateTime::new()
            .with_calendar_fields(temporal_rs::fields::CalendarFields {
                year: Some(year),
                month: Some(month),
                month_code: None,
                day: Some(day),
                era: None,
                era_year: None,
            })
            .with_time(temporal_rs::partial::PartialTime {
                hour: Some(hour),
                minute: Some(minute),
                second: Some(second),
                millisecond: Some(millisecond),
                microsecond: Some(microsecond),
                nanosecond: Some(nanosecond),
            })
            .with_timezone(Some(tz));

        if let Some(offset) = offset_utc {
            partial = partial.with_offset(offset);
        }

        let zdt = temporal_rs::ZonedDateTime::from_partial_with_provider(
            partial,
            Some(temporal_rs::options::Overflow::Constrain),
            Some(temporal_rs::options::Disambiguation::Compatible),
            if offset_utc.is_some() {
                Some(temporal_rs::options::OffsetDisambiguation::Reject)
            } else {
                None
            },
            tz_provider(),
        )
        .map_err(temporal_err)?;

        // Apply calendar if non-default
        let zdt = if cal != temporal_rs::Calendar::default() {
            zdt.with_calendar(cal)
        } else {
            zdt
        };

        return Ok(zdt);
    }

    // 3. If string, parse
    if val.is_string() {
        let s = ncx.to_string_value(val)?;
        let zdt = temporal_rs::ZonedDateTime::from_utf8_with_provider(
            s.as_str().as_bytes(),
            temporal_rs::options::Disambiguation::Compatible,
            temporal_rs::options::OffsetDisambiguation::Reject,
            tz_provider(),
        )
        .map_err(temporal_err)?;
        return Ok(zdt);
    }

    Err(VmError::type_error("Cannot convert to ZonedDateTime"))
}

fn to_temporal_plain_time(
    ncx: &mut NativeContext<'_>,
    val: &Value,
) -> Result<temporal_rs::PlainTime, VmError> {
    // Check for known Temporal types first via TemporalValue extraction
    if let Some(obj) = val.as_object() {
        if let Ok(inner) = extract_temporal_inner(&obj) {
            match &*inner {
                TemporalValue::PlainTime(pt) => {
                    return Ok(*pt);
                }
                TemporalValue::PlainDateTime(pdt) => {
                    return temporal_rs::PlainTime::new(
                        pdt.hour(),
                        pdt.minute(),
                        pdt.second(),
                        pdt.millisecond(),
                        pdt.microsecond(),
                        pdt.nanosecond(),
                    )
                    .map_err(temporal_err);
                }
                TemporalValue::ZonedDateTime(zdt) => {
                    return temporal_rs::PlainTime::new(
                        zdt.hour(),
                        zdt.minute(),
                        zdt.second(),
                        zdt.millisecond(),
                        zdt.microsecond(),
                        zdt.nanosecond(),
                    )
                    .map_err(temporal_err);
                }
                _ => {}
            }
        }

        // Property bag: read time fields in alphabetical order (per spec)
        return read_time_property_bag(ncx, val);
    }

    // Also handle proxies as property bags
    if val.as_proxy().is_some() {
        return read_time_property_bag(ncx, val);
    }

    if val.is_string() {
        let s = ncx.to_string_value(val)?;
        return temporal_rs::PlainTime::from_utf8(s.as_str().as_bytes()).map_err(temporal_err);
    }

    Err(VmError::type_error("Cannot convert to PlainTime"))
}

fn read_time_property_bag(
    ncx: &mut NativeContext<'_>,
    val: &Value,
) -> Result<temporal_rs::PlainTime, VmError> {
    let mut get_int = |name: &str| -> Result<Option<f64>, VmError> {
        let v = ncx.get_property_of_value(val, &PropertyKey::string(name))?;
        if v.is_undefined() {
            Ok(None)
        } else {
            Ok(Some(to_integer_with_truncation(ncx, &v)?))
        }
    };

    let hour = get_int("hour")?;
    let microsecond = get_int("microsecond")?;
    let millisecond = get_int("millisecond")?;
    let minute = get_int("minute")?;
    let nanosecond = get_int("nanosecond")?;
    let second = get_int("second")?;

    // At least one property must be present
    if hour.is_none()
        && minute.is_none()
        && second.is_none()
        && millisecond.is_none()
        && microsecond.is_none()
        && nanosecond.is_none()
    {
        return Err(VmError::type_error("Cannot convert to PlainTime"));
    }

    temporal_rs::PlainTime::new(
        hour.unwrap_or(0.0) as u8,
        minute.unwrap_or(0.0) as u8,
        second.unwrap_or(0.0) as u8,
        millisecond.unwrap_or(0.0) as u16,
        microsecond.unwrap_or(0.0) as u16,
        nanosecond.unwrap_or(0.0) as u16,
    )
    .map_err(temporal_err)
}

/// Parse an offset string like "+01:00", "-05:30", "Z" to nanoseconds.
/// Strictly validates the format per the Temporal spec:
/// ±HH:MM, ±HHMM, ±HH:MM:SS, ±HHMMSS, ±HH:MM:SS.f{1-9}, ±HHMMSS.f{1-9}
fn parse_offset_string_to_ns(s: &str) -> Result<i64, VmError> {
    if s == "Z" || s == "z" {
        return Ok(0);
    }

    let err = || VmError::range_error(format!("Invalid offset string: {}", s));

    let (sign, rest) = if let Some(stripped) = s.strip_prefix('+') {
        (1i64, stripped)
    } else if let Some(stripped) = s.strip_prefix('-') {
        (-1i64, stripped)
    } else if let Some(stripped) = s.strip_prefix('\u{2212}') {
        (-1i64, stripped)
    } else {
        return Err(err());
    };

    // Must have at least 2 chars for hours
    if rest.len() < 2 {
        return Err(err());
    }

    // Hours: exactly 2 digits
    let hh = &rest[..2];
    if !hh.bytes().all(|b| b.is_ascii_digit()) {
        return Err(err());
    }
    let hours: i64 = hh.parse().map_err(|_| err())?;
    if hours > 23 {
        return Err(err());
    }

    let after_hours = &rest[2..];
    if after_hours.is_empty() {
        // ±HH only
        return Ok(sign * hours * 3_600_000_000_000);
    }

    // Determine separator style
    let has_colon = after_hours.starts_with(':');
    let mm_start = if has_colon { 1 } else { 0 };

    if after_hours.len() < mm_start + 2 {
        return Err(err());
    }

    let mm = &after_hours[mm_start..mm_start + 2];
    if !mm.bytes().all(|b| b.is_ascii_digit()) {
        return Err(err());
    }
    let minutes: i64 = mm.parse().map_err(|_| err())?;
    if minutes > 59 {
        return Err(err());
    }

    let after_minutes = &after_hours[mm_start + 2..];
    if after_minutes.is_empty() {
        // ±HH:MM or ±HHMM
        return Ok(sign * (hours * 3600 + minutes * 60) * 1_000_000_000);
    }

    // Seconds: must match separator style
    let has_sec_colon = after_minutes.starts_with(':');
    if has_colon != has_sec_colon {
        return Err(err()); // Separator style mismatch
    }
    let ss_start = if has_sec_colon { 1 } else { 0 };

    if after_minutes.len() < ss_start + 2 {
        return Err(err());
    }

    let ss = &after_minutes[ss_start..ss_start + 2];
    if !ss.bytes().all(|b| b.is_ascii_digit()) {
        return Err(err());
    }
    let seconds: i64 = ss.parse().map_err(|_| err())?;
    if seconds > 59 {
        return Err(err());
    }

    let after_seconds = &after_minutes[ss_start + 2..];
    if after_seconds.is_empty() {
        // ±HH:MM:SS or ±HHMMSS
        return Ok(sign * (hours * 3600 + minutes * 60 + seconds) * 1_000_000_000);
    }

    // Fractional seconds: must start with '.' and have 1-9 digits
    if !after_seconds.starts_with('.') {
        return Err(err());
    }
    let frac = &after_seconds[1..];
    if frac.is_empty() || frac.len() > 9 || !frac.bytes().all(|b| b.is_ascii_digit()) {
        return Err(err());
    }

    // Pad to 9 digits and parse
    let padded = format!("{:0<9}", frac);
    let frac_ns: i64 = padded.parse().map_err(|_| err())?;

    Ok(sign * ((hours * 3600 + minutes * 60 + seconds) * 1_000_000_000 + frac_ns))
}

/// Parse toString options for ZonedDateTime.
fn parse_zdt_to_string_options(
    ncx: &mut NativeContext<'_>,
    options_val: &Value,
) -> Result<
    (
        temporal_rs::options::DisplayOffset,
        temporal_rs::options::DisplayTimeZone,
        temporal_rs::options::DisplayCalendar,
        temporal_rs::options::ToStringRoundingOptions,
    ),
    VmError,
> {
    if options_val.is_undefined() {
        return Ok((
            temporal_rs::options::DisplayOffset::Auto,
            temporal_rs::options::DisplayTimeZone::Auto,
            temporal_rs::options::DisplayCalendar::Auto,
            temporal_rs::options::ToStringRoundingOptions::default(),
        ));
    }

    let _obj = get_options_object(options_val)?;

    let cal_val = get_option_value(ncx, options_val, "calendarName")?;
    let display_cal = if cal_val.is_undefined() {
        temporal_rs::options::DisplayCalendar::Auto
    } else {
        let s = ncx.to_string_value(&cal_val)?;
        s.as_str()
            .parse::<temporal_rs::options::DisplayCalendar>()
            .map_err(temporal_err)?
    };

    let fsd_val = get_option_value(ncx, options_val, "fractionalSecondDigits")?;
    let precision = if fsd_val.is_undefined() {
        temporal_rs::parsers::Precision::Auto
    } else if fsd_val.is_number() {
        // GetStringOrNumberOption: Number path
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
        // GetStringOrNumberOption: String path (call ToString for non-string/non-number)
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

    let off_val = get_option_value(ncx, options_val, "offset")?;
    let display_offset = if off_val.is_undefined() {
        temporal_rs::options::DisplayOffset::Auto
    } else {
        let s = ncx.to_string_value(&off_val)?;
        s.as_str()
            .parse::<temporal_rs::options::DisplayOffset>()
            .map_err(temporal_err)?
    };

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

    let su_val = get_option_value(ncx, options_val, "smallestUnit")?;
    let smallest_unit = if su_val.is_undefined() {
        None
    } else {
        let s = ncx.to_string_value(&su_val)?;
        Some(parse_temporal_unit(s.as_str())?)
    };

    let tzn_val = get_option_value(ncx, options_val, "timeZoneName")?;
    let display_tz = if tzn_val.is_undefined() {
        temporal_rs::options::DisplayTimeZone::Auto
    } else {
        let s = ncx.to_string_value(&tzn_val)?;
        s.as_str()
            .parse::<temporal_rs::options::DisplayTimeZone>()
            .map_err(temporal_err)?
    };

    Ok((
        display_offset,
        display_tz,
        display_cal,
        temporal_rs::options::ToStringRoundingOptions {
            precision,
            smallest_unit,
            rounding_mode,
        },
    ))
}

/// Parse difference options for until/since.
fn parse_zdt_difference_options(
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

/// Parse fields for ZonedDateTime.with().
fn parse_zdt_fields(
    ncx: &mut NativeContext<'_>,
    fields_val: &Value,
) -> Result<temporal_rs::fields::ZonedDateTimeFields, VmError> {
    let day_val = ncx.get_property_of_value(fields_val, &PropertyKey::string("day"))?;
    let day = if !day_val.is_undefined() {
        let d = to_integer_with_truncation(ncx, &day_val)?;
        if d < 1.0 {
            return Err(VmError::range_error("day must be positive"));
        }
        Some(d as u8)
    } else {
        None
    };

    let hour_val = ncx.get_property_of_value(fields_val, &PropertyKey::string("hour"))?;
    let hour = if !hour_val.is_undefined() {
        Some(to_integer_with_truncation(ncx, &hour_val)? as u8)
    } else {
        None
    };

    let microsecond_val =
        ncx.get_property_of_value(fields_val, &PropertyKey::string("microsecond"))?;
    let microsecond = if !microsecond_val.is_undefined() {
        Some(to_integer_with_truncation(ncx, &microsecond_val)? as u16)
    } else {
        None
    };

    let millisecond_val =
        ncx.get_property_of_value(fields_val, &PropertyKey::string("millisecond"))?;
    let millisecond = if !millisecond_val.is_undefined() {
        Some(to_integer_with_truncation(ncx, &millisecond_val)? as u16)
    } else {
        None
    };

    let minute_val = ncx.get_property_of_value(fields_val, &PropertyKey::string("minute"))?;
    let minute = if !minute_val.is_undefined() {
        Some(to_integer_with_truncation(ncx, &minute_val)? as u8)
    } else {
        None
    };

    let month_val = ncx.get_property_of_value(fields_val, &PropertyKey::string("month"))?;
    let month = if !month_val.is_undefined() {
        let m = to_integer_with_truncation(ncx, &month_val)?;
        if m < 1.0 {
            return Err(VmError::range_error("month must be positive"));
        }
        Some(m as u8)
    } else {
        None
    };

    let month_code_val =
        ncx.get_property_of_value(fields_val, &PropertyKey::string("monthCode"))?;
    let month_code = if !month_code_val.is_undefined() {
        let s = ncx.to_string_value(&month_code_val)?;
        Some(
            temporal_rs::MonthCode::try_from_utf8(s.as_str().as_bytes())
                .map_err(|e| VmError::range_error(format!("{e}")))?,
        )
    } else {
        None
    };

    let nanosecond_val =
        ncx.get_property_of_value(fields_val, &PropertyKey::string("nanosecond"))?;
    let nanosecond = if !nanosecond_val.is_undefined() {
        Some(to_integer_with_truncation(ncx, &nanosecond_val)? as u16)
    } else {
        None
    };

    let offset_val = ncx.get_property_of_value(fields_val, &PropertyKey::string("offset"))?;
    let offset = if !offset_val.is_undefined() {
        // Offset must be a string or object (ToString for objects)
        let s = if offset_val.is_string() {
            offset_val.as_string().unwrap().as_str().to_string()
        } else if offset_val.as_object().is_some() || offset_val.as_proxy().is_some() {
            ncx.to_string_value(&offset_val)?.as_str().to_string()
        } else {
            return Err(VmError::type_error("offset must be a string"));
        };
        let ns = parse_offset_string_to_ns(&s)?;
        let minutes = (ns / 60_000_000_000) as i16;
        Some(temporal_rs::UtcOffset::from_minutes(minutes))
    } else {
        None
    };

    let second_val = ncx.get_property_of_value(fields_val, &PropertyKey::string("second"))?;
    let second = if !second_val.is_undefined() {
        Some(to_integer_with_truncation(ncx, &second_val)? as u8)
    } else {
        None
    };

    let year_val = ncx.get_property_of_value(fields_val, &PropertyKey::string("year"))?;
    let year = if !year_val.is_undefined() {
        Some(to_integer_with_truncation(ncx, &year_val)? as i32)
    } else {
        None
    };

    let mut fields = temporal_rs::fields::ZonedDateTimeFields::new();
    fields.calendar_fields.year = year;
    fields.calendar_fields.month = month;
    fields.calendar_fields.month_code = month_code;
    fields.calendar_fields.day = day;
    fields.time.hour = hour;
    fields.time.minute = minute;
    fields.time.second = second;
    fields.time.millisecond = millisecond;
    fields.time.microsecond = microsecond;
    fields.time.nanosecond = nanosecond;
    fields.offset = offset;
    Ok(fields)
}

/// Parse rounding options for ZonedDateTime.round().
fn parse_zdt_rounding_options(
    ncx: &mut NativeContext<'_>,
    options_val: &Value,
) -> Result<temporal_rs::options::RoundingOptions, VmError> {
    if options_val.is_undefined() {
        return Err(VmError::type_error(
            "options parameter is required for round()",
        ));
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

/// Parse TransitionDirection for getTimeZoneTransition.
fn parse_transition_direction(
    ncx: &mut NativeContext<'_>,
    val: &Value,
) -> Result<temporal_rs::provider::TransitionDirection, VmError> {
    // undefined → TypeError (direction is required)
    if val.is_undefined() {
        return Err(VmError::type_error("direction is required"));
    }

    // String shorthand: "next" or "previous"
    if val.is_string() {
        let s = val.as_string().unwrap();
        return match s.as_str() {
            "next" => Ok(temporal_rs::provider::TransitionDirection::Next),
            "previous" => Ok(temporal_rs::provider::TransitionDirection::Previous),
            _ => Err(VmError::range_error(format!(
                "Invalid transition direction: {}",
                s
            ))),
        };
    }

    // Must be an object (GetOptionsObject)
    let _obj = get_options_object(val)?;

    // Read direction property
    let dir_val = get_option_value(ncx, val, "direction")?;
    if dir_val.is_undefined() {
        return Err(VmError::range_error("direction is required"));
    }
    let s = ncx.to_string_value(&dir_val)?;
    match s.as_str() {
        "next" => Ok(temporal_rs::provider::TransitionDirection::Next),
        "previous" => Ok(temporal_rs::provider::TransitionDirection::Previous),
        _ => Err(VmError::range_error(format!(
            "Invalid transition direction: {}",
            s
        ))),
    }
}
