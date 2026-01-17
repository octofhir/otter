//! SQL error types

use thiserror::Error;

pub type SqlResult<T> = Result<T, SqlError>;

#[derive(Debug, Error)]
pub enum SqlError {
    #[error("SQLite error: {message}")]
    Sqlite { message: String, code: Option<i32> },

    #[error("PostgreSQL error: {message}")]
    Postgres {
        message: String,
        code: Option<String>,
        detail: Option<String>,
        hint: Option<String>,
    },

    #[error("Connection error: {0}")]
    Connection(String),

    #[error("Connection pool error: {0}")]
    Pool(String),

    #[error("Query error: {0}")]
    Query(String),

    #[error("Transaction error: {0}")]
    Transaction(String),

    #[error("Type conversion error: {0}")]
    TypeConversion(String),

    #[error("Invalid URL: {0}")]
    InvalidUrl(String),

    #[error("COPY operation error: {0}")]
    Copy(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Unsupported operation: {0}")]
    Unsupported(String),

    #[error("Configuration error: {0}")]
    Config(String),
}

impl SqlError {
    pub fn sqlite(err: rusqlite::Error) -> Self {
        SqlError::Sqlite {
            message: err.to_string(),
            code: err.sqlite_error_code().map(|c| c as i32),
        }
    }

    pub fn postgres(err: tokio_postgres::Error) -> Self {
        if let Some(db_err) = err.as_db_error() {
            SqlError::Postgres {
                message: db_err.message().to_string(),
                code: Some(db_err.code().code().to_string()),
                detail: db_err.detail().map(|s| s.to_string()),
                hint: db_err.hint().map(|s| s.to_string()),
            }
        } else {
            SqlError::Postgres {
                message: err.to_string(),
                code: None,
                detail: None,
                hint: None,
            }
        }
    }
}

impl From<rusqlite::Error> for SqlError {
    fn from(err: rusqlite::Error) -> Self {
        SqlError::sqlite(err)
    }
}

impl From<tokio_postgres::Error> for SqlError {
    fn from(err: tokio_postgres::Error) -> Self {
        SqlError::postgres(err)
    }
}

impl From<deadpool_postgres::PoolError> for SqlError {
    fn from(err: deadpool_postgres::PoolError) -> Self {
        SqlError::Pool(err.to_string())
    }
}

impl From<serde_json::Error> for SqlError {
    fn from(err: serde_json::Error) -> Self {
        SqlError::TypeConversion(err.to_string())
    }
}
