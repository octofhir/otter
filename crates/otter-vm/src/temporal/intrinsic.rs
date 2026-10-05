//! `Temporal` namespace bootstrap driver.

#![allow(missing_docs)]

use crate::bootstrap::BootstrapFeatures;
use crate::intrinsic_install::BuiltinIntrinsic;
use crate::js_surface::{
    Attr, JsSurfaceError, MethodSpec, NamespaceBuilder, NamespaceSpec, ObjectBuilder,
};
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
        crate::bootstrap::define_global_value(global, heap, Self::NAME, temporal_value)?;

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
        let temporal = temporal_value
            .as_object()
            .expect("Temporal namespace stays rooted during bootstrap");
        // §Temporal.Now is an ordinary object; the `Now` property is
        // {writable, non-enumerable, configurable}. Its
        // %Object.prototype% link installs in `install_well_knowns`,
        // after the Object intrinsics exist.
        if !object::define_own_property(
            temporal,
            heap,
            NOW_SPEC.name,
            crate::object::PropertyDescriptor::data(Value::object(now), true, false, true),
        )? {
            return Err(JsSurfaceError::DefinePropertyFailed("[[BuiltinProperty]]"));
        };
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

    let Some(mut temporal) =
        object::get(global, heap, "Temporal").and_then(|value| value.as_object())
    else {
        return Ok(());
    };
    let mut prototype = match object::get(global, heap, "Object")
        .and_then(|constructor| constructor.as_native_function())
    {
        Some(constructor) => constructor
            .own_property_descriptor(heap, "prototype")?
            .and_then(|descriptor| match descriptor.kind {
                object::DescriptorKind::Data { value } => Some(value),
                object::DescriptorKind::Accessor { .. } => None,
            })
            .unwrap_or_else(Value::null),
        None => Value::null(),
    };
    let mut roots = otter_gc::RootScope::new(heap);
    // SAFETY: these actual slots precede the scope and all allocating owners
    // reload them after collection. The per-realm well-known table is rooted
    // by the bootstrap caller.
    unsafe {
        roots.add_object(&mut temporal);
        roots.add_value(&mut prototype);
    }
    let mut builder = ObjectBuilder::from_object_with_value_roots(
        heap,
        temporal,
        vec![Value::object(global), prototype],
    );
    builder.to_string_tag(well_known, "Temporal")?;
    temporal = builder.build();
    if let Some(prototype) = prototype.as_object()
        && !object::set_prototype(&mut temporal, heap, Some(prototype))?
    {
        return Err(JsSurfaceError::DefinePropertyFailed(
            "Temporal.[[Prototype]]",
        ));
    }
    if let Some(mut now) = object::get(temporal, heap, "Now").and_then(|value| value.as_object()) {
        let mut builder = ObjectBuilder::from_object_with_value_roots(
            heap,
            now,
            vec![Value::object(global), Value::object(temporal), prototype],
        );
        builder.to_string_tag(well_known, "Temporal.Now")?;
        now = builder.build();
        if let Some(prototype) = prototype.as_object()
            && !object::set_prototype(&mut now, heap, Some(prototype))?
        {
            return Err(JsSurfaceError::DefinePropertyFailed(
                "Temporal.Now.[[Prototype]]",
            ));
        }
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
