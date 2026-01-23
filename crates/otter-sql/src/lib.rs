//! Otter SQL - SQL API for Otter
//!
//! Provides a unified SQL interface for SQLite and PostgreSQL databases
//! with support for tagged template queries, transactions, and COPY operations.
//!
//! # Usage
//!
//! ```typescript
//! import { sql, SQL, kv } from "otter";
//!
//! // Default sql uses DATABASE_URL or SQLite :memory:
//! const users = await sql`SELECT * FROM users`;
//!
//! // Create specific connections
//! const db = new SQL(":memory:");
//! const pg = new SQL("postgres://localhost/mydb");
//! ```

pub mod adapter;
pub mod postgres;
pub mod query;
pub mod sqlite;
pub mod transaction;

mod error;
// mod extension; // TODO: re-enable when extension system is ported to new VM
mod value;

pub use adapter::{SqlAdapter, SqlRow};
pub use error::{SqlError, SqlResult};
// pub use extension::sql_extension;
pub use query::QueryBuilder;
pub use value::SqlValue;

/// JS wrapper code for the SQL module
pub const SQL_JS: &str = include_str!("sql.js");
