//! SQL extension for Otter runtime
//!
//! Registers the SQL ops with the runtime and provides JS interop.

use crate::SQL_JS;
use crate::adapter::{CopyFormat, CopyFromOptions, CopyStream, CopyToOptions, SharedAdapter};
use crate::postgres::PostgresAdapter;
use crate::query::{ParamStyle, QueryBuilder};
use crate::sqlite::SqliteAdapter;
use otter_runtime::Extension;
use otter_runtime::error::{JscError, JscResult};
use otter_runtime::extension::{OpContext, op_async, op_sync};
use parking_lot::RwLock;
use serde_json::{Value as JsonValue, json};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::Mutex as TokioMutex;

/// Helper to convert any error to JscError
fn sql_error(msg: impl Into<String>) -> JscError {
    JscError::internal(msg)
}

/// Connection ID counter
static CONNECTION_ID: AtomicU64 = AtomicU64::new(1);

/// Connection registry
static CONNECTIONS: once_cell::sync::Lazy<RwLock<HashMap<u64, SharedAdapter>>> =
    once_cell::sync::Lazy::new(|| RwLock::new(HashMap::new()));

/// Default connection (from DATABASE_URL or :memory:)
static DEFAULT_CONNECTION: once_cell::sync::Lazy<RwLock<Option<SharedAdapter>>> =
    once_cell::sync::Lazy::new(|| RwLock::new(None));

/// Stream ID counter for COPY TO streaming
static STREAM_ID: AtomicU64 = AtomicU64::new(1);

/// Active COPY TO streams
type BoxedCopyStream = Box<dyn CopyStream + Send>;
static COPY_STREAMS: once_cell::sync::Lazy<RwLock<HashMap<u64, Arc<TokioMutex<BoxedCopyStream>>>>> =
    once_cell::sync::Lazy::new(|| RwLock::new(HashMap::new()));

/// Create the SQL extension
pub fn sql_extension() -> Extension {
    Extension::new("otter-sql")
        .with_ops(vec![
            // Connection management
            op_async("__otter_sql_connect", sql_connect),
            op_sync("__otter_sql_close", sql_close),
            op_sync("__otter_sql_get_default", sql_get_default),
            // Query execution
            op_async("__otter_sql_query", sql_query),
            op_async("__otter_sql_execute", sql_execute),
            // Transactions
            op_async("__otter_sql_begin", sql_begin),
            op_async("__otter_sql_commit", sql_commit),
            op_async("__otter_sql_rollback", sql_rollback),
            op_async("__otter_sql_savepoint", sql_savepoint),
            // COPY operations
            op_async("__otter_sql_copy_from", sql_copy_from),
            op_async("__otter_sql_copy_to", sql_copy_to),
            // Streaming COPY TO
            op_async("__otter_sql_copy_to_start", sql_copy_to_start),
            op_async("__otter_sql_copy_to_read", sql_copy_to_read),
            op_sync("__otter_sql_copy_to_close", sql_copy_to_close),
        ])
        .with_js(SQL_JS)
}

/// Helper to get first argument as object
fn get_arg(args: &[JsonValue]) -> Option<&JsonValue> {
    args.first()
}

/// Connect to a database
async fn sql_connect(_ctx: OpContext, args: Vec<JsonValue>) -> JscResult<JsonValue> {
    let arg = get_arg(&args).ok_or_else(|| sql_error("Missing arguments"))?;
    let url = arg
        .get("url")
        .and_then(|v| v.as_str())
        .ok_or_else(|| sql_error("Missing url"))?;

    let adapter: SharedAdapter = if url == ":memory:"
        || url.starts_with("sqlite://")
        || url.ends_with(".db")
        || url.ends_with(".sqlite")
        || url.ends_with(".sqlite3")
    {
        Arc::new(SqliteAdapter::open(url).map_err(|e| sql_error(e.to_string()))?)
    } else if url.starts_with("postgres://") || url.starts_with("postgresql://") {
        Arc::new(
            PostgresAdapter::connect(url)
                .await
                .map_err(|e| sql_error(e.to_string()))?,
        )
    } else {
        return Err(sql_error(format!("Unknown database URL format: {}", url)));
    };

    let id = CONNECTION_ID.fetch_add(1, Ordering::SeqCst);
    CONNECTIONS.write().insert(id, adapter.clone());

    Ok(json!({
        "id": id,
        "adapter": adapter.adapter_type()
    }))
}

