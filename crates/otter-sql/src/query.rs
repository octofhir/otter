//! Query builder for tagged template SQL queries
//!
//! Handles parsing and building SQL queries from tagged template literals,
//! supporting parameter substitution for both SQLite and PostgreSQL.

use crate::value::SqlValue;
use serde_json::Value as JsonValue;

/// Parameter style for different databases
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamStyle {
    /// SQLite style: ?, ?, ?
    Positional,
    /// PostgreSQL style: $1, $2, $3
    Dollar,
}

/// Query builder for constructing parameterized SQL queries
#[derive(Debug)]
pub struct QueryBuilder {
    sql: String,
    params: Vec<SqlValue>,
    param_style: ParamStyle,
    param_count: usize,
}

impl QueryBuilder {
    pub fn new(param_style: ParamStyle) -> Self {
        Self {
            sql: String::new(),
            params: Vec::new(),
            param_style,
            param_count: 0,
        }
    }

    /// Build a query from tagged template parts and values
    ///
    /// The strings array contains the literal parts between placeholders,
    /// and values contains the interpolated values.
    pub fn from_template(strings: &[String], values: &[JsonValue], style: ParamStyle) -> Self {
        let mut builder = Self::new(style);

        for (i, part) in strings.iter().enumerate() {
            builder.sql.push_str(part);

            if i < values.len() {
                builder.add_value(&values[i]);
            }
        }

        builder
    }

    /// Add a value as a parameter
    fn add_value(&mut self, value: &JsonValue) {
        // Check if this is a special sql() helper call
        if let Some(obj) = value.as_object() {
            if let Some(type_field) = obj.get("__sql_type") {
                match type_field.as_str() {
                    Some("identifier") => {
                        // sql("table_name") - escape and inline identifier
                        if let Some(name) = obj.get("value").and_then(|v| v.as_str()) {
                            self.sql.push_str(&escape_identifier(name));
                        }
                        return;
                    }
                    Some("object_insert") => {
                        // sql(object) or sql(object, ...columns) - expand for INSERT
                        self.expand_object_insert(obj);
                        return;
                    }
                    Some("object_update") => {
                        // sql(object, ...columns) for UPDATE SET
                        self.expand_object_update(obj);
                        return;
                    }
                    Some("array_in") => {
                        // sql([1, 2, 3]) for IN clause
                        self.expand_array_in(obj);
                        return;
                    }
                    Some("array_values") => {
                        // sql.array([...]) for PostgreSQL array
                        self.expand_pg_array(obj);
                        return;
                    }
                    Some("raw") => {
                        // sql.raw("...") - inline raw SQL (dangerous!)
                        if let Some(raw) = obj.get("value").and_then(|v| v.as_str()) {
                            self.sql.push_str(raw);
                        }
                        return;
                    }
                    _ => {}
                }
            }
        }

        // Regular value - add as parameter
        self.param_count += 1;
        match self.param_style {
            ParamStyle::Positional => self.sql.push('?'),
            ParamStyle::Dollar => {
                self.sql.push('$');
                self.sql.push_str(&self.param_count.to_string());
            }
        }
        self.params.push(SqlValue::from(value.clone()));
    }

    /// Expand object for INSERT (columns) VALUES (params)
    fn expand_object_insert(&mut self, obj: &serde_json::Map<String, JsonValue>) {
        let values = match obj.get("values") {
            Some(JsonValue::Array(arr)) => arr,
            Some(JsonValue::Object(single)) => {
                self.expand_single_object_insert(single, obj.get("columns"));
                return;
            }
            _ => return,
        };

        let columns = obj.get("columns").and_then(|c| c.as_array());

        if values.is_empty() {
            return;
        }

        // Get column names from first object if not specified
        let col_names: Vec<String> = if let Some(cols) = columns {
            cols.iter()
                .filter_map(|c| c.as_str().map(String::from))
                .collect()
        } else if let Some(first) = values.first().and_then(|v| v.as_object()) {
            first.keys().cloned().collect()
        } else {
            return;
        };

        // Build (col1, col2) VALUES ($1, $2), ($3, $4)
        self.sql.push('(');
        for (i, col) in col_names.iter().enumerate() {
            if i > 0 {
                self.sql.push_str(", ");
            }
            self.sql.push_str(&escape_identifier(col));
        }
        self.sql.push_str(") VALUES ");

        for (row_idx, row) in values.iter().enumerate() {
            if row_idx > 0 {
                self.sql.push_str(", ");
            }
            self.sql.push('(');

            if let Some(row_obj) = row.as_object() {
                for (col_idx, col) in col_names.iter().enumerate() {
                    if col_idx > 0 {
                        self.sql.push_str(", ");
                    }
                    let value = row_obj.get(col).cloned().unwrap_or(JsonValue::Null);
                    self.param_count += 1;
                    match self.param_style {
                        ParamStyle::Positional => self.sql.push('?'),
                        ParamStyle::Dollar => {
                            self.sql.push('$');
                            self.sql.push_str(&self.param_count.to_string());
                        }
                    }
                    self.params.push(SqlValue::from(value));
                }
            }

            self.sql.push(')');
        }
    }

