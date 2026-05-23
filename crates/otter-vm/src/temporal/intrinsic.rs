//! `Temporal` namespace bootstrap.
//!
//! Installs the global `Temporal` object together with its
//! sub-namespaces — `Instant`, `Duration`, `PlainDate`, `PlainTime`,
//! `PlainDateTime`, and `Now` — as ordinary JS objects holding the
//! static methods (`from`, `compare`, `fromEpochMilliseconds`, the
//! `Now.<view>()` snapshots) backed by the existing
//! `temporal::dispatch::call` engine.
//!
//! The classes are currently exposed as namespace objects rather
//! than callable constructors: `typeof Temporal.Instant === "object"`
//! and `new Temporal.Instant(...)` is not yet supported. The static
//! methods (`Temporal.Instant.from`, `Temporal.Now.instant`, …)
//! match the spec.
//!
//! # Contents
//! - [`Intrinsic`] — `BuiltinIntrinsic` adapter installed by
//!   bootstrap.
//!
//! # See also
//! - <https://tc39.es/proposal-temporal/>

use crate::bootstrap::{BootstrapFeatures, define_global_value};
use crate::intrinsic_install::BuiltinIntrinsic;
use crate::js_surface::{Attr, JsSurfaceError, MethodSpec, NamespaceBuilder, NamespaceSpec};
use crate::native_function::NativeCall;
use crate::object::JsObject;
use crate::temporal::dispatch::{TemporalError, call as call_static};
use crate::{NativeCtx, NativeError, Value};

/// `BuiltinIntrinsic` adapter that installs the real `Temporal`
/// namespace. Replaces the previous empty placeholder so
/// `Temporal.Now.instant()` and friends resolve through ordinary
/// dynamic dispatch.
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

        // Sub-namespaces: each class hangs off `Temporal` as an
        // ordinary own data property.
        for sub in TEMPORAL_SUBNAMESPACES {
            let ns = NamespaceBuilder::from_spec_with_value_roots(
                heap,
                sub,
                vec![global_root, temporal_value],
            )?
            .build()?;
            crate::object::set(temporal, heap, sub.name, Value::object(ns));
        }

        define_global_value(global, heap, Self::NAME, temporal_value);
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

const TEMPORAL_SUBNAMESPACES: &[&NamespaceSpec] = &[
    &INSTANT_SPEC,
    &DURATION_SPEC,
    &PLAIN_DATE_SPEC,
    &PLAIN_TIME_SPEC,
    &PLAIN_DATE_TIME_SPEC,
    &NOW_SPEC,
];

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

// ----- per-class specs -----

const INSTANT_SPEC: NamespaceSpec = NamespaceSpec {
    name: "Instant",
    methods: &[
        method("from", 1, native_instant_from),
        method("fromEpochMilliseconds", 1, native_instant_from_epoch_ms),
        method("compare", 2, native_instant_compare),
    ],
    accessors: &[],
    constants: &[],
    attrs: Attr::builtin_function(),
};

const DURATION_SPEC: NamespaceSpec = NamespaceSpec {
    name: "Duration",
    methods: &[
        method("from", 1, native_duration_from),
        method("compare", 2, native_duration_compare),
    ],
    accessors: &[],
    constants: &[],
    attrs: Attr::builtin_function(),
};

const PLAIN_DATE_SPEC: NamespaceSpec = NamespaceSpec {
    name: "PlainDate",
    methods: &[
        method("from", 1, native_plain_date_from),
        method("compare", 2, native_plain_date_compare),
    ],
    accessors: &[],
    constants: &[],
    attrs: Attr::builtin_function(),
};

const PLAIN_TIME_SPEC: NamespaceSpec = NamespaceSpec {
    name: "PlainTime",
    methods: &[
        method("from", 1, native_plain_time_from),
        method("compare", 2, native_plain_time_compare),
    ],
    accessors: &[],
    constants: &[],
    attrs: Attr::builtin_function(),
};

const PLAIN_DATE_TIME_SPEC: NamespaceSpec = NamespaceSpec {
    name: "PlainDateTime",
    methods: &[
        method("from", 1, native_plain_date_time_from),
        method("compare", 2, native_plain_date_time_compare),
    ],
    accessors: &[],
    constants: &[],
    attrs: Attr::builtin_function(),
};

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

// ----- native function bodies -----
//
// Each native delegates to `temporal::dispatch::call` with the
// matching `TemporalClassId` / `TemporalMethod`. The dispatcher
// returns either a `Value` or a `TemporalError`; we surface the
// latter as a `NativeError` keyed to the call site.

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
        } => NativeError::RangeError {
            name: "Temporal",
            reason: format!("Temporal.{class}.{method}: {message}"),
        },
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
