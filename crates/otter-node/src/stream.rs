//! Web Streams API implementation for Otter.
//!
//! Provides Web-standard Streams API for handling streaming data.
//!
//! # Example
//!
//! ```javascript
//! const readable = new ReadableStream({
//!     start(controller) {
//!         controller.enqueue('Hello');
//!         controller.enqueue('World');
//!         controller.close();
//!     }
//! });
//!
//! const reader = readable.getReader();
//! const { value, done } = await reader.read();
//! ```

use parking_lot::Mutex;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use thiserror::Error;

/// Errors that can occur during Stream operations.
#[derive(Debug, Error, Clone)]
pub enum StreamError {
    #[error("Stream is locked")]
    Locked,

    #[error("Stream is closed")]
    Closed,

    #[error("Stream is errored: {0}")]
    Errored(String),

    #[error("Stream not found: {0}")]
    NotFound(u32),

    #[error("Invalid state: {0}")]
    InvalidState(String),

    #[error("Queue is full")]
    QueueFull,
}

/// State of a stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamState {
    /// Stream is readable/writable
    Open,
    /// Stream is closed
    Closed,
    /// Stream has errored
    Errored,
}

/// A chunk of data in a stream.
#[derive(Debug, Clone)]
pub enum StreamChunk {
    /// Text data
    Text(String),
    /// Binary data
    Binary(Vec<u8>),
    /// JSON data
    Json(serde_json::Value),
}

impl StreamChunk {
    /// Convert to JSON value
    pub fn to_json(&self) -> serde_json::Value {
        match self {
            StreamChunk::Text(s) => serde_json::Value::String(s.clone()),
            StreamChunk::Binary(b) => serde_json::json!({
                "type": "Buffer",
                "data": b
            }),
            StreamChunk::Json(v) => v.clone(),
        }
    }

    /// Create from JSON value
    pub fn from_json(value: serde_json::Value) -> Self {
        if let Some(s) = value.as_str() {
            StreamChunk::Text(s.to_string())
        } else if let Some(obj) = value.as_object() {
            if obj.get("type").and_then(|v| v.as_str()) == Some("Buffer")
                && let Some(data) = obj.get("data").and_then(|v| v.as_array())
            {
                let bytes: Vec<u8> = data
                    .iter()
                    .filter_map(|v| v.as_u64().map(|n| n as u8))
                    .collect();
                return StreamChunk::Binary(bytes);
            }
            StreamChunk::Json(serde_json::Value::Object(obj.clone()))
        } else {
            StreamChunk::Json(value)
        }
    }
}

/// Internal readable stream data
struct ReadableStreamData {
    queue: VecDeque<StreamChunk>,
    state: StreamState,
    locked: bool,
    high_water_mark: usize,
    error: Option<String>,
}

impl Default for ReadableStreamData {
    fn default() -> Self {
        Self {
            queue: VecDeque::new(),
            state: StreamState::Open,
            locked: false,
            high_water_mark: 16, // Default high water mark
            error: None,
        }
    }
}

/// Internal writable stream data
struct WritableStreamData {
    chunks: Vec<StreamChunk>,
    state: StreamState,
    locked: bool,
    error: Option<String>,
}

impl Default for WritableStreamData {
    fn default() -> Self {
        Self {
            chunks: Vec::new(),
            state: StreamState::Open,
            locked: false,
            error: None,
        }
    }
}

/// Manager for Web Streams.
pub struct StreamManager {
    readable_streams: Mutex<HashMap<u32, ReadableStreamData>>,
    writable_streams: Mutex<HashMap<u32, WritableStreamData>>,
    next_id: AtomicU32,
    /// Flag to track if there are pending reads
    has_pending_reads: AtomicBool,
}

impl StreamManager {
    /// Create a new stream manager.
    pub fn new() -> Self {
        Self {
            readable_streams: Mutex::new(HashMap::new()),
            writable_streams: Mutex::new(HashMap::new()),
            next_id: AtomicU32::new(1),
            has_pending_reads: AtomicBool::new(false),
        }
    }

    /// Create a new readable stream.
    pub fn create_readable(&self, high_water_mark: Option<usize>) -> u32 {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let mut data = ReadableStreamData::default();
        if let Some(hwm) = high_water_mark {
            data.high_water_mark = hwm;
        }
        self.readable_streams.lock().insert(id, data);
        id
    }

    /// Create a new writable stream.
    pub fn create_writable(&self) -> u32 {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        self.writable_streams
            .lock()
            .insert(id, WritableStreamData::default());
        id
    }

    /// Enqueue a chunk to a readable stream.
    pub fn enqueue(&self, id: u32, chunk: StreamChunk) -> Result<(), StreamError> {
        let mut streams = self.readable_streams.lock();
        let stream = streams.get_mut(&id).ok_or(StreamError::NotFound(id))?;

        if stream.state != StreamState::Open {
            return Err(StreamError::Closed);
        }

        if stream.queue.len() >= stream.high_water_mark {
            return Err(StreamError::QueueFull);
        }

        stream.queue.push_back(chunk);
        Ok(())
    }