    fn expand_single_object_insert(
        &mut self,
        obj: &serde_json::Map<String, JsonValue>,
        columns: Option<&JsonValue>,
    ) {
        let col_names: Vec<String> = if let Some(JsonValue::Array(cols)) = columns {
            cols.iter()
                .filter_map(|c| c.as_str().map(String::from))
                .collect()
        } else {
            obj.keys().cloned().collect()
        };

        self.sql.push('(');
        for (i, col) in col_names.iter().enumerate() {
            if i > 0 {
                self.sql.push_str(", ");
            }
            self.sql.push_str(&escape_identifier(col));
        }
        self.sql.push_str(") VALUES (");

        for (i, col) in col_names.iter().enumerate() {
            if i > 0 {
                self.sql.push_str(", ");
            }
            let value = obj.get(col).cloned().unwrap_or(JsonValue::Null);
            self.param_count += 1;
            match self.param_style {
                ParamStyle::Positional => self.sql.push('?'),
                ParamStyle::Dollar => {
                    self.sql.push('$');
                    self.sql.push_str(&self.param_count.to_string());
                }
            }
            self.params.push(SqlValue::from(value));
        }

        self.sql.push(')');
    }

    /// Expand object for UPDATE SET col1 = $1, col2 = $2
    fn expand_object_update(&mut self, obj: &serde_json::Map<String, JsonValue>) {
        let values = match obj.get("values").and_then(|v| v.as_object()) {
            Some(v) => v,
            None => return,
        };

        let columns = obj.get("columns").and_then(|c| c.as_array());

        let col_names: Vec<String> = if let Some(cols) = columns {
            cols.iter()
                .filter_map(|c| c.as_str().map(String::from))
                .collect()
        } else {
            values.keys().cloned().collect()
        };

        for (i, col) in col_names.iter().enumerate() {
            if i > 0 {
                self.sql.push_str(", ");
            }
            self.sql.push_str(&escape_identifier(col));
            self.sql.push_str(" = ");

            let value = values.get(col).cloned().unwrap_or(JsonValue::Null);
            self.param_count += 1;
            match self.param_style {
                ParamStyle::Positional => self.sql.push('?'),
                ParamStyle::Dollar => {
                    self.sql.push('$');
                    self.sql.push_str(&self.param_count.to_string());
                }
            }
            self.params.push(SqlValue::from(value));
        }
    }

    /// Expand array for IN clause: ($1, $2, $3)
    fn expand_array_in(&mut self, obj: &serde_json::Map<String, JsonValue>) {
        let values = match obj.get("values").and_then(|v| v.as_array()) {
            Some(v) => v,
            None => return,
        };

        self.sql.push('(');
        for (i, value) in values.iter().enumerate() {
            if i > 0 {
                self.sql.push_str(", ");
            }
            self.param_count += 1;
            match self.param_style {
                ParamStyle::Positional => self.sql.push('?'),
                ParamStyle::Dollar => {
                    self.sql.push('$');
                    self.sql.push_str(&self.param_count.to_string());
                }
            }
            self.params.push(SqlValue::from(value.clone()));
        }
        self.sql.push(')');
    }

    /// Expand PostgreSQL array: ARRAY[$1, $2, $3]
    fn expand_pg_array(&mut self, obj: &serde_json::Map<String, JsonValue>) {
        let values = match obj.get("values").and_then(|v| v.as_array()) {
            Some(v) => v,
            None => return,
        };

        self.sql.push_str("ARRAY[");
        for (i, value) in values.iter().enumerate() {
            if i > 0 {
                self.sql.push_str(", ");
            }
            self.param_count += 1;
            match self.param_style {
                ParamStyle::Positional => self.sql.push('?'),
                ParamStyle::Dollar => {
                    self.sql.push('$');
                    self.sql.push_str(&self.param_count.to_string());
                }
            }
            self.params.push(SqlValue::from(value.clone()));
        }
        self.sql.push(']');
    }

    pub fn sql(&self) -> &str {
        &self.sql
    }

    pub fn params(&self) -> &[SqlValue] {
        &self.params
    }

    pub fn into_parts(self) -> (String, Vec<SqlValue>) {
        (self.sql, self.params)
    }
}

/// Escape a SQL identifier (table or column name)
fn escape_identifier(name: &str) -> String {
    // Handle schema.table format
    if name.contains('.') {
        name.split('.')
            .map(|part| format!("\"{}\"", part.replace('"', "\"\"")))
            .collect::<Vec<_>>()
            .join(".")
    } else {
        format!("\"{}\"", name.replace('"', "\"\""))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simple_query_pg() {
        let builder = QueryBuilder::from_template(
            &["SELECT * FROM users WHERE id = ".into(), "".into()],
            &[serde_json::json!(42)],
            ParamStyle::Dollar,
        );

        assert_eq!(builder.sql(), "SELECT * FROM users WHERE id = $1");
        assert_eq!(builder.params().len(), 1);
    }

    #[test]
    fn test_simple_query_sqlite() {
        let builder = QueryBuilder::from_template(
            &["SELECT * FROM users WHERE id = ".into(), "".into()],
            &[serde_json::json!(42)],
            ParamStyle::Positional,
        );

        assert_eq!(builder.sql(), "SELECT * FROM users WHERE id = ?");
        assert_eq!(builder.params().len(), 1);
    }

    #[test]
    fn test_escape_identifier() {
        assert_eq!(escape_identifier("users"), "\"users\"");
        assert_eq!(escape_identifier("my table"), "\"my table\"");
        assert_eq!(escape_identifier("public.users"), "\"public\".\"users\"");
    }

    #[test]
    fn test_identifier_in_template() {
        let builder = QueryBuilder::from_template(
            &["SELECT * FROM ".into(), "".into()],
            &[serde_json::json!({"__sql_type": "identifier", "value": "users"})],
            ParamStyle::Dollar,
        );

        assert_eq!(builder.sql(), "SELECT * FROM \"users\"");
        assert_eq!(builder.params().len(), 0);
    }
}
