//! Node.js compatibility layer for Otter.
//!
//! This crate provides Node.js-compatible APIs for the Otter runtime.
//!
//! # Modules
//!
//! - `path` - Path manipulation utilities (no capabilities required)
//! - `buffer` - Binary data handling
//! - `fs` - File system operations (requires capabilities)
//! - `crypto` - Cryptographic operations (randomBytes, createHash, etc.)
//! - `stream` - Web Streams API (ReadableStream, WritableStream)
//! - `websocket` - WebSocket client API
//! - `worker` - Web Worker API for background threads
//! - `test` - Test runner (describe, it, assert)
//! - `extensions` - JavaScript extensions for runtime integration
//!
//! # Example
//!
//! ```no_run
//! use otter_node::path;
//! use otter_node::buffer::Buffer;
//!
//! // Path manipulation
//! let joined = path::join(&["foo", "bar", "baz.txt"]);
//! assert_eq!(joined, "foo/bar/baz.txt");
//!
//! // Buffer operations
//! let buf = Buffer::from_string("hello", "utf8").unwrap();
//! assert_eq!(buf.to_string("base64", 0, buf.len()), "aGVsbG8=");
//! ```

pub mod buffer;
pub mod child_process;
pub mod crypto;
pub mod events;
pub mod extensions;
pub mod fs;
pub mod http_request;
pub mod http_server;
pub mod http_service;
pub mod ipc;
pub mod net;
pub mod os;
pub mod path;
pub mod path_ext;
pub mod process;
pub mod stream;
pub mod test;
pub mod url;
pub mod util;
pub mod websocket;
pub mod worker;

pub use buffer::{Buffer, BufferError};
pub use child_process::{
    ChildProcessError, ChildProcessEvent, ChildProcessManager, SpawnOptions, SpawnSyncResult,
    StdioConfig,
};
pub use crypto::{CryptoError, Hash, HashAlgorithm, Hmac};
pub use events::{event_emitter_js, EventEmitter, Listener, DEFAULT_MAX_LISTENERS};
pub use ipc::{has_ipc, IpcChannel, IpcMessage, IPC_FD_ENV};
pub use os::{os_module_js, CpuInfo, CpuTimes, NetworkInterface, OsType, UserInfo};
pub use extensions::{
    create_buffer_extension, create_child_process_extension, create_crypto_extension,
    create_events_extension, create_fs_extension, create_http_server_extension,
    create_os_extension, create_process_extension,
    create_process_ipc_extension, create_streams_extension, create_test_extension,
    create_url_extension, create_util_extension, create_websocket_extension, create_worker_extension,
};
// Path and net extensions use the new #[dive] macro architecture
pub use path_ext::create_path_extension;
pub use net::{create_net_extension, init_net_manager, ActiveNetServerCount, NetError, NetEvent, NetManager};
pub use http_server::{ActiveServerCount, HttpEvent, HttpServer, HttpServerError, HttpServerManager, TlsConfig};
pub use fs::{FsError, ReadResult, Stats};
pub use path::ParsedPath;
pub use process::ProcessInfo;
pub use stream::{StreamChunk, StreamError, StreamManager, StreamState};
pub use test::{
    MockBehavior, MockCall, MockFn, MockManager, MockManagerHandle, SnapshotManager,
    SnapshotManagerHandle, SnapshotResult, SnapshotStats, TestResult, TestRunner, TestRunnerHandle,
    TestSummary, diff_snapshots, mock_assertions,
};
pub use websocket::{
    ReadyState, WebSocketError, WebSocketEvent, WebSocketManager, WebSocketMessage,
};
pub use worker::{WorkerError, WorkerEvent, WorkerManager, WorkerMessage};

// Re-export capabilities for convenience
pub use otter_engine::Capabilities;
