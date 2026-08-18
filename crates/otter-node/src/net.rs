//! Native transport under the vendored `net` stack: TCP and Unix-domain
//! servers and connections.
//!
//! # Contents
//! - [`net_binding_cjs_value`] exports the raw dial/listen/byte surface as
//!   `internal/otter/net` for the compat `tcp_wrap`/`pipe_wrap` handles.
//! - A table of listeners and connections owned by the natives, keyed by the
//!   handle id the wraps hold.
//! - An accept loop per listener and a read loop per connection, both on the
//!   host's IO runtime, delivering onto the isolate thread.
//!
//! # Invariants
//! - Listening and connecting are network operations and are gated by the
//!   `net` capability, checked against the address involved.
//! - The loops never touch VM state; they hand owned bytes to a task that
//!   re-enters JavaScript on the isolate thread.
//! - Writing never blocks the isolate thread: bytes are queued and written by
//!   a task, so ordering is the order of the writes.
//! - A listener holds the runtime open while it is listening, and a connection
//!   while it is open, so a program serving or awaiting data does not exit.
//!
//! # See also
//! - `nodelib/compat/internal_otter_stream_handle.js`

use std::collections::HashMap;
use std::net::ToSocketAddrs;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use otter_runtime::{
    CapabilitySet, OtterError, Runtime, RuntimeExecutionContext, RuntimeKeepAlive, RuntimeLiveness,
    RuntimeLocal, RuntimeNativeCtx, RuntimeNativeError, RuntimeNativeScope, RuntimeTask,
    RuntimeTaskSpawner, RuntimeValue, runtime_arg_to_string, runtime_type_error,
};

/// One live listener or connection.
struct Entry {
    kind: EntryKind,
    keep_alive: Option<RuntimeKeepAlive>,
    local: Option<std::net::SocketAddr>,
    remote: Option<std::net::SocketAddr>,
}

enum EntryKind {
    Listener {
        /// Wakes the accept task so the listening socket closes the moment
        /// `close` is called, not at the next accept wakeup — a connect
        /// racing a just-closed server must be refused, never accepted by
        /// the kernel backlog and then reset.
        shutdown: Arc<tokio::sync::Notify>,
    },
    Connection {
        outgoing: Option<tokio::sync::mpsc::UnboundedSender<WriteMsg>>,
        socket: NetSocket,
        /// Wakes the read loop when the entry is closed or reset, so the
        /// task drops its stream clone and the kernel closes the socket
        /// immediately instead of at the peer's next transition.
        abort: Arc<tokio::sync::Notify>,
        /// Messages handed to the writer task and not yet completed. A
        /// direct `tryWrite` is only allowed while this is zero, or bytes
        /// would overtake the queue.
        queued: Arc<std::sync::atomic::AtomicUsize>,
    },
}

/// A carried connection: TCP or a Unix domain socket. Both sides of the I/O
/// contract (readiness + non-blocking try ops) are identical, so the loops
/// run over this enum instead of a concrete stream type.
#[derive(Clone)]
enum NetSocket {
    Tcp(Arc<tokio::net::TcpStream>),
    Unix(Arc<tokio::net::UnixStream>),
}

impl NetSocket {
    async fn readable(&self) -> std::io::Result<()> {
        match self {
            Self::Tcp(stream) => stream.readable().await,
            Self::Unix(stream) => stream.readable().await,
        }
    }

    fn try_read(&self, buffer: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::Tcp(stream) => stream.try_read(buffer),
            Self::Unix(stream) => stream.try_read(buffer),
        }
    }

    async fn writable(&self) -> std::io::Result<()> {
        match self {
            Self::Tcp(stream) => stream.writable().await,
            Self::Unix(stream) => stream.writable().await,
        }
    }

    fn try_write(&self, bytes: &[u8]) -> std::io::Result<usize> {
        match self {
            Self::Tcp(stream) => stream.try_write(bytes),
            Self::Unix(stream) => stream.try_write(bytes),
        }
    }

    fn local_addr(&self) -> Option<std::net::SocketAddr> {
        match self {
            Self::Tcp(stream) => stream.local_addr().ok(),
            // Unix sockets have no IP address; `socket.address()` answers
            // `{}` for them, exactly as Node's does.
            Self::Unix(_) => None,
        }
    }

    fn set_nodelay(&self, flag: bool) -> std::io::Result<()> {
        match self {
            Self::Tcp(stream) => stream.set_nodelay(flag),
            Self::Unix(_) => Ok(()),
        }
    }

    fn set_ttl(&self, ttl: u32) -> std::io::Result<()> {
        match self {
            Self::Tcp(stream) => stream.set_ttl(ttl),
            Self::Unix(_) => Ok(()),
        }
    }

    /// Arm an immediate RST on close (SO_LINGER 0). Unix sockets have no
    /// reset semantics; closing them is already abrupt. A zero linger never
    /// blocks the closing thread — the kernel discards the send queue and
    /// resets at once.
    fn arm_reset(&self) {
        if let Self::Tcp(_) = self {
            let linger = libc::linger {
                l_onoff: 1,
                l_linger: 0,
            };
            // SAFETY: the fd is owned by the live stream for the duration of
            // this call, and `linger` is a valid, initialized option payload
            // of the size passed.
            unsafe {
                libc::setsockopt(
                    self.raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_LINGER,
                    std::ptr::from_ref(&linger).cast(),
                    std::mem::size_of::<libc::linger>() as libc::socklen_t,
                );
            }
        }
    }

    fn raw_fd(&self) -> std::os::fd::RawFd {
        use std::os::fd::AsRawFd;
        match self {
            Self::Tcp(stream) => stream.as_raw_fd(),
            Self::Unix(stream) => stream.as_raw_fd(),
        }
    }
}

