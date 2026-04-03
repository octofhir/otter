use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use otter_runtime::{
    HostedNativeModule, HostedNativeModuleLoader, NativeFunctionDescriptor, ObjectHandle,
    RegisterValue, RuntimeState, VmNativeCallError,
};
use otter_vm::object::HeapValueKind;
use otter_vm::payload::{VmTrace, VmValueTracer};
use rusqlite::types::{ToSqlOutput, Value as RusqliteValue, ValueRef};
use rusqlite::{Connection, OpenFlags, Row};
use serde_json::{Map as JsonMap, Number as JsonNumber, Value as JsonValue};

const MAX_JSON_DEPTH: usize = 64;
static MEMORY_DB_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, thiserror::Error)]
pub enum SqlError {
    #[error("sqlite error: {0}")]
    Sqlite(String),
    #[error("invalid path: {0}")]
    InvalidPath(String),
    #[error("database is closed")]
    Closed,
    #[error("query parameters must be an array")]
    InvalidParams,
}

pub type SqlResult<T> = Result<T, SqlError>;

#[derive(Debug)]
pub struct SqlDatabase {
    conn: Connection,
    path: Box<str>,
    is_memory: bool,
    backing_path: Option<PathBuf>,
}

impl SqlDatabase {
    pub fn open(path: &str) -> SqlResult<Self> {
        let (display_path, is_memory, backing_path) = if path.is_empty() || path == ":memory:" {
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
                    .map_err(|error| SqlError::InvalidPath(error.to_string()))?;
            }
            (
                path.to_string_lossy().into_owned(),
                false,
                Some(path.to_path_buf()),
            )
        };

        let normalized = backing_path
            .as_ref()
            .expect("sql backing path should be initialized");
        let conn = if is_memory {
            Connection::open(normalized)
        } else {
            let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_FULL_MUTEX;
            Connection::open_with_flags(normalized, flags)
        }
        .map_err(sqlite_error)?;

        if !is_memory {
            conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")
                .map_err(sqlite_error)?;
        }
        conn.execute("PRAGMA foreign_keys = ON", [])
            .map_err(sqlite_error)?;

