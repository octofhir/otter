use crate::context::NativeContext;
use crate::error::VmError;
use crate::gc::GcRef;
use crate::memory::MemoryManager;
use crate::object::{JsObject, PropertyAttributes, PropertyDescriptor, PropertyKey};
use crate::string::JsString;
use crate::value::Value;
use std::sync::Arc;

use super::common::*;

/// Install PlainTime constructor and prototype onto `temporal_obj`.
pub(super) fn install_plain_time(
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
        PropertyDescriptor::function_length(Value::string(JsString::intern("PlainTime"))),
    );
    ctor_obj.define_property(
        PropertyKey::string("length"),
        PropertyDescriptor::function_length(Value::number(0.0)),
    );

    // Constructor: new Temporal.PlainTime(hour?, minute?, second?, ms?, us?, ns?)
    let pt_proto_check = proto.clone();
    let ctor_fn: Box<
        dyn Fn(&Value, &[Value], &mut NativeContext<'_>) -> Result<Value, VmError> + Send + Sync,
    > = Box::new(move |this, args, ncx| {
        // Step 1: If NewTarget is undefined, throw TypeError
        let is_new_target = if let Some(obj) = this.as_object() {
            obj.prototype()
                .as_object()
                .map_or(false, |p| p.as_ptr() == pt_proto_check.as_ptr())
        } else {
            false
        };
        if !is_new_target {
            return Err(VmError::type_error(
                "Temporal.PlainTime constructor requires 'new'",
            ));
        }
        let obj = this.as_object().unwrap();

        // Per spec: use ToIntegerWithTruncation — truncate fractions, reject NaN/Infinity/Symbol/BigInt
        let get_arg = |ncx: &mut NativeContext<'_>, idx: usize| -> Result<f64, VmError> {
            match args.get(idx) {
                None => Ok(0.0),
                Some(v) if v.is_undefined() => Ok(0.0),
                Some(v) => to_integer_with_truncation(ncx, v),
            }
        };

        let h = get_arg(ncx, 0)? as i32;
        let mi = get_arg(ncx, 1)? as i32;
        let sec = get_arg(ncx, 2)? as i32;
        let ms = get_arg(ncx, 3)? as i32;
        let us = get_arg(ncx, 4)? as i32;
        let ns = get_arg(ncx, 5)? as i32;

        // RejectTime: validate ranges (negative values would saturate to 0 with `as u8`)
        if h < 0
            || h > 23
            || mi < 0
            || mi > 59
            || sec < 0
            || sec > 59
            || ms < 0
            || ms > 999
            || us < 0
            || us > 999
            || ns < 0
            || ns > 999
        {
            return Err(VmError::range_error("time value out of range"));
        }

        // Constructor uses reject semantics — no clamping
        let pt = temporal_rs::PlainTime::try_new(
            h as u8, mi as u8, sec as u8, ms as u16, us as u16, ns as u16,
        )
        .map_err(super::common::temporal_err)?;
        store_temporal_inner(&obj, crate::temporal_value::TemporalValue::PlainTime(pt));

        Ok(Value::undefined())
    });

    let ctor_value = Value::native_function_with_proto_and_object(
        Arc::from(ctor_fn),
        mm.clone(),
        fn_proto.clone(),
        ctor_obj.clone(),
    );

    // Prototype accessor getters
    for (name, field_index) in &[
        ("hour", 0u8),
        ("minute", 1),
        ("second", 2),
        ("millisecond", 3),
        ("microsecond", 4),
        ("nanosecond", 5),
    ] {
        let field_idx = *field_index;
        let getter_name = *name;
        proto.define_property(
            PropertyKey::string(name),
            PropertyDescriptor::Accessor {
                get: Some(Value::native_function_with_proto(
                    move |this, _args, _ncx| {
                        let obj = this.as_object().ok_or_else(|| {
                            VmError::type_error(format!(
                                "Temporal.PlainTime.prototype.{} requires a PlainTime receiver",
                                getter_name
                            ))
                        })?;
                        let pt = extract_plain_time(&obj).map_err(|_| {
                            VmError::type_error(format!(
                                "Temporal.PlainTime.prototype.{} requires a PlainTime receiver",
                                getter_name
                            ))
                        })?;
                        let val = match field_idx {
                            0 => pt.hour() as i32,
                            1 => pt.minute() as i32,
                            2 => pt.second() as i32,
                            3 => pt.millisecond() as i32,
                            4 => pt.microsecond() as i32,
                            5 => pt.nanosecond() as i32,
                            _ => unreachable!(),
                        };
                        Ok(Value::int32(val))
                    },
                    mm.clone(),
                    fn_proto.clone(),
                )),
                set: None,
                attributes: PropertyAttributes {
                    writable: false,
                    enumerable: false,
                    configurable: true,
                },
            },
        );
    }

    // toString
    let to_string_fn = Value::native_function_with_proto_named(
        |this, args, ncx| {
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("toString"))?;
            let pt = extract_plain_time(&obj)?;
            let options_val = args.first().cloned().unwrap_or(Value::undefined());
            let opts = parse_to_string_rounding_options(ncx, &options_val)?;
            let s = pt.to_ixdtf_string(opts).map_err(temporal_err)?;
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

    // toJSON — always use default options (no rounding)
    let to_json_fn = Value::native_function_with_proto_named(
        |this, _args, _ncx| {
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("toJSON"))?;
            let pt = extract_plain_time(&obj)?;
            let s = pt
                .to_ixdtf_string(Default::default())
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

    // valueOf — always throw TypeError
    let value_of_fn = Value::native_function_with_proto_named(
        |_this, _args, _ncx| {
            Err(VmError::type_error(
                "Temporal.PlainTime cannot be converted to a primitive",
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

    // toLocaleString — delegate to toString, with branding check
    let to_locale_string_fn = Value::native_function_with_proto_named(
        |this, _args, ncx| {
            // Per spec: RequireInternalSlot — must be a PlainTime object
            let obj = this.as_object().ok_or_else(|| {
                VmError::type_error(
                    "Temporal.PlainTime.prototype.toLocaleString called on incompatible receiver",
                )
            })?;
            let temporal_type = obj
                .get(&PropertyKey::string(SLOT_TEMPORAL_TYPE))
                .and_then(|v| v.as_string().map(|s| s.as_str().to_string()));
            if temporal_type.as_deref() != Some("PlainTime") {
                return Err(VmError::type_error(
                    "Temporal.PlainTime.prototype.toLocaleString called on incompatible receiver",
                ));
            }
            if let Some(ts) = obj.get(&PropertyKey::string("toString")) {
                return ncx.call_function(&ts, this.clone(), &[]);
            }
            Err(VmError::type_error("toLocaleString"))
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

    // equals
    let equals_fn = Value::native_function_with_proto_named(
        |this, args, ncx| {
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("equals"))?;
            let pt_a = extract_plain_time(&obj)?;
            let other_val = args.first().cloned().unwrap_or(Value::undefined());
            let pt_b = to_temporal_plain_time(ncx, &other_val)?;
            Ok(Value::boolean(pt_a == pt_b))
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

    // @@toStringTag
    proto.define_property(
        PropertyKey::Symbol(crate::intrinsics::well_known::to_string_tag_symbol()),
        PropertyDescriptor::data_with_attrs(
            Value::string(JsString::intern("Temporal.PlainTime")),
            PropertyAttributes {
                writable: false,
                enumerable: false,
                configurable: true,
            },
        ),
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
    // .add(duration)
    // ========================================================================
    let add_fn = Value::native_function_with_proto_named(
        |this, args, ncx| {
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("add called on non-object"))?;
            let pt = extract_plain_time(&obj)?;
            let dur_arg = args.first().cloned().unwrap_or(Value::undefined());
            let duration = to_temporal_duration(ncx, &dur_arg)?;
            let result = pt.add(&duration).map_err(temporal_err)?;
            construct_plain_time_value(ncx, &result)
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

    // ========================================================================
    // .subtract(duration)
    // ========================================================================
    let subtract_fn = Value::native_function_with_proto_named(
        |this, args, ncx| {
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("subtract called on non-object"))?;
            let pt = extract_plain_time(&obj)?;
            let dur_arg = args.first().cloned().unwrap_or(Value::undefined());
            let duration = to_temporal_duration(ncx, &dur_arg)?;
            let result = pt.subtract(&duration).map_err(temporal_err)?;
            construct_plain_time_value(ncx, &result)
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

    // ========================================================================
    // .since(other, options)
    // ========================================================================
    let since_fn = Value::native_function_with_proto_named(
        |this, args, ncx| {
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("since called on non-object"))?;
            let pt = extract_plain_time(&obj)?;
            let other_arg = args.first().cloned().unwrap_or(Value::undefined());
            let other_pt = to_temporal_plain_time(ncx, &other_arg)?;
            let options_val = args.get(1).cloned().unwrap_or(Value::undefined());
            let settings = parse_difference_settings_for_time(ncx, &options_val)?;
            let duration = pt.since(&other_pt, settings).map_err(temporal_err)?;
            construct_duration_value(ncx, &duration)
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

    // ========================================================================
    // .until(other, options)
    // ========================================================================
    let until_fn = Value::native_function_with_proto_named(
        |this, args, ncx| {
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("until called on non-object"))?;
            let pt = extract_plain_time(&obj)?;
            let other_arg = args.first().cloned().unwrap_or(Value::undefined());
            let other_pt = to_temporal_plain_time(ncx, &other_arg)?;
            let options_val = args.get(1).cloned().unwrap_or(Value::undefined());
            let settings = parse_difference_settings_for_time(ncx, &options_val)?;
            let duration = pt.until(&other_pt, settings).map_err(temporal_err)?;
            construct_duration_value(ncx, &duration)
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

    // ========================================================================
    // .with(timeLike, options)
    // ========================================================================
    let with_fn = Value::native_function_with_proto_named(
        |this, args, ncx| {
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("with called on non-object"))?;
            let pt = extract_plain_time(&obj)?;
            let item = args.first().cloned().unwrap_or(Value::undefined());
            if !item.as_object().is_some() && item.as_proxy().is_none() {
                return Err(VmError::type_error("with argument must be an object"));
            }
            // Reject Temporal types
            if let Some(arg_obj) = item.as_object() {
                let tt = arg_obj
                    .get(&PropertyKey::string(SLOT_TEMPORAL_TYPE))
                    .and_then(|v| v.as_string().map(|s| s.as_str().to_string()));
                if tt.is_some() {
                    return Err(VmError::type_error(
                        "with argument must be a plain object, not a Temporal type",
                    ));
                }
            }
            // Reject calendar and timeZone properties
            let calendar_val =
                ncx.get_property_of_value(&item, &PropertyKey::string("calendar"))?;
            if !calendar_val.is_undefined() {
                return Err(VmError::type_error("calendar not allowed in with()"));
            }
            let tz_val = ncx.get_property_of_value(&item, &PropertyKey::string("timeZone"))?;
            if !tz_val.is_undefined() {
                return Err(VmError::type_error("timeZone not allowed in with()"));
            }
            // Read time fields in ALPHABETICAL order with interleaved coercion (per spec):
            // hour, microsecond, millisecond, minute, nanosecond, second
            let mut has_any = false;
            let mut read_and_coerce =
                |ncx: &mut NativeContext<'_>, name: &str| -> Result<Option<f64>, VmError> {
                    let v = ncx.get_property_of_value(&item, &PropertyKey::string(name))?;
                    if v.is_undefined() {
                        return Ok(None);
                    }
                    has_any = true;
                    Ok(Some(to_integer_with_truncation(ncx, &v)?))
                };
            let h = read_and_coerce(ncx, "hour")?;
            let us = read_and_coerce(ncx, "microsecond")?;
            let ms = read_and_coerce(ncx, "millisecond")?;
            let mi = read_and_coerce(ncx, "minute")?;
            let ns = read_and_coerce(ncx, "nanosecond")?;
            let sec = read_and_coerce(ncx, "second")?;
            if !has_any {
                return Err(VmError::type_error(
                    "with argument must have at least one time property",
                ));
            }
            // Read overflow option AFTER field coercion (per spec ordering)
            let options_val = args.get(1).cloned().unwrap_or(Value::undefined());
            let overflow = parse_overflow_option(ncx, &options_val)?;
            // In reject mode, validate ranges explicitly (PartialTime uses u8/u16 and can't represent negatives)
            if overflow == temporal_rs::options::Overflow::Reject {
                let check_range =
                    |v: Option<f64>, name: &str, min: f64, max: f64| -> Result<(), VmError> {
                        if let Some(n) = v {
                            if n < min || n > max {
                                return Err(VmError::range_error(format!(
                                    "{} value {} out of range",
                                    name, n
                                )));
                            }
                        }
                        Ok(())
                    };
                check_range(h, "hour", 0.0, 23.0)?;
                check_range(mi, "minute", 0.0, 59.0)?;
                check_range(sec, "second", 0.0, 59.0)?;
                check_range(ms, "millisecond", 0.0, 999.0)?;
                check_range(us, "microsecond", 0.0, 999.0)?;
                check_range(ns, "nanosecond", 0.0, 999.0)?;
            }
            let partial = temporal_rs::partial::PartialTime::new()
                .with_hour(h.map(|n| n.clamp(0.0, 255.0) as u8))
                .with_minute(mi.map(|n| n.clamp(0.0, 255.0) as u8))
                .with_second(sec.map(|n| n.clamp(0.0, 255.0) as u8))
                .with_millisecond(ms.map(|n| n.clamp(0.0, 65535.0) as u16))
                .with_microsecond(us.map(|n| n.clamp(0.0, 65535.0) as u16))
                .with_nanosecond(ns.map(|n| n.clamp(0.0, 65535.0) as u16));
            let result = pt.with(partial, Some(overflow)).map_err(temporal_err)?;
            construct_plain_time_value(ncx, &result)
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

    // ========================================================================
    // .round(options)
    // ========================================================================
    let round_fn = Value::native_function_with_proto_named(
        |this, args, ncx| {
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("round called on non-object"))?;
            let pt = extract_plain_time(&obj)?;
            let options_val = args.first().cloned().unwrap_or(Value::undefined());
            let opts = parse_rounding_options_for_time(ncx, &options_val)?;
            let result = pt.round(opts).map_err(temporal_err)?;
            construct_plain_time_value(ncx, &result)
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

    // ========================================================================
    // .toPlainDateTime(dateLike)
    // ========================================================================
    let to_pdt_fn = Value::native_function_with_proto_named(
        |this, args, ncx| {
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("toPlainDateTime called on non-object"))?;
            let pt = extract_plain_time(&obj)?;
            let date_arg = args.first().cloned().unwrap_or(Value::undefined());
            let pd = to_temporal_plain_date(ncx, &date_arg, None)?;
            let pdt = pd.to_plain_date_time(Some(pt)).map_err(temporal_err)?;
            construct_plain_date_time_value(ncx, &pdt)
        },
        mm.clone(),
        fn_proto.clone(),
        "toPlainDateTime",
        1,
    );
    proto.define_property(
        PropertyKey::string("toPlainDateTime"),
        PropertyDescriptor::builtin_method(to_pdt_fn),
    );

    // ========================================================================
    // .getISOFields()
    // ========================================================================
    let get_iso_fn = Value::native_function_with_proto_named(
        |this, _args, _ncx| {
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("getISOFields called on non-object"))?;
            let pt = extract_plain_time(&obj)?;
            let mm = obj.memory_manager();
            let obj_proto_val = obj.prototype();
            // Get Object.prototype (grandparent of PlainTime prototype)
            let base_proto = if let Some(proto) = obj_proto_val.as_object() {
                proto.prototype()
            } else {
                Value::null()
            };
            let result = GcRef::new(JsObject::new(base_proto, mm.clone()));
            result.define_property(
                PropertyKey::string("isoHour"),
                PropertyDescriptor::builtin_data(Value::int32(pt.hour() as i32)),
            );
            result.define_property(
                PropertyKey::string("isoMicrosecond"),
                PropertyDescriptor::builtin_data(Value::int32(pt.microsecond() as i32)),
            );
            result.define_property(
                PropertyKey::string("isoMillisecond"),
                PropertyDescriptor::builtin_data(Value::int32(pt.millisecond() as i32)),
            );
            result.define_property(
                PropertyKey::string("isoMinute"),
                PropertyDescriptor::builtin_data(Value::int32(pt.minute() as i32)),
            );
            result.define_property(
                PropertyKey::string("isoNanosecond"),
                PropertyDescriptor::builtin_data(Value::int32(pt.nanosecond() as i32)),
            );
            result.define_property(
                PropertyKey::string("isoSecond"),
                PropertyDescriptor::builtin_data(Value::int32(pt.second() as i32)),
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
        PropertyDescriptor::builtin_method(get_iso_fn),
    );

    // ========================================================================
    // PlainTime.compare(one, two) — static on constructor
    // ========================================================================
    let compare_fn = Value::native_function_with_proto_named(
        |_this, args, ncx| {
            let one_arg = args.first().cloned().unwrap_or(Value::undefined());
            let two_arg = args.get(1).cloned().unwrap_or(Value::undefined());
            let one = to_temporal_plain_time(ncx, &one_arg)?;
            let two = to_temporal_plain_time(ncx, &two_arg)?;
            let cmp = one.cmp(&two);
            Ok(Value::int32(match cmp {
                std::cmp::Ordering::Less => -1,
                std::cmp::Ordering::Equal => 0,
                std::cmp::Ordering::Greater => 1,
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

    // PlainTime.from(item, options?)
    let from_proto = proto.clone();
    let from_mm = mm.clone();
    let from_fn = Value::native_function_with_proto_named(
        move |_this, args, ncx| {
            let item = args.first().cloned().unwrap_or(Value::undefined());
            let options_val = args.get(1).cloned().unwrap_or(Value::undefined());
            let is_object_like = item.as_object().is_some() || item.as_proxy().is_some();

            // If already a PlainTime, PlainDateTime, or ZonedDateTime
            if let Some(obj) = item.as_object() {
                let tt = obj
                    .get(&PropertyKey::string(SLOT_TEMPORAL_TYPE))
                    .and_then(|v| v.as_string().map(|s| s.as_str().to_string()));
                if tt.as_deref() == Some("PlainTime") {
                    // Read overflow option for observable side effects, then discard
                    let _ = parse_overflow_option(ncx, &options_val)?;
                    let pt = extract_plain_time(&obj)?;
                    let result = GcRef::new(JsObject::new(
                        Value::object(from_proto.clone()),
                        from_mm.clone(),
                    ));
                    store_temporal_inner(
                        &result,
                        crate::temporal_value::TemporalValue::PlainTime(pt),
                    );
                    return Ok(Value::object(result));
                }

                // PlainDateTime → extract time
                if tt.as_deref() == Some("PlainDateTime") {
                    let _ = parse_overflow_option(ncx, &options_val)?;
                    let pdt = extract_plain_date_time(&obj)?;
                    let pt = temporal_rs::PlainTime::from(pdt);
                    let result = GcRef::new(JsObject::new(
                        Value::object(from_proto.clone()),
                        from_mm.clone(),
                    ));
                    store_temporal_inner(
                        &result,
                        crate::temporal_value::TemporalValue::PlainTime(pt),
                    );
                    return Ok(Value::object(result));
                }

                // ZonedDateTime → extract PlainTime via temporal_rs
                if tt.as_deref() == Some("ZonedDateTime") {
                    let _ = parse_overflow_option(ncx, &options_val)?;
                    let zdt = extract_zoned_date_time(&obj)?;
                    let pt = zdt.to_plain_time();
                    let result = GcRef::new(JsObject::new(
                        Value::object(from_proto.clone()),
                        from_mm.clone(),
                    ));
                    store_temporal_inner(
                        &result,
                        crate::temporal_value::TemporalValue::PlainTime(pt),
                    );
                    return Ok(Value::object(result));
                }
            }

            // Property bag — handles both plain objects and proxies
            if is_object_like {
                // Per spec: Read fields FIRST (alphabetical order), then overflow option.
                // Each field: get property → ToIntegerWithTruncation (interleaved).
                let mut has_any = false;
                let mut get_field =
                    |ncx: &mut NativeContext<'_>, name: &str| -> Result<Option<f64>, VmError> {
                        let v = ncx.get_property_of_value(&item, &PropertyKey::string(name))?;
                        if v.is_undefined() {
                            return Ok(None);
                        }
                        has_any = true;
                        Ok(Some(to_integer_with_truncation(ncx, &v)?))
                    };
                // Alphabetical: hour, microsecond, millisecond, minute, nanosecond, second
                let h = get_field(ncx, "hour")?;
                let us = get_field(ncx, "microsecond")?;
                let ms = get_field(ncx, "millisecond")?;
                let mi = get_field(ncx, "minute")?;
                let ns = get_field(ncx, "nanosecond")?;
                let sec = get_field(ncx, "second")?;
                if !has_any {
                    return Err(VmError::type_error(
                        "property bag must have at least one time property",
                    ));
                }

                // Read overflow option AFTER field reads (per spec observable ordering)
                let overflow = parse_overflow_option(ncx, &options_val)?;

                // Use temporal_rs with overflow handling
                let partial = temporal_rs::partial::PartialTime::new()
                    .with_hour(h.map(|n| n as u8))
                    .with_minute(mi.map(|n| n as u8))
                    .with_second(sec.map(|n| n as u8))
                    .with_millisecond(ms.map(|n| n as u16))
                    .with_microsecond(us.map(|n| n as u16))
                    .with_nanosecond(ns.map(|n| n as u16));
                let pt = temporal_rs::PlainTime::from_partial(partial, Some(overflow))
                    .map_err(temporal_err)?;
                let result = GcRef::new(JsObject::new(
                    Value::object(from_proto.clone()),
                    from_mm.clone(),
                ));
                store_temporal_inner(&result, crate::temporal_value::TemporalValue::PlainTime(pt));
                return Ok(Value::object(result));
            }

            // String — delegate to temporal_rs for full spec-compliant parsing
            // Per spec: parse string first, then read (and discard) overflow option
            if item.is_string() {
                let s = ncx.to_string_value(&item)?;
                let pt = temporal_rs::PlainTime::from_utf8(s.as_str().as_bytes())
                    .map_err(temporal_err)?;
                // Read overflow after parsing for observable side effects, then discard
                let _ = parse_overflow_option(ncx, &options_val)?;
                return construct_plain_time_value(ncx, &pt);
            }

            Err(VmError::type_error("Cannot convert to PlainTime"))
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

    // Register on namespace
    temporal_obj.define_property(
        PropertyKey::string("PlainTime"),
        PropertyDescriptor::data_with_attrs(ctor_value, PropertyAttributes::builtin_method()),
    );
}
