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
//! - One message is at most 16 MiB, and one isolate's IPC family retains at
//!   most 4,096 messages or 64 MiB across all channels and both directions.
//!   The finite family ledger and runtime ledger admit bytes before an owned
//!   payload allocation.
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

use otter_resource::{ResourceAccount, ResourceClass, ResourceLeaseSet, ResourceLimits};

use crate::{OtterError, RuntimeKeepAlive, RuntimeLiveness, RuntimeTask, RuntimeTaskSpawner};

const MAX_IPC_MESSAGE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_IPC_QUEUED_MESSAGES: u64 = 4_096;
const MAX_IPC_QUEUED_MESSAGE_BYTES: u64 = 64 * 1024 * 1024;

/// Names the channel a launched process is to join. A process launched without
/// one, or one whose channel has already been joined, has no IPC members.
pub const CHANNEL_VAR: &str = "OTTER_CHANNEL";

/// What arrives on a channel.
// Channel events are immediately embedded in a boxed runtime task. Boxing the
// message again would add one allocation without shrinking that owning task.
#[allow(clippy::large_enum_variant)]
pub enum IpcEvent {
    /// A message the peer sent, as the text the sender encoded, together
    /// with any open files it carried.
    Message(IpcMessage),
    /// The peer is gone; nothing further will arrive.
    Closed,
    /// A message the program handed over has left this process, named by the
    /// token the program gave it. What the message carried is the peer's from
    /// here on, which is when the sender may let go of its own copy.
    Sent(u32),
}

/// Why a synchronous handoff to the channel writer was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IpcSendError {
    /// The peer or local channel end is gone.
    Disconnected,
    /// The finite IPC family or runtime queue budget cannot admit the payload.
    Backpressure,
}

impl IpcEvent {
    /// Take the open files off this event.
    ///
    /// A consumer that cannot report the message is the last holder of what it
    /// carried, and closes what it takes here.
    pub fn take_handles(&mut self) -> Vec<RawFd> {
        match self {
            Self::Message(message) => message.take_handles(),
            Self::Closed | Self::Sent(_) => Vec::new(),
        }
    }
}

#[cfg(unix)]
impl Drop for IpcEvent {
    fn drop(&mut self) {
        // A queued runtime task owns the event until it runs. If shutdown or
        // inbox cancellation drops that task first, nobody gets a chance to
        // take the descriptors, so the event itself is their final owner.
        close_all(self.take_handles());
    }
}

/// One admitted incoming message.
///
/// The text, carried descriptors, and queue charges remain one ownership unit
/// until the isolate task consumes or drops the event.
pub struct IpcMessage {
    text: String,
    handles: Vec<RawFd>,
    _leases: IpcPayloadLeases,
}

impl IpcMessage {
    /// Borrow the encoded message text.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Move the encoded text out while the event retains its queue charges.
    pub fn take_text(&mut self) -> String {
        std::mem::take(&mut self.text)
    }

