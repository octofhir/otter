//! `Temporal` namespace bootstrap driver.

#![allow(missing_docs)]

use crate::bootstrap::BootstrapFeatures;
use crate::intrinsic_install::BuiltinIntrinsic;
use crate::js_surface::{Attr, JsSurfaceError, MethodSpec, NamespaceBuilder, NamespaceSpec};
use crate::native_function::NativeCall;
use crate::object::{self, JsObject};
use crate::{NativeCtx, NativeError, Value};

pub struct Intrinsic;

impl BuiltinIntrinsic for Intrinsic {
    const NAME: &'static str = "Temporal";
    const FEATURE: BootstrapFeatures = BootstrapFeatures::CORE;

    fn install(heap: &mut otter_gc::GcHeap, global: JsObject) -> Result<(), JsSurfaceError> {
        let global_root = Value::object(global);
        let temporal =
            NamespaceBuilder::from_spec_with_value_roots(heap, &TEMPORAL_SPEC, vec![global_root])?
                .build()?;
        let temporal_value = Value::object(temporal);
        crate::bootstrap::define_global_value(global, heap, Self::NAME, temporal_value);

        crate::temporal::instant::InstantIntrinsic::install(heap, global)?;
        crate::temporal::duration::DurationIntrinsic::install(heap, global)?;
        crate::temporal::plain_date::PlainDateIntrinsic::install(heap, global)?;
        crate::temporal::plain_time::PlainTimeIntrinsic::install(heap, global)?;
        crate::temporal::plain_date_time::PlainDateTimeIntrinsic::install(heap, global)?;
        crate::temporal::plain_year_month::PlainYearMonthIntrinsic::install(heap, global)?;
        crate::temporal::plain_month_day::PlainMonthDayIntrinsic::install(heap, global)?;
        crate::temporal::zoned_date_time::ZonedDateTimeIntrinsic::install(heap, global)?;

        let now = NamespaceBuilder::from_spec_with_value_roots(
            heap,
            &NOW_SPEC,
            vec![global_root, temporal_value],
        )?
        .build()?;
        object::set(temporal, heap, NOW_SPEC.name, Value::object(now));
        Ok(())
    }
}

/// Post-bootstrap fixup: install `@@toStringTag` on the `Temporal`
/// namespace, the `Temporal.Now` namespace, and every
/// `Temporal.<Class>.prototype`. Each tag is `{ value: "Temporal.<X>",
/// writable: false, enumerable: false, configurable: true }` per the
/// proposal-temporal spec.
pub fn install_temporal_well_knowns_post_bootstrap(
    heap: &mut otter_gc::GcHeap,
    global: JsObject,
    well_known: &crate::symbol::WellKnownSymbols,
) -> Result<(), JsSurfaceError> {
    use crate::object::PartialPropertyDescriptor;
    use crate::symbol::WellKnown;

    let tag_sym = well_known.get(WellKnown::ToStringTag);

    let install =
        |heap: &mut otter_gc::GcHeap, obj: JsObject, tag: &str| -> Result<(), JsSurfaceError> {
            let value = crate::string::JsString::from_str(tag, heap)
                .map_err(|_| JsSurfaceError::OutOfMemory)?;
            object::define_own_symbol_property_partial(
                obj,
                heap,
                tag_sym,
                PartialPropertyDescriptor {
                    value: Some(Value::string(value)),
                    writable: Some(false),
                    enumerable: Some(false),
                    configurable: Some(true),
                    ..Default::default()
                },
            );
            Ok(())
        };

    let Some(temporal) = object::get(global, heap, "Temporal").and_then(|v| v.as_object()) else {
        return Ok(());
    };
    install(heap, temporal, "Temporal")?;

    if let Some(now) = object::get(temporal, heap, "Now").and_then(|v| v.as_object()) {
        install(heap, now, "Temporal.Now")?;
    }

    for class in [
        "Instant",
        "Duration",
        "PlainDate",
        "PlainTime",
        "PlainDateTime",
        "PlainYearMonth",
        "PlainMonthDay",
        "ZonedDateTime",
    ] {
        let Some(ctor) = object::get(temporal, heap, class).and_then(|v| v.as_native_function())
        else {
            continue;
        };
        let prototype = ctor
            .own_property_descriptor(heap, "prototype")
            .map_err(|_| JsSurfaceError::OutOfMemory)?
            .and_then(|d| match d.kind {
                crate::object::DescriptorKind::Data { value } => value.as_object(),
                _ => None,
            });
        if let Some(prototype) = prototype {
            install(heap, prototype, &format!("Temporal.{class}"))?;
        }
    }
    Ok(())
}

const TEMPORAL_SPEC: NamespaceSpec = NamespaceSpec {
    name: "Temporal",
    methods: &[],
    accessors: &[],
    constants: &[],
    attrs: Attr::global_binding(),
};

const fn method(
    name: &'static str,
    length: u8,
    call: for<'rt> fn(&mut NativeCtx<'rt>, &[Value]) -> Result<Value, NativeError>,
) -> MethodSpec {
    MethodSpec {
        name,
        length,
        attrs: Attr::builtin_function(),
        call: NativeCall::Static(call),
    }
}

const NOW_SPEC: NamespaceSpec = NamespaceSpec {
    name: "Now",
    methods: &[
        method("instant", 0, crate::temporal::now::instant),
        method("timeZoneId", 0, crate::temporal::now::time_zone_id),
        method(
            "zonedDateTimeISO",
            0,
            crate::temporal::now::zoned_date_time_iso,
        ),
        method(
            "plainDateTimeISO",
            0,
            crate::temporal::now::plain_date_time_iso,
        ),
        method("plainDateISO", 0, crate::temporal::now::plain_date_iso),
        method("plainTimeISO", 0, crate::temporal::now::plain_time_iso),
    ],
    accessors: &[],
    constants: &[],
    attrs: Attr::builtin_function(),
};