        Ok(Self {
            conn,
            path: display_path.into_boxed_str(),
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

    fn query(&mut self, sql: &str, params: &[RusqliteParam]) -> SqlResult<Vec<JsonValue>> {
        let mut stmt = self.conn.prepare(sql).map_err(sqlite_error)?;
        let param_refs: Vec<&dyn rusqlite::ToSql> = params
            .iter()
            .map(|param| param as &dyn rusqlite::ToSql)
            .collect();

        let column_names: Vec<String> = stmt
            .column_names()
            .iter()
            .map(|name| name.to_string())
            .collect();
        let column_count = column_names.len();
        let rows = stmt
            .query_map(param_refs.as_slice(), |row| {
                row_to_json(row, &column_names, column_count)
            })
            .map_err(sqlite_error)?;

        let mut result = Vec::new();
        for row in rows {
            result.push(row.map_err(sqlite_error)?);
        }
        Ok(result)
    }

    fn execute(&mut self, sql: &str, params: &[RusqliteParam]) -> SqlResult<u64> {
        let param_refs: Vec<&dyn rusqlite::ToSql> = params
            .iter()
            .map(|param| param as &dyn rusqlite::ToSql)
            .collect();
        self.conn
            .execute(sql, param_refs.as_slice())
            .map(|affected| affected as u64)
            .map_err(sqlite_error)
    }

    fn execute_meta(&mut self, sql: &str, params: &[RusqliteParam]) -> SqlResult<ExecuteMeta> {
        let rows_affected = self.execute(sql, params)?;
        Ok(ExecuteMeta {
            rows_affected,
            last_insert_row_id: Some(self.conn.last_insert_rowid()),
        })
    }

    fn query_one(&mut self, sql: &str, params: &[RusqliteParam]) -> SqlResult<Option<JsonValue>> {
        self.query(sql, params)
            .map(|mut rows| rows.drain(..).next())
    }

    fn query_value(&mut self, sql: &str, params: &[RusqliteParam]) -> SqlResult<Option<JsonValue>> {
        let mut stmt = self.conn.prepare(sql).map_err(sqlite_error)?;
        let param_refs: Vec<&dyn rusqlite::ToSql> = params
            .iter()
            .map(|param| param as &dyn rusqlite::ToSql)
            .collect();
        let mut rows = stmt.query(param_refs.as_slice()).map_err(sqlite_error)?;
        let Some(row) = rows.next().map_err(sqlite_error)? else {
            return Ok(None);
        };
        Ok(Some(value_ref_to_json(
            row.get_ref(0).map_err(sqlite_error)?,
        )))
    }

    fn begin(&mut self) -> SqlResult<()> {
        self.conn
            .execute("BEGIN IMMEDIATE", [])
            .map_err(sqlite_error)?;
        Ok(())
    }

    fn commit(&mut self) -> SqlResult<()> {
        self.conn.execute("COMMIT", []).map_err(sqlite_error)?;
        Ok(())
    }

    fn rollback(&mut self) -> SqlResult<()> {
        self.conn.execute("ROLLBACK", []).map_err(sqlite_error)?;
        Ok(())
    }
}

impl Drop for SqlDatabase {
    fn drop(&mut self) {
        if self.is_memory
            && let Some(path) = self.backing_path.take()
        {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[derive(Debug)]
struct SqlDatabasePayload {
    db: Option<SqlDatabase>,
    path: Box<str>,
    adapter: &'static str,
    is_memory: bool,
    in_transaction: bool,
    last_insert_row_id: Option<i64>,
}

impl VmTrace for SqlDatabasePayload {
    fn trace(&self, _tracer: &mut dyn VmValueTracer) {}
}

#[derive(Debug)]
struct RusqliteParam(RusqliteValue);

#[derive(Debug, Clone, Copy)]
struct ExecuteMeta {
    rows_affected: u64,
    last_insert_row_id: Option<i64>,
}

impl rusqlite::ToSql for RusqliteParam {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::Owned(self.0.clone()))
    }
}

#[derive(Debug)]
pub(crate) struct SqlModule;

impl HostedNativeModuleLoader for SqlModule {
    fn load(&self, runtime: &mut RuntimeState) -> Result<HostedNativeModule, String> {
        let namespace = runtime.alloc_object();
        let open = alloc_named_function(runtime, "openSql", 1, sql_open);
        let sql = alloc_named_function(runtime, "sql", 1, sql_open);
        let default_prop = runtime.intern_property_name("default");
        let open_prop = runtime.intern_property_name("openSql");
        let sql_prop = runtime.intern_property_name("sql");

        runtime
            .objects_mut()
            .set_property(
                namespace,
                default_prop,
                RegisterValue::from_object_handle(open.0),
            )
            .map_err(|error| format!("failed to install otter:sql default export: {error:?}"))?;
        runtime
            .objects_mut()
            .set_property(
                namespace,
                open_prop,
                RegisterValue::from_object_handle(open.0),
            )
            .map_err(|error| format!("failed to install otter:sql openSql export: {error:?}"))?;
        runtime
            .objects_mut()
            .set_property(
                namespace,
                sql_prop,
                RegisterValue::from_object_handle(sql.0),
            )
            .map_err(|error| format!("failed to install otter:sql sql export: {error:?}"))?;

        Ok(HostedNativeModule::Esm(namespace))
    }
}

fn sql_open(
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

    let db = SqlDatabase::open(&path).map_err(|error| sql_error(runtime, error))?;
    let is_memory = db.is_memory();
    let path_value = db.path().to_string().into_boxed_str();
    let object = runtime.alloc_native_object(SqlDatabasePayload {
        db: Some(db),
        path: path_value,
        adapter: "sqlite",
        is_memory,
        in_transaction: false,
        last_insert_row_id: None,
    });

    install_method(runtime, object, "query", 2, sql_query)?;
    install_method(runtime, object, "queryOne", 2, sql_query_one)?;
    install_method(runtime, object, "queryValue", 2, sql_query_value)?;
    install_method(runtime, object, "execute", 2, sql_execute)?;
    install_method(runtime, object, "executeMeta", 2, sql_execute_meta)?;
    install_method(runtime, object, "begin", 0, sql_begin)?;
    install_method(runtime, object, "commit", 0, sql_commit)?;
    install_method(runtime, object, "rollback", 0, sql_rollback)?;
    install_method(runtime, object, "close", 0, sql_close)?;
    install_getter(runtime, object, "adapter", sql_adapter)?;
    install_getter(runtime, object, "path", sql_path)?;
    install_getter(runtime, object, "isMemory", sql_is_memory)?;
    install_getter(runtime, object, "closed", sql_closed)?;
    install_getter(runtime, object, "inTransaction", sql_in_transaction)?;
    install_getter(runtime, object, "lastInsertRowId", sql_last_insert_row_id)?;

    Ok(RegisterValue::from_object_handle(object.0))
}

fn sql_query(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let sql = required_string_arg(runtime, args.first(), "sql.query: missing SQL string")?;
    let params = params_arg(runtime, args.get(1))?;
    let rows = with_database_mut(runtime, this, |db| db.query(&sql, &params))?;
    json_array_to_js(rows, runtime)
}

fn sql_execute(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let sql = required_string_arg(runtime, args.first(), "sql.execute: missing SQL string")?;
    let params = params_arg(runtime, args.get(1))?;
    let meta = with_database_payload_mut(runtime, this, |payload| {
        let db = payload.db.as_mut().ok_or(SqlError::Closed)?;
        let meta = db.execute_meta(&sql, &params)?;
        payload.last_insert_row_id = meta.last_insert_row_id;
        Ok(meta)
    })?;
    Ok(RegisterValue::from_number(meta.rows_affected as f64))
}

fn sql_execute_meta(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let sql = required_string_arg(runtime, args.first(), "sql.executeMeta: missing SQL string")?;
    let params = params_arg(runtime, args.get(1))?;
    let meta = with_database_payload_mut(runtime, this, |payload| {
        let db = payload.db.as_mut().ok_or(SqlError::Closed)?;
        let meta = db.execute_meta(&sql, &params)?;
        payload.last_insert_row_id = meta.last_insert_row_id;
        Ok(meta)
    })?;
    let object = runtime.alloc_object();
    let rows_affected = runtime.intern_property_name("rowsAffected");
    let last_insert_row_id = runtime.intern_property_name("lastInsertRowId");
    runtime
        .objects_mut()
        .set_property(
            object,
            rows_affected,
            RegisterValue::from_number(meta.rows_affected as f64),
        )
        .map_err(|error| {
            VmNativeCallError::Internal(
                format!("failed to materialize executeMeta rowsAffected: {error:?}").into(),
            )
        })?;
    runtime
        .objects_mut()
        .set_property(
            object,
            last_insert_row_id,
            meta.last_insert_row_id
                .and_then(|value| i32::try_from(value).ok().map(RegisterValue::from_i32))
                .unwrap_or_else(|| {
                    meta.last_insert_row_id
                        .map(|value| RegisterValue::from_number(value as f64))
                        .unwrap_or_else(RegisterValue::null)
                }),
        )
        .map_err(|error| {
            VmNativeCallError::Internal(
                format!("failed to materialize executeMeta lastInsertRowId: {error:?}").into(),
            )
        })?;
    Ok(RegisterValue::from_object_handle(object.0))
}

fn sql_query_one(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let sql = required_string_arg(runtime, args.first(), "sql.queryOne: missing SQL string")?;
    let params = params_arg(runtime, args.get(1))?;
    let row = with_database_mut(runtime, this, |db| db.query_one(&sql, &params))?;
    match row {
        Some(row) => json_to_register(&row, runtime, 0),
        None => Ok(RegisterValue::undefined()),
    }
}

fn sql_query_value(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let sql = required_string_arg(runtime, args.first(), "sql.queryValue: missing SQL string")?;
    let params = params_arg(runtime, args.get(1))?;
    let value = with_database_mut(runtime, this, |db| db.query_value(&sql, &params))?;
    match value {
        Some(value) => json_to_register(&value, runtime, 0),
        None => Ok(RegisterValue::undefined()),
    }
}

fn sql_begin(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    with_database_payload_mut(runtime, this, |payload| {
        let db = payload.db.as_mut().ok_or(SqlError::Closed)?;
        db.begin()?;
        payload.in_transaction = true;
        Ok(())
    })?;
    Ok(RegisterValue::undefined())
}

fn sql_commit(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    with_database_payload_mut(runtime, this, |payload| {
        let db = payload.db.as_mut().ok_or(SqlError::Closed)?;
        db.commit()?;
        payload.in_transaction = false;
        Ok(())
    })?;
    Ok(RegisterValue::undefined())
}

fn sql_rollback(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    with_database_payload_mut(runtime, this, |payload| {
        let db = payload.db.as_mut().ok_or(SqlError::Closed)?;
        db.rollback()?;
        payload.in_transaction = false;
        Ok(())
    })?;
    Ok(RegisterValue::undefined())
}

fn sql_close(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let payload = match runtime.native_payload_mut_from_value::<SqlDatabasePayload>(this) {
        Ok(payload) => payload,
        Err(_) => {
            return Err(throw_type_error(
                runtime,
                "sql.close: receiver is not a SQL database",
            ));
        }
    };
    payload.db.take();
    payload.in_transaction = false;
    Ok(RegisterValue::undefined())
}

fn sql_adapter(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let adapter = {
        let payload = match runtime.native_payload_mut_from_value::<SqlDatabasePayload>(this) {
            Ok(payload) => payload,
            Err(_) => {
                return Err(throw_type_error(
                    runtime,
                    "sql.adapter: receiver is not a SQL database",
                ));
            }
        };
        payload.adapter
    };
    Ok(RegisterValue::from_object_handle(
        runtime.alloc_string(adapter).0,
    ))
}

fn sql_path(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let path = {
        let payload = match runtime.native_payload_mut_from_value::<SqlDatabasePayload>(this) {
            Ok(payload) => payload,
            Err(_) => {
                return Err(throw_type_error(
                    runtime,
                    "sql.path: receiver is not a SQL database",
                ));
            }
        };
        payload.path.clone()
    };
    Ok(RegisterValue::from_object_handle(
        runtime.alloc_string(path).0,
    ))
}

fn sql_is_memory(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let is_memory = {
        let payload = match runtime.native_payload_mut_from_value::<SqlDatabasePayload>(this) {
            Ok(payload) => payload,
            Err(_) => {
                return Err(throw_type_error(
                    runtime,
                    "sql.isMemory: receiver is not a SQL database",
                ));
            }
        };
        payload.is_memory
    };
    Ok(RegisterValue::from_bool(is_memory))
}

fn sql_closed(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let closed = {
        let payload = match runtime.native_payload_mut_from_value::<SqlDatabasePayload>(this) {
            Ok(payload) => payload,
            Err(_) => {
                return Err(throw_type_error(
                    runtime,
                    "sql.closed: receiver is not a SQL database",
                ));
            }
        };
        payload.db.is_none()
    };
    Ok(RegisterValue::from_bool(closed))
}

fn sql_in_transaction(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let in_transaction = {
        let payload = match runtime.native_payload_mut_from_value::<SqlDatabasePayload>(this) {
            Ok(payload) => payload,
            Err(_) => {
                return Err(throw_type_error(
                    runtime,
                    "sql.inTransaction: receiver is not a SQL database",
                ));
            }
        };
        payload.in_transaction
    };
    Ok(RegisterValue::from_bool(in_transaction))
}

fn sql_last_insert_row_id(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let last_insert_row_id = {
        let payload = match runtime.native_payload_mut_from_value::<SqlDatabasePayload>(this) {
            Ok(payload) => payload,
            Err(_) => {
                return Err(throw_type_error(
                    runtime,
                    "sql.lastInsertRowId: receiver is not a SQL database",
                ));
            }
        };
        payload.last_insert_row_id
    };
    Ok(match last_insert_row_id {
        Some(value) => i32::try_from(value)
            .map(RegisterValue::from_i32)
            .unwrap_or_else(|_| RegisterValue::from_number(value as f64)),
        None => RegisterValue::null(),
    })
}

fn with_database_mut<T>(
    runtime: &mut RuntimeState,
    this: &RegisterValue,
    f: impl FnOnce(&mut SqlDatabase) -> SqlResult<T>,
) -> Result<T, VmNativeCallError> {
    let payload = match runtime.native_payload_mut_from_value::<SqlDatabasePayload>(this) {
        Ok(payload) => payload,
        Err(_) => return Err(throw_type_error(runtime, "receiver is not a SQL database")),
    };
    let db = match payload.db.as_mut() {
        Some(db) => db,
        None => return Err(throw_type_error(runtime, "SQL database is closed")),
    };
    f(db).map_err(|error| sql_error(runtime, error))
}

fn with_database_payload_mut<T>(
    runtime: &mut RuntimeState,
    this: &RegisterValue,
    f: impl FnOnce(&mut SqlDatabasePayload) -> SqlResult<T>,
) -> Result<T, VmNativeCallError> {
    let payload = match runtime.native_payload_mut_from_value::<SqlDatabasePayload>(this) {
        Ok(payload) => payload,
        Err(_) => return Err(throw_type_error(runtime, "receiver is not a SQL database")),
    };
    f(payload).map_err(|error| sql_error(runtime, error))
}

fn params_arg(
    runtime: &mut RuntimeState,
    value: Option<&RegisterValue>,
) -> Result<Vec<RusqliteParam>, VmNativeCallError> {
    let Some(value) = value.copied() else {
        return Ok(Vec::new());
    };
    if value == RegisterValue::undefined() || value == RegisterValue::null() {
        return Ok(Vec::new());
    }
    let handle = value
        .as_object_handle()
        .map(ObjectHandle)
        .ok_or_else(|| throw_type_error(runtime, "sql parameters must be an array"))?;
    if runtime.objects().kind(handle) != Ok(HeapValueKind::Array) {
        return Err(throw_type_error(runtime, "sql parameters must be an array"));
    }
    let args = runtime.array_to_args(handle)?;
    let mut seen = HashSet::new();
    let mut params = Vec::with_capacity(args.len());
    for arg in args {
        params.push(js_to_sql_param(arg, runtime, 0, &mut seen)?);
    }
    Ok(params)
}

fn js_to_sql_param(
    value: RegisterValue,
    runtime: &mut RuntimeState,
    depth: usize,
    seen: &mut HashSet<ObjectHandle>,
) -> Result<RusqliteParam, VmNativeCallError> {
    if depth > MAX_JSON_DEPTH {
        return Err(throw_type_error(
            runtime,
            "sql parameter exceeds maximum JSON nesting depth",
        ));
    }
    if value == RegisterValue::undefined() || value == RegisterValue::null() {
        return Ok(RusqliteParam(RusqliteValue::Null));
    }
    if let Some(boolean) = value.as_bool() {
        return Ok(RusqliteParam(RusqliteValue::Integer(if boolean {
            1
        } else {
            0
        })));
    }
    if let Some(number) = value.as_number() {
        if number.fract() == 0.0
            && number.is_finite()
            && number >= i64::MIN as f64
            && number <= i64::MAX as f64
        {
            return Ok(RusqliteParam(RusqliteValue::Integer(number as i64)));
        }
        return Ok(RusqliteParam(RusqliteValue::Real(number)));
    }
    let Some(handle) = value.as_object_handle().map(ObjectHandle) else {
        return Err(throw_type_error(runtime, "unsupported SQL parameter value"));
    };

    match runtime.objects().kind(handle) {
        Ok(HeapValueKind::String) => Ok(RusqliteParam(RusqliteValue::Text(
            runtime.js_to_string_infallible(value).into_string(),
        ))),
        Ok(HeapValueKind::Array | HeapValueKind::Object) => {
            let json = register_to_json(value, runtime, depth + 1, seen)?;
            Ok(RusqliteParam(RusqliteValue::Text(json.to_string())))
        }
        Ok(_) | Err(_) => Err(throw_type_error(
            runtime,
            "unsupported SQL parameter object",
        )),
    }
}

fn row_to_json(
    row: &Row<'_>,
    column_names: &[String],
    column_count: usize,
) -> rusqlite::Result<JsonValue> {
    let mut map = JsonMap::new();
    for (index, name) in column_names.iter().enumerate().take(column_count) {
        let value = match row.get_ref(index)? {
            ValueRef::Null => JsonValue::Null,
            ValueRef::Integer(value) => JsonValue::Number(value.into()),
            ValueRef::Real(value) => JsonNumber::from_f64(value)
                .map(JsonValue::Number)
                .unwrap_or(JsonValue::Null),
            ValueRef::Text(value) => {
                let text = String::from_utf8_lossy(value).to_string();
                if ((text.starts_with('{') && text.ends_with('}'))
                    || (text.starts_with('[') && text.ends_with(']')))
                    && let Ok(json) = serde_json::from_str(&text)
                {
                    json
                } else {
                    JsonValue::String(text)
                }
            }
            ValueRef::Blob(bytes) => JsonValue::Array(
                bytes
                    .iter()
                    .map(|byte| JsonValue::Number((*byte).into()))
                    .collect(),
            ),
        };
        map.insert(name.clone(), value);
    }
    Ok(JsonValue::Object(map))
}

fn value_ref_to_json(value: ValueRef<'_>) -> JsonValue {
    match value {
        ValueRef::Null => JsonValue::Null,
        ValueRef::Integer(value) => JsonValue::Number(value.into()),
        ValueRef::Real(value) => JsonNumber::from_f64(value)
            .map(JsonValue::Number)
            .unwrap_or(JsonValue::Null),
        ValueRef::Text(value) => {
            let text = String::from_utf8_lossy(value).to_string();
            if ((text.starts_with('{') && text.ends_with('}'))
                || (text.starts_with('[') && text.ends_with(']')))
                && let Ok(json) = serde_json::from_str(&text)
            {
                json
            } else {
                JsonValue::String(text)
            }
        }
        ValueRef::Blob(bytes) => JsonValue::Array(
            bytes
                .iter()
                .map(|byte| JsonValue::Number((*byte).into()))
                .collect(),
        ),
    }
}

fn json_array_to_js(
    rows: Vec<JsonValue>,
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let mut values = Vec::with_capacity(rows.len());
    for row in rows {
        values.push(json_to_register(&row, runtime, 0)?);
    }
    Ok(RegisterValue::from_object_handle(
        runtime.alloc_array_with_elements(&values).0,
    ))
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
            "value exceeds maximum JSON nesting depth",
        ));
    }
    if value == RegisterValue::undefined() {
        return Err(throw_type_error(
            runtime,
            "undefined values are not supported",
        ));
    }
    if value == RegisterValue::null() {
        return Ok(JsonValue::Null);
    }
    if let Some(boolean) = value.as_bool() {
        return Ok(JsonValue::Bool(boolean));
    }
    if let Some(number) = value.as_number() {
        let number = JsonNumber::from_f64(number)
            .ok_or_else(|| throw_type_error(runtime, "non-finite numbers are not supported"))?;
        return Ok(JsonValue::Number(number));
    }

