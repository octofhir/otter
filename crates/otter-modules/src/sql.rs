//! `otter:sql` SQLite host access.
//!
//! This slice keeps SQLite state as owned Rust data and checks filesystem
//! capabilities before opening a database path.

use std::path::{Path, PathBuf};

use otter_runtime::CapabilitySet;
use otter_runtime::{
    RuntimeHostObjectError, RuntimeJsObject as JsObject, RuntimeNativeCtx as NativeCtx,
    RuntimeNativeError as NativeError, RuntimeObjectBuilder as ObjectBuilder,
    RuntimeValue as Value, runtime_alloc_object, runtime_array_from_elements, runtime_set_property,
    runtime_this_object, runtime_with_host_data_mut,
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
    let object = build_database_object(ctx, db)?;
    Ok(Value::object(object))
}

fn build_database_object(
    ctx: &mut NativeCtx<'_>,
    db: SqlDatabase,
) -> Result<JsObject, NativeError> {
    let mut builder = ObjectBuilder::from_host_data(ctx, db)?;
    builder
        .builtin_method("execute", 1, method_execute)
        .map_err(|err| crate::type_error("SqlDatabase", err.to_string()))?
        .builtin_method("query", 1, method_query)
        .map_err(|err| crate::type_error("SqlDatabase", err.to_string()))?
        .builtin_method("queryOne", 1, method_query_one)
        .map_err(|err| crate::type_error("SqlDatabase", err.to_string()))?;
    Ok(builder.build())
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
        Some(row) => json_row_to_object(ctx, row).map(Value::object),
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
    let values = rows
        .into_iter()
        .map(|row| json_row_to_object(ctx, row).map(Value::object))
        .collect::<Result<Vec<_>, _>>()?;
    let arr = runtime_array_from_elements(ctx, values)?;
    Ok(Value::array(arr))
}

fn json_row_to_object(ctx: &mut NativeCtx<'_>, row: JsonValue) -> Result<JsObject, NativeError> {
    let object = runtime_alloc_object(ctx)?;
    let JsonValue::Object(map) = row else {
        return Err(crate::type_error(
            "SqlDatabase.query",
            "row is not an object",
        ));
    };
    for (name, value) in map {
        let value = crate::json_to_value(ctx, value)?;
        runtime_set_property(ctx, object, &name, value);
    }
    Ok(object)
}