/// One queued item for a connection's writer task.
enum WriteMsg {
    /// Bytes to send; a non-zero token requests a completion event.
    Data(Vec<u8>, u32),
    /// Flush everything queued before this marker, then close the write
    /// half and report completion for the token.
    End(u32),
}

type Table = Arc<Mutex<HashMap<u32, Entry>>>;

/// Deliver one event to the isolate without dropping or reordering it.
/// See [`RuntimeTaskSpawner::enqueue_ordered`].
async fn enqueue_ordered(
    spawner: &RuntimeTaskSpawner,
    event: NetEvent,
    liveness: RuntimeLiveness,
) -> bool {
    spawner.enqueue_ordered(event, liveness).await
}

/// Build the CommonJS export of `internal/otter/net` — the raw dial,
/// listen, and byte-transport surface the compat `tcp_wrap`/`pipe_wrap`
/// handle classes drive. Vendored `net` never touches this directly.
///
/// # Errors
/// Returns a native error when the binding object fails to allocate.
pub fn net_binding_cjs_value<'scope>(
    scope: &mut RuntimeNativeScope<'scope, '_>,
    capabilities: &CapabilitySet,
    runtime_task_spawner: Option<RuntimeTaskSpawner>,
    _module: RuntimeLocal<'scope>,
    _require: RuntimeLocal<'scope>,
) -> Result<RuntimeLocal<'scope>, RuntimeNativeError> {
    build_native(scope, capabilities, runtime_task_spawner)
}

