use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use otter_macros::{burrow, dive, lodge};
use otter_runtime::{ObjectHandle, RegisterValue, RuntimeState, VmNativeCallError};
use otter_vm::object::HeapValueKind;
use otter_vm::payload::{VmTrace, VmValueTracer};
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use serde_json::{Map as JsonMap, Number as JsonNumber, Value as JsonValue};

#[derive(Debug, thiserror::Error)]
pub enum KvError {
    #[error("database error: {0}")]
    Database(String),
    #[error("serialization error: {0}")]
    Serialization(String),
    #[error("invalid path: {0}")]
    InvalidPath(String),
    #[error("store is closed")]
    Closed,
}

pub type KvResult<T> = Result<T, KvError>;

const TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("kv");
const MAX_JSON_DEPTH: usize = 64;
static MEMORY_STORE_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug)]
pub struct KvStore {
    db: Database,
    path: Box<str>,
    is_memory: bool,
    backing_path: Option<PathBuf>,
}

impl KvStore {
    pub fn open(path: &str) -> KvResult<Self> {
        let (path_string, is_memory, backing_path) = if path.is_empty() || path == ":memory:" {
            (
                ":memory:".to_string(),
                true,
                Some(next_memory_backing_path()),
            )
        } else {
            let path = Path::new(path);
            if let Some(parent) = path.parent()
                && !parent.as_os_str().is_empty()
                && !parent.exists()
            {
                std::fs::create_dir_all(parent)
                    .map_err(|error| KvError::InvalidPath(error.to_string()))?;
            }
            (
                path.to_string_lossy().into_owned(),
                false,
                Some(path.to_path_buf()),
            )
        };

        let db = Database::create(
            backing_path
                .as_ref()
                .expect("kv store backing path should be initialized"),
        )
        .map_err(|error| KvError::Database(error.to_string()))?;

        let write_txn = db
            .begin_write()
            .map_err(|error| KvError::Database(error.to_string()))?;
        {
            let _table = write_txn
                .open_table(TABLE)
                .map_err(|error| KvError::Database(error.to_string()))?;
        }
        write_txn
            .commit()
            .map_err(|error| KvError::Database(error.to_string()))?;

        Ok(Self {
            db,
            path: path_string.into_boxed_str(),
            is_memory,
            backing_path,
        })
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn is_memory(&self) -> bool {
        self.is_memory
    }

    pub fn set(&self, key: &str, value: &JsonValue) -> KvResult<()> {
        let serialized =
            serde_json::to_vec(value).map_err(|error| KvError::Serialization(error.to_string()))?;

        let write_txn = self
            .db
            .begin_write()
            .map_err(|error| KvError::Database(error.to_string()))?;
        {
            let mut table = write_txn
                .open_table(TABLE)
                .map_err(|error| KvError::Database(error.to_string()))?;
            table
                .insert(key, serialized.as_slice())
                .map_err(|error| KvError::Database(error.to_string()))?;
        }
        write_txn
            .commit()
            .map_err(|error| KvError::Database(error.to_string()))?;

        Ok(())
    }

    pub fn get(&self, key: &str) -> KvResult<Option<JsonValue>> {
        let read_txn = self
            .db
            .begin_read()
            .map_err(|error| KvError::Database(error.to_string()))?;
        let table = read_txn
            .open_table(TABLE)
            .map_err(|error| KvError::Database(error.to_string()))?;

        match table.get(key) {
            Ok(Some(guard)) => serde_json::from_slice(guard.value())
                .map(Some)
                .map_err(|error| KvError::Serialization(error.to_string())),
            Ok(None) => Ok(None),
            Err(error) => Err(KvError::Database(error.to_string())),
        }
    }

    pub fn has(&self, key: &str) -> KvResult<bool> {
        let read_txn = self
            .db
            .begin_read()
            .map_err(|error| KvError::Database(error.to_string()))?;
        let table = read_txn
            .open_table(TABLE)
            .map_err(|error| KvError::Database(error.to_string()))?;
        match table.get(key) {
            Ok(Some(_)) => Ok(true),
            Ok(None) => Ok(false),
            Err(error) => Err(KvError::Database(error.to_string())),
        }
    }

    pub fn delete(&self, key: &str) -> KvResult<bool> {
        let write_txn = self
            .db
            .begin_write()
            .map_err(|error| KvError::Database(error.to_string()))?;
        let removed = {
            let mut table = write_txn
                .open_table(TABLE)
                .map_err(|error| KvError::Database(error.to_string()))?;
            table
                .remove(key)
                .map_err(|error| KvError::Database(error.to_string()))?
                .is_some()
        };
        write_txn
            .commit()
            .map_err(|error| KvError::Database(error.to_string()))?;
        Ok(removed)
    }

    pub fn keys(&self) -> KvResult<Vec<String>> {
        let read_txn = self
            .db
            .begin_read()
            .map_err(|error| KvError::Database(error.to_string()))?;
        let table = read_txn
            .open_table(TABLE)
            .map_err(|error| KvError::Database(error.to_string()))?;
        let mut keys = Vec::new();
        let iter = table
            .iter()
            .map_err(|error| KvError::Database(error.to_string()))?;
        for item in iter {
            let (key, _) = item.map_err(|error| KvError::Database(error.to_string()))?;
            keys.push(key.value().to_string());
        }
        Ok(keys)
    }

    pub fn len(&self) -> KvResult<usize> {
        self.keys().map(|keys| keys.len())
    }

    pub fn is_empty(&self) -> KvResult<bool> {
        self.len().map(|n| n == 0)
    }

    pub fn clear(&self) -> KvResult<()> {
        let keys = self.keys()?;
        let write_txn = self
            .db
            .begin_write()
            .map_err(|error| KvError::Database(error.to_string()))?;
        {
            let mut table = write_txn
                .open_table(TABLE)
                .map_err(|error| KvError::Database(error.to_string()))?;
            for key in keys {
                table
                    .remove(key.as_str())
                    .map_err(|error| KvError::Database(error.to_string()))?;
            }
        }
        write_txn
            .commit()
            .map_err(|error| KvError::Database(error.to_string()))?;
        Ok(())
    }
}

impl Drop for KvStore {
    fn drop(&mut self) {
        if self.is_memory
            && let Some(path) = self.backing_path.take()
        {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[derive(Debug)]
struct KvStorePayload {
    store: Option<KvStore>,
    path: Box<str>,
    is_memory: bool,
}

impl VmTrace for KvStorePayload {
    fn trace(&self, _tracer: &mut dyn VmValueTracer) {}
}

lodge!(
    kv_module,
    module_specifiers = ["otter:kv"],
    default = function(kv_open as "kv"),
    functions = [
        ("kv", kv_open),
        ("openKv", kv_open as "openKv"),
    ],
);

#[dive(name = "kv", length = 1)]
fn kv_open(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let path = args
        .first()
        .copied()
        .filter(|value| *value != RegisterValue::undefined())
        .map(|value| runtime.js_to_string_infallible(value).into_string())
        .unwrap_or_else(|| ":memory:".to_string());

    let store = KvStore::open(&path).map_err(|error| kv_error(runtime, error))?;
    let is_memory = store.is_memory();
    let path_value = store.path().to_string().into_boxed_str();
    let object = runtime.alloc_native_object(KvStorePayload {
        store: Some(store),
        path: path_value,
        is_memory,
    });

    let members = burrow! {
        fns = [
            kv_set,
            kv_get,
            kv_delete,
            kv_has,
            kv_keys,
            kv_clear,
            kv_close,
            kv_size,
            kv_path,
            kv_is_memory,
            kv_closed
        ]
    };
    runtime.install_burrow(object, &members)?;

    Ok(RegisterValue::from_object_handle(object.0))
}

#[dive(name = "set", length = 2)]
fn kv_set(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let key = required_string_arg(runtime, args.first(), "kv.set: missing key")?;
    let value = *args
        .get(1)
        .ok_or_else(|| throw_type_error(runtime, "kv.set: missing value"))?;
    let json = register_to_json(value, runtime, 0, &mut HashSet::new())?;
    with_store_mut(runtime, this, |store| store.set(&key, &json))?;
    Ok(RegisterValue::undefined())
}

#[dive(name = "get", length = 1)]
fn kv_get(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let key = required_string_arg(runtime, args.first(), "kv.get: missing key")?;
    let value = with_store_mut(runtime, this, |store| store.get(&key))?;
    match value {
        Some(value) => json_to_register(&value, runtime, 0),
        None => Ok(RegisterValue::undefined()),
    }
}

#[dive(name = "delete", length = 1)]
fn kv_delete(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let key = required_string_arg(runtime, args.first(), "kv.delete: missing key")?;
    let deleted = with_store_mut(runtime, this, |store| store.delete(&key))?;
    Ok(RegisterValue::from_bool(deleted))
}

#[dive(name = "has", length = 1)]
fn kv_has(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let key = required_string_arg(runtime, args.first(), "kv.has: missing key")?;
    let has = with_store_mut(runtime, this, |store| store.has(&key))?;
    Ok(RegisterValue::from_bool(has))
}

#[dive(name = "keys", length = 0)]
fn kv_keys(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let keys = with_store_mut(runtime, this, |store| store.keys())?;
    let elements = keys
        .into_iter()
        .map(|key| RegisterValue::from_object_handle(runtime.alloc_string(key).0))
        .collect::<Vec<_>>();
    Ok(RegisterValue::from_object_handle(
        runtime.alloc_array_with_elements(&elements).0,
    ))
}

#[dive(name = "clear", length = 0)]
fn kv_clear(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    with_store_mut(runtime, this, |store| store.clear())?;
    Ok(RegisterValue::undefined())
}

#[dive(name = "close", length = 0)]
fn kv_close(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let payload = match runtime.native_payload_mut_from_value::<KvStorePayload>(this) {
        Ok(payload) => payload,
        Err(_) => {
            return Err(throw_type_error(
                runtime,
                "kv.close: receiver is not a KV store",
            ));
        }
    };
    payload.store.take();
    Ok(RegisterValue::undefined())
}

#[dive(name = "size", getter)]
fn kv_size(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let size = with_store_mut(runtime, this, |store| store.len())?;
    Ok(RegisterValue::from_number(size as f64))
}

#[dive(name = "path", getter)]
fn kv_path(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let path = {
        let payload = match runtime.native_payload_mut_from_value::<KvStorePayload>(this) {
            Ok(payload) => payload,
            Err(_) => {
                return Err(throw_type_error(
                    runtime,
                    "kv.path: receiver is not a KV store",
                ));
            }
        };
        payload.path.clone()
    };
    Ok(RegisterValue::from_object_handle(
        runtime.alloc_string(path).0,
    ))
}

#[dive(name = "isMemory", getter)]
fn kv_is_memory(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let is_memory = {
        let payload = match runtime.native_payload_mut_from_value::<KvStorePayload>(this) {
            Ok(payload) => payload,
            Err(_) => {
                return Err(throw_type_error(
                    runtime,
                    "kv.isMemory: receiver is not a KV store",
                ));
            }
        };
        payload.is_memory
    };
    Ok(RegisterValue::from_bool(is_memory))
}

#[dive(name = "closed", getter)]
fn kv_closed(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let closed = {
        let payload = match runtime.native_payload_mut_from_value::<KvStorePayload>(this) {
            Ok(payload) => payload,
            Err(_) => {
                return Err(throw_type_error(
                    runtime,
                    "kv.closed: receiver is not a KV store",
                ));
            }
        };
        payload.store.is_none()
    };
    Ok(RegisterValue::from_bool(closed))
}

fn register_to_json(
    value: RegisterValue,
    runtime: &mut RuntimeState,
    depth: usize,
    seen: &mut HashSet<ObjectHandle>,
) -> Result<JsonValue, VmNativeCallError> {
    if depth > MAX_JSON_DEPTH {
        return Err(throw_type_error(
            runtime,
            "kv.set: value exceeds maximum JSON nesting depth",
        ));
    }
    if value == RegisterValue::undefined() {
        return Err(throw_type_error(
            runtime,
            "kv.set: undefined values are not supported",
        ));
    }
    if value == RegisterValue::null() {
        return Ok(JsonValue::Null);
    }
    if let Some(boolean) = value.as_bool() {
        return Ok(JsonValue::Bool(boolean));
    }
    if let Some(number) = value.as_number() {
        let number = JsonNumber::from_f64(number).ok_or_else(|| {
            throw_type_error(runtime, "kv.set: non-finite numbers are not supported")
        })?;
        return Ok(JsonValue::Number(number));
    }

    let Some(handle) = value.as_object_handle().map(ObjectHandle) else {
        return Err(throw_type_error(runtime, "kv.set: unsupported value type"));
    };

    if !seen.insert(handle) {
        return Err(throw_type_error(
            runtime,
            "kv.set: cyclic values are not supported",
        ));
    }

    let result = match runtime.objects().kind(handle) {
        Ok(HeapValueKind::String) => Ok(JsonValue::String(
            runtime.js_to_string_infallible(value).into_string(),
        )),
        Ok(HeapValueKind::Array) => {
            let elements = runtime.array_to_args(handle)?;
            let mut values = Vec::with_capacity(elements.len());
            for element in elements {
                values.push(register_to_json(element, runtime, depth + 1, seen)?);
            }
            Ok(JsonValue::Array(values))
        }
        Ok(HeapValueKind::Object) => {
            let mut map = JsonMap::new();
            for key in runtime.enumerable_own_property_keys(handle)? {
                let Some(name) = runtime.property_names().get(key).map(str::to_owned) else {
                    continue;
                };
                let property = runtime.own_property_value(handle, key)?;
                map.insert(name, register_to_json(property, runtime, depth + 1, seen)?);
            }
            Ok(JsonValue::Object(map))
        }
        Ok(_) | Err(_) => Err(throw_type_error(runtime, "kv.set: unsupported object type")),
    };

    seen.remove(&handle);
    result
}

fn json_to_register(
    value: &JsonValue,
    runtime: &mut RuntimeState,
    depth: usize,
) -> Result<RegisterValue, VmNativeCallError> {
    if depth > MAX_JSON_DEPTH {
        return Err(throw_type_error(
            runtime,
            "kv.get: stored value exceeds maximum JSON nesting depth",
        ));
    }

    match value {
        JsonValue::Null => Ok(RegisterValue::null()),
        JsonValue::Bool(boolean) => Ok(RegisterValue::from_bool(*boolean)),
        JsonValue::Number(number) => {
            if let Some(integer) = number.as_i64()
                && let Ok(integer) = i32::try_from(integer)
            {
                return Ok(RegisterValue::from_i32(integer));
            }
            let as_f64 = number
                .as_f64()
                .ok_or_else(|| throw_type_error(runtime, "kv.get: invalid numeric value"))?;
            Ok(RegisterValue::from_number(as_f64))
        }
        JsonValue::String(string) => Ok(RegisterValue::from_object_handle(
            runtime.alloc_string(string.clone()).0,
        )),
        JsonValue::Array(values) => {
            let mut elements = Vec::with_capacity(values.len());
            for value in values {
                elements.push(json_to_register(value, runtime, depth + 1)?);
            }
            Ok(RegisterValue::from_object_handle(
                runtime.alloc_array_with_elements(&elements).0,
            ))
        }
        JsonValue::Object(entries) => {
            let object = runtime.alloc_object();
            for (key, value) in entries {
                let property = runtime.intern_property_name(key);
                let value = json_to_register(value, runtime, depth + 1)?;
                runtime
                    .objects_mut()
                    .set_property(object, property, value)
                    .map_err(|error| {
                        VmNativeCallError::Internal(
                            format!("kv.get: failed to materialize object property: {error:?}")
                                .into(),
                        )
                    })?;
            }
            Ok(RegisterValue::from_object_handle(object.0))
        }
    }
}

fn with_store_mut<T>(
    runtime: &mut RuntimeState,
    this: &RegisterValue,
    f: impl FnOnce(&mut KvStore) -> KvResult<T>,
) -> Result<T, VmNativeCallError> {
    let payload = match runtime.native_payload_mut_from_value::<KvStorePayload>(this) {
        Ok(payload) => payload,
        Err(_) => return Err(throw_type_error(runtime, "receiver is not a KV store")),
    };
    let store = match payload.store.as_mut() {
        Some(store) => store,
        None => return Err(throw_type_error(runtime, "KV store is closed")),
    };
    f(store).map_err(|error| kv_error(runtime, error))
}

fn required_string_arg(
    runtime: &mut RuntimeState,
    value: Option<&RegisterValue>,
    message: &str,
) -> Result<String, VmNativeCallError> {
    let value = *value.ok_or_else(|| throw_type_error(runtime, message))?;
    Ok(runtime.js_to_string_infallible(value).into_string())
}

fn throw_type_error(runtime: &mut RuntimeState, message: &str) -> VmNativeCallError {
    match runtime.alloc_type_error(message) {
        Ok(error) => VmNativeCallError::Thrown(RegisterValue::from_object_handle(error.0)),
        Err(_) => VmNativeCallError::Internal(message.into()),
    }
}

fn kv_error(runtime: &mut RuntimeState, error: KvError) -> VmNativeCallError {
    throw_type_error(runtime, &error.to_string())
}

fn next_memory_backing_path() -> PathBuf {
    let unique = MEMORY_STORE_ID.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "otter-modules-kv-{}-{}.redb",
        std::process::id(),
        unique
    ))
}
