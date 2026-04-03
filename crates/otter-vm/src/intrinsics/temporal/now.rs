//! Temporal.Now namespace.
//!
//! §2.2 The Temporal.Now Object
//! <https://tc39.es/proposal-temporal/#sec-temporal-now-object>

use crate::descriptors::{NativeFunctionDescriptor, VmNativeCallError, VmNativeFunction};
use crate::object::{PropertyAttributes, PropertyValue};
use crate::value::RegisterValue;

use super::helpers::temporal_err;
use super::payload::{TemporalPayload, construct_temporal};
use super::{IntrinsicsError, VmIntrinsics, install_on_namespace};

use super::super::install::IntrinsicInstallContext;

/// Installs the `Temporal.Now` namespace with its methods.
pub fn install_temporal_now(
    intrinsics: &mut VmIntrinsics,
    cx: &mut IntrinsicInstallContext<'_>,
) -> Result<(), IntrinsicsError> {
    let now_obj = cx.heap.alloc_object();
    cx.heap
        .set_prototype(now_obj, Some(intrinsics.object_prototype()))?;

    let methods: &[(&str, u16, VmNativeFunction)] = &[
        ("instant", 0, now_instant),
        ("timeZoneId", 0, now_time_zone_id),
        ("plainDateTimeISO", 0, now_plain_date_time_iso),
        ("plainDateISO", 0, now_plain_date_iso),
        ("plainTimeISO", 0, now_plain_time_iso),
    ];

    for &(name, arity, func) in methods {
        let desc = NativeFunctionDescriptor::method(name, arity, func);
        let host_id = cx.native_functions.register(desc);
        let fn_handle = cx.heap.alloc_host_function(host_id);
        cx.heap
            .set_prototype(fn_handle, Some(intrinsics.function_prototype()))?;
        let prop = cx.property_names.intern(name);
        cx.heap.define_own_property(
            now_obj,
            prop,
            PropertyValue::data_with_attrs(
                RegisterValue::from_object_handle(fn_handle.0),
                PropertyAttributes::from_flags(true, false, true),
            ),
        )?;
    }

    // Install Now on the Temporal namespace.
    install_on_namespace(intrinsics.temporal_namespace, "Now", now_obj, cx)?;

    Ok(())
}

/// §2.2.1 Temporal.Now.instant ( )
/// <https://tc39.es/proposal-temporal/#sec-temporal.now.instant>
fn now_instant(
    _this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let instant = temporal_rs::Temporal::local_now()
        .instant()
        .map_err(|e| temporal_err(e, runtime))?;
    let proto = runtime.intrinsics().temporal_instant_prototype();
    let handle = construct_temporal(TemporalPayload::Instant(instant), proto, runtime);
    Ok(RegisterValue::from_object_handle(handle.0))
}

/// §2.2.2 Temporal.Now.timeZoneId ( )
/// <https://tc39.es/proposal-temporal/#sec-temporal.now.timezoneid>
fn now_time_zone_id(
    _this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let tz = temporal_rs::Temporal::local_now()
        .time_zone()
        .map_err(|e| temporal_err(e, runtime))?;
    let id = tz.identifier().map_err(|e| temporal_err(e, runtime))?;
    let handle = runtime.alloc_string(id);
    Ok(RegisterValue::from_object_handle(handle.0))
}

/// §2.2.3 Temporal.Now.plainDateTimeISO ( [ timeZone ] )
/// <https://tc39.es/proposal-temporal/#sec-temporal.now.plaindatetimeiso>
fn now_plain_date_time_iso(
    _this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let pdt = temporal_rs::Temporal::local_now()
        .plain_date_time_iso(None)
        .map_err(|e| temporal_err(e, runtime))?;
    let proto = runtime.intrinsics().temporal_plain_date_time_prototype();
    let handle = construct_temporal(TemporalPayload::PlainDateTime(pdt), proto, runtime);
    Ok(RegisterValue::from_object_handle(handle.0))
}

/// §2.2.4 Temporal.Now.plainDateISO ( [ timeZone ] )
/// <https://tc39.es/proposal-temporal/#sec-temporal.now.plaindateiso>
fn now_plain_date_iso(
    _this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let pd = temporal_rs::Temporal::local_now()
        .plain_date_iso(None)
        .map_err(|e| temporal_err(e, runtime))?;
    let proto = runtime.intrinsics().temporal_plain_date_prototype();
    let handle = construct_temporal(TemporalPayload::PlainDate(pd), proto, runtime);
    Ok(RegisterValue::from_object_handle(handle.0))
}

/// §2.2.5 Temporal.Now.plainTimeISO ( [ timeZone ] )
/// <https://tc39.es/proposal-temporal/#sec-temporal.now.plaintimeiso>
fn now_plain_time_iso(
    _this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let pt = temporal_rs::Temporal::local_now()
        .plain_time_iso(None)
        .map_err(|e| temporal_err(e, runtime))?;
    let proto = runtime.intrinsics().temporal_plain_time_prototype();
    let handle = construct_temporal(TemporalPayload::PlainTime(pt), proto, runtime);
    Ok(RegisterValue::from_object_handle(handle.0))
}
