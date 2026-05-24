//! `Temporal` namespace bootstrap.
//!
//! Installs the global `Temporal` object together with its
//! sub-classes — `Instant`, `Duration`, `PlainDate`, `PlainTime`,
//! `PlainDateTime` — as real `NativeFunction` constructors (each
//! `typeof === "function"`) plus the `Now` namespace object.
//!
//! The five class constructors are declared via `couch!` with
//! `install_on = temporal_host`, so the per-class install bodies
//! bind their constructor on the `Temporal` namespace object
//! instead of on `globalThis`. `TemporalIntrinsic` is the single
//! `BOOTSTRAP_ENTRIES` row; it allocates the namespace + `Now`
//! sub-namespace and then drives each per-class couch!-generated
//! install fn in declaration order.
//!
//! Direct construction (`new Temporal.Instant(epochNanoseconds)`,
//! `new Temporal.PlainDate(y, m, d)`, etc.) is implemented per
//! proposal-temporal §7.1.1, §8.1.1, §3.1.1, §4.1.1, §5.1.1.
//! Each class has its own ctor body that pre-checks `new.target`
//! and coerces the positional argument shape; calling the
//! constructor without `new` throws `TypeError`.
//!
//! # See also
//! - <https://tc39.es/proposal-temporal/>

use crate::bootstrap::BootstrapFeatures;
use crate::intrinsic_install::BuiltinIntrinsic;
use crate::js_surface::{Attr, JsSurfaceError, MethodSpec, NamespaceBuilder, NamespaceSpec};
use crate::native_function::NativeCall;
use crate::object::{self, JsObject};
use crate::temporal::dispatch::{TemporalError, call as call_static};
use crate::temporal::{duration, instant, plain_date, plain_date_time, plain_time};
use crate::{NativeCtx, NativeError, Value};

// ---------------------------------------------------------------
// Per-class constructors — couch!-driven, all nested under Temporal.
// ---------------------------------------------------------------

/// Resolve the `Temporal` namespace object that the per-class
/// couch! installers bind on. `TemporalIntrinsic` allocates it
/// first; the per-class installers are private (not in
/// `BOOTSTRAP_ENTRIES`) so this lookup is always satisfied at
/// callee time.
fn temporal_host(global: JsObject, heap: &mut otter_gc::GcHeap) -> JsObject {
    object::get(global, heap, "Temporal")
        .and_then(|v| v.as_object())
        .expect("Temporal namespace must be installed before class constructors")
}

otter_macros::couch! {
    name = "Instant",
    feature = CORE,
    intrinsic = InstantIntrinsic,
    constructor = (length = 1, call = instant::construct),
    statics = {
        "from"                  / 1 => native_instant_from,
        "fromEpochMilliseconds" / 1 => native_instant_from_epoch_ms,
        "compare"               / 2 => native_instant_compare,
    },
    install_on = temporal_host,
}

otter_macros::couch! {
    name = "Duration",
    feature = CORE,
    intrinsic = DurationIntrinsic,
    constructor = (length = 0, call = duration::construct),
    statics = {
        "from"    / 1 => native_duration_from,
        "compare" / 2 => native_duration_compare,
    },
    install_on = temporal_host,
}

otter_macros::couch! {
    name = "PlainDate",
    feature = CORE,
    intrinsic = PlainDateIntrinsic,
    constructor = (length = 3, call = plain_date::construct),
    statics = {
        "from"    / 1 => native_plain_date_from,
        "compare" / 2 => native_plain_date_compare,
    },
    install_on = temporal_host,
}

otter_macros::couch! {
    name = "PlainTime",
    feature = CORE,
    intrinsic = PlainTimeIntrinsic,
    constructor = (length = 0, call = plain_time::construct),
    statics = {
        "from"    / 1 => native_plain_time_from,
        "compare" / 2 => native_plain_time_compare,
    },
    install_on = temporal_host,
}

otter_macros::couch! {
    name = "PlainDateTime",
    feature = CORE,
    intrinsic = PlainDateTimeIntrinsic,
    constructor = (length = 3, call = plain_date_time::construct),
    statics = {
        "from"    / 1 => native_plain_date_time_from,
        "compare" / 2 => native_plain_date_time_compare,
    },
    install_on = temporal_host,
}

