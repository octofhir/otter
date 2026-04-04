//! Temporal namespace and sub-type intrinsics.
//!
//! Implements the Temporal proposal (Stage 4):
//! <https://tc39.es/proposal-temporal/>
//!
//! The `Temporal` global is a plain namespace object (like `Math` / `JSON`).
//! Each Temporal type (Instant, Duration, PlainDate, …) is installed as a
//! property of the `Temporal` namespace, not as a direct global.

pub mod duration;
pub mod helpers;
pub mod instant;
pub mod now;
pub mod payload;
pub mod plain_date;
pub mod plain_date_time;
pub mod plain_month_day;
pub mod plain_time;
pub mod plain_year_month;
pub mod zoned_date_time;

use crate::builders::ClassBuilder;
use crate::object::{ObjectHandle, PropertyAttributes, PropertyValue};
use crate::value::RegisterValue;

use super::install::{IntrinsicInstallContext, IntrinsicInstaller, install_class_plan};
use super::{IntrinsicsError, VmIntrinsics, WellKnownSymbol};

pub(super) static TEMPORAL_INTRINSIC: TemporalIntrinsic = TemporalIntrinsic;

pub(super) struct TemporalIntrinsic;

impl IntrinsicInstaller for TemporalIntrinsic {
    fn init(
        &self,
        intrinsics: &mut VmIntrinsics,
        cx: &mut IntrinsicInstallContext<'_>,
    ) -> Result<(), IntrinsicsError> {
        // ── Temporal.Instant ────────────────────────────────────────
        install_temporal_class(
            intrinsics.temporal_instant_prototype,
            &mut intrinsics.temporal_instant_constructor,
            &instant::instant_class_descriptor(),
            intrinsics.function_prototype,
            cx,
        )?;

        // ── Temporal.Duration ───────────────────────────────────────
        install_temporal_class(
            intrinsics.temporal_duration_prototype,
            &mut intrinsics.temporal_duration_constructor,
            &duration::duration_class_descriptor(),
            intrinsics.function_prototype,
            cx,
        )?;

        // ── Temporal.PlainDate ──────────────────────────────────────
        install_temporal_class(
            intrinsics.temporal_plain_date_prototype,
            &mut intrinsics.temporal_plain_date_constructor,
            &plain_date::plain_date_class_descriptor(),
            intrinsics.function_prototype,
            cx,
        )?;

        // ── Temporal.PlainTime ─────────────────────────────────────
        install_temporal_class(
            intrinsics.temporal_plain_time_prototype,
            &mut intrinsics.temporal_plain_time_constructor,
            &plain_time::plain_time_class_descriptor(),
            intrinsics.function_prototype,
            cx,
        )?;

        // ── Temporal.PlainDateTime ─────────────────────────────────
        install_temporal_class(
            intrinsics.temporal_plain_date_time_prototype,
            &mut intrinsics.temporal_plain_date_time_constructor,
            &plain_date_time::plain_date_time_class_descriptor(),
            intrinsics.function_prototype,
            cx,
        )?;

        // ── Temporal.PlainYearMonth ────────────────────────────────
        install_temporal_class(
            intrinsics.temporal_plain_year_month_prototype,
            &mut intrinsics.temporal_plain_year_month_constructor,
            &plain_year_month::plain_year_month_class_descriptor(),
            intrinsics.function_prototype,
            cx,
        )?;

        // ── Temporal.PlainMonthDay ─────────────────────────────────
        install_temporal_class(
            intrinsics.temporal_plain_month_day_prototype,
            &mut intrinsics.temporal_plain_month_day_constructor,
            &plain_month_day::plain_month_day_class_descriptor(),
            intrinsics.function_prototype,
            cx,
        )?;

        // ── Temporal.ZonedDateTime ─────────────────────────────────
        install_temporal_class(
            intrinsics.temporal_zoned_date_time_prototype,
            &mut intrinsics.temporal_zoned_date_time_constructor,
            &zoned_date_time::zoned_date_time_class_descriptor(),
            intrinsics.function_prototype,
            cx,
        )?;

        // ── @@toStringTag on Temporal namespace ─────────────────────
        let tag_symbol = cx
            .property_names
            .intern_symbol(WellKnownSymbol::ToStringTag.stable_id());
        let tag_str = cx.heap.alloc_string("Temporal");
        cx.heap.define_own_property(
            intrinsics.temporal_namespace,
            tag_symbol,
            PropertyValue::data_with_attrs(
                RegisterValue::from_object_handle(tag_str.0),
                PropertyAttributes::from_flags(false, false, true),
            ),
        )?;

        // ── Install Temporal.Now namespace ───────────────────────────
        now::install_temporal_now(intrinsics, cx)?;

        Ok(())
    }

