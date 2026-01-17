//! IPC (Inter-Process Communication) support for Otter.
//!
//! Provides Unix socket-based communication between parent and child processes,
//! enabling `fork()` functionality and `process.send()`/`process.on('message')` API.
//!
//! # Protocol
//!
//! Messages are serialized as JSON with a 4-byte little-endian length prefix:
//! ```text
//! [len: u32 LE][payload: JSON bytes]
//! ```
//!
//! # Example
//!
//! ```javascript
//! // Parent
//! const child = fork('./worker.ts');
//! child.send({ task: 'compute' });
//! child.on('message', (msg) => console.log(msg));
//!
//! // Child (worker.ts)
//! process.on('message', (msg) => {
//!     process.send({ result: msg.task });
//! });
//! ```

use std::io;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[cfg(unix)]
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};

#[cfg(unix)]
use tokio::net::UnixStream;

/// IPC channel for bidirectional communication.
#[cfg(unix)]
pub struct IpcChannel {
    stream: UnixStream,
}

#[cfg(unix)]
impl IpcChannel {
    /// Create a pair of connected IPC channels.
    ///
    /// Returns `(parent_channel, child_fd)` where `child_fd` should be passed
    /// to the child process via environment variable `OTTER_IPC_FD`.
    pub fn create_pair() -> io::Result<(Self, RawFd)> {
        let (parent, child) = UnixStream::pair()?;
        let child_fd = child.as_raw_fd();

        // We need to keep the child socket alive until it's passed to the child process
        // by leaking it here. The child process will take ownership.
        std::mem::forget(child);

        Ok((Self { stream: parent }, child_fd))
    }

    /// Create an IPC channel from an existing file descriptor.
    ///
    /// This is used by child processes to connect to the parent.
    ///
    /// # Safety
    ///
    /// The file descriptor must be a valid, open Unix socket.
    pub unsafe fn from_raw_fd(fd: RawFd) -> io::Result<Self> {
        // SAFETY: Caller guarantees fd is a valid, open Unix socket
        let std_stream = unsafe { std::os::unix::net::UnixStream::from_raw_fd(fd) };
        std_stream.set_nonblocking(true)?;
        let stream = UnixStream::from_std(std_stream)?;
        Ok(Self { stream })
    }

    /// Send a JSON message through the IPC channel.
    pub async fn send(&mut self, msg: &serde_json::Value) -> io::Result<()> {
        let payload = serde_json::to_vec(msg)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

        // Write length prefix (4 bytes, little-endian)
        let len = (payload.len() as u32).to_le_bytes();
        self.stream.write_all(&len).await?;

        // Write payload
        self.stream.write_all(&payload).await?;
        self.stream.flush().await?;

        Ok(())
    }

    /// Receive a JSON message from the IPC channel.
    ///
    /// Returns `None` if the channel is closed.
    pub async fn recv(&mut self) -> io::Result<Option<serde_json::Value>> {
        // Read length prefix
        let mut len_buf = [0u8; 4];
        match self.stream.read_exact(&mut len_buf).await {
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e),
        }

        let len = u32::from_le_bytes(len_buf) as usize;

        // Sanity check on message size (max 64MB)
        if len > 64 * 1024 * 1024 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Message too large: {} bytes", len),
            ));
        }

        // Read payload
        let mut payload = vec![0u8; len];
        self.stream.read_exact(&mut payload).await?;

        // Parse JSON
        let msg = serde_json::from_slice(&payload)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

        Ok(Some(msg))
    }

    /// Try to receive a message without blocking.
    ///
    /// Returns `Ok(None)` if no message is available.
    pub fn try_recv(&mut self) -> io::Result<Option<serde_json::Value>> {
        // This is a synchronous non-blocking read
        // For async polling, use recv() with select!
        let std_stream = self.stream.as_raw_fd();

        // Check if data is available using poll
        unsafe {
            let mut pollfd = libc::pollfd {
                fd: std_stream,
                events: libc::POLLIN,
                revents: 0,
            };

            let result = libc::poll(&mut pollfd, 1, 0);
            if result <= 0 || (pollfd.revents & libc::POLLIN) == 0 {
                return Ok(None);
            }
        }

        // Data is available, but we need async context to read
        // This method is primarily for checking availability
        Ok(None)
    }

    /// Close the IPC channel.
    pub async fn close(self) -> io::Result<()> {
        drop(self.stream);
        Ok(())
    }

    /// Get the raw file descriptor.
    pub fn as_raw_fd(&self) -> RawFd {
        self.stream.as_raw_fd()
    }
}

/// Environment variable name for IPC file descriptor.
pub const IPC_FD_ENV: &str = "OTTER_IPC_FD";

/// Check if the current process was spawned with IPC support.
pub fn has_ipc() -> bool {
    std::env::var(IPC_FD_ENV).is_ok()
}

/// Get the IPC file descriptor from environment.
///
/// Returns `None` if not running as a forked child.
#[cfg(unix)]
pub fn get_ipc_fd() -> Option<RawFd> {
    std::env::var(IPC_FD_ENV)
        .ok()
        .and_then(|s| s.parse().ok())
}

/// Connect to the parent process IPC channel.
///
/// This should be called by child processes that were spawned with `ipc: true`.
/// Returns `None` if not running as a forked child.
#[cfg(unix)]
pub async fn connect_to_parent() -> io::Result<Option<IpcChannel>> {
    match get_ipc_fd() {
        Some(fd) => {
            let channel = unsafe { IpcChannel::from_raw_fd(fd)? };
            Ok(Some(channel))
        }
        None => Ok(None),
    }
}

/// IPC message wrapper for type safety.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct IpcMessage {
    /// Message type (for internal routing)
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub msg_type: Option<String>,
    /// Message payload
    #[serde(flatten)]
    pub data: serde_json::Value,
}

// Stub implementations for non-Unix platforms
#[cfg(not(unix))]
pub struct IpcChannel;

#[cfg(not(unix))]
impl IpcChannel {
    pub fn create_pair() -> io::Result<(Self, i32)> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "IPC is only supported on Unix platforms",
        ))
    }

    pub async fn send(&mut self, _msg: &serde_json::Value) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "IPC is only supported on Unix platforms",
        ))
    }

    pub async fn recv(&mut self) -> io::Result<Option<serde_json::Value>> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "IPC is only supported on Unix platforms",
        ))
    }
}

#[cfg(not(unix))]
pub fn get_ipc_fd() -> Option<i32> {
    None
}

#[cfg(not(unix))]
pub async fn connect_to_parent() -> io::Result<Option<IpcChannel>> {
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[cfg(unix)]
    async fn test_ipc_channel_pair() {
        let (mut parent, child_fd) = IpcChannel::create_pair().unwrap();

        // Create child channel from fd
        let mut child = unsafe { IpcChannel::from_raw_fd(child_fd).unwrap() };

        // Parent sends to child
        let msg = serde_json::json!({"hello": "world"});
        parent.send(&msg).await.unwrap();

        // Child receives
        let received = child.recv().await.unwrap().unwrap();
        assert_eq!(received, msg);

        // Child sends to parent
        let response = serde_json::json!({"response": 42});
        child.send(&response).await.unwrap();

        // Parent receives
        let received = parent.recv().await.unwrap().unwrap();
        assert_eq!(received, response);
    }

    #[test]
    fn test_ipc_fd_env() {
        assert!(!has_ipc());
    }
}