// ---------------------------------------------------------------
// `Temporal` namespace + `Temporal.Now` sub-namespace.
// ---------------------------------------------------------------

/// `BuiltinIntrinsic` adapter that installs the real `Temporal`
/// namespace and drives the per-class couch! installers in order.
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

        // Per-class installers — each binds itself on the Temporal
        // namespace via `install_on = temporal_host`.
        InstantIntrinsic::install(heap, global)?;
        DurationIntrinsic::install(heap, global)?;
        PlainDateIntrinsic::install(heap, global)?;
        PlainTimeIntrinsic::install(heap, global)?;
        PlainDateTimeIntrinsic::install(heap, global)?;

        // `Temporal.Now` is a namespace object per spec, not a
        // constructor — keep it as a plain object with method specs.
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
        method("instant", 0, native_now_instant),
        method("plainDateTimeISO", 0, native_now_plain_date_time_iso),
        method("plainDateISO", 0, native_now_plain_date_iso),
        method("plainTimeISO", 0, native_now_plain_time_iso),
    ],
    accessors: &[],
    constants: &[],
    attrs: Attr::builtin_function(),
};

// ---------------------------------------------------------------
// Native function bodies — each delegates to
// `temporal::dispatch::call`.
// ---------------------------------------------------------------

fn temporal_native(
    ctx: &mut NativeCtx<'_>,
    class: otter_bytecode::method_id::TemporalClassId,
    method: otter_bytecode::method_id::TemporalMethod,
    args: &[Value],
) -> Result<Value, NativeError> {
    call_static(ctx.heap_mut(), class, method, args).map_err(temporal_native_error)
}

fn temporal_native_error(err: TemporalError) -> NativeError {
    match err {
        TemporalError::BadArgument {
            class,
            method,
            index,
            reason,
        } => NativeError::TypeError {
            name: "Temporal",
            reason: format!("Temporal.{class}.{method}: argument {index} {reason}"),
        },
        TemporalError::Engine {
            class,
            method,
            message,
            kind,
        } => {
            let reason = format!("Temporal.{class}.{method}: {message}");
            use temporal_rs::error::ErrorKind;
            match kind {
                ErrorKind::Range => NativeError::RangeError {
                    name: "Temporal",
                    reason,
                },
                ErrorKind::Type => NativeError::TypeError {
                    name: "Temporal",
                    reason,
                },
                ErrorKind::Syntax => NativeError::SyntaxError {
                    name: "Temporal",
                    reason,
                },
                _ => NativeError::TypeError {
                    name: "Temporal",
                    reason,
                },
            }
        }
        TemporalError::UnknownMember { class, method } => NativeError::TypeError {
            name: "Temporal",
            reason: format!("Temporal.{class}.{method} is not defined"),
        },
        TemporalError::OutOfMemory { .. } => NativeError::TypeError {
            name: "Temporal",
            reason: "out of memory".to_string(),
        },
    }
}

macro_rules! temporal_native {
    ($fn_name:ident, $class:ident, $method:ident) => {
        fn $fn_name(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
            temporal_native(
                ctx,
                otter_bytecode::method_id::TemporalClassId::$class,
                otter_bytecode::method_id::TemporalMethod::$method,
                args,
            )
        }
    };
}

temporal_native!(native_instant_from, Instant, From);
temporal_native!(native_instant_from_epoch_ms, Instant, FromEpochMilliseconds);
temporal_native!(native_instant_compare, Instant, Compare);
temporal_native!(native_duration_from, Duration, From);
temporal_native!(native_duration_compare, Duration, Compare);
temporal_native!(native_plain_date_from, PlainDate, From);
temporal_native!(native_plain_date_compare, PlainDate, Compare);
temporal_native!(native_plain_time_from, PlainTime, From);
temporal_native!(native_plain_time_compare, PlainTime, Compare);
temporal_native!(native_plain_date_time_from, PlainDateTime, From);
temporal_native!(native_plain_date_time_compare, PlainDateTime, Compare);
temporal_native!(native_now_instant, Now, NowInstant);
temporal_native!(native_now_plain_date_time_iso, Now, NowPlainDateTimeISO);
temporal_native!(native_now_plain_date_iso, Now, NowPlainDateISO);
temporal_native!(native_now_plain_time_iso, Now, NowPlainTimeISO);
