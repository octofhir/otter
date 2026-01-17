//! SQL value types for cross-database compatibility

use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

/// Represents a SQL value that can be used as a parameter or returned from a query
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SqlValue {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
    Blob(Vec<u8>),
    Array(Vec<SqlValue>),
    Json(JsonValue),
}

impl SqlValue {
    pub fn is_null(&self) -> bool {
        matches!(self, SqlValue::Null)
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            SqlValue::Bool(b) => Some(*b),
            SqlValue::Int(i) => Some(*i != 0),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            SqlValue::Int(i) => Some(*i),
            SqlValue::Float(f) => Some(*f as i64),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            SqlValue::Float(f) => Some(*f),
            SqlValue::Int(i) => Some(*i as f64),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            SqlValue::Text(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            SqlValue::Blob(b) => Some(b),
            SqlValue::Text(s) => Some(s.as_bytes()),
            _ => None,
        }
    }

    pub fn into_json(self) -> JsonValue {
        match self {
            SqlValue::Null => JsonValue::Null,
            SqlValue::Bool(b) => JsonValue::Bool(b),
            SqlValue::Int(i) => JsonValue::Number(i.into()),
            SqlValue::Float(f) => serde_json::Number::from_f64(f)
                .map(JsonValue::Number)
                .unwrap_or(JsonValue::Null),
            SqlValue::Text(s) => JsonValue::String(s),
            SqlValue::Blob(b) => {
                // Encode as base64 for JSON
                JsonValue::String(base64_encode(&b))
            }
            SqlValue::Array(arr) => {
                JsonValue::Array(arr.into_iter().map(|v| v.into_json()).collect())
            }
            SqlValue::Json(v) => v,
        }
    }
}

impl From<JsonValue> for SqlValue {
    fn from(value: JsonValue) -> Self {
        match value {
            JsonValue::Null => SqlValue::Null,
            JsonValue::Bool(b) => SqlValue::Bool(b),
            JsonValue::Number(n) => {
                if let Some(i) = n.as_i64() {
                    SqlValue::Int(i)
                } else if let Some(f) = n.as_f64() {
                    SqlValue::Float(f)
                } else {
                    SqlValue::Text(n.to_string())
                }
            }
            JsonValue::String(s) => SqlValue::Text(s),
            JsonValue::Array(arr) => SqlValue::Array(arr.into_iter().map(SqlValue::from).collect()),
            JsonValue::Object(_) => SqlValue::Json(value),
        }
    }
}

impl From<()> for SqlValue {
    fn from(_: ()) -> Self {
        SqlValue::Null
    }
}

impl From<bool> for SqlValue {
    fn from(b: bool) -> Self {
        SqlValue::Bool(b)
    }
}

impl From<i32> for SqlValue {
    fn from(i: i32) -> Self {
        SqlValue::Int(i as i64)
    }
}

impl From<i64> for SqlValue {
    fn from(i: i64) -> Self {
        SqlValue::Int(i)
    }
}

impl From<f64> for SqlValue {
    fn from(f: f64) -> Self {
        SqlValue::Float(f)
    }
}

impl From<String> for SqlValue {
    fn from(s: String) -> Self {
        SqlValue::Text(s)
    }
}

impl From<&str> for SqlValue {
    fn from(s: &str) -> Self {
        SqlValue::Text(s.to_string())
    }
}

impl From<Vec<u8>> for SqlValue {
    fn from(b: Vec<u8>) -> Self {
        SqlValue::Blob(b)
    }
}

impl<T: Into<SqlValue>> From<Option<T>> for SqlValue {
    fn from(opt: Option<T>) -> Self {
        match opt {
            Some(v) => v.into(),
            None => SqlValue::Null,
        }
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
