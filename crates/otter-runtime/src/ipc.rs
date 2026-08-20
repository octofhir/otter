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

#[cfg(unix)]
use std::os::fd::RawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::{OtterError, RuntimeKeepAlive, RuntimeLiveness, RuntimeTask, RuntimeTaskSpawner};

/// Names the channel a launched process is to join. A process launched without
/// one, or one whose channel has already been joined, has no IPC members.
pub const CHANNEL_VAR: &str = "OTTER_CHANNEL";

/// What arrives on a channel.
pub enum IpcEvent {
    /// A message the peer sent, as the text the sender encoded, together
    /// with any open files it carried.
    Message(String, Vec<RawFd>),
    /// The peer is gone; nothing further will arrive.
    Closed,
    /// A message the program handed over has left this process, named by the
    /// token the program gave it. What the message carried is the peer's from
    /// here on, which is when the sender may let go of its own copy.
    Sent(u32),
}

impl IpcEvent {
    /// Take the open files off this event.
    ///
    /// A consumer that cannot report the message is the last holder of what it
    /// carried, and closes what it takes here.
    pub fn take_handles(&mut self) -> Vec<RawFd> {
        match self {
            Self::Message(_, handles) => std::mem::take(handles),
            Self::Closed | Self::Sent(_) => Vec::new(),
        }
    }
}

/// The open files one message arrived with, held until someone takes them.
///
/// Whatever is left when this goes away was never handed to anyone, and is
/// closed rather than leaked.
pub struct CarriedHandles(Vec<RawFd>);

impl CarriedHandles {
    /// Take ownership of what a message arrived with.
    #[must_use]
    pub fn new(handles: Vec<RawFd>) -> Self {
        Self(handles)
    }

    /// The one descriptor a message is allowed to carry. Anything past the
    /// first is not part of the protocol and is closed here.
    pub fn take_first(&mut self) -> Option<RawFd> {
        let mut handles = std::mem::take(&mut self.0).into_iter();
        let first = handles.next();
        for extra in handles {
            let _ = nix::unistd::close(extra);
        }
        first
    }
}

impl Drop for CarriedHandles {
    fn drop(&mut self) {
        for handle in std::mem::take(&mut self.0) {
            let _ = nix::unistd::close(handle);
        }
    }
}

/// A message on its way out, with the open files it carries.
#[cfg(unix)]
pub struct OutgoingFrame {
    bytes: Vec<u8>,
    handles: Vec<RawFd>,
    /// Held until the frame has left this process. A message the program
    /// handed to the channel is work in flight, and a program does not finish
    /// with unsent work — the socket takes only a buffer's worth at a time, so
    /// a large message needs turns of the loop that must still happen.
    in_flight: Option<RuntimeKeepAlive>,
    /// What the program calls this message, when it asked to be told that it
    /// has gone.
    sent: Option<u32>,
}

#[cfg(unix)]
impl OutgoingFrame {
    /// A frame carrying descriptors that never reached the socket still has to
    /// let go of them: they were duplicated for a crossing that did not
    /// happen, and nobody else holds a name for them.
    fn drop_handles(&mut self) {
        for handle in std::mem::take(&mut self.handles) {
            let _ = nix::unistd::close(handle);
        }
    }
}

#[cfg(unix)]
impl Drop for OutgoingFrame {
    fn drop(&mut self) {
        self.drop_handles();
    }
}

/// One end of a channel.
pub struct IpcChannel {
    outgoing: Mutex<Option<tokio::sync::mpsc::UnboundedSender<OutgoingFrame>>>,
    /// What a frame in flight holds the run loop open with.
    spawner: RuntimeTaskSpawner,
    /// Whether this end is pulling messages off the socket yet. A channel is
    /// read because someone is listening on it; until then what the peer sent
    /// waits in the socket, which is what lets a message sent to a process
    /// still starting up survive until its program is there to hear it.
    reading: Arc<ReadGate>,
    /// Shared with the task carrying the channel: whichever of the two ends it
    /// first — this side disconnecting, or the peer going away — releases the
    /// hold, so a process is never kept running by a channel nobody is on.
    keep_alive: Arc<Mutex<Option<RuntimeKeepAlive>>>,
    connected: Arc<AtomicBool>,
    address: Option<PathBuf>,
}

/// The signal a channel's reader waits on before it pulls anything.
struct ReadGate {
    open: AtomicBool,
    opened: tokio::sync::Notify,
}

impl ReadGate {
    fn new(open: bool) -> Self {
        Self {
            open: AtomicBool::new(open),
            opened: tokio::sync::Notify::new(),
        }
    }

