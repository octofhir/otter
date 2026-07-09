//! `otter:kv` host storage.
//!
//! The active slice provides an owned Rust `KvStore` and a static namespace spec
//! for the hosted-module loader. File-backed stores persist a JSON object and
//! enforce runtime read/write capabilities before opening or mutating paths.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use otter_runtime::CapabilitySet;
use otter_runtime::{
    RuntimeHostObjectError, RuntimeJsObject as JsObject, RuntimeNativeCtx as NativeCtx,
    RuntimeNativeError as NativeError, RuntimeObjectBuilder as ObjectBuilder,
    RuntimeValue as Value, runtime_this_object, runtime_with_host_data, runtime_with_host_data_mut,
};
use serde_json::Value as JsonValue;

/// Errors produced by `otter:kv`.
#[derive(Debug, thiserror::Error)]
pub enum KvError {
    /// Filesystem permission denied.
    #[error("permission denied for `{path}`")]
    PermissionDenied {
        /// Path that was rejected.
        path: PathBuf,
    },
    /// The persisted JSON file was not an object.
    #[error("kv backing file must contain a JSON object")]
    InvalidBacking,
    /// Filesystem error.
    #[error("io error: {0}")]
    Io(String),
    /// Serialization error.
    #[error("serialization error: {0}")]
    Serialization(String),
}

/// Result alias for `otter:kv`.
pub type KvResult<T> = Result<T, KvError>;

/// Permission-gated key/value store.
#[derive(Debug, Clone)]
pub struct KvStore {
    path: Option<PathBuf>,
    entries: BTreeMap<String, JsonValue>,
    can_write: bool,
}

impl KvStore {
    /// Open an in-memory store.
    #[must_use]
    pub fn memory() -> Self {
        Self {
            path: None,
            entries: BTreeMap::new(),
            can_write: true,
        }
    }

    /// Open a file-backed store after checking read/write capabilities.
    ///
    /// An absent file starts as an empty object but still requires write
    /// permission because the store may create it on first mutation.
    pub fn open(path: impl AsRef<Path>, capabilities: &CapabilitySet) -> KvResult<Self> {
        let path = path.as_ref().to_path_buf();
        if !capabilities.read.matches_path(&path) || !capabilities.write.matches_path(&path) {
            return Err(KvError::PermissionDenied { path });
        }
        let entries = if path.exists() {
            let text =
                std::fs::read_to_string(&path).map_err(|err| KvError::Io(err.to_string()))?;
            let value: JsonValue = serde_json::from_str(&text)
                .map_err(|err| KvError::Serialization(err.to_string()))?;
            json_object_to_map(value)?
        } else {
            BTreeMap::new()
        };
        Ok(Self {
            path: Some(path),
            entries,
            can_write: true,
        })
    }

    /// Store a JSON value under `key`.
    pub fn set(&mut self, key: impl Into<String>, value: JsonValue) -> KvResult<()> {
        self.require_write()?;
        self.entries.insert(key.into(), value);
        self.flush()
    }

    /// Return a cloned JSON value for `key`.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<JsonValue> {
        self.entries.get(key).cloned()
    }

    /// Return whether `key` exists.
    #[must_use]
    pub fn has(&self, key: &str) -> bool {
        self.entries.contains_key(key)
    }

    /// Delete `key`, returning whether it existed.
    pub fn delete(&mut self, key: &str) -> KvResult<bool> {
        self.require_write()?;
        let existed = self.entries.remove(key).is_some();
        self.flush()?;
        Ok(existed)
    }

    /// Clear every key.
    pub fn clear(&mut self) -> KvResult<()> {
        self.require_write()?;
        self.entries.clear();
        self.flush()
    }

    /// Keys in deterministic order.
    #[must_use]
    pub fn keys(&self) -> Vec<String> {
        self.entries.keys().cloned().collect()
    }

    /// Entry count.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the store is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn require_write(&self) -> KvResult<()> {
        if self.can_write {
            Ok(())
        } else {
            Err(KvError::PermissionDenied {
                path: self.path.clone().unwrap_or_default(),
            })
        }
    }

    fn flush(&self) -> KvResult<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|err| KvError::Io(err.to_string()))?;
        }
        let text = serde_json::to_string_pretty(&self.entries)
            .map_err(|err| KvError::Serialization(err.to_string()))?;
        std::fs::write(path, text).map_err(|err| KvError::Io(err.to_string()))
    }
}

fn json_object_to_map(value: JsonValue) -> KvResult<BTreeMap<String, JsonValue>> {
    match value {
        JsonValue::Object(map) => Ok(map.into_iter().collect()),
        _ => Err(KvError::InvalidBacking),
    }
}

otter_macros::lodge! {
    prefix = "otter",
    name = "kv",
    capabilities = true,
    exports = {
        "openKv" / 1 => open_kv,
        "kv"     / 1 => open_kv,
    },
}

