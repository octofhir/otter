//! Node.js compatibility layer for Otter.
//!
//! This crate provides Node.js-compatible APIs for the Otter runtime.
//!
//! # Status
//!
//! The extension modules (_ext) are temporarily disabled while the VM is being ported.
//! Pure logic modules are available for reference.

// Pure logic modules (no otter_runtime dependency)
pub mod buffer;
pub mod child_process;
pub mod crypto;
pub mod dgram;
pub mod dns;
pub mod events;
pub mod fs;
pub mod ipc;
pub mod os;
pub mod path;
pub mod process;
pub mod stream;
pub mod test;
pub mod url;
pub mod websocket;
pub mod worker;
pub mod zlib;

// Modules with cross-dependencies on disabled modules
// pub mod http_request; // depends on http_service
// pub mod tls; // depends on net

// Extension modules (depend on otter_runtime - disabled for now)
// TODO: Re-enable when extension system is ported to new VM
/*
pub mod assert_ext;
pub mod async_hooks_ext;
pub mod buffer_ext;
pub mod child_process_ext;
pub mod crypto_ext;
pub mod dgram_ext;
pub mod dns_ext;
pub mod events_ext;
pub mod ext;
pub mod fs_ext;
pub mod http2_ext;
pub mod http_ext;
pub mod http_server;
pub mod http_server_ext;
pub mod http_service;
pub mod https_ext;
pub mod module_ext;
pub mod net;
pub mod node_stream_ext;
pub mod os_ext;
pub mod path_ext;
pub mod perf_hooks_ext;
pub mod process_ext;
pub mod process_ipc_ext;
pub mod querystring_ext;
pub mod readline_ext;
pub mod streams_ext;
pub mod string_decoder_ext;
pub mod test_ext;
pub mod timers_ext;
pub mod tls_ext;
pub mod tty_ext;
pub mod url_ext;
pub mod util_ext;
pub mod websocket_ext;
pub mod worker_ext;
pub mod worker_threads;
pub mod worker_threads_ext;
pub mod zlib_ext;
*/

pub use buffer::{Buffer, BufferError};
pub use child_process::{
    ChildProcessError, ChildProcessEvent, ChildProcessManager, SpawnOptions, SpawnSyncResult,
    StdioConfig,
};
pub use crypto::{CryptoError, Hash, HashAlgorithm, Hmac};
pub use events::{DEFAULT_MAX_LISTENERS, EventEmitter, Listener, event_emitter_js};
pub use fs::{FsError, ReadResult, Stats};
pub use ipc::{IPC_FD_ENV, IpcChannel, IpcMessage, has_ipc};
pub use os::{CpuInfo, CpuTimes, NetworkInterface, OsType, UserInfo, os_module_js};
pub use path::ParsedPath;
pub use process::ProcessInfo;
pub use stream::{StreamChunk, StreamError, StreamManager, StreamState};
pub use test::{
    MockBehavior, MockCall, MockFn, MockManager, MockManagerHandle, SnapshotManager,
    SnapshotManagerHandle, SnapshotResult, SnapshotStats, TestResult, TestRunner, TestRunnerHandle,
    TestSummary, diff_snapshots, mock_assertions,
};
// pub use tls::{ActiveTlsServerCount, TlsError, TlsEvent, TlsManager, TlsResult}; // disabled
pub use websocket::{
    ReadyState, WebSocketError, WebSocketEvent, WebSocketManager, WebSocketMessage,
};
pub use worker::{WorkerError, WorkerEvent, WorkerManager, WorkerMessage};

// Re-export capabilities for convenience
pub use otter_engine::Capabilities;