    /// Read from a readable stream.
    pub fn read(&self, id: u32) -> Result<Option<StreamChunk>, StreamError> {
        let mut streams = self.readable_streams.lock();
        let stream = streams.get_mut(&id).ok_or(StreamError::NotFound(id))?;

        if stream.state == StreamState::Errored {
            return Err(StreamError::Errored(
                stream.error.clone().unwrap_or_default(),
            ));
        }

        if let Some(chunk) = stream.queue.pop_front() {
            Ok(Some(chunk))
        } else if stream.state == StreamState::Closed {
            Ok(None) // Done
        } else {
            // No data available yet
            self.has_pending_reads.store(true, Ordering::SeqCst);
            Ok(None)
        }
    }

    /// Close a readable stream.
    pub fn close_readable(&self, id: u32) -> Result<(), StreamError> {
        let mut streams = self.readable_streams.lock();
        let stream = streams.get_mut(&id).ok_or(StreamError::NotFound(id))?;
        stream.state = StreamState::Closed;
        Ok(())
    }

    /// Error a readable stream.
    pub fn error_readable(&self, id: u32, error: String) -> Result<(), StreamError> {
        let mut streams = self.readable_streams.lock();
        let stream = streams.get_mut(&id).ok_or(StreamError::NotFound(id))?;
        stream.state = StreamState::Errored;
        stream.error = Some(error);
        Ok(())
    }

    /// Get readable stream state.
    pub fn readable_state(&self, id: u32) -> Option<StreamState> {
        self.readable_streams.lock().get(&id).map(|s| s.state)
    }

    /// Lock a readable stream.
    pub fn lock_readable(&self, id: u32) -> Result<(), StreamError> {
        let mut streams = self.readable_streams.lock();
        let stream = streams.get_mut(&id).ok_or(StreamError::NotFound(id))?;

        if stream.locked {
            return Err(StreamError::Locked);
        }

        stream.locked = true;
        Ok(())
    }

    /// Unlock a readable stream.
    pub fn unlock_readable(&self, id: u32) -> Result<(), StreamError> {
        let mut streams = self.readable_streams.lock();
        let stream = streams.get_mut(&id).ok_or(StreamError::NotFound(id))?;
        stream.locked = false;
        Ok(())
    }

    /// Check if readable stream is locked.
    pub fn is_readable_locked(&self, id: u32) -> bool {
        self.readable_streams
            .lock()
            .get(&id)
            .is_some_and(|s| s.locked)
    }

    /// Get queue size of readable stream.
    pub fn readable_queue_size(&self, id: u32) -> usize {
        self.readable_streams
            .lock()
            .get(&id)
            .map(|s| s.queue.len())
            .unwrap_or(0)
    }

    /// Write to a writable stream.
    pub fn write(&self, id: u32, chunk: StreamChunk) -> Result<(), StreamError> {
        let mut streams = self.writable_streams.lock();
        let stream = streams.get_mut(&id).ok_or(StreamError::NotFound(id))?;

        if stream.state != StreamState::Open {
            return Err(StreamError::Closed);
        }

        stream.chunks.push(chunk);
        Ok(())
    }

    /// Close a writable stream.
    pub fn close_writable(&self, id: u32) -> Result<(), StreamError> {
        let mut streams = self.writable_streams.lock();
        let stream = streams.get_mut(&id).ok_or(StreamError::NotFound(id))?;
        stream.state = StreamState::Closed;
        Ok(())
    }

    /// Error a writable stream.
    pub fn error_writable(&self, id: u32, error: String) -> Result<(), StreamError> {
        let mut streams = self.writable_streams.lock();
        let stream = streams.get_mut(&id).ok_or(StreamError::NotFound(id))?;
        stream.state = StreamState::Errored;
        stream.error = Some(error);
        Ok(())
    }

    /// Get writable stream state.
    pub fn writable_state(&self, id: u32) -> Option<StreamState> {
        self.writable_streams.lock().get(&id).map(|s| s.state)
    }

    /// Lock a writable stream.
    pub fn lock_writable(&self, id: u32) -> Result<(), StreamError> {
        let mut streams = self.writable_streams.lock();
        let stream = streams.get_mut(&id).ok_or(StreamError::NotFound(id))?;

        if stream.locked {
            return Err(StreamError::Locked);
        }

        stream.locked = true;
        Ok(())
    }

    /// Unlock a writable stream.
    pub fn unlock_writable(&self, id: u32) -> Result<(), StreamError> {
        let mut streams = self.writable_streams.lock();
        let stream = streams.get_mut(&id).ok_or(StreamError::NotFound(id))?;
        stream.locked = false;
        Ok(())
    }

    /// Check if writable stream is locked.
    pub fn is_writable_locked(&self, id: u32) -> bool {
        self.writable_streams
            .lock()
            .get(&id)
            .is_some_and(|s| s.locked)
    }

