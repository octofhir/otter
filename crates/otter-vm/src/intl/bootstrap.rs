//! Real `Intl.<Kind>` constructors + prototype method installation.
//!
//! Each `Intl.*` instance is a [`crate::Value::Intl`] exotic with no
//! own-property storage. Method calls resolve through §7.3.11
//! `GetMethod` + §7.3.14 `Call`: `ordinary_get_value` walks the
//! instance's kind prototype (installed here on
//! `Intl.<Kind>.prototype`) to a real native method that re-enters the
//! per-kind intrinsic implementation.
//!
//! # Contents
//! - [`intl_host`] — `install_on` resolver returning the `Intl`
//!   namespace object the per-kind constructors bind on.
//! - [`install`] — builds the `Intl` namespace then installs the eight
//!   `couch!`-generated constructors.
//! - `native_intl_method` — re-entrant bridge from a JS-visible
//!   prototype method to the per-kind [`crate::intl::lookup_prototype`]
//!   intrinsic table.
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
use crate::intrinsics::{IntrinsicArgs, IntrinsicError};
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
    let intl =
        NamespaceBuilder::from_spec_with_value_roots(heap, &INTL_SPEC, vec![global_root])?.build()?;
    crate::bootstrap::define_global_value(global, heap, "Intl", Value::object(intl));

    CollatorIntrinsic::install(heap, global)?;
    NumberFormatIntrinsic::install(heap, global)?;
    DateTimeFormatIntrinsic::install(heap, global)?;
    PluralRulesIntrinsic::install(heap, global)?;
    RelativeTimeFormatIntrinsic::install(heap, global)?;
    ListFormatIntrinsic::install(heap, global)?;
    DisplayNamesIntrinsic::install(heap, global)?;
    SegmenterIntrinsic::install(heap, global)?;
    Ok(())
}

const INTL_SPEC: NamespaceSpec = NamespaceSpec {
    name: "Intl",
    methods: &[],
    accessors: &[],
    constants: &[],
    attrs: Attr::global_binding(),
};

/// Re-entrant bridge: resolve `name` against the receiver's per-kind
/// intrinsic table (which brand-checks the receiver) and run it with a
/// live heap handle. Mirrors `crate::date::prototype::native_date_method`.
fn native_intl_method(
    name: &'static str,
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    let receiver = *ctx.this_value();
    let entry =
        intl::lookup_prototype(&receiver, ctx.heap(), name).ok_or(NativeError::TypeError {
            name,
            reason: "called on a receiver that is not the expected Intl object".to_string(),
        })?;
    let allocation_roots = ctx.collect_native_roots();
    (entry.impl_fn)(&mut IntrinsicArgs {
        receiver: &receiver,
        args,
        gc_heap: ctx.heap_mut(),
        allocation_roots: allocation_roots.as_slice(),
    })
    .map_err(|err| match err {
        IntrinsicError::OutOfRange { .. } => NativeError::RangeError {
            name,
            reason: err.to_string(),
        },
        _ => NativeError::TypeError {
            name,
            reason: err.to_string(),
        },
    })
}

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

// Prototype-method bridges. One per JS-visible method name; the bridge
// routes by the receiver's own kind via `lookup_prototype`, so a single
// `resolvedOptions` / `format` bridge serves every kind that exposes it.
fn intl_compare(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    native_intl_method("compare", ctx, args)
}
fn intl_format(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    native_intl_method("format", ctx, args)
}
fn intl_format_to_parts(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    native_intl_method("formatToParts", ctx, args)
}
fn intl_resolved_options(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    native_intl_method("resolvedOptions", ctx, args)
}
fn intl_select(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    native_intl_method("select", ctx, args)
}
fn intl_of(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    native_intl_method("of", ctx, args)
}
fn intl_segment(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    native_intl_method("segment", ctx, args)
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
fn relative_time_format_ctor(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
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
    prototype = {
        methods = {
            "compare"         / 2 => intl_compare,
            "resolvedOptions" / 0 => intl_resolved_options,
        },
    },
    install_on = crate::intl::bootstrap::intl_host,
}

// §11 NumberFormat.
otter_macros::couch! {
    name = "NumberFormat",
    feature = CORE,
    intrinsic = NumberFormatIntrinsic,
    constructor = (length = 0, call = number_format_ctor),
    prototype = {
        methods = {
            "format"          / 1 => intl_format,
            "resolvedOptions" / 0 => intl_resolved_options,
        },
    },
    install_on = crate::intl::bootstrap::intl_host,
}

// §12 DateTimeFormat.
otter_macros::couch! {
    name = "DateTimeFormat",
    feature = CORE,
    intrinsic = DateTimeFormatIntrinsic,
    constructor = (length = 0, call = date_time_format_ctor),
    prototype = {
        methods = {
            "format"          / 1 => intl_format,
            "resolvedOptions" / 0 => intl_resolved_options,
        },
    },
    install_on = crate::intl::bootstrap::intl_host,
}

// §16 PluralRules.
otter_macros::couch! {
    name = "PluralRules",
    feature = CORE,
    intrinsic = PluralRulesIntrinsic,
    constructor = (length = 0, call = plural_rules_ctor),
    prototype = {
        methods = {
            "select"          / 1 => intl_select,
            "resolvedOptions" / 0 => intl_resolved_options,
        },
    },
    install_on = crate::intl::bootstrap::intl_host,
}

// §18 RelativeTimeFormat.
otter_macros::couch! {
    name = "RelativeTimeFormat",
    feature = CORE,
    intrinsic = RelativeTimeFormatIntrinsic,
    constructor = (length = 0, call = relative_time_format_ctor),
    prototype = {
        methods = {
            "format"          / 2 => intl_format,
            "formatToParts"   / 2 => intl_format_to_parts,
            "resolvedOptions" / 0 => intl_resolved_options,
        },
    },
    install_on = crate::intl::bootstrap::intl_host,
}

// §13 ListFormat.
otter_macros::couch! {
    name = "ListFormat",
    feature = CORE,
    intrinsic = ListFormatIntrinsic,
    constructor = (length = 0, call = list_format_ctor),
    prototype = {
        methods = {
            "format"          / 1 => intl_format,
            "formatToParts"   / 1 => intl_format_to_parts,
            "resolvedOptions" / 0 => intl_resolved_options,
        },
    },
    install_on = crate::intl::bootstrap::intl_host,
}

// §14 DisplayNames.
otter_macros::couch! {
    name = "DisplayNames",
    feature = CORE,
    intrinsic = DisplayNamesIntrinsic,
    constructor = (length = 2, call = display_names_ctor),
    prototype = {
        methods = {
            "of"              / 1 => intl_of,
            "resolvedOptions" / 0 => intl_resolved_options,
        },
    },
    install_on = crate::intl::bootstrap::intl_host,
}

// §19 Segmenter.
otter_macros::couch! {
    name = "Segmenter",
    feature = CORE,
    intrinsic = SegmenterIntrinsic,
    constructor = (length = 0, call = segmenter_ctor),
    prototype = {
        methods = {
            "segment"         / 1 => intl_segment,
            "resolvedOptions" / 0 => intl_resolved_options,
        },
    },
    install_on = crate::intl::bootstrap::intl_host,
}
