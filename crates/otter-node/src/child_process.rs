//! Child process spawning and management for Otter.
//!
//! Provides both `Otter.spawn()` and Node.js-compatible `child_process` APIs.
//!
//!
//! ```javascript
//! const proc = Otter.spawn(["echo", "hello"]);
//! const output = await proc.stdout.text();
//! await proc.exited;
//! ```
//!
//! # Example (Node.js-style)
//!
//! ```javascript
//! import { spawn, execSync } from 'child_process';
//!
//! const child = spawn('ls', ['-la']);
//! child.stdout.on('data', (data) => console.log(data.toString()));
//! child.on('exit', (code) => console.log('exit:', code));
//!
//! const result = execSync('echo hello');
//! ```

use parking_lot::Mutex;
use std::collections::HashMap;
use std::io;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot};

#[cfg(unix)]
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
#[cfg(unix)]
use std::os::unix::net::UnixStream;
#[cfg(unix)]
use std::os::unix::process::CommandExt;

/// Errors that can occur during child process operations.
#[derive(Debug, Error)]
pub enum ChildProcessError {
    #[error("Process not found: {0}")]
    NotFound(u32),

    #[error("Process already exited")]
    AlreadyExited,

    #[error("Spawn failed: {0}")]
    SpawnFailed(String),

    #[error("IO error: {0}")]
    IoError(String),

    #[error("Signal error: {0}")]
    SignalError(String),

    #[error("Kill failed: {0}")]
    KillFailed(String),

    #[error("Timeout exceeded")]
    Timeout,

    #[error("Permission denied: spawn not allowed")]
    PermissionDenied,

    #[error("Invalid argument: {0}")]
    InvalidArgument(String),

    #[error("IPC error: {0}")]
    IpcError(String),

    #[error("IPC not enabled")]
    IpcNotEnabled,
}

impl From<io::Error> for ChildProcessError {
    fn from(err: io::Error) -> Self {
        ChildProcessError::IoError(err.to_string())
    }
}

/// Configuration for stdio streams.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StdioConfig {
    /// Create a pipe for the stream
    Pipe,
    /// Ignore the stream (/dev/null)
    Ignore,
    /// Inherit from parent process
    Inherit,
}

impl Default for StdioConfig {
    fn default() -> Self {
        StdioConfig::Pipe
    }
}

impl StdioConfig {
    fn to_stdio(self) -> Stdio {
        match self {
            StdioConfig::Pipe => Stdio::piped(),
            StdioConfig::Ignore => Stdio::null(),
            StdioConfig::Inherit => Stdio::inherit(),
        }
    }
}

/// Options for spawning a child process.
#[derive(Debug, Clone, Default)]
pub struct SpawnOptions {
    /// Working directory for the child process
    pub cwd: Option<String>,
    /// Environment variables (replaces inherited env if set)
    pub env: Option<HashMap<String, String>>,
    /// Configuration for stdin
    pub stdin: StdioConfig,
    /// Configuration for stdout
    pub stdout: StdioConfig,
    /// Configuration for stderr
    pub stderr: StdioConfig,
    /// Run command through shell
    pub shell: Option<String>,
    /// Timeout in milliseconds (0 = no timeout)
    pub timeout: Option<u64>,
    /// Signal to send when timeout is reached (default: SIGTERM)
    pub kill_signal: Option<String>,
    /// Detach the child process
    pub detached: bool,
    /// Enable IPC channel for fork()
    pub ipc: bool,
}

/// Events emitted by child processes.
#[derive(Debug, Clone)]
pub enum ChildProcessEvent {
    /// Process spawned successfully
    Spawn,
    /// Data received from stdout
    Stdout(Vec<u8>),
    /// Data received from stderr
    Stderr(Vec<u8>),
    /// Process exited with exit code and optional signal
    Exit {
        code: Option<i32>,
        signal: Option<String>,
    },
    /// All stdio streams closed
    Close {
        code: Option<i32>,
        signal: Option<String>,
    },
    /// Error occurred
    Error(String),
    /// IPC message received (for fork)
    Message(serde_json::Value),
}

/// Result of synchronous spawn operations.
#[derive(Debug, Clone)]
pub struct SpawnSyncResult {
    /// Process ID (if successfully spawned)
    pub pid: Option<u32>,
    /// Stdout output
    pub stdout: Vec<u8>,
    /// Stderr output
    pub stderr: Vec<u8>,
    /// Exit status code
    pub status: Option<i32>,
    /// Signal that terminated the process
    pub signal: Option<String>,
    /// Error message if spawn failed
    pub error: Option<String>,
}