/// Close a connection
fn sql_close(_ctx: OpContext, args: Vec<JsonValue>) -> JscResult<JsonValue> {
    let arg = get_arg(&args).ok_or_else(|| sql_error("Missing arguments"))?;
    let id = arg
        .get("id")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| sql_error("Missing connection id"))?;

    if CONNECTIONS.write().remove(&id).is_some() {
        Ok(json!(true))
    } else {
        Ok(json!(false))
    }
}

/// Get or create default connection
fn sql_get_default(_ctx: OpContext, _args: Vec<JsonValue>) -> JscResult<JsonValue> {
    // Check if default already exists
    if let Some(ref adapter) = *DEFAULT_CONNECTION.read() {
        // Find the ID for this adapter (if registered)
        let connections = CONNECTIONS.read();
        for (id, conn) in connections.iter() {
            if Arc::ptr_eq(conn, adapter) {
                return Ok(json!({
                    "id": *id,
                    "adapter": adapter.adapter_type()
                }));
            }
        }
    }

    // Create default connection
    let url = std::env::var("DATABASE_URL").unwrap_or_else(|_| ":memory:".to_string());

    // For sync creation, we can only use SQLite here
    // PostgreSQL needs async connect
    if url == ":memory:"
        || url.starts_with("sqlite://")
        || url.ends_with(".db")
        || url.ends_with(".sqlite")
    {
        let adapter: SharedAdapter =
            Arc::new(SqliteAdapter::open(&url).map_err(|e| sql_error(e.to_string()))?);

        let id = CONNECTION_ID.fetch_add(1, Ordering::SeqCst);
        CONNECTIONS.write().insert(id, adapter.clone());
        *DEFAULT_CONNECTION.write() = Some(adapter.clone());

        Ok(json!({
            "id": id,
            "adapter": adapter.adapter_type()
        }))
    } else {
        // For PostgreSQL, return a special marker
        Ok(json!({
            "id": null,
            "url": url,
            "needsAsyncConnect": true
        }))
    }
}

/// Execute a query
async fn sql_query(_ctx: OpContext, args: Vec<JsonValue>) -> JscResult<JsonValue> {
    let arg = get_arg(&args).ok_or_else(|| sql_error("Missing arguments"))?;
    let id = arg
        .get("id")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| sql_error("Missing connection id"))?;

    let strings = arg
        .get("strings")
        .and_then(|v| v.as_array())
        .ok_or_else(|| sql_error("Missing strings"))?
        .iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect::<Vec<_>>();

    let values = arg
        .get("values")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let adapter = CONNECTIONS
        .read()
        .get(&id)
        .cloned()
        .ok_or_else(|| sql_error("Connection not found"))?;

    let param_style = if adapter.adapter_type() == "sqlite" {
        ParamStyle::Positional
    } else {
        ParamStyle::Dollar
    };

    let query = QueryBuilder::from_template(&strings, &values, param_style);
    let (sql, params) = query.into_parts();

    let result = adapter
        .query(&sql, &params)
        .await
        .map_err(|e| sql_error(e.to_string()))?;

    let format = arg
        .get("format")
        .and_then(|v| v.as_str())
        .unwrap_or("objects");

    match format {
        "values" => Ok(result.to_values_arrays()),
        _ => Ok(result.to_json_array()),
    }
}

