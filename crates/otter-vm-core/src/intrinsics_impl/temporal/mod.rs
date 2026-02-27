//! Temporal namespace initialization
//!
//! Creates the Temporal global namespace with constructors:
//! - Temporal.Now
//! - Temporal.Instant
//! - Temporal.PlainDate, PlainTime, PlainDateTime
//! - Temporal.PlainYearMonth, PlainMonthDay
//! - Temporal.ZonedDateTime
//! - Temporal.Duration

use crate::context::NativeContext;
use crate::error::VmError;
use crate::gc::GcRef;
use crate::memory::MemoryManager;
use crate::object::{JsObject, PropertyAttributes, PropertyDescriptor, PropertyKey};
use crate::string::JsString;
use crate::temporal_value::TemporalValue;
use crate::value::Value;
use chrono::{Datelike, Timelike};
use std::sync::Arc;
mod common;
use common::*;

mod plain_month_day;
use plain_month_day::*;

mod plain_date;
use plain_date::*;

mod plain_date_time;
use plain_date_time::*;

mod duration;
mod instant;
mod plain_time;
mod plain_year_month;
mod zoned_date_time;

// ============================================================================
// Install Temporal namespace
// ============================================================================

/// Create and install Temporal namespace on global object
pub fn install_temporal_namespace(global: GcRef<JsObject>, mm: &Arc<MemoryManager>) {
    let fn_proto_val = global
        .get(&PropertyKey::string("Function"))
        .and_then(|v| v.as_object())
        .and_then(|ctor| {
            ctor.get(&PropertyKey::string("prototype"))
                .and_then(|v| v.as_object())
        });

    let object_proto_val = global
        .get(&PropertyKey::string("Object"))
        .and_then(|v| v.as_object())
        .and_then(|ctor| {
            ctor.get(&PropertyKey::string("prototype"))
                .and_then(|v| v.as_object())
        });

    // Create main Temporal namespace object
    let temporal_obj = GcRef::new(JsObject::new(
        object_proto_val.map(Value::object).unwrap_or(Value::null()),
        mm.clone(),
    ));

    // Tag it
    temporal_obj.define_property(
        PropertyKey::Symbol(crate::intrinsics::well_known::to_string_tag_symbol()),
        PropertyDescriptor::data_with_attrs(
            Value::string(JsString::intern("Temporal")),
            PropertyAttributes {
                writable: false,
                enumerable: false,
                configurable: true,
            },
        ),
    );

    let fn_proto =
        fn_proto_val.unwrap_or_else(|| GcRef::new(JsObject::new(Value::null(), mm.clone())));
    let obj_proto =
        object_proto_val.unwrap_or_else(|| GcRef::new(JsObject::new(Value::null(), mm.clone())));

    // ====================================================================
    // Temporal.Now (namespace object, not a constructor)
    // ====================================================================
    let temporal_now = GcRef::new(JsObject::new(Value::object(obj_proto.clone()), mm.clone()));
    temporal_now.define_property(
        PropertyKey::Symbol(crate::intrinsics::well_known::to_string_tag_symbol()),
        PropertyDescriptor::data_with_attrs(
            Value::string(JsString::intern("Temporal.Now")),
            PropertyAttributes {
                writable: false,
                enumerable: false,
                configurable: true,
            },
        ),
    );
    temporal_obj.define_property(
        PropertyKey::string("Now"),
        PropertyDescriptor::data_with_attrs(
            Value::object(temporal_now.clone()),
            PropertyAttributes::builtin_method(),
        ),
    );

    // ====================================================================
    // Temporal.PlainMonthDay
    // ====================================================================
    let pmd_proto = GcRef::new(JsObject::new(Value::object(obj_proto.clone()), mm.clone()));

    install_plain_month_day_prototype(pmd_proto.clone(), fn_proto.clone(), mm);

    let pmd_ctor_obj = GcRef::new(JsObject::new(Value::object(fn_proto.clone()), mm.clone()));

    // Wire constructor.prototype
    pmd_ctor_obj.define_property(
        PropertyKey::string("prototype"),
        PropertyDescriptor::data_with_attrs(
            Value::object(pmd_proto.clone()),
            PropertyAttributes {
                writable: false,
                enumerable: false,
                configurable: false,
            },
        ),
    );

    let pmd_ctor_fn = create_plain_month_day_constructor(pmd_proto.clone());
    let pmd_ctor_value = Value::native_function_with_proto_and_object(
        Arc::from(pmd_ctor_fn),
        mm.clone(),
        fn_proto.clone(),
        pmd_ctor_obj.clone(),
    );

    // Wire prototype.constructor
    pmd_proto.define_property(
        PropertyKey::string("constructor"),
        PropertyDescriptor::data_with_attrs(
            pmd_ctor_value.clone(),
            PropertyAttributes::constructor_link(),
        ),
    );

    // Set name and length
    pmd_ctor_obj.define_property(
        PropertyKey::string("name"),
        PropertyDescriptor::function_length(Value::string(JsString::intern("PlainMonthDay"))),
    );
    pmd_ctor_obj.define_property(
        PropertyKey::string("length"),
        PropertyDescriptor::function_length(Value::number(2.0)),
    );

    // PlainMonthDay.from() static method
    let pmd_ctor_for_from = pmd_ctor_value.clone();
    let from_fn = Value::native_function_with_proto_named(
        move |this, args, ncx| plain_month_day_from(pmd_ctor_for_from.clone(), this, args, ncx),
        mm.clone(),
        fn_proto.clone(),
        "from",
        1,
    );
    // Remove __non_constructor tag is set by default in native_function_with_proto_named
    // That's fine — .from() is not a constructor

    pmd_ctor_obj.define_property(
        PropertyKey::string("from"),
        PropertyDescriptor::builtin_method(from_fn),
    );

    temporal_obj.define_property(
        PropertyKey::string("PlainMonthDay"),
        PropertyDescriptor::data_with_attrs(pmd_ctor_value, PropertyAttributes::builtin_method()),
    );

    // ====================================================================
    // Stub constructors for other Temporal types (plain objects for now)
    // ====================================================================
    // ====================================================================
    // Temporal.PlainDate
    // ====================================================================
    {
        let pd_proto = GcRef::new(JsObject::new(Value::object(obj_proto.clone()), mm.clone()));

        install_plain_date_prototype(pd_proto.clone(), fn_proto.clone(), mm);

        let pd_ctor_obj = GcRef::new(JsObject::new(Value::object(fn_proto.clone()), mm.clone()));
        pd_ctor_obj.define_property(
            PropertyKey::string("prototype"),
            PropertyDescriptor::data_with_attrs(
                Value::object(pd_proto.clone()),
                PropertyAttributes {
                    writable: false,
                    enumerable: false,
                    configurable: false,
                },
            ),
        );
        pd_ctor_obj.define_property(
            PropertyKey::string("name"),
            PropertyDescriptor::function_length(Value::string(JsString::intern("PlainDate"))),
        );
        pd_ctor_obj.define_property(
            PropertyKey::string("length"),
            PropertyDescriptor::function_length(Value::number(3.0)),
        );
        let pd_proto_for_ctor = pd_proto.clone();
        let pd_ctor_fn: Box<
            dyn Fn(&Value, &[Value], &mut NativeContext<'_>) -> Result<Value, VmError>
                + Send
                + Sync,
        > = Box::new(move |this, args, ncx| {
            // Step 1: If NewTarget is undefined, throw TypeError
            let is_new_target = if let Some(obj) = this.as_object() {
                obj.prototype()
                    .as_object()
                    .map_or(false, |p| p.as_ptr() == pd_proto_for_ctor.as_ptr())
            } else {
                false
            };
            if !is_new_target {
                return Err(VmError::type_error(
                    "Temporal.PlainDate constructor requires 'new'",
                ));
            }

            let year = to_integer_with_truncation(
                ncx,
                &args.first().cloned().unwrap_or(Value::undefined()),
            )? as i32;
            let month = to_integer_with_truncation(
                ncx,
                &args.get(1).cloned().unwrap_or(Value::undefined()),
            )? as i32;
            let day = to_integer_with_truncation(
                ncx,
                &args.get(2).cloned().unwrap_or(Value::undefined()),
            )? as i32;

            // Calendar validation (arg 3)
            let calendar_val = args.get(3).cloned().unwrap_or(Value::undefined());
            let calendar = if !calendar_val.is_undefined() {
                let cal_str = validate_calendar_arg_standalone(ncx, &calendar_val)?;
                cal_str
                    .parse::<temporal_rs::Calendar>()
                    .map_err(|_| VmError::range_error(format!("Unknown calendar: {}", cal_str)))?
            } else {
                temporal_rs::Calendar::default()
            };

            // Use temporal_rs for full validation (handles limits, leap years, etc.)
            // Constructor uses Reject mode — invalid dates (e.g., Feb 30) throw RangeError
            let pd = temporal_rs::PlainDate::try_new(year, month as u8, day as u8, calendar)
                .map_err(temporal_err)?;

            if let Some(obj) = this.as_object() {
                store_temporal_inner(&obj, TemporalValue::PlainDate(pd));
            }
            Ok(Value::undefined())
        });
        let pd_ctor_value = Value::native_function_with_proto_and_object(
            Arc::from(pd_ctor_fn),
            mm.clone(),
            fn_proto.clone(),
            pd_ctor_obj.clone(),
        );
        pd_proto.define_property(
            PropertyKey::string("constructor"),
            PropertyDescriptor::data_with_attrs(
                pd_ctor_value.clone(),
                PropertyAttributes::constructor_link(),
            ),
        );

        // PlainDate.from() static method
        let pd_from_fn = Value::native_function_with_proto_named(
            move |_this, args, ncx| {
                let item = args.first().cloned().unwrap_or(Value::undefined());
                let options_val = args.get(1).cloned().unwrap_or(Value::undefined());

                // Per spec: for string inputs, parse string first, then read overflow
                if item.is_string() {
                    let pd = to_temporal_plain_date(ncx, &item, None)?;
                    // Read overflow after parsing for observable side effects
                    let _ = parse_overflow_option(ncx, &options_val)?;
                    return construct_plain_date_value(
                        ncx,
                        pd.year(),
                        pd.month() as i32,
                        pd.day() as i32,
                    );
                }

                // Reject non-object types BEFORE reading options (per spec observable ordering)
                if item.as_object().is_none() && item.as_proxy().is_none() {
                    return Err(VmError::type_error(format!(
                        "cannot convert {} to a PlainDate",
                        item.type_of()
                    )));
                }

                // For temporal types: read overflow for observable side effects, then extract
                if let Some(obj) = item.as_object() {
                    let tt = obj
                        .get(&PropertyKey::string(SLOT_TEMPORAL_TYPE))
                        .and_then(|v| v.as_string().map(|s| s.as_str().to_string()));
                    if tt.as_deref() == Some("PlainDate")
                        || tt.as_deref() == Some("PlainDateTime")
                        || tt.as_deref() == Some("ZonedDateTime")
                    {
                        let _ = parse_overflow_option(ncx, &options_val)?;
                        let pd = to_temporal_plain_date(ncx, &item, None)?;
                        return construct_plain_date_value(
                            ncx,
                            pd.year(),
                            pd.month() as i32,
                            pd.day() as i32,
                        );
                    }
                }

                // For property bags (plain objects and proxies): overflow read after fields inside to_temporal_plain_date
                let pd = to_temporal_plain_date(ncx, &item, Some(&options_val))?;
                construct_plain_date_value(ncx, pd.year(), pd.month() as i32, pd.day() as i32)
            },
            mm.clone(),
            fn_proto.clone(),
            "from",
            1,
        );
        pd_ctor_obj.define_property(
            PropertyKey::string("from"),
            PropertyDescriptor::builtin_method(pd_from_fn),
        );

        // PlainDate.compare(one, two) — static method
        let pd_compare_fn = Value::native_function_with_proto_named(
            |_this, args, ncx| {
                let one_arg = args.first().cloned().unwrap_or(Value::undefined());
                let two_arg = args.get(1).cloned().unwrap_or(Value::undefined());
                let one = to_temporal_plain_date(ncx, &one_arg, None)?;
                let two = to_temporal_plain_date(ncx, &two_arg, None)?;
                match temporal_rs::PlainDate::compare_iso(&one, &two) {
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
        pd_ctor_obj.define_property(
            PropertyKey::string("compare"),
            PropertyDescriptor::builtin_method(pd_compare_fn),
        );

        temporal_obj.define_property(
            PropertyKey::string("PlainDate"),
            PropertyDescriptor::data_with_attrs(
                pd_ctor_value,
                PropertyAttributes::builtin_method(),
            ),
        );
    }

    // ====================================================================
    // Temporal.PlainDateTime
    // ====================================================================
    {
        let pdt_proto = GcRef::new(JsObject::new(Value::object(obj_proto.clone()), mm.clone()));
        install_plain_date_time_prototype(pdt_proto.clone(), fn_proto.clone(), mm);

        let pdt_ctor_obj = GcRef::new(JsObject::new(Value::object(fn_proto.clone()), mm.clone()));
        pdt_ctor_obj.define_property(
            PropertyKey::string("prototype"),
            PropertyDescriptor::data_with_attrs(
                Value::object(pdt_proto.clone()),
                PropertyAttributes {
                    writable: false,
                    enumerable: false,
                    configurable: false,
                },
            ),
        );
        pdt_ctor_obj.define_property(
            PropertyKey::string("name"),
            PropertyDescriptor::function_length(Value::string(JsString::intern("PlainDateTime"))),
        );
        pdt_ctor_obj.define_property(
            PropertyKey::string("length"),
            PropertyDescriptor::function_length(Value::number(3.0)),
        );

        let pdt_proto_for_ctor = pdt_proto.clone();
        let pdt_ctor_fn: Box<
            dyn Fn(&Value, &[Value], &mut NativeContext<'_>) -> Result<Value, VmError>
                + Send
                + Sync,
        > = Box::new(move |this, args, ncx| {
            // Step 1: If NewTarget is undefined, throw TypeError
            let is_new_target = if let Some(obj) = this.as_object() {
                obj.prototype()
                    .as_object()
                    .map_or(false, |p| p.as_ptr() == pdt_proto_for_ctor.as_ptr())
            } else {
                false
            };
            if !is_new_target {
                return Err(VmError::type_error(
                    "Temporal.PlainDateTime constructor requires 'new'",
                ));
            }

            // new Temporal.PlainDateTime(year, month, day [, hour, minute, second, ms, us, ns [, calendar]])
            let year = to_integer_with_truncation(
                ncx,
                &args.first().cloned().unwrap_or(Value::undefined()),
            )? as i32;
            let month = to_integer_with_truncation(
                ncx,
                &args.get(1).cloned().unwrap_or(Value::undefined()),
            )? as i32;
            let day = to_integer_with_truncation(
                ncx,
                &args.get(2).cloned().unwrap_or(Value::undefined()),
            )? as i32;

            // Time fields default to 0 (undefined counts as missing)
            let get_time_arg = |idx: usize, ncx: &mut NativeContext<'_>| -> Result<i32, VmError> {
                match args.get(idx) {
                    Some(v) if !v.is_undefined() => Ok(to_integer_with_truncation(ncx, v)? as i32),
                    _ => Ok(0),
                }
            };
            let hour = get_time_arg(3, ncx)?;
            let minute = get_time_arg(4, ncx)?;
            let second = get_time_arg(5, ncx)?;
            let ms = get_time_arg(6, ncx)?;
            let us = get_time_arg(7, ncx)?;
            let ns = get_time_arg(8, ncx)?;

            // Calendar validation (arg 9)
            let calendar_val = args.get(9).cloned().unwrap_or(Value::undefined());
            let calendar = if !calendar_val.is_undefined() {
                let cal_str = validate_calendar_arg_standalone(ncx, &calendar_val)?;
                cal_str
                    .parse::<temporal_rs::Calendar>()
                    .map_err(|_| VmError::range_error(format!("Unknown calendar: {}", cal_str)))?
            } else {
                temporal_rs::Calendar::default()
            };

            // Use temporal_rs for full validation (handles limits, leap years, etc.)
            let pdt = temporal_rs::PlainDateTime::try_new(
                year,
                month as u8,
                day as u8,
                hour as u8,
                minute as u8,
                second as u8,
                ms as u16,
                us as u16,
                ns as u16,
                calendar,
            )
            .map_err(temporal_err)?;

            if let Some(obj) = this.as_object() {
                store_temporal_inner(&obj, TemporalValue::PlainDateTime(pdt));
            }
            Ok(Value::undefined())
        });

        let pdt_ctor_value = Value::native_function_with_proto_and_object(
            Arc::from(pdt_ctor_fn),
            mm.clone(),
            fn_proto.clone(),
            pdt_ctor_obj.clone(),
        );

        pdt_proto.define_property(
            PropertyKey::string("constructor"),
            PropertyDescriptor::data_with_attrs(
                pdt_ctor_value.clone(),
                PropertyAttributes::constructor_link(),
            ),
        );

        // PlainDateTime.from()
        let pdt_ctor_for_from = pdt_ctor_value.clone();
        let pdt_from_fn = Value::native_function_with_proto_named(
            move |_this, args, ncx| {
                let item = args.first().cloned().unwrap_or(Value::undefined());
                let options_val = args.get(1).cloned().unwrap_or(Value::undefined());

                if item.is_string() {
                    let s = ncx.to_string_value(&item)?;
                    // Reject Z designator
                    reject_utc_designator_for_plain(s.as_str())?;
                    let (year, month, day, h, mi, sec, ms, us, ns) =
                        parse_iso_datetime_string(s.as_str())?;
                    // Read options (for observable get, but we don't use the value for string inputs)
                    let _ = parse_overflow_option(ncx, &options_val)?;
                    return ncx.call_function_construct(
                        &pdt_ctor_for_from,
                        Value::undefined(),
                        &[
                            Value::int32(year),
                            Value::int32(month as i32),
                            Value::int32(day as i32),
                            Value::int32(h),
                            Value::int32(mi),
                            Value::int32(sec),
                            Value::int32(ms),
                            Value::int32(us),
                            Value::int32(ns),
                        ],
                    );
                }

                // Property bag (object or proxy)
                let is_proxy = item.as_proxy().is_some();
                if item.as_object().is_some() || is_proxy {
                    // Check for temporal type (only for real objects, not proxies)
                    let temporal_type = if let Some(obj) = item.as_object() {
                        obj.get(&PropertyKey::string(SLOT_TEMPORAL_TYPE))
                            .and_then(|v| v.as_string().map(|s| s.as_str().to_string()))
                    } else {
                        None
                    };
                    let obj = item.as_object(); // may be None for proxy

                    if temporal_type.as_deref() == Some("PlainDateTime") {
                        let o = obj.as_ref().unwrap();
                        // Read options first (for observable get ordering)
                        let _ = parse_overflow_option(ncx, &options_val)?;
                        // Extract via temporal_rs TemporalValue
                        let pdt = extract_plain_date_time(o)?;
                        return construct_plain_date_time_value(ncx, &pdt);
                    }

                    if temporal_type.as_deref() == Some("PlainDate") {
                        let o = obj.as_ref().unwrap();
                        // PlainDate -> PlainDateTime with time 00:00:00
                        let _ = parse_overflow_option(ncx, &options_val)?;
                        let pd = extract_plain_date(o)?;
                        // PlainDate → PlainDateTime(date, midnight) via temporal_rs
                        let pdt = pd.to_plain_date_time(None).map_err(temporal_err)?;
                        return construct_plain_date_time_value(ncx, &pdt);
                    }

                    if temporal_type.as_deref() == Some("ZonedDateTime") {
                        let o = obj.as_ref().unwrap();
                        // ZonedDateTime → PlainDateTime via temporal_rs (handles tz offset correctly)
                        let _ = parse_overflow_option(ncx, &options_val)?;
                        let zdt = extract_zoned_date_time(o)?;
                        let pdt = zdt.to_plain_date_time();
                        return construct_plain_date_time_value(ncx, &pdt);
                    }

                    // Helper for observable property get (supports both object and proxy)
                    let get_field =
                        |ncx: &mut NativeContext<'_>, name: &str| -> Result<Value, VmError> {
                            if let Some(proxy) = item.as_proxy() {
                                let key = PropertyKey::string(name);
                                let key_value =
                                    crate::proxy_operations::property_key_to_value_pub(&key);
                                crate::proxy_operations::proxy_get(
                                    ncx,
                                    proxy,
                                    &key,
                                    key_value,
                                    item.clone(),
                                )
                            } else if let Some(obj) = item.as_object() {
                                ncx.get_property(&obj, &PropertyKey::string(name))
                            } else {
                                Ok(Value::undefined())
                            }
                        };

                    // Validate calendar property if present
                    let calendar_val = get_field(ncx, "calendar")?;
                    if !calendar_val.is_undefined() {
                        resolve_calendar_from_property(ncx, &calendar_val)?;
                    }

                    // PrepareTemporalFields — get + IMMEDIATELY convert each field (alphabetical order)
                    // This ensures valueOf/toString is called right after each get
                    let day_val = get_field(ncx, "day")?;
                    let d = if !day_val.is_undefined() {
                        let n = ncx.to_number_value(&day_val)?;
                        if n.is_infinite() {
                            return Err(VmError::range_error("day property cannot be Infinity"));
                        }
                        Some(n as i32)
                    } else {
                        None
                    };

                    let hour_val = get_field(ncx, "hour")?;
                    let h = if !hour_val.is_undefined() {
                        let n = ncx.to_number_value(&hour_val)?;
                        if n.is_infinite() {
                            return Err(VmError::range_error("hour property cannot be Infinity"));
                        }
                        n as i32
                    } else {
                        0
                    };

                    let microsecond_val = get_field(ncx, "microsecond")?;
                    let us = if !microsecond_val.is_undefined() {
                        let n = ncx.to_number_value(&microsecond_val)?;
                        if n.is_infinite() {
                            return Err(VmError::range_error(
                                "microsecond property cannot be Infinity",
                            ));
                        }
                        n as i32
                    } else {
                        0
                    };

                    let millisecond_val = get_field(ncx, "millisecond")?;
                    let ms = if !millisecond_val.is_undefined() {
                        let n = ncx.to_number_value(&millisecond_val)?;
                        if n.is_infinite() {
                            return Err(VmError::range_error(
                                "millisecond property cannot be Infinity",
                            ));
                        }
                        n as i32
                    } else {
                        0
                    };

                    let minute_val = get_field(ncx, "minute")?;
                    let mi = if !minute_val.is_undefined() {
                        let n = ncx.to_number_value(&minute_val)?;
                        if n.is_infinite() {
                            return Err(VmError::range_error("minute property cannot be Infinity"));
                        }
                        n as i32
                    } else {
                        0
                    };

                    let month_val = get_field(ncx, "month")?;
                    let month_num = if !month_val.is_undefined() {
                        let n = ncx.to_number_value(&month_val)?;
                        if n.is_infinite() {
                            return Err(VmError::range_error("month property cannot be Infinity"));
                        }
                        Some(n as i32)
                    } else {
                        None
                    };

                    let month_code_val = get_field(ncx, "monthCode")?;
                    let mc_str = if !month_code_val.is_undefined() {
                        // monthCode: ToPrimitive(value, string) then RequireString
                        if month_code_val.as_symbol().is_some() {
                            return Err(VmError::type_error(
                                "Cannot convert a Symbol value to a string",
                            ));
                        }
                        // ToPrimitive for objects calls toString/valueOf
                        let primitive = if month_code_val.as_object().is_some()
                            || month_code_val.as_proxy().is_some()
                        {
                            ncx.to_primitive(
                                &month_code_val,
                                crate::interpreter::PreferredType::String,
                            )?
                        } else {
                            month_code_val.clone()
                        };
                        // RequireString: result must be a String
                        if !primitive.is_string() {
                            return Err(VmError::type_error(format!(
                                "monthCode must be a string, got {}",
                                primitive.type_of()
                            )));
                        }
                        let mc = primitive.as_string().unwrap().as_str().to_string();
                        // Syntax validation happens at read time (before other field conversions)
                        validate_month_code_syntax(&mc)?;
                        Some(mc)
                    } else {
                        None
                    };

                    let nanosecond_val = get_field(ncx, "nanosecond")?;
                    let ns = if !nanosecond_val.is_undefined() {
                        let n = ncx.to_number_value(&nanosecond_val)?;
                        if n.is_infinite() {
                            return Err(VmError::range_error(
                                "nanosecond property cannot be Infinity",
                            ));
                        }
                        n as i32
                    } else {
                        0
                    };

                    let second_val = get_field(ncx, "second")?;
                    let s = if !second_val.is_undefined() {
                        let n = ncx.to_number_value(&second_val)?;
                        if n.is_infinite() {
                            return Err(VmError::range_error("second property cannot be Infinity"));
                        }
                        n as i32
                    } else {
                        0
                    };

                    let year_val = get_field(ncx, "year")?;
                    let y = if !year_val.is_undefined() {
                        let n = ncx.to_number_value(&year_val)?;
                        if n.is_infinite() {
                            return Err(VmError::range_error("year property cannot be Infinity"));
                        }
                        Some(n as i32)
                    } else {
                        None
                    };

                    // Read options — overflow read comes AFTER field gets per spec
                    let overflow = parse_overflow_option(ncx, &options_val)?;

                    // CalendarResolveFields: check required fields FIRST (TypeError)
                    // before monthCode suitability validation (RangeError)
                    let y = y.ok_or_else(|| VmError::type_error("year is required"))?;
                    if mc_str.is_none() && month_num.is_none() {
                        return Err(VmError::type_error("month or monthCode is required"));
                    }
                    let d = d.ok_or_else(|| VmError::type_error("day is required"))?;

                    // Resolve month from monthCode and/or month
                    // (syntax already validated at read time; suitability validated here)
                    let m = if let Some(ref mc) = mc_str {
                        let mc_month = validate_month_code_iso_suitability(mc.as_str())? as i32;
                        if let Some(m_int) = month_num {
                            if m_int != mc_month {
                                return Err(VmError::range_error("month and monthCode must agree"));
                            }
                        }
                        mc_month
                    } else {
                        month_num.unwrap() // safe: checked above
                    };

                    // Use temporal_rs for validation with overflow
                    let ov = overflow;
                    if m < 0 || m > 255 {
                        return Err(VmError::range_error(format!("month out of range: {}", m)));
                    }
                    if d < 0 || d > 255 {
                        return Err(VmError::range_error(format!("day out of range: {}", d)));
                    }
                    let pdt = temporal_rs::PlainDateTime::new_with_overflow(
                        y,
                        m as u8,
                        d as u8,
                        h.clamp(0, 255) as u8,
                        mi.clamp(0, 255) as u8,
                        s.clamp(0, 255) as u8,
                        ms.clamp(0, 65535) as u16,
                        us.clamp(0, 65535) as u16,
                        ns.clamp(0, 65535) as u16,
                        temporal_rs::Calendar::default(),
                        ov,
                    )
                    .map_err(temporal_err)?;

                    return ncx.call_function_construct(
                        &pdt_ctor_for_from,
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
                    );
                }

                Err(VmError::type_error("PlainDateTime.from: invalid argument"))
            },
            mm.clone(),
            fn_proto.clone(),
            "from",
            1,
        );
        pdt_ctor_obj.define_property(
            PropertyKey::string("from"),
            PropertyDescriptor::builtin_method(pdt_from_fn),
        );

        // PlainDateTime.compare(one, two) — static method
        let pdt_compare_fn = Value::native_function_with_proto_named(
            |_this, args, ncx| {
                let one_arg = args.first().cloned().unwrap_or(Value::undefined());
                let two_arg = args.get(1).cloned().unwrap_or(Value::undefined());
                let one = to_temporal_datetime_standalone(ncx, &one_arg)?;
                let two = to_temporal_datetime_standalone(ncx, &two_arg)?;
                match temporal_rs::PlainDateTime::compare_iso(&one, &two) {
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
        pdt_ctor_obj.define_property(
            PropertyKey::string("compare"),
            PropertyDescriptor::builtin_method(pdt_compare_fn),
        );

        temporal_obj.define_property(
            PropertyKey::string("PlainDateTime"),
            PropertyDescriptor::data_with_attrs(
                pdt_ctor_value,
                PropertyAttributes::builtin_method(),
            ),
        );
    }

    // Install Temporal.Now methods (after all constructors are defined)
    {
        // Temporal.Now.plainDateTimeISO([timeZone])
        let pdt_iso_fn = Value::native_function_with_proto_named(
            |_this, args, ncx| {
                // Validate timezone argument (if provided)
                let tz_arg = args.first().cloned().unwrap_or(Value::undefined());
                if !tz_arg.is_undefined() {
                    let _ = to_temporal_timezone_identifier(ncx, &tz_arg)?;
                }
                let now = chrono::Local::now();
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
                        Value::int32(now.year()),
                        Value::int32(now.month() as i32),
                        Value::int32(now.day() as i32),
                        Value::int32(now.hour() as i32),
                        Value::int32(now.minute() as i32),
                        Value::int32(now.second() as i32),
                        Value::int32((now.nanosecond() / 1_000_000) as i32),
                        Value::int32(((now.nanosecond() % 1_000_000) / 1000) as i32),
                        Value::int32((now.nanosecond() % 1000) as i32),
                    ],
                )
            },
            mm.clone(),
            fn_proto.clone(),
            "plainDateTimeISO",
            0,
        );
        temporal_now.define_property(
            PropertyKey::string("plainDateTimeISO"),
            PropertyDescriptor::data_with_attrs(pdt_iso_fn, PropertyAttributes::builtin_method()),
        );

        // Temporal.Now.plainDateISO([timeZone])
        let pd_iso_fn = Value::native_function_with_proto_named(
            |_this, args, ncx| {
                let tz_arg = args.first().cloned().unwrap_or(Value::undefined());
                if !tz_arg.is_undefined() {
                    let _ = to_temporal_timezone_identifier(ncx, &tz_arg)?;
                }
                let now = chrono::Local::now();
                let temporal_ns = ncx
                    .ctx
                    .get_global("Temporal")
                    .ok_or_else(|| VmError::type_error("Temporal namespace not found"))?;
                let temporal_obj = temporal_ns
                    .as_object()
                    .ok_or_else(|| VmError::type_error("Temporal namespace not found"))?;
                let ctor = temporal_obj
                    .get(&PropertyKey::string("PlainDate"))
                    .ok_or_else(|| VmError::type_error("PlainDate constructor not found"))?;
                ncx.call_function_construct(
                    &ctor,
                    Value::undefined(),
                    &[
                        Value::int32(now.year()),
                        Value::int32(now.month() as i32),
                        Value::int32(now.day() as i32),
                    ],
                )
            },
            mm.clone(),
            fn_proto.clone(),
            "plainDateISO",
            0,
        );
        temporal_now.define_property(
            PropertyKey::string("plainDateISO"),
            PropertyDescriptor::data_with_attrs(pd_iso_fn, PropertyAttributes::builtin_method()),
        );

        // Temporal.Now.plainTimeISO([timeZone])
        let pt_iso_fn = Value::native_function_with_proto_named(
            |_this, args, ncx| {
                let tz_arg = args.first().cloned().unwrap_or(Value::undefined());
                if !tz_arg.is_undefined() {
                    let _ = to_temporal_timezone_identifier(ncx, &tz_arg)?;
                }
                let now = chrono::Local::now();
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
                        Value::int32(now.hour() as i32),
                        Value::int32(now.minute() as i32),
                        Value::int32(now.second() as i32),
                        Value::int32((now.nanosecond() / 1_000_000) as i32),
                        Value::int32(((now.nanosecond() % 1_000_000) / 1000) as i32),
                        Value::int32((now.nanosecond() % 1000) as i32),
                    ],
                )
            },
            mm.clone(),
            fn_proto.clone(),
            "plainTimeISO",
            0,
        );
        temporal_now.define_property(
            PropertyKey::string("plainTimeISO"),
            PropertyDescriptor::data_with_attrs(pt_iso_fn, PropertyAttributes::builtin_method()),
        );

        // Temporal.Now.instant() → Temporal.Instant
        let instant_fn = Value::native_function_with_proto_named(
            |_this, _args, ncx| {
                let now = std::time::SystemTime::now();
                let since_epoch = now
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_err(|_| VmError::range_error("System time before epoch"))?;
                let epoch_ns: i128 = since_epoch.as_nanos() as i128;
                // Validate epoch_ns range via Instant
                temporal_rs::Instant::try_new(epoch_ns)
                    .map_err(|e| VmError::range_error(format!("{e}")))?;
                // Construct Temporal.Instant
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
                ncx.call_function_construct(
                    &ctor,
                    Value::undefined(),
                    &[Value::bigint(epoch_ns.to_string())],
                )
            },
            mm.clone(),
            fn_proto.clone(),
            "instant",
            0,
        );
        temporal_now.define_property(
            PropertyKey::string("instant"),
            PropertyDescriptor::data_with_attrs(instant_fn, PropertyAttributes::builtin_method()),
        );

        // Temporal.Now.timeZoneId() → string
        let tz_id_fn = Value::native_function_with_proto_named(
            |_this, _args, _ncx| {
                // Use iana-time-zone crate for IANA timezone name.
                // Fall back to UTC offset format if we can't get the name.
                let tz_id = if let Ok(tz) = iana_time_zone::get_timezone() {
                    tz
                } else {
                    let offset = *chrono::Local::now().offset();
                    let total_secs = offset.local_minus_utc();
                    if total_secs == 0 {
                        "UTC".to_string()
                    } else {
                        let h = total_secs.abs() / 3600;
                        let m = (total_secs.abs() % 3600) / 60;
                        let sign = if total_secs >= 0 { "+" } else { "-" };
                        format!("{}{:02}:{:02}", sign, h, m)
                    }
                };
                Ok(Value::string(JsString::intern(&tz_id)))
            },
            mm.clone(),
            fn_proto.clone(),
            "timeZoneId",
            0,
        );
        temporal_now.define_property(
            PropertyKey::string("timeZoneId"),
            PropertyDescriptor::data_with_attrs(tz_id_fn, PropertyAttributes::builtin_method()),
        );

        // Temporal.Now.zonedDateTimeISO([timeZone]) → Temporal.ZonedDateTime
        let zdt_iso_fn = Value::native_function_with_proto_named(
            |_this, args, ncx| {
                // Get epoch nanoseconds
                let now = std::time::SystemTime::now();
                let since_epoch = now
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_err(|_| VmError::range_error("System time before epoch"))?;
                let epoch_ns: i128 = since_epoch.as_nanos() as i128;

                // Resolve timezone
                let tz_arg = args.first().cloned().unwrap_or(Value::undefined());
                let tz_id = if tz_arg.is_undefined() {
                    // System timezone
                    if let Ok(tz) = iana_time_zone::get_timezone() {
                        tz
                    } else {
                        "UTC".to_string()
                    }
                } else {
                    // Validate via ToTemporalTimeZoneIdentifier
                    let tz = to_temporal_timezone_identifier(ncx, &tz_arg)?;
                    // Get the string representation for the ZonedDateTime constructor
                    tz.identifier().map_err(temporal_err)?
                };

                // Construct ZonedDateTime(epochNanoseconds, timeZone)
                let temporal_ns = ncx
                    .ctx
                    .get_global("Temporal")
                    .ok_or_else(|| VmError::type_error("Temporal not found"))?;
                let temporal_obj = temporal_ns
                    .as_object()
                    .ok_or_else(|| VmError::type_error("Temporal not found"))?;
                let ctor = temporal_obj
                    .get(&PropertyKey::string("ZonedDateTime"))
                    .ok_or_else(|| VmError::type_error("ZonedDateTime not found"))?;
                ncx.call_function_construct(
                    &ctor,
                    Value::undefined(),
                    &[
                        Value::bigint(epoch_ns.to_string()),
                        Value::string(JsString::intern(&tz_id)),
                    ],
                )
            },
            mm.clone(),
            fn_proto.clone(),
            "zonedDateTimeISO",
            0,
        );
        temporal_now.define_property(
            PropertyKey::string("zonedDateTimeISO"),
            PropertyDescriptor::data_with_attrs(zdt_iso_fn, PropertyAttributes::builtin_method()),
        );
    }

    // Install remaining Temporal types from their individual modules
    instant::install_instant(&temporal_obj, &obj_proto, &fn_proto, &mm);
    plain_time::install_plain_time(&temporal_obj, &obj_proto, &fn_proto, &mm);
    plain_year_month::install_plain_year_month(&temporal_obj, &obj_proto, &fn_proto, &mm);
    zoned_date_time::install_zoned_date_time(&temporal_obj, &obj_proto, &fn_proto, &mm);
    duration::install_duration(&temporal_obj, &obj_proto, &fn_proto, &mm);

    // Install Temporal on global as non-enumerable
    global.define_property(
        PropertyKey::string("Temporal"),
        PropertyDescriptor::data_with_attrs(
            Value::object(temporal_obj),
            PropertyAttributes::builtin_method(),
        ),
    );
}
