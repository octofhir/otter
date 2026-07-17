//! `otter:sql` SQLite host access.
//!
//! This slice keeps SQLite state as owned Rust data and checks filesystem
//! capabilities before opening a database path.

use std::path::{Path, PathBuf};

use otter_runtime::CapabilitySet;
use otter_runtime::{
    RuntimeAttr as Attr, RuntimeHostObjectError, RuntimeJsObject as JsObject, RuntimeLocal,
    RuntimeNativeCtx as NativeCtx, RuntimeNativeError as NativeError, RuntimeNativeScope,
    RuntimeValue as Value, runtime_this_object, runtime_with_host_data_mut,
};
use rusqlite::types::{ToSqlOutput, Value as SqliteValue, ValueRef};
use rusqlite::{Connection, OpenFlags};
use serde_json::{Map as JsonMap, Number as JsonNumber, Value as JsonValue};

/// Errors produced by `otter:sql`.
#[derive(Debug, thiserror::Error)]
pub enum SqlError {
    /// Filesystem permission denied.
    #[error("permission denied for `{path}`")]
    PermissionDenied {
        /// Path that was rejected.
        path: PathBuf,
    },
    /// SQLite error.
    #[error("sqlite error: {0}")]
    Sqlite(String),
    /// Query parameters must be scalar JSON values.
    #[error("unsupported SQL parameter")]
    UnsupportedParam,
}

/// Result alias for `otter:sql`.
pub type SqlResult<T> = Result<T, SqlError>;

/// Permission-gated SQLite database.
#[derive(Debug)]
pub struct SqlDatabase {
    conn: Connection,
    path: Option<PathBuf>,
}

impl SqlDatabase {
    /// Open an in-memory SQLite database.
    pub fn memory() -> SqlResult<Self> {
        let conn = Connection::open_in_memory().map_err(sqlite_error)?;
        configure(&conn)?;
        Ok(Self { conn, path: None })
    }

    /// Open a file-backed SQLite database after read/write checks.
    pub fn open(path: impl AsRef<Path>, capabilities: &CapabilitySet) -> SqlResult<Self> {
        let path = path.as_ref().to_path_buf();
        if !capabilities.read.matches_path(&path) || !capabilities.write.matches_path(&path) {
            return Err(SqlError::PermissionDenied { path });
        }
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|err| SqlError::Sqlite(err.to_string()))?;
        }
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_FULL_MUTEX;
        let conn = Connection::open_with_flags(&path, flags).map_err(sqlite_error)?;
        configure(&conn)?;
        Ok(Self {
            conn,
            path: Some(path),
        })
    }

    /// Open path, if this database is file-backed.
    #[must_use]
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Execute SQL and return affected rows.
    pub fn execute(&mut self, sql: &str, params: &[JsonValue]) -> SqlResult<u64> {
        let params = convert_params(params)?;
        let refs: Vec<&dyn rusqlite::ToSql> = params
            .iter()
            .map(|param| param as &dyn rusqlite::ToSql)
            .collect();
        self.conn
            .execute(sql, refs.as_slice())
            .map(|rows| rows as u64)
            .map_err(sqlite_error)
    }

    /// Query rows as JSON objects.
    pub fn query(&mut self, sql: &str, params: &[JsonValue]) -> SqlResult<Vec<JsonValue>> {
        let params = convert_params(params)?;
        let refs: Vec<&dyn rusqlite::ToSql> = params
            .iter()
            .map(|param| param as &dyn rusqlite::ToSql)
            .collect();
        let mut stmt = self.conn.prepare(sql).map_err(sqlite_error)?;
        let names: Vec<String> = stmt
            .column_names()
            .iter()
            .map(|name| name.to_string())
            .collect();
        let count = names.len();
        let rows = stmt
            .query_map(refs.as_slice(), |row| {
                let mut out = JsonMap::new();
                for (idx, name) in names.iter().enumerate().take(count) {
                    out.insert(name.clone(), sqlite_value_to_json(row.get_ref(idx)?));
                }
                Ok(JsonValue::Object(out))
            })
            .map_err(sqlite_error)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(sqlite_error)?);
        }
        Ok(out)
    }

    /// Query one row.
    pub fn query_one(&mut self, sql: &str, params: &[JsonValue]) -> SqlResult<Option<JsonValue>> {
        Ok(self.query(sql, params)?.into_iter().next())
    }
}

