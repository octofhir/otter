//! SQLite type conversion utilities

use crate::value::SqlValue;
use rusqlite::Row;
use rusqlite::types::{ToSqlOutput, Value as RusqliteValue, ValueRef};

/// Wrapper for rusqlite parameters
pub struct SqliteParam(RusqliteValue);

impl rusqlite::ToSql for SqliteParam {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        match &self.0 {
            RusqliteValue::Null => Ok(ToSqlOutput::Owned(RusqliteValue::Null)),
            RusqliteValue::Integer(i) => Ok(ToSqlOutput::Owned(RusqliteValue::Integer(*i))),
            RusqliteValue::Real(f) => Ok(ToSqlOutput::Owned(RusqliteValue::Real(*f))),
            RusqliteValue::Text(s) => Ok(ToSqlOutput::Owned(RusqliteValue::Text(s.clone()))),
            RusqliteValue::Blob(b) => Ok(ToSqlOutput::Owned(RusqliteValue::Blob(b.clone()))),
        }
    }
}

/// Convert SqlValue to rusqlite Value
pub fn to_rusqlite_value(value: &SqlValue) -> SqliteParam {
    SqliteParam(match value {
        SqlValue::Null => RusqliteValue::Null,
        SqlValue::Bool(b) => RusqliteValue::Integer(if *b { 1 } else { 0 }),
        SqlValue::Int(i) => RusqliteValue::Integer(*i),
        SqlValue::Float(f) => RusqliteValue::Real(*f),
        SqlValue::Text(s) => RusqliteValue::Text(s.clone()),
        SqlValue::Blob(b) => RusqliteValue::Blob(b.clone()),
        SqlValue::Array(arr) => {
            // Store arrays as JSON
            RusqliteValue::Text(serde_json::to_string(arr).unwrap_or_default())
        }
        SqlValue::Json(v) => RusqliteValue::Text(serde_json::to_string(v).unwrap_or_default()),
    })
}

/// Convert rusqlite row value to SqlValue
pub fn from_rusqlite_value(row: &Row, index: usize) -> SqlValue {
    match row.get_ref(index) {
        Ok(ValueRef::Null) => SqlValue::Null,
        Ok(ValueRef::Integer(i)) => SqlValue::Int(i),
        Ok(ValueRef::Real(f)) => SqlValue::Float(f),
        Ok(ValueRef::Text(s)) => {
            let text = String::from_utf8_lossy(s).to_string();
            // Try to parse as JSON if it looks like JSON
            if (text.starts_with('[') && text.ends_with(']'))
                || (text.starts_with('{') && text.ends_with('}'))
            {
                if let Ok(json) = serde_json::from_str(&text) {
                    return SqlValue::Json(json);
                }
            }
            SqlValue::Text(text)
        }
        Ok(ValueRef::Blob(b)) => SqlValue::Blob(b.to_vec()),
        Err(_) => SqlValue::Null,
    }
}
