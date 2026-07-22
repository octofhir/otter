//! `node:util` / `util` hosted module.
//!
//! A practical subset of Node's `util`, implemented as a dependency-free JS
//! shim ([`SHIM`]) run through [`otter_runtime::run_builtin_cjs_shim`]. `inspect`
//! (the suite's single most-used helper) and `format` are the focus, alongside
//! `types`, `promisify`, `inherits`, `isDeepStrictEqual`, `deprecate`, dotenv
//! parsing, USV-string normalization, and the ANSI/style helpers.
//!
//! # Invariants
//! - Live call sites are captured through [`NativeCtx`]'s explicit runtime
//!   turn; the Node adapter never borrows interpreter frame internals.
//! - Native results are allocated through rooted scope operations.
//!
//! # See also
//! - [`otter_vm::runtime_cx`] — runtime-turn and rooted native APIs.

use otter_runtime::{
    CapabilitySet, RuntimeLocal as Local, RuntimeNativeScope as NativeScope, RuntimeTaskSpawner,
};
use otter_vm::binary::TypedArrayKind;
use otter_vm::{Attr, NativeCtx, NativeError, Value};

/// Embedded `util` implementation.
const SHIM: &str = include_str!("util.js");

/// Native backing for `util.getCallSites`: capture the live JS call
/// stack as a JSON array of call-site records. `args[0]` is the number
/// of frames to skip from the top (the JS `getCallSites` wrapper passes
/// `1` to hide its own frame); `args[1]` is the requested frame count.
/// Returns a JSON string the shim `JSON.parse`s into plain objects.
fn capture_call_sites(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let skip = args
        .first()
        .and_then(|v| v.as_f64())
        .filter(|n| n.is_finite() && *n >= 0.0)
        .map(|n| n as usize)
        .unwrap_or(0);
    let count = args
        .get(1)
        .and_then(|v| v.as_f64())
        .filter(|n| n.is_finite() && *n >= 0.0)
        .map(|n| n as usize)
        .unwrap_or(10);
    let context = ctx
        .execution_context()
        .ok_or_else(|| NativeError::TypeError {
            name: "util.getCallSites",
            reason: "missing execution context".to_string(),
        })?;
    let json = ctx.capture_call_sites_json(context, skip, count);
    ctx.scope(|mut scope| {
        let json = scope.string(&json)?;
        Ok(scope.finish(json))
    })
}

fn typed_arrays_equal(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let heap = ctx.heap_mut();
    let Some(left) = args.first().and_then(|value| value.as_typed_array(heap)) else {
        return Ok(Value::number_i32(0));
    };
    let Some(right) = args.get(1).and_then(|value| value.as_typed_array(heap)) else {
        return Ok(Value::number_i32(0));
    };
    if left.kind() != right.kind() || left.length(heap) != right.length(heap) {
        return Ok(Value::number_i32(0));
    }

    let byte_length = left
        .length(heap)
        .saturating_mul(left.kind().bytes_per_element());
    let left_start = left.byte_offset(heap);
    let Some(left_end) = left_start.checked_add(byte_length) else {
        return Ok(Value::number_i32(0));
    };
    let left_bytes = left.buffer(heap).with_bytes(heap, |bytes| {
        bytes.get(left_start..left_end).map(<[u8]>::to_vec)
    });
    let Some(left_bytes) = left_bytes else {
        return Ok(Value::number_i32(0));
    };

    let right_start = right.byte_offset(heap);
    let Some(right_end) = right_start.checked_add(byte_length) else {
        return Ok(Value::number_i32(0));
    };
    let equal = right.buffer(heap).with_bytes(heap, |bytes| {
        bytes
            .get(right_start..right_end)
            .is_some_and(|right_bytes| {
                typed_array_bytes_equal(left.kind(), &left_bytes, right_bytes)
            })
    });
    if !equal {
        return Ok(Value::number_i32(0));
    }
    let has_expando = left.expando(heap).is_some() || right.expando(heap).is_some();
    Ok(Value::number_i32(if has_expando { 2 } else { 1 }))
}

fn typed_array_bytes_equal(kind: TypedArrayKind, left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    match kind {
        TypedArrayKind::Float16 => {
            left.chunks_exact(2)
                .zip(right.chunks_exact(2))
                .all(|(left, right)| {
                    let left = u16::from_le_bytes([left[0], left[1]]);
                    let right = u16::from_le_bytes([right[0], right[1]]);
                    left == right || (float16_is_nan(left) && float16_is_nan(right))
                })
        }
        TypedArrayKind::Float32 => {
            left.chunks_exact(4)
                .zip(right.chunks_exact(4))
                .all(|(left, right)| {
                    let left = f32::from_le_bytes(left.try_into().expect("four-byte chunk"));
                    let right = f32::from_le_bytes(right.try_into().expect("four-byte chunk"));
                    left.to_bits() == right.to_bits() || (left.is_nan() && right.is_nan())
                })
        }
        TypedArrayKind::Float64 => {
            left.chunks_exact(8)
                .zip(right.chunks_exact(8))
                .all(|(left, right)| {
                    let left = f64::from_le_bytes(left.try_into().expect("eight-byte chunk"));
                    let right = f64::from_le_bytes(right.try_into().expect("eight-byte chunk"));
                    left.to_bits() == right.to_bits() || (left.is_nan() && right.is_nan())
                })
        }
        _ => left == right,
    }
}

fn float16_is_nan(bits: u16) -> bool {
    bits & 0x7c00 == 0x7c00 && bits & 0x03ff != 0
}

/// CommonJS export: the `util` namespace.
pub fn util_cjs_value<'scope>(
    scope: &mut NativeScope<'scope, '_>,
    _caps: &CapabilitySet,
    _runtime_task_spawner: Option<RuntimeTaskSpawner>,
    module: Local<'scope>,
    require: Local<'scope>,
) -> Result<Local<'scope>, NativeError> {
    let export = otter_runtime::run_builtin_cjs_shim(scope, "node:util", SHIM, module, require)?;
    let callsites = scope.native_method("captureCallSites", 2, capture_call_sites)?;
    let typed_arrays_equal = scope.native_method("typedArraysEqual", 2, typed_arrays_equal)?;
    let flags = Attr {
        writable: false,
        enumerable: false,
        configurable: false,
    }
    .to_flags();
    scope.define(export, "__otterCaptureCallSites", callsites, flags)?;
    scope.define(export, "__otterTypedArraysEqual", typed_arrays_equal, flags)?;
    Ok(export)
}

/// CommonJS `util/types` export. Node exposes the same object by identity as
/// `require('util').types`.
pub fn util_types_cjs_value<'scope>(
    scope: &mut NativeScope<'scope, '_>,
    _caps: &CapabilitySet,
    _runtime_task_spawner: Option<RuntimeTaskSpawner>,
    _module: Local<'scope>,
    require: Local<'scope>,
) -> Result<Local<'scope>, NativeError> {
    let util = otter_runtime::require_commonjs_dependency(scope, require, "util")?;
    scope.get(util, "types")
}
