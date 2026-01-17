//! PostgreSQL adapter implementation
//!
//! Provides a PostgreSQL backend using tokio-postgres with connection pooling
//! and COPY FROM/TO support.

mod copy;
mod types;

use crate::adapter::{
    CopyFromOptions, CopySink, CopyStream, CopyToOptions, IsolationLevel, QueryResult,
    ReservedConnection, SqlAdapter, SqlRow, SqlTransaction,
};
use crate::error::{SqlError, SqlResult};
use crate::transaction::isolation_level_sql;
use crate::value::SqlValue;
use async_trait::async_trait;
use deadpool_postgres::{Config, ManagerConfig, Pool, RecyclingMethod, Runtime};
use std::time::Duration;
use tokio_postgres::{NoTls, Row as PgRow};

pub use copy::{PostgresCopySink, PostgresCopyStream};

/// PostgreSQL connection options
#[derive(Debug, Clone)]
pub struct PostgresOptions {
    pub host: String,
    pub port: u16,
    pub database: String,
    pub user: Option<String>,
    pub password: Option<String>,
    pub ssl_mode: SslMode,
    pub max_connections: usize,
    pub idle_timeout: Option<Duration>,
    pub connection_timeout: Option<Duration>,
    pub application_name: Option<String>,
}

impl Default for PostgresOptions {
    fn default() -> Self {
        Self {
            host: "localhost".into(),
            port: 5432,
            database: "postgres".into(),
            user: None,
            password: None,
            ssl_mode: SslMode::Prefer,
            max_connections: 10,
            idle_timeout: Some(Duration::from_secs(30)),
            connection_timeout: Some(Duration::from_secs(10)),
            application_name: Some("otter".into()),
        }
    }
}

/// SSL mode for PostgreSQL connections
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SslMode {
    Disable,
    #[default]
    Prefer,
    Require,
}

impl PostgresOptions {
    /// Parse a PostgreSQL connection URL
    pub fn from_url(url: &str) -> SqlResult<Self> {
        let url = url::Url::parse(url).map_err(|e| SqlError::InvalidUrl(e.to_string()))?;

        if url.scheme() != "postgres" && url.scheme() != "postgresql" {
            return Err(SqlError::InvalidUrl(format!(
                "Expected postgres:// or postgresql:// scheme, got {}://",
                url.scheme()
            )));
        }

        let host = url.host_str().unwrap_or("localhost").to_string();
        let port = url.port().unwrap_or(5432);
        let database = url.path().trim_start_matches('/').to_string();
        let database = if database.is_empty() {
            "postgres".to_string()
        } else {
            database
        };

        let user = if url.username().is_empty() {
            None
        } else {
            Some(url.username().to_string())
        };
        let password = url.password().map(|s| s.to_string());

        // Parse query parameters
        let mut ssl_mode = SslMode::Prefer;
        let mut max_connections = 10;
        let mut application_name = Some("otter".to_string());

        for (key, value) in url.query_pairs() {
            match key.as_ref() {
                "sslmode" => {
                    ssl_mode = match value.as_ref() {
                        "disable" => SslMode::Disable,
                        "require" => SslMode::Require,
                        _ => SslMode::Prefer,
                    };
                }
                "pool_max_conns" | "max" => {
                    max_connections = value.parse().unwrap_or(10);
                }
                "application_name" => {
                    application_name = Some(value.to_string());
                }
                _ => {}
            }
        }

        Ok(Self {
            host,
            port,
            database,
            user,
            password,
            ssl_mode,
            max_connections,
            idle_timeout: Some(Duration::from_secs(30)),
            connection_timeout: Some(Duration::from_secs(10)),
            application_name,
        })
    }
}

/// PostgreSQL database adapter with connection pooling
pub struct PostgresAdapter {
    pool: Pool,
    options: PostgresOptions,
}