    let Some(handle) = value.as_object_handle().map(ObjectHandle) else {
        return Err(throw_type_error(runtime, "unsupported value type"));
    };
    if !seen.insert(handle) {
        return Err(throw_type_error(runtime, "cyclic values are not supported"));
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
        Ok(_) | Err(_) => Err(throw_type_error(runtime, "unsupported object type")),
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
            "value exceeds maximum JSON nesting depth",
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
            Ok(RegisterValue::from_number(number.as_f64().ok_or_else(
                || throw_type_error(runtime, "invalid numeric value"),
            )?))
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
                            format!("failed to materialize SQL row property: {error:?}").into(),
                        )
                    })?;
            }
            Ok(RegisterValue::from_object_handle(object.0))
        }
    }
}

fn install_method(
    runtime: &mut RuntimeState,
    target: ObjectHandle,
    name: &str,
    arity: u16,
    callback: fn(
        &RegisterValue,
        &[RegisterValue],
        &mut RuntimeState,
    ) -> Result<RegisterValue, VmNativeCallError>,
) -> Result<(), VmNativeCallError> {
    let function = alloc_named_function(runtime, name, arity, callback);
    let property = runtime.intern_property_name(name);
    runtime
        .objects_mut()
        .set_property(
            target,
            property,
            RegisterValue::from_object_handle(function.0),
        )
        .map_err(|error| {
            VmNativeCallError::Internal(
                format!("failed to install sql method '{name}': {error:?}").into(),
            )
        })?;
    Ok(())
}