fn build_native<'scope>(
    scope: &mut RuntimeNativeScope<'scope, '_>,
    capabilities: &CapabilitySet,
    spawner: Option<RuntimeTaskSpawner>,
) -> Result<RuntimeLocal<'scope>, RuntimeNativeError> {
    let object = scope.object()?;
    let table: Table = Arc::new(Mutex::new(HashMap::new()));
    let next_id = Arc::new(AtomicU32::new(1));

    let listen_caps = capabilities.clone();
    let listen_table = table.clone();
    let listen_ids = next_id.clone();
    let listen_spawner = spawner.clone();
    let listen = scope.native_closure(
        "listen",
        3,
        &[],
        move |ctx: &mut RuntimeNativeCtx<'_>, args: &[RuntimeValue], _c: &[RuntimeValue]| {
            listen(
                ctx,
                args,
                &listen_caps,
                &listen_table,
                &listen_ids,
                listen_spawner.as_ref(),
            )
        },
    )?;
    scope.set(object, "listen", listen)?;

    let connect_caps = capabilities.clone();
    let connect_table = table.clone();
    let connect_ids = next_id.clone();
    let connect_spawner = spawner.clone();
    let connect = scope.native_closure(
        "connect",
        3,
        &[],
        move |ctx: &mut RuntimeNativeCtx<'_>, args: &[RuntimeValue], _c: &[RuntimeValue]| {
            connect(
                ctx,
                args,
                &connect_caps,
                &connect_table,
                &connect_ids,
                connect_spawner.as_ref(),
            )
        },
    )?;
    scope.set(object, "connect", connect)?;

    let listen_path_caps = capabilities.clone();
    let listen_path_table = table.clone();
    let listen_path_ids = next_id.clone();
    let listen_path_spawner = spawner.clone();
    let listen_path = scope.native_closure(
        "listenPath",
        1,
        &[],
        move |ctx: &mut RuntimeNativeCtx<'_>, args: &[RuntimeValue], _c: &[RuntimeValue]| {
            listen_unix(
                ctx,
                args,
                &listen_path_caps,
                &listen_path_table,
                &listen_path_ids,
                listen_path_spawner.as_ref(),
            )
        },
    )?;
    scope.set(object, "listenPath", listen_path)?;

    let connect_path_caps = capabilities.clone();
    let connect_path_table = table.clone();
    let connect_path_ids = next_id.clone();
    let connect_path_spawner = spawner.clone();
    let connect_path = scope.native_closure(
        "connectPath",
        2,
        &[],
        move |ctx: &mut RuntimeNativeCtx<'_>, args: &[RuntimeValue], _c: &[RuntimeValue]| {
            connect_unix(
                ctx,
                args,
                &connect_path_caps,
                &connect_path_table,
                &connect_path_ids,
                connect_path_spawner.as_ref(),
            )
        },
    )?;
    scope.set(object, "connectPath", connect_path)?;

    let write_table = table.clone();
    let write = scope.native_closure(
        "write",
        2,
        &[],
        move |ctx: &mut RuntimeNativeCtx<'_>, args: &[RuntimeValue], _c: &[RuntimeValue]| {
            let id = handle_arg(args, 0);
            let token = args.get(2).and_then(|v| v.as_f64()).unwrap_or(0.0) as u32;
            // A typed-array payload crosses as raw bytes; only string
            // payloads take the latin1 detour.
            if let Some(view) = args.get(1).and_then(|v| v.as_typed_array(ctx.heap())) {
                let heap = ctx.heap();
                let offset = view.byte_offset(heap);
                let len = view.byte_length(heap);
                let sent = view.buffer(heap).with_bytes(heap, |bytes| {
                    write_bytes(&write_table, id, &bytes[offset..offset + len], token)
                });
                return Ok(RuntimeValue::boolean(sent));
            }
            let payload = runtime_arg_to_string(args, 1, ctx.heap());
            Ok(RuntimeValue::boolean(write_bytes(
                &write_table,
                id,
                &latin1_to_bytes(&payload),
                token,
            )))
        },
    )?;
    scope.set(object, "write", write)?;

    let try_write_table = table.clone();
    let try_write = scope.native_closure(
        "tryWrite",
        2,
        &[],
        move |ctx: &mut RuntimeNativeCtx<'_>, args: &[RuntimeValue], _c: &[RuntimeValue]| {
            let id = handle_arg(args, 0);
            let written = if let Some(view) = args.get(1).and_then(|v| v.as_typed_array(ctx.heap()))
            {
                let heap = ctx.heap();
                let offset = view.byte_offset(heap);
                let len = view.byte_length(heap);
                view.buffer(heap).with_bytes(heap, |bytes| {
                    try_write_bytes(&try_write_table, id, &bytes[offset..offset + len])
                })
            } else {
                let payload = runtime_arg_to_string(args, 1, ctx.heap());
                try_write_bytes(&try_write_table, id, &latin1_to_bytes(&payload))
            };
            Ok(RuntimeValue::number(
                otter_vm::number::NumberValue::from_f64(written as f64),
            ))
        },
    )?;
    scope.set(object, "tryWrite", try_write)?;

    let close_table = table.clone();
    let close = scope.native_closure(
        "close",
        1,
        &[],
        move |_ctx: &mut RuntimeNativeCtx<'_>, args: &[RuntimeValue], _c: &[RuntimeValue]| {
            close_entry(&close_table, handle_arg(args, 0));
            Ok(RuntimeValue::undefined())
        },
    )?;
    scope.set(object, "close", close)?;

    // Half-close: the peer is told there is nothing further to read, while
    // this side stays open to whatever it still has to say.
    let end_table = table.clone();
    let end = scope.native_closure(
        "end",
        1,
        &[],
        move |_ctx: &mut RuntimeNativeCtx<'_>, args: &[RuntimeValue], _c: &[RuntimeValue]| {
            let id = handle_arg(args, 0);
            let token = args.get(1).and_then(|v| v.as_f64()).unwrap_or(0.0) as u32;
            let mut table = end_table
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(entry) = table.get_mut(&id)
                && let EntryKind::Connection {
                    outgoing, queued, ..
                } = &mut entry.kind
                && let Some(sender) = outgoing.take()
            {
                queued.fetch_add(1, Ordering::SeqCst);
                if sender.send(WriteMsg::End(token)).is_err() {
                    queued.fetch_sub(1, Ordering::SeqCst);
                }
            }
            Ok(RuntimeValue::undefined())
        },
    )?;
    scope.set(object, "end", end)?;

    let address_table = table.clone();
    let address = scope.native_closure(
        "address",
        2,
        &[],
        move |ctx: &mut RuntimeNativeCtx<'_>, args: &[RuntimeValue], _c: &[RuntimeValue]| {
            let id = handle_arg(args, 0);
            let want_remote = string_arg(ctx, args, 1).as_deref() == Some("remote");
            let found = {
                let table = address_table
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                table.get(&id).and_then(|entry| {
                    if want_remote {
                        entry.remote
                    } else {
                        entry.local
                    }
                })
            };
            let Some(found) = found else {
                return Ok(RuntimeValue::undefined());
            };
            ctx.scope(|mut scope| {
                let result = address_object(&mut scope, found)?;
                Ok(scope.finish(result))
            })
        },
    )?;
    scope.set(object, "address", address)?;

    let option_table = table.clone();
    let option = scope.native_closure(
        "setOption",
        3,
        &[],
        move |ctx: &mut RuntimeNativeCtx<'_>, args: &[RuntimeValue], _c: &[RuntimeValue]| {
            set_option(ctx, args, &option_table)
        },
    )?;
    scope.set(object, "setOption", option)?;

    // A program may say it is not waiting on a connection, the way it can for
    // a timer, without closing it.
    let hold_table = table.clone();
    let hold = scope.native_closure(
        "hold",
        2,
        &[],
        move |_ctx: &mut RuntimeNativeCtx<'_>, args: &[RuntimeValue], _c: &[RuntimeValue]| {
            let id = handle_arg(args, 0);
            let referenced = args
                .get(1)
                .and_then(|value| value.as_boolean())
                .unwrap_or(true);
            let table = hold_table
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(entry) = table.get(&id)
                && let Some(keep_alive) = &entry.keep_alive
            {
                if referenced {
                    keep_alive.ref_();
                } else {
                    keep_alive.unref();
                }
            }
            Ok(RuntimeValue::undefined())
        },
    )?;
    scope.set(object, "hold", hold)?;

    let reset_table = table.clone();
    let reset = scope.native_closure(
        "reset",
        1,
        &[],
        move |_ctx: &mut RuntimeNativeCtx<'_>, args: &[RuntimeValue], _c: &[RuntimeValue]| {
            let id = handle_arg(args, 0);
            {
                let table = reset_table
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if let Some(Entry {
                    kind: EntryKind::Connection { socket, .. },
                    ..
                }) = table.get(&id)
                {
                    socket.arm_reset();
                }
            }
            close_entry(&reset_table, id);
            Ok(RuntimeValue::undefined())
        },
    )?;
    scope.set(object, "reset", reset)?;
    Ok(object)
}

