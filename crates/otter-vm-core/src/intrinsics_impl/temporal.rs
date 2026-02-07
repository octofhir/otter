//! Temporal namespace initialization
//!
//! Creates the Temporal global namespace with all constructors:
//! - Temporal.Now
//! - Temporal.Instant
//! - Temporal.PlainDate, PlainTime, PlainDateTime
//! - Temporal.PlainYearMonth, PlainMonthDay
//! - Temporal.ZonedDateTime
//! - Temporal.Duration

use crate::gc::GcRef;
use crate::object::{JsObject, PropertyKey};
use crate::value::Value;
use crate::memory::MemoryManager;
use std::sync::Arc;

/// Create and install Temporal namespace on global object
///
/// This function expects that all __Temporal_* ops have already been registered as globals.
/// It creates namespace objects and wires the ops as constructors and static methods.
pub fn install_temporal_namespace(
    global: GcRef<JsObject>,
    mm: &Arc<MemoryManager>,
) {
    // Create main Temporal namespace object
    let temporal_obj = GcRef::new(JsObject::new(Value::null(), mm.clone()));

    // ====================================================================
    // Temporal.Now
    // ====================================================================
    let temporal_now = GcRef::new(JsObject::new(Value::null(), mm.clone()));

    if let Some(instant_fn) = global.get(&PropertyKey::string("__Temporal_Now_instant")) {
        let _ = temporal_now.set(PropertyKey::string("instant"), instant_fn);
    }
    if let Some(tz_fn) = global.get(&PropertyKey::string("__Temporal_Now_timeZoneId")) {
        let _ = temporal_now.set(PropertyKey::string("timeZoneId"), tz_fn);
    }
    if let Some(zdt_fn) = global.get(&PropertyKey::string("__Temporal_Now_zonedDateTimeISO")) {
        let _ = temporal_now.set(PropertyKey::string("zonedDateTimeISO"), zdt_fn);
    }
    if let Some(pdt_fn) = global.get(&PropertyKey::string("__Temporal_Now_plainDateTimeISO")) {
        let _ = temporal_now.set(PropertyKey::string("plainDateTimeISO"), pdt_fn);
    }
    if let Some(pd_fn) = global.get(&PropertyKey::string("__Temporal_Now_plainDateISO")) {
        let _ = temporal_now.set(PropertyKey::string("plainDateISO"), pd_fn);
    }
    if let Some(pt_fn) = global.get(&PropertyKey::string("__Temporal_Now_plainTimeISO")) {
        let _ = temporal_now.set(PropertyKey::string("plainTimeISO"), pt_fn);
    }

    let _ = temporal_obj.set(PropertyKey::string("Now"), Value::object(temporal_now));

    // ====================================================================
    // Temporal.Instant
    // ====================================================================
    let temporal_instant = GcRef::new(JsObject::new(Value::null(), mm.clone()));
    if let Some(from_fn) = global.get(&PropertyKey::string("__Temporal_Instant_from")) {
        let _ = temporal_instant.set(PropertyKey::string("from"), from_fn);
    }
    if let Some(fn_val) = global.get(&PropertyKey::string("__Temporal_Instant_epochSeconds")) {
        let _ = temporal_instant.set(PropertyKey::string("epochSeconds"), fn_val);
    }
    let _ = temporal_obj.set(PropertyKey::string("Instant"), Value::object(temporal_instant));

    // ====================================================================
    // Temporal.PlainDate
    // ====================================================================
    let temporal_plain_date = GcRef::new(JsObject::new(Value::null(), mm.clone()));
    if let Some(from_fn) = global.get(&PropertyKey::string("__Temporal_PlainDate_from")) {
        let _ = temporal_plain_date.set(PropertyKey::string("from"), from_fn);
    }
    if let Some(cmp_fn) = global.get(&PropertyKey::string("__Temporal_PlainDate_compare")) {
        let _ = temporal_plain_date.set(PropertyKey::string("compare"), cmp_fn);
    }
    let _ = temporal_obj.set(PropertyKey::string("PlainDate"), Value::object(temporal_plain_date));

    // ====================================================================
    // Temporal.PlainTime
    // ====================================================================
    let temporal_plain_time = GcRef::new(JsObject::new(Value::null(), mm.clone()));
    if let Some(from_fn) = global.get(&PropertyKey::string("__Temporal_PlainTime_from")) {
        let _ = temporal_plain_time.set(PropertyKey::string("from"), from_fn);
    }
    if let Some(cmp_fn) = global.get(&PropertyKey::string("__Temporal_PlainTime_compare")) {
        let _ = temporal_plain_time.set(PropertyKey::string("compare"), cmp_fn);
    }
    let _ = temporal_obj.set(PropertyKey::string("PlainTime"), Value::object(temporal_plain_time));

    // ====================================================================
    // Temporal.PlainDateTime
    // ====================================================================
    let temporal_plain_date_time = GcRef::new(JsObject::new(Value::null(), mm.clone()));
    if let Some(from_fn) = global.get(&PropertyKey::string("__Temporal_PlainDateTime_from")) {
        let _ = temporal_plain_date_time.set(PropertyKey::string("from"), from_fn);
    }
    if let Some(cmp_fn) = global.get(&PropertyKey::string("__Temporal_PlainDateTime_compare")) {
        let _ = temporal_plain_date_time.set(PropertyKey::string("compare"), cmp_fn);
    }
    let _ = temporal_obj.set(PropertyKey::string("PlainDateTime"), Value::object(temporal_plain_date_time));

    // ====================================================================
    // Temporal.PlainYearMonth
    // ====================================================================
    let temporal_plain_year_month = GcRef::new(JsObject::new(Value::null(), mm.clone()));
    if let Some(from_fn) = global.get(&PropertyKey::string("__Temporal_PlainYearMonth_from")) {
        let _ = temporal_plain_year_month.set(PropertyKey::string("from"), from_fn);
    }
    let _ = temporal_obj.set(PropertyKey::string("PlainYearMonth"), Value::object(temporal_plain_year_month));

    // ====================================================================
    // Temporal.PlainMonthDay
    // ====================================================================
    let temporal_plain_month_day = GcRef::new(JsObject::new(Value::null(), mm.clone()));
    if let Some(from_fn) = global.get(&PropertyKey::string("__Temporal_PlainMonthDay_from")) {
        let _ = temporal_plain_month_day.set(PropertyKey::string("from"), from_fn);
    }
    let _ = temporal_obj.set(PropertyKey::string("PlainMonthDay"), Value::object(temporal_plain_month_day));

    // ====================================================================
    // Temporal.ZonedDateTime
    // ====================================================================
    let temporal_zoned_date_time = GcRef::new(JsObject::new(Value::null(), mm.clone()));
    if let Some(from_fn) = global.get(&PropertyKey::string("__Temporal_ZonedDateTime_from")) {
        let _ = temporal_zoned_date_time.set(PropertyKey::string("from"), from_fn);
    }
    if let Some(cmp_fn) = global.get(&PropertyKey::string("__Temporal_ZonedDateTime_compare")) {
        let _ = temporal_zoned_date_time.set(PropertyKey::string("compare"), cmp_fn);
    }
    let _ = temporal_obj.set(PropertyKey::string("ZonedDateTime"), Value::object(temporal_zoned_date_time));

    // ====================================================================
    // Temporal.Duration
    // ====================================================================
    let temporal_duration = GcRef::new(JsObject::new(Value::null(), mm.clone()));
    if let Some(from_fn) = global.get(&PropertyKey::string("__Temporal_Duration_from")) {
        let _ = temporal_duration.set(PropertyKey::string("from"), from_fn);
    }
    if let Some(cmp_fn) = global.get(&PropertyKey::string("__Temporal_Duration_compare")) {
        let _ = temporal_duration.set(PropertyKey::string("compare"), cmp_fn);
    }
    let _ = temporal_obj.set(PropertyKey::string("Duration"), Value::object(temporal_duration));

    // Install Temporal on global
    let _ = global.set(PropertyKey::string("Temporal"), Value::object(temporal_obj));
}