fn open_kv(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    capabilities: &CapabilitySet,
) -> Result<Value, NativeError> {
    let path = crate::arg_string(args, 0, "openKv", ctx.heap())?;
    let store = if path.is_empty() || path == ":memory:" {
        KvStore::memory()
    } else {
        KvStore::open(&path, capabilities)
            .map_err(|err| crate::type_error("openKv", err.to_string()))?
    };
    let object = build_store_object(ctx, store)?;
    Ok(Value::object(object))
}

fn build_store_object(ctx: &mut NativeCtx<'_>, store: KvStore) -> Result<JsObject, NativeError> {
    let mut builder = ObjectBuilder::from_host_data(ctx, store)?;
    builder
        .builtin_method("set", 2, method_set)
        .map_err(|err| crate::type_error("KvStore", err.to_string()))?
        .builtin_method("get", 1, method_get)
        .map_err(|err| crate::type_error("KvStore", err.to_string()))?
        .builtin_method("has", 1, method_has)
        .map_err(|err| crate::type_error("KvStore", err.to_string()))?
        .builtin_method("delete", 1, method_delete)
        .map_err(|err| crate::type_error("KvStore", err.to_string()))?
        .builtin_method("keys", 0, method_keys)
        .map_err(|err| crate::type_error("KvStore", err.to_string()))?
        .builtin_method("clear", 0, method_clear)
        .map_err(|err| crate::type_error("KvStore", err.to_string()))?;
    Ok(builder.build())
}

fn store_receiver(ctx: &NativeCtx<'_>, name: &'static str) -> Result<JsObject, NativeError> {
    runtime_this_object(ctx, name, "KvStore")
}

fn host_error(name: &'static str, err: RuntimeHostObjectError) -> NativeError {
    crate::type_error(name, err.to_string())
}

fn method_set(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let object = store_receiver(ctx, "KvStore.set")?;
    let key = crate::arg_string(args, 0, "KvStore.set", ctx.heap())?;
    let value = args
        .get(1)
        .map(|v| crate::value_to_json(v, ctx.heap()))
        .transpose()?
        .unwrap_or(JsonValue::Null);
    let result =
        runtime_with_host_data_mut::<KvStore, _>(ctx, object, |store| store.set(key, value))
            .map_err(|err| host_error("KvStore.set", err))?;
    result.map_err(|err| crate::type_error("KvStore.set", err.to_string()))?;
    Ok(Value::undefined())
}

fn method_get(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let object = store_receiver(ctx, "KvStore.get")?;
    let key = crate::arg_string(args, 0, "KvStore.get", ctx.heap())?;
    let value = runtime_with_host_data::<KvStore, _>(ctx, object, |store| {
        store.get(&key).unwrap_or(JsonValue::Null)
    })
    .map_err(|err| host_error("KvStore.get", err))?;
    crate::json_to_value(ctx, value)
}

fn method_has(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let object = store_receiver(ctx, "KvStore.has")?;
    let key = crate::arg_string(args, 0, "KvStore.has", ctx.heap())?;
    let has = runtime_with_host_data::<KvStore, _>(ctx, object, |store| store.has(&key))
        .map_err(|err| host_error("KvStore.has", err))?;
    Ok(Value::boolean(has))
}

fn method_delete(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let object = store_receiver(ctx, "KvStore.delete")?;
    let key = crate::arg_string(args, 0, "KvStore.delete", ctx.heap())?;
    let result = runtime_with_host_data_mut::<KvStore, _>(ctx, object, |store| store.delete(&key))
        .map_err(|err| host_error("KvStore.delete", err))?;
    let deleted = result.map_err(|err| crate::type_error("KvStore.delete", err.to_string()))?;
    Ok(Value::boolean(deleted))
}

fn method_keys(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let object = store_receiver(ctx, "KvStore.keys")?;
    let keys = runtime_with_host_data::<KvStore, _>(ctx, object, KvStore::keys)
        .map_err(|err| host_error("KvStore.keys", err))?;
    // Each key string and the backing array are separate allocations. Collecting
    // the strings into a `Vec` first left every earlier `JsString` unrooted
    // across the later allocations; fill the array through the scope instead so
    // each element is parked the moment it is created.
    ctx.scope(|ctx, s| {
        let array = ctx.scoped_array(s, keys.len())?;
        for (index, key) in keys.iter().enumerate() {
            let value = ctx.scoped_string(s, key)?;
            ctx.scoped_set_index(s, array, index, value)?;
        }
        Ok::<Value, NativeError>(ctx.escape(array))
    })
}

fn method_clear(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let object = store_receiver(ctx, "KvStore.clear")?;
    let result = runtime_with_host_data_mut::<KvStore, _>(ctx, object, KvStore::clear)
        .map_err(|err| host_error("KvStore.clear", err))?;
    result.map_err(|err| crate::type_error("KvStore.clear", err.to_string()))?;
    Ok(Value::undefined())
}