/// Socket options a connection carries. Each is applied through the platform,
/// so a value it refuses is refused here too.
fn set_option(
    ctx: &mut RuntimeNativeCtx<'_>,
    args: &[RuntimeValue],
    table: &Table,
) -> Result<RuntimeValue, RuntimeNativeError> {
    let id = handle_arg(args, 0);
    let name = string_arg(ctx, args, 1).unwrap_or_default();
    let flag = args
        .get(2)
        .and_then(|value| value.as_boolean())
        .unwrap_or(false);
    let number = args.get(2).and_then(|value| value.as_f64()).unwrap_or(0.0);

    let socket = {
        let table = table
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match table.get(&id).map(|entry| &entry.kind) {
            Some(EntryKind::Connection { socket, .. }) => Some(socket.clone()),
            _ => None,
        }
    };
    let Some(socket) = socket else {
        return Ok(RuntimeValue::undefined());
    };
    let outcome = match name.as_str() {
        "setNoDelay" => socket.set_nodelay(flag),
        "setTTL" => socket.set_ttl(number as u32),
        _ => Ok(()),
    };
    outcome.map_err(|error| system_error(&error, "setsockopt", ""))?;
    Ok(RuntimeValue::undefined())
}

/// Start listening, and deliver each connection as it arrives.
fn listen(
    ctx: &mut RuntimeNativeCtx<'_>,
    args: &[RuntimeValue],
    capabilities: &CapabilitySet,
    table: &Table,
    next_id: &Arc<AtomicU32>,
    spawner: Option<&RuntimeTaskSpawner>,
) -> Result<RuntimeValue, RuntimeNativeError> {
    let host = string_arg(ctx, args, 0).unwrap_or_default();
    let port = args.get(1).and_then(|value| value.as_f64()).unwrap_or(0.0) as u16;
    let host = if host.is_empty() {
        "0.0.0.0".to_string()
    } else {
        host
    };
    if !capabilities.net.matches(&host) {
        return Err(runtime_type_error(
            "net.listen",
            format!("permission denied for '{host}'"),
        ));
    }
    let spawner = io_spawner(spawner, "net.listen")?;
    let io = spawner
        .io_handle()
        .ok_or_else(|| runtime_type_error("net.listen", "no IO runtime".to_string()))?;

    let target = resolve(&host, port).ok_or_else(|| system_error_code("ENOTFOUND", "listen"))?;
    let listener = std::net::TcpListener::bind(target)
        .and_then(|listener| {
            listener.set_nonblocking(true)?;
            Ok(listener)
        })
        .map_err(|error| system_error(&error, "listen", &host))?;
    let local = listener
        .local_addr()
        .map_err(|error| system_error(&error, "listen", &host))?;
    // Binding registers with the reactor and needs the runtime in scope.
    let listener = {
        let _guard = io.enter();
        tokio::net::TcpListener::from_std(listener)
            .map_err(|error| system_error(&error, "listen", &host))?
    };

    let id = next_id.fetch_add(1, Ordering::Relaxed);
    let keep_alive = spawner.retain_keep_alive(RuntimeLiveness::Ref);
    let shutdown = Arc::new(tokio::sync::Notify::new());
    table
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(
            id,
            Entry {
                kind: EntryKind::Listener {
                    shutdown: shutdown.clone(),
                },
                keep_alive: Some(keep_alive),
                local: Some(local),
                remote: None,
            },
        );

    let accept_table = table.clone();
    let accept_ids = next_id.clone();
    let accept_spawner = spawner.clone();
    io.spawn(async move {
        loop {
            let accepted = tokio::select! {
                accepted = listener.accept() => accepted,
                () = shutdown.notified() => return,
            };
            let still_listening = accept_table
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .contains_key(&id);
            if !still_listening {
                return;
            }
            let Ok((stream, remote)) = accepted else {
                return;
            };
            let connection = adopt(
                NetSocket::Tcp(Arc::new(stream)),
                Some(remote),
                &accept_table,
                &accept_ids,
                &accept_spawner,
            );
            if !enqueue_ordered(
                &accept_spawner,
                NetEvent::Accepted {
                    server: id,
                    connection,
                    remote: Some(remote),
                },
                RuntimeLiveness::Unref,
            )
            .await
            {
                return;
            }
        }
    });

    ctx.scope(|mut scope| {
        let result = scope.object()?;
        let handle = scope.number(f64::from(id));
        scope.set(result, "handle", handle)?;
        let address = address_object(&mut scope, local)?;
        scope.set(result, "address", address)?;
        Ok(scope.finish(result))
    })
}

