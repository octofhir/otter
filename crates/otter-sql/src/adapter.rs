//! SQL adapter trait for unified database access
//!
//! This module defines the core abstraction for database adapters,
//! allowing SQLite and PostgreSQL to be used interchangeably.

use crate::error::SqlResult;
use crate::value::SqlValue;
use async_trait::async_trait;
use serde_json::Value as JsonValue;
use std::sync::Arc;

/// A row returned from a SQL query
#[derive(Debug, Clone)]
pub struct SqlRow {
    columns: Vec<String>,
    values: Vec<SqlValue>,
}

impl SqlRow {
    pub fn new(columns: Vec<String>, values: Vec<SqlValue>) -> Self {
        Self { columns, values }
    }

    pub fn columns(&self) -> &[String] {
        &self.columns
    }

    pub fn values(&self) -> &[SqlValue] {
        &self.values
    }

    pub fn get(&self, column: &str) -> Option<&SqlValue> {
        self.columns
            .iter()
            .position(|c| c == column)
            .and_then(|i| self.values.get(i))
    }

    pub fn get_by_index(&self, index: usize) -> Option<&SqlValue> {
        self.values.get(index)
    }

    /// Convert to JSON object
    pub fn to_json(&self) -> JsonValue {
        let mut map = serde_json::Map::new();
        for (col, val) in self.columns.iter().zip(self.values.iter()) {
            map.insert(col.clone(), val.clone().into_json());
        }
        JsonValue::Object(map)
    }

    /// Convert to array of values (for .values() result format)
    pub fn to_values_array(&self) -> JsonValue {
        JsonValue::Array(self.values.iter().map(|v| v.clone().into_json()).collect())
    }
}

/// Query result from a database operation
#[derive(Debug)]
pub struct QueryResult {
    pub rows: Vec<SqlRow>,
    pub rows_affected: u64,
    pub last_insert_id: Option<i64>,
}

impl QueryResult {
    pub fn new(rows: Vec<SqlRow>) -> Self {
        Self {
            rows,
            rows_affected: 0,
            last_insert_id: None,
        }
    }

    pub fn with_affected(rows_affected: u64) -> Self {
        Self {
            rows: Vec::new(),
            rows_affected,
            last_insert_id: None,
        }
    }

    pub fn to_json_array(&self) -> JsonValue {
        JsonValue::Array(self.rows.iter().map(|r| r.to_json()).collect())
    }

    pub fn to_values_arrays(&self) -> JsonValue {
        JsonValue::Array(self.rows.iter().map(|r| r.to_values_array()).collect())
    }
}

/// Options for COPY FROM operation
#[derive(Debug, Clone)]
pub struct CopyFromOptions {
    pub table: String,
    pub columns: Option<Vec<String>>,
    pub format: CopyFormat,
    pub header: bool,
    pub delimiter: Option<char>,
    pub null_string: Option<String>,
    pub quote: Option<char>,
    pub escape: Option<char>,
}

/// Options for COPY TO operation
#[derive(Debug, Clone)]
pub struct CopyToOptions {
    pub table_or_query: String,
    pub is_query: bool,
    pub columns: Option<Vec<String>>,
    pub format: CopyFormat,
    pub header: bool,
    pub delimiter: Option<char>,
    pub null_string: Option<String>,
}

/// Format for COPY operations
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CopyFormat {
    #[default]
    Text,
    Csv,
    Binary,
}

impl CopyFormat {
    pub fn as_str(&self) -> &'static str {
        match self {
            CopyFormat::Text => "TEXT",
            CopyFormat::Csv => "CSV",
            CopyFormat::Binary => "BINARY",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "csv" => CopyFormat::Csv,
            "binary" => CopyFormat::Binary,
            _ => CopyFormat::Text,
        }
    }
}

/// Transaction isolation level
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IsolationLevel {
    #[default]
    ReadCommitted,
    RepeatableRead,
    Serializable,
}

/// COPY data sink for streaming data into database
#[async_trait]
pub trait CopySink: Send {
    /// Send a chunk of data
    async fn send(&mut self, data: &[u8]) -> SqlResult<()>;

    /// Finish the COPY operation successfully
    async fn finish(self: Box<Self>) -> SqlResult<u64>;

    /// Abort the COPY operation
    async fn abort(self: Box<Self>, message: Option<&str>) -> SqlResult<()>;
}

/// COPY data stream for reading data from database
#[async_trait]
pub trait CopyStream: Send {
    /// Read the next chunk of data
    async fn next(&mut self) -> SqlResult<Option<bytes::Bytes>>;
}

/// Transaction handle for executing queries within a transaction
#[async_trait]
pub trait SqlTransaction: Send + Sync {
    /// Execute a query within the transaction
    async fn query(&self, sql: &str, params: &[SqlValue]) -> SqlResult<QueryResult>;

    /// Execute a statement within the transaction
    async fn execute(&self, sql: &str, params: &[SqlValue]) -> SqlResult<u64>;

    /// Create a savepoint
    async fn savepoint(&self, name: &str) -> SqlResult<()>;

    /// Release a savepoint
    async fn release_savepoint(&self, name: &str) -> SqlResult<()>;

    /// Rollback to a savepoint
    async fn rollback_to_savepoint(&self, name: &str) -> SqlResult<()>;

    /// Commit the transaction
    async fn commit(self: Box<Self>) -> SqlResult<()>;

    /// Rollback the transaction
    async fn rollback(self: Box<Self>) -> SqlResult<()>;
}

/// Reserved connection for exclusive use
#[async_trait]
pub trait ReservedConnection: Send + Sync {
    /// Execute a query on this connection
    async fn query(&self, sql: &str, params: &[SqlValue]) -> SqlResult<QueryResult>;

    /// Execute a statement on this connection
    async fn execute(&self, sql: &str, params: &[SqlValue]) -> SqlResult<u64>;

    /// Release the connection back to the pool
    fn release(self: Box<Self>);
}

/// Core trait for SQL database adapters
#[async_trait]
pub trait SqlAdapter: Send + Sync {
    /// Get the adapter type name
    fn adapter_type(&self) -> &'static str;

    /// Execute a query and return results
    async fn query(&self, sql: &str, params: &[SqlValue]) -> SqlResult<QueryResult>;

    /// Execute a statement and return affected rows
    async fn execute(&self, sql: &str, params: &[SqlValue]) -> SqlResult<u64>;

    /// Begin a transaction
    async fn begin(&self, isolation: Option<IsolationLevel>) -> SqlResult<Box<dyn SqlTransaction>>;

    /// Reserve a connection for exclusive use
    async fn reserve(&self) -> SqlResult<Box<dyn ReservedConnection>>;

    /// Close all connections
    async fn close(&self) -> SqlResult<()>;

    /// Start a COPY FROM operation (PostgreSQL only)
    async fn copy_from(&self, options: CopyFromOptions) -> SqlResult<Box<dyn CopySink>> {
        Err(crate::error::SqlError::Unsupported(format!(
            "COPY FROM not supported for {}",
            self.adapter_type()
        )))
    }

    /// Start a COPY TO operation (PostgreSQL only)
    async fn copy_to(&self, options: CopyToOptions) -> SqlResult<Box<dyn CopyStream>> {
        Err(crate::error::SqlError::Unsupported(format!(
            "COPY TO not supported for {}",
            self.adapter_type()
        )))
    }
}

/// Shared adapter instance
pub type SharedAdapter = Arc<dyn SqlAdapter>;
