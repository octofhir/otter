use crate::context::NativeContext;
use crate::error::VmError;
use crate::gc::GcRef;
use crate::memory::MemoryManager;
use crate::object::{JsObject, PropertyAttributes, PropertyDescriptor, PropertyKey};
use crate::string::JsString;
use crate::value::Value;
use std::sync::Arc;

use super::common::*;

const SLOT_REF_ISO_DAY: &str = "__pym_ref_iso_day__";

/// Install PlainYearMonth constructor and prototype onto `temporal_obj`.
pub(super) fn install_plain_year_month(
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
        PropertyDescriptor::function_length(Value::string(JsString::intern("PlainYearMonth"))),
    );
    ctor_obj.define_property(
        PropertyKey::string("length"),
        PropertyDescriptor::function_length(Value::number(2.0)),
    );

    // Constructor: new Temporal.PlainYearMonth(year, month [, calendar [, referenceISODay]])
    let ctor_proto = proto.clone();
    let ctor_fn: Box<
        dyn Fn(&Value, &[Value], &mut NativeContext<'_>) -> Result<Value, VmError> + Send + Sync,
    > = Box::new(move |this, args, ncx| {
        let obj = this.as_object().ok_or_else(|| {
            VmError::type_error("Temporal.PlainYearMonth constructor requires 'new'")
        })?;
        // Check it's actually a new target (prototype matches)
        let is_new = obj.prototype().as_object().map_or(false, |p| p.as_ptr() == ctor_proto.as_ptr());
        if !is_new {
            return Err(VmError::type_error("Temporal.PlainYearMonth constructor requires 'new'"));
        }

        // year (required)
        let year_val = args.first().cloned().unwrap_or(Value::undefined());
        let year = to_integer_with_truncation(ncx, &year_val)? as i32;

        // month (required)
        let month_val = args.get(1).cloned().unwrap_or(Value::undefined());
        let month = to_integer_with_truncation(ncx, &month_val)? as i32;

        // calendar (optional, arg 2)
        let calendar_val = args.get(2).cloned().unwrap_or(Value::undefined());
        if !calendar_val.is_undefined() {
            validate_calendar_arg_for_pym(ncx, &calendar_val)?;
        }

        // referenceISODay (optional, arg 3) — defaults to 1
        let ref_day = match args.get(3) {
            Some(v) if !v.is_undefined() => to_integer_with_truncation(ncx, v)? as i32,
            _ => 1,
        };

        // Validate via temporal_rs
        let _validated = temporal_rs::PlainYearMonth::try_new_iso(
            year, month as u8, Some(ref_day as u8),
        ).map_err(temporal_err)?;

        // Store internal slots
        obj.define_property(PropertyKey::string(SLOT_TEMPORAL_TYPE),
            PropertyDescriptor::builtin_data(Value::string(JsString::intern("PlainYearMonth"))));
        obj.define_property(PropertyKey::string(SLOT_ISO_YEAR),
            PropertyDescriptor::builtin_data(Value::int32(year)));
        obj.define_property(PropertyKey::string(SLOT_ISO_MONTH),
            PropertyDescriptor::builtin_data(Value::int32(month)));
        obj.define_property(PropertyKey::string(SLOT_REF_ISO_DAY),
            PropertyDescriptor::builtin_data(Value::int32(ref_day)));

        Ok(Value::undefined())
    });

    let ctor_value = Value::native_function_with_proto_and_object(
        Arc::from(ctor_fn),
        mm.clone(),
        fn_proto.clone(),
        ctor_obj.clone(),
    );

    // Prototype getters
    let make_getter = |slot: &'static str, name: &'static str, mm: &Arc<MemoryManager>, fn_proto: &GcRef<JsObject>| -> Value {
        Value::native_function_with_proto(
            move |this, _args, _ncx| {
                let obj = this.as_object().ok_or_else(|| {
                    VmError::type_error(&format!("{} called on non-object", name))
                })?;
                let ty = obj.get(&PropertyKey::string(SLOT_TEMPORAL_TYPE))
                    .and_then(|v| v.as_string().map(|s| s.as_str().to_string()));
                if ty.as_deref() != Some("PlainYearMonth") {
                    return Err(VmError::type_error(&format!("{} called on non-PlainYearMonth", name)));
                }
                obj.get(&PropertyKey::string(slot))
                    .filter(|v| !v.is_undefined())
                    .ok_or_else(|| VmError::type_error(&format!("{} called on non-PlainYearMonth", name)))
            },
            mm.clone(),
            fn_proto.clone(),
        )
    };

    // year, month getters
    for (slot, name) in &[
        (SLOT_ISO_YEAR, "year"),
        (SLOT_ISO_MONTH, "month"),
    ] {
        proto.define_property(
            PropertyKey::string(name),
            PropertyDescriptor::Accessor {
                get: Some(make_getter(slot, name, mm, fn_proto)),
                set: None,
                attributes: PropertyAttributes { writable: false, enumerable: false, configurable: true },
            },
        );
    }

    // monthCode getter
    proto.define_property(
        PropertyKey::string("monthCode"),
        PropertyDescriptor::Accessor {
            get: Some(Value::native_function_with_proto(
                |this, _args, _ncx| {
                    let obj = this.as_object().ok_or_else(|| VmError::type_error("monthCode getter"))?;
                    let ty = obj.get(&PropertyKey::string(SLOT_TEMPORAL_TYPE))
                        .and_then(|v| v.as_string().map(|s| s.as_str().to_string()));
                    if ty.as_deref() != Some("PlainYearMonth") {
                        return Err(VmError::type_error("monthCode called on non-PlainYearMonth"));
                    }
                    let month = obj.get(&PropertyKey::string(SLOT_ISO_MONTH)).and_then(|v| v.as_int32()).unwrap_or(1);
                    Ok(Value::string(JsString::intern(&format_month_code(month as u32))))
                },
                mm.clone(), fn_proto.clone(),
            )),
            set: None,
            attributes: PropertyAttributes { writable: false, enumerable: false, configurable: true },
        },
    );

    // calendarId getter
    proto.define_property(
        PropertyKey::string("calendarId"),
        PropertyDescriptor::Accessor {
            get: Some(Value::native_function_with_proto(
                |this, _args, _ncx| {
                    let obj = this.as_object().ok_or_else(|| VmError::type_error("calendarId getter"))?;
                    let ty = obj.get(&PropertyKey::string(SLOT_TEMPORAL_TYPE))
                        .and_then(|v| v.as_string().map(|s| s.as_str().to_string()));
                    if ty.as_deref() != Some("PlainYearMonth") {
                        return Err(VmError::type_error("calendarId called on non-PlainYearMonth"));
                    }
                    Ok(Value::string(JsString::intern("iso8601")))
                },
                mm.clone(), fn_proto.clone(),
            )),
            set: None,
            attributes: PropertyAttributes { writable: false, enumerable: false, configurable: true },
        },
    );

    // daysInYear, daysInMonth, monthsInYear, inLeapYear getters
    proto.define_property(
        PropertyKey::string("daysInYear"),
        PropertyDescriptor::Accessor {
            get: Some(Value::native_function_with_proto(
                |this, _args, _ncx| {
                    let obj = this.as_object().ok_or_else(|| VmError::type_error("daysInYear getter"))?;
                    let pym = extract_pym(&obj)?;
                    Ok(Value::int32(pym.days_in_year() as i32))
                },
                mm.clone(), fn_proto.clone(),
            )),
            set: None,
            attributes: PropertyAttributes { writable: false, enumerable: false, configurable: true },
        },
    );
    proto.define_property(
        PropertyKey::string("daysInMonth"),
        PropertyDescriptor::Accessor {
            get: Some(Value::native_function_with_proto(
                |this, _args, _ncx| {
                    let obj = this.as_object().ok_or_else(|| VmError::type_error("daysInMonth getter"))?;
                    let pym = extract_pym(&obj)?;
                    Ok(Value::int32(pym.days_in_month() as i32))
                },
                mm.clone(), fn_proto.clone(),
            )),
            set: None,
            attributes: PropertyAttributes { writable: false, enumerable: false, configurable: true },
        },
    );
    proto.define_property(
        PropertyKey::string("monthsInYear"),
        PropertyDescriptor::Accessor {
            get: Some(Value::native_function_with_proto(
                |this, _args, _ncx| {
                    let obj = this.as_object().ok_or_else(|| VmError::type_error("monthsInYear getter"))?;
                    let pym = extract_pym(&obj)?;
                    Ok(Value::int32(pym.months_in_year() as i32))
                },
                mm.clone(), fn_proto.clone(),
            )),
            set: None,
            attributes: PropertyAttributes { writable: false, enumerable: false, configurable: true },
        },
    );
    proto.define_property(
        PropertyKey::string("inLeapYear"),
        PropertyDescriptor::Accessor {
            get: Some(Value::native_function_with_proto(
                |this, _args, _ncx| {
                    let obj = this.as_object().ok_or_else(|| VmError::type_error("inLeapYear getter"))?;
                    let pym = extract_pym(&obj)?;
                    Ok(Value::boolean(pym.in_leap_year()))
                },
                mm.clone(), fn_proto.clone(),
            )),
            set: None,
            attributes: PropertyAttributes { writable: false, enumerable: false, configurable: true },
        },
    );

    // era / eraYear — always undefined for iso8601
    for name in &["era", "eraYear"] {
        proto.define_property(
            PropertyKey::string(name),
            PropertyDescriptor::Accessor {
                get: Some(Value::native_function_with_proto(
                    |this, _args, _ncx| {
                        let obj = this.as_object().ok_or_else(|| VmError::type_error("era getter"))?;
                        let ty = obj.get(&PropertyKey::string(SLOT_TEMPORAL_TYPE))
                            .and_then(|v| v.as_string().map(|s| s.as_str().to_string()));
                        if ty.as_deref() != Some("PlainYearMonth") {
                            return Err(VmError::type_error("era called on non-PlainYearMonth"));
                        }
                        Ok(Value::undefined())
                    },
                    mm.clone(), fn_proto.clone(),
                )),
                set: None,
                attributes: PropertyAttributes { writable: false, enumerable: false, configurable: true },
            },
        );
    }

    // toString
    let to_string_fn = Value::native_function_with_proto_named(
        |this, args, ncx| {
            let obj = this.as_object().ok_or_else(|| VmError::type_error("toString"))?;
            let ty = obj.get(&PropertyKey::string(SLOT_TEMPORAL_TYPE))
                .and_then(|v| v.as_string().map(|s| s.as_str().to_string()));
            if ty.as_deref() != Some("PlainYearMonth") {
                return Err(VmError::type_error("toString called on non-PlainYearMonth"));
            }
            let y = obj.get(&PropertyKey::string(SLOT_ISO_YEAR)).and_then(|v| v.as_int32()).unwrap_or(0);
            let m = obj.get(&PropertyKey::string(SLOT_ISO_MONTH)).and_then(|v| v.as_int32()).unwrap_or(1);
            let ref_day = obj.get(&PropertyKey::string(SLOT_REF_ISO_DAY)).and_then(|v| v.as_int32()).unwrap_or(1);

            // Read calendarName option
            let options_val = args.first().cloned().unwrap_or(Value::undefined());
            let calendar_name = if !options_val.is_undefined() {
                // Type validation: primitives are not valid options
                if options_val.is_null() || options_val.is_boolean() || options_val.is_number()
                    || options_val.is_bigint() || options_val.is_string() || options_val.as_symbol().is_some() {
                    return Err(VmError::type_error(format!("{} is not a valid options argument", options_val.type_of())));
                }
                let cn = ncx.get_property_of_value(&options_val, &PropertyKey::string("calendarName"))?;
                if !cn.is_undefined() {
                    let s = ncx.to_string_value(&cn)?;
                    match s.as_str() {
                        "auto" | "always" | "never" | "critical" => s,
                        _ => return Err(VmError::range_error(format!("invalid calendarName: {}", s))),
                    }
                } else { "auto".to_string() }
            } else { "auto".to_string() };

            let year_str = format_iso_year(y);
            let base = format!("{}-{:02}", year_str, m);

            let s = match calendar_name.as_str() {
                "always" => {
                    // Must include reference day when showing calendar
                    format!("{}-{:02}[u-ca=iso8601]", base, ref_day)
                }
                "critical" => {
                    format!("{}-{:02}[!u-ca=iso8601]", base, ref_day)
                }
                _ => base,
            };
            Ok(Value::string(JsString::intern(&s)))
        },
        mm.clone(), fn_proto.clone(), "toString", 0,
    );
    proto.define_property(PropertyKey::string("toString"), PropertyDescriptor::builtin_method(to_string_fn));

    // toJSON — calls toString() with no options
    let to_json_fn = Value::native_function_with_proto_named(
        |this, _args, ncx| {
            let obj = this.as_object().ok_or_else(|| VmError::type_error("toJSON"))?;
            let ty = obj.get(&PropertyKey::string(SLOT_TEMPORAL_TYPE))
                .and_then(|v| v.as_string().map(|s| s.as_str().to_string()));
            if ty.as_deref() != Some("PlainYearMonth") {
                return Err(VmError::type_error("toJSON called on non-PlainYearMonth"));
            }
            // Call toString with no args
            if let Some(ts) = obj.get(&PropertyKey::string("toString")) {
                return ncx.call_function(&ts, Value::object(obj), &[]);
            }
            Err(VmError::type_error("toJSON"))
        },
        mm.clone(), fn_proto.clone(), "toJSON", 0,
    );
    proto.define_property(PropertyKey::string("toJSON"), PropertyDescriptor::builtin_method(to_json_fn));

    // toLocaleString
    let to_locale_string_fn = Value::native_function_with_proto_named(
        |this, _args, ncx| {
            let obj = this.as_object().ok_or_else(|| VmError::type_error("toLocaleString"))?;
            let ty = obj.get(&PropertyKey::string(SLOT_TEMPORAL_TYPE))
                .and_then(|v| v.as_string().map(|s| s.as_str().to_string()));
            if ty.as_deref() != Some("PlainYearMonth") {
                return Err(VmError::type_error("toLocaleString called on non-PlainYearMonth"));
            }
            if let Some(ts) = obj.get(&PropertyKey::string("toString")) {
                return ncx.call_function(&ts, Value::object(obj), &[]);
            }
            Err(VmError::type_error("toLocaleString"))
        },
        mm.clone(), fn_proto.clone(), "toLocaleString", 0,
    );
    proto.define_property(PropertyKey::string("toLocaleString"), PropertyDescriptor::builtin_method(to_locale_string_fn));

    // valueOf — always throw
    let value_of_fn = Value::native_function_with_proto_named(
        |_this, _args, _ncx| {
            Err(VmError::type_error("use compare() or toString() to compare Temporal.PlainYearMonth"))
        },
        mm.clone(), fn_proto.clone(), "valueOf", 0,
    );
    proto.define_property(PropertyKey::string("valueOf"), PropertyDescriptor::builtin_method(value_of_fn));

    // equals
    let equals_fn = Value::native_function_with_proto_named(
        |this, args, ncx| {
            let obj = this.as_object().ok_or_else(|| VmError::type_error("equals"))?;
            let this_pym = extract_pym(&obj)?;
            let other_val = args.first().cloned().unwrap_or(Value::undefined());
            let other_pym = to_temporal_year_month(ncx, &other_val)?;
            Ok(Value::boolean(this_pym.compare_iso(&other_pym) == std::cmp::Ordering::Equal))
        },
        mm.clone(), fn_proto.clone(), "equals", 1,
    );
    proto.define_property(PropertyKey::string("equals"), PropertyDescriptor::builtin_method(equals_fn));

    // with(fields [, options])
    let with_fn = Value::native_function_with_proto_named(
        |this, args, ncx| {
            let obj = this.as_object().ok_or_else(|| VmError::type_error("with"))?;
            let this_pym = extract_pym(&obj)?;
            let fields_val = args.first().cloned().unwrap_or(Value::undefined());
            if !fields_val.as_object().is_some() && !fields_val.as_proxy().is_some() {
                return Err(VmError::type_error("with requires an object argument"));
            }

            // Reject Temporal objects (IsPartialTemporalObject step 2)
            if let Some(arg_obj) = fields_val.as_object() {
                let tt = arg_obj.get(&PropertyKey::string(SLOT_TEMPORAL_TYPE))
                    .and_then(|v| v.as_string().map(|s| s.as_str().to_string()));
                if tt.is_some() {
                    return Err(VmError::type_error("a Temporal object is not a valid argument to with()"));
                }
            }

            // Reject calendar and timeZone properties (per spec)
            let cal_check = ncx.get_property_of_value(&fields_val, &PropertyKey::string("calendar"))?;
            if !cal_check.is_undefined() {
                return Err(VmError::type_error("calendar not allowed in with()"));
            }
            let tz_check = ncx.get_property_of_value(&fields_val, &PropertyKey::string("timeZone"))?;
            if !tz_check.is_undefined() {
                return Err(VmError::type_error("timeZone not allowed in with()"));
            }

            // Read and coerce fields interleaved (per spec observable order):
            // month → coerce, monthCode → coerce, year → coerce
            let month_val = ncx.get_property_of_value(&fields_val, &PropertyKey::string("month"))?;
            let month_num = if !month_val.is_undefined() {
                Some(ncx.to_number_value(&month_val)?)
            } else { None };

            let month_code_val = ncx.get_property_of_value(&fields_val, &PropertyKey::string("monthCode"))?;
            // Validate monthCode syntax during reading (coerce to string)
            let month_code_coerced = if !month_code_val.is_undefined() {
                let mc_str = ncx.to_string_value(&month_code_val)?;
                validate_month_code_syntax(&mc_str)?;
                Some(mc_str)
            } else { None };

            let year_val = ncx.get_property_of_value(&fields_val, &PropertyKey::string("year"))?;
            let year_num = if !year_val.is_undefined() {
                Some(ncx.to_number_value(&year_val)?)
            } else { None };

            // Check that at least one recognized field is provided
            if month_num.is_none() && month_code_coerced.is_none() && year_num.is_none() {
                return Err(VmError::type_error("at least one recognized field required"));
            }

            let year = if let Some(n) = year_num {
                if n.is_infinite() { return Err(VmError::range_error("year cannot be Infinity")); }
                n as i32
            } else {
                this_pym.year()
            };

            // Resolve month (with RequirePositiveInteger check BEFORE GetOptionsObject)
            let month_num_int = if let Some(m) = month_num {
                if m.is_infinite() { return Err(VmError::range_error("month cannot be Infinity")); }
                let m_int = m as i32;
                if m_int <= 0 { return Err(VmError::range_error(format!("month {} is out of range", m_int))); }
                Some(m_int)
            } else { None };

            // Read overflow option (GetOptionsObject) — observable side effects
            let options_val = args.get(1).cloned().unwrap_or(Value::undefined());
            let overflow = if !options_val.is_undefined() {
                parse_overflow_option(ncx, &options_val)?
            } else {
                temporal_rs::options::Overflow::Constrain
            };

            // NOW validate monthCode suitability (after overflow is read)
            let month = if let Some(ref mc_str) = month_code_coerced {
                let mc_month = validate_month_code_iso_suitability(mc_str)? as i32;
                if let Some(m) = month_num_int {
                    if m != mc_month {
                        return Err(VmError::range_error("Mismatch between month and monthCode"));
                    }
                }
                mc_month
            } else if let Some(m_int) = month_num_int {
                m_int
            } else {
                this_pym.month() as i32
            };

            let pym = temporal_rs::PlainYearMonth::new_with_overflow(
                year, month as u8, None, temporal_rs::Calendar::default(), overflow,
            ).map_err(temporal_err)?;
            construct_plain_year_month_value_full(ncx, pym.year(), pym.month() as i32, 1)
        },
        mm.clone(), fn_proto.clone(), "with", 1,
    );
    proto.define_property(PropertyKey::string("with"), PropertyDescriptor::builtin_method(with_fn));

    // toPlainDate(fields)
    let to_plain_date_fn = Value::native_function_with_proto_named(
        |this, args, ncx| {
            let obj = this.as_object().ok_or_else(|| VmError::type_error("toPlainDate"))?;
            let this_pym = extract_pym(&obj)?;
            let fields_val = args.first().cloned().unwrap_or(Value::undefined());
            if fields_val.is_undefined() {
                return Err(VmError::type_error("toPlainDate requires an argument"));
            }
            if !fields_val.as_object().is_some() && !fields_val.as_proxy().is_some() {
                return Err(VmError::type_error("toPlainDate argument must be an object"));
            }
            let day_val = ncx.get_property_of_value(&fields_val, &PropertyKey::string("day"))?;
            if day_val.is_undefined() {
                return Err(VmError::type_error("day is required"));
            }
            let day_num = ncx.to_number_value(&day_val)?;
            if day_num.is_infinite() { return Err(VmError::range_error("day cannot be Infinity")); }
            let day = day_num as i32;
            // Use constrain overflow to handle out-of-range days (e.g. Feb 29 in non-leap year)
            let pd = temporal_rs::PlainDate::new_with_overflow(
                this_pym.year(), this_pym.month(), day as u8,
                temporal_rs::Calendar::default(),
                temporal_rs::options::Overflow::Constrain,
            ).map_err(temporal_err)?;
            construct_plain_date_value(ncx, pd.year(), pd.month() as i32, pd.day() as i32)
        },
        mm.clone(), fn_proto.clone(), "toPlainDate", 1,
    );
    proto.define_property(PropertyKey::string("toPlainDate"), PropertyDescriptor::builtin_method(to_plain_date_fn));

    // add(duration [, options])
    let add_fn = Value::native_function_with_proto_named(
        |this, args, ncx| {
            let obj = this.as_object().ok_or_else(|| VmError::type_error("add"))?;
            let this_pym = extract_pym(&obj)?;
            let dur_val = args.first().cloned().unwrap_or(Value::undefined());
            let dur = to_temporal_duration(ncx, &dur_val)?;
            let options_val = args.get(1).cloned().unwrap_or(Value::undefined());
            let overflow = if !options_val.is_undefined() {
                parse_overflow_option(ncx, &options_val)?
            } else {
                temporal_rs::options::Overflow::Constrain
            };
            let result = this_pym.add(&dur, overflow).map_err(temporal_err)?;
            construct_plain_year_month_value_full(ncx, result.year(), result.month() as i32, result.reference_day() as i32)
        },
        mm.clone(), fn_proto.clone(), "add", 1,
    );
    proto.define_property(PropertyKey::string("add"), PropertyDescriptor::builtin_method(add_fn));

    // subtract(duration [, options])
    let subtract_fn = Value::native_function_with_proto_named(
        |this, args, ncx| {
            let obj = this.as_object().ok_or_else(|| VmError::type_error("subtract"))?;
            let this_pym = extract_pym(&obj)?;
            let dur_val = args.first().cloned().unwrap_or(Value::undefined());
            let dur = to_temporal_duration(ncx, &dur_val)?;
            let options_val = args.get(1).cloned().unwrap_or(Value::undefined());
            let overflow = if !options_val.is_undefined() {
                parse_overflow_option(ncx, &options_val)?
            } else {
                temporal_rs::options::Overflow::Constrain
            };
            let result = this_pym.subtract(&dur, overflow).map_err(temporal_err)?;
            construct_plain_year_month_value_full(ncx, result.year(), result.month() as i32, result.reference_day() as i32)
        },
        mm.clone(), fn_proto.clone(), "subtract", 1,
    );
    proto.define_property(PropertyKey::string("subtract"), PropertyDescriptor::builtin_method(subtract_fn));

    // until(other [, options])
    let until_fn = Value::native_function_with_proto_named(
        |this, args, ncx| {
            let obj = this.as_object().ok_or_else(|| VmError::type_error("until"))?;
            let this_pym = extract_pym(&obj)?;
            let other_val = args.first().cloned().unwrap_or(Value::undefined());
            let other_pym = to_temporal_year_month(ncx, &other_val)?;
            let options_val = args.get(1).cloned().unwrap_or(Value::undefined());
            let settings = parse_difference_settings_for_year_month(ncx, &options_val, false)?;
            let dur = this_pym.until(&other_pym, settings).map_err(temporal_err)?;
            construct_duration_value(ncx, &dur)
        },
        mm.clone(), fn_proto.clone(), "until", 1,
    );
    proto.define_property(PropertyKey::string("until"), PropertyDescriptor::builtin_method(until_fn));

    // since(other [, options])
    let since_fn = Value::native_function_with_proto_named(
        |this, args, ncx| {
            let obj = this.as_object().ok_or_else(|| VmError::type_error("since"))?;
            let this_pym = extract_pym(&obj)?;
            let other_val = args.first().cloned().unwrap_or(Value::undefined());
            let other_pym = to_temporal_year_month(ncx, &other_val)?;
            let options_val = args.get(1).cloned().unwrap_or(Value::undefined());
            let settings = parse_difference_settings_for_year_month(ncx, &options_val, true)?;
            let dur = this_pym.since(&other_pym, settings).map_err(temporal_err)?;
            construct_duration_value(ncx, &dur)
        },
        mm.clone(), fn_proto.clone(), "since", 1,
    );
    proto.define_property(PropertyKey::string("since"), PropertyDescriptor::builtin_method(since_fn));

    // getISOFields()
    let get_iso_fields_fn = Value::native_function_with_proto_named(
        |this, _args, _ncx| {
            let obj = this.as_object().ok_or_else(|| VmError::type_error("getISOFields"))?;
            let ty = obj.get(&PropertyKey::string(SLOT_TEMPORAL_TYPE))
                .and_then(|v| v.as_string().map(|s| s.as_str().to_string()));
            if ty.as_deref() != Some("PlainYearMonth") {
                return Err(VmError::type_error("getISOFields called on non-PlainYearMonth"));
            }
            // Not supported — Temporal spec removed getISOFields in later drafts
            Err(VmError::type_error("getISOFields is not supported"))
        },
        mm.clone(), fn_proto.clone(), "getISOFields", 0,
    );
    proto.define_property(PropertyKey::string("getISOFields"), PropertyDescriptor::builtin_method(get_iso_fields_fn));

    // @@toStringTag
    proto.define_property(
        PropertyKey::Symbol(crate::intrinsics::well_known::to_string_tag_symbol()),
        PropertyDescriptor::data_with_attrs(
            Value::string(JsString::intern("Temporal.PlainYearMonth")),
            PropertyAttributes { writable: false, enumerable: false, configurable: true },
        ),
    );

    // prototype.constructor
    proto.define_property(
        PropertyKey::string("constructor"),
        PropertyDescriptor::data_with_attrs(ctor_value.clone(), PropertyAttributes::constructor_link()),
    );

    // PlainYearMonth.from(item [, options])
    let from_fn = Value::native_function_with_proto_named(
        |_this, args, ncx| {
            let item = args.first().cloned().unwrap_or(Value::undefined());
            let options_val = args.get(1).cloned().unwrap_or(Value::undefined());

            // String — parse first, then validate options (per spec: string errors before options TypeError)
            if item.is_string() {
                let s = ncx.to_string_value(&item)?;
                // Reject fractional hours/minutes (only if string has a time portion)
                reject_fractional_hours_minutes(s.as_str())?;
                // Validate annotations in the string
                if let Some(bracket_pos) = s.find('[') {
                    validate_annotations(&s[bracket_pos..])?;
                }
                // Reject UTC designator Z
                reject_utc_designator_for_plain(s.as_str())?;
                // Parse via temporal_rs
                let pym = temporal_rs::PlainYearMonth::from_utf8(s.as_bytes()).map_err(temporal_err)?;
                // Read overflow option (observable side effects even for strings)
                if !options_val.is_undefined() {
                    parse_overflow_option(ncx, &options_val)?;
                }
                return construct_plain_year_month_value_full(ncx, pym.year(), pym.month() as i32, 1);
            }

            // Non-object types → throw
            if item.is_undefined() || item.is_null() || item.is_boolean()
                || item.is_number() || item.is_bigint() {
                return Err(VmError::type_error(format!(
                    "cannot convert {} to a PlainYearMonth", item.type_of()
                )));
            }
            if item.as_symbol().is_some() {
                return Err(VmError::type_error("Cannot convert a Symbol value to a string"));
            }

            let is_object_like = item.as_object().is_some() || item.as_proxy().is_some();

            // Object — could be PlainYearMonth, PlainDate, or property bag
            if let Some(obj) = item.as_object() {
                let temporal_type = obj.get(&PropertyKey::string(SLOT_TEMPORAL_TYPE))
                    .and_then(|v| v.as_string().map(|s| s.as_str().to_string()));

                // Already a PlainYearMonth — read overflow option (observable), then clone
                if temporal_type.as_deref() == Some("PlainYearMonth") {
                    if !options_val.is_undefined() {
                        parse_overflow_option(ncx, &options_val)?;
                    }
                    let y = obj.get(&PropertyKey::string(SLOT_ISO_YEAR)).and_then(|v| v.as_int32()).unwrap_or(0);
                    let m = obj.get(&PropertyKey::string(SLOT_ISO_MONTH)).and_then(|v| v.as_int32()).unwrap_or(1);
                    let d = obj.get(&PropertyKey::string(SLOT_REF_ISO_DAY)).and_then(|v| v.as_int32()).unwrap_or(1);
                    return construct_plain_year_month_value_full(ncx, y, m as i32, d);
                }

                // PlainDate → extract year/month
                if temporal_type.as_deref() == Some("PlainDate") {
                    if !options_val.is_undefined() {
                        parse_overflow_option(ncx, &options_val)?;
                    }
                    let y = obj.get(&PropertyKey::string(SLOT_ISO_YEAR)).and_then(|v| v.as_int32()).unwrap_or(0);
                    let m = obj.get(&PropertyKey::string(SLOT_ISO_MONTH)).and_then(|v| v.as_int32()).unwrap_or(1);
                    return construct_plain_year_month_value_full(ncx, y, m as i32, 1);
                }
            }

            // Property bag — handles both plain objects and proxies
            if is_object_like {
                // Read fields in spec order: calendar → month → monthCode → year
                let cal_val = ncx.get_property_of_value(&item, &PropertyKey::string("calendar"))?;
                if !cal_val.is_undefined() {
                    validate_calendar_arg_standalone(ncx, &cal_val)?;
                }

                // month — read and coerce
                let month_val = ncx.get_property_of_value(&item, &PropertyKey::string("month"))?;
                let month_num = if !month_val.is_undefined() {
                    let n = ncx.to_number_value(&month_val)?;
                    Some(n)
                } else { None };

                // monthCode — read and validate type (must be string, per RequireString)
                let month_code_val = ncx.get_property_of_value(&item, &PropertyKey::string("monthCode"))?;
                let month_code_str = if !month_code_val.is_undefined() {
                    if !month_code_val.is_string() {
                        return Err(VmError::type_error(format!(
                            "monthCode must be a string, got {}",
                            month_code_val.type_of()
                        )));
                    }
                    let s = month_code_val.as_string().unwrap().as_str().to_string();
                    // Validate syntax immediately (RangeError before year TypeError)
                    validate_month_code_syntax(&s)?;
                    Some(s)
                } else { None };

                // year — read and coerce
                let year_val = ncx.get_property_of_value(&item, &PropertyKey::string("year"))?;
                let year_num = if !year_val.is_undefined() {
                    let n = ncx.to_number_value(&year_val)?;
                    Some(n)
                } else { None };

                // Read overflow from options BEFORE field validation (per spec)
                let overflow = if !options_val.is_undefined() {
                    validate_options_type(&options_val)?;
                    parse_overflow_option(ncx, &options_val)?
                } else {
                    temporal_rs::options::Overflow::Constrain
                };

                // Type validation (TypeError) — check required fields present
                let year = match year_num {
                    Some(n) => {
                        if n.is_infinite() { return Err(VmError::range_error("year cannot be Infinity")); }
                        n as i32
                    }
                    None => return Err(VmError::type_error("year is required")),
                };

                if month_num.is_none() && month_code_str.is_none() {
                    return Err(VmError::type_error("month or monthCode is required"));
                }

                // Range validation (RangeError) — validate month
                let month = if let Some(ref mc_str) = month_code_str {
                    let mc_month = validate_month_code_iso_suitability(mc_str)? as i32;
                    // If both month and monthCode provided, they must agree
                    if let Some(m_num) = month_num {
                        if m_num as i32 != mc_month {
                            return Err(VmError::range_error("Mismatch between month and monthCode"));
                        }
                    }
                    mc_month
                } else if let Some(m_num) = month_num {
                    if m_num.is_infinite() { return Err(VmError::range_error("month cannot be Infinity")); }
                    // Validate month range (negative and out-of-range)
                    let m_int = m_num as i32;
                    if m_int < 1 || m_int > 12 {
                        if overflow == temporal_rs::options::Overflow::Reject {
                            return Err(VmError::range_error(format!("month {} is out of range (1-12)", m_int)));
                        }
                        // For constrain: clamp to 1-12
                        if m_int < 1 {
                            return Err(VmError::range_error(format!("month {} is out of range", m_int)));
                        }
                    }
                    m_int
                } else {
                    unreachable!()
                };

                // Validate via temporal_rs (with overflow)
                let pym = temporal_rs::PlainYearMonth::new_with_overflow(
                    year, month as u8, None, temporal_rs::Calendar::default(), overflow,
                ).map_err(temporal_err)?;

                return construct_plain_year_month_value_full(ncx, pym.year(), pym.month() as i32, 1);
            }

            Err(VmError::type_error("Expected an object or string for PlainYearMonth.from"))
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

    // PlainYearMonth.compare
    let compare_fn = Value::native_function_with_proto_named(
        |_this, args, ncx| {
            let one_val = args.first().cloned().unwrap_or(Value::undefined());
            let two_val = args.get(1).cloned().unwrap_or(Value::undefined());
            let one = to_temporal_year_month(ncx, &one_val)?;
            let two = to_temporal_year_month(ncx, &two_val)?;
            match one.compare_iso(&two) {
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

    // Register on namespace
    temporal_obj.define_property(
        PropertyKey::string("PlainYearMonth"),
        PropertyDescriptor::data_with_attrs(ctor_value, PropertyAttributes::builtin_method()),
    );
}

/// Extract a temporal_rs::PlainYearMonth from an object's internal slots.
fn extract_pym(obj: &GcRef<JsObject>) -> Result<temporal_rs::PlainYearMonth, VmError> {
    let ty = obj.get(&PropertyKey::string(SLOT_TEMPORAL_TYPE))
        .and_then(|v| v.as_string().map(|s| s.as_str().to_string()));
    if ty.as_deref() != Some("PlainYearMonth") {
        return Err(VmError::type_error("not a PlainYearMonth"));
    }
    let y = obj.get(&PropertyKey::string(SLOT_ISO_YEAR)).and_then(|v| v.as_int32()).unwrap_or(0);
    let m = obj.get(&PropertyKey::string(SLOT_ISO_MONTH)).and_then(|v| v.as_int32()).unwrap_or(1);
    let d = obj.get(&PropertyKey::string(SLOT_REF_ISO_DAY)).and_then(|v| v.as_int32()).unwrap_or(1);
    temporal_rs::PlainYearMonth::try_new_iso(y, m as u8, Some(d as u8)).map_err(temporal_err)
}

/// Convert a JS value to temporal_rs::PlainYearMonth (string or object).
fn to_temporal_year_month(ncx: &mut NativeContext<'_>, item: &Value) -> Result<temporal_rs::PlainYearMonth, VmError> {
    if item.is_string() {
        let s = ncx.to_string_value(item)?;
        return temporal_rs::PlainYearMonth::from_utf8(s.as_bytes()).map_err(temporal_err);
    }
    let is_object_like = item.as_object().is_some() || item.as_proxy().is_some();
    if !is_object_like {
        return Err(VmError::type_error("Expected an object or string for PlainYearMonth"));
    }
    // Check for native PlainYearMonth object (not proxy)
    if let Some(obj) = item.as_object() {
        let temporal_type = obj.get(&PropertyKey::string(SLOT_TEMPORAL_TYPE))
            .and_then(|v| v.as_string().map(|s| s.as_str().to_string()));
        if temporal_type.as_deref() == Some("PlainYearMonth") {
            return extract_pym(&obj);
        }
    }
    // Property bag (object or proxy) — read and coerce fields in spec order
    // calendar
    let cal_val = ncx.get_property_of_value(item, &PropertyKey::string("calendar"))?;
    if !cal_val.is_undefined() {
        validate_calendar_arg_standalone(ncx, &cal_val)?;
    }
    // month — read and coerce
    let month_val = ncx.get_property_of_value(item, &PropertyKey::string("month"))?;
    let month_num = if !month_val.is_undefined() {
        Some(ncx.to_number_value(&month_val)?)
    } else { None };
    // monthCode — read and coerce
    let month_code_val = ncx.get_property_of_value(item, &PropertyKey::string("monthCode"))?;
    let month_code_str = if !month_code_val.is_undefined() {
        Some(ncx.to_string_value(&month_code_val)?)
    } else { None };
    // year — read and coerce
    let year_val = ncx.get_property_of_value(item, &PropertyKey::string("year"))?;
    let year_num = if !year_val.is_undefined() {
        Some(ncx.to_number_value(&year_val)?)
    } else { None };

    // Validation: year required
    let year = match year_num {
        Some(n) => {
            if n.is_infinite() { return Err(VmError::range_error("year cannot be Infinity")); }
            if n == 0.0 && n.is_sign_negative() {
                return Err(VmError::range_error("negative zero is not a valid year"));
            }
            n as i32
        }
        None => return Err(VmError::type_error("year required")),
    };
    // Validation: month from monthCode or month
    let month = if let Some(ref mc_str) = month_code_str {
        validate_month_code_syntax(mc_str)?;
        let mc_month = validate_month_code_iso_suitability(mc_str)? as i32;
        if let Some(m) = month_num {
            if m as i32 != mc_month {
                return Err(VmError::range_error("Mismatch between month and monthCode"));
            }
        }
        mc_month
    } else if let Some(m) = month_num {
        m as i32
    } else {
        return Err(VmError::type_error("month or monthCode required"));
    };
    temporal_rs::PlainYearMonth::try_new_iso(year, month as u8, None).map_err(temporal_err)
}

/// Helper to construct PlainYearMonth via the constructor with all 4 args.
fn construct_plain_year_month_value_full(
    ncx: &mut NativeContext<'_>,
    year: i32,
    month: i32,
    ref_day: i32,
) -> Result<Value, VmError> {
    let temporal_ns = ncx.ctx.get_global("Temporal")
        .ok_or_else(|| VmError::type_error("Temporal namespace not found"))?;
    let temporal_obj = temporal_ns.as_object()
        .ok_or_else(|| VmError::type_error("Temporal namespace not found"))?;
    let ctor = temporal_obj.get(&PropertyKey::string("PlainYearMonth"))
        .ok_or_else(|| VmError::type_error("PlainYearMonth constructor not found"))?;
    ncx.call_function_construct(&ctor, Value::undefined(), &[
        Value::int32(year), Value::int32(month),
        Value::string(JsString::intern("iso8601")), Value::int32(ref_day),
    ])
}

/// Validate calendar arg for PlainYearMonth constructor — same rules as PlainDateTime
fn validate_calendar_arg_for_pym(ncx: &mut NativeContext<'_>, cal: &Value) -> Result<(), VmError> {
    if cal.is_null() || cal.is_boolean() || cal.is_number() || cal.is_bigint() {
        return Err(VmError::type_error("invalid calendar argument"));
    }
    if cal.as_symbol().is_some() {
        return Err(VmError::type_error("Cannot convert a Symbol value to a string"));
    }
    if cal.as_object().is_some() || cal.as_proxy().is_some()
        || cal.is_function() || cal.is_native_function() {
        return Err(VmError::type_error("object is not a valid calendar"));
    }
    let s = ncx.to_string_value(cal)?;
    let lower = s.as_str().to_ascii_lowercase();
    if lower != "iso8601" {
        return Err(VmError::range_error(format!("Unknown calendar: {}", s)));
    }
    Ok(())
}

/// Validate options type: must be undefined, null-ok for absent, or an object/function.
/// Primitives like null, boolean, string, number, symbol, bigint throw TypeError.
fn validate_options_type(val: &Value) -> Result<(), VmError> {
    if val.is_null() || val.is_boolean() || val.is_number() || val.is_bigint() {
        return Err(VmError::type_error(format!(
            "{} is not a valid options argument",
            if val.is_null() { "null" } else { val.type_of() }
        )));
    }
    if val.as_symbol().is_some() {
        return Err(VmError::type_error("Cannot convert a Symbol to an object"));
    }
    if val.is_string() {
        return Err(VmError::type_error("string is not a valid options argument"));
    }
    // Objects, functions, proxies are OK
    Ok(())
}