impl PostgresAdapter {
    /// Create a new PostgreSQL adapter from a connection URL
    pub async fn connect(url: &str) -> SqlResult<Self> {
        let options = PostgresOptions::from_url(url)?;
        Self::connect_with_options(options).await
    }

    /// Create a new PostgreSQL adapter with options
    pub async fn connect_with_options(options: PostgresOptions) -> SqlResult<Self> {
        let mut cfg = Config::new();
        cfg.host = Some(options.host.clone());
        cfg.port = Some(options.port);
        cfg.dbname = Some(options.database.clone());
        cfg.user = options.user.clone();
        cfg.password = options.password.clone();
        cfg.application_name = options.application_name.clone();

        cfg.manager = Some(ManagerConfig {
            recycling_method: RecyclingMethod::Fast,
        });

        cfg.pool = Some(deadpool_postgres::PoolConfig {
            max_size: options.max_connections,
            timeouts: deadpool_postgres::Timeouts {
                wait: options.connection_timeout,
                create: options.connection_timeout,
                recycle: options.idle_timeout,
            },
            queue_mode: deadpool::managed::QueueMode::Fifo,
        });

        // TODO: Add TLS support when ssl_mode is Require
        let pool = cfg
            .create_pool(Some(Runtime::Tokio1), NoTls)
            .map_err(|e| SqlError::Pool(e.to_string()))?;

        // Test connection
        let _client = pool.get().await?;

        Ok(Self { pool, options })
    }

    /// Convert a tokio_postgres Row to SqlRow
    fn row_to_sql_row(row: &PgRow) -> SqlRow {
        let columns: Vec<String> = row.columns().iter().map(|c| c.name().to_string()).collect();
        let values: Vec<SqlValue> = (0..columns.len())
            .map(|i| types::from_pg_value(row, i))
            .collect();
        SqlRow::new(columns, values)
    }
}

#[async_trait]
impl SqlAdapter for PostgresAdapter {
    fn adapter_type(&self) -> &'static str {
        "postgres"
    }

    async fn query(&self, sql: &str, params: &[SqlValue]) -> SqlResult<QueryResult> {
        let client = self.pool.get().await?;
        let pg_params = types::to_pg_params(params);
        let param_refs = types::params_as_refs(&pg_params);

        let rows = client
            .query(sql, &param_refs)
            .await
            .map_err(SqlError::postgres)?;

        let sql_rows: Vec<SqlRow> = rows.iter().map(Self::row_to_sql_row).collect();
        Ok(QueryResult::new(sql_rows))
    }

    async fn execute(&self, sql: &str, params: &[SqlValue]) -> SqlResult<u64> {
        let client = self.pool.get().await?;
        let pg_params = types::to_pg_params(params);
        let param_refs = types::params_as_refs(&pg_params);

        let rows_affected = client
            .execute(sql, &param_refs)
            .await
            .map_err(SqlError::postgres)?;

        Ok(rows_affected)
    }

    async fn begin(&self, isolation: Option<IsolationLevel>) -> SqlResult<Box<dyn SqlTransaction>> {
        let client = self.pool.get().await?;
        let isolation = isolation.unwrap_or_default();
        let isolation_sql = isolation_level_sql(isolation, true);

        client
            .batch_execute(&format!("BEGIN ISOLATION LEVEL {}", isolation_sql))
            .await
            .map_err(SqlError::postgres)?;

        Ok(Box::new(PostgresTransaction {
            client,
            committed: false,
        }))
    }

    async fn reserve(&self) -> SqlResult<Box<dyn ReservedConnection>> {
        let client = self.pool.get().await?;
        Ok(Box::new(PostgresReservedConnection { client }))
    }

    async fn close(&self) -> SqlResult<()> {
        self.pool.close();
        Ok(())
    }

    async fn copy_from(&self, options: CopyFromOptions) -> SqlResult<Box<dyn CopySink>> {
        let client = self.pool.get().await?;
        copy::start_copy_from(client, options).await
    }

    async fn copy_to(&self, options: CopyToOptions) -> SqlResult<Box<dyn CopyStream>> {
        let client = self.pool.get().await?;
        copy::start_copy_to(client, options).await
    }
}