fn install_getter(
    runtime: &mut RuntimeState,
    target: ObjectHandle,
    name: &str,
    callback: fn(
        &RegisterValue,
        &[RegisterValue],
        &mut RuntimeState,
    ) -> Result<RegisterValue, VmNativeCallError>,
) -> Result<(), VmNativeCallError> {
    let descriptor = NativeFunctionDescriptor::getter(name, callback);
    let getter_id = runtime.register_native_function(descriptor);
    let getter = runtime.alloc_host_function(getter_id);
    let property = runtime.intern_property_name(name);
    runtime
        .objects_mut()
        .define_accessor(target, property, Some(getter), None)
        .map_err(|error| {
            VmNativeCallError::Internal(
                format!("failed to install sql getter '{name}': {error:?}").into(),
            )
        })?;
    Ok(())
}

fn alloc_named_function(
    runtime: &mut RuntimeState,
    name: &str,
    arity: u16,
    callback: fn(
        &RegisterValue,
        &[RegisterValue],
        &mut RuntimeState,
    ) -> Result<RegisterValue, VmNativeCallError>,
) -> ObjectHandle {
    let descriptor = NativeFunctionDescriptor::method(name, arity, callback);
    let function = runtime.register_native_function(descriptor);
    runtime.alloc_host_function(function)
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

fn sql_error(runtime: &mut RuntimeState, error: SqlError) -> VmNativeCallError {
    throw_type_error(runtime, &error.to_string())
}

fn sqlite_error(error: rusqlite::Error) -> SqlError {
    SqlError::Sqlite(error.to_string())
}

fn next_memory_backing_path() -> PathBuf {
    let unique = MEMORY_DB_ID.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "otter-modules-sql-{}-{}.db",
        std::process::id(),
        unique
    ))
}
