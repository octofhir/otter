//! First-use Wasmtime backing for one JavaScript realm.
//!
//! # Contents
//! - The existing hidden realm cache and owned, shared initialization state.
//! - Atomic publication of the Engine, Store and well-known native JSTag.
//! - Scoped cache construction independent of expensive native initialization.
//!
//! # Invariants
//! Bootstrap may create the realm carrier and canonical JSTag wrapper without
//! starting Wasmtime. Real Wasm operations resolve this one owner; initialization
//! invokes no JavaScript and holds no VM handles. All three backing values are
//! published only after Engine and tag construction succeed, so failures retain
//! the empty owner and their canonical typed cause. Every tag shell retains its
//! originating realm; there is no process Engine cache or parallel registry.
//!
//! # See also
//! - `super::WasmTag` owns the stable JavaScript wrapper.
//! - `super::boundary` projects actual Wasmtime resource and host failures.

use std::sync::{Arc, Mutex};

use otter_macros::HostClass;
use otter_runtime::marshal::{IntoJs, JsError, JsValue, MarshalCx, class_instance};
use otter_runtime::{RuntimeErrorKind, RuntimeNativeError, object};
use wasmtime::{Config, Engine, Store, Tag, ValType};

use super::{SharedStore, StoreState, from_wasmtime, make_tag};

const REALM_KEY: &str = "__otterWasmRealm";

#[derive(Clone, HostClass)]
pub(super) struct WasmRealm {
    state: Arc<Mutex<RealmState>>,
}

enum RealmState {
    Uninitialized,
    Ready {
        engine: Engine,
        store: SharedStore,
        js_tag: Tag,
    },
}

impl WasmRealm {
    fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(RealmState::Uninitialized)),
        }
    }

    /// Resolve native backing without JavaScript re-entry or VM allocation.
    pub(super) fn resolve(&self) -> Result<(Engine, SharedStore, Tag), JsError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| JsError::Native(RuntimeNativeError::InvalidOperand))?;
        if let RealmState::Ready {
            engine,
            store,
            js_tag,
        } = &*state
        {
            return Ok((engine.clone(), store.clone(), *js_tag));
        }
        let engine = Engine::new(&realm_config())
            .map_err(|error| from_wasmtime(error, RuntimeErrorKind::WasmRuntimeError))?;
        let store: SharedStore = Arc::new(Mutex::new(Store::new(&engine, StoreState::default())));
        let js_tag = make_tag(&engine, &store, &[ValType::EXTERNREF])?;
        let result = (engine.clone(), store.clone(), js_tag);
        *state = RealmState::Ready {
            engine,
            store,
            js_tag,
        };
        Ok(result)
    }
}

impl IntoJs for WasmRealm {
    fn into_js<'s>(self, cx: &mut MarshalCx<'_, '_, 's>) -> Result<JsValue<'s>, JsError> {
        class_instance(cx, "WebAssembly.__Realm", self)
    }
}

fn realm_config() -> Config {
    let mut config = Config::new();
    config.wasm_reference_types(true);
    config.wasm_function_references(true);
    config.wasm_gc(true);
    config.wasm_exceptions(true);
    config
}

/// Resolve the existing scoped realm carrier without initializing its backing.
pub(super) fn realm_handle(cx: &mut MarshalCx<'_, '_, '_>) -> Result<WasmRealm, JsError> {
    let global = cx.global_this();
    let existing = cx.get(global, REALM_KEY)?;
    if let Ok(realm) = cx.with_host_data::<WasmRealm, WasmRealm>(existing, Clone::clone) {
        return Ok(realm);
    }
    let realm = WasmRealm::new();
    let value = realm.clone().into_js(cx)?;
    cx.define(
        global,
        REALM_KEY,
        value,
        object::PropertyFlags::new(false, false, false),
    )?;
    Ok(realm)
}

/// Most entry points require Engine and Store but do not use JSTag directly.
pub(super) fn realm(cx: &mut MarshalCx<'_, '_, '_>) -> Result<(Engine, SharedStore), JsError> {
    let (engine, store, _) = realm_handle(cx)?.resolve()?;
    Ok((engine, store))
}

#[cfg(test)]
mod tests;