/// Internal handle for a child process.
struct ChildProcessHandle {
    #[allow(dead_code)]
    id: u32,
    pid: Option<u32>,
    stdin_tx: Option<mpsc::Sender<Vec<u8>>>,
    /// IPC channel sender for sending messages to child (for fork)
    ipc_tx: Option<mpsc::Sender<Vec<u8>>>,
    running: Arc<AtomicBool>,
    exit_code: Arc<Mutex<Option<i32>>>,
    signal_code: Arc<Mutex<Option<String>>>,
    killed: AtomicBool,
    ref_count: AtomicBool,
}

/// Manager for child processes.
///
/// Handles spawning, communication, and lifecycle management of child processes.
pub struct ChildProcessManager {
    processes: Mutex<HashMap<u32, ChildProcessHandle>>,
    next_id: AtomicU32,
    event_tx: mpsc::Sender<(u32, ChildProcessEvent)>,
    event_rx: Mutex<mpsc::Receiver<(u32, ChildProcessEvent)>>,
}

impl ChildProcessManager {
    /// Create a new child process manager.
    pub fn new() -> Self {
        let (event_tx, event_rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
        Self {
            processes: Mutex::new(HashMap::new()),
            next_id: AtomicU32::new(1),
            event_tx,
            event_rx: Mutex::new(event_rx),
        }
    }

    /// Spawn a new child process asynchronously.
    ///
    /// Returns the internal process ID (not OS PID).
    pub fn spawn(
        &self,
        command: &[String],
        options: SpawnOptions,
    ) -> Result<u32, ChildProcessError> {
        if command.is_empty() {
            return Err(ChildProcessError::InvalidArgument(
                "Command cannot be empty".to_string(),
            ));
        }

        let id = self.next_id.fetch_add(1, Ordering::SeqCst);

        // Build the command
        let mut cmd = if let Some(ref shell) = options.shell {
            let mut c = Command::new(shell);
            c.arg("-c");
            c.arg(command.join(" "));
            c
        } else {
            let mut c = Command::new(&command[0]);
            if command.len() > 1 {
                c.args(&command[1..]);
            }
            c
        };

        // Set working directory
        if let Some(ref cwd) = options.cwd {
            cmd.current_dir(cwd);
        }

        // Set environment
        if let Some(ref env) = options.env {
            cmd.env_clear();
            for (k, v) in env {
                cmd.env(k, v);
            }
        }

        // Configure stdio
        cmd.stdin(options.stdin.to_stdio());
        cmd.stdout(options.stdout.to_stdio());
        cmd.stderr(options.stderr.to_stdio());

        // Setup IPC if enabled (Unix only for now)
        #[cfg(unix)]
        let ipc_parent_socket = if options.ipc {
            // Create a Unix socket pair for IPC
            let (parent_socket, child_socket) =
                UnixStream::pair().map_err(|e| ChildProcessError::IpcError(e.to_string()))?;

            // Make child socket non-blocking for tokio
            parent_socket
                .set_nonblocking(true)
                .map_err(|e| ChildProcessError::IpcError(e.to_string()))?;

            // Get child socket's fd before we move it
            let child_fd = child_socket.as_raw_fd();

            // Set IPC environment variables (both for Node.js and Otter compatibility)
            cmd.env("NODE_CHANNEL_FD", "3");
            cmd.env("OTTER_IPC_FD", "3");

            // SAFETY: pre_exec runs in child after fork, before exec.
            // We duplicate the child socket to fd 3 (standard for Node IPC).
            // The child_socket will be closed when dropped in parent.
            unsafe {
                cmd.pre_exec(move || {
                    // Duplicate child_fd to fd 3
                    if libc::dup2(child_fd, 3) == -1 {
                        return Err(io::Error::last_os_error());
                    }
                    // Close the original fd if it's not 3
                    if child_fd != 3 {
                        libc::close(child_fd);
                    }
                    Ok(())
                });
            }

            Some((parent_socket, child_socket))
        } else {
            None
        };

        #[cfg(not(unix))]
        let ipc_parent_socket: Option<()> = None;

        // Spawn the process
        let mut child = cmd
            .spawn()
            .map_err(|e| ChildProcessError::SpawnFailed(e.to_string()))?;

        // Child's end of socket is dropped when ipc_parent_socket is destructured
        // The parent_socket is used for IPC, child_socket is passed to child via dup2
        #[cfg(unix)]
        let _ = &ipc_parent_socket; // Suppress unused variable warning

        let pid = child.id();
        let running = Arc::new(AtomicBool::new(true));
        let exit_code = Arc::new(Mutex::new(None));
        let signal_code = Arc::new(Mutex::new(None));

        // Send spawn event (use try_send since we're in sync context)
        let _ = self.event_tx.try_send((id, ChildProcessEvent::Spawn));

        // Setup stdin writer with bounded channel for backpressure
        let stdin_tx = if options.stdin == StdioConfig::Pipe {
            let (tx, mut rx) = mpsc::channel::<Vec<u8>>(STDIN_CHANNEL_CAPACITY);
            if let Some(mut stdin) = child.stdin.take() {
                tokio::spawn(async move {
                    while let Some(data) = rx.recv().await {
                        if stdin.write_all(&data).await.is_err() {
                            break;
                        }
                        // Note: flush() after every write is inefficient for pipes,
                        // but ensures data is delivered promptly for interactive use.
                        // Consider removing if bulk throughput is more important.
                        if stdin.flush().await.is_err() {
                            break;
                        }
                    }
                });
            }
            Some(tx)
        } else {
            None
        };

        // Setup IPC channel reader/writer (Unix only for now)
        // Uses length-prefixed binary protocol: [len: u32 LE][payload: JSON bytes]
        #[cfg(unix)]
        let ipc_tx = if let Some((parent_socket, _child_socket)) = ipc_parent_socket {
            use tokio::io::AsyncReadExt;

            // Convert std UnixStream to tokio UnixStream
            let tokio_socket = tokio::net::UnixStream::from_std(parent_socket)
                .map_err(|e| ChildProcessError::IpcError(e.to_string()))?;

            let (mut read_half, mut write_half) = tokio_socket.into_split();

            // Setup IPC writer - uses length-prefixed protocol
            let (tx, mut rx) = mpsc::channel::<Vec<u8>>(STDIN_CHANNEL_CAPACITY);
            tokio::spawn(async move {
                while let Some(payload) = rx.recv().await {
                    // Write length prefix (4 bytes, little-endian)
                    let len = (payload.len() as u32).to_le_bytes();
                    if write_half.write_all(&len).await.is_err() {
                        break;
                    }
                    // Write payload
                    if write_half.write_all(&payload).await.is_err() {
                        break;
                    }
                    if write_half.flush().await.is_err() {
                        break;
                    }
                }
            });

            // Setup IPC reader - uses length-prefixed protocol
            let event_tx = self.event_tx.clone();
            tokio::spawn(async move {
                loop {
                    // Read length prefix
                    let mut len_buf = [0u8; 4];
                    if read_half.read_exact(&mut len_buf).await.is_err() {
                        break;
                    }
                    let len = u32::from_le_bytes(len_buf) as usize;

                    // Sanity check on message size (max 64MB)
                    if len > 64 * 1024 * 1024 {
                        break;
                    }

                    // Read payload
                    let mut payload = vec![0u8; len];
                    if read_half.read_exact(&mut payload).await.is_err() {
                        break;
                    }

                    // Parse JSON and emit message event
                    if let Ok(msg) = serde_json::from_slice::<serde_json::Value>(&payload) {
                        let _ = event_tx.send((id, ChildProcessEvent::Message(msg))).await;
                    }
                }
            });

            Some(tx)
        } else {
            None
        };

        #[cfg(not(unix))]
        let ipc_tx: Option<mpsc::Sender<Vec<u8>>> = None;

        // Setup stdout/stderr readers - returns receivers that signal when readers are done
        let (stdout_done_rx, stderr_done_rx) =
            self.setup_output_readers(&mut child, id, options.stdout, options.stderr);

        // Setup process wait
        let event_tx = self.event_tx.clone();
        let running_clone = running.clone();
        let exit_code_clone = exit_code.clone();
        let signal_code_clone = signal_code.clone();

        tokio::spawn(async move {
            let status = child.wait().await;
            running_clone.store(false, Ordering::SeqCst);

            match status {
                Ok(exit_status) => {
                    let code = exit_status.code();
                    *exit_code_clone.lock() = code;

                    #[cfg(unix)]
                    let signal = {
                        use std::os::unix::process::ExitStatusExt;
                        exit_status.signal().map(signal_name)
                    };
                    #[cfg(not(unix))]
                    let signal: Option<String> = None;

                    *signal_code_clone.lock() = signal.clone();

                    // Send Exit event first
                    let _ = event_tx
                        .send((
                            id,
                            ChildProcessEvent::Exit {
                                code,
                                signal: signal.clone(),
                            },
                        ))
                        .await;

                    // Wait for stdout/stderr readers to finish before sending Close
                    // This ensures all output data is delivered before Close event
                    let _ = tokio::time::timeout(Duration::from_secs(5), async {
                        if let Some(rx) = stdout_done_rx {
                            let _ = rx.await;
                        }
                        if let Some(rx) = stderr_done_rx {
                            let _ = rx.await;
                        }
                    })
                    .await;

                    // Now safe to send Close - all data has been read
                    let _ = event_tx
                        .send((id, ChildProcessEvent::Close { code, signal }))
                        .await;
                }
                Err(e) => {
                    let _ = event_tx
                        .send((id, ChildProcessEvent::Error(e.to_string())))
                        .await;
                }
            }
        });

        // Store handle
        let handle = ChildProcessHandle {
            id,
            pid,
            stdin_tx,
            ipc_tx,
            running,
            exit_code,
            signal_code,
            killed: AtomicBool::new(false),
            ref_count: AtomicBool::new(true),
        };

        self.processes.lock().insert(id, handle);

        Ok(id)
    }

    /// Helper to setup stdout/stderr readers.
    /// Returns oneshot receivers that signal when each reader has finished (EOF).
    fn setup_output_readers(
        &self,
        child: &mut Child,
        id: u32,
        stdout_config: StdioConfig,
        stderr_config: StdioConfig,
    ) -> (Option<oneshot::Receiver<()>>, Option<oneshot::Receiver<()>>) {
        // Stdout reader
        let stdout_done_rx = if stdout_config == StdioConfig::Pipe {
            if let Some(stdout) = child.stdout.take() {
                let event_tx = self.event_tx.clone();
                let (done_tx, done_rx) = oneshot::channel();
                tokio::spawn(async move {
                    let mut reader = BufReader::new(stdout);
                    let mut buf = vec![0u8; 8192];
                    loop {
                        use tokio::io::AsyncReadExt;
                        match reader.read(&mut buf).await {
                            Ok(0) => break,
                            Ok(n) => {
                                // Await on send provides natural backpressure if event
                                // channel is full (JS not polling fast enough)
                                if event_tx
                                    .send((id, ChildProcessEvent::Stdout(buf[..n].to_vec())))
                                    .await
                                    .is_err()
                                {
                                    break; // Receiver dropped
                                }
                            }
                            Err(_) => break,
                        }
                    }
                    // Signal that stdout reader is done
                    let _ = done_tx.send(());
                });
                Some(done_rx)
            } else {
                None
            }
        } else {
            None
        };

        // Stderr reader
        let stderr_done_rx = if stderr_config == StdioConfig::Pipe {
            if let Some(stderr) = child.stderr.take() {
                let event_tx = self.event_tx.clone();
                let (done_tx, done_rx) = oneshot::channel();
                tokio::spawn(async move {
                    let mut reader = BufReader::new(stderr);
                    let mut buf = vec![0u8; 8192];
                    loop {
                        use tokio::io::AsyncReadExt;
                        match reader.read(&mut buf).await {
                            Ok(0) => break,
                            Ok(n) => {
                                // Await on send provides natural backpressure if event
                                // channel is full (JS not polling fast enough)
                                if event_tx
                                    .send((id, ChildProcessEvent::Stderr(buf[..n].to_vec())))
                                    .await
                                    .is_err()
                                {
                                    break; // Receiver dropped
                                }
                            }
                            Err(_) => break,
                        }
                    }
                    // Signal that stderr reader is done
                    let _ = done_tx.send(());
                });
                Some(done_rx)
            } else {
                None
            }
        } else {
            None
        };

        (stdout_done_rx, stderr_done_rx)
    }

    /// Spawn a process synchronously and wait for completion.
    ///
    /// Supports `timeout` option (in milliseconds) - if the process doesn't complete
    /// within the timeout, it will be killed with `kill_signal` (default: SIGTERM).
    pub fn spawn_sync(&self, command: &[String], options: SpawnOptions) -> SpawnSyncResult {
        if command.is_empty() {
            return SpawnSyncResult {
                pid: None,
                stdout: vec![],
                stderr: vec![],
                status: None,
                signal: None,
                error: Some("Command cannot be empty".to_string()),
            };
        }

        // Build command
        let mut cmd = if let Some(ref shell) = options.shell {
            let mut c = std::process::Command::new(shell);
            c.arg("-c");
            c.arg(command.join(" "));
            c
        } else {
            let mut c = std::process::Command::new(&command[0]);
            if command.len() > 1 {
                c.args(&command[1..]);
            }
            c
        };

        // Set working directory
        if let Some(ref cwd) = options.cwd {
            cmd.current_dir(cwd);
        }

        // Set environment
        if let Some(ref env) = options.env {
            cmd.env_clear();
            for (k, v) in env {
                cmd.env(k, v);
            }
        }

        // Capture output
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        // Spawn the process
        match cmd.spawn() {
            Ok(mut child) => {
                let pid = child.id();
                let timeout_ms = options.timeout.unwrap_or(0);

                // If timeout is set, use a watchdog thread
                if timeout_ms > 0 {
                    self.spawn_sync_with_timeout(child, pid, timeout_ms, options.kill_signal)
                } else {
                    // No timeout - simple wait
                    match child.wait_with_output() {
                        Ok(output) => {
                            #[cfg(unix)]
                            let signal = {
                                use std::os::unix::process::ExitStatusExt;
                                output.status.signal().map(signal_name)
                            };
                            #[cfg(not(unix))]
                            let signal: Option<String> = None;

                            SpawnSyncResult {
                                pid: Some(pid),
                                stdout: output.stdout,
                                stderr: output.stderr,
                                status: output.status.code(),
                                signal,
                                error: None,
                            }
                        }
                        Err(e) => SpawnSyncResult {
                            pid: Some(pid),
                            stdout: vec![],
                            stderr: vec![],
                            status: None,
                            signal: None,
                            error: Some(e.to_string()),
                        },
                    }
                }
            }
            Err(e) => SpawnSyncResult {
                pid: None,
                stdout: vec![],
                stderr: vec![],
                status: None,
                signal: None,
                error: Some(e.to_string()),
            },
        }
    }

    /// Helper for spawn_sync with timeout support.
    fn spawn_sync_with_timeout(
        &self,
        mut child: std::process::Child,
        pid: u32,
        timeout_ms: u64,
        kill_signal: Option<String>,
    ) -> SpawnSyncResult {
        use std::sync::mpsc;
        use std::thread;

        // Channel to signal completion or timeout
        let (tx, rx) = mpsc::channel();
        let child_pid = pid;

        // Spawn watchdog thread for timeout
        let timeout_duration = Duration::from_millis(timeout_ms);
        let kill_sig = kill_signal.clone().unwrap_or_else(|| "SIGTERM".to_string());

        thread::spawn(move || {
            thread::sleep(timeout_duration);
            // Send timeout signal - if receiver is still listening, kill the process
            let _ = tx.send(());
        });

        // Try to wait with a check for timeout
        // We need to poll child.try_wait() while checking for timeout signal
        let mut stdout_data = Vec::new();
        let mut stderr_data = Vec::new();

        // Take stdout/stderr handles for reading
        let mut stdout_handle = child.stdout.take();
        let mut stderr_handle = child.stderr.take();

        // Read stdout/stderr in separate threads to avoid blocking
        let (stdout_tx, stdout_rx) = mpsc::channel();
        let (stderr_tx, stderr_rx) = mpsc::channel();

        if let Some(mut stdout) = stdout_handle {
            thread::spawn(move || {
                use std::io::Read;
                let mut buf = Vec::new();
                let _ = stdout.read_to_end(&mut buf);
                let _ = stdout_tx.send(buf);
            });
        } else {
            let _ = stdout_tx.send(Vec::new());
        }

        if let Some(mut stderr) = stderr_handle {
            thread::spawn(move || {
                use std::io::Read;
                let mut buf = Vec::new();
                let _ = stderr.read_to_end(&mut buf);
                let _ = stderr_tx.send(buf);
            });
        } else {
            let _ = stderr_tx.send(Vec::new());
        }

        // Poll for completion or timeout
        let mut timed_out = false;
        loop {
            // Check if process has exited
            match child.try_wait() {
                Ok(Some(status)) => {
                    // Process exited normally
                    stdout_data = stdout_rx.recv().unwrap_or_default();
                    stderr_data = stderr_rx.recv().unwrap_or_default();

                    #[cfg(unix)]
                    let signal = {
                        use std::os::unix::process::ExitStatusExt;
                        status.signal().map(signal_name)
                    };
                    #[cfg(not(unix))]
                    let signal: Option<String> = None;

                    return SpawnSyncResult {
                        pid: Some(pid),
                        stdout: stdout_data,
                        stderr: stderr_data,
                        status: status.code(),
                        signal,
                        error: None,
                    };
                }
                Ok(None) => {
                    // Process still running - check for timeout
                    if rx.try_recv().is_ok() {
                        // Timeout reached - kill the process
                        timed_out = true;
                        #[cfg(unix)]
                        {
                            let sig = match kill_sig.as_str() {
                                "SIGKILL" | "9" => libc::SIGKILL,
                                "SIGINT" | "2" => libc::SIGINT,
                                _ => libc::SIGTERM,
                            };
                            unsafe {
                                libc::kill(child_pid as i32, sig);
                            }
                        }
                        #[cfg(not(unix))]
                        {
                            let _ = child.kill();
                        }
                        break;
                    }
                    // Sleep briefly before next poll
                    thread::sleep(Duration::from_millis(10));
                }
                Err(e) => {
                    return SpawnSyncResult {
                        pid: Some(pid),
                        stdout: vec![],
                        stderr: vec![],
                        status: None,
                        signal: None,
                        error: Some(e.to_string()),
                    };
                }
            }
        }

        // Wait for process to actually exit after kill
        let _ = child.wait();

        // Collect any output that was produced before timeout
        stdout_data = stdout_rx.recv_timeout(Duration::from_millis(100)).unwrap_or_default();
        stderr_data = stderr_rx.recv_timeout(Duration::from_millis(100)).unwrap_or_default();

        SpawnSyncResult {
            pid: Some(pid),
            stdout: stdout_data,
            stderr: stderr_data,
            status: None, // No exit code when killed by timeout
            signal: if timed_out {
                Some(kill_signal.unwrap_or_else(|| "SIGTERM".to_string()))
            } else {
                None
            },
            error: Some("ETIMEDOUT".to_string()),
        }
    }

    /// Write data to a process's stdin.
    ///
    /// Uses non-blocking send with backpressure. If the internal buffer is full,
    /// returns an error (the child process isn't consuming stdin fast enough).
    pub fn write_stdin(&self, id: u32, data: Vec<u8>) -> Result<(), ChildProcessError> {
        let processes = self.processes.lock();
        let handle = processes.get(&id).ok_or(ChildProcessError::NotFound(id))?;

        if !handle.running.load(Ordering::SeqCst) {
            return Err(ChildProcessError::AlreadyExited);
        }

        if let Some(ref tx) = handle.stdin_tx {
            tx.try_send(data).map_err(|e| match e {
                mpsc::error::TrySendError::Full(_) => {
                    ChildProcessError::IoError("stdin buffer full".to_string())
                }
                mpsc::error::TrySendError::Closed(_) => {
                    ChildProcessError::IoError("stdin closed".to_string())
                }
            })?;
        }

        Ok(())
    }

    /// Close a process's stdin.
    pub fn close_stdin(&self, id: u32) -> Result<(), ChildProcessError> {
        let mut processes = self.processes.lock();
        let handle = processes
            .get_mut(&id)
            .ok_or(ChildProcessError::NotFound(id))?;

        // Drop the sender to close stdin
        handle.stdin_tx = None;
        Ok(())
    }

    /// Send an IPC message to a child process (for fork).
    ///
    /// The message is JSON-serialized and sent over the IPC channel.
    pub fn send_message(
        &self,
        id: u32,
        message: serde_json::Value,
    ) -> Result<(), ChildProcessError> {
        let processes = self.processes.lock();
        let handle = processes.get(&id).ok_or(ChildProcessError::NotFound(id))?;

        if !handle.running.load(Ordering::SeqCst) {
            return Err(ChildProcessError::AlreadyExited);
        }

        if let Some(ref tx) = handle.ipc_tx {
            let json_str = serde_json::to_string(&message)
                .map_err(|e| ChildProcessError::IpcError(e.to_string()))?;
            tx.try_send(json_str.into_bytes()).map_err(|e| match e {
                mpsc::error::TrySendError::Full(_) => {
                    ChildProcessError::IpcError("IPC buffer full".to_string())
                }
                mpsc::error::TrySendError::Closed(_) => {
                    ChildProcessError::IpcError("IPC channel closed".to_string())
                }
            })?;
            Ok(())
        } else {
            Err(ChildProcessError::IpcNotEnabled)
        }
    }

    /// Kill a process.
    ///
    /// Returns `true` if the signal was sent successfully, `false` if the process
    /// has already exited or doesn't exist.
    pub fn kill(&self, id: u32, signal: Option<&str>) -> Result<bool, ChildProcessError> {
        let processes = self.processes.lock();
        let handle = processes.get(&id).ok_or(ChildProcessError::NotFound(id))?;

        if !handle.running.load(Ordering::SeqCst) {
            return Ok(false);
        }

        #[cfg(unix)]
        {
            if let Some(pid) = handle.pid {
                let sig = match signal {
                    Some("SIGKILL") | Some("9") => libc::SIGKILL,
                    Some("SIGINT") | Some("2") => libc::SIGINT,
                    Some("SIGHUP") | Some("1") => libc::SIGHUP,
                    Some("SIGQUIT") | Some("3") => libc::SIGQUIT,
                    Some("SIGUSR1") | Some("10") => libc::SIGUSR1,
                    Some("SIGUSR2") | Some("12") => libc::SIGUSR2,
                    Some("SIGCONT") | Some("18") => libc::SIGCONT,
                    Some("SIGSTOP") | Some("19") => libc::SIGSTOP,
                    _ => libc::SIGTERM, // Default to SIGTERM
                };

                // SAFETY: libc::kill is safe to call with valid pid and signal
                let result = unsafe { libc::kill(pid as i32, sig) };

                if result == 0 {
                    // Signal was sent successfully
                    handle.killed.store(true, Ordering::SeqCst);
                    return Ok(true);
                } else {
                    // Check errno for specific error
                    let err = std::io::Error::last_os_error();
                    if err.raw_os_error() == Some(libc::ESRCH) {
                        // Process doesn't exist (already exited)
                        return Ok(false);
                    } else if err.raw_os_error() == Some(libc::EPERM) {
                        // Permission denied
                        return Err(ChildProcessError::KillFailed(
                            "Permission denied".to_string(),
                        ));
                    }
                    return Err(ChildProcessError::KillFailed(err.to_string()));
                }
            }
            Ok(false)
        }

        #[cfg(windows)]
        {
            let _ = signal; // Signal is ignored on Windows, we always terminate
            if let Some(pid) = handle.pid {
                use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
                use windows_sys::Win32::System::Threading::{
                    OpenProcess, TerminateProcess, PROCESS_TERMINATE,
                };

                // SAFETY: Windows API calls with proper handle management
                unsafe {
                    let process_handle: HANDLE = OpenProcess(PROCESS_TERMINATE, 0, pid);
                    if process_handle == 0 {
                        // Failed to open process - likely already exited
                        return Ok(false);
                    }

                    let result = TerminateProcess(process_handle, 1);
                    CloseHandle(process_handle);

                    if result != 0 {
                        handle.killed.store(true, Ordering::SeqCst);
                        return Ok(true);
                    } else {
                        let err = std::io::Error::last_os_error();
                        return Err(ChildProcessError::KillFailed(err.to_string()));
                    }
                }
            }
            Ok(false)
        }
    }

    /// Get the OS PID of a process.
    pub fn pid(&self, id: u32) -> Option<u32> {
        self.processes.lock().get(&id).and_then(|h| h.pid)
    }

    /// Get the exit code of a process (if exited).
    pub fn exit_code(&self, id: u32) -> Option<i32> {
        self.processes
            .lock()
            .get(&id)
            .and_then(|h| *h.exit_code.lock())
    }

    /// Get the signal code of a process (if terminated by signal).
    pub fn signal_code(&self, id: u32) -> Option<String> {
        self.processes
            .lock()
            .get(&id)
            .and_then(|h| h.signal_code.lock().clone())
    }

    /// Check if a process is running.
    pub fn is_running(&self, id: u32) -> bool {
        self.processes
            .lock()
            .get(&id)
            .is_some_and(|h| h.running.load(Ordering::SeqCst))
    }

    /// Check if a process was killed.
    pub fn is_killed(&self, id: u32) -> bool {
        self.processes
            .lock()
            .get(&id)
            .is_some_and(|h| h.killed.load(Ordering::SeqCst))
    }

    /// Ref a process (keep event loop alive).
    pub fn ref_process(&self, id: u32) {
        if let Some(handle) = self.processes.lock().get(&id) {
            handle.ref_count.store(true, Ordering::SeqCst);
        }
    }

    /// Unref a process (don't keep event loop alive).
    pub fn unref_process(&self, id: u32) {
        if let Some(handle) = self.processes.lock().get(&id) {
            handle.ref_count.store(false, Ordering::SeqCst);
        }
    }

    /// Check if there are any ref'd running processes.
    pub fn has_active_refs(&self) -> bool {
        self.processes
            .lock()
            .values()
            .any(|h| h.running.load(Ordering::SeqCst) && h.ref_count.load(Ordering::SeqCst))
    }

    /// Poll for process events.
    pub fn poll_events(&self) -> Vec<(u32, ChildProcessEvent)> {
        let mut rx = self.event_rx.lock();
        let mut events = Vec::new();

        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }

        events
    }

