//! PostgreSQL COPY FROM/TO implementation

use crate::adapter::{CopyFormat, CopyFromOptions, CopySink, CopyStream, CopyToOptions};
use crate::error::{SqlError, SqlResult};
use async_trait::async_trait;
use bytes::Bytes;
use deadpool_postgres::Object as PooledClient;
use futures_util::{SinkExt, StreamExt};
use std::pin::Pin;
use tokio_postgres::CopyInSink;
use tokio_postgres::CopyOutStream;

/// Build COPY FROM STDIN SQL statement
fn build_copy_from_sql(options: &CopyFromOptions) -> String {
    let mut sql = String::from("COPY ");
    sql.push_str(&escape_identifier(&options.table));

    if let Some(cols) = &options.columns {
        sql.push_str(" (");
        for (i, col) in cols.iter().enumerate() {
            if i > 0 {
                sql.push_str(", ");
            }
            sql.push_str(&escape_identifier(col));
        }
        sql.push(')');
    }

    sql.push_str(" FROM STDIN");

    // Add format options
    let mut with_options = Vec::new();

    match options.format {
        CopyFormat::Csv => with_options.push("FORMAT CSV".to_string()),
        CopyFormat::Binary => with_options.push("FORMAT BINARY".to_string()),
        CopyFormat::Text => {} // Default
    }

    if options.header && options.format == CopyFormat::Csv {
        with_options.push("HEADER".to_string());
    }

    if let Some(delim) = options.delimiter {
        if options.format != CopyFormat::Binary {
            with_options.push(format!("DELIMITER '{}'", delim));
        }
    }

    if let Some(ref null_str) = options.null_string {
        with_options.push(format!("NULL '{}'", null_str.replace('\'', "''")));
    }

    if let Some(quote) = options.quote {
        if options.format == CopyFormat::Csv {
            with_options.push(format!("QUOTE '{}'", quote));
        }
    }

    if let Some(escape) = options.escape {
        if options.format == CopyFormat::Csv {
            with_options.push(format!("ESCAPE '{}'", escape));
        }
    }

    if !with_options.is_empty() {
        sql.push_str(" WITH (");
        sql.push_str(&with_options.join(", "));
        sql.push(')');
    }

    sql
}

/// Build COPY TO STDOUT SQL statement
fn build_copy_to_sql(options: &CopyToOptions) -> String {
    let mut sql = String::from("COPY ");

    if options.is_query {
        sql.push('(');
        sql.push_str(&options.table_or_query);
        sql.push(')');
    } else {
        sql.push_str(&escape_identifier(&options.table_or_query));

        if let Some(cols) = &options.columns {
            sql.push_str(" (");
            for (i, col) in cols.iter().enumerate() {
                if i > 0 {
                    sql.push_str(", ");
                }
                sql.push_str(&escape_identifier(col));
            }
            sql.push(')');
        }
    }

    sql.push_str(" TO STDOUT");

    // Add format options
    let mut with_options = Vec::new();

    match options.format {
        CopyFormat::Csv => with_options.push("FORMAT CSV".to_string()),
        CopyFormat::Binary => with_options.push("FORMAT BINARY".to_string()),
        CopyFormat::Text => {}
    }

    if options.header && options.format == CopyFormat::Csv {
        with_options.push("HEADER".to_string());
    }

    if let Some(delim) = options.delimiter {
        if options.format != CopyFormat::Binary {
            with_options.push(format!("DELIMITER '{}'", delim));
        }
    }

    if let Some(ref null_str) = options.null_string {
        with_options.push(format!("NULL '{}'", null_str.replace('\'', "''")));
    }

    if !with_options.is_empty() {
        sql.push_str(" WITH (");
        sql.push_str(&with_options.join(", "));
        sql.push(')');
    }

    sql
}

/// Escape a SQL identifier
fn escape_identifier(name: &str) -> String {
    if name.contains('.') {
        name.split('.')
            .map(|part| format!("\"{}\"", part.replace('"', "\"\"")))
            .collect::<Vec<_>>()
            .join(".")
    } else {
        format!("\"{}\"", name.replace('"', "\"\""))
    }
}

/// Start a COPY FROM operation
pub async fn start_copy_from(
    client: PooledClient,
    options: CopyFromOptions,
) -> SqlResult<Box<dyn CopySink>> {
    let sql = build_copy_from_sql(&options);

    let sink = client.copy_in(&sql).await.map_err(SqlError::postgres)?;

    Ok(Box::new(PostgresCopySink {
        sink: Box::pin(sink),
        bytes_written: 0,
    }))
}

