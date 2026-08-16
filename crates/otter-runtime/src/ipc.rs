//! A message channel between two processes running this runtime.
//!
//! The channel is a runtime concept rather than a descriptor handed to
//! JavaScript: a parent opens one, launches a child that joins it, and both
//! ends then exchange whole messages. Neither side ever sees the transport.
//!
//! # Contents
//! - [`IpcChannel`] — one end of a channel: sends messages, delivers arriving
//!   ones onto the isolate thread, and reports when the peer goes away.
//! - [`IpcChannel::listen`] opens the end a parent keeps.
//! - [`IpcChannel::join`] opens the end a launched child was told to join.
//!
//! # Invariants
//! - Exactly one process can ever join a channel: the address is withdrawn as
//!   soon as the child connects, so a process the child launches in turn
//!   cannot rejoin its parent's channel even though it inherits the variable
//!   naming it.
//! - Sending never blocks the isolate thread: a message is queued and written
//!   by a task on the IO runtime, so ordering is the order of the sends. A
//!   message sent before the child joins is queued, not lost.
//! - Nothing but owned bytes crosses to the IO runtime; every re-entry into
//!   JavaScript happens through a [`RuntimeTask`] on the isolate thread.
//! - An open channel holds the run loop open, so a process waiting for a
//!   message does not exit first. Disconnecting releases that hold.
//!
//! # See also
//! - [`crate::process_ipc`] — the `process` members a joined child gets.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::{OtterError, RuntimeKeepAlive, RuntimeLiveness, RuntimeTask, RuntimeTaskSpawner};

/// Names the channel a launched process is to join. A process launched without
/// one, or one whose channel has already been joined, has no IPC members.
pub const CHANNEL_VAR: &str = "OTTER_CHANNEL";

/// What arrives on a channel.
pub enum IpcEvent {
    /// A message the peer sent, as the text the sender encoded.
    Message(String),
    /// The peer is gone; nothing further will arrive.
    Closed,
}

/// One end of a channel.
pub struct IpcChannel {
    outgoing: Mutex<Option<tokio::sync::mpsc::UnboundedSender<Vec<u8>>>>,
    /// Shared with the task carrying the channel: whichever of the two ends it
    /// first — this side disconnecting, or the peer going away — releases the
    /// hold, so a process is never kept running by a channel nobody is on.
    keep_alive: Arc<Mutex<Option<RuntimeKeepAlive>>>,
    connected: Arc<AtomicBool>,
    address: Option<PathBuf>,
}

fn host_error(message: impl Into<String>) -> OtterError {
    OtterError::Internal {
        code: "IPC_CHANNEL".to_string(),
        message: message.into(),
    }
}

/// Where a new channel listens. The name is unique to this launch, so two
/// children of the same process never collide.
#[cfg(unix)]
fn fresh_address() -> PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos());
    std::env::temp_dir().join(format!("otter-{}-{stamp:x}.ipc", std::process::id()))
}

impl IpcChannel {
    /// Open the end a parent keeps, and answer the address the child it
    /// launches must be told to join.
    ///
    /// The channel is usable at once: messages sent before the child joins are
    /// queued and delivered when it does.
    ///
    /// # Errors
    /// Returns [`OtterError`] when the runtime has no IO runtime to carry the
    /// channel, or when the address cannot be opened.
    #[cfg(unix)]
    pub fn listen<T, F>(
        spawner: &RuntimeTaskSpawner,
        deliver: F,
    ) -> Result<(Arc<Self>, PathBuf), OtterError>
    where
        F: Fn(IpcEvent) -> T + Send + Sync + 'static,
        T: RuntimeTask,
    {
        let io = spawner
            .io_handle()
            .ok_or_else(|| host_error("an IPC channel needs the host's IO runtime"))?;
        let address = fresh_address();
        let listener = std::os::unix::net::UnixListener::bind(&address)
            .map_err(|error| host_error(format!("IPC channel: {error}")))?;
        listener
            .set_nonblocking(true)
            .map_err(|error| host_error(format!("IPC channel: {error}")))?;
        // Binding registers with the reactor and needs the runtime in scope;
        // entering is how a thread already inside it gets that.
        let listener = {
            let _guard = io.enter();
            tokio::net::UnixListener::from_std(listener)
                .map_err(|error| host_error(format!("IPC channel: {error}")))?
        };

        let (channel, outgoing) = Self::new(spawner, Some(address.clone()));
        let accept_address = address.clone();
        let accept_connected = channel.connected.clone();
        let accept_keep_alive = channel.keep_alive.clone();
        let accept_spawner = spawner.clone();
        io.spawn(async move {
            let joined = listener.accept().await;
            // One process joins a channel, and the address is withdrawn the
            // moment it does.
            let _ = std::fs::remove_file(&accept_address);
            let Ok((stream, _)) = joined else {
                accept_connected.store(false, Ordering::SeqCst);
                release(&accept_keep_alive);
                let _ = accept_spawner.enqueue(deliver(IpcEvent::Closed), RuntimeLiveness::Unref);
                return;
            };
            carry(
                stream,
                outgoing,
                accept_connected,
                accept_keep_alive,
                accept_spawner,
                deliver,
            )
            .await;
        });
        Ok((channel, address))
    }

