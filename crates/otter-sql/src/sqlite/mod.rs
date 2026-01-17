//! SQLite adapter implementation
//!
//! Provides a SQLite backend using rusqlite with async support via spawn_blocking.

mod types;

use crate::adapter::{
    IsolationLevel, QueryResult, ReservedConnection, SqlAdapter, SqlRow, SqlTransaction,
};
use crate::error::{SqlError, SqlResult};
use crate::value::SqlValue;
use async_trait::async_trait;
use parking_lot::Mutex;
use rusqlite::{Connection, OpenFlags};
use std::sync::Arc;

/// SQLite database connection
pub struct SqliteAdapter {
    conn: Arc<Mutex<Connection>>,
    path: String,
}

impl SqliteAdapter {
    /// Open a SQLite database
    ///
    /// # Arguments
    /// * `path` - Database path. Use `:memory:` for in-memory database,
    ///           or a file path (optionally with `sqlite://` prefix)
    pub fn open(path: &str) -> SqlResult<Self> {
        let normalized_path = normalize_path(path);
        let conn = if normalized_path == ":memory:" {
            Connection::open_in_memory()
        } else {
            let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_FULL_MUTEX;
            Connection::open_with_flags(&normalized_path, flags)
        }
        .map_err(SqlError::sqlite)?;

        // Enable WAL mode for better concurrency
        if normalized_path != ":memory:" {
            conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")
                .map_err(SqlError::sqlite)?;
        }

        // Enable foreign keys
        conn.execute("PRAGMA foreign_keys = ON", [])
            .map_err(SqlError::sqlite)?;

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            path: normalized_path,
        })
    }

    /// Execute a query and return results (sync, internal)
    fn query_sync(conn: &Connection, sql: &str, params: &[SqlValue]) -> SqlResult<QueryResult> {
        let mut stmt = conn.prepare(sql).map_err(SqlError::sqlite)?;

        let rusqlite_params: Vec<_> = params.iter().map(types::to_rusqlite_value).collect();
        let param_refs: Vec<&dyn rusqlite::ToSql> = rusqlite_params
            .iter()
            .map(|p| p as &dyn rusqlite::ToSql)
            .collect();

        let column_names: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
        let column_count = column_names.len();

        let rows_result: Result<Vec<SqlRow>, rusqlite::Error> = stmt
            .query(param_refs.as_slice())?
            .mapped(|row| {
                let mut values = Vec::with_capacity(column_count);
                for i in 0..column_count {
                    values.push(types::from_rusqlite_value(row, i));
                }
                Ok(SqlRow::new(column_names.clone(), values))
            })
            .collect();

        Ok(QueryResult::new(rows_result.map_err(SqlError::sqlite)?))
    }

    /// Execute a statement (sync, internal)
    fn execute_sync(conn: &Connection, sql: &str, params: &[SqlValue]) -> SqlResult<u64> {
        let rusqlite_params: Vec<_> = params.iter().map(types::to_rusqlite_value).collect();
        let param_refs: Vec<&dyn rusqlite::ToSql> = rusqlite_params
            .iter()
            .map(|p| p as &dyn rusqlite::ToSql)
            .collect();

        let rows_affected = conn
            .execute(sql, param_refs.as_slice())
            .map_err(SqlError::sqlite)?;

        Ok(rows_affected as u64)
    }
}

