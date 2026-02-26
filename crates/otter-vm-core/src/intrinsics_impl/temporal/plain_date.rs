use crate::context::NativeContext;
use crate::error::VmError;
use crate::gc::GcRef;
use crate::memory::MemoryManager;
use crate::object::{JsObject, PropertyAttributes, PropertyDescriptor, PropertyKey};
use crate::string::JsString;
use crate::value::Value;
use std::sync::Arc;

use super::common::*;

// ============================================================================
// PlainDate prototype methods
// ============================================================================

pub(super) fn install_plain_date_prototype(
    proto: GcRef<JsObject>,
    fn_proto: GcRef<JsObject>,
    mm: &Arc<MemoryManager>,
) {
    // year getter
    proto.define_property(
        PropertyKey::string("year"),
        PropertyDescriptor::Accessor {
            get: Some(Value::native_function_with_proto(
                |this, _args, _ncx| {
                    let obj = this
                        .as_object()
                        .ok_or_else(|| VmError::type_error("year called on non-object"))?;
                    let pd = extract_plain_date(&obj)?;
                    Ok(Value::int32(pd.year()))
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

    // month getter
    proto.define_property(
        PropertyKey::string("month"),
        PropertyDescriptor::Accessor {
            get: Some(Value::native_function_with_proto(
                |this, _args, _ncx| {
                    let obj = this
                        .as_object()
                        .ok_or_else(|| VmError::type_error("month called on non-object"))?;
                    let pd = extract_plain_date(&obj)?;
                    Ok(Value::int32(pd.month() as i32))
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

    // day getter
    proto.define_property(
        PropertyKey::string("day"),
        PropertyDescriptor::Accessor {
            get: Some(Value::native_function_with_proto(
                |this, _args, _ncx| {
                    let obj = this
                        .as_object()
                        .ok_or_else(|| VmError::type_error("day called on non-object"))?;
                    let pd = extract_plain_date(&obj)?;
                    Ok(Value::int32(pd.day() as i32))
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

    // monthCode getter
    proto.define_property(
        PropertyKey::string("monthCode"),
        PropertyDescriptor::Accessor {
            get: Some(Value::native_function_with_proto(
                |this, _args, _ncx| {
                    let obj = this
                        .as_object()
                        .ok_or_else(|| VmError::type_error("monthCode called on non-object"))?;
                    let pd = extract_plain_date(&obj)?;
                    Ok(Value::string(JsString::intern(&format_month_code(
                        pd.month() as u32,
                    ))))
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

    // calendarId getter
    proto.define_property(
        PropertyKey::string("calendarId"),
        PropertyDescriptor::Accessor {
            get: Some(Value::native_function_with_proto(
                |this, _args, _ncx| {
                    let obj = this
                        .as_object()
                        .ok_or_else(|| VmError::type_error("calendarId called on non-object"))?;
                    let pd = extract_plain_date(&obj)?;
                    Ok(Value::string(JsString::intern(pd.calendar().identifier())))
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

    // era getter — always undefined for ISO calendar
    proto.define_property(
        PropertyKey::string("era"),
        PropertyDescriptor::Accessor {
            get: Some(Value::native_function_with_proto(
                |this, _args, _ncx| {
                    let obj = this
                        .as_object()
                        .ok_or_else(|| VmError::type_error("era called on non-object"))?;
                    let _ = extract_plain_date(&obj)?;
                    Ok(Value::undefined())
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

    // eraYear getter — always undefined for ISO calendar
    proto.define_property(
        PropertyKey::string("eraYear"),
        PropertyDescriptor::Accessor {
            get: Some(Value::native_function_with_proto(
                |this, _args, _ncx| {
                    let obj = this
                        .as_object()
                        .ok_or_else(|| VmError::type_error("eraYear called on non-object"))?;
                    let _ = extract_plain_date(&obj)?;
                    Ok(Value::undefined())
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

    // dayOfWeek, dayOfYear, daysInMonth, daysInYear, monthsInYear, inLeapYear — via temporal_rs::PlainDate
    for (prop, getter_fn) in &[
        (
            "dayOfWeek",
            (|pd: &temporal_rs::PlainDate| pd.day_of_week() as i32)
                as fn(&temporal_rs::PlainDate) -> i32,
        ),
        ("dayOfYear", |pd: &temporal_rs::PlainDate| {
            pd.day_of_year() as i32
        }),
        ("daysInMonth", |pd: &temporal_rs::PlainDate| {
            pd.days_in_month() as i32
        }),
        ("daysInYear", |pd: &temporal_rs::PlainDate| {
            pd.days_in_year() as i32
        }),
        ("monthsInYear", |pd: &temporal_rs::PlainDate| {
            pd.months_in_year() as i32
        }),
    ] {
        let f = *getter_fn;
        proto.define_property(
            PropertyKey::string(prop),
            PropertyDescriptor::Accessor {
                get: Some(Value::native_function_with_proto(
                    move |this, _args, _ncx| {
                        let obj = this
                            .as_object()
                            .ok_or_else(|| VmError::type_error("getter called on non-object"))?;
                        let pd = extract_plain_date(&obj)?;
                        Ok(Value::int32(f(&pd)))
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

    proto.define_property(
        PropertyKey::string("inLeapYear"),
        PropertyDescriptor::Accessor {
            get: Some(Value::native_function_with_proto(
                |this, _args, _ncx| {
                    let obj = this
                        .as_object()
                        .ok_or_else(|| VmError::type_error("inLeapYear"))?;
                    let pd = extract_plain_date(&obj)?;
                    Ok(Value::boolean(pd.in_leap_year()))
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

    // weekOfYear getter
    proto.define_property(
        PropertyKey::string("weekOfYear"),
        PropertyDescriptor::Accessor {
            get: Some(Value::native_function_with_proto(
                |this, _args, _ncx| {
                    let obj = this
                        .as_object()
                        .ok_or_else(|| VmError::type_error("weekOfYear"))?;
                    let pd = extract_plain_date(&obj)?;
                    match pd.week_of_year() {
                        Some(w) => Ok(Value::int32(w as i32)),
                        None => Ok(Value::undefined()),
                    }
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

    // yearOfWeek getter
    proto.define_property(
        PropertyKey::string("yearOfWeek"),
        PropertyDescriptor::Accessor {
            get: Some(Value::native_function_with_proto(
                |this, _args, _ncx| {
                    let obj = this
                        .as_object()
                        .ok_or_else(|| VmError::type_error("yearOfWeek"))?;
                    let pd = extract_plain_date(&obj)?;
                    match pd.year_of_week() {
                        Some(y) => Ok(Value::int32(y)),
                        None => Ok(Value::undefined()),
                    }
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

    proto.define_property(
        PropertyKey::string("daysInWeek"),
        PropertyDescriptor::Accessor {
            get: Some(Value::native_function_with_proto(
                |this, _args, _ncx| {
                    let obj = this
                        .as_object()
                        .ok_or_else(|| VmError::type_error("daysInWeek"))?;
                    let _ = extract_plain_date(&obj)?;
                    Ok(Value::int32(7))
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

    // ========================================================================
    // .equals(other)
    // ========================================================================
    let equals_fn = Value::native_function_with_proto_named(
        |this, args, ncx| {
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("equals called on non-object"))?;
            let this_pd = extract_plain_date(&obj)?;
            let other = args.first().cloned().unwrap_or(Value::undefined());
            let other_pd = to_temporal_plain_date(ncx, &other, None)?;
            Ok(Value::boolean(
                this_pd.compare_iso(&other_pd) == std::cmp::Ordering::Equal,
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

    // ========================================================================
    // .add(duration, options)
    // ========================================================================
    let add_fn = Value::native_function_with_proto_named(
        |this, args, ncx| {
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("add called on non-object"))?;
            let this_pd = extract_plain_date(&obj)?;
            let dur_arg = args.first().cloned().unwrap_or(Value::undefined());
            let duration = to_temporal_duration(ncx, &dur_arg)?;
            let options_val = args.get(1).cloned().unwrap_or(Value::undefined());
            let overflow = parse_overflow_option(ncx, &options_val)?;
            let result = this_pd
                .add(&duration, Some(overflow))
                .map_err(temporal_err)?;
            construct_plain_date_value(
                ncx,
                result.year(),
                result.month() as i32,
                result.day() as i32,
            )
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
    // .subtract(duration, options)
    // ========================================================================
    let subtract_fn = Value::native_function_with_proto_named(
        |this, args, ncx| {
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("subtract called on non-object"))?;
            let this_pd = extract_plain_date(&obj)?;
            let dur_arg = args.first().cloned().unwrap_or(Value::undefined());
            let duration = to_temporal_duration(ncx, &dur_arg)?;
            let options_val = args.get(1).cloned().unwrap_or(Value::undefined());
            let overflow = parse_overflow_option(ncx, &options_val)?;
            let result = this_pd
                .subtract(&duration, Some(overflow))
                .map_err(temporal_err)?;
            construct_plain_date_value(
                ncx,
                result.year(),
                result.month() as i32,
                result.day() as i32,
            )
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
            let this_pd = extract_plain_date(&obj)?;
            let other_arg = args.first().cloned().unwrap_or(Value::undefined());
            let other_pd = to_temporal_plain_date(ncx, &other_arg, None)?;
            let options_val = args.get(1).cloned().unwrap_or(Value::undefined());
            let settings = parse_difference_settings_for_date(ncx, &options_val)?;
            let duration = this_pd.since(&other_pd, settings).map_err(temporal_err)?;
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
            let this_pd = extract_plain_date(&obj)?;
            let other_arg = args.first().cloned().unwrap_or(Value::undefined());
            let other_pd = to_temporal_plain_date(ncx, &other_arg, None)?;
            let options_val = args.get(1).cloned().unwrap_or(Value::undefined());
            let settings = parse_difference_settings_for_date(ncx, &options_val)?;
            let duration = this_pd.until(&other_pd, settings).map_err(temporal_err)?;
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
    // .with(temporalDateLike, options)
    // ========================================================================
    let with_fn = Value::native_function_with_proto_named(
        |this, args, ncx| {
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("with called on non-object"))?;
            let cur_pd = extract_plain_date(&obj)?;
            let cur_y = cur_pd.year();
            let cur_m = cur_pd.month() as i32;
            let cur_d = cur_pd.day() as i32;

            let item = args.first().cloned().unwrap_or(Value::undefined());
            if !item.as_object().is_some() && item.as_proxy().is_none() {
                return Err(VmError::type_error("with argument must be an object"));
            }

            // Helper: read property from object or proxy
            fn get_prop(
                ncx: &mut NativeContext<'_>,
                val: &Value,
                key: &str,
            ) -> Result<Value, VmError> {
                if let Some(obj) = val.as_object() {
                    ncx.get_property(&obj, &PropertyKey::string(key))
                } else if let Some(proxy) = val.as_proxy() {
                    proxy_get_property(ncx, proxy, val, key)
                } else {
                    Ok(Value::undefined())
                }
            }

            // Reject temporal types (PlainDate, PlainMonthDay etc.)
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

            // Read fields in alphabetical order (spec: PrepareTemporalFields)
            let calendar_val = get_prop(ncx, &item, "calendar")?;
            if !calendar_val.is_undefined() {
                return Err(VmError::type_error("calendar not allowed in with()"));
            }
            let time_zone_val = get_prop(ncx, &item, "timeZone")?;
            if !time_zone_val.is_undefined() {
                return Err(VmError::type_error("timeZone not allowed in with()"));
            }

            let day_raw = get_prop(ncx, &item, "day")?;
            let day = if !day_raw.is_undefined() {
                let n = ncx.to_number_value(&day_raw)?;
                if n.is_infinite() {
                    return Err(VmError::range_error("day property cannot be Infinity"));
                }
                n as i32
            } else {
                cur_d
            };

            let month_raw = get_prop(ncx, &item, "month")?;
            let month_n = if !month_raw.is_undefined() {
                let n = ncx.to_number_value(&month_raw)?;
                if n.is_infinite() {
                    return Err(VmError::range_error("month property cannot be Infinity"));
                }
                Some(n as i32)
            } else {
                None
            };

            let month_code_raw = get_prop(ncx, &item, "monthCode")?;
            let mc_str = if !month_code_raw.is_undefined() {
                let mc = ncx.to_string_value(&month_code_raw)?;
                validate_month_code_syntax(mc.as_str())?;
                Some(mc)
            } else {
                None
            };

            let year_raw = get_prop(ncx, &item, "year")?;
            let year = if !year_raw.is_undefined() {
                let n = ncx.to_number_value(&year_raw)?;
                if n.is_infinite() {
                    return Err(VmError::range_error("year property cannot be Infinity"));
                }
                n as i32
            } else {
                cur_y
            };

            // Check at least one recognized field
            let has_any = [&day_raw, &month_raw, &month_code_raw, &year_raw]
                .iter()
                .any(|v| !v.is_undefined());
            if !has_any {
                return Err(VmError::type_error(
                    "with argument must have at least one recognized temporal property",
                ));
            }

            // Validate minimum before options
            let month_pre = month_n.unwrap_or(cur_m);
            if month_pre < 1 {
                return Err(VmError::range_error(format!(
                    "month must be >= 1, got {}",
                    month_pre
                )));
            }
            if day < 1 {
                return Err(VmError::range_error(format!(
                    "day must be >= 1, got {}",
                    day
                )));
            }

            // Read options AFTER field reads
            let options_val = args.get(1).cloned().unwrap_or(Value::undefined());
            let overflow = parse_overflow_option(ncx, &options_val)?;

            // Resolve month from monthCode
            let month = if let Some(ref mc) = mc_str {
                let mc_month = validate_month_code_iso_suitability(mc.as_str())? as i32;
                if let Some(mn) = month_n {
                    if mn != mc_month {
                        return Err(VmError::range_error("month and monthCode must agree"));
                    }
                }
                mc_month
            } else {
                month_pre
            };

            let result = temporal_rs::PlainDate::new_with_overflow(
                year,
                month as u8,
                day as u8,
                temporal_rs::Calendar::default(),
                overflow,
            )
            .map_err(temporal_err)?;

            construct_plain_date_value(
                ncx,
                result.year(),
                result.month() as i32,
                result.day() as i32,
            )
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
    // .toPlainDateTime(plainTimeLike?)
    // ========================================================================
    let to_pdt_fn = Value::native_function_with_proto_named(
        |this, args, ncx| {
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("toPlainDateTime called on non-object"))?;
            let this_pd = extract_plain_date(&obj)?;

            let time_arg = args.first().cloned().unwrap_or(Value::undefined());
            if time_arg.is_undefined() {
                // No time argument — midnight
                let pdt = this_pd.to_plain_date_time(None).map_err(temporal_err)?;
                return construct_plain_date_time_value(ncx, &pdt);
            }

            let pt = to_temporal_plain_time(ncx, &time_arg)?;
            let pdt = this_pd.to_plain_date_time(Some(pt)).map_err(temporal_err)?;
            construct_plain_date_time_value(ncx, &pdt)
        },
        mm.clone(),
        fn_proto.clone(),
        "toPlainDateTime",
        0,
    );
    proto.define_property(
        PropertyKey::string("toPlainDateTime"),
        PropertyDescriptor::builtin_method(to_pdt_fn),
    );

    // ========================================================================
    // .toPlainYearMonth()
    // ========================================================================
    let to_pym_fn = Value::native_function_with_proto_named(
        |this, _args, ncx| {
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("toPlainYearMonth called on non-object"))?;
            let pd = extract_plain_date(&obj)?;
            let pym = pd.to_plain_year_month().map_err(temporal_err)?;
            construct_plain_year_month_value(ncx, pym.year(), pym.month() as i32)
        },
        mm.clone(),
        fn_proto.clone(),
        "toPlainYearMonth",
        0,
    );
    proto.define_property(
        PropertyKey::string("toPlainYearMonth"),
        PropertyDescriptor::builtin_method(to_pym_fn),
    );

    // ========================================================================
    // .toPlainMonthDay()
    // ========================================================================
    let to_pmd_fn = Value::native_function_with_proto_named(
        |this, _args, ncx| {
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("toPlainMonthDay called on non-object"))?;
            let pd = extract_plain_date(&obj)?;
            let pmd = pd.to_plain_month_day().map_err(temporal_err)?;
            let month = pmd.month_code().to_month_integer() as i32;
            construct_plain_month_day_value(ncx, month, pmd.day() as i32)
        },
        mm.clone(),
        fn_proto.clone(),
        "toPlainMonthDay",
        0,
    );
    proto.define_property(
        PropertyKey::string("toPlainMonthDay"),
        PropertyDescriptor::builtin_method(to_pmd_fn),
    );

    // ========================================================================
    // .withCalendar(calendar)
    // ========================================================================
    let with_cal_fn = Value::native_function_with_proto_named(
        |this, args, ncx| {
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("withCalendar called on non-object"))?;
            let pd = extract_plain_date(&obj)?;
            let cal_arg = args.first().cloned().unwrap_or(Value::undefined());
            if cal_arg.is_undefined() {
                return Err(VmError::type_error("missing calendar argument"));
            }
            resolve_calendar_from_property(ncx, &cal_arg)?;
            // ISO calendar only — return copy with same fields
            construct_plain_date_value(ncx, pd.year(), pd.month() as i32, pd.day() as i32)
        },
        mm.clone(),
        fn_proto.clone(),
        "withCalendar",
        1,
    );
    proto.define_property(
        PropertyKey::string("withCalendar"),
        PropertyDescriptor::builtin_method(with_cal_fn),
    );

    // ========================================================================
    // .getISOFields()
    // ========================================================================
    let get_iso_fn = Value::native_function_with_proto_named(
        |this, _args, ncx| {
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("getISOFields called on non-object"))?;
            let pd = extract_plain_date(&obj)?;
            let result = GcRef::new(JsObject::new(
                Value::undefined(),
                ncx.ctx.memory_manager().clone(),
            ));
            result.define_property(
                PropertyKey::string("calendar"),
                PropertyDescriptor::data(Value::string(JsString::intern("iso8601"))),
            );
            result.define_property(
                PropertyKey::string("isoDay"),
                PropertyDescriptor::data(Value::int32(pd.day() as i32)),
            );
            result.define_property(
                PropertyKey::string("isoMonth"),
                PropertyDescriptor::data(Value::int32(pd.month() as i32)),
            );
            result.define_property(
                PropertyKey::string("isoYear"),
                PropertyDescriptor::data(Value::int32(pd.year())),
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

    // toString()
    let to_string_fn = Value::native_function_with_proto_named(
        |this, args, ncx| {
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("toString on non-PlainDate"))?;
            let pd = extract_plain_date(&obj)?;

            // Check calendarName option
            let options_val = args.first().cloned().unwrap_or(Value::undefined());
            // Per spec: GetOptionsObject — undefined → empty obj, object/proxy → ok, else TypeError
            if !options_val.is_undefined() {
                if options_val.is_null()
                    || options_val.is_boolean()
                    || options_val.is_number()
                    || options_val.is_string()
                    || options_val.is_bigint()
                    || options_val.as_symbol().is_some()
                {
                    return Err(VmError::type_error(format!(
                        "options must be an object or undefined, got {}",
                        options_val.type_of()
                    )));
                }
            }
            let (show_calendar, is_critical) = if !options_val.is_undefined() {
                let cn =
                    ncx.get_property_of_value(&options_val, &PropertyKey::string("calendarName"))?;
                if !cn.is_undefined() {
                    let cn_str = ncx.to_string_value(&cn)?;
                    match cn_str.as_str() {
                        "auto" | "never" => (false, false),
                        "always" => (true, false),
                        "critical" => (true, true),
                        _ => {
                            return Err(VmError::range_error(format!(
                                "{} is not a valid calendar display option",
                                cn_str
                            )));
                        }
                    }
                } else {
                    (false, false)
                }
            } else {
                (false, false)
            };

            let date_str = format!(
                "{}-{:02}-{:02}",
                format_iso_year(pd.year()),
                pd.month(),
                pd.day()
            );
            if show_calendar {
                let prefix = if is_critical { "!" } else { "" };
                Ok(Value::string(JsString::intern(&format!(
                    "{}[{}u-ca=iso8601]",
                    date_str, prefix
                ))))
            } else {
                Ok(Value::string(JsString::intern(&date_str)))
            }
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
                .ok_or_else(|| VmError::type_error("toJSON"))?;
            let pd = extract_plain_date(&obj)?;
            Ok(Value::string(JsString::intern(&format!(
                "{}-{:02}-{:02}",
                format_iso_year(pd.year()),
                pd.month(),
                pd.day()
            ))))
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

    // valueOf() — always throws
    let value_of_fn = Value::native_function_with_proto_named(
        |_this, _args, _ncx| {
            Err(VmError::type_error(
                "use compare() or toString() to compare Temporal.PlainDate",
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

    // toLocaleString()
    let to_locale_string_fn = Value::native_function_with_proto_named(
        |this, _args, _ncx| {
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("toLocaleString"))?;
            let pd = extract_plain_date(&obj)?;
            Ok(Value::string(JsString::intern(&format!(
                "{}-{:02}-{:02}",
                format_iso_year(pd.year()),
                pd.month(),
                pd.day()
            ))))
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

    // toZonedDateTime(item)
    // Per spec: item is either a string (timezone ID) or an object with
    // { timeZone, plainTime? } properties.
    // Uses temporal_rs PlainDate::to_zoned_date_time(tz, Option<PlainTime>) directly.
    let to_zoned_fn = Value::native_function_with_proto_named(
        |this, args, ncx| {
            let obj = this
                .as_object()
                .ok_or_else(|| VmError::type_error("not a PlainDate"))?;
            let pd = extract_plain_date(&obj)?;

            let item = args.first().cloned().unwrap_or(Value::undefined());

            // Parse argument: string → timezone ID, object/proxy → { timeZone, plainTime? }
            let (tz, time) = if item.is_string() {
                let tz = to_temporal_timezone_identifier(ncx, &item)?;
                (tz, None)
            } else if item.as_object().is_some() || item.as_proxy().is_some() {
                // Read timeZone property (required) — supports Proxy via get_property_of_value
                let tz_val = ncx.get_property_of_value(&item, &PropertyKey::string("timeZone"))?;
                if tz_val.is_undefined() {
                    return Err(VmError::type_error("timeZone is required"));
                }
                let tz = to_temporal_timezone_identifier(ncx, &tz_val)?;

                // Read plainTime property (optional)
                let pt_val = ncx.get_property_of_value(&item, &PropertyKey::string("plainTime"))?;
                let time = if pt_val.is_undefined() {
                    None
                } else {
                    Some(to_temporal_plain_time(ncx, &pt_val)?)
                };
                (tz, time)
            } else {
                return Err(VmError::type_error(
                    "toZonedDateTime argument must be a string or object",
                ));
            };

            // Delegate to temporal_rs: PlainDate::to_zoned_date_time(tz, Option<PlainTime>)
            let zdt = pd.to_zoned_date_time(tz, time).map_err(temporal_err)?;

            // Construct JS ZonedDateTime value
            construct_zoned_date_time_value(ncx, &zdt)
        },
        mm.clone(),
        fn_proto.clone(),
        "toZonedDateTime",
        1,
    );
    proto.define_property(
        PropertyKey::string("toZonedDateTime"),
        PropertyDescriptor::builtin_method(to_zoned_fn),
    );

    // @@toStringTag
    proto.define_property(
        PropertyKey::Symbol(crate::intrinsics::well_known::to_string_tag_symbol()),
        PropertyDescriptor::data_with_attrs(
            Value::string(JsString::intern("Temporal.PlainDate")),
            PropertyAttributes {
                writable: false,
                enumerable: false,
                configurable: true,
            },
        ),
    );
}
