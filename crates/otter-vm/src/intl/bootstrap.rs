//! Real `Intl.<Kind>` constructors + prototype method installation.
//!
//! Each `Intl.*` instance is a [`crate::Value::Intl`] exotic with no
//! own-property storage. Method calls resolve through §7.3.11
//! `GetMethod` + §7.3.14 `Call`: `ordinary_get_value` walks the
//! instance's kind prototype (installed here on
//! `Intl.<Kind>.prototype`) to that kind's own [`NativeCtx`] native.
//!
//! # Contents
//! - [`intl_host`] — `install_on` resolver returning the `Intl`
//!   namespace object the per-kind constructors bind on.
//! - [`install`] — builds the `Intl` namespace then installs the eight
//!   `couch!`-generated constructors, each prototype method pointing at
//!   that kind's own [`NativeCtx`] native.
//!
//! # Invariants
//! - The `Intl` namespace must be installed before any
//!   `Intl.<Kind>` constructor, because each constructor's `couch!`
//!   `install_on` resolver reads it back off `globalThis`.
//! - `Intl.Collator` / `Intl.NumberFormat` / `Intl.DateTimeFormat`
//!   are callable without `new` (legacy normative-optional, ECMA-402
//!   §10/§11/§12); the remaining five throw a `TypeError` when
//!   `NewTarget` is `undefined`.
//!
//! # See also
//! - <https://tc39.es/ecma402/>

use crate::intl::payload::IntlKind;
use crate::js_surface::{Attr, JsSurfaceError, NamespaceBuilder, NamespaceSpec};
use crate::object::{self, JsObject};
use crate::{NativeCtx, NativeError, Value, intl};

/// `install_on` resolver for the per-kind constructors: returns the
/// `Intl` namespace object the constructor binds on.
pub fn intl_host(global: JsObject, heap: &mut otter_gc::GcHeap) -> JsObject {
    object::get(global, heap, "Intl")
        .and_then(|v| v.as_object())
        .expect("Intl namespace must be installed before Intl.<Kind> constructors")
}

/// Install the `Intl` namespace and the eight `Intl.<Kind>`
/// constructors with their prototype methods.
pub fn install(heap: &mut otter_gc::GcHeap, global: JsObject) -> Result<(), JsSurfaceError> {
    use crate::intrinsic_install::BuiltinIntrinsic;

    let global_root = Value::object(global);
    let intl = NamespaceBuilder::from_spec_with_value_roots(heap, &INTL_SPEC, vec![global_root])?
        .build()?;
    crate::bootstrap::define_global_value(global, heap, "Intl", Value::object(intl));

    CollatorIntrinsic::install(heap, global)?;
    NumberFormatIntrinsic::install(heap, global)?;
    DateTimeFormatIntrinsic::install(heap, global)?;
    PluralRulesIntrinsic::install(heap, global)?;
    RelativeTimeFormatIntrinsic::install(heap, global)?;
    ListFormatIntrinsic::install(heap, global)?;
    DisplayNamesIntrinsic::install(heap, global)?;
    SegmenterIntrinsic::install(heap, global)?;
    LocaleIntrinsic::install(heap, global)?;
    DurationFormatIntrinsic::install(heap, global)?;
    Ok(())
}