    fn open(&self) {
        if !self.open.swap(true, Ordering::SeqCst) {
            // A stored permit, not a broadcast: the reader may not have parked
            // yet, and a missed wake is a channel that never reads again.
            self.opened.notify_one();
        }
    }

    async fn wait(&self) {
        while !self.open.load(Ordering::SeqCst) {
            self.opened.notified().await;
        }
    }
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

        let (channel, outgoing) = Self::new_gated(spawner, Some(address.clone()), true);
        let accept_gate = channel.reading.clone();
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
                accept_gate,
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

        let (channel, outgoing) = Self::new_gated(spawner, None, false);
        let connected = channel.connected.clone();
        let gate = channel.reading.clone();
        let keep_alive = channel.keep_alive.clone();
        let join_spawner = spawner.clone();
        io.spawn(async move {
            carry(
                stream,
                outgoing,
                connected,
                keep_alive,
                join_spawner,
                gate,
                deliver,
            )
            .await;
        });
        Ok(channel)
    }

    fn new_gated(
        spawner: &RuntimeTaskSpawner,
        address: Option<PathBuf>,
        reading: bool,
    ) -> (
        Arc<Self>,
        tokio::sync::mpsc::UnboundedReceiver<OutgoingFrame>,
    ) {
        let (outgoing, queued) = tokio::sync::mpsc::unbounded_channel::<OutgoingFrame>();
        // An open channel is a reason to keep running only while the program
        // is listening to it, which it says by referencing the channel. A
        // process that never asks for a message must be free to finish.
        let keep_alive = spawner.retain_keep_alive(RuntimeLiveness::Unref);
        let channel = Arc::new(Self {
            outgoing: Mutex::new(Some(outgoing)),
            spawner: spawner.clone(),
            reading: Arc::new(ReadGate::new(reading)),
            keep_alive: Arc::new(Mutex::new(Some(keep_alive))),
            connected: Arc::new(AtomicBool::new(true)),
            address,
        });
        (channel, queued)
    }

    /// Start pulling messages off this channel.
    ///
    /// A process joins the channel it was launched with before its program
    /// runs, so nothing is read until the program says it is listening.
    pub fn start_reading(&self) {
        self.reading.open();
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
        self.send_with_handles(payload, Vec::new(), None)
    }

    /// Send one message together with the open files it carries.
    ///
    /// The descriptors ride the same datagram as the message's first byte, so
    /// the peer can tell which message they belong to without a protocol of
    /// their own.
    #[must_use]
    pub fn send_with_handles(
        &self,
        payload: &str,
        handles: Vec<RawFd>,
        sent: Option<u32>,
    ) -> bool {
        // The descriptors belong to the frame from here on, so every way out
        // of this call closes them exactly once: the writer does it once they
        // have crossed, and dropping the frame does it if they never do.
        let mut frame = OutgoingFrame {
            bytes: Vec::new(),
            handles,
            in_flight: None,
            sent,
        };
        if !self.connected() {
            return false;
        }
        let Ok(length) = u32::try_from(payload.len()) else {
            return false;
        };
        frame.bytes.reserve(4 + payload.len());
        frame.bytes.extend_from_slice(&length.to_le_bytes());
        frame.bytes.extend_from_slice(payload.as_bytes());
        frame.in_flight = Some(self.spawner.retain_keep_alive(RuntimeLiveness::Ref));
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
    mut queued: tokio::sync::mpsc::UnboundedReceiver<OutgoingFrame>,
    connected: Arc<AtomicBool>,
    keep_alive: Arc<Mutex<Option<RuntimeKeepAlive>>>,
    spawner: RuntimeTaskSpawner,
    reading: Arc<ReadGate>,
    deliver: F,
) where
    F: Fn(IpcEvent) -> T + Send + Sync + 'static,
    T: RuntimeTask,
{
    let (reader, mut writer) = stream.into_split();
    // Both halves report what happens on this channel, so the closure that
    // names those reports is shared rather than owned by the reading side.
    let deliver = Arc::new(deliver);
    // Dropping the write half is what lets the peer observe the end of the
    // channel, so the writer task owns it and nothing else holds it.
    let writer_spawner = spawner.clone();
    let writer_deliver = deliver.clone();
    tokio::spawn(async move {
        while let Some(mut frame) = queued.recv().await {
            let outcome = write_frame(&mut writer, &frame.bytes, &frame.handles).await;
            // The peer has its own copies now; these were duplicated for the
            // crossing and are this side's to close.
            frame.drop_handles();
            // The hold this frame had on the run loop is let go of on the loop
            // itself: dropping it here would lower the count without waking
            // the thread that reads it, and a program with nothing left to do
            // would keep waiting to be told so.
            if let Some(in_flight) = frame.in_flight.take() {
                let _ = writer_spawner.enqueue(FrameWritten { in_flight }, RuntimeLiveness::Unref);
            }
            // The message has gone; what it carried belongs to the peer now,
            // and the sender is told so it can let go of its own copy.
            if let Some(token) = frame.sent.take() {
                let _ = writer_spawner
                    .enqueue(writer_deliver(IpcEvent::Sent(token)), RuntimeLiveness::Unref);
            }
            if outcome.is_err() {
                return;
            }
        }
    });
    read_loop(&reader, &spawner, &reading, deliver.as_ref()).await;
    connected.store(false, Ordering::SeqCst);
    // The peer is gone, so this end stops holding the run loop open whether or
    // not the program ever calls `disconnect` itself.
    release(&keep_alive);
    let _ = spawner.enqueue(deliver(IpcEvent::Closed), RuntimeLiveness::Unref);
}

