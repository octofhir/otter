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
use crate::value::Value;
use chrono::{Datelike, Timelike};
use std::sync::Arc;
use temporal_rs::options::Overflow;

mod common;
use common::*;

mod plain_month_day;
use plain_month_day::*;

mod plain_date;
use plain_date::*;

mod plain_date_time;
use plain_date_time::*;

// ============================================================================
// Install Temporal namespace
// ============================================================================

/// Create and install Temporal namespace on global object
pub fn install_temporal_namespace(
    global: GcRef<JsObject>,
    mm: &Arc<MemoryManager>,
) {
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
        object_proto_val
            .map(Value::object)
            .unwrap_or(Value::null()),
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

    let fn_proto = fn_proto_val.unwrap_or_else(|| {
        GcRef::new(JsObject::new(Value::null(), mm.clone()))
    });
    let obj_proto = object_proto_val.unwrap_or_else(|| {
        GcRef::new(JsObject::new(Value::null(), mm.clone()))
    });

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
    let pmd_proto =
        GcRef::new(JsObject::new(Value::object(obj_proto.clone()), mm.clone()));

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
        PropertyDescriptor::data_with_attrs(
            pmd_ctor_value,
            PropertyAttributes::builtin_method(),
        ),
    );

    // ====================================================================
    // Stub constructors for other Temporal types (plain objects for now)
    // ====================================================================
    // ====================================================================
    // Temporal.PlainDate
    // ====================================================================
    {
        let pd_proto =
            GcRef::new(JsObject::new(Value::object(obj_proto.clone()), mm.clone()));

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
            dyn Fn(&Value, &[Value], &mut NativeContext<'_>) -> Result<Value, VmError> + Send + Sync,
        > = Box::new(move |this, args, ncx| {
            // Step 1: If NewTarget is undefined, throw TypeError
            let is_new_target = if let Some(obj) = this.as_object() {
                obj.prototype().as_object().map_or(false, |p| p.as_ptr() == pd_proto_for_ctor.as_ptr())
            } else {
                false
            };
            if !is_new_target {
                return Err(VmError::type_error("Temporal.PlainDate constructor requires 'new'"));
            }

            let year = to_integer_with_truncation(ncx, &args.first().cloned().unwrap_or(Value::undefined()))? as i32;
            let month = to_integer_with_truncation(ncx, &args.get(1).cloned().unwrap_or(Value::undefined()))? as i32;
            let day = to_integer_with_truncation(ncx, &args.get(2).cloned().unwrap_or(Value::undefined()))? as i32;

            // Calendar validation (arg 3)
            let calendar_val = args.get(3).cloned().unwrap_or(Value::undefined());
            if !calendar_val.is_undefined() {
                if calendar_val.is_null() || calendar_val.is_boolean() || calendar_val.is_number() || calendar_val.is_bigint() {
                    return Err(VmError::type_error(format!(
                        "{} is not a valid calendar",
                        if calendar_val.is_null() { "null".to_string() } else { calendar_val.type_of().to_string() }
                    )));
                }
                if calendar_val.as_symbol().is_some() {
                    return Err(VmError::type_error("Cannot convert a Symbol value to a string"));
                }
                let cal_str = ncx.to_string_value(&calendar_val)?;
                let lower = cal_str.as_str().to_ascii_lowercase();
                if lower != "iso8601" {
                    return Err(VmError::range_error(format!("Unknown calendar: {}", cal_str)));
                }
            }

            // Use temporal_rs for full validation (handles limits, leap years, etc.)
            if month < 1 || month > 12 { return Err(VmError::range_error(format!("month must be 1-12, got {}", month))); }
            if day < 1 || day > 31 { return Err(VmError::range_error(format!("day out of range: {}", day))); }
            let _validated = temporal_rs::PlainDate::try_new_iso(year, month as u8, day as u8)
                .map_err(temporal_err)?;

            if let Some(obj) = this.as_object() {
                obj.define_property(PropertyKey::string(SLOT_ISO_YEAR), PropertyDescriptor::builtin_data(Value::int32(year)));
                obj.define_property(PropertyKey::string(SLOT_ISO_MONTH), PropertyDescriptor::builtin_data(Value::int32(month)));
                obj.define_property(PropertyKey::string(SLOT_ISO_DAY), PropertyDescriptor::builtin_data(Value::int32(day)));
                obj.define_property(PropertyKey::string(SLOT_TEMPORAL_TYPE), PropertyDescriptor::builtin_data(Value::string(JsString::intern("PlainDate"))));
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

                // Parse overflow option
                let overflow = parse_overflow_option(ncx, &options_val)?;

                let pd = to_temporal_plain_date(ncx, &item, Some(overflow))?;
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
                PropertyAttributes { writable: false, enumerable: false, configurable: false },
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
            dyn Fn(&Value, &[Value], &mut NativeContext<'_>) -> Result<Value, VmError> + Send + Sync,
        > = Box::new(move |this, args, ncx| {
            // Step 1: If NewTarget is undefined, throw TypeError
            let is_new_target = if let Some(obj) = this.as_object() {
                obj.prototype().as_object().map_or(false, |p| p.as_ptr() == pdt_proto_for_ctor.as_ptr())
            } else {
                false
            };
            if !is_new_target {
                return Err(VmError::type_error("Temporal.PlainDateTime constructor requires 'new'"));
            }

            // new Temporal.PlainDateTime(year, month, day [, hour, minute, second, ms, us, ns [, calendar]])
            let year = to_integer_with_truncation(ncx, &args.first().cloned().unwrap_or(Value::undefined()))? as i32;
            let month = to_integer_with_truncation(ncx, &args.get(1).cloned().unwrap_or(Value::undefined()))? as i32;
            let day = to_integer_with_truncation(ncx, &args.get(2).cloned().unwrap_or(Value::undefined()))? as i32;

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
            if !calendar_val.is_undefined() {
                if calendar_val.is_null() || calendar_val.is_boolean() || calendar_val.is_number() || calendar_val.is_bigint() {
                    return Err(VmError::type_error(format!(
                        "{} is not a valid calendar",
                        if calendar_val.is_null() { "null".to_string() } else { calendar_val.type_of().to_string() }
                    )));
                }
                if calendar_val.as_symbol().is_some() {
                    return Err(VmError::type_error("Cannot convert a Symbol value to a string"));
                }
                let cal_str = ncx.to_string_value(&calendar_val)?;
                let lower = cal_str.as_str().to_ascii_lowercase();
                if lower != "iso8601" {
                    return Err(VmError::range_error(format!("Unknown calendar: {}", cal_str)));
                }
            }

            // Range-check before casting to narrower types
            if month < 1 || month > 12 { return Err(VmError::range_error(format!("month must be 1-12, got {}", month))); }
            if day < 1 || day > 31 { return Err(VmError::range_error(format!("day out of range: {}", day))); }
            if hour < 0 || hour > 23 { return Err(VmError::range_error(format!("hour must be 0-23, got {}", hour))); }
            if minute < 0 || minute > 59 { return Err(VmError::range_error(format!("minute must be 0-59, got {}", minute))); }
            if second < 0 || second > 59 { return Err(VmError::range_error(format!("second must be 0-59, got {}", second))); }
            if ms < 0 || ms > 999 { return Err(VmError::range_error(format!("millisecond must be 0-999, got {}", ms))); }
            if us < 0 || us > 999 { return Err(VmError::range_error(format!("microsecond must be 0-999, got {}", us))); }
            if ns < 0 || ns > 999 { return Err(VmError::range_error(format!("nanosecond must be 0-999, got {}", ns))); }

            // Use temporal_rs for full validation (handles limits, leap years, etc.)
            let _validated = temporal_rs::PlainDateTime::try_new_iso(
                year, month as u8, day as u8,
                hour as u8, minute as u8, second as u8,
                ms as u16, us as u16, ns as u16,
            ).map_err(temporal_err)?;

            if let Some(obj) = this.as_object() {
                obj.define_property(PropertyKey::string(SLOT_ISO_YEAR), PropertyDescriptor::builtin_data(Value::int32(year)));
                obj.define_property(PropertyKey::string(SLOT_ISO_MONTH), PropertyDescriptor::builtin_data(Value::int32(month)));
                obj.define_property(PropertyKey::string(SLOT_ISO_DAY), PropertyDescriptor::builtin_data(Value::int32(day)));
                obj.define_property(PropertyKey::string(SLOT_ISO_HOUR), PropertyDescriptor::builtin_data(Value::int32(hour)));
                obj.define_property(PropertyKey::string(SLOT_ISO_MINUTE), PropertyDescriptor::builtin_data(Value::int32(minute)));
                obj.define_property(PropertyKey::string(SLOT_ISO_SECOND), PropertyDescriptor::builtin_data(Value::int32(second)));
                obj.define_property(PropertyKey::string(SLOT_ISO_MILLISECOND), PropertyDescriptor::builtin_data(Value::int32(ms)));
                obj.define_property(PropertyKey::string(SLOT_ISO_MICROSECOND), PropertyDescriptor::builtin_data(Value::int32(us)));
                obj.define_property(PropertyKey::string(SLOT_ISO_NANOSECOND), PropertyDescriptor::builtin_data(Value::int32(ns)));
                obj.define_property(PropertyKey::string(SLOT_TEMPORAL_TYPE), PropertyDescriptor::builtin_data(Value::string(JsString::intern("PlainDateTime"))));
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
            PropertyDescriptor::data_with_attrs(pdt_ctor_value.clone(), PropertyAttributes::constructor_link()),
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
                    let (year, month, day, h, mi, sec, ms, us, ns) = parse_iso_datetime_string(s.as_str())?;
                    // Read options (for observable get, but we don't use the value for string inputs)
                    let _ = parse_overflow_option(ncx, &options_val)?;
                    return ncx.call_function_construct(
                        &pdt_ctor_for_from,
                        Value::undefined(),
                        &[
                            Value::int32(year), Value::int32(month as i32), Value::int32(day as i32),
                            Value::int32(h), Value::int32(mi), Value::int32(sec),
                            Value::int32(ms), Value::int32(us), Value::int32(ns),
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
                    } else { None };
                    let obj = item.as_object(); // may be None for proxy

                    if temporal_type.as_deref() == Some("PlainDateTime") {
                        let o = obj.as_ref().unwrap();
                        // Read options first (for observable get ordering)
                        let _ = parse_overflow_option(ncx, &options_val)?;
                        // Copy from existing PlainDateTime
                        let y = o.get(&PropertyKey::string(SLOT_ISO_YEAR)).and_then(|v| v.as_int32()).unwrap_or(0);
                        let mo = o.get(&PropertyKey::string(SLOT_ISO_MONTH)).and_then(|v| v.as_int32()).unwrap_or(1);
                        let d = o.get(&PropertyKey::string(SLOT_ISO_DAY)).and_then(|v| v.as_int32()).unwrap_or(1);
                        let h = o.get(&PropertyKey::string(SLOT_ISO_HOUR)).and_then(|v| v.as_int32()).unwrap_or(0);
                        let mi = o.get(&PropertyKey::string(SLOT_ISO_MINUTE)).and_then(|v| v.as_int32()).unwrap_or(0);
                        let s = o.get(&PropertyKey::string(SLOT_ISO_SECOND)).and_then(|v| v.as_int32()).unwrap_or(0);
                        let ms = o.get(&PropertyKey::string(SLOT_ISO_MILLISECOND)).and_then(|v| v.as_int32()).unwrap_or(0);
                        let us = o.get(&PropertyKey::string(SLOT_ISO_MICROSECOND)).and_then(|v| v.as_int32()).unwrap_or(0);
                        let ns = o.get(&PropertyKey::string(SLOT_ISO_NANOSECOND)).and_then(|v| v.as_int32()).unwrap_or(0);
                        return ncx.call_function_construct(
                            &pdt_ctor_for_from, Value::undefined(),
                            &[Value::int32(y), Value::int32(mo), Value::int32(d),
                              Value::int32(h), Value::int32(mi), Value::int32(s),
                              Value::int32(ms), Value::int32(us), Value::int32(ns)],
                        );
                    }

                    if temporal_type.as_deref() == Some("PlainDate") {
                        let o = obj.as_ref().unwrap();
                        // PlainDate -> PlainDateTime with time 00:00:00
                        let _ = parse_overflow_option(ncx, &options_val)?;
                        let y = o.get(&PropertyKey::string(SLOT_ISO_YEAR)).and_then(|v| v.as_int32()).unwrap_or(0);
                        let mo = o.get(&PropertyKey::string(SLOT_ISO_MONTH)).and_then(|v| v.as_int32()).unwrap_or(1);
                        let d = o.get(&PropertyKey::string(SLOT_ISO_DAY)).and_then(|v| v.as_int32()).unwrap_or(1);
                        return ncx.call_function_construct(
                            &pdt_ctor_for_from, Value::undefined(),
                            &[Value::int32(y), Value::int32(mo), Value::int32(d)],
                        );
                    }

                    if temporal_type.as_deref() == Some("ZonedDateTime") {
                        let o = obj.as_ref().unwrap();
                        // ZonedDateTime → PlainDateTime: apply timezone offset
                        let _ = parse_overflow_option(ncx, &options_val)?;
                        let epoch_ns_val = o.get(&PropertyKey::string("epochNanoseconds"))
                            .unwrap_or(Value::int32(0));
                        let tz_id_val = o.get(&PropertyKey::string("timeZoneId"))
                            .unwrap_or(Value::string(JsString::intern("UTC")));
                        let tz_id = if let Some(s) = tz_id_val.as_string() { s.as_str().to_string() } else { "UTC".to_string() };

                        // Parse epoch nanoseconds from BigInt or number
                        let epoch_ns: i128 = if epoch_ns_val.is_bigint() {
                            // BigInt: convert to string, then parse
                            let s = ncx.to_string_value(&epoch_ns_val)?;
                            // Remove trailing 'n' if present
                            let s = s.trim_end_matches('n');
                            s.parse::<i128>().unwrap_or(0)
                        } else if let Some(n) = epoch_ns_val.as_number() {
                            n as i128
                        } else { 0 };

                        // Compute offset nanoseconds from timezone
                        let offset_ns: i128 = parse_timezone_offset_ns(&tz_id);

                        // Apply offset to get wall-clock nanoseconds
                        let wall_ns = epoch_ns + offset_ns;

                        // GetISOPartsFromEpoch using Euclidean division for correct floor behavior
                        let ns_per_ms: i128 = 1_000_000;
                        let ms_per_s: i128 = 1_000;

                        let epoch_ms = wall_ns.div_euclid(ns_per_ms);
                        let remainder_ns = wall_ns.rem_euclid(ns_per_ms);
                        let us_part = (remainder_ns / 1000) as i32;
                        let ns_part = (remainder_ns % 1000) as i32;

                        let epoch_secs = epoch_ms.div_euclid(ms_per_s);
                        let ms_rem = epoch_ms.rem_euclid(ms_per_s) as i32;

                        let ndt = chrono::DateTime::from_timestamp(epoch_secs as i64, (ms_rem as u32) * 1_000_000)
                            .unwrap_or_else(|| chrono::DateTime::from_timestamp(0, 0).unwrap())
                            .naive_utc();

                        return ncx.call_function_construct(
                            &pdt_ctor_for_from, Value::undefined(),
                            &[
                                Value::int32(ndt.year()),
                                Value::int32(ndt.month() as i32),
                                Value::int32(ndt.day() as i32),
                                Value::int32(ndt.hour() as i32),
                                Value::int32(ndt.minute() as i32),
                                Value::int32(ndt.second() as i32),
                                Value::int32(ms_rem),
                                Value::int32(us_part),
                                Value::int32(ns_part),
                            ],
                        );
                    }

                    // Helper for observable property get (supports both object and proxy)
                    let get_field = |ncx: &mut NativeContext<'_>, name: &str| -> Result<Value, VmError> {
                        if let Some(proxy) = item.as_proxy() {
                            let key = PropertyKey::string(name);
                            let key_value = crate::proxy_operations::property_key_to_value_pub(&key);
                            crate::proxy_operations::proxy_get(ncx, proxy, &key, key_value, item.clone())
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
                        if n.is_infinite() { return Err(VmError::range_error("day property cannot be Infinity")); }
                        Some(n as i32)
                    } else { None };

                    let hour_val = get_field(ncx, "hour")?;
                    let h = if !hour_val.is_undefined() {
                        let n = ncx.to_number_value(&hour_val)?;
                        if n.is_infinite() { return Err(VmError::range_error("hour property cannot be Infinity")); }
                        n as i32
                    } else { 0 };

                    let microsecond_val = get_field(ncx, "microsecond")?;
                    let us = if !microsecond_val.is_undefined() {
                        let n = ncx.to_number_value(&microsecond_val)?;
                        if n.is_infinite() { return Err(VmError::range_error("microsecond property cannot be Infinity")); }
                        n as i32
                    } else { 0 };

                    let millisecond_val = get_field(ncx, "millisecond")?;
                    let ms = if !millisecond_val.is_undefined() {
                        let n = ncx.to_number_value(&millisecond_val)?;
                        if n.is_infinite() { return Err(VmError::range_error("millisecond property cannot be Infinity")); }
                        n as i32
                    } else { 0 };

                    let minute_val = get_field(ncx, "minute")?;
                    let mi = if !minute_val.is_undefined() {
                        let n = ncx.to_number_value(&minute_val)?;
                        if n.is_infinite() { return Err(VmError::range_error("minute property cannot be Infinity")); }
                        n as i32
                    } else { 0 };

                    let month_val = get_field(ncx, "month")?;
                    let month_num = if !month_val.is_undefined() {
                        let n = ncx.to_number_value(&month_val)?;
                        if n.is_infinite() { return Err(VmError::range_error("month property cannot be Infinity")); }
                        Some(n as i32)
                    } else { None };

                    let month_code_val = get_field(ncx, "monthCode")?;
                    let mc_str = if !month_code_val.is_undefined() {
                        // monthCode: ToPrimitive(value, string) then RequireString
                        if month_code_val.as_symbol().is_some() {
                            return Err(VmError::type_error("Cannot convert a Symbol value to a string"));
                        }
                        // ToPrimitive for objects calls toString/valueOf
                        let primitive = if month_code_val.as_object().is_some() || month_code_val.as_proxy().is_some() {
                            ncx.to_primitive(&month_code_val, crate::interpreter::PreferredType::String)?
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
                    } else { None };

                    let nanosecond_val = get_field(ncx, "nanosecond")?;
                    let ns = if !nanosecond_val.is_undefined() {
                        let n = ncx.to_number_value(&nanosecond_val)?;
                        if n.is_infinite() { return Err(VmError::range_error("nanosecond property cannot be Infinity")); }
                        n as i32
                    } else { 0 };

                    let second_val = get_field(ncx, "second")?;
                    let s = if !second_val.is_undefined() {
                        let n = ncx.to_number_value(&second_val)?;
                        if n.is_infinite() { return Err(VmError::range_error("second property cannot be Infinity")); }
                        n as i32
                    } else { 0 };

                    let year_val = get_field(ncx, "year")?;
                    let y = if !year_val.is_undefined() {
                        let n = ncx.to_number_value(&year_val)?;
                        if n.is_infinite() { return Err(VmError::range_error("year property cannot be Infinity")); }
                        Some(n as i32)
                    } else { None };

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
                    if m < 0 || m > 255 { return Err(VmError::range_error(format!("month out of range: {}", m))); }
                    if d < 0 || d > 255 { return Err(VmError::range_error(format!("day out of range: {}", d))); }
                    let pdt = temporal_rs::PlainDateTime::new_with_overflow(
                        y, m as u8, d as u8,
                        h.clamp(0, 255) as u8, mi.clamp(0, 255) as u8, s.clamp(0, 255) as u8,
                        ms.clamp(0, 65535) as u16, us.clamp(0, 65535) as u16, ns.clamp(0, 65535) as u16,
                        temporal_rs::Calendar::default(), ov,
                    ).map_err(temporal_err)?;

                    return ncx.call_function_construct(
                        &pdt_ctor_for_from, Value::undefined(),
                        &[Value::int32(pdt.year()), Value::int32(pdt.month() as i32), Value::int32(pdt.day() as i32),
                          Value::int32(pdt.hour() as i32), Value::int32(pdt.minute() as i32), Value::int32(pdt.second() as i32),
                          Value::int32(pdt.millisecond() as i32), Value::int32(pdt.microsecond() as i32), Value::int32(pdt.nanosecond() as i32)],
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
            PropertyDescriptor::data_with_attrs(pdt_ctor_value, PropertyAttributes::builtin_method()),
        );
    }

    // Install Temporal.Now methods (after all constructors are defined)
    {
        let now_method = |name: &str, ctor_name: &'static str, arg_builder: fn() -> Vec<Value>| -> Value {
            Value::native_function_with_proto_named(
                move |_this, _args, ncx| {
                    let temporal_ns = ncx.ctx.get_global("Temporal")
                        .ok_or_else(|| VmError::type_error("Temporal namespace not found"))?;
                    let temporal_obj = temporal_ns.as_object()
                        .ok_or_else(|| VmError::type_error("Temporal namespace not found"))?;
                    let ctor = temporal_obj.get(&PropertyKey::string(ctor_name))
                        .ok_or_else(|| VmError::type_error(format!("{} constructor not found", ctor_name)))?;
                    let args = arg_builder();
                    ncx.call_function_construct(&ctor, Value::undefined(), &args)
                },
                mm.clone(),
                fn_proto.clone(),
                name,
                0,
            )
        };

        fn pdt_args() -> Vec<Value> {
            let now = chrono::Local::now();
            vec![
                Value::int32(now.year()),
                Value::int32(now.month() as i32),
                Value::int32(now.day() as i32),
                Value::int32(now.hour() as i32),
                Value::int32(now.minute() as i32),
                Value::int32(now.second() as i32),
                Value::int32((now.nanosecond() / 1_000_000) as i32),
                Value::int32(((now.nanosecond() % 1_000_000) / 1000) as i32),
                Value::int32((now.nanosecond() % 1000) as i32),
            ]
        }
        fn pd_args() -> Vec<Value> {
            let now = chrono::Local::now();
            vec![
                Value::int32(now.year()),
                Value::int32(now.month() as i32),
                Value::int32(now.day() as i32),
            ]
        }
        fn pt_args() -> Vec<Value> {
            let now = chrono::Local::now();
            vec![
                Value::int32(now.hour() as i32),
                Value::int32(now.minute() as i32),
                Value::int32(now.second() as i32),
                Value::int32((now.nanosecond() / 1_000_000) as i32),
                Value::int32(((now.nanosecond() % 1_000_000) / 1000) as i32),
                Value::int32((now.nanosecond() % 1000) as i32),
            ]
        }

        temporal_now.define_property(
            PropertyKey::string("plainDateTimeISO"),
            PropertyDescriptor::data_with_attrs(
                now_method("plainDateTimeISO", "PlainDateTime", pdt_args),
                PropertyAttributes::builtin_method(),
            ),
        );
        temporal_now.define_property(
            PropertyKey::string("plainDateISO"),
            PropertyDescriptor::data_with_attrs(
                now_method("plainDateISO", "PlainDate", pd_args),
                PropertyAttributes::builtin_method(),
            ),
        );
        temporal_now.define_property(
            PropertyKey::string("plainTimeISO"),
            PropertyDescriptor::data_with_attrs(
                now_method("plainTimeISO", "PlainTime", pt_args),
                PropertyAttributes::builtin_method(),
            ),
        );
    }

    let stub_types = [
        "Instant",
        "PlainTime",
        "PlainYearMonth",
        "ZonedDateTime",
        "Duration",
    ];

    for name in &stub_types {
        let stub_proto =
            GcRef::new(JsObject::new(Value::object(obj_proto.clone()), mm.clone()));
        let stub_ctor_obj = GcRef::new(JsObject::new(Value::object(fn_proto.clone()), mm.clone()));
        stub_ctor_obj.define_property(
            PropertyKey::string("prototype"),
            PropertyDescriptor::data_with_attrs(
                Value::object(stub_proto.clone()),
                PropertyAttributes {
                    writable: false,
                    enumerable: false,
                    configurable: false,
                },
            ),
        );
        stub_ctor_obj.define_property(
            PropertyKey::string("name"),
            PropertyDescriptor::function_length(Value::string(JsString::intern(name))),
        );
        stub_ctor_obj.define_property(
            PropertyKey::string("length"),
            PropertyDescriptor::function_length(Value::number(0.0)),
        );
        let name_owned = name.to_string();
        let stub_ctor_fn: Box<
            dyn Fn(&Value, &[Value], &mut NativeContext<'_>) -> Result<Value, VmError> + Send + Sync,
        > = Box::new(move |this, args, _ncx| {
            // Store temporal type on this
            if let Some(obj) = this.as_object() {
                obj.define_property(
                    PropertyKey::string(SLOT_TEMPORAL_TYPE),
                    PropertyDescriptor::builtin_data(Value::string(JsString::intern(&name_owned))),
                );
                // ZonedDateTime: store epochNanoseconds (arg0) and timeZoneId (arg1)
                if name_owned == "ZonedDateTime" {
                    if let Some(epoch_ns) = args.first() {
                        obj.define_property(
                            PropertyKey::string("epochNanoseconds"),
                            PropertyDescriptor::builtin_data(epoch_ns.clone()),
                        );
                    }
                    if let Some(tz_id) = args.get(1) {
                        obj.define_property(
                            PropertyKey::string("timeZoneId"),
                            PropertyDescriptor::builtin_data(tz_id.clone()),
                        );
                    }
                }
                // Instant: store epochNanoseconds (arg0)
                if name_owned == "Instant" {
                    if let Some(epoch_ns) = args.first() {
                        obj.define_property(
                            PropertyKey::string("epochNanoseconds"),
                            PropertyDescriptor::builtin_data(epoch_ns.clone()),
                        );
                    }
                }
                // Duration: store fields from args
                if name_owned == "Duration" {
                    let dur_fields = ["years","months","weeks","days","hours","minutes","seconds","milliseconds","microseconds","nanoseconds"];
                    for (i, field) in dur_fields.iter().enumerate() {
                        if let Some(val) = args.get(i) {
                            if !val.is_undefined() {
                                obj.define_property(
                                    PropertyKey::string(field),
                                    PropertyDescriptor::builtin_data(val.clone()),
                                );
                            }
                        }
                    }
                }
            }
            Ok(Value::undefined())
        });
        let stub_value = Value::native_function_with_proto_and_object(
            Arc::from(stub_ctor_fn),
            mm.clone(),
            fn_proto.clone(),
            stub_ctor_obj.clone(),
        );
        // Wire prototype.constructor
        stub_proto.define_property(
            PropertyKey::string("constructor"),
            PropertyDescriptor::data_with_attrs(
                stub_value.clone(),
                PropertyAttributes::constructor_link(),
            ),
        );

        // Add from() static method to stub types
        let from_ctor = stub_value.clone();
        let from_name = name.to_string();
        let from_fn = Value::native_function_with_proto_named(
            move |_this, args, ncx| {
                let item = args.first().cloned().unwrap_or(Value::undefined());
                if from_name == "Duration" {
                    // Duration.from: use ToTemporalDuration-like logic
                    if item.is_string() {
                        let s = ncx.to_string_value(&item)?;
                        let dur = temporal_rs::Duration::from_utf8(s.as_bytes()).map_err(temporal_err)?;
                        // Construct via constructor with extracted fields
                        let dur_args = vec![
                            Value::number(dur.years() as f64), Value::number(dur.months() as f64),
                            Value::number(dur.weeks() as f64), Value::number(dur.days() as f64),
                            Value::number(dur.hours() as f64), Value::number(dur.minutes() as f64),
                            Value::number(dur.seconds() as f64), Value::number(dur.milliseconds() as f64),
                            Value::number(dur.microseconds() as f64), Value::number(dur.nanoseconds() as f64),
                        ];
                        return ncx.call_function_construct(&from_ctor, Value::undefined(), &dur_args);
                    }
                    // For objects/property bags: read fields properly
                    if let Some(obj) = item.as_object() {
                        // Check if it's already a Duration instance
                        let tt = obj.get(&PropertyKey::string(SLOT_TEMPORAL_TYPE))
                            .and_then(|v| v.as_string().map(|s| s.as_str().to_string()));
                        if tt.as_deref() == Some("Duration") {
                            // Copy fields from existing Duration
                            let fields = ["years","months","weeks","days","hours","minutes","seconds","milliseconds","microseconds","nanoseconds"];
                            let dur_args: Vec<Value> = fields.iter().map(|f| {
                                obj.get(&PropertyKey::string(f)).unwrap_or(Value::int32(0))
                            }).collect();
                            return ncx.call_function_construct(&from_ctor, Value::undefined(), &dur_args);
                        }
                        // Generic property bag: read fields in alphabetical order
                        let field_names_alpha = ["days","hours","microseconds","milliseconds","minutes","months","nanoseconds","seconds","weeks","years"];
                        let field_names_ctor  = ["years","months","weeks","days","hours","minutes","seconds","milliseconds","microseconds","nanoseconds"];
                        let mut field_map = std::collections::HashMap::new();
                        for &f in &field_names_alpha {
                            let v = ncx.get_property(&obj, &PropertyKey::string(f))?;
                            if !v.is_undefined() {
                                let n = ncx.to_number_value(&v)?;
                                if n.is_infinite() { return Err(VmError::range_error(format!("{} cannot be Infinity", f))); }
                                if n.is_nan() { return Err(VmError::range_error(format!("{} cannot be NaN", f))); }
                                if n != n.trunc() { return Err(VmError::range_error(format!("{} must be an integer", f))); }
                                field_map.insert(f, n);
                            }
                        }
                        if field_map.is_empty() {
                            return Err(VmError::type_error("duration object must have at least one temporal property"));
                        }
                        let dur_args: Vec<Value> = field_names_ctor.iter().map(|f| {
                            Value::number(*field_map.get(f).unwrap_or(&0.0))
                        }).collect();
                        return ncx.call_function_construct(&from_ctor, Value::undefined(), &dur_args);
                    }
                    return Err(VmError::type_error("invalid argument for Duration.from"));
                }
                // Non-Duration types: just pass through to constructor
                ncx.call_function_construct(&from_ctor, Value::undefined(), &[item])
            },
            mm.clone(),
            fn_proto.clone(),
            "from",
            1,
        );
        stub_ctor_obj.define_property(
            PropertyKey::string("from"),
            PropertyDescriptor::data_with_attrs(from_fn, PropertyAttributes::builtin_method()),
        );

        // Duration-specific static and prototype methods
        if *name == "Duration" {
            // Duration.compare(d1, d2) — compares two durations by total nanoseconds
            let compare_fn = Value::native_function_with_proto_named(
                |_this, args, _ncx| {
                    let fields = ["years","months","weeks","days","hours","minutes","seconds","milliseconds","microseconds","nanoseconds"];
                    let d1 = args.first().and_then(|v| v.as_object()).ok_or_else(|| VmError::type_error("compare: first argument must be a Duration"))?;
                    let d2 = args.get(1).and_then(|v| v.as_object()).ok_or_else(|| VmError::type_error("compare: second argument must be a Duration"))?;
                    let mut v1 = [0f64; 10];
                    let mut v2 = [0f64; 10];
                    for (i, f) in fields.iter().enumerate() {
                        v1[i] = d1.get(&PropertyKey::string(f)).and_then(|v| v.as_number()).unwrap_or(0.0);
                        v2[i] = d2.get(&PropertyKey::string(f)).and_then(|v| v.as_number()).unwrap_or(0.0);
                    }
                    // total ns: ns + us*1e3 + ms*1e6 + s*1e9 + min*60e9 + h*3600e9 + d*86400e9
                    let ns1 = v1[9] + v1[8]*1e3 + v1[7]*1e6 + v1[6]*1e9 + v1[5]*60e9 + v1[4]*3600e9 + v1[3]*86400e9;
                    let ns2 = v2[9] + v2[8]*1e3 + v2[7]*1e6 + v2[6]*1e9 + v2[5]*60e9 + v2[4]*3600e9 + v2[3]*86400e9;
                    if ns1 < ns2 {
                        Ok(Value::int32(-1))
                    } else if ns1 > ns2 {
                        Ok(Value::int32(1))
                    } else {
                        Ok(Value::int32(0))
                    }
                },
                mm.clone(), fn_proto.clone(), "compare", 2,
            );
            stub_ctor_obj.define_property(
                PropertyKey::string("compare"),
                PropertyDescriptor::data_with_attrs(compare_fn, PropertyAttributes::builtin_method()),
            );

            // .negated() method
            let neg_ctor = stub_value.clone();
            let negated_fn = Value::native_function_with_proto_named(
                move |this, _args, ncx| {
                    let obj = this.as_object().ok_or_else(|| VmError::type_error("negated called on non-Duration"))?;
                    let dur_field_names = ["years","months","weeks","days","hours","minutes","seconds","milliseconds","microseconds","nanoseconds"];
                    let mut neg_args = Vec::with_capacity(10);
                    for field in &dur_field_names {
                        let v = obj.get(&PropertyKey::string(field)).and_then(|v| v.as_number()).unwrap_or(0.0);
                        // Avoid -0: negate only non-zero values
                        neg_args.push(if v == 0.0 { Value::number(0.0) } else { Value::number(-v) });
                    }
                    ncx.call_function_construct(&neg_ctor, Value::undefined(), &neg_args)
                },
                mm.clone(), fn_proto.clone(), "negated", 0,
            );
            stub_proto.define_property(PropertyKey::string("negated"), PropertyDescriptor::builtin_method(negated_fn));

            // .toString() method
            let tostring_fn = Value::native_function_with_proto_named(
                |this, _args, _ncx| {
                    let obj = this.as_object().ok_or_else(|| VmError::type_error("toString called on non-Duration"))?;
                    let dur_field_names = ["years","months","weeks","days","hours","minutes","seconds","milliseconds","microseconds","nanoseconds"];
                    let mut vals = [0i64; 10];
                    for (i, field) in dur_field_names.iter().enumerate() {
                        vals[i] = obj.get(&PropertyKey::string(field)).and_then(|v| v.as_number()).unwrap_or(0.0) as i64;
                    }
                    let [years, months, weeks, days, hours, minutes, seconds, milliseconds, microseconds, nanoseconds] = vals;
                    // Build ISO 8601 duration string
                    let sign = if [years,months,weeks,days,hours,minutes,seconds,milliseconds,microseconds,nanoseconds].iter().any(|&v| v < 0) {
                        // If any field is negative, all should be (per Temporal spec)
                        -1i64
                    } else { 1 };
                    let mut s = String::new();
                    if sign < 0 { s.push('-'); }
                    s.push('P');
                    let ay = years.unsigned_abs();
                    let amo = months.unsigned_abs();
                    let aw = weeks.unsigned_abs();
                    let ad = days.unsigned_abs();
                    if ay > 0 { s.push_str(&format!("{}Y", ay)); }
                    if amo > 0 { s.push_str(&format!("{}M", amo)); }
                    if aw > 0 { s.push_str(&format!("{}W", aw)); }
                    if ad > 0 { s.push_str(&format!("{}D", ad)); }
                    let ah = hours.unsigned_abs();
                    let ami = minutes.unsigned_abs();
                    // Balance seconds/ms/us/ns: compute total nanoseconds then extract seconds + frac
                    let total_ns_i128 = (seconds as i128) * 1_000_000_000
                        + (milliseconds as i128) * 1_000_000
                        + (microseconds as i128) * 1_000
                        + nanoseconds as i128;
                    let total_ns_abs = total_ns_i128.unsigned_abs();
                    let balanced_secs = total_ns_abs / 1_000_000_000;
                    let frac_ns = total_ns_abs % 1_000_000_000;
                    if ah > 0 || ami > 0 || balanced_secs > 0 || frac_ns > 0 {
                        s.push('T');
                        if ah > 0 { s.push_str(&format!("{}H", ah)); }
                        if ami > 0 { s.push_str(&format!("{}M", ami)); }
                        if balanced_secs > 0 || frac_ns > 0 {
                            if frac_ns > 0 {
                                let frac = format!("{:09}", frac_ns);
                                let frac = frac.trim_end_matches('0');
                                s.push_str(&format!("{}.{}S", balanced_secs, frac));
                            } else {
                                s.push_str(&format!("{}S", balanced_secs));
                            }
                        }
                    }
                    if s == "P" || s == "-P" { s = "PT0S".to_string(); }
                    Ok(Value::string(JsString::intern(&s)))
                },
                mm.clone(), fn_proto.clone(), "toString", 0,
            );
            stub_proto.define_property(PropertyKey::string("toString"), PropertyDescriptor::builtin_method(tostring_fn));

            // .total(options) method — returns total number of given unit
            let total_fn = Value::native_function_with_proto_named(
                |this, args, ncx| {
                    let obj = this.as_object().ok_or_else(|| VmError::type_error("total called on non-Duration"))?;
                    let dur_field_names = ["years","months","weeks","days","hours","minutes","seconds","milliseconds","microseconds","nanoseconds"];
                    let mut vals = [0f64; 10];
                    for (i, field) in dur_field_names.iter().enumerate() {
                        vals[i] = obj.get(&PropertyKey::string(field)).and_then(|v| v.as_number()).unwrap_or(0.0);
                    }
                    let [years, months, weeks, days, hours, minutes, seconds, milliseconds, microseconds, nanoseconds] = vals;

                    // Get unit from argument — can be a string or options object with "unit" property
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

                    // Convert to total nanoseconds first, then divide
                    // For time-only durations (no date components), compute total directly
                    let total_ns = nanoseconds
                        + microseconds * 1e3
                        + milliseconds * 1e6
                        + seconds * 1e9
                        + minutes * 60e9
                        + hours * 3600e9
                        + days * 86400e9;

                    let result = match unit_str.as_str() {
                        "nanosecond" | "nanoseconds" => total_ns,
                        "microsecond" | "microseconds" => total_ns / 1e3,
                        "millisecond" | "milliseconds" => total_ns / 1e6,
                        "second" | "seconds" => total_ns / 1e9,
                        "minute" | "minutes" => total_ns / 60e9,
                        "hour" | "hours" => total_ns / 3600e9,
                        "day" | "days" => total_ns / 86400e9,
                        "week" | "weeks" => total_ns / (7.0 * 86400e9),
                        "month" | "months" => {
                            // Approximate — requires calendar context in full impl
                            months + years * 12.0
                        }
                        "year" | "years" => {
                            years + months / 12.0
                        }
                        _ => return Err(VmError::range_error(format!("{} is not a valid unit", unit_str))),
                    };
                    Ok(Value::number(result))
                },
                mm.clone(), fn_proto.clone(), "total", 1,
            );
            stub_proto.define_property(PropertyKey::string("total"), PropertyDescriptor::builtin_method(total_fn));

            // .add(other) method
            let add_dur_ctor = stub_value.clone();
            let add_fn = Value::native_function_with_proto_named(
                move |this, args, ncx| {
                    let obj = this.as_object().ok_or_else(|| VmError::type_error("add called on non-Duration"))?;
                    let dur_field_names = ["years","months","weeks","days","hours","minutes","seconds","milliseconds","microseconds","nanoseconds"];
                    let mut this_vals = [0f64; 10];
                    for (i, field) in dur_field_names.iter().enumerate() {
                        this_vals[i] = obj.get(&PropertyKey::string(field)).and_then(|v| v.as_number()).unwrap_or(0.0);
                    }
                    // Parse other duration
                    let other_arg = args.first().cloned().unwrap_or(Value::undefined());
                    let other_obj = if let Some(o) = other_arg.as_object() {
                        o
                    } else {
                        return Err(VmError::type_error("add requires a Duration argument"));
                    };
                    let mut other_vals = [0f64; 10];
                    for (i, field) in dur_field_names.iter().enumerate() {
                        other_vals[i] = other_obj.get(&PropertyKey::string(field)).and_then(|v| v.as_number()).unwrap_or(0.0);
                    }
                    let result_args: Vec<Value> = (0..10).map(|i| Value::number(this_vals[i] + other_vals[i])).collect();
                    ncx.call_function_construct(&add_dur_ctor, Value::undefined(), &result_args)
                },
                mm.clone(), fn_proto.clone(), "add", 1,
            );
            stub_proto.define_property(PropertyKey::string("add"), PropertyDescriptor::builtin_method(add_fn));

            // @@toStringTag for Duration
            stub_proto.define_property(
                PropertyKey::Symbol(crate::intrinsics::well_known::to_string_tag_symbol()),
                PropertyDescriptor::data_with_attrs(
                    Value::string(JsString::intern("Temporal.Duration")),
                    PropertyAttributes { writable: false, enumerable: false, configurable: true },
                ),
            );
        }

        temporal_obj.define_property(
            PropertyKey::string(name),
            PropertyDescriptor::data_with_attrs(
                stub_value,
                PropertyAttributes::builtin_method(),
            ),
        );
    }

    // Install Temporal on global as non-enumerable
    global.define_property(
        PropertyKey::string("Temporal"),
        PropertyDescriptor::data_with_attrs(
            Value::object(temporal_obj),
            PropertyAttributes::builtin_method(),
        ),
    );
}