/// Execute a statement
async fn sql_execute(_ctx: OpContext, args: Vec<JsonValue>) -> JscResult<JsonValue> {
    let arg = get_arg(&args).ok_or_else(|| sql_error("Missing arguments"))?;
    let id = arg
        .get("id")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| sql_error("Missing connection id"))?;

    let strings = arg
        .get("strings")
        .and_then(|v| v.as_array())
        .ok_or_else(|| sql_error("Missing strings"))?
        .iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect::<Vec<_>>();

    let values = arg
        .get("values")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let adapter = CONNECTIONS
        .read()
        .get(&id)
        .cloned()
        .ok_or_else(|| sql_error("Connection not found"))?;

    let param_style = if adapter.adapter_type() == "sqlite" {
        ParamStyle::Positional
    } else {
        ParamStyle::Dollar
    };

    let query = QueryBuilder::from_template(&strings, &values, param_style);
    let (sql, params) = query.into_parts();

    let rows_affected = adapter
        .execute(&sql, &params)
        .await
        .map_err(|e| sql_error(e.to_string()))?;

    Ok(json!({ "rowsAffected": rows_affected }))
}

/// Begin a transaction
async fn sql_begin(_ctx: OpContext, args: Vec<JsonValue>) -> JscResult<JsonValue> {
    let arg = get_arg(&args).ok_or_else(|| sql_error("Missing arguments"))?;
    let id = arg
        .get("id")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| sql_error("Missing connection id"))?;

    let adapter = CONNECTIONS
        .read()
        .get(&id)
        .cloned()
        .ok_or_else(|| sql_error("Connection not found"))?;

    // For now, we don't store transaction handles - the JS wrapper manages this
    // by using BEGIN/COMMIT/ROLLBACK statements
    adapter
        .execute("BEGIN", &[])
        .await
        .map_err(|e| sql_error(e.to_string()))?;

    Ok(json!(true))
}

/// Commit a transaction
async fn sql_commit(_ctx: OpContext, args: Vec<JsonValue>) -> JscResult<JsonValue> {
    let arg = get_arg(&args).ok_or_else(|| sql_error("Missing arguments"))?;
    let id = arg
        .get("id")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| sql_error("Missing connection id"))?;

    let adapter = CONNECTIONS
        .read()
        .get(&id)
        .cloned()
        .ok_or_else(|| sql_error("Connection not found"))?;

    adapter
        .execute("COMMIT", &[])
        .await
        .map_err(|e| sql_error(e.to_string()))?;

    Ok(json!(true))
}

/// Rollback a transaction
async fn sql_rollback(_ctx: OpContext, args: Vec<JsonValue>) -> JscResult<JsonValue> {
    let arg = get_arg(&args).ok_or_else(|| sql_error("Missing arguments"))?;
    let id = arg
        .get("id")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| sql_error("Missing connection id"))?;

    let adapter = CONNECTIONS
        .read()
        .get(&id)
        .cloned()
        .ok_or_else(|| sql_error("Connection not found"))?;

    adapter
        .execute("ROLLBACK", &[])
        .await
        .map_err(|e| sql_error(e.to_string()))?;

    Ok(json!(true))
}

/// Create a savepoint
async fn sql_savepoint(_ctx: OpContext, args: Vec<JsonValue>) -> JscResult<JsonValue> {
    let arg = get_arg(&args).ok_or_else(|| sql_error("Missing arguments"))?;
    let id = arg
        .get("id")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| sql_error("Missing connection id"))?;

    let name = arg
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| sql_error("Missing savepoint name"))?;

    let adapter = CONNECTIONS
        .read()
        .get(&id)
        .cloned()
        .ok_or_else(|| sql_error("Connection not found"))?;

    let sql = format!("SAVEPOINT \"{}\"", name.replace('"', "\"\""));
    adapter
        .execute(&sql, &[])
        .await
        .map_err(|e| sql_error(e.to_string()))?;

    Ok(json!(true))
}