    /// Get the number of active processes.
    pub fn active_count(&self) -> usize {
        self.processes
            .lock()
            .values()
            .filter(|h| h.running.load(Ordering::SeqCst))
            .count()
    }

    /// Clean up terminated processes.
    pub fn cleanup(&self) {
        let mut processes = self.processes.lock();
        processes.retain(|_, h| h.running.load(Ordering::SeqCst));
    }
}

impl Default for ChildProcessManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Channel capacity for process events (stdout/stderr data, exit, etc.)
/// With 8KB chunks, this allows ~8MB of buffered data per direction before backpressure.
const EVENT_CHANNEL_CAPACITY: usize = 1024;

/// Channel capacity for stdin writes.
/// Provides backpressure if JS writes faster than the child can consume.
const STDIN_CHANNEL_CAPACITY: usize = 64;

/// Convert a signal number to its name.
#[cfg(unix)]
fn signal_name(sig: i32) -> String {
    match sig {
        libc::SIGHUP => "SIGHUP".to_string(),
        libc::SIGINT => "SIGINT".to_string(),
        libc::SIGQUIT => "SIGQUIT".to_string(),
        libc::SIGILL => "SIGILL".to_string(),
        libc::SIGTRAP => "SIGTRAP".to_string(),
        libc::SIGABRT => "SIGABRT".to_string(),
        libc::SIGFPE => "SIGFPE".to_string(),
        libc::SIGKILL => "SIGKILL".to_string(),
        libc::SIGSEGV => "SIGSEGV".to_string(),
        libc::SIGPIPE => "SIGPIPE".to_string(),
        libc::SIGALRM => "SIGALRM".to_string(),
        libc::SIGTERM => "SIGTERM".to_string(),
        _ => format!("SIG{}", sig),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_manager_creation() {
        let manager = ChildProcessManager::new();
        assert_eq!(manager.active_count(), 0);
    }

    #[tokio::test]
    async fn test_spawn_echo() {
        let manager = ChildProcessManager::new();
        let id = manager
            .spawn(
                &["echo".to_string(), "hello".to_string()],
                SpawnOptions::default(),
            )
            .unwrap();

        assert!(id > 0);

        // Wait for process to complete
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let events = manager.poll_events();
        assert!(!events.is_empty());

        // Should have Spawn event
        assert!(
            events
                .iter()
                .any(|(_, e)| matches!(e, ChildProcessEvent::Spawn))
        );
    }

    #[tokio::test]
    async fn test_spawn_sync() {
        let manager = ChildProcessManager::new();
        let result = manager.spawn_sync(
            &["echo".to_string(), "hello".to_string()],
            SpawnOptions::default(),
        );

        assert!(result.error.is_none());
        assert_eq!(result.status, Some(0));
        assert!(String::from_utf8_lossy(&result.stdout).contains("hello"));
    }

    #[tokio::test]
    async fn test_spawn_with_shell() {
        let manager = ChildProcessManager::new();
        let result = manager.spawn_sync(
            &["echo hello && echo world".to_string()],
            SpawnOptions {
                shell: Some("/bin/sh".to_string()),
                ..Default::default()
            },
        );

        assert!(result.error.is_none());
        let output = String::from_utf8_lossy(&result.stdout);
        assert!(output.contains("hello"));
        assert!(output.contains("world"));
    }

    #[tokio::test]
    async fn test_kill_process() {
        let manager = ChildProcessManager::new();
        let id = manager
            .spawn(
                &["sleep".to_string(), "10".to_string()],
                SpawnOptions::default(),
            )
            .unwrap();

        assert!(manager.is_running(id));

        manager.kill(id, Some("SIGTERM")).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        assert!(!manager.is_running(id));
        assert!(manager.is_killed(id));
    }

    #[test]
    fn test_empty_command() {
        let manager = ChildProcessManager::new();
        let result = manager.spawn(&[], SpawnOptions::default());
        assert!(matches!(result, Err(ChildProcessError::InvalidArgument(_))));
    }
}