/// Install the `@@toStringTag` on every `Intl.<Class>.prototype` at
/// construction time (each `couch!` carries a `string_tag`). Fanned out
/// by [`crate::intrinsics::placeholders::IntlIntrinsic::install_well_knowns`]
/// because the per-class intrinsics are not standalone bootstrap
/// entries.
pub fn install_well_knowns(
    heap: &mut otter_gc::GcHeap,
    global: JsObject,
    well_known: &crate::symbol::WellKnownSymbols,
) -> Result<(), JsSurfaceError> {
    use crate::intrinsic_install::BuiltinIntrinsic;

    CollatorIntrinsic::install_well_knowns(heap, global, well_known)?;
    NumberFormatIntrinsic::install_well_knowns(heap, global, well_known)?;
    DateTimeFormatIntrinsic::install_well_knowns(heap, global, well_known)?;
    PluralRulesIntrinsic::install_well_knowns(heap, global, well_known)?;
    RelativeTimeFormatIntrinsic::install_well_knowns(heap, global, well_known)?;
    ListFormatIntrinsic::install_well_knowns(heap, global, well_known)?;
    DisplayNamesIntrinsic::install_well_knowns(heap, global, well_known)?;
    SegmenterIntrinsic::install_well_knowns(heap, global, well_known)?;
    LocaleIntrinsic::install_well_knowns(heap, global, well_known)?;
    DurationFormatIntrinsic::install_well_knowns(heap, global, well_known)?;
    crate::intl::namespace::install_namespace_well_knowns(heap, global, well_known)?;
    Ok(())
}

const INTL_SPEC: NamespaceSpec = NamespaceSpec {
    name: "Intl",
    methods: crate::intl::namespace::INTL_NAMESPACE_METHODS,
    accessors: &[],
    constants: &[],
    attrs: Attr::global_binding(),
};

/// `Intl.<Kind>(...)` / `new Intl.<Kind>(...)` shared body. The
/// literal `new Intl.<Kind>(...)` shape lowers to `Op::NewIntl` in the
/// compiler; this path serves bare calls and `Reflect.construct`.
fn intl_construct(
    kind: IntlKind,
    requires_new: bool,
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    let class = kind.class_name();
    if requires_new && !ctx.is_construct_call() {
        return Err(NativeError::TypeError {
            name: class,
            reason: "constructor requires 'new'".to_string(),
        });
    }
    let locale = args.first().copied().unwrap_or_else(Value::undefined);
    let options = args.get(1).copied().unwrap_or_else(Value::undefined);
    intl::construct(class, &locale, &options, ctx.heap_mut()).map_err(|err| match err {
        intl::IntlError::Engine { message, .. } => NativeError::Thrown {
            name: class,
            message,
        },
        intl::IntlError::Range { message } => NativeError::RangeError {
            name: class,
            reason: message,
        },
        intl::IntlError::OutOfMemory { .. } => NativeError::TypeError {
            name: class,
            reason: "out of memory".to_string(),
        },
        other => NativeError::TypeError {
            name: class,
            reason: other.to_string(),
        },
    })
}

fn collator_ctor(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    intl_construct(IntlKind::Collator, false, ctx, args)
}
fn number_format_ctor(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    intl_construct(IntlKind::NumberFormat, false, ctx, args)
}
fn date_time_format_ctor(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    intl_construct(IntlKind::DateTimeFormat, false, ctx, args)
}
fn plural_rules_ctor(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    intl_construct(IntlKind::PluralRules, true, ctx, args)
}
fn relative_time_format_ctor(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    intl_construct(IntlKind::RelativeTimeFormat, true, ctx, args)
}
fn list_format_ctor(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    intl_construct(IntlKind::ListFormat, true, ctx, args)
}
fn display_names_ctor(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    intl_construct(IntlKind::DisplayNames, true, ctx, args)
}
fn segmenter_ctor(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    intl_construct(IntlKind::Segmenter, true, ctx, args)
}

// §10 Collator.
otter_macros::couch! {
    name = "Collator",
    feature = CORE,
    intrinsic = CollatorIntrinsic,
    constructor = (length = 0, call = collator_ctor),
    statics = {
        "supportedLocalesOf" / 1 => crate::intl::supported::supported_locales_of,
    },
    prototype = {
        methods = {
            "compare"         / 2 => crate::intl::collator::collator_compare,
            "resolvedOptions" / 0 => crate::intl::collator::collator_resolved_options,
        },
    },
    install_on = crate::intl::bootstrap::intl_host,
    string_tag = "Intl.Collator",
}