/// COPY FROM operation
async fn sql_copy_from(_ctx: OpContext, args: Vec<JsonValue>) -> JscResult<JsonValue> {
    let arg = get_arg(&args).ok_or_else(|| sql_error("Missing arguments"))?;
    let id = arg
        .get("id")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| sql_error("Missing connection id"))?;

    let table = arg
        .get("table")
        .and_then(|v| v.as_str())
        .ok_or_else(|| sql_error("Missing table"))?
        .to_string();

    let columns = arg.get("columns").and_then(|v| v.as_array()).map(|arr| {
        arr.iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect()
    });

    let format = arg
        .get("format")
        .and_then(|v| v.as_str())
        .map(CopyFormat::from_str)
        .unwrap_or_default();

    let header = arg.get("header").and_then(|v| v.as_bool()).unwrap_or(false);

    let delimiter = arg
        .get("delimiter")
        .and_then(|v| v.as_str())
        .and_then(|s| s.chars().next());

    let data = arg
        .get("data")
        .and_then(|v| v.as_str())
        .ok_or_else(|| sql_error("Missing data"))?;

    let adapter = CONNECTIONS
        .read()
        .get(&id)
        .cloned()
        .ok_or_else(|| sql_error("Connection not found"))?;

    let options = CopyFromOptions {
        table,
        columns,
        format,
        header,
        delimiter,
        null_string: None,
        quote: None,
        escape: None,
    };

    let mut sink = adapter
        .copy_from(options)
        .await
        .map_err(|e| sql_error(e.to_string()))?;

    sink.send(data.as_bytes())
        .await
        .map_err(|e| sql_error(e.to_string()))?;
    let rows = sink.finish().await.map_err(|e| sql_error(e.to_string()))?;

    Ok(json!({ "rowsCopied": rows }))
}

/// COPY TO operation
async fn sql_copy_to(_ctx: OpContext, args: Vec<JsonValue>) -> JscResult<JsonValue> {
    let arg = get_arg(&args).ok_or_else(|| sql_error("Missing arguments"))?;
    let id = arg
        .get("id")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| sql_error("Missing connection id"))?;

    let table_or_query = arg
        .get("table")
        .or_else(|| arg.get("query"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| sql_error("Missing table or query"))?
        .to_string();

    let is_query = arg.get("query").is_some();

    let columns = arg.get("columns").and_then(|v| v.as_array()).map(|arr| {
        arr.iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect()
    });

    let format = arg
        .get("format")
        .and_then(|v| v.as_str())
        .map(CopyFormat::from_str)
        .unwrap_or_default();

    let header = arg.get("header").and_then(|v| v.as_bool()).unwrap_or(false);

    let delimiter = arg
        .get("delimiter")
        .and_then(|v| v.as_str())
        .and_then(|s| s.chars().next());

    let adapter = CONNECTIONS
        .read()
        .get(&id)
        .cloned()
        .ok_or_else(|| sql_error("Connection not found"))?;

    let options = CopyToOptions {
        table_or_query,
        is_query,
        columns,
        format,
        header,
        delimiter,
        null_string: None,
    };

    let mut stream = adapter
        .copy_to(options)
        .await
        .map_err(|e| sql_error(e.to_string()))?;

    // Collect all chunks into a single buffer
    let mut data = Vec::new();
    while let Some(chunk) = stream.next().await.map_err(|e| sql_error(e.to_string()))? {
        data.extend_from_slice(&chunk);
    }

    // Return as base64 for binary, string for text/csv
    if format == CopyFormat::Binary {
        Ok(json!({
            "data": base64_encode(&data),
            "encoding": "base64"
        }))
    } else {
        let text = String::from_utf8_lossy(&data).to_string();
        Ok(json!({
            "data": text,
            "encoding": "utf8"
        }))
    }
}