    /// Join the channel this process was launched to join, if it was launched
    /// with one and the address is still open.
    ///
    /// # Errors
    /// Returns [`OtterError`] when the runtime has no IO runtime to carry the
    /// channel.
    #[cfg(unix)]
    pub fn join<T, F>(
        address: &Path,
        spawner: &RuntimeTaskSpawner,
        deliver: F,
    ) -> Result<Arc<Self>, OtterError>
    where
        F: Fn(IpcEvent) -> T + Send + Sync + 'static,
        T: RuntimeTask,
    {
        let io = spawner
            .io_handle()
            .ok_or_else(|| host_error("an IPC channel needs the host's IO runtime"))?;
        let stream = std::os::unix::net::UnixStream::connect(address)
            .map_err(|error| host_error(format!("IPC channel: {error}")))?;
        stream
            .set_nonblocking(true)
            .map_err(|error| host_error(format!("IPC channel: {error}")))?;
        let stream = {
            let _guard = io.enter();
            tokio::net::UnixStream::from_std(stream)
                .map_err(|error| host_error(format!("IPC channel: {error}")))?
        };

        let (channel, outgoing) = Self::new(spawner, None);
        let connected = channel.connected.clone();
        let keep_alive = channel.keep_alive.clone();
        let join_spawner = spawner.clone();
        io.spawn(async move {
            carry(
                stream,
                outgoing,
                connected,
                keep_alive,
                join_spawner,
                deliver,
            )
            .await;
        });
        Ok(channel)
    }

    fn new(
        spawner: &RuntimeTaskSpawner,
        address: Option<PathBuf>,
    ) -> (Arc<Self>, tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>) {
        let (outgoing, queued) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        // An open channel is a reason to keep running only while the program
        // is listening to it, which it says by referencing the channel. A
        // process that never asks for a message must be free to finish.
        let keep_alive = spawner.retain_keep_alive(RuntimeLiveness::Unref);
        let channel = Arc::new(Self {
            outgoing: Mutex::new(Some(outgoing)),
            keep_alive: Arc::new(Mutex::new(Some(keep_alive))),
            connected: Arc::new(AtomicBool::new(true)),
            address,
        });
        (channel, queued)
    }

    /// Whether the peer is still reachable.
    #[must_use]
    pub fn connected(&self) -> bool {
        self.connected.load(Ordering::SeqCst)
    }

    /// Say whether waiting on this channel should keep the process running.
    ///
    /// A program that listens for messages is waiting for one, and a program
    /// that does not is not — so this follows the listeners rather than the
    /// channel simply being open.
    pub fn set_referenced(&self, referenced: bool) {
        if let Some(keep_alive) = self
            .keep_alive
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
        {
            if referenced {
                keep_alive.ref_();
            } else {
                keep_alive.unref();
            }
        }
    }

    /// Queue one message. Answers whether it was accepted; a disconnected
    /// channel accepts nothing.
    pub fn send(&self, payload: &str) -> bool {
        if !self.connected() {
            return false;
        }
        let Ok(length) = u32::try_from(payload.len()) else {
            return false;
        };
        let mut frame = Vec::with_capacity(4 + payload.len());
        frame.extend_from_slice(&length.to_le_bytes());
        frame.extend_from_slice(payload.as_bytes());
        let queue = self.outgoing.lock().unwrap_or_else(|p| p.into_inner());
        queue
            .as_ref()
            .is_some_and(|outgoing| outgoing.send(frame).is_ok())
    }

    /// Close this end. Messages already queued are still written, then the peer
    /// observes the end of the channel.
    pub fn disconnect(&self) {
        self.connected.store(false, Ordering::SeqCst);
        self.outgoing
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        release(&self.keep_alive);
    }
}

/// Let go of the hold this channel has on the run loop. Idempotent: the local
/// side and the task watching the peer both call it, and only one can be first.
fn release(keep_alive: &Arc<Mutex<Option<RuntimeKeepAlive>>>) {
    keep_alive.lock().unwrap_or_else(|p| p.into_inner()).take();
}

impl Drop for IpcChannel {
    fn drop(&mut self) {
        // A parent that never saw its child join still owns the address.
        if let Some(address) = &self.address {
            let _ = std::fs::remove_file(address);
        }
    }
}

