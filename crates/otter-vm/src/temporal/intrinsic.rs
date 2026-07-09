//! `Temporal` namespace bootstrap driver.

#![allow(missing_docs)]

use crate::bootstrap::BootstrapFeatures;
use crate::intrinsic_install::BuiltinIntrinsic;
use crate::js_surface::{Attr, JsSurfaceError, MethodSpec, NamespaceBuilder, NamespaceSpec};
use crate::native_function::NativeCall;
use crate::object::{self, JsObject};
use crate::rooting::RootScopeExt;
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
        let mut temporal_value = Value::object(temporal);
        let mut temporal_scope = otter_gc::RootScope::new(heap);
        // SAFETY: `temporal_value` is declared before the scope and remains the
        // single canonical handle while the nested class installers allocate.
        unsafe { temporal_scope.add_value(&mut temporal_value) };
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
        let mut temporal = temporal_value
            .as_object()
            .expect("Temporal namespace stays rooted during bootstrap");
        object::set(&mut temporal, heap, NOW_SPEC.name, Value::object(now));
        Ok(())
    }

    fn install_well_knowns(
        heap: &mut otter_gc::GcHeap,
        global: JsObject,
        well_known: &crate::symbol::WellKnownSymbols,
    ) -> Result<(), JsSurfaceError> {
        install_temporal_well_knowns(heap, global, well_known)
    }
}

/// Install `@@toStringTag` on the `Temporal` namespace, the
/// `Temporal.Now` namespace, and every `Temporal.<Class>.prototype`.
/// The per-class prototype tags are installed at construction time by
/// each `couch!`'s `string_tag` (fanned out here); the two namespace
/// objects are not `couch!` classes, so their tags are pinned here.
/// Each tag is `{ value: "Temporal.<X>", writable: false, enumerable:
/// false, configurable: true }` per the proposal-temporal spec.
fn install_temporal_well_knowns(
    heap: &mut otter_gc::GcHeap,
    global: JsObject,
    well_known: &crate::symbol::WellKnownSymbols,
) -> Result<(), JsSurfaceError> {
    use crate::intrinsic_install::BuiltinIntrinsic;
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

    crate::temporal::instant::InstantIntrinsic::install_well_knowns(heap, global, well_known)?;
    crate::temporal::duration::DurationIntrinsic::install_well_knowns(heap, global, well_known)?;
    crate::temporal::plain_date::PlainDateIntrinsic::install_well_knowns(heap, global, well_known)?;
    crate::temporal::plain_time::PlainTimeIntrinsic::install_well_knowns(heap, global, well_known)?;
    crate::temporal::plain_date_time::PlainDateTimeIntrinsic::install_well_knowns(
        heap, global, well_known,
    )?;
    crate::temporal::plain_year_month::PlainYearMonthIntrinsic::install_well_knowns(
        heap, global, well_known,
    )?;
    crate::temporal::plain_month_day::PlainMonthDayIntrinsic::install_well_knowns(
        heap, global, well_known,
    )?;
    crate::temporal::zoned_date_time::ZonedDateTimeIntrinsic::install_well_knowns(
        heap, global, well_known,
    )?;
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