fn base64_encode(data: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut result = String::with_capacity((data.len() + 2) / 3 * 4);

    for chunk in data.chunks(3) {
        let mut n = 0u32;
        for (i, &byte) in chunk.iter().enumerate() {
            n |= (byte as u32) << (16 - i * 8);
        }

        let chars = match chunk.len() {
            3 => 4,
            2 => 3,
            1 => 2,
            _ => unreachable!(),
        };

        for i in 0..chars {
            let idx = ((n >> (18 - i * 6)) & 0x3f) as usize;
            result.push(ALPHABET[idx] as char);
        }

        for _ in chars..4 {
            result.push('=');
        }
    }

    result
}

// ============================================
// Streaming COPY TO operations
// ============================================

/// Start a streaming COPY TO operation
async fn sql_copy_to_start(_ctx: OpContext, args: Vec<JsonValue>) -> JscResult<JsonValue> {
    let arg = get_arg(&args).ok_or_else(|| sql_error("Missing arguments"))?;
    let id = arg
        .get("id")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| sql_error("Missing connection id"))?;

    let table_or_query = arg
        .get("table")
        .or_else(|| arg.get("query"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| sql_error("Missing table or query"))?
        .to_string();

    let is_query = arg.get("query").is_some();

    let columns = arg.get("columns").and_then(|v| v.as_array()).map(|arr| {
        arr.iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect()
    });

    let format = arg
        .get("format")
        .and_then(|v| v.as_str())
        .map(CopyFormat::from_str)
        .unwrap_or_default();

    let header = arg.get("header").and_then(|v| v.as_bool()).unwrap_or(false);

    let delimiter = arg
        .get("delimiter")
        .and_then(|v| v.as_str())
        .and_then(|s| s.chars().next());

    let adapter = CONNECTIONS
        .read()
        .get(&id)
        .cloned()
        .ok_or_else(|| sql_error("Connection not found"))?;

    let options = CopyToOptions {
        table_or_query,
        is_query,
        columns,
        format,
        header,
        delimiter,
        null_string: None,
    };

    let stream = adapter
        .copy_to(options)
        .await
        .map_err(|e| sql_error(e.to_string()))?;

    let stream_id = STREAM_ID.fetch_add(1, Ordering::SeqCst);
    COPY_STREAMS
        .write()
        .insert(stream_id, Arc::new(TokioMutex::new(stream)));

    Ok(json!({
        "streamId": stream_id,
        "format": format.as_str()
    }))
}

/// Read the next chunk from a COPY TO stream
async fn sql_copy_to_read(_ctx: OpContext, args: Vec<JsonValue>) -> JscResult<JsonValue> {
    let arg = get_arg(&args).ok_or_else(|| sql_error("Missing arguments"))?;
    let stream_id = arg
        .get("streamId")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| sql_error("Missing streamId"))?;

    let stream = COPY_STREAMS
        .read()
        .get(&stream_id)
        .cloned()
        .ok_or_else(|| sql_error("Stream not found"))?;

    let mut stream_guard = stream.lock().await;
    match stream_guard.next().await {
        Ok(Some(chunk)) => {
            // Return chunk as base64 for efficient transfer
            Ok(json!({
                "done": false,
                "chunk": base64_encode(&chunk),
                "size": chunk.len()
            }))
        }
        Ok(None) => {
            // Stream finished
            Ok(json!({
                "done": true,
                "chunk": null,
                "size": 0
            }))
        }
        Err(e) => Err(sql_error(e.to_string())),
    }
}

/// Close a COPY TO stream
fn sql_copy_to_close(_ctx: OpContext, args: Vec<JsonValue>) -> JscResult<JsonValue> {
    let arg = get_arg(&args).ok_or_else(|| sql_error("Missing arguments"))?;
    let stream_id = arg
        .get("streamId")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| sql_error("Missing streamId"))?;

    let removed = COPY_STREAMS.write().remove(&stream_id).is_some();
    Ok(json!({ "closed": removed }))
}
