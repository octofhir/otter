use crate::context::NativeContext;
use crate::error::VmError;
use crate::gc::GcRef;
use crate::memory::MemoryManager;
use crate::object::{JsObject, PropertyAttributes, PropertyDescriptor, PropertyKey};
use crate::string::JsString;
use crate::value::Value;
use std::sync::Arc;

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

    // Constructor
    let ctor_fn: Box<
        dyn Fn(&Value, &[Value], &mut NativeContext<'_>) -> Result<Value, VmError> + Send + Sync,
    > = Box::new(|this, args, _ncx| {
        if let Some(obj) = this.as_object() {
            obj.define_property(
                PropertyKey::string(SLOT_TEMPORAL_TYPE),
                PropertyDescriptor::builtin_data(Value::string(JsString::intern("Duration"))),
            );
            let dur_fields = [
                "years", "months", "weeks", "days", "hours", "minutes",
                "seconds", "milliseconds", "microseconds", "nanoseconds",
            ];
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
        PropertyDescriptor::data_with_attrs(ctor_value.clone(), PropertyAttributes::constructor_link()),
    );

    // ========================================================================
    // Duration.from()
    // ========================================================================
    let from_ctor = ctor_value.clone();
    let from_fn = Value::native_function_with_proto_named(
        move |_this, args, ncx| {
            let item = args.first().cloned().unwrap_or(Value::undefined());
            if item.is_string() {
                let s = ncx.to_string_value(&item)?;
                let dur = temporal_rs::Duration::from_utf8(s.as_bytes()).map_err(temporal_err)?;
                let dur_args = vec![
                    Value::number(dur.years() as f64), Value::number(dur.months() as f64),
                    Value::number(dur.weeks() as f64), Value::number(dur.days() as f64),
                    Value::number(dur.hours() as f64), Value::number(dur.minutes() as f64),
                    Value::number(dur.seconds() as f64), Value::number(dur.milliseconds() as f64),
                    Value::number(dur.microseconds() as f64), Value::number(dur.nanoseconds() as f64),
                ];
                return ncx.call_function_construct(&from_ctor, Value::undefined(), &dur_args);
            }
            if let Some(obj) = item.as_object() {
                let tt = obj.get(&PropertyKey::string(SLOT_TEMPORAL_TYPE))
                    .and_then(|v| v.as_string().map(|s| s.as_str().to_string()));
                if tt.as_deref() == Some("Duration") {
                    let fields = ["years","months","weeks","days","hours","minutes","seconds","milliseconds","microseconds","nanoseconds"];
                    let dur_args: Vec<Value> = fields.iter().map(|f| {
                        obj.get(&PropertyKey::string(f)).unwrap_or(Value::int32(0))
                    }).collect();
                    return ncx.call_function_construct(&from_ctor, Value::undefined(), &dur_args);
                }
                // Generic property bag
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
            let fields = ["years","months","weeks","days","hours","minutes","seconds","milliseconds","microseconds","nanoseconds"];
            let d1 = args.first().and_then(|v| v.as_object()).ok_or_else(|| VmError::type_error("compare: first argument must be a Duration"))?;
            let d2 = args.get(1).and_then(|v| v.as_object()).ok_or_else(|| VmError::type_error("compare: second argument must be a Duration"))?;
            let mut v1 = [0f64; 10];
            let mut v2 = [0f64; 10];
            for (i, f) in fields.iter().enumerate() {
                v1[i] = d1.get(&PropertyKey::string(f)).and_then(|v| v.as_number()).unwrap_or(0.0);
                v2[i] = d2.get(&PropertyKey::string(f)).and_then(|v| v.as_number()).unwrap_or(0.0);
            }
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
    ctor_obj.define_property(
        PropertyKey::string("compare"),
        PropertyDescriptor::data_with_attrs(compare_fn, PropertyAttributes::builtin_method()),
    );

    // ========================================================================
    // Prototype methods
    // ========================================================================

    // .negated()
    let neg_ctor = ctor_value.clone();
    let negated_fn = Value::native_function_with_proto_named(
        move |this, _args, ncx| {
            let obj = this.as_object().ok_or_else(|| VmError::type_error("negated called on non-Duration"))?;
            let dur_field_names = ["years","months","weeks","days","hours","minutes","seconds","milliseconds","microseconds","nanoseconds"];
            let mut neg_args = Vec::with_capacity(10);
            for field in &dur_field_names {
                let v = obj.get(&PropertyKey::string(field)).and_then(|v| v.as_number()).unwrap_or(0.0);
                neg_args.push(if v == 0.0 { Value::number(0.0) } else { Value::number(-v) });
            }
            ncx.call_function_construct(&neg_ctor, Value::undefined(), &neg_args)
        },
        mm.clone(), fn_proto.clone(), "negated", 0,
    );
    proto.define_property(PropertyKey::string("negated"), PropertyDescriptor::builtin_method(negated_fn));

    // .toString()
    let tostring_fn = Value::native_function_with_proto_named(
        |this, _args, _ncx| {
            let obj = this.as_object().ok_or_else(|| VmError::type_error("toString called on non-Duration"))?;
            let dur_field_names = ["years","months","weeks","days","hours","minutes","seconds","milliseconds","microseconds","nanoseconds"];
            let mut vals = [0i64; 10];
            for (i, field) in dur_field_names.iter().enumerate() {
                vals[i] = obj.get(&PropertyKey::string(field)).and_then(|v| v.as_number()).unwrap_or(0.0) as i64;
            }
            let [years, months, _weeks, days, hours, minutes, seconds, milliseconds, microseconds, nanoseconds] = vals;
            let sign = if [years,months,_weeks,days,hours,minutes,seconds,milliseconds,microseconds,nanoseconds].iter().any(|&v| v < 0) {
                -1i64
            } else { 1 };
            let mut s = String::new();
            if sign < 0 { s.push('-'); }
            s.push('P');
            let ay = years.unsigned_abs();
            let amo = months.unsigned_abs();
            let aw = _weeks.unsigned_abs();
            let ad = days.unsigned_abs();
            if ay > 0 { s.push_str(&format!("{}Y", ay)); }
            if amo > 0 { s.push_str(&format!("{}M", amo)); }
            if aw > 0 { s.push_str(&format!("{}W", aw)); }
            if ad > 0 { s.push_str(&format!("{}D", ad)); }
            let ah = hours.unsigned_abs();
            let ami = minutes.unsigned_abs();
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
    proto.define_property(PropertyKey::string("toString"), PropertyDescriptor::builtin_method(tostring_fn));

    // .total(options)
    let total_fn = Value::native_function_with_proto_named(
        |this, args, ncx| {
            let obj = this.as_object().ok_or_else(|| VmError::type_error("total called on non-Duration"))?;
            let dur_field_names = ["years","months","weeks","days","hours","minutes","seconds","milliseconds","microseconds","nanoseconds"];
            let mut vals = [0f64; 10];
            for (i, field) in dur_field_names.iter().enumerate() {
                vals[i] = obj.get(&PropertyKey::string(field)).and_then(|v| v.as_number()).unwrap_or(0.0);
            }
            let [years, months, _weeks, days, hours, minutes, seconds, milliseconds, microseconds, nanoseconds] = vals;

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
                "month" | "months" => months + years * 12.0,
                "year" | "years" => years + months / 12.0,
                _ => return Err(VmError::range_error(format!("{} is not a valid unit", unit_str))),
            };
            Ok(Value::number(result))
        },
        mm.clone(), fn_proto.clone(), "total", 1,
    );
    proto.define_property(PropertyKey::string("total"), PropertyDescriptor::builtin_method(total_fn));

    // .add(other)
    let add_dur_ctor = ctor_value.clone();
    let add_fn = Value::native_function_with_proto_named(
        move |this, args, ncx| {
            let obj = this.as_object().ok_or_else(|| VmError::type_error("add called on non-Duration"))?;
            let dur_field_names = ["years","months","weeks","days","hours","minutes","seconds","milliseconds","microseconds","nanoseconds"];
            let mut this_vals = [0f64; 10];
            for (i, field) in dur_field_names.iter().enumerate() {
                this_vals[i] = obj.get(&PropertyKey::string(field)).and_then(|v| v.as_number()).unwrap_or(0.0);
            }
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
    proto.define_property(PropertyKey::string("add"), PropertyDescriptor::builtin_method(add_fn));

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