// §11 NumberFormat.
otter_macros::couch! {
    name = "NumberFormat",
    feature = CORE,
    intrinsic = NumberFormatIntrinsic,
    constructor = (length = 0, call = number_format_ctor),
    statics = {
        "supportedLocalesOf" / 1 => crate::intl::supported::supported_locales_of,
    },
    prototype = {
        methods = {
            "formatToParts"        / 1 => crate::intl::number_format::number_format_format_to_parts,
            "formatRange"          / 2 => crate::intl::number_format::number_format_format_range,
            "formatRangeToParts"   / 2 => crate::intl::number_format::number_format_format_range_to_parts,
            "resolvedOptions"      / 0 => crate::intl::number_format::number_format_resolved_options,
        },
        accessors = [
            ("format", get = crate::intl::number_format::number_format_format_getter),
        ],
    },
    install_on = crate::intl::bootstrap::intl_host,
    string_tag = "Intl.NumberFormat",
}

// §12 DateTimeFormat.
otter_macros::couch! {
    name = "DateTimeFormat",
    feature = CORE,
    intrinsic = DateTimeFormatIntrinsic,
    constructor = (length = 0, call = date_time_format_ctor),
    statics = {
        "supportedLocalesOf" / 1 => crate::intl::supported::supported_locales_of,
    },
    prototype = {
        methods = {
            "formatToParts"        / 1 => crate::intl::date_time_format::date_time_format_format_to_parts,
            "formatRange"          / 2 => crate::intl::date_time_format::date_time_format_format_range,
            "formatRangeToParts"   / 2 => crate::intl::date_time_format::date_time_format_format_range_to_parts,
            "resolvedOptions"      / 0 => crate::intl::date_time_format::date_time_format_resolved_options,
        },
        accessors = [
            ("format", get = crate::intl::date_time_format::date_time_format_format_getter),
        ],
    },
    install_on = crate::intl::bootstrap::intl_host,
    string_tag = "Intl.DateTimeFormat",
}

// §16 PluralRules.
otter_macros::couch! {
    name = "PluralRules",
    feature = CORE,
    intrinsic = PluralRulesIntrinsic,
    constructor = (length = 0, call = plural_rules_ctor),
    statics = {
        "supportedLocalesOf" / 1 => crate::intl::supported::supported_locales_of,
    },
    prototype = {
        methods = {
            "select"          / 1 => crate::intl::plural_rules::plural_rules_select,
            "resolvedOptions" / 0 => crate::intl::plural_rules::plural_rules_resolved_options,
        },
    },
    install_on = crate::intl::bootstrap::intl_host,
    string_tag = "Intl.PluralRules",
}

// §18 RelativeTimeFormat.
otter_macros::couch! {
    name = "RelativeTimeFormat",
    feature = CORE,
    intrinsic = RelativeTimeFormatIntrinsic,
    constructor = (length = 0, call = relative_time_format_ctor),
    statics = {
        "supportedLocalesOf" / 1 => crate::intl::supported::supported_locales_of,
    },
    prototype = {
        methods = {
            "format"          / 2 => crate::intl::relative_time_format::relative_time_format_format,
            "formatToParts"   / 2 => crate::intl::relative_time_format::relative_time_format_format_to_parts,
            "resolvedOptions" / 0 => crate::intl::relative_time_format::relative_time_format_resolved_options,
        },
    },
    install_on = crate::intl::bootstrap::intl_host,
    string_tag = "Intl.RelativeTimeFormat",
}

// §13 ListFormat.
otter_macros::couch! {
    name = "ListFormat",
    feature = CORE,
    intrinsic = ListFormatIntrinsic,
    constructor = (length = 0, call = list_format_ctor),
    statics = {
        "supportedLocalesOf" / 1 => crate::intl::supported::supported_locales_of,
    },
    prototype = {
        methods = {
            "format"          / 1 => crate::intl::list_format::list_format_format,
            "formatToParts"   / 1 => crate::intl::list_format::list_format_format_to_parts,
            "resolvedOptions" / 0 => crate::intl::list_format::list_format_resolved_options,
        },
    },
    install_on = crate::intl::bootstrap::intl_host,
    string_tag = "Intl.ListFormat",
}