/// Start a COPY TO operation
pub async fn start_copy_to(
    client: PooledClient,
    options: CopyToOptions,
) -> SqlResult<Box<dyn CopyStream>> {
    let sql = build_copy_to_sql(&options);

    let stream = client.copy_out(&sql).await.map_err(SqlError::postgres)?;

    Ok(Box::new(PostgresCopyStream {
        // IMPORTANT: Keep client alive for the duration of the stream!
        _client: client,
        stream: Box::pin(stream),
    }))
}

/// COPY FROM sink for streaming data into PostgreSQL
pub struct PostgresCopySink {
    sink: Pin<Box<CopyInSink<Bytes>>>,
    bytes_written: u64,
}

#[async_trait]
impl CopySink for PostgresCopySink {
    async fn send(&mut self, data: &[u8]) -> SqlResult<()> {
        let bytes = Bytes::copy_from_slice(data);
        self.sink
            .send(bytes)
            .await
            .map_err(|e| SqlError::Copy(e.to_string()))?;
        self.bytes_written += data.len() as u64;
        Ok(())
    }

    async fn finish(mut self: Box<Self>) -> SqlResult<u64> {
        // Close the sink to signal end of data
        // The finish() method returns the number of rows copied
        let rows_copied = self
            .sink
            .as_mut()
            .finish()
            .await
            .map_err(|e| SqlError::Copy(e.to_string()))?;
        Ok(rows_copied)
    }

    async fn abort(mut self: Box<Self>, _message: Option<&str>) -> SqlResult<()> {
        // Close the sink without finishing to abort the COPY
        // The connection will rollback the incomplete COPY
        drop(self.sink);
        Ok(())
    }
}

/// COPY TO stream for reading data from PostgreSQL
pub struct PostgresCopyStream {
    /// Keep client alive for the duration of the stream
    _client: PooledClient,
    stream: Pin<Box<CopyOutStream>>,
}

#[async_trait]
impl CopyStream for PostgresCopyStream {
    async fn next(&mut self) -> SqlResult<Option<Bytes>> {
        match self.stream.next().await {
            Some(Ok(bytes)) => Ok(Some(bytes)),
            Some(Err(e)) => Err(SqlError::Copy(e.to_string())),
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_copy_from_sql() {
        let options = CopyFromOptions {
            table: "users".to_string(),
            columns: Some(vec!["name".to_string(), "email".to_string()]),
            format: CopyFormat::Csv,
            header: true,
            delimiter: Some(','),
            null_string: None,
            quote: None,
            escape: None,
        };

        let sql = build_copy_from_sql(&options);
        assert!(sql.contains("COPY \"users\""));
        assert!(sql.contains("(\"name\", \"email\")"));
        assert!(sql.contains("FROM STDIN"));
        assert!(sql.contains("FORMAT CSV"));
        assert!(sql.contains("HEADER"));
    }

    #[test]
    fn test_build_copy_to_sql_table() {
        let options = CopyToOptions {
            table_or_query: "users".to_string(),
            is_query: false,
            columns: None,
            format: CopyFormat::Csv,
            header: true,
            delimiter: None,
            null_string: None,
        };

        let sql = build_copy_to_sql(&options);
        assert!(sql.contains("COPY \"users\""));
        assert!(sql.contains("TO STDOUT"));
        assert!(sql.contains("FORMAT CSV"));
        assert!(sql.contains("HEADER"));
    }

    #[test]
    fn test_build_copy_to_sql_query() {
        let options = CopyToOptions {
            table_or_query: "SELECT * FROM users WHERE active = true".to_string(),
            is_query: true,
            columns: None,
            format: CopyFormat::Text,
            header: false,
            delimiter: Some('\t'),
            null_string: Some("\\N".to_string()),
        };

        let sql = build_copy_to_sql(&options);
        assert!(sql.contains("COPY (SELECT * FROM users WHERE active = true)"));
        assert!(sql.contains("TO STDOUT"));
        assert!(sql.contains("DELIMITER '\t'"));
        assert!(sql.contains("NULL '\\N'"));
    }

    #[test]
    fn test_escape_identifier() {
        assert_eq!(escape_identifier("users"), "\"users\"");
        assert_eq!(escape_identifier("public.users"), "\"public\".\"users\"");
        assert_eq!(escape_identifier("my\"table"), "\"my\"\"table\"");
    }
}
