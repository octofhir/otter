//! `otter:kv` host storage.
//!
//! The active slice provides an owned Rust `KvStore` and a static namespace spec
//! for the hosted-module loader. File-backed stores persist a JSON object and
//! enforce runtime read/write capabilities before opening or mutating paths.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use otter_runtime::CapabilitySet;
use otter_vm::array;
use otter_vm::{Attr, Interpreter, NativeCall, NativeCtx, NativeError, ObjectBuilder, Value};
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

/// Install the `otter:kv` namespace object.
pub fn install_kv_module(
    interp: &mut Interpreter,
    capabilities: &CapabilitySet,
) -> Result<otter_vm::JsObject, String> {
    let caps = capabilities.clone();
    let open = std::sync::Arc::new(
        move |ctx: &mut NativeCtx<'_>, args: &[Value], _captures: &[Value]| {
            open_kv(ctx, args, &caps)
        },
    );
    let mut builder = ObjectBuilder::new(interp.gc_heap_mut()).map_err(|err| err.to_string())?;
    builder
        .method(
            "openKv",
            1,
            NativeCall::Dynamic(open.clone()),
            Attr::builtin_function(),
        )
        .map_err(|err| err.to_string())?
        .method("kv", 1, NativeCall::Dynamic(open), Attr::builtin_function())
        .map_err(|err| err.to_string())?;
    Ok(builder.build())
}

fn open_kv(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    capabilities: &CapabilitySet,
) -> Result<Value, NativeError> {
    let path = crate::arg_string(args, 0, "openKv")?;
    let store = if path.is_empty() || path == ":memory:" {
        KvStore::memory()
    } else {
        KvStore::open(&path, capabilities)
            .map_err(|err| crate::type_error("openKv", err.to_string()))?
    };
    let store = Arc::new(Mutex::new(store));
    let object = build_store_object(ctx, store)?;
    Ok(Value::Object(object))
}

fn build_store_object(
    ctx: &mut NativeCtx<'_>,
    store: Arc<Mutex<KvStore>>,
) -> Result<otter_vm::JsObject, NativeError> {
    let mut builder = ObjectBuilder::new_in_ctx(ctx)?;
    builder
        .method(
            "set",
            2,
            NativeCall::Dynamic(method_set(store.clone())),
            Attr::builtin_function(),
        )
        .map_err(|err| crate::type_error("KvStore", err.to_string()))?
        .method(
            "get",
            1,
            NativeCall::Dynamic(method_get(store.clone())),
            Attr::builtin_function(),
        )
        .map_err(|err| crate::type_error("KvStore", err.to_string()))?
        .method(
            "has",
            1,
            NativeCall::Dynamic(method_has(store.clone())),
            Attr::builtin_function(),
        )
        .map_err(|err| crate::type_error("KvStore", err.to_string()))?
        .method(
            "delete",
            1,
            NativeCall::Dynamic(method_delete(store.clone())),
            Attr::builtin_function(),
        )
        .map_err(|err| crate::type_error("KvStore", err.to_string()))?
        .method(
            "keys",
            0,
            NativeCall::Dynamic(method_keys(store.clone())),
            Attr::builtin_function(),
        )
        .map_err(|err| crate::type_error("KvStore", err.to_string()))?
        .method(
            "clear",
            0,
            NativeCall::Dynamic(method_clear(store)),
            Attr::builtin_function(),
        )
        .map_err(|err| crate::type_error("KvStore", err.to_string()))?;
    Ok(builder.build())
}

fn method_set(store: Arc<Mutex<KvStore>>) -> Arc<otter_vm::NativeFn> {
    Arc::new(move |_ctx, args, _captures| {
        let key = crate::arg_string(args, 0, "KvStore.set")?;
        let value = args
            .get(1)
            .map(crate::value_to_json)
            .transpose()?
            .unwrap_or(JsonValue::Null);
        store
            .lock()
            .map_err(|_| crate::type_error("KvStore.set", "store lock poisoned"))?
            .set(key, value)
            .map_err(|err| crate::type_error("KvStore.set", err.to_string()))?;
        Ok(Value::Undefined)
    })
}

fn method_get(store: Arc<Mutex<KvStore>>) -> Arc<otter_vm::NativeFn> {
    Arc::new(move |ctx, args, _captures| {
        let key = crate::arg_string(args, 0, "KvStore.get")?;
        let value = store
            .lock()
            .map_err(|_| crate::type_error("KvStore.get", "store lock poisoned"))?
            .get(&key)
            .unwrap_or(JsonValue::Null);
        crate::json_to_value(ctx, value)
    })
}

fn method_has(store: Arc<Mutex<KvStore>>) -> Arc<otter_vm::NativeFn> {
    Arc::new(move |_ctx, args, _captures| {
        let key = crate::arg_string(args, 0, "KvStore.has")?;
        let has = store
            .lock()
            .map_err(|_| crate::type_error("KvStore.has", "store lock poisoned"))?
            .has(&key);
        Ok(Value::Boolean(has))
    })
}

fn method_delete(store: Arc<Mutex<KvStore>>) -> Arc<otter_vm::NativeFn> {
    Arc::new(move |_ctx, args, _captures| {
        let key = crate::arg_string(args, 0, "KvStore.delete")?;
        let deleted = store
            .lock()
            .map_err(|_| crate::type_error("KvStore.delete", "store lock poisoned"))?
            .delete(&key)
            .map_err(|err| crate::type_error("KvStore.delete", err.to_string()))?;
        Ok(Value::Boolean(deleted))
    })
}

fn method_keys(store: Arc<Mutex<KvStore>>) -> Arc<otter_vm::NativeFn> {
    Arc::new(move |ctx, _args, _captures| {
        let keys = store
            .lock()
            .map_err(|_| crate::type_error("KvStore.keys", "store lock poisoned"))?
            .keys();
        let values = keys
            .iter()
            .map(|key| crate::string_value(ctx, key))
            .collect::<Result<Vec<_>, _>>()?;
        let array = array::from_elements(ctx.interp_mut().gc_heap_mut(), values)?;
        Ok(Value::Array(array))
    })
}

fn method_clear(store: Arc<Mutex<KvStore>>) -> Arc<otter_vm::NativeFn> {
    Arc::new(move |_ctx, _args, _captures| {
        store
            .lock()
            .map_err(|_| crate::type_error("KvStore.clear", "store lock poisoned"))?
            .clear()
            .map_err(|err| crate::type_error("KvStore.clear", err.to_string()))?;
        Ok(Value::Undefined)
    })
}