// §14 DisplayNames.
otter_macros::couch! {
    name = "DisplayNames",
    feature = CORE,
    intrinsic = DisplayNamesIntrinsic,
    constructor = (length = 2, call = display_names_ctor),
    statics = {
        "supportedLocalesOf" / 1 => crate::intl::supported::supported_locales_of,
    },
    prototype = {
        methods = {
            "of"              / 1 => crate::intl::display_names::display_names_of,
            "resolvedOptions" / 0 => crate::intl::display_names::display_names_resolved_options,
        },
    },
    install_on = crate::intl::bootstrap::intl_host,
    string_tag = "Intl.DisplayNames",
}

// §19 Segmenter.
otter_macros::couch! {
    name = "Segmenter",
    feature = CORE,
    intrinsic = SegmenterIntrinsic,
    constructor = (length = 0, call = segmenter_ctor),
    statics = {
        "supportedLocalesOf" / 1 => crate::intl::supported::supported_locales_of,
    },
    prototype = {
        methods = {
            "segment"         / 1 => crate::intl::segmenter::segmenter_segment,
            "resolvedOptions" / 0 => crate::intl::segmenter::segmenter_resolved_options,
        },
    },
    install_on = crate::intl::bootstrap::intl_host,
    string_tag = "Intl.Segmenter",
}

// §1 DurationFormat (Intl DurationFormat proposal).
otter_macros::couch! {
    name = "DurationFormat",
    feature = CORE,
    intrinsic = DurationFormatIntrinsic,
    constructor = (length = 0, call = crate::intl::duration_format::duration_format_ctor),
    statics = {
        "supportedLocalesOf" / 1 => crate::intl::supported::supported_locales_of,
    },
    prototype = {
        methods = {
            "format"          / 1 => crate::intl::duration_format::format,
            "formatToParts"   / 1 => crate::intl::duration_format::format_to_parts,
            "resolvedOptions" / 0 => crate::intl::duration_format::resolved_options,
        },
    },
    install_on = crate::intl::bootstrap::intl_host,
    string_tag = "Intl.DurationFormat",
}

// §14 Locale.
otter_macros::couch! {
    name = "Locale",
    feature = CORE,
    intrinsic = LocaleIntrinsic,
    constructor = (length = 1, call = crate::intl::locale::locale_ctor),
    prototype = {
        methods = {
            "maximize" / 0 => crate::intl::locale::maximize,
            "minimize" / 0 => crate::intl::locale::minimize,
            "toString" / 0 => crate::intl::locale::to_string,
            "getCalendars" / 0 => crate::intl::locale::get_calendars,
            "getCollations" / 0 => crate::intl::locale::get_collations,
            "getHourCycles" / 0 => crate::intl::locale::get_hour_cycles,
            "getNumberingSystems" / 0 => crate::intl::locale::get_numbering_systems,
            "getTimeZones" / 0 => crate::intl::locale::get_time_zones,
            "getTextInfo" / 0 => crate::intl::locale::get_text_info,
            "getWeekInfo" / 0 => crate::intl::locale::get_week_info,
        },
        accessors = [
            ("baseName",        get = crate::intl::locale::get_base_name),
            ("language",        get = crate::intl::locale::get_language),
            ("script",          get = crate::intl::locale::get_script),
            ("region",          get = crate::intl::locale::get_region),
            ("variants",        get = crate::intl::locale::get_variants),
            ("calendar",        get = crate::intl::locale::get_calendar),
            ("firstDayOfWeek",  get = crate::intl::locale::get_first_day_of_week),
            ("collation",       get = crate::intl::locale::get_collation),
            ("hourCycle",       get = crate::intl::locale::get_hour_cycle),
            ("caseFirst",       get = crate::intl::locale::get_case_first),
            ("numeric",         get = crate::intl::locale::get_numeric),
            ("numberingSystem", get = crate::intl::locale::get_numbering_system),
        ],
    },
    install_on = crate::intl::bootstrap::intl_host,
    string_tag = "Intl.Locale",
}
