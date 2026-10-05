//! Owned error transport at the Wasmtime / JavaScript boundary.
//!
//! # Contents
//! - Wasmtime user-error downcast into the existing `JsError` carrier.
//! - Intrinsic Compile/Link/RuntimeError selection for actual Wasm failures.
//! - Canonical synchronous user-value throwing and catchable projection.
//!
//! # Invariants
//! - Host structural/control/OOM causes never become a Wasm exception payload.
//! - Wasmtime backtrace context retains the typed host error; no pending fatal
//!   slot, text decoder, or second owned error carrier is used.
//! - Engine errors select the realm's pinned Error registry, never mutable JS
//!   constructors. Materialization failure replaces success with its actual cause.
//! - JS identity is retained only by the existing synchronous pending root and
//!   StoreState persistent roots used for JSTag/externref payloads.
//!
//! # See also
//! - `otter_runtime::marshal::MarshalCx::error_value`
//! - `super::run_import` and `super::surface_call_failure`

use otter_runtime::marshal::{JsError, JsValue, MarshalCx};
use otter_runtime::{RuntimeErrorKind, RuntimeNativeError};

/// Native Wasm failures have an intrinsic class; host user-errors keep their
/// original owned type through Wasmtime's actual context/backtrace chain.
pub(super) fn from_wasmtime(error: wasmtime::Error, kind: RuntimeErrorKind) -> JsError {
    match error.downcast::<JsError>() {
        Ok(error) => error,
        Err(error) if error.is::<wasmtime::OutOfMemory>() => {
            // Wasmtime host allocation is not an Otter cage request. Never
            // fabricate a cage requested/cap tuple or a catchable RuntimeError.
            JsError::Native(RuntimeNativeError::BudgetExceeded {
                reason: format!("WebAssembly host allocation failed: {error}"),
            })
        }
        Err(error) => intrinsic(kind, error.to_string()),
    }
}

/// Select a class through the existing canonical native error descriptor.
pub(super) fn intrinsic(kind: RuntimeErrorKind, message: impl Into<String>) -> JsError {
    JsError::Native(RuntimeNativeError::SpecError {
        kind,
        message: message.into(),
    })
}

/// Project only a catchable cause. Heap exhaustion retains the actual native
/// failure even when enough diagnostic headroom could materialize a RangeError.
pub(super) fn catchable_value<'s>(
    cx: &mut MarshalCx<'_, '_, 's>,
    error: JsError,
) -> Result<JsValue<'s>, JsError> {
    if error.is_out_of_memory() {
        return Err(error);
    }
    cx.error_value(error)
}

/// Publish the actual scoped value to the existing pending-throw root, then
/// return immediately. This is the same high-level native boundary as JS throw.
pub(super) fn throw_value(
    cx: &mut MarshalCx<'_, '_, '_>,
    value: JsValue<'_>,
    name: &'static str,
) -> RuntimeNativeError {
    let value = cx.escape(value);
    cx.ctx().throw_value(name, value)
}