    fn take_handles(&mut self) -> Vec<RawFd> {
        std::mem::take(&mut self.handles)
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
    /// Runtime and per-family queue charges for `bytes`.
    payload_leases: Option<IpcPayloadLeases>,
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
    outgoing: Mutex<Option<tokio::sync::mpsc::Sender<OutgoingFrame>>>,
    payload_budget: IpcPayloadBudget,
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

/// Paired admission state for payloads retained by one isolate's IPC family.
#[derive(Clone)]
struct IpcPayloadBudget {
    runtime: ResourceAccount,
    family: ResourceAccount,
}

impl IpcPayloadBudget {
    fn standard(spawner: &RuntimeTaskSpawner) -> Self {
        Self {
            runtime: spawner.resource_account(),
            family: spawner.ipc_resource_account(),
        }
    }

    #[cfg(test)]
    fn with_limits(runtime: ResourceAccount, messages: u64, bytes: u64) -> Self {
        Self {
            runtime,
            family: ResourceAccount::new(
                ResourceLimits::builder()
                    .limit(ResourceClass::QueuedMessages, messages)
                    .limit(ResourceClass::QueuedMessageBytes, bytes)
                    .build(),
            ),
        }
    }

    fn admit(&self, bytes: usize) -> Result<IpcPayloadLeases, IpcPayloadError> {
        let bytes = u64::try_from(bytes).map_err(|_| IpcPayloadError::TooLarge)?;
        if bytes > MAX_IPC_MESSAGE_BYTES {
            return Err(IpcPayloadError::TooLarge);
        }
        let requests = [
            (ResourceClass::QueuedMessages, 1),
            (ResourceClass::QueuedMessageBytes, bytes),
        ];
        let family = self
            .family
            .reserve_exact_many(&requests)
            .map_err(|_| IpcPayloadError::FamilyBudget)?;
        let runtime = self
            .runtime
            .reserve_exact_many(&requests)
            .map_err(|_| IpcPayloadError::RuntimeBudget)?;
        Ok(IpcPayloadLeases {
            _family: family,
            _runtime: runtime,
        })
    }
}

/// Queue charges retained with one incoming or outgoing payload.
struct IpcPayloadLeases {
    _family: ResourceLeaseSet,
    _runtime: ResourceLeaseSet,
}

#[derive(Debug)]
enum IpcPayloadError {
    TooLarge,
    FamilyBudget,
    RuntimeBudget,
}

pub(crate) fn standard_ipc_resource_account() -> ResourceAccount {
    ResourceAccount::new(
        ResourceLimits::builder()
            .limit(ResourceClass::QueuedMessages, MAX_IPC_QUEUED_MESSAGES)
            .limit(
                ResourceClass::QueuedMessageBytes,
                MAX_IPC_QUEUED_MESSAGE_BYTES,
            )
            .build(),
    )
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
        let accept_budget = channel.payload_budget.clone();
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
                accept_spawner
                    .enqueue_ordered(deliver(IpcEvent::Closed), RuntimeLiveness::Unref)
                    .await;
                return;
            };
            carry(
                stream,
                outgoing,
                accept_connected,
                accept_keep_alive,
                accept_spawner,
                accept_gate,
                accept_budget,
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
        let budget = channel.payload_budget.clone();
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
                budget,
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
    ) -> (Arc<Self>, tokio::sync::mpsc::Receiver<OutgoingFrame>) {
        let (outgoing, queued) =
            tokio::sync::mpsc::channel::<OutgoingFrame>(MAX_IPC_QUEUED_MESSAGES as usize);
        // An open channel is a reason to keep running only while the program
        // is listening to it, which it says by referencing the channel. A
        // process that never asks for a message must be free to finish.
        let keep_alive = spawner.retain_keep_alive(RuntimeLiveness::Unref);
        let channel = Arc::new(Self {
            outgoing: Mutex::new(Some(outgoing)),
            payload_budget: IpcPayloadBudget::standard(spawner),
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

    /// Queue one message.
    ///
    /// # Errors
    /// Returns [`IpcSendError::Disconnected`] when the channel is closed and
    /// [`IpcSendError::Backpressure`] when the retained queue cannot admit the
    /// message.
    pub fn send(&self, payload: &str) -> Result<(), IpcSendError> {
        self.send_with_handles(payload, Vec::new(), None)
    }

    /// Send one message together with the open files it carries.
    ///
    /// The descriptors ride the same datagram as the message's first byte, so
    /// the peer can tell which message they belong to without a protocol of
    /// their own.
    ///
    /// # Errors
    /// Returns [`IpcSendError::Disconnected`] when the channel is closed and
    /// [`IpcSendError::Backpressure`] when the retained queue cannot admit the
    /// message. Refused descriptors are closed before returning.
    pub fn send_with_handles(
        &self,
        payload: &str,
        handles: Vec<RawFd>,
        sent: Option<u32>,
    ) -> Result<(), IpcSendError> {
        if !self.connected() {
            close_all(handles);
            return Err(IpcSendError::Disconnected);
        }
        let mut frame = prepare_outgoing_frame(payload, handles, sent, &self.payload_budget)?;
        frame.in_flight = Some(self.spawner.retain_keep_alive(RuntimeLiveness::Ref));
        let queue = self.outgoing.lock().unwrap_or_else(|p| p.into_inner());
        let Some(outgoing) = queue.as_ref() else {
            return Err(IpcSendError::Disconnected);
        };
        outgoing.try_send(frame).map_err(|error| match error {
            tokio::sync::mpsc::error::TrySendError::Full(_) => IpcSendError::Backpressure,
            tokio::sync::mpsc::error::TrySendError::Closed(_) => IpcSendError::Disconnected,
        })
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

/// Admit and copy one frame before it enters the writer queue.
fn prepare_outgoing_frame(
    payload: &str,
    handles: Vec<RawFd>,
    sent: Option<u32>,
    budget: &IpcPayloadBudget,
) -> Result<OutgoingFrame, IpcSendError> {
    // The descriptors belong to the frame from here on, so every way out
    // closes them exactly once: the writer after crossing, or Drop on refusal.
    let mut frame = OutgoingFrame {
        bytes: Vec::new(),
        handles,
        payload_leases: None,
        in_flight: None,
        sent,
    };
    let payload_leases = budget
        .admit(payload.len())
        .map_err(|_| IpcSendError::Backpressure)?;
    let length = u32::try_from(payload.len()).map_err(|_| IpcSendError::Backpressure)?;
    let frame_len = 4_usize
        .checked_add(payload.len())
        .ok_or(IpcSendError::Backpressure)?;
    frame
        .bytes
        .try_reserve_exact(frame_len)
        .map_err(|_| IpcSendError::Backpressure)?;
    frame.bytes.extend_from_slice(&length.to_le_bytes());
    frame.bytes.extend_from_slice(payload.as_bytes());
    frame.payload_leases = Some(payload_leases);
    Ok(frame)
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
    mut queued: tokio::sync::mpsc::Receiver<OutgoingFrame>,
    connected: Arc<AtomicBool>,
    keep_alive: Arc<Mutex<Option<RuntimeKeepAlive>>>,
    spawner: RuntimeTaskSpawner,
    reading: Arc<ReadGate>,
    payload_budget: IpcPayloadBudget,
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
            let in_flight = frame.in_flight.take();
            let sent = frame.sent.take();
            // The socket no longer borrows the bytes. Release their queue
            // charge before awaiting isolate delivery of completion events.
            drop(frame.payload_leases.take());
            drop(frame);
            // The hold this frame had on the run loop is let go of on the loop
            // itself: dropping it here would lower the count without waking
            // the thread that reads it, and a program with nothing left to do
            // would keep waiting to be told so.
            if let Some(in_flight) = in_flight {
                writer_spawner
                    .enqueue_ordered(FrameWritten { in_flight }, RuntimeLiveness::Unref)
                    .await;
            }
            // The message has gone; what it carried belongs to the peer now,
            // and the sender is told so it can let go of its own copy.
            if let Some(token) = sent {
                writer_spawner
                    .enqueue_ordered(
                        writer_deliver(IpcEvent::Sent(token)),
                        RuntimeLiveness::Unref,
                    )
                    .await;
            }
            if outcome.is_err() {
                return;
            }
        }
    });
    read_loop(
        &reader,
        &spawner,
        &reading,
        &payload_budget,
        deliver.as_ref(),
    )
    .await;
    connected.store(false, Ordering::SeqCst);
    // The peer is gone, so this end stops holding the run loop open whether or
    // not the program ever calls `disconnect` itself.
    release(&keep_alive);
    spawner
        .enqueue_ordered(deliver(IpcEvent::Closed), RuntimeLiveness::Unref)
        .await;
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
    payload_budget: &IpcPayloadBudget,
    deliver: &F,
) where
    F: Fn(IpcEvent) -> T + Send + Sync + 'static,
    T: RuntimeTask,
{
    // Nothing leaves the socket before someone is there to hear it.
    reading.wait().await;
    let mut decoder = IncomingFrameDecoder::default();
    let mut chunk = vec![0u8; 8192];
    // Descriptors arrive with the byte their sender attached them to, which
    // is the first byte of the message that carries them. Remembering where
    // in the stream that byte fell is what pairs them with that message and
    // no other.
    let mut arrived: std::collections::VecDeque<(u64, RawFd)> = std::collections::VecDeque::new();
    let mut received: u64 = 0;
    let mut decoded: u64 = 0;
    'reading: loop {
        if stream.readable().await.is_err() {
            break;
        }
        let read = match receive_with_fds(stream, &mut chunk) {
            Ok((0, _)) => break,
            Ok((length, handles)) => {
                let offset = received;
                let Some(next_received) = received.checked_add(length as u64) else {
                    close_all(handles);
                    break;
                };
                let mut handles = handles.into_iter();
                while let Some(handle) = handles.next() {
                    if arrived.len() == MAX_HANDLES_PER_MESSAGE {
                        let _ = nix::unistd::close(handle);
                        close_all(handles);
                        break 'reading;
                    }
                    arrived.push_back((offset, handle));
                }
                received = next_received;
                length
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(_) => break,
        };
        let mut cursor = 0;
        while cursor < read {
            let Ok((used, message)) = decoder.consume(&chunk[cursor..read], payload_budget) else {
                break 'reading;
            };
            cursor += used;
            decoded += used as u64;
            let Some(mut message) = message else {
                continue;
            };
            let mut handles = Vec::new();
            while arrived.front().is_some_and(|(offset, _)| *offset < decoded) {
                if let Some((_, handle)) = arrived.pop_front() {
                    handles.push(handle);
                }
            }
            message.handles = handles;
            // Room in the inbox is worth waiting for — a peer that sends
            // faster than the program reads must be made to wait, not go
            // unheard — so only an isolate that is gone ends the loop. The
            // event owns its descriptors, including when its queued task is
            // cancelled before delivery.
            if !spawner
                .enqueue_ordered(deliver(IpcEvent::Message(message)), RuntimeLiveness::Unref)
                .await
            {
                break 'reading;
            }
        }
    }
    // Descriptors that arrived attached to a message the peer never finished
    // sending are this side's to close.
    close_all(arrived.into_iter().map(|(_, handle)| handle));
}

/// Incremental decoder for the length-prefixed IPC wire format.
///
/// The four-byte header is fixed scratch. Once it is complete, admission and
/// exact capacity reservation happen before any payload byte is copied.
#[derive(Default)]
struct IncomingFrameDecoder {
    header: [u8; 4],
    header_len: usize,
    expected: Option<usize>,
    payload: Vec<u8>,
    leases: Option<IpcPayloadLeases>,
}

impl IncomingFrameDecoder {
    fn consume(
        &mut self,
        input: &[u8],
        budget: &IpcPayloadBudget,
    ) -> Result<(usize, Option<IpcMessage>), IncomingFrameError> {
        debug_assert!(!input.is_empty());
        let mut used = 0;
        if self.expected.is_none() {
            let header_bytes = (4 - self.header_len).min(input.len());
            self.header[self.header_len..self.header_len + header_bytes]
                .copy_from_slice(&input[..header_bytes]);
            self.header_len += header_bytes;
            used += header_bytes;
            if self.header_len != 4 {
                return Ok((used, None));
            }

            let expected = u32::from_le_bytes(self.header) as usize;
            let leases = budget.admit(expected).map_err(IncomingFrameError::from)?;
            self.payload
                .try_reserve_exact(expected)
                .map_err(|_| IncomingFrameError::Allocation)?;
            self.expected = Some(expected);
            self.leases = Some(leases);
            if expected == 0 {
                return self.finish(used);
            }
        }

        let expected = self.expected.expect("a complete header sets a length");
        let payload_bytes = (expected - self.payload.len()).min(input.len() - used);
        self.payload
            .extend_from_slice(&input[used..used + payload_bytes]);
        used += payload_bytes;
        if self.payload.len() == expected {
            self.finish(used)
        } else {
            Ok((used, None))
        }
    }

    fn finish(&mut self, used: usize) -> Result<(usize, Option<IpcMessage>), IncomingFrameError> {
        let payload = std::mem::take(&mut self.payload);
        let leases = self
            .leases
            .take()
            .expect("an admitted frame retains its leases");
        self.header_len = 0;
        self.expected = None;
        let text = String::from_utf8(payload).map_err(|_| IncomingFrameError::InvalidUtf8)?;
        Ok((
            used,
            Some(IpcMessage {
                text,
                handles: Vec::new(),
                _leases: leases,
            }),
        ))
    }
}

#[derive(Debug)]
enum IncomingFrameError {
    TooLarge,
    QueueBudget,
    Allocation,
    InvalidUtf8,
}

impl From<IpcPayloadError> for IncomingFrameError {
    fn from(error: IpcPayloadError) -> Self {
        match error {
            IpcPayloadError::TooLarge => Self::TooLarge,
            IpcPayloadError::FamilyBudget | IpcPayloadError::RuntimeBudget => Self::QueueBudget,
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    struct CaptureEvent {
        event: IpcEvent,
        sender: std::sync::mpsc::Sender<IpcEvent>,
    }

    impl RuntimeTask for CaptureEvent {
        fn run(self: Box<Self>, _runtime: &mut crate::Runtime) -> Result<(), OtterError> {
            self.sender
                .send(self.event)
                .map_err(|_| OtterError::Internal {
                    code: "IPC_TEST".to_string(),
                    message: "IPC test receiver dropped".to_string(),
                })
        }
    }

    fn frame(payload: &str) -> Vec<u8> {
        let mut bytes = u32::try_from(payload.len()).unwrap().to_le_bytes().to_vec();
        bytes.extend_from_slice(payload.as_bytes());
        bytes
    }

    fn current(account: &ResourceAccount, class: ResourceClass) -> u64 {
        account.snapshot().get(class).current()
    }

    #[test]
    fn payload_charges_both_ledgers_until_message_drop() {
        let runtime = ResourceAccount::default();
        let budget = IpcPayloadBudget::with_limits(runtime.clone(), 2, 8);
        let leases = budget.admit(3).expect("payload admitted");
        let message = IpcMessage {
            text: "abc".to_string(),
            handles: Vec::new(),
            _leases: leases,
        };

        assert_eq!(current(&runtime, ResourceClass::QueuedMessages), 1);
        assert_eq!(current(&runtime, ResourceClass::QueuedMessageBytes), 3);
        assert_eq!(
            current(&budget.family, ResourceClass::QueuedMessageBytes),
            3
        );

        drop(message);
        assert_eq!(current(&runtime, ResourceClass::QueuedMessages), 0);
        assert_eq!(current(&runtime, ResourceClass::QueuedMessageBytes), 0);
    }

    #[test]
    fn runtime_rejection_rolls_family_charge_back() {
        let runtime = ResourceAccount::new(
            ResourceLimits::builder()
                .limit(ResourceClass::QueuedMessageBytes, 2)
                .build(),
        );
        let budget = IpcPayloadBudget::with_limits(runtime.clone(), 2, 8);

        assert!(matches!(
            budget.admit(3),
            Err(IpcPayloadError::RuntimeBudget)
        ));
        assert_eq!(current(&runtime, ResourceClass::QueuedMessages), 0);
        assert_eq!(current(&budget.family, ResourceClass::QueuedMessages), 0);
    }

    #[test]
    fn outgoing_frame_pressure_rejects_before_copy_and_recovers() {
        let runtime = ResourceAccount::new(
            ResourceLimits::builder()
                .limit(ResourceClass::QueuedMessages, 1)
                .limit(ResourceClass::QueuedMessageBytes, 3)
                .build(),
        );
        let budget = IpcPayloadBudget::with_limits(runtime.clone(), 2, 8);

        let outgoing =
            prepare_outgoing_frame("abc", Vec::new(), None, &budget).expect("first frame admitted");
        assert_eq!(outgoing.bytes, frame("abc"));
        assert!(matches!(
            prepare_outgoing_frame("x", Vec::new(), None, &budget),
            Err(IpcSendError::Backpressure)
        ));
        assert_eq!(current(&runtime, ResourceClass::QueuedMessages), 1);

        drop(outgoing);
        assert_eq!(current(&runtime, ResourceClass::QueuedMessages), 0);
        drop(prepare_outgoing_frame("x", Vec::new(), None, &budget).expect("capacity recovered"));
        assert_eq!(current(&runtime, ResourceClass::QueuedMessages), 0);
    }

    #[test]
    fn oversized_header_is_rejected_before_payload_allocation() {
        let runtime = ResourceAccount::default();
        let budget = IpcPayloadBudget::with_limits(runtime.clone(), 4_096, 64 * 1024 * 1024);
        let mut decoder = IncomingFrameDecoder::default();
        let header = u32::try_from(MAX_IPC_MESSAGE_BYTES + 1)
            .unwrap()
            .to_le_bytes();

        assert!(matches!(
            decoder.consume(&header, &budget),
            Err(IncomingFrameError::TooLarge)
        ));
        assert_eq!(decoder.payload.capacity(), 0);
        assert_eq!(current(&runtime, ResourceClass::QueuedMessages), 0);
    }

    #[test]
    fn incremental_decoder_preserves_arbitrary_text_and_charges_it() {
        let runtime = ResourceAccount::default();
        let budget = IpcPayloadBudget::with_limits(runtime.clone(), 4_096, 64 * 1024 * 1024);
        let payload = "{\"line\":\"a\\nb\",\"nul\":\"\\u0000\"}\n\n";
        let bytes = frame(payload);
        let mut decoder = IncomingFrameDecoder::default();
        let mut message = None;
        for byte in bytes {
            let (used, decoded) = decoder.consume(&[byte], &budget).unwrap();
            assert_eq!(used, 1);
            if decoded.is_some() {
                message = decoded;
            }
        }
        let message = message.expect("complete message");
        assert_eq!(message.text(), payload);
        assert_eq!(
            current(&runtime, ResourceClass::QueuedMessageBytes),
            payload.len() as u64
        );
        drop(message);
        assert_eq!(current(&runtime, ResourceClass::QueuedMessageBytes), 0);
    }

    #[test]
    fn decoder_returns_multiple_frames_without_losing_remainder() {
        let runtime = ResourceAccount::default();
        let budget = IpcPayloadBudget::with_limits(runtime.clone(), 4, 16);
        let mut bytes = frame("one");
        bytes.extend_from_slice(&frame(""));
        bytes.extend_from_slice(&frame("three"));
        let mut decoder = IncomingFrameDecoder::default();
        let mut cursor = 0;
        let mut messages = Vec::new();

        while cursor < bytes.len() {
            let (used, message) = decoder.consume(&bytes[cursor..], &budget).unwrap();
            assert!(used > 0);
            cursor += used;
            if let Some(message) = message {
                messages.push(message);
            }
        }

        let texts: Vec<&str> = messages.iter().map(IpcMessage::text).collect();
        assert_eq!(texts, ["one", "", "three"]);
        assert_eq!(current(&runtime, ResourceClass::QueuedMessages), 3);
        drop(messages);
        assert_eq!(current(&runtime, ResourceClass::QueuedMessages), 0);
    }

    #[cfg(unix)]
    #[test]
    fn live_channel_holds_incoming_charge_until_event_drop() {
        let otter = crate::Otter::builder().build().unwrap();
        let spawner = otter.handle().task_spawner();
        let (parent_tx, _parent_rx) = std::sync::mpsc::channel();
        let (parent, address) = IpcChannel::listen(&spawner, move |event| CaptureEvent {
            event,
            sender: parent_tx.clone(),
        })
        .unwrap();
        let (child_tx, child_rx) = std::sync::mpsc::channel();
        let child = IpcChannel::join(&address, &spawner, move |event| CaptureEvent {
            event,
            sender: child_tx.clone(),
        })
        .unwrap();
        child.start_reading();

        parent.send("abc").expect("message accepted");
        let event = child_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("message delivered");
        let IpcEvent::Message(message) = &event else {
            panic!("expected a message event");
        };
        assert_eq!(message.text(), "abc");
        assert_eq!(
            current(&otter.resource_account(), ResourceClass::QueuedMessageBytes),
            3
        );

        drop(event);
        assert_eq!(
            current(&otter.resource_account(), ResourceClass::QueuedMessageBytes),
            0
        );
        parent.disconnect();
        child.disconnect();
    }

    #[cfg(unix)]
    #[test]
    fn dropping_a_message_closes_untaken_handles() {
        use std::io::Read;
        use std::os::fd::IntoRawFd;

        let (carried, mut peer) = std::os::unix::net::UnixStream::pair().unwrap();
        let raw = carried.into_raw_fd();
        let budget =
            IpcPayloadBudget::with_limits(ResourceAccount::default(), 4_096, 64 * 1024 * 1024);
        let message = IpcMessage {
            text: "message".to_string(),
            handles: vec![raw],
            _leases: budget.admit(7).unwrap(),
        };
        drop(IpcEvent::Message(message));

        let mut byte = [0_u8; 1];
        assert_eq!(peer.read(&mut byte).unwrap(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn taking_handles_transfers_them_out_of_the_event() {
        use std::os::fd::AsRawFd;

        let (read_end, write_end) = nix::unistd::pipe().unwrap();
        let raw = read_end.as_raw_fd();
        let budget =
            IpcPayloadBudget::with_limits(ResourceAccount::default(), 4_096, 64 * 1024 * 1024);
        let message = IpcMessage {
            text: "message".to_string(),
            handles: vec![raw],
            _leases: budget.admit(7).unwrap(),
        };
        let mut event = IpcEvent::Message(message);
        let handles = event.take_handles();
        assert_eq!(handles, vec![raw]);

        // `read_end` remains the test's safe owner while the event temporarily
        // carries its raw identity. A mistaken close in `Drop` would make the
        // read below fail with EBADF; taking the handle must leave it open.
        drop(event);
        assert_eq!(nix::unistd::write(&write_end, b"x").unwrap(), 1);
        let mut byte = [0_u8; 1];
        assert_eq!(nix::unistd::read(&read_end, &mut byte).unwrap(), 1);
        assert_eq!(byte, [b'x']);
    }
}
