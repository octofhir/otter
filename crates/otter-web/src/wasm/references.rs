//! Fallible Wasm value conversion using the existing borrowed Store owner.
//!
//! # Contents
//! - Externref persistent-root insertion and exact payload lookup.
//! - Numeric/BigInt conversion and shared-store entry wrappers.
//!
//! # Invariants
//! - Import conversions use their existing Caller context while the export or
//!   start driver holds the Store mutex; they never try to acquire it again.
//! - JS values remain Local handles through allocation and observable coercion.
//! - Missing roots/indices and failed BigInt creation remain typed failures.
//! - StoreState::js_refs is the sole persistent-root table for Wasm JS references.
//!
//! # See also
//! - `super::run_import`
//! - `super::StoreState`

use super::{ExternIndex, SharedStore, StoreState, to_wasm_i32};
use otter_runtime::marshal::{JsError, JsValue, MarshalCx};
use otter_runtime::{RuntimeErrorKind, RuntimeNativeError};
use wasmtime::{AsContextMut, ExternRef, HeapType, Rooted, Val, ValType};

pub(super) fn extern_ref_from_js_in(
    cx: &mut MarshalCx<'_, '_, '_>,
    mut store: impl AsContextMut<Data = StoreState>,
    value: JsValue<'_>,
) -> Result<Option<Rooted<ExternRef>>, JsError> {
    if cx.is_nullish(value) {
        return Ok(None);
    }
    let value = cx.escape(value);
    let root = cx.ctx().persistent_root_insert(value);
    let index = store.as_context_mut().data().js_refs.len();
    store.as_context_mut().data_mut().js_refs.push(root);
    // Keep the table entry even on failure: its index must never be reused
    // while existing Wasmtime externrefs might retain it.
    ExternRef::new(store, ExternIndex(index))
        .map(Some)
        .map_err(|error| super::boundary::from_wasmtime(error, RuntimeErrorKind::WasmRuntimeError))
}

pub(super) fn extern_ref_to_js_in<'s>(
    cx: &mut MarshalCx<'_, '_, 's>,
    mut store: impl AsContextMut<Data = StoreState>,
    handle: Option<Rooted<ExternRef>>,
) -> Result<JsValue<'s>, JsError> {
    let Some(handle) = handle else {
        return Ok(cx.null());
    };
    let context = store.as_context_mut();
    let index = handle
        .data(&context)
        .map_err(|error| super::boundary::from_wasmtime(error, RuntimeErrorKind::WasmRuntimeError))?
        .and_then(|payload| payload.downcast_ref::<ExternIndex>())
        .ok_or(JsError::Native(RuntimeNativeError::InvalidOperand))?
        .0;
    let root = context
        .data()
        .js_refs
        .get(index)
        .copied()
        .ok_or(JsError::Native(RuntimeNativeError::InvalidOperand))?;
    let value = cx
        .ctx()
        .persistent_root_get(root)
        .ok_or(JsError::Native(RuntimeNativeError::InvalidOperand))?;
    Ok(cx.park(value))
}

fn primitive_to_js<'s>(
    cx: &mut MarshalCx<'_, '_, 's>,
    value: &Val,
) -> Result<JsValue<'s>, JsError> {
    match value {
        Val::I32(x) => Ok(cx.number(f64::from(*x))),
        Val::I64(x) => cx.bigint_i64(*x),
        Val::F32(bits) => Ok(cx.number(f64::from(f32::from_bits(*bits)))),
        Val::F64(bits) => Ok(cx.number(f64::from_bits(*bits))),
        // The current public function/table ABI has no callable funcref view.
        Val::FuncRef(_) => Ok(cx.null()),
        _ => Err(JsError::Type(
            "unsupported reference-type wasm result".to_string(),
        )),
    }
}

fn primitive_from_js(
    cx: &mut MarshalCx<'_, '_, '_>,
    handle: JsValue<'_>,
    ty: &ValType,
) -> Result<Option<Val>, JsError> {
    Ok(Some(match ty {
        ValType::I32 => Val::I32(to_wasm_i32(cx.to_number_spec(handle)?)),
        ValType::I64 => {
            let n = cx.i64_from_bigint(cx.escape(handle)).ok_or_else(|| {
                JsError::Type("cannot convert a non-BigInt value to a wasm i64".to_string())
            })?;
            Val::I64(n)
        }
        ValType::F32 => Val::F32((cx.to_number_spec(handle)? as f32).to_bits()),
        ValType::F64 => Val::F64(cx.to_number_spec(handle)?.to_bits()),
        ValType::Ref(ty) if ty.heap_type().matches(&HeapType::Extern) => return Ok(None),
        _ => {
            return Err(JsError::Type(
                "unsupported reference-type wasm value".to_string(),
            ));
        }
    }))
}

pub(super) fn val_to_js_in<'s>(
    cx: &mut MarshalCx<'_, '_, 's>,
    store: impl AsContextMut<Data = StoreState>,
    value: &Val,
) -> Result<JsValue<'s>, JsError> {
    match value {
        Val::ExternRef(handle) => extern_ref_to_js_in(cx, store, *handle),
        _ => primitive_to_js(cx, value),
    }
}

pub(super) fn js_to_val_in(
    cx: &mut MarshalCx<'_, '_, '_>,
    store: impl AsContextMut<Data = StoreState>,
    handle: JsValue<'_>,
    ty: &ValType,
) -> Result<Val, JsError> {
    match primitive_from_js(cx, handle, ty)? {
        Some(value) => Ok(value),
        None => Ok(Val::ExternRef(extern_ref_from_js_in(cx, store, handle)?)),
    }
}

pub(super) fn extern_ref_from_js(
    cx: &mut MarshalCx<'_, '_, '_>,
    store: &SharedStore,
    value: JsValue<'_>,
) -> Result<Option<Rooted<ExternRef>>, JsError> {
    let mut store = store.try_lock().map_err(|_| {
        super::boundary::intrinsic(
            RuntimeErrorKind::WasmRuntimeError,
            "re-entrant WebAssembly call is not supported",
        )
    })?;
    extern_ref_from_js_in(cx, &mut *store, value)
}

pub(super) fn extern_ref_to_js<'s>(
    cx: &mut MarshalCx<'_, '_, 's>,
    store: &SharedStore,
    value: Option<Rooted<ExternRef>>,
) -> Result<JsValue<'s>, JsError> {
    let mut store = store.try_lock().map_err(|_| {
        super::boundary::intrinsic(
            RuntimeErrorKind::WasmRuntimeError,
            "re-entrant WebAssembly call is not supported",
        )
    })?;
    extern_ref_to_js_in(cx, &mut *store, value)
}

pub(super) fn val_to_js<'s>(
    cx: &mut MarshalCx<'_, '_, 's>,
    store: &SharedStore,
    value: &Val,
) -> Result<JsValue<'s>, JsError> {
    match value {
        Val::ExternRef(handle) => extern_ref_to_js(cx, store, *handle),
        _ => primitive_to_js(cx, value),
    }
}

pub(super) fn js_to_val(
    cx: &mut MarshalCx<'_, '_, '_>,
    store: &SharedStore,
    handle: JsValue<'_>,
    ty: &ValType,
) -> Result<Val, JsError> {
    // Coercion completes before acquiring the Wasmtime Store. The externref
    // branch has no observable coercion and alone requires that borrow.
    match primitive_from_js(cx, handle, ty)? {
        Some(value) => Ok(value),
        None => Ok(Val::ExternRef(extern_ref_from_js(cx, store, handle)?)),
    }
}
