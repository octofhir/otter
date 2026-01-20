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

pub mod assert_ext;
pub mod async_hooks_ext;
pub mod buffer;
pub mod buffer_ext;
pub mod child_process;
pub mod child_process_ext;
pub mod crypto;
pub mod crypto_ext;
pub mod dgram;
pub mod dgram_ext;
pub mod dns;
pub mod dns_ext;
pub mod events;
pub mod events_ext;
pub mod ext;
pub mod fs;
pub mod fs_ext;
pub mod http2_ext;
pub mod http_ext;
pub mod http_request;
pub mod http_server;
pub mod http_server_ext;
pub mod http_service;
pub mod https_ext;
pub mod ipc;
pub mod module_ext;
pub mod net;
pub mod node_stream_ext;
pub mod os;
pub mod perf_hooks_ext;
pub mod os_ext;
pub mod path;
pub mod path_ext;
pub mod process;
pub mod process_ext;
pub mod process_ipc_ext;
pub mod querystring_ext;
pub mod readline_ext;
pub mod stream;
pub mod streams_ext;
pub mod string_decoder_ext;
pub mod test;
pub mod test_ext;
pub mod timers_ext;
pub mod tty_ext;
pub mod url;
pub mod url_ext;
pub mod util_ext;
pub mod websocket;
pub mod websocket_ext;
pub mod worker;
pub mod worker_ext;
pub mod worker_threads;
pub mod worker_threads_ext;
pub mod zlib;
pub mod zlib_ext;

pub use buffer::{Buffer, BufferError};
pub use child_process::{
    ChildProcessError, ChildProcessEvent, ChildProcessManager, SpawnOptions, SpawnSyncResult,
    StdioConfig,
};
pub use crypto::{CryptoError, Hash, HashAlgorithm, Hmac};
pub use events::{DEFAULT_MAX_LISTENERS, EventEmitter, Listener, event_emitter_js};
pub use fs::{FsError, ReadResult, Stats};
pub use http_server::{
    ActiveServerCount, HttpEvent, HttpServer, HttpServerError, HttpServerManager, TlsConfig,
};
pub use ipc::{IPC_FD_ENV, IpcChannel, IpcMessage, has_ipc};
pub use net::{ActiveNetServerCount, NetError, NetEvent, NetManager, init_net_manager};
pub use os::{CpuInfo, CpuTimes, NetworkInterface, OsType, UserInfo, os_module_js};
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
pub use worker_threads::{
    ActiveWorkerCount, ResourceLimits, WorkerThreadError, WorkerThreadEvent, WorkerThreadManager,
    WorkerThreadOptions,
};

// Re-export capabilities for convenience
pub use otter_engine::Capabilities;