    fn install_on_global(
        &self,
        intrinsics: &VmIntrinsics,
        cx: &mut IntrinsicInstallContext<'_>,
    ) -> Result<(), IntrinsicsError> {
        // Install `Temporal` namespace on global.
        cx.install_global_value(
            intrinsics,
            "Temporal",
            RegisterValue::from_object_handle(intrinsics.temporal_namespace.0),
        )?;

        // Install constructors as properties of the Temporal namespace.
        install_on_namespace(
            intrinsics.temporal_namespace,
            "Instant",
            intrinsics.temporal_instant_constructor,
            cx,
        )?;
        install_on_namespace(
            intrinsics.temporal_namespace,
            "Duration",
            intrinsics.temporal_duration_constructor,
            cx,
        )?;
        install_on_namespace(
            intrinsics.temporal_namespace,
            "PlainDate",
            intrinsics.temporal_plain_date_constructor,
            cx,
        )?;
        install_on_namespace(
            intrinsics.temporal_namespace,
            "PlainTime",
            intrinsics.temporal_plain_time_constructor,
            cx,
        )?;
        install_on_namespace(
            intrinsics.temporal_namespace,
            "PlainDateTime",
            intrinsics.temporal_plain_date_time_constructor,
            cx,
        )?;
        install_on_namespace(
            intrinsics.temporal_namespace,
            "PlainYearMonth",
            intrinsics.temporal_plain_year_month_constructor,
            cx,
        )?;
        install_on_namespace(
            intrinsics.temporal_namespace,
            "PlainMonthDay",
            intrinsics.temporal_plain_month_day_constructor,
            cx,
        )?;
        install_on_namespace(
            intrinsics.temporal_namespace,
            "ZonedDateTime",
            intrinsics.temporal_zoned_date_time_constructor,
            cx,
        )?;

        Ok(())
    }
}

// ── Shared installation helpers ─────────────────────────────────────

/// Installs a Temporal class following the same pattern as ArrayBuffer/DataView:
/// register the constructor as a proper host function, then install members.
///
/// Returns the **actual** constructor handle (which may differ from the
/// pre-allocated placeholder if the constructor was re-allocated as a host
/// function).
fn install_temporal_class(
    prototype: ObjectHandle,
    constructor: &mut ObjectHandle,
    descriptor: &crate::descriptors::JsClassDescriptor,
    function_prototype: ObjectHandle,
    cx: &mut IntrinsicInstallContext<'_>,
) -> Result<(), IntrinsicsError> {
    let plan = ClassBuilder::from_descriptor(descriptor)
        .expect("Temporal class descriptor should normalize")
        .build();

    if let Some(ctor_desc) = plan.constructor() {
        let host_id = cx.native_functions.register(ctor_desc.clone());
        *constructor = cx.alloc_intrinsic_host_function(host_id, function_prototype)?;
    }

    install_class_plan(prototype, *constructor, &plan, function_prototype, cx)?;
    Ok(())
}

fn install_on_namespace(
    namespace: ObjectHandle,
    name: &str,
    value_handle: ObjectHandle,
    cx: &mut IntrinsicInstallContext<'_>,
) -> Result<(), IntrinsicsError> {
    let prop = cx.property_names.intern(name);
    cx.heap.define_own_property(
        namespace,
        prop,
        PropertyValue::data_with_attrs(
            RegisterValue::from_object_handle(value_handle.0),
            PropertyAttributes::from_flags(true, false, true),
        ),
    )?;
    Ok(())
}