/// Carry messages both ways over a joined channel until either end goes away.
#[cfg(unix)]
async fn carry<T, F>(
    stream: tokio::net::UnixStream,
    mut queued: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
    connected: Arc<AtomicBool>,
    keep_alive: Arc<Mutex<Option<RuntimeKeepAlive>>>,
    spawner: RuntimeTaskSpawner,
    deliver: F,
) where
    F: Fn(IpcEvent) -> T + Send + Sync + 'static,
    T: RuntimeTask,
{
    let (reader, mut writer) = stream.into_split();
    // Dropping the write half is what lets the peer observe the end of the
    // channel, so the writer task owns it and nothing else holds it.
    tokio::spawn(async move {
        while let Some(frame) = queued.recv().await {
            if write_frame(&mut writer, &frame).await.is_err() {
                return;
            }
        }
    });
    read_loop(&reader, &spawner, &deliver).await;
    connected.store(false, Ordering::SeqCst);
    // The peer is gone, so this end stops holding the run loop open whether or
    // not the program ever calls `disconnect` itself.
    release(&keep_alive);
    let _ = spawner.enqueue(deliver(IpcEvent::Closed), RuntimeLiveness::Unref);
}

/// Read whole messages until the peer's end goes away.
#[cfg(unix)]
async fn read_loop<T, F>(
    stream: &tokio::net::unix::OwnedReadHalf,
    spawner: &RuntimeTaskSpawner,
    deliver: &F,
) where
    F: Fn(IpcEvent) -> T + Send + Sync + 'static,
    T: RuntimeTask,
{
    let mut pending: Vec<u8> = Vec::new();
    let mut chunk = vec![0u8; 8192];
    loop {
        if stream.readable().await.is_err() {
            break;
        }
        match stream.try_read(&mut chunk) {
            Ok(0) => break,
            Ok(length) => pending.extend_from_slice(&chunk[..length]),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(_) => break,
        }
        while let Some(payload) = take_frame(&mut pending) {
            if spawner
                .enqueue(deliver(IpcEvent::Message(payload)), RuntimeLiveness::Unref)
                .is_err()
            {
                return;
            }
        }
    }
}

/// Write one whole message, however many turns the socket needs to take it.
#[cfg(unix)]
async fn write_frame(
    stream: &mut tokio::net::unix::OwnedWriteHalf,
    frame: &[u8],
) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;

    stream.write_all(frame).await
}

/// Split off the first whole message, if the buffer holds one yet.
///
/// A message is its byte length followed by its text, so a reader never has to
/// guess where one ends — which a delimiter would force it to do, and which
/// would then constrain what a message may contain.
fn take_frame(buffer: &mut Vec<u8>) -> Option<String> {
    if buffer.len() < 4 {
        return None;
    }
    let length = u32::from_le_bytes([buffer[0], buffer[1], buffer[2], buffer[3]]) as usize;
    if buffer.len() < 4 + length {
        return None;
    }
    let payload = String::from_utf8(buffer[4..4 + length].to_vec()).ok();
    buffer.drain(..4 + length);
    payload
}

#[cfg(test)]
mod tests {
    use super::take_frame;

    fn frame(payload: &str) -> Vec<u8> {
        let mut bytes = u32::try_from(payload.len()).unwrap().to_le_bytes().to_vec();
        bytes.extend_from_slice(payload.as_bytes());
        bytes
    }

    #[test]
    fn a_partial_message_is_left_in_the_buffer() {
        let whole = frame("hello");
        for split in 0..whole.len() {
            let mut buffer = whole[..split].to_vec();
            assert!(take_frame(&mut buffer).is_none(), "split at {split}");
            assert_eq!(buffer.len(), split);
        }
    }

    #[test]
    fn messages_come_out_in_order_however_they_arrived() {
        let mut buffer = Vec::new();
        for payload in ["one", "", "three"] {
            buffer.extend_from_slice(&frame(payload));
        }
        assert_eq!(take_frame(&mut buffer).as_deref(), Some("one"));
        assert_eq!(take_frame(&mut buffer).as_deref(), Some(""));
        assert_eq!(take_frame(&mut buffer).as_deref(), Some("three"));
        assert!(take_frame(&mut buffer).is_none());
        assert!(buffer.is_empty());
    }

    #[test]
    fn a_message_may_contain_anything_a_delimiter_would_have_claimed() {
        let payload = "{\"line\":\"a\\nb\",\"nul\":\"\\u0000\"}\n\n";
        let mut buffer = frame(payload);
        assert_eq!(take_frame(&mut buffer).as_deref(), Some(payload));
        assert!(buffer.is_empty());
    }
}