/// Take ownership of a connected stream and start carrying it.
fn adopt(
    stream: NetSocket,
    remote: Option<std::net::SocketAddr>,
    table: &Table,
    next_id: &Arc<AtomicU32>,
    spawner: &RuntimeTaskSpawner,
) -> u32 {
    let id = next_id.fetch_add(1, Ordering::Relaxed);
    let local = stream.local_addr();
    let (outgoing, mut queued) = tokio::sync::mpsc::unbounded_channel::<WriteMsg>();
    let keep_alive = spawner.retain_keep_alive(RuntimeLiveness::Ref);
    let abort = Arc::new(tokio::sync::Notify::new());
    let queued_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    table
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(
            id,
            Entry {
                kind: EntryKind::Connection {
                    outgoing: Some(outgoing),
                    socket: stream.clone(),
                    abort: abort.clone(),
                    queued: queued_count.clone(),
                },
                keep_alive: Some(keep_alive),
                local,
                remote,
            },
        );

    let writer_stream = stream.clone();
    let writer_spawner = spawner.clone();
    let writer_queued = queued_count;
    tokio::spawn(async move {
        while let Some(message) = queued.recv().await {
            let _decrement = scopeguard_decrement(&writer_queued);
            match message {
                WriteMsg::Data(bytes, token) => {
                    if let Err(error) = write_all(&writer_stream, &bytes).await {
                        enqueue_ordered(
                            &writer_spawner,
                            NetEvent::WriteDone {
                                connection: id,
                                token,
                                code: Some(io_code(&error)),
                            },
                            RuntimeLiveness::Ref,
                        )
                        .await;
                        return;
                    }
                    if token != 0
                        && !enqueue_ordered(
                            &writer_spawner,
                            NetEvent::WriteDone {
                                connection: id,
                                token,
                                code: None,
                            },
                            RuntimeLiveness::Ref,
                        )
                        .await
                    {
                        return;
                    }
                }
                WriteMsg::End(token) => {
                    // Everything queued ahead of the marker has been written;
                    // close the write half and report the shutdown.
                    shutdown_write(&writer_stream);
                    enqueue_ordered(
                        &writer_spawner,
                        NetEvent::ShutdownDone {
                            connection: id,
                            token,
                        },
                        RuntimeLiveness::Ref,
                    )
                    .await;
                    return;
                }
            }
        }
        // The sender dropped without an End marker (hard close); the write
        // half closes so the peer reads EOF.
        shutdown_write(&writer_stream);
    });

    let reader_spawner = spawner.clone();
    let reader_table = table.clone();
    tokio::spawn(async move {
        let mut chunk = vec![0u8; 65_536];
        let mut read_error: Option<&'static str> = None;
        loop {
            let ready = tokio::select! {
                ready = stream.readable() => ready,
                () = abort.notified() => {
                    // Closed or reset from the isolate side: drop the stream
                    // clone without reporting anything — the JS handle is
                    // already gone.
                    return;
                }
            };
            if ready.is_err() {
                break;
            }
            match stream.try_read(&mut chunk) {
                Ok(0) => break,
                Ok(length) => {
                    if !enqueue_ordered(
                        &reader_spawner,
                        NetEvent::Data {
                            connection: id,
                            payload: bytes_to_latin1(&chunk[..length]),
                        },
                        RuntimeLiveness::Unref,
                    )
                    .await
                    {
                        return;
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => continue,
                Err(error) => {
                    read_error = Some(io_code(&error));
                    break;
                }
            }
        }
        // The peer has nothing further to send (or the read failed). The
        // connection stays open for this side to finish writing, so only its
        // hold on the loop is released here. The final notification rides a
        // Ref-class task: the hold is already gone, and an Unref event would
        // be dropped if nothing else kept the loop alive.
        release(&reader_table, id);
        let event = match read_error {
            Some(code) => NetEvent::ReadFailed {
                connection: id,
                code,
            },
            None => NetEvent::Ended { connection: id },
        };
        enqueue_ordered(&reader_spawner, event, RuntimeLiveness::Ref).await;
    });
    id
}

/// Bind a Unix domain socket and deliver each connection as it arrives.
/// Mirrors [`listen`], with the socket path standing in for host+port.
fn listen_unix(
    ctx: &mut RuntimeNativeCtx<'_>,
    args: &[RuntimeValue],
    capabilities: &CapabilitySet,
    table: &Table,
    next_id: &Arc<AtomicU32>,
    spawner: Option<&RuntimeTaskSpawner>,
) -> Result<RuntimeValue, RuntimeNativeError> {
    let path = string_arg(ctx, args, 0).unwrap_or_default();
    if path.is_empty() {
        return Err(runtime_type_error("net.listen", "missing socket path".to_string()));
    }
    if !capabilities.net.matches(&path) {
        return Err(runtime_type_error(
            "net.listen",
            format!("permission denied for '{path}'"),
        ));
    }
    let spawner = io_spawner(spawner, "net.listen")?;
    let io = spawner
        .io_handle()
        .ok_or_else(|| runtime_type_error("net.listen", "no IO runtime".to_string()))?;

    // Binding registers with the reactor and needs the runtime in scope.
    let listener = {
        let _guard = io.enter();
        tokio::net::UnixListener::bind(&path).map_err(|error| system_error(&error, "listen", &path))?
    };

    let id = next_id.fetch_add(1, Ordering::Relaxed);
    let keep_alive = spawner.retain_keep_alive(RuntimeLiveness::Ref);
    let shutdown = Arc::new(tokio::sync::Notify::new());
    table
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(
            id,
            Entry {
                kind: EntryKind::Listener {
                    shutdown: shutdown.clone(),
                },
                keep_alive: Some(keep_alive),
                local: None,
                remote: None,
            },
        );

    let accept_table = table.clone();
    let accept_ids = next_id.clone();
    let accept_spawner = spawner.clone();
    io.spawn(async move {
        loop {
            let accepted = tokio::select! {
                accepted = listener.accept() => accepted,
                () = shutdown.notified() => return,
            };
            let still_listening = accept_table
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .contains_key(&id);
            if !still_listening {
                return;
            }
            let Ok((stream, _remote)) = accepted else {
                return;
            };
            let connection = adopt(
                NetSocket::Unix(Arc::new(stream)),
                None,
                &accept_table,
                &accept_ids,
                &accept_spawner,
            );
            if accept_spawner
                .enqueue(
                    NetEvent::Accepted {
                        server: id,
                        connection,
                        remote: None,
                    },
                    RuntimeLiveness::Unref,
                )
                .is_err()
            {
                return;
            }
        }
    });

    ctx.scope(|mut scope| {
        let result = scope.object()?;
        let handle = scope.number(f64::from(id));
        scope.set(result, "handle", handle)?;
        Ok(scope.finish(result))
    })
}

/// Connect to a Unix domain socket. Mirrors [`connect`].
fn connect_unix(
    ctx: &mut RuntimeNativeCtx<'_>,
    args: &[RuntimeValue],
    capabilities: &CapabilitySet,
    table: &Table,
    next_id: &Arc<AtomicU32>,
    spawner: Option<&RuntimeTaskSpawner>,
) -> Result<RuntimeValue, RuntimeNativeError> {
    let path = string_arg(ctx, args, 0).unwrap_or_default();
    let token = handle_arg(args, 1);
    if path.is_empty() {
        return Err(runtime_type_error("net.connect", "missing socket path".to_string()));
    }
    if !capabilities.net.matches(&path) {
        return Err(runtime_type_error(
            "net.connect",
            format!("permission denied for '{path}'"),
        ));
    }
    let spawner = io_spawner(spawner, "net.connect")?;
    let io = spawner
        .io_handle()
        .ok_or_else(|| runtime_type_error("net.connect", "no IO runtime".to_string()))?;

    let connect_table = table.clone();
    let connect_ids = next_id.clone();
    let connect_spawner = spawner.clone();
    let attempt = spawner.retain_keep_alive(RuntimeLiveness::Ref);
    io.spawn(async move {
        let _attempt = attempt;
        match tokio::net::UnixStream::connect(&path).await {
            Ok(stream) => {
                let connection = adopt(
                    NetSocket::Unix(Arc::new(stream)),
                    None,
                    &connect_table,
                    &connect_ids,
                    &connect_spawner,
                );
                enqueue_ordered(
                    &connect_spawner,
                    NetEvent::Connected { token, connection },
                    RuntimeLiveness::Unref,
                    )
                    .await;
            }
            Err(error) => {
                let code = io_code(&error);
                enqueue_ordered(
                    &connect_spawner,
                    NetEvent::ConnectFailed {
                        token,
                        code,
                        message: format!("connect {code} {path}"),
                    },
                    RuntimeLiveness::Unref,
                    )
                    .await;
            }
        }
    });
    Ok(RuntimeValue::undefined())
}

/// Open a connection, and report whether it was made.
fn connect(
    ctx: &mut RuntimeNativeCtx<'_>,
    args: &[RuntimeValue],
    capabilities: &CapabilitySet,
    table: &Table,
    next_id: &Arc<AtomicU32>,
    spawner: Option<&RuntimeTaskSpawner>,
) -> Result<RuntimeValue, RuntimeNativeError> {
    let host = string_arg(ctx, args, 0).unwrap_or_default();
    let port = args.get(1).and_then(|value| value.as_f64()).unwrap_or(0.0) as u16;
    let token = handle_arg(args, 2);
    let host = if host.is_empty() {
        "127.0.0.1".to_string()
    } else {
        host
    };
    if !capabilities.net.matches(&host) {
        return Err(runtime_type_error(
            "net.connect",
            format!("permission denied for '{host}'"),
        ));
    }
    let spawner = io_spawner(spawner, "net.connect")?;
    let io = spawner
        .io_handle()
        .ok_or_else(|| runtime_type_error("net.connect", "no IO runtime".to_string()))?;

    let connect_table = table.clone();
    let connect_ids = next_id.clone();
    let connect_spawner = spawner.clone();
    // A connection being made is work the program is waiting on, so it holds
    // the run loop until it is either established or refused. Without this a
    // program whose only work is one connection exits before hearing about it.
    let attempt = spawner.retain_keep_alive(RuntimeLiveness::Ref);
    io.spawn(async move {
        let _attempt = attempt;
        let target = match tokio::net::lookup_host((host.as_str(), port)).await {
            Ok(mut candidates) => candidates.next(),
            Err(_) => None,
        };
        let Some(target) = target else {
            enqueue_ordered(
                &connect_spawner,
                NetEvent::ConnectFailed {
                    token,
                    code: "ENOTFOUND",
                    message: format!("getaddrinfo ENOTFOUND {host}"),
                },
                RuntimeLiveness::Unref,
                )
                .await;
            return;
        };
        match tokio::net::TcpStream::connect(target).await {
            Ok(stream) => {
                let connection = adopt(
                    NetSocket::Tcp(Arc::new(stream)),
                    Some(target),
                    &connect_table,
                    &connect_ids,
                    &connect_spawner,
                );
                enqueue_ordered(
                    &connect_spawner,
                    NetEvent::Connected { token, connection },
                    RuntimeLiveness::Unref,
                    )
                    .await;
            }
            Err(error) => {
                let code = io_code(&error);
                enqueue_ordered(
                    &connect_spawner,
                    NetEvent::ConnectFailed {
                        token,
                        code,
                        message: format!("connect {code} {target}"),
                    },
                    RuntimeLiveness::Unref,
                    )
                    .await;
            }
        }
    });
    Ok(RuntimeValue::undefined())
}

/// Everything the shim is told about, in the order it happened.
#[derive(Clone)]
enum NetEvent {
    Accepted {
        server: u32,
        connection: u32,
        remote: Option<std::net::SocketAddr>,
    },
    Connected {
        token: u32,
        connection: u32,
    },
    ConnectFailed {
        token: u32,
        code: &'static str,
        message: String,
    },
    Data {
        connection: u32,
        payload: String,
    },
    Ended {
        connection: u32,
    },
    ReadFailed {
        connection: u32,
        code: &'static str,
    },
    WriteDone {
        connection: u32,
        token: u32,
        code: Option<&'static str>,
    },
    ShutdownDone {
        connection: u32,
        token: u32,
    },
}

impl RuntimeTask for NetEvent {
    fn run(self: Box<Self>, runtime: &mut Runtime) -> Result<(), OtterError> {
        let Some(context) = runtime.realm_execution_context() else {
            return Ok(());
        };
        deliver(runtime, &context, *self)
    }
}

/// Hand one event to the shim's dispatcher, which owns the JavaScript side of
/// every server and connection.
fn deliver(
    runtime: &mut Runtime,
    context: &RuntimeExecutionContext,
    event: NetEvent,
) -> Result<(), OtterError> {
    runtime.run_native_event(context, |ctx| {
        ctx.scope(|mut scope| {
            let globals = scope.global_this();
            let dispatch = scope.get(globals, "__otterNetDeliver")?;
            if !scope.is_callable(dispatch) {
                let undefined = scope.undefined();
                return Ok(scope.finish(undefined));
            }
            let (name, first, second, third) = match &event {
                NetEvent::Accepted {
                    server,
                    connection,
                    remote,
                } => {
                    let name = scope.string("accept")?;
                    let server = scope.number(f64::from(*server));
                    let connection = scope.number(f64::from(*connection));
                    let remote = match remote {
                        Some(remote) => address_object(&mut scope, *remote)?,
                        None => scope.object()?,
                    };
                    (name, server, connection, remote)
                }
                NetEvent::Connected { token, connection } => {
                    let name = scope.string("connect")?;
                    let token = scope.number(f64::from(*token));
                    let connection = scope.number(f64::from(*connection));
                    let undefined = scope.undefined();
                    (name, token, connection, undefined)
                }
                NetEvent::ConnectFailed {
                    token,
                    code,
                    message,
                } => {
                    let name = scope.string("connectError")?;
                    let token = scope.number(f64::from(*token));
                    let code = scope.string(code)?;
                    let message = scope.string(message)?;
                    (name, token, code, message)
                }
                NetEvent::Data {
                    connection,
                    payload,
                } => {
                    let name = scope.string("data")?;
                    let connection = scope.number(f64::from(*connection));
                    let payload = scope.string(payload)?;
                    let undefined = scope.undefined();
                    (name, connection, payload, undefined)
                }
                NetEvent::Ended { connection } => {
                    let name = scope.string("end")?;
                    let connection = scope.number(f64::from(*connection));
                    let undefined = scope.undefined();
                    let second = scope.undefined();
                    (name, connection, undefined, second)
                }
                NetEvent::WriteDone {
                    connection,
                    token,
                    code,
                } => {
                    let name = scope.string("writeDone")?;
                    let connection = scope.number(f64::from(*connection));
                    let token = scope.number(f64::from(*token));
                    let code = match code {
                        Some(code) => scope.string(code)?,
                        None => scope.undefined(),
                    };
                    (name, connection, token, code)
                }
                NetEvent::ShutdownDone { connection, token } => {
                    let name = scope.string("shutdownDone")?;
                    let connection = scope.number(f64::from(*connection));
                    let token = scope.number(f64::from(*token));
                    let undefined = scope.undefined();
                    (name, connection, token, undefined)
                }
                NetEvent::ReadFailed { connection, code } => {
                    let name = scope.string("readError")?;
                    let connection = scope.number(f64::from(*connection));
                    let code = scope.string(code)?;
                    let undefined = scope.undefined();
                    (name, connection, code, undefined)
                }
            };
            let undefined = scope.undefined();
            let result = scope.call(dispatch, undefined, &[name, first, second, third])?;
            Ok(scope.finish(result))
        })
    })
}

fn io_spawner(
    spawner: Option<&RuntimeTaskSpawner>,
    who: &'static str,
) -> Result<RuntimeTaskSpawner, RuntimeNativeError> {
    spawner.cloned().ok_or_else(|| {
        runtime_type_error(
            who,
            "host runtime did not install an event loop".to_string(),
        )
    })
}

fn write_bytes(table: &Table, id: u32, bytes: &[u8], token: u32) -> bool {
    let table = table
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    match table.get(&id).map(|entry| &entry.kind) {
        Some(EntryKind::Connection {
            outgoing: Some(outgoing),
            queued,
            ..
        }) => {
            queued.fetch_add(1, Ordering::SeqCst);
            let sent = outgoing.send(WriteMsg::Data(bytes.to_vec(), token)).is_ok();
            if !sent {
                queued.fetch_sub(1, Ordering::SeqCst);
            }
            sent
        }
        _ => false,
    }
}

/// Attempt an in-line non-blocking write while the writer queue is idle.
/// Answers the number of bytes accepted by the kernel (possibly zero); the
/// caller queues the remainder through the ordinary async path.
fn try_write_bytes(table: &Table, id: u32, bytes: &[u8]) -> usize {
    let table = table
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    match table.get(&id).map(|entry| &entry.kind) {
        Some(EntryKind::Connection {
            outgoing: Some(_),
            socket,
            queued,
            ..
        }) if queued.load(Ordering::SeqCst) == 0 => {
            socket.try_write(bytes).unwrap_or_default()
        }
        _ => 0,
    }
}

/// Decrement `counter` when the returned guard drops, so every writer-task
/// message is accounted exactly once even on early returns.
fn scopeguard_decrement(
    counter: &Arc<std::sync::atomic::AtomicUsize>,
) -> impl Drop + use<> {
    struct Guard(Arc<std::sync::atomic::AtomicUsize>);
    impl Drop for Guard {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }
    Guard(counter.clone())
}

/// Forget an entry entirely: its loops end and its hold on the runtime goes
/// with it.
fn close_entry(table: &Table, id: u32) {
    let removed = table
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .remove(&id);
    match removed {
        Some(Entry {
            kind: EntryKind::Listener { ref shutdown },
            ..
        }) => shutdown.notify_one(),
        Some(Entry {
            kind: EntryKind::Connection { ref abort, .. },
            ..
        }) => abort.notify_one(),
        None => {}
    }
}

/// Let go of an entry's hold on the runtime while leaving it usable.
fn release(table: &Table, id: u32) {
    let mut table = table
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(entry) = table.get_mut(&id) {
        entry.keep_alive.take();
    }
}

async fn write_all(stream: &NetSocket, bytes: &[u8]) -> std::io::Result<()> {
    let mut rest = bytes;
    while !rest.is_empty() {
        stream.writable().await?;
        match stream.try_write(rest) {
            Ok(0) => return Err(std::io::ErrorKind::WriteZero.into()),
            Ok(written) => rest = &rest[written..],
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn shutdown_write(stream: &NetSocket) {
    // SAFETY: the descriptor is owned by a live stream, and `SHUT_WR` leaves
    // the read half — which the reader task still holds — untouched.
    unsafe {
        libc::shutdown(stream.raw_fd(), libc::SHUT_WR);
    }
}

fn address_object<'scope>(
    scope: &mut RuntimeNativeScope<'scope, '_>,
    address: std::net::SocketAddr,
) -> Result<RuntimeLocal<'scope>, RuntimeNativeError> {
    let result = scope.object()?;
    let host = scope.string(&address.ip().to_string())?;
    scope.set(result, "address", host)?;
    let port = scope.number(f64::from(address.port()));
    scope.set(result, "port", port)?;
    let family = scope.string(if address.is_ipv4() { "IPv4" } else { "IPv6" })?;
    scope.set(result, "family", family)?;
    Ok(result)
}

fn resolve(host: &str, port: u16) -> Option<std::net::SocketAddr> {
    (host, port).to_socket_addrs().ok()?.next()
}

fn handle_arg(args: &[RuntimeValue], index: usize) -> u32 {
    args.get(index)
        .and_then(|value| value.as_f64())
        .unwrap_or(0.0) as u32
}

fn string_arg(
    ctx: &mut RuntimeNativeCtx<'_>,
    args: &[RuntimeValue],
    index: usize,
) -> Option<String> {
    args.get(index)
        .and_then(|value| value.as_string(ctx.heap()))
        .map(|value| value.to_lossy_string(ctx.heap()))
}

fn io_code(error: &std::io::Error) -> &'static str {
    match error.kind() {
        std::io::ErrorKind::NotFound => "ENOENT",
        std::io::ErrorKind::AddrInUse => "EADDRINUSE",
        std::io::ErrorKind::AddrNotAvailable => "EADDRNOTAVAIL",
        std::io::ErrorKind::PermissionDenied => "EACCES",
        std::io::ErrorKind::ConnectionRefused => "ECONNREFUSED",
        std::io::ErrorKind::ConnectionReset => "ECONNRESET",
        std::io::ErrorKind::TimedOut => "ETIMEDOUT",
        _ => "EINVAL",
    }
}

fn system_error(
    error: &std::io::Error,
    syscall: &'static str,
    address: &str,
) -> RuntimeNativeError {
    let code = io_code(error);
    RuntimeNativeError::Syscall {
        code,
        message: format!("{syscall} {code} {address}").trim_end().to_string(),
        syscall,
        path: None,
        dest: None,
        errno: error.raw_os_error().unwrap_or(0),
    }
}

fn system_error_code(code: &'static str, syscall: &'static str) -> RuntimeNativeError {
    RuntimeNativeError::Syscall {
        code,
        message: format!("{syscall} {code}"),
        syscall,
        path: None,
        dest: None,
        errno: 0,
    }
}

fn bytes_to_latin1(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| *byte as char).collect()
}

fn latin1_to_bytes(text: &str) -> Vec<u8> {
    text.chars()
        .map(|character| character as u32 as u8)
        .collect()
}

/// Releasing the entry releases its hold on the runtime, which is what lets a
/// program with nothing left open exit.
impl Drop for Entry {
    fn drop(&mut self) {
        self.keep_alive.take();
    }
}