fn configure(conn: &Connection) -> SqlResult<()> {
    conn.execute_batch("PRAGMA foreign_keys = ON;")
        .map_err(sqlite_error)
}

fn sqlite_error(error: rusqlite::Error) -> SqlError {
    SqlError::Sqlite(error.to_string())
}

#[derive(Debug, Clone)]
struct Param(SqliteValue);

impl rusqlite::ToSql for Param {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::Owned(self.0.clone()))
    }
}

fn convert_params(params: &[JsonValue]) -> SqlResult<Vec<Param>> {
    params
        .iter()
        .map(|value| match value {
            JsonValue::Null => Ok(Param(SqliteValue::Null)),
            JsonValue::Bool(value) => Ok(Param(SqliteValue::Integer(i64::from(*value)))),
            JsonValue::Number(value) => {
                if let Some(i) = value.as_i64() {
                    Ok(Param(SqliteValue::Integer(i)))
                } else if let Some(f) = value.as_f64() {
                    Ok(Param(SqliteValue::Real(f)))
                } else {
                    Err(SqlError::UnsupportedParam)
                }
            }
            JsonValue::String(value) => Ok(Param(SqliteValue::Text(value.clone()))),
            JsonValue::Array(_) | JsonValue::Object(_) => Err(SqlError::UnsupportedParam),
        })
        .collect()
}

fn sqlite_value_to_json(value: ValueRef<'_>) -> JsonValue {
    match value {
        ValueRef::Null => JsonValue::Null,
        ValueRef::Integer(value) => JsonValue::Number(JsonNumber::from(value)),
        ValueRef::Real(value) => JsonNumber::from_f64(value)
            .map(JsonValue::Number)
            .unwrap_or(JsonValue::Null),
        ValueRef::Text(value) => JsonValue::String(String::from_utf8_lossy(value).into_owned()),
        ValueRef::Blob(value) => JsonValue::Array(
            value
                .iter()
                .map(|byte| JsonValue::Number(JsonNumber::from(*byte)))
                .collect(),
        ),
    }
}

otter_macros::lodge! {
    prefix = "otter",
    name = "sql",
    capabilities = true,
    exports = {
        "openSql" / 1 => open_sql,
        "sql"     / 1 => open_sql,
    },
}

fn open_sql(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    capabilities: &CapabilitySet,
) -> Result<Value, NativeError> {
    let path = crate::arg_string(args, 0, "openSql", ctx.heap())?;
    let db = if path.is_empty() || path == ":memory:" {
        SqlDatabase::memory().map_err(|err| crate::type_error("openSql", err.to_string()))?
    } else {
        SqlDatabase::open(&path, capabilities)
            .map_err(|err| crate::type_error("openSql", err.to_string()))?
    };
    ctx.scope(|mut scope| {
        let object = build_database_object(&mut scope, db)?;
        Ok(scope.finish(object))
    })
}

fn build_database_object<'scope>(
    scope: &mut RuntimeNativeScope<'scope, '_>,
    db: SqlDatabase,
) -> Result<RuntimeLocal<'scope>, NativeError> {
    let object = scope.host_object(db)?;
    let attrs = Attr::builtin_function().to_flags();
    for (name, length, call) in [
        (
            "execute",
            1,
            method_execute as otter_runtime::RuntimeNativeFastFn,
        ),
        ("query", 1, method_query),
        ("queryOne", 1, method_query_one),
    ] {
        let method = scope.native_method(name, length, call)?;
        scope.define(object, name, method, attrs)?;
    }
    Ok(object)
}

fn database_receiver(ctx: &NativeCtx<'_>, name: &'static str) -> Result<JsObject, NativeError> {
    runtime_this_object(ctx, name, "SqlDatabase")
}

fn host_error(name: &'static str, err: RuntimeHostObjectError) -> NativeError {
    crate::type_error(name, err.to_string())
}

fn rest_args(args: &[Value]) -> &[Value] {
    args.get(1..).unwrap_or(&[])
}

fn method_execute(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let object = database_receiver(ctx, "SqlDatabase.execute")?;
    let sql = crate::arg_string(args, 0, "SqlDatabase.execute", ctx.heap())?;
    let params = js_params(rest_args(args), ctx.heap())?;
    let result =
        runtime_with_host_data_mut::<SqlDatabase, _>(ctx, object, |db| db.execute(&sql, &params))
            .map_err(|err| host_error("SqlDatabase.execute", err))?;
    let affected =
        result.map_err(|err| crate::type_error("SqlDatabase.execute", err.to_string()))?;
    Ok(Value::number_f64(affected as f64))
}

