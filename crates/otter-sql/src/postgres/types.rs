//! PostgreSQL type conversion utilities

use crate::value::SqlValue;
use bytes::BytesMut;
use tokio_postgres::Row;
use tokio_postgres::types::{IsNull, ToSql, Type, to_sql_checked};

/// Wrapper for PostgreSQL parameters that implements ToSql
#[derive(Debug)]
pub enum PgValue {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
    Blob(Vec<u8>),
}

impl ToSql for PgValue {
    fn to_sql(
        &self,
        ty: &Type,
        out: &mut BytesMut,
    ) -> Result<IsNull, Box<dyn std::error::Error + Sync + Send>> {
        match self {
            PgValue::Null => Ok(IsNull::Yes),
            PgValue::Bool(b) => b.to_sql(ty, out),
            PgValue::Int(i) => {
                // Handle integer type conversion based on target type
                match *ty {
                    Type::INT2 => (*i as i16).to_sql(ty, out),
                    Type::INT4 => (*i as i32).to_sql(ty, out),
                    Type::INT8 => i.to_sql(ty, out),
                    Type::FLOAT4 => (*i as f32).to_sql(ty, out),
                    Type::FLOAT8 => (*i as f64).to_sql(ty, out),
                    Type::TEXT | Type::VARCHAR => i.to_string().to_sql(ty, out),
                    _ => i.to_sql(ty, out),
                }
            }
            PgValue::Float(f) => {
                // Handle float type conversion
                match *ty {
                    Type::FLOAT4 => (*f as f32).to_sql(ty, out),
                    Type::INT2 => (*f as i16).to_sql(ty, out),
                    Type::INT4 => (*f as i32).to_sql(ty, out),
                    Type::INT8 => (*f as i64).to_sql(ty, out),
                    Type::TEXT | Type::VARCHAR => f.to_string().to_sql(ty, out),
                    _ => f.to_sql(ty, out),
                }
            }
            PgValue::Text(s) => s.to_sql(ty, out),
            PgValue::Blob(b) => b.as_slice().to_sql(ty, out),
        }
    }

    fn accepts(_ty: &Type) -> bool {
        true
    }

    to_sql_checked!();
}

/// Convert SqlValue to PgValue
impl From<&SqlValue> for PgValue {
    fn from(value: &SqlValue) -> Self {
        match value {
            SqlValue::Null => PgValue::Null,
            SqlValue::Bool(b) => PgValue::Bool(*b),
            SqlValue::Int(i) => PgValue::Int(*i),
            SqlValue::Float(f) => PgValue::Float(*f),
            SqlValue::Text(s) => PgValue::Text(s.clone()),
            SqlValue::Blob(b) => PgValue::Blob(b.clone()),
            SqlValue::Array(arr) => PgValue::Text(serde_json::to_string(arr).unwrap_or_default()),
            SqlValue::Json(j) => PgValue::Text(serde_json::to_string(j).unwrap_or_default()),
        }
    }
}

/// Convert SqlValue array to PgValue array
pub fn to_pg_params(params: &[SqlValue]) -> Vec<PgValue> {
    params.iter().map(PgValue::from).collect()
}

/// Build parameter references for query execution
pub fn params_as_refs(params: &[PgValue]) -> Vec<&(dyn ToSql + Sync)> {
    params.iter().map(|p| p as &(dyn ToSql + Sync)).collect()
}

/// Convert PostgreSQL row value to SqlValue
pub fn from_pg_value(row: &Row, index: usize) -> SqlValue {
    let col = &row.columns()[index];
    let ty = col.type_();

    // Try to get the value based on type
    match ty.name() {
        "bool" => row
            .get::<_, Option<bool>>(index)
            .map(SqlValue::Bool)
            .unwrap_or(SqlValue::Null),
        "int2" => row
            .get::<_, Option<i16>>(index)
            .map(|v| SqlValue::Int(v as i64))
            .unwrap_or(SqlValue::Null),
        "int4" => row
            .get::<_, Option<i32>>(index)
            .map(|v| SqlValue::Int(v as i64))
            .unwrap_or(SqlValue::Null),
        "int8" => row
            .get::<_, Option<i64>>(index)
            .map(SqlValue::Int)
            .unwrap_or(SqlValue::Null),
        "float4" => row
            .get::<_, Option<f32>>(index)
            .map(|v| SqlValue::Float(v as f64))
            .unwrap_or(SqlValue::Null),
        "float8" => row
            .get::<_, Option<f64>>(index)
            .map(SqlValue::Float)
            .unwrap_or(SqlValue::Null),
        "numeric" | "decimal" => {
            // Get as string and parse
            row.get::<_, Option<String>>(index)
                .and_then(|s| s.parse::<f64>().ok())
                .map(SqlValue::Float)
                .unwrap_or(SqlValue::Null)
        }
        "text" | "varchar" | "char" | "bpchar" | "name" => row
            .get::<_, Option<String>>(index)
            .map(SqlValue::Text)
            .unwrap_or(SqlValue::Null),
        "bytea" => row
            .get::<_, Option<Vec<u8>>>(index)
            .map(SqlValue::Blob)
            .unwrap_or(SqlValue::Null),
        "json" | "jsonb" => row
            .get::<_, Option<serde_json::Value>>(index)
            .map(SqlValue::Json)
            .unwrap_or(SqlValue::Null),
        "uuid" => {
            // Get UUID as string
            row.get::<_, Option<String>>(index)
                .map(SqlValue::Text)
                .unwrap_or(SqlValue::Null)
        }
        "timestamp" | "timestamptz" => {
            // Get timestamp as string for now (could use chrono for better handling)
            row.get::<_, Option<String>>(index)
                .map(SqlValue::Text)
                .unwrap_or(SqlValue::Null)
        }
        "date" => row
            .get::<_, Option<String>>(index)
            .map(SqlValue::Text)
            .unwrap_or(SqlValue::Null),
        "time" | "timetz" => row
            .get::<_, Option<String>>(index)
            .map(SqlValue::Text)
            .unwrap_or(SqlValue::Null),
        // Array types
        name if name.starts_with('_') => {
            // PostgreSQL array types start with underscore
            // Try to get as string array first
            if let Ok(Some(arr)) = row.try_get::<_, Option<Vec<String>>>(index) {
                return SqlValue::Array(arr.into_iter().map(SqlValue::Text).collect());
            }
            if let Ok(Some(arr)) = row.try_get::<_, Option<Vec<i64>>>(index) {
                return SqlValue::Array(arr.into_iter().map(SqlValue::Int).collect());
            }
            if let Ok(Some(arr)) = row.try_get::<_, Option<Vec<f64>>>(index) {
                return SqlValue::Array(arr.into_iter().map(SqlValue::Float).collect());
            }
            if let Ok(Some(arr)) = row.try_get::<_, Option<Vec<bool>>>(index) {
                return SqlValue::Array(arr.into_iter().map(SqlValue::Bool).collect());
            }
            SqlValue::Null
        }
        _ => {
            // For unknown types, try to get as string
            row.get::<_, Option<String>>(index)
                .map(SqlValue::Text)
                .unwrap_or(SqlValue::Null)
        }
    }
}
