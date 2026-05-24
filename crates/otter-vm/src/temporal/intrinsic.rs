//! `Temporal` namespace bootstrap.
//!
//! Installs the global `Temporal` object together with its
//! sub-classes — `Instant`, `Duration`, `PlainDate`, `PlainTime`,
//! `PlainDateTime` — as real `NativeFunction` constructors (each
//! `typeof === "function"`) plus the `Now` namespace object. Each
//! constructor carries its static methods (`from`, `compare`,
//! `fromEpochMilliseconds`) as own data properties backed by the
//! existing `temporal::dispatch::call` engine.
//!
//! Direct construction (`new Temporal.Instant(...)`) throws a
//! `TypeError` for now — the foundation does not yet thread the
//! `epochNanoseconds` / partial-record argument shapes through the
//! `[[Construct]]` path. Spec-recommended factories (`from`,
//! `Now.instant()`, ...) work end-to-end.
//!
//! # Contents
//! - [`Intrinsic`] — `BuiltinIntrinsic` adapter installed by
//!   bootstrap.
//!
//! # See also
//! - <https://tc39.es/proposal-temporal/>

use crate::bootstrap::{
    BootstrapFeatures, define_global_value, native_constructor_static_with_value_roots,
    native_static_with_value_roots,
};
use crate::intrinsic_install::BuiltinIntrinsic;
use crate::js_surface::{Attr, JsSurfaceError, MethodSpec, NamespaceBuilder, NamespaceSpec};
use crate::native_function::NativeCall;
use crate::object::{self, JsObject, PropertyDescriptor};
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

        for spec in TEMPORAL_CLASSES {
            install_class(heap, global_root, temporal, temporal_value, spec)?;
        }

        // `Temporal.Now` is a namespace object per spec, not a
        // constructor — keep it as a plain object with method specs.
        let now = NamespaceBuilder::from_spec_with_value_roots(
            heap,
            &NOW_SPEC,
            vec![global_root, temporal_value],
        )?
        .build()?;
        object::set(temporal, heap, NOW_SPEC.name, Value::object(now));

        define_global_value(global, heap, Self::NAME, temporal_value);
        Ok(())
    }
}

/// Per-class installer descriptor: the JS-visible class name plus
/// the static methods exposed on the constructor function.
struct TemporalClassSpec {
    name: &'static str,
    methods: &'static [TemporalStatic],
}

struct TemporalStatic {
    name: &'static str,
    length: u8,
    call: crate::native_function::NativeFastFn,
}

const fn temporal_static(
    name: &'static str,
    length: u8,
    call: crate::native_function::NativeFastFn,
) -> TemporalStatic {
    TemporalStatic { name, length, call }
}

const TEMPORAL_CLASSES: &[TemporalClassSpec] = &[
    TemporalClassSpec {
        name: "Instant",
        methods: &[
            temporal_static("from", 1, native_instant_from),
            temporal_static("fromEpochMilliseconds", 1, native_instant_from_epoch_ms),
            temporal_static("compare", 2, native_instant_compare),
        ],
    },
    TemporalClassSpec {
        name: "Duration",
        methods: &[
            temporal_static("from", 1, native_duration_from),
            temporal_static("compare", 2, native_duration_compare),
        ],
    },
    TemporalClassSpec {
        name: "PlainDate",
        methods: &[
            temporal_static("from", 1, native_plain_date_from),
            temporal_static("compare", 2, native_plain_date_compare),
        ],
    },
    TemporalClassSpec {
        name: "PlainTime",
        methods: &[
            temporal_static("from", 1, native_plain_time_from),
            temporal_static("compare", 2, native_plain_time_compare),
        ],
    },
    TemporalClassSpec {
        name: "PlainDateTime",
        methods: &[
            temporal_static("from", 1, native_plain_date_time_from),
            temporal_static("compare", 2, native_plain_date_time_compare),
        ],
    },
];

fn install_class(
    heap: &mut otter_gc::GcHeap,
    global_root: Value,
    temporal: JsObject,
    temporal_value: Value,
    spec: &TemporalClassSpec,
) -> Result<(), JsSurfaceError> {
    // Constructor itself — direct `new Temporal.X(...)` throws a
    // `TypeError` until the foundation wires the spec partial-record
    // / epochNanoseconds argument shapes through `[[Construct]]`.
    let ctor = native_constructor_static_with_value_roots(
        heap,
        spec.name,
        0,
        temporal_class_direct_construct,
        &[&global_root, &temporal_value],
    )
    .map_err(|_| JsSurfaceError::OutOfMemory)?;
    let ctor_value = Value::native_function(ctor);

    // Install each spec-listed static method as an own data property
    // on the constructor. Length / attrs match the namespace methods
    // installed by the prior namespace-object form.
    for method in spec.methods {
        let fn_obj = native_static_with_value_roots(
            heap,
            method.name,
            method.length,
            method.call,
            &[&global_root, &temporal_value, &ctor_value],
        )
        .map_err(|_| JsSurfaceError::OutOfMemory)?;
        let desc = PropertyDescriptor::data(Value::native_function(fn_obj), true, false, true);
        if !ctor.define_own_property(heap, method.name, desc) {
            return Err(JsSurfaceError::DefinePropertyFailed(method.name));
        }
    }

    object::set(temporal, heap, spec.name, ctor_value);
    Ok(())
}

fn temporal_class_direct_construct(
    ctx: &mut NativeCtx<'_>,
    _args: &[Value],
) -> Result<Value, NativeError> {
    let name = if ctx.is_construct_call() {
        "Temporal class constructor"
    } else {
        "Temporal class call"
    };
    Err(NativeError::TypeError {
        name,
        reason: "direct construction not implemented; use the class's spec-defined factory \
                 (e.g. `Temporal.Instant.from`, `Temporal.Now.instant`)"
            .to_string(),
    })
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