#[async_trait]
impl SqlAdapter for SqliteAdapter {
    fn adapter_type(&self) -> &'static str {
        "sqlite"
    }

    async fn query(&self, sql: &str, params: &[SqlValue]) -> SqlResult<QueryResult> {
        let conn = self.conn.clone();
        let sql = sql.to_string();
        let params = params.to_vec();

        tokio::task::spawn_blocking(move || {
            let conn = conn.lock();
            Self::query_sync(&conn, &sql, &params)
        })
        .await
        .map_err(|e| SqlError::Query(format!("Task join error: {}", e)))?
    }

    async fn execute(&self, sql: &str, params: &[SqlValue]) -> SqlResult<u64> {
        let conn = self.conn.clone();
        let sql = sql.to_string();
        let params = params.to_vec();

        tokio::task::spawn_blocking(move || {
            let conn = conn.lock();
            Self::execute_sync(&conn, &sql, &params)
        })
        .await
        .map_err(|e| SqlError::Query(format!("Task join error: {}", e)))?
    }

    async fn begin(&self, isolation: Option<IsolationLevel>) -> SqlResult<Box<dyn SqlTransaction>> {
        let conn = self.conn.clone();
        let isolation = isolation.unwrap_or_default();

        tokio::task::spawn_blocking(move || {
            let conn_guard = conn.lock();
            // SQLite doesn't support isolation levels in the same way as PostgreSQL
            // We use IMMEDIATE to get a write lock immediately
            conn_guard
                .execute("BEGIN IMMEDIATE", [])
                .map_err(SqlError::sqlite)?;
            drop(conn_guard);

            Ok(Box::new(SqliteTransaction {
                conn,
                committed: false,
            }) as Box<dyn SqlTransaction>)
        })
        .await
        .map_err(|e| SqlError::Transaction(format!("Task join error: {}", e)))?
    }

    async fn reserve(&self) -> SqlResult<Box<dyn ReservedConnection>> {
        // For SQLite, we just return a wrapper around the same connection
        // since SQLite is single-threaded anyway
        Ok(Box::new(SqliteReservedConnection {
            conn: self.conn.clone(),
        }))
    }

    async fn close(&self) -> SqlResult<()> {
        // SQLite connection will be closed when dropped
        // We could add explicit close logic here if needed
        Ok(())
    }
}

/// SQLite transaction
struct SqliteTransaction {
    conn: Arc<Mutex<Connection>>,
    committed: bool,
}

#[async_trait]
impl SqlTransaction for SqliteTransaction {
    async fn query(&self, sql: &str, params: &[SqlValue]) -> SqlResult<QueryResult> {
        let conn = self.conn.clone();
        let sql = sql.to_string();
        let params = params.to_vec();

        tokio::task::spawn_blocking(move || {
            let conn = conn.lock();
            SqliteAdapter::query_sync(&conn, &sql, &params)
        })
        .await
        .map_err(|e| SqlError::Query(format!("Task join error: {}", e)))?
    }

    async fn execute(&self, sql: &str, params: &[SqlValue]) -> SqlResult<u64> {
        let conn = self.conn.clone();
        let sql = sql.to_string();
        let params = params.to_vec();

        tokio::task::spawn_blocking(move || {
            let conn = conn.lock();
            SqliteAdapter::execute_sync(&conn, &sql, &params)
        })
        .await
        .map_err(|e| SqlError::Query(format!("Task join error: {}", e)))?
    }

    async fn savepoint(&self, name: &str) -> SqlResult<()> {
        let conn = self.conn.clone();
        let sql = format!("SAVEPOINT \"{}\"", name.replace('"', "\"\""));

        tokio::task::spawn_blocking(move || {
            let conn = conn.lock();
            conn.execute(&sql, []).map_err(SqlError::sqlite)?;
            Ok(())
        })
        .await
        .map_err(|e| SqlError::Transaction(format!("Task join error: {}", e)))?
    }

    async fn release_savepoint(&self, name: &str) -> SqlResult<()> {
        let conn = self.conn.clone();
        let sql = format!("RELEASE SAVEPOINT \"{}\"", name.replace('"', "\"\""));

        tokio::task::spawn_blocking(move || {
            let conn = conn.lock();
            conn.execute(&sql, []).map_err(SqlError::sqlite)?;
            Ok(())
        })
        .await
        .map_err(|e| SqlError::Transaction(format!("Task join error: {}", e)))?
    }

    async fn rollback_to_savepoint(&self, name: &str) -> SqlResult<()> {
        let conn = self.conn.clone();
        let sql = format!("ROLLBACK TO SAVEPOINT \"{}\"", name.replace('"', "\"\""));

        tokio::task::spawn_blocking(move || {
            let conn = conn.lock();
            conn.execute(&sql, []).map_err(SqlError::sqlite)?;
            Ok(())
        })
        .await
        .map_err(|e| SqlError::Transaction(format!("Task join error: {}", e)))?
    }

    async fn commit(mut self: Box<Self>) -> SqlResult<()> {
        let conn = self.conn.clone();

        tokio::task::spawn_blocking(move || {
            let conn = conn.lock();
            conn.execute("COMMIT", []).map_err(SqlError::sqlite)?;
            Ok(())
        })
        .await
        .map_err(|e| SqlError::Transaction(format!("Task join error: {}", e)))?
        .map(|_| {
            self.committed = true;
        })
    }