fn method_query(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let object = database_receiver(ctx, "SqlDatabase.query")?;
    let sql = crate::arg_string(args, 0, "SqlDatabase.query", ctx.heap())?;
    let params = js_params(rest_args(args), ctx.heap())?;
    let result =
        runtime_with_host_data_mut::<SqlDatabase, _>(ctx, object, |db| db.query(&sql, &params))
            .map_err(|err| host_error("SqlDatabase.query", err))?;
    let rows = result.map_err(|err| crate::type_error("SqlDatabase.query", err.to_string()))?;
    json_rows_to_array(ctx, rows)
}

fn method_query_one(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let object = database_receiver(ctx, "SqlDatabase.queryOne")?;
    let sql = crate::arg_string(args, 0, "SqlDatabase.queryOne", ctx.heap())?;
    let params = js_params(rest_args(args), ctx.heap())?;
    let result =
        runtime_with_host_data_mut::<SqlDatabase, _>(ctx, object, |db| db.query_one(&sql, &params))
            .map_err(|err| host_error("SqlDatabase.queryOne", err))?;
    match result.map_err(|err| crate::type_error("SqlDatabase.queryOne", err.to_string()))? {
        Some(row) => ctx.scope(|mut scope| {
            let object = scoped_json_row_to_object(&mut scope, row)?;
            Ok::<Value, NativeError>(scope.finish(object))
        }),
        None => Ok(Value::null()),
    }
}

fn js_params(
    values: &[Value],
    heap: &otter_runtime::otter_gc::GcHeap,
) -> Result<Vec<JsonValue>, NativeError> {
    values
        .iter()
        .map(|v| crate::value_to_json(v, heap))
        .collect()
}

fn json_rows_to_array(ctx: &mut NativeCtx<'_>, rows: Vec<JsonValue>) -> Result<Value, NativeError> {
    // Each row object and the backing array are separate allocations, and each
    // row itself allocates per field. Collecting the row objects into a `Vec`
    // left every earlier object unrooted across the later allocations. Build the
    // array through the scope, storing each row into the (rooted) array before
    // an inner scope drops the row's transient field handles.
    ctx.scope(|mut scope| {
        let array = scope.array(rows.len())?;
        for (index, row) in rows.into_iter().enumerate() {
            scope.scope(|mut row_scope| {
                let object = scoped_json_row_to_object(&mut row_scope, row)?;
                row_scope.set_index(array, index, object)
            })?;
        }
        Ok::<Value, NativeError>(scope.finish(array))
    })
}

/// Build a JS object for a SQL result row, parking it and every field value in
/// scope `s`. The object is created first and every field is written through the
/// arena, so a moving collection driven by a later field allocation can never
/// strand the object or an earlier field.
fn scoped_json_row_to_object<'s>(
    scope: &mut RuntimeNativeScope<'s, '_>,
    row: JsonValue,
) -> Result<RuntimeLocal<'s>, NativeError> {
    let object = scope.object()?;
    let JsonValue::Object(map) = row else {
        return Err(crate::type_error(
            "SqlDatabase.query",
            "row is not an object",
        ));
    };
    for (name, value) in map {
        let value = scoped_json_to_value(scope, value)?;
        scope.set(object, &name, value)?;
    }
    Ok(object)
}

/// Scoped counterpart of [`crate::json_to_value`]: convert a JSON scalar to a JS
/// value parked in scope `s`. Only the string arm allocates; parking keeps every
/// arm reading like an ordinary scoped creation so the caller never holds a raw
/// handle across a sibling allocation.
fn scoped_json_to_value<'s>(
    scope: &mut RuntimeNativeScope<'s, '_>,
    value: JsonValue,
) -> Result<RuntimeLocal<'s>, NativeError> {
    match value {
        JsonValue::Null => Ok(scope.null()),
        JsonValue::Bool(b) => Ok(scope.boolean(b)),
        JsonValue::Number(n) => Ok(scope.number(n.as_f64().unwrap_or(f64::NAN))),
        JsonValue::String(text) => scope.string(&text),
        other => scope.string(&other.to_string()),
    }
}
