//! Async operation tracing

use parking_lot::Mutex;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

/// Async operation tracer
pub struct AsyncTracer {
    /// Next span ID
    next_id: AtomicU64,
    /// Active spans
    active: Mutex<HashMap<u64, AsyncSpan>>,
    /// Completed spans
    completed: Mutex<Vec<AsyncSpan>>,
    /// Start time for timestamps
    start_time: Instant,
}

/// An async operation span
#[derive(Debug, Clone, Serialize)]
pub struct AsyncSpan {
    /// Unique ID
    pub id: u64,
    /// Operation name
    pub name: String,
    /// Parent span ID (for nesting)
    pub parent_id: Option<u64>,
    /// Start timestamp (microseconds)
    pub start_us: u64,
    /// End timestamp (microseconds)
    pub end_us: Option<u64>,
    /// Operation type (fetch, file, timer, etc)
    pub op_type: String,
    /// Additional metadata
    pub metadata: HashMap<String, String>,
}

impl AsyncTracer {
    /// Create new tracer
    pub fn new() -> Self {
        Self {
            next_id: AtomicU64::new(1),
            active: Mutex::new(HashMap::new()),
            completed: Mutex::new(Vec::new()),
            start_time: Instant::now(),
        }
    }

    /// Start a new span
    pub fn span_start(&self, name: &str, op_type: &str, parent_id: Option<u64>) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let span = AsyncSpan {
            id,
            name: name.to_string(),
            parent_id,
            start_us: self.start_time.elapsed().as_micros() as u64,
            end_us: None,
            op_type: op_type.to_string(),
            metadata: HashMap::new(),
        };

        self.active.lock().insert(id, span);
        id
    }

    /// End a span
    pub fn span_end(&self, id: u64) {
        if let Some(mut span) = self.active.lock().remove(&id) {
            span.end_us = Some(self.start_time.elapsed().as_micros() as u64);
            self.completed.lock().push(span);
        }
    }

    /// Add metadata to active span
    pub fn add_metadata(&self, id: u64, key: &str, value: &str) {
        if let Some(span) = self.active.lock().get_mut(&id) {
            span.metadata.insert(key.to_string(), value.to_string());
        }
    }

    /// Get completed spans
    pub fn completed_spans(&self) -> Vec<AsyncSpan> {
        self.completed.lock().clone()
    }

    /// Export to Chrome trace format
    pub fn to_chrome_trace(&self) -> serde_json::Value {
        let completed = self.completed.lock();
        let events: Vec<_> = completed
            .iter()
            .map(|span| {
                serde_json::json!({
                    "name": span.name,
                    "cat": span.op_type,
                    "ph": "X",
                    "ts": span.start_us,
                    "dur": span.end_us.unwrap_or(span.start_us) - span.start_us,
                    "pid": 1,
                    "tid": 1,
                    "args": span.metadata,
                })
            })
            .collect();

        serde_json::json!({
            "traceEvents": events
        })
    }

    /// Export to OpenTelemetry format
    pub fn to_otlp(&self) -> serde_json::Value {
        let completed = self.completed.lock();
        let spans: Vec<_> = completed
            .iter()
            .map(|span| {
                serde_json::json!({
                    "traceId": format!("{:032x}", span.id),
                    "spanId": format!("{:016x}", span.id),
                    "parentSpanId": span.parent_id.map(|id| format!("{:016x}", id)),
                    "name": span.name,
                    "startTimeUnixNano": span.start_us * 1000,
                    "endTimeUnixNano": span.end_us.map(|t| t * 1000),
                    "attributes": span.metadata.iter().map(|(k, v)| {
                        serde_json::json!({"key": k, "value": {"stringValue": v}})
                    }).collect::<Vec<_>>(),
                })
            })
            .collect();

        serde_json::json!({
            "resourceSpans": [{
                "scopeSpans": [{
                    "spans": spans
                }]
            }]
        })
    }
}

impl Default for AsyncTracer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_span_lifecycle() {
        let tracer = AsyncTracer::new();

        let id = tracer.span_start("fetch", "network", None);
        tracer.add_metadata(id, "url", "https://example.com");
        tracer.span_end(id);

        let spans = tracer.completed_spans();
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].name, "fetch");
        assert!(spans[0].end_us.is_some());
    }

    #[test]
    fn test_nested_spans() {
        let tracer = AsyncTracer::new();

        let parent = tracer.span_start("request", "http", None);
        let child = tracer.span_start("parse", "json", Some(parent));

        tracer.span_end(child);
        tracer.span_end(parent);

        let spans = tracer.completed_spans();
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].parent_id, Some(parent));
    }

    #[test]
    fn test_chrome_trace_export() {
        let tracer = AsyncTracer::new();

        let id = tracer.span_start("test_op", "test", None);
        tracer.span_end(id);

        let trace = tracer.to_chrome_trace();
        let events = trace["traceEvents"].as_array().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["name"], "test_op");
    }

    #[test]
    fn test_otlp_export() {
        let tracer = AsyncTracer::new();

        let id = tracer.span_start("test_op", "test", None);
        tracer.span_end(id);

        let otlp = tracer.to_otlp();
        let spans = &otlp["resourceSpans"][0]["scopeSpans"][0]["spans"];
        assert!(spans.is_array());
        assert_eq!(spans.as_array().unwrap().len(), 1);
    }
}