/// A frame has left this process; running this on the loop is what lets go of
/// the hold it had.
struct FrameWritten {
    in_flight: RuntimeKeepAlive,
}

impl RuntimeTask for FrameWritten {
    fn run(self: Box<Self>, _runtime: &mut crate::Runtime) -> Result<(), OtterError> {
        drop(self.in_flight);
        Ok(())
    }
}

/// Read whole messages until the peer's end goes away.
#[cfg(unix)]
async fn read_loop<T, F>(
    stream: &tokio::net::unix::OwnedReadHalf,
    spawner: &RuntimeTaskSpawner,
    reading: &ReadGate,
    deliver: &F,
) where
    F: Fn(IpcEvent) -> T + Send + Sync + 'static,
    T: RuntimeTask,
{
    // Nothing leaves the socket before someone is there to hear it.
    reading.wait().await;
    let mut pending: Vec<u8> = Vec::new();
    let mut chunk = vec![0u8; 8192];
    // Descriptors arrive with the byte their sender attached them to, which
    // is the first byte of the message that carries them. Remembering where
    // in the stream that byte fell is what pairs them with that message and
    // no other.
    let mut arrived: std::collections::VecDeque<(u64, RawFd)> = std::collections::VecDeque::new();
    let mut consumed: u64 = 0;
    'reading: loop {
        if stream.readable().await.is_err() {
            break;
        }
        let read = match receive_with_fds(stream, &mut chunk) {
            Ok((0, _)) => break,
            Ok((length, handles)) => {
                let offset = consumed + pending.len() as u64;
                for handle in handles {
                    arrived.push_back((offset, handle));
                }
                length
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(_) => break,
        };
        pending.extend_from_slice(&chunk[..read]);
        while let Some((payload, frame_len)) = take_frame_sized(&mut pending) {
            let end = consumed + frame_len as u64;
            let mut handles = Vec::new();
            while arrived.front().is_some_and(|(offset, _)| *offset < end) {
                if let Some((_, handle)) = arrived.pop_front() {
                    handles.push(handle);
                }
            }
            consumed = end;
            // A message that cannot be reported is a message nobody will take
            // the descriptors off, so they are closed here instead.
            let carried = handles.clone();
            if spawner
                .enqueue(
                    deliver(IpcEvent::Message(payload, handles)),
                    RuntimeLiveness::Unref,
                )
                .is_err()
            {
                close_all(carried);
                break 'reading;
            }
        }
    }
    // Descriptors that arrived attached to a message the peer never finished
    // sending are this side's to close.
    close_all(arrived.into_iter().map(|(_, handle)| handle));
}

/// Close descriptors nothing will take ownership of.
#[cfg(unix)]
fn close_all(handles: impl IntoIterator<Item = RawFd>) {
    for handle in handles {
        let _ = nix::unistd::close(handle);
    }
}

/// Write one whole message, however many turns the socket needs to take it.
#[cfg(unix)]
async fn write_frame(
    stream: &mut tokio::net::unix::OwnedWriteHalf,
    frame: &[u8],
    handles: &[RawFd],
) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;

    if handles.is_empty() {
        return stream.write_all(frame).await;
    }
    // The descriptors go with the frame's first byte; the rest of the frame
    // follows as ordinary bytes on the same stream, so the peer pairs them
    // by position.
    loop {
        stream.writable().await?;
        match stream.as_ref().try_io(tokio::io::Interest::WRITABLE, || {
            send_with_fds(stream, &frame[..1], handles)
        }) {
            Ok(0) => return Err(std::io::ErrorKind::WriteZero.into()),
            Ok(_) => break,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(error) => return Err(error),
        }
    }
    stream.write_all(&frame[1..]).await
}

