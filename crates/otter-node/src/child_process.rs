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
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use thiserror::Error;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

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

    #[error("Timeout exceeded")]
    Timeout,

    #[error("Permission denied: spawn not allowed")]
    PermissionDenied,

    #[error("Invalid argument: {0}")]
    InvalidArgument(String),
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
    stdin_tx: Option<mpsc::UnboundedSender<Vec<u8>>>,
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
    event_tx: mpsc::UnboundedSender<(u32, ChildProcessEvent)>,
    event_rx: Mutex<mpsc::UnboundedReceiver<(u32, ChildProcessEvent)>>,
}

impl ChildProcessManager {
    /// Create a new child process manager.
    pub fn new() -> Self {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
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

        // Spawn the process
        let mut child = cmd
            .spawn()
            .map_err(|e| ChildProcessError::SpawnFailed(e.to_string()))?;

        let pid = child.id();
        let running = Arc::new(AtomicBool::new(true));
        let exit_code = Arc::new(Mutex::new(None));
        let signal_code = Arc::new(Mutex::new(None));

        // Send spawn event
        let _ = self.event_tx.send((id, ChildProcessEvent::Spawn));

        // Setup stdin writer
        let stdin_tx = if options.stdin == StdioConfig::Pipe {
            let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
            if let Some(mut stdin) = child.stdin.take() {
                tokio::spawn(async move {
                    while let Some(data) = rx.recv().await {
                        if stdin.write_all(&data).await.is_err() {
                            break;
                        }
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

        // Setup stdout/stderr readers
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

                    let _ = event_tx.send((
                        id,
                        ChildProcessEvent::Exit {
                            code,
                            signal: signal.clone(),
                        },
                    ));
                    let _ = event_tx.send((id, ChildProcessEvent::Close { code, signal }));
                }
                Err(e) => {
                    let _ = event_tx.send((id, ChildProcessEvent::Error(e.to_string())));
                }
            }
        });

        // Store handle
        let handle = ChildProcessHandle {
            id,
            pid,
            stdin_tx,
            running,
            exit_code,
            signal_code,
            killed: AtomicBool::new(false),
            ref_count: AtomicBool::new(true),
        };

        self.processes.lock().insert(id, handle);

        Ok(id)
    }

    /// Helper to setup stdout/stderr readers
    fn setup_output_readers(
        &self,
        child: &mut Child,
        id: u32,
        stdout_config: StdioConfig,
        stderr_config: StdioConfig,
    ) {
        // Stdout reader
        if stdout_config == StdioConfig::Pipe {
            if let Some(stdout) = child.stdout.take() {
                let event_tx = self.event_tx.clone();
                tokio::spawn(async move {
                    let mut reader = BufReader::new(stdout);
                    let mut buf = vec![0u8; 8192];
                    loop {
                        use tokio::io::AsyncReadExt;
                        match reader.read(&mut buf).await {
                            Ok(0) => break,
                            Ok(n) => {
                                let _ = event_tx
                                    .send((id, ChildProcessEvent::Stdout(buf[..n].to_vec())));
                            }
                            Err(_) => break,
                        }
                    }
                });
            }
        }

        // Stderr reader
        if stderr_config == StdioConfig::Pipe {
            if let Some(stderr) = child.stderr.take() {
                let event_tx = self.event_tx.clone();
                tokio::spawn(async move {
                    let mut reader = BufReader::new(stderr);
                    let mut buf = vec![0u8; 8192];
                    loop {
                        use tokio::io::AsyncReadExt;
                        match reader.read(&mut buf).await {
                            Ok(0) => break,
                            Ok(n) => {
                                let _ = event_tx
                                    .send((id, ChildProcessEvent::Stderr(buf[..n].to_vec())));
                            }
                            Err(_) => break,
                        }
                    }
                });
            }
        }
    }

    /// Spawn a process synchronously and wait for completion.
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

        match cmd.output() {
            Ok(output) => {
                #[cfg(unix)]
                let signal = {
                    use std::os::unix::process::ExitStatusExt;
                    output.status.signal().map(signal_name)
                };
                #[cfg(not(unix))]
                let signal: Option<String> = None;

                SpawnSyncResult {
                    pid: None, // Not available for sync spawn
                    stdout: output.stdout,
                    stderr: output.stderr,
                    status: output.status.code(),
                    signal,
                    error: None,
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

    /// Write data to a process's stdin.
    pub fn write_stdin(&self, id: u32, data: Vec<u8>) -> Result<(), ChildProcessError> {
        let processes = self.processes.lock();
        let handle = processes.get(&id).ok_or(ChildProcessError::NotFound(id))?;

        if !handle.running.load(Ordering::SeqCst) {
            return Err(ChildProcessError::AlreadyExited);
        }

        if let Some(ref tx) = handle.stdin_tx {
            tx.send(data)
                .map_err(|e| ChildProcessError::IoError(e.to_string()))?;
        }

        Ok(())
    }

    /// Close a process's stdin.
    pub fn close_stdin(&self, id: u32) -> Result<(), ChildProcessError> {
        let mut processes = self.processes.lock();
        let handle = processes.get_mut(&id).ok_or(ChildProcessError::NotFound(id))?;

        // Drop the sender to close stdin
        handle.stdin_tx = None;
        Ok(())
    }

    /// Kill a process.
    pub fn kill(&self, id: u32, signal: Option<&str>) -> Result<bool, ChildProcessError> {
        let processes = self.processes.lock();
        let handle = processes.get(&id).ok_or(ChildProcessError::NotFound(id))?;

        if !handle.running.load(Ordering::SeqCst) {
            return Ok(false);
        }

        handle.killed.store(true, Ordering::SeqCst);

        #[cfg(unix)]
        if let Some(pid) = handle.pid {
            let sig = match signal {
                Some("SIGKILL") | Some("9") => libc::SIGKILL,
                Some("SIGINT") | Some("2") => libc::SIGINT,
                Some("SIGHUP") | Some("1") => libc::SIGHUP,
                Some("SIGQUIT") | Some("3") => libc::SIGQUIT,
                _ => libc::SIGTERM, // Default to SIGTERM
            };

            unsafe {
                libc::kill(pid as i32, sig);
            }
        }

        #[cfg(not(unix))]
        {
            // On Windows, we can only terminate
            let _ = signal; // Unused on Windows
        }

        Ok(true)
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
        self.processes.lock().values().any(|h| {
            h.running.load(Ordering::SeqCst) && h.ref_count.load(Ordering::SeqCst)
        })
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
            .spawn(&["echo".to_string(), "hello".to_string()], SpawnOptions::default())
            .unwrap();

        assert!(id > 0);

        // Wait for process to complete
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let events = manager.poll_events();
        assert!(!events.is_empty());

        // Should have Spawn event
        assert!(events.iter().any(|(_, e)| matches!(e, ChildProcessEvent::Spawn)));
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
            .spawn(&["sleep".to_string(), "10".to_string()], SpawnOptions::default())
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