    async fn rollback(self: Box<Self>) -> SqlResult<()> {
        let conn = self.conn.clone();

        tokio::task::spawn_blocking(move || {
            let conn = conn.lock();
            conn.execute("ROLLBACK", []).map_err(SqlError::sqlite)?;
            Ok(())
        })
        .await
        .map_err(|e| SqlError::Transaction(format!("Task join error: {}", e)))?
    }
}

impl Drop for SqliteTransaction {
    fn drop(&mut self) {
        if !self.committed {
            // Try to rollback on drop
            if let Some(conn) = self.conn.try_lock() {
                let _ = conn.execute("ROLLBACK", []);
            }
        }
    }
}

/// Reserved SQLite connection
struct SqliteReservedConnection {
    conn: Arc<Mutex<Connection>>,
}

#[async_trait]
impl ReservedConnection for SqliteReservedConnection {
    async fn query(&self, sql: &str, params: &[SqlValue]) -> SqlResult<QueryResult> {
        let conn = self.conn.clone();
        let sql = sql.to_string();
        let params = params.to_vec();

        tokio::task::spawn_blocking(move || {
            let conn = conn.lock();
            SqliteAdapter::query_sync(&conn, &sql, &params)
        })
        .await
        .map_err(|e| SqlError::Query(format!("Task join error: {}", e)))?
    }

    async fn execute(&self, sql: &str, params: &[SqlValue]) -> SqlResult<u64> {
        let conn = self.conn.clone();
        let sql = sql.to_string();
        let params = params.to_vec();

        tokio::task::spawn_blocking(move || {
            let conn = conn.lock();
            SqliteAdapter::execute_sync(&conn, &sql, &params)
        })
        .await
        .map_err(|e| SqlError::Query(format!("Task join error: {}", e)))?
    }

    fn release(self: Box<Self>) {
        // Nothing to do for SQLite
    }
}

/// Normalize a database path
fn normalize_path(path: &str) -> String {
    if path == ":memory:" {
        return path.to_string();
    }

    // Strip sqlite:// prefix if present
    let path = path
        .strip_prefix("sqlite://")
        .or_else(|| path.strip_prefix("sqlite:"))
        .unwrap_or(path);

    path.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_sqlite_basic() {
        let db = SqliteAdapter::open(":memory:").unwrap();

        db.execute("CREATE TABLE test (id INTEGER PRIMARY KEY, name TEXT)", &[])
            .await
            .unwrap();

        db.execute(
            "INSERT INTO test (name) VALUES (?)",
            &[SqlValue::Text("Alice".into())],
        )
        .await
        .unwrap();

        let result = db.query("SELECT * FROM test", &[]).await.unwrap();
        assert_eq!(result.rows.len(), 1);
        assert_eq!(
            result.rows[0].get("name"),
            Some(&SqlValue::Text("Alice".into()))
        );
    }

    #[tokio::test]
    async fn test_sqlite_transaction() {
        let db = SqliteAdapter::open(":memory:").unwrap();

        db.execute(
            "CREATE TABLE test (id INTEGER PRIMARY KEY, value INTEGER)",
            &[],
        )
        .await
        .unwrap();

        // Test commit
        {
            let tx = db.begin(None).await.unwrap();
            tx.execute("INSERT INTO test (value) VALUES (?)", &[SqlValue::Int(1)])
                .await
                .unwrap();
            tx.commit().await.unwrap();
        }

        let result = db
            .query("SELECT COUNT(*) as cnt FROM test", &[])
            .await
            .unwrap();
        assert_eq!(result.rows[0].get("cnt"), Some(&SqlValue::Int(1)));

        // Test rollback
        {
            let tx = db.begin(None).await.unwrap();
            tx.execute("INSERT INTO test (value) VALUES (?)", &[SqlValue::Int(2)])
                .await
                .unwrap();
            tx.rollback().await.unwrap();
        }

        let result = db
            .query("SELECT COUNT(*) as cnt FROM test", &[])
            .await
            .unwrap();
        assert_eq!(result.rows[0].get("cnt"), Some(&SqlValue::Int(1)));
    }
}