    /// Get all written chunks from a writable stream.
    pub fn get_written_chunks(&self, id: u32) -> Vec<StreamChunk> {
        self.writable_streams
            .lock()
            .get(&id)
            .map(|s| s.chunks.clone())
            .unwrap_or_default()
    }

    /// Check if there are any pending reads.
    pub fn has_pending_reads(&self) -> bool {
        self.has_pending_reads.load(Ordering::SeqCst)
    }

    /// Clear pending reads flag.
    pub fn clear_pending_reads(&self) {
        self.has_pending_reads.store(false, Ordering::SeqCst);
    }

    /// Delete a readable stream.
    pub fn delete_readable(&self, id: u32) {
        self.readable_streams.lock().remove(&id);
    }

    /// Delete a writable stream.
    pub fn delete_writable(&self, id: u32) {
        self.writable_streams.lock().remove(&id);
    }
}

impl Default for StreamManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_stream_manager_creation() {
        let manager = StreamManager::new();
        assert!(!manager.has_pending_reads());
    }

    #[test]
    fn test_create_readable_stream() {
        let manager = StreamManager::new();
        let id = manager.create_readable(None);
        assert!(id > 0);
        assert_eq!(manager.readable_state(id), Some(StreamState::Open));
    }

    #[test]
    fn test_create_writable_stream() {
        let manager = StreamManager::new();
        let id = manager.create_writable();
        assert!(id > 0);
        assert_eq!(manager.writable_state(id), Some(StreamState::Open));
    }

    #[test]
    fn test_enqueue_and_read() {
        let manager = StreamManager::new();
        let id = manager.create_readable(None);

        manager
            .enqueue(id, StreamChunk::Text("hello".to_string()))
            .unwrap();
        manager
            .enqueue(id, StreamChunk::Text("world".to_string()))
            .unwrap();

        let chunk1 = manager.read(id).unwrap();
        assert!(matches!(chunk1, Some(StreamChunk::Text(s)) if s == "hello"));

        let chunk2 = manager.read(id).unwrap();
        assert!(matches!(chunk2, Some(StreamChunk::Text(s)) if s == "world"));

        let chunk3 = manager.read(id).unwrap();
        assert!(chunk3.is_none()); // No more data
    }

    #[test]
    fn test_close_readable() {
        let manager = StreamManager::new();
        let id = manager.create_readable(None);

        manager.close_readable(id).unwrap();
        assert_eq!(manager.readable_state(id), Some(StreamState::Closed));
    }

    #[test]
    fn test_error_readable() {
        let manager = StreamManager::new();
        let id = manager.create_readable(None);

        manager
            .error_readable(id, "test error".to_string())
            .unwrap();
        assert_eq!(manager.readable_state(id), Some(StreamState::Errored));

        let result = manager.read(id);
        assert!(matches!(result, Err(StreamError::Errored(_))));
    }

    #[test]
    fn test_write_to_writable() {
        let manager = StreamManager::new();
        let id = manager.create_writable();

        manager
            .write(id, StreamChunk::Text("data1".to_string()))
            .unwrap();
        manager
            .write(id, StreamChunk::Text("data2".to_string()))
            .unwrap();

        let chunks = manager.get_written_chunks(id);
        assert_eq!(chunks.len(), 2);
    }

    #[test]
    fn test_lock_unlock_readable() {
        let manager = StreamManager::new();
        let id = manager.create_readable(None);

        assert!(!manager.is_readable_locked(id));

        manager.lock_readable(id).unwrap();
        assert!(manager.is_readable_locked(id));

        // Second lock should fail
        let result = manager.lock_readable(id);
        assert!(matches!(result, Err(StreamError::Locked)));

        manager.unlock_readable(id).unwrap();
        assert!(!manager.is_readable_locked(id));
    }

    #[test]
    fn test_stream_chunk_json_conversion() {
        let text = StreamChunk::Text("hello".to_string());
        let json = text.to_json();
        assert_eq!(json, serde_json::json!("hello"));

        let binary = StreamChunk::Binary(vec![1, 2, 3]);
        let json = binary.to_json();
        assert_eq!(
            json,
            serde_json::json!({"type": "Buffer", "data": [1, 2, 3]})
        );
    }

    #[test]
    fn test_queue_full() {
        let manager = StreamManager::new();
        let id = manager.create_readable(Some(2)); // High water mark of 2

        manager
            .enqueue(id, StreamChunk::Text("1".to_string()))
            .unwrap();
        manager
            .enqueue(id, StreamChunk::Text("2".to_string()))
            .unwrap();

        // Third enqueue should fail
        let result = manager.enqueue(id, StreamChunk::Text("3".to_string()));
        assert!(matches!(result, Err(StreamError::QueueFull)));
    }

    #[test]
    fn test_delete_streams() {
        let manager = StreamManager::new();
        let readable_id = manager.create_readable(None);
        let writable_id = manager.create_writable();

        manager.delete_readable(readable_id);
        manager.delete_writable(writable_id);

        assert_eq!(manager.readable_state(readable_id), None);
        assert_eq!(manager.writable_state(writable_id), None);
    }
}
