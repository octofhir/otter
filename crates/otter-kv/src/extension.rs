//! KV extension for Otter runtime
//!
//! Registers the KV ops with the runtime and provides JS interop.

use crate::store::KvStore;
use crate::KV_JS;
use otter_runtime::error::{JscError, JscResult};
use otter_runtime::extension::{op_sync, OpContext};
use otter_runtime::Extension;
use parking_lot::RwLock;
use serde_json::{json, Value as JsonValue};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

/// Store ID counter
static STORE_ID: AtomicU64 = AtomicU64::new(1);

/// Store registry
static STORES: once_cell::sync::Lazy<RwLock<HashMap<u64, KvStore>>> =
    once_cell::sync::Lazy::new(|| RwLock::new(HashMap::new()));

/// Helper to convert errors to JscError
fn kv_error(msg: impl Into<String>) -> JscError {
    JscError::internal(msg)
}

/// Create the KV extension
pub fn kv_extension() -> Extension {
    Extension::new("otter-kv")
        .with_ops(vec![
            op_sync("__otter_kv_open", kv_open),
            op_sync("__otter_kv_close", kv_close),
            op_sync("__otter_kv_set", kv_set),
            op_sync("__otter_kv_get", kv_get),
            op_sync("__otter_kv_delete", kv_delete),
            op_sync("__otter_kv_has", kv_has),
            op_sync("__otter_kv_keys", kv_keys),
            op_sync("__otter_kv_clear", kv_clear),
            op_sync("__otter_kv_len", kv_len),
        ])
        .with_js(KV_JS)
}

/// Helper to get first argument as object
fn get_arg(args: &[JsonValue]) -> Option<&JsonValue> {
    args.first()
}

/// Open a KV store
fn kv_open(_ctx: OpContext, args: Vec<JsonValue>) -> JscResult<JsonValue> {
    let arg = get_arg(&args).ok_or_else(|| kv_error("Missing arguments"))?;
    let path = arg
        .get("path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| kv_error("Missing path"))?;

    let store = KvStore::open(path).map_err(|e| kv_error(e.to_string()))?;

    let id = STORE_ID.fetch_add(1, Ordering::SeqCst);
    STORES.write().insert(id, store);

    Ok(json!({ "id": id }))
}

/// Close a KV store
fn kv_close(_ctx: OpContext, args: Vec<JsonValue>) -> JscResult<JsonValue> {
    let arg = get_arg(&args).ok_or_else(|| kv_error("Missing arguments"))?;
    let id = arg
        .get("id")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| kv_error("Missing store id"))?;

    if STORES.write().remove(&id).is_some() {
        Ok(json!(true))
    } else {
        Ok(json!(false))
    }
}

/// Set a value
fn kv_set(_ctx: OpContext, args: Vec<JsonValue>) -> JscResult<JsonValue> {
    let arg = get_arg(&args).ok_or_else(|| kv_error("Missing arguments"))?;
    let id = arg
        .get("id")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| kv_error("Missing store id"))?;

    let key = arg
        .get("key")
        .and_then(|v| v.as_str())
        .ok_or_else(|| kv_error("Missing key"))?;

    let value = arg.get("value").ok_or_else(|| kv_error("Missing value"))?;

    let stores = STORES.read();
    let store = stores.get(&id).ok_or_else(|| kv_error("Store not found"))?;

    store.set(key, value).map_err(|e| kv_error(e.to_string()))?;

    Ok(json!(true))
}

/// Get a value
fn kv_get(_ctx: OpContext, args: Vec<JsonValue>) -> JscResult<JsonValue> {
    let arg = get_arg(&args).ok_or_else(|| kv_error("Missing arguments"))?;
    let id = arg
        .get("id")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| kv_error("Missing store id"))?;

    let key = arg
        .get("key")
        .and_then(|v| v.as_str())
        .ok_or_else(|| kv_error("Missing key"))?;

    let stores = STORES.read();
    let store = stores.get(&id).ok_or_else(|| kv_error("Store not found"))?;

    match store.get(key).map_err(|e| kv_error(e.to_string()))? {
        Some(value) => Ok(value),
        None => Ok(JsonValue::Null),
    }
}

/// Delete a key
fn kv_delete(_ctx: OpContext, args: Vec<JsonValue>) -> JscResult<JsonValue> {
    let arg = get_arg(&args).ok_or_else(|| kv_error("Missing arguments"))?;
    let id = arg
        .get("id")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| kv_error("Missing store id"))?;

    let key = arg
        .get("key")
        .and_then(|v| v.as_str())
        .ok_or_else(|| kv_error("Missing key"))?;

    let stores = STORES.read();
    let store = stores.get(&id).ok_or_else(|| kv_error("Store not found"))?;

    let deleted = store.delete(key).map_err(|e| kv_error(e.to_string()))?;

    Ok(json!(deleted))
}

/// Check if a key exists
fn kv_has(_ctx: OpContext, args: Vec<JsonValue>) -> JscResult<JsonValue> {
    let arg = get_arg(&args).ok_or_else(|| kv_error("Missing arguments"))?;
    let id = arg
        .get("id")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| kv_error("Missing store id"))?;

    let key = arg
        .get("key")
        .and_then(|v| v.as_str())
        .ok_or_else(|| kv_error("Missing key"))?;

    let stores = STORES.read();
    let store = stores.get(&id).ok_or_else(|| kv_error("Store not found"))?;

    let exists = store.has(key).map_err(|e| kv_error(e.to_string()))?;

    Ok(json!(exists))
}

/// Get all keys
fn kv_keys(_ctx: OpContext, args: Vec<JsonValue>) -> JscResult<JsonValue> {
    let arg = get_arg(&args).ok_or_else(|| kv_error("Missing arguments"))?;
    let id = arg
        .get("id")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| kv_error("Missing store id"))?;

    let stores = STORES.read();
    let store = stores.get(&id).ok_or_else(|| kv_error("Store not found"))?;

    let keys = store.keys().map_err(|e| kv_error(e.to_string()))?;

    Ok(json!(keys))
}

/// Clear all keys
fn kv_clear(_ctx: OpContext, args: Vec<JsonValue>) -> JscResult<JsonValue> {
    let arg = get_arg(&args).ok_or_else(|| kv_error("Missing arguments"))?;
    let id = arg
        .get("id")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| kv_error("Missing store id"))?;

    let stores = STORES.read();
    let store = stores.get(&id).ok_or_else(|| kv_error("Store not found"))?;

    store.clear().map_err(|e| kv_error(e.to_string()))?;

    Ok(json!(true))
}

/// Get the number of keys
fn kv_len(_ctx: OpContext, args: Vec<JsonValue>) -> JscResult<JsonValue> {
    let arg = get_arg(&args).ok_or_else(|| kv_error("Missing arguments"))?;
    let id = arg
        .get("id")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| kv_error("Missing store id"))?;

    let stores = STORES.read();
    let store = stores.get(&id).ok_or_else(|| kv_error("Store not found"))?;

    let len = store.len().map_err(|e| kv_error(e.to_string()))?;

    Ok(json!(len))
}