/// PostgreSQL transaction
struct PostgresTransaction {
    client: deadpool_postgres::Object,
    committed: bool,
}

#[async_trait]
impl SqlTransaction for PostgresTransaction {
    async fn query(&self, sql: &str, params: &[SqlValue]) -> SqlResult<QueryResult> {
        let pg_params = types::to_pg_params(params);
        let param_refs = types::params_as_refs(&pg_params);

        let rows = self
            .client
            .query(sql, &param_refs)
            .await
            .map_err(SqlError::postgres)?;

        let sql_rows: Vec<SqlRow> = rows.iter().map(PostgresAdapter::row_to_sql_row).collect();
        Ok(QueryResult::new(sql_rows))
    }

    async fn execute(&self, sql: &str, params: &[SqlValue]) -> SqlResult<u64> {
        let pg_params = types::to_pg_params(params);
        let param_refs = types::params_as_refs(&pg_params);

        let rows_affected = self
            .client
            .execute(sql, &param_refs)
            .await
            .map_err(SqlError::postgres)?;

        Ok(rows_affected)
    }

    async fn savepoint(&self, name: &str) -> SqlResult<()> {
        let sql = format!("SAVEPOINT \"{}\"", name.replace('"', "\"\""));
        self.client
            .batch_execute(&sql)
            .await
            .map_err(SqlError::postgres)
    }

    async fn release_savepoint(&self, name: &str) -> SqlResult<()> {
        let sql = format!("RELEASE SAVEPOINT \"{}\"", name.replace('"', "\"\""));
        self.client
            .batch_execute(&sql)
            .await
            .map_err(SqlError::postgres)
    }

    async fn rollback_to_savepoint(&self, name: &str) -> SqlResult<()> {
        let sql = format!("ROLLBACK TO SAVEPOINT \"{}\"", name.replace('"', "\"\""));
        self.client
            .batch_execute(&sql)
            .await
            .map_err(SqlError::postgres)
    }

    async fn commit(mut self: Box<Self>) -> SqlResult<()> {
        self.client
            .batch_execute("COMMIT")
            .await
            .map_err(SqlError::postgres)?;
        self.committed = true;
        Ok(())
    }

    async fn rollback(self: Box<Self>) -> SqlResult<()> {
        self.client
            .batch_execute("ROLLBACK")
            .await
            .map_err(SqlError::postgres)
    }
}

impl Drop for PostgresTransaction {
    fn drop(&mut self) {
        if !self.committed {
            // We can't do async in drop, but the connection will be recycled
            // and any uncommitted transaction will be rolled back
        }
    }
}

/// Reserved PostgreSQL connection
struct PostgresReservedConnection {
    client: deadpool_postgres::Object,
}

#[async_trait]
impl ReservedConnection for PostgresReservedConnection {
    async fn query(&self, sql: &str, params: &[SqlValue]) -> SqlResult<QueryResult> {
        let pg_params = types::to_pg_params(params);
        let param_refs = types::params_as_refs(&pg_params);

        let rows = self
            .client
            .query(sql, &param_refs)
            .await
            .map_err(SqlError::postgres)?;

        let sql_rows: Vec<SqlRow> = rows.iter().map(PostgresAdapter::row_to_sql_row).collect();
        Ok(QueryResult::new(sql_rows))
    }

    async fn execute(&self, sql: &str, params: &[SqlValue]) -> SqlResult<u64> {
        let pg_params = types::to_pg_params(params);
        let param_refs = types::params_as_refs(&pg_params);

        let rows_affected = self
            .client
            .execute(sql, &param_refs)
            .await
            .map_err(SqlError::postgres)?;

        Ok(rows_affected)
    }

    fn release(self: Box<Self>) {
        // Connection returns to pool when dropped
    }
}
