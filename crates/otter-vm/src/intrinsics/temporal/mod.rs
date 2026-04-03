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
            intrinsics.temporal_instant_constructor,
            &instant::instant_class_descriptor(),
            intrinsics.function_prototype,
            cx,
        )?;

        // ── Temporal.Duration ───────────────────────────────────────
        install_temporal_class(
            intrinsics.temporal_duration_prototype,
            intrinsics.temporal_duration_constructor,
            &duration::duration_class_descriptor(),
            intrinsics.function_prototype,
            cx,
        )?;

        // ── Temporal.PlainDate ──────────────────────────────────────
        install_temporal_class(
            intrinsics.temporal_plain_date_prototype,
            intrinsics.temporal_plain_date_constructor,
            &plain_date::plain_date_class_descriptor(),
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

        Ok(())
    }
}

// ── Shared installation helpers ─────────────────────────────────────

fn install_temporal_class(
    prototype: ObjectHandle,
    constructor: ObjectHandle,
    descriptor: &crate::descriptors::JsClassDescriptor,
    function_prototype: ObjectHandle,
    cx: &mut IntrinsicInstallContext<'_>,
) -> Result<(), IntrinsicsError> {
    let plan = ClassBuilder::from_descriptor(descriptor)
        .expect("Temporal class descriptor should normalize")
        .build();

    if let Some(ctor_desc) = plan.constructor() {
        let host_id = cx.native_functions.register(ctor_desc.clone());
        cx.heap.define_own_property(
            constructor,
            cx.property_names.intern("length"),
            PropertyValue::data_with_attrs(
                RegisterValue::from_i32(i32::from(ctor_desc.length())),
                PropertyAttributes::from_flags(false, false, true),
            ),
        )?;
        let name_str = cx.heap.alloc_string(ctor_desc.js_name());
        cx.heap.define_own_property(
            constructor,
            cx.property_names.intern("name"),
            PropertyValue::data_with_attrs(
                RegisterValue::from_object_handle(name_str.0),
                PropertyAttributes::from_flags(false, false, true),
            ),
        )?;
        // Store the host function id on the constructor.
        let _ = host_id;
    }

    install_class_plan(prototype, constructor, &plan, function_prototype, cx)?;
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