/// `sendmsg` with the descriptors in an `SCM_RIGHTS` control message.
#[cfg(unix)]
fn send_with_fds(
    stream: &tokio::net::unix::OwnedWriteHalf,
    bytes: &[u8],
    handles: &[RawFd],
) -> std::io::Result<usize> {
    use std::io::IoSlice;
    use std::os::fd::{AsFd, AsRawFd};

    let control = [nix::sys::socket::ControlMessage::ScmRights(handles)];
    let iov = [IoSlice::new(bytes)];
    nix::sys::socket::sendmsg::<()>(
        stream.as_ref().as_fd().as_raw_fd(),
        &iov,
        &control,
        nix::sys::socket::MsgFlags::empty(),
        None,
    )
    .map_err(std::io::Error::from)
}

/// `recvmsg` that also collects any descriptors the peer attached.
#[cfg(unix)]
fn receive_with_fds(
    stream: &tokio::net::unix::OwnedReadHalf,
    buffer: &mut [u8],
) -> std::io::Result<(usize, Vec<RawFd>)> {
    use std::io::IoSliceMut;
    use std::os::fd::{AsFd, AsRawFd};

    // The read goes through tokio's readiness bookkeeping, which a bare
    // `recvmsg` would bypass: an unconsumed readiness flag turns the loop
    // into a spin that starves every other task on the runtime.
    stream.as_ref().try_io(tokio::io::Interest::READABLE, || {
        let mut iov = [IoSliceMut::new(buffer)];
        let mut space = nix::cmsg_space!([RawFd; MAX_HANDLES_PER_MESSAGE]);
        let received = nix::sys::socket::recvmsg::<()>(
            stream.as_ref().as_fd().as_raw_fd(),
            &mut iov,
            Some(&mut space),
            nix::sys::socket::MsgFlags::empty(),
        )
        .map_err(std::io::Error::from)?;
        let mut handles = Vec::new();
        for message in received.cmsgs().map_err(std::io::Error::from)? {
            if let nix::sys::socket::ControlMessageOwned::ScmRights(fds) = message {
                handles.extend(fds);
            }
        }
        Ok((received.bytes, handles))
    })
}

/// The most descriptors one message may carry.
#[cfg(unix)]
const MAX_HANDLES_PER_MESSAGE: usize = 4;

/// Split off the first whole message, if the buffer holds one yet, and say
/// how many bytes it occupied.
///
/// A message is its byte length followed by its text, so a reader never has
/// to guess where one ends — which a delimiter would force it to do, and
/// which would then constrain what a message may contain. The size is what
/// pairs a message with the descriptors that arrived inside it.
fn take_frame_sized(buffer: &mut Vec<u8>) -> Option<(String, usize)> {
    if buffer.len() < 4 {
        return None;
    }
    let length = u32::from_le_bytes([buffer[0], buffer[1], buffer[2], buffer[3]]) as usize;
    if buffer.len() < 4 + length {
        return None;
    }
    let payload = String::from_utf8(buffer[4..4 + length].to_vec()).ok();
    buffer.drain(..4 + length);
    payload.map(|text| (text, 4 + length))
}

#[cfg(test)]
mod tests {
    use super::take_frame_sized;

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
            assert!(take_frame_sized(&mut buffer).is_none(), "split at {split}");
            assert_eq!(buffer.len(), split);
        }
    }

    #[test]
    fn messages_come_out_in_order_however_they_arrived() {
        let mut buffer = Vec::new();
        for payload in ["one", "", "three"] {
            buffer.extend_from_slice(&frame(payload));
        }
        assert_eq!(
            take_frame_sized(&mut buffer)
                .map(|(text, _)| text)
                .as_deref(),
            Some("one")
        );
        assert_eq!(
            take_frame_sized(&mut buffer)
                .map(|(text, _)| text)
                .as_deref(),
            Some("")
        );
        assert_eq!(
            take_frame_sized(&mut buffer)
                .map(|(text, _)| text)
                .as_deref(),
            Some("three")
        );
        assert!(take_frame_sized(&mut buffer).is_none());
        assert!(buffer.is_empty());
    }

    #[test]
    fn a_message_may_contain_anything_a_delimiter_would_have_claimed() {
        let payload = "{\"line\":\"a\\nb\",\"nul\":\"\\u0000\"}\n\n";
        let mut buffer = frame(payload);
        assert_eq!(
            take_frame_sized(&mut buffer)
                .map(|(text, _)| text)
                .as_deref(),
            Some(payload)
        );
        assert!(buffer.is_empty());
    }
}
