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
        method("plainDateTimeISO", 0, crate::temporal::now::plain_date_time_iso),
        method("plainDateISO", 0, crate::temporal::now::plain_date_iso),
        method("plainTimeISO", 0, crate::temporal::now::plain_time_iso),
    ],
    accessors: &[],
    constants: &[],
    attrs: Attr::builtin_function(),
};
