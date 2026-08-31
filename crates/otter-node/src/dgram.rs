//! Native transport under the vendored `dgram` stack: UDP sockets.
//!
//! # Contents
//! - [`dgram_binding_cjs_value`] exports the raw bind/send/option surface as
//!   `internal/otter/dgram` for the compat `udp_wrap` handle.
//! - A socket table owned by the natives, keyed by the handle the wrap holds.
//! - A receive loop per bound socket, delivering datagrams onto the isolate
//!   thread.
//!
//! # Invariants
//! - Binding and sending are network operations and are gated by the `net`
//!   capability, checked against the address involved.
//! - The receive loop runs on the host's IO runtime and never touches VM
//!   state; it hands owned bytes to a task that re-enters JavaScript on the
//!   isolate thread, in order. Every retained datagram is admitted before
//!   allocation against finite binding and runtime ledgers.
//! - A socket holds the runtime open while it is bound, so a program waiting
//!   for a datagram does not exit early.
//!
//! # See also
//! - `nodelib/compat/internal_udp_wrap.js`

use std::collections::HashMap;
use std::net::ToSocketAddrs;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use otter_runtime::{
    CapabilitySet, OtterError, Runtime, RuntimeExecutionContext, RuntimeKeepAlive, RuntimeLiveness,
    RuntimeLocal, RuntimeNativeCtx, RuntimeNativeError, RuntimeNativeScope, RuntimeTask,
    RuntimeTaskSpawner, RuntimeValue, runtime_type_error,
};

use crate::transport_payload::{QueuedPayload, TransportPayloadBudget};

/// One live socket: the sending half, plus whatever keeps the loop alive.
struct SocketEntry {
    socket: Arc<tokio::net::UdpSocket>,
    keep_alive: Option<RuntimeKeepAlive>,
    /// Whether this process is the one taking this socket's datagrams. A
    /// socket held only to be shared with another process must leave them in
    /// the kernel for whoever is reading.
    reading: Arc<std::sync::atomic::AtomicBool>,
    /// Woken when `reading` turns on, so the loop does not poll for it.
    resumed: Arc<tokio::sync::Notify>,
}

struct DatagramTable {
    entries: Mutex<HashMap<u32, SocketEntry>>,
    payloads: TransportPayloadBudget,
}

impl DatagramTable {
    fn new(runtime_resources: otter_runtime::ResourceAccount) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            payloads: TransportPayloadBudget::standard(runtime_resources),
        }
    }

    fn lock(&self) -> std::sync::LockResult<std::sync::MutexGuard<'_, HashMap<u32, SocketEntry>>> {
        self.entries.lock()
    }
}

type SocketTable = Arc<DatagramTable>;

/// Build the CommonJS export of `node:dgram`.
///
/// # Errors
/// Returns a native error when the shim fails to allocate or evaluate.
/// The raw socket surface the compat `udp_wrap` drives, exported as
/// `internal/otter/dgram`.
///
/// # Errors
/// Returns a native error when the surface fails to allocate.
pub fn dgram_binding_cjs_value<'scope>(
    scope: &mut RuntimeNativeScope<'scope, '_>,
    capabilities: &CapabilitySet,
    runtime_task_spawner: Option<RuntimeTaskSpawner>,
    _module: RuntimeLocal<'scope>,
    _require: RuntimeLocal<'scope>,
) -> Result<RuntimeLocal<'scope>, RuntimeNativeError> {
    let native = build_native(scope, capabilities, runtime_task_spawner)?;
    let globals = scope.global_this();
    // The receive loop dispatches through a global the compat handle
    // installs, so the native surface is reachable from it too. Both stay
    // non-enumerable: the Node test harness flags any enumerable global it
    // does not recognize as a leak.
    scope.define(
        globals,
        "__otterDgramNative",
        native,
        otter_vm::Attr {
            writable: true,
            enumerable: false,
            configurable: true,
        }
        .to_flags(),
    )?;
    Ok(native)
}

fn build_native<'scope>(
    scope: &mut RuntimeNativeScope<'scope, '_>,
    capabilities: &CapabilitySet,
    spawner: Option<RuntimeTaskSpawner>,
) -> Result<RuntimeLocal<'scope>, RuntimeNativeError> {
    let object = scope.object()?;
    let runtime_resources = spawner
        .as_ref()
        .map(RuntimeTaskSpawner::resource_account)
        .unwrap_or_default();
    let sockets: SocketTable = Arc::new(DatagramTable::new(runtime_resources));
    let next_id = Arc::new(AtomicU32::new(1));

    let bind_caps = capabilities.clone();
    let bind_sockets = sockets.clone();
    let bind_ids = next_id.clone();
    let bind_spawner = spawner.clone();
    let bind = scope.native_closure(
        "bind",
        3,
        &[],
        move |ctx: &mut RuntimeNativeCtx<'_>, args: &[RuntimeValue], _c: &[RuntimeValue]| {
            bind_socket(
                ctx,
                args,
                &bind_caps,
                &bind_sockets,
                &bind_ids,
                bind_spawner.as_ref(),
            )
        },
    )?;
    scope.set(object, "bind", bind)?;

    let send_caps = capabilities.clone();
    let send_sockets = sockets.clone();
    let send = scope.native_closure(
        "send",
        4,
        &[],
        move |ctx: &mut RuntimeNativeCtx<'_>, args: &[RuntimeValue], _c: &[RuntimeValue]| {
            send_datagram(ctx, args, &send_caps, &send_sockets)
        },
    )?;
    scope.set(object, "send", send)?;

    let address_sockets = sockets.clone();
    let address = scope.native_closure(
        "address",
        1,
        &[],
        move |ctx: &mut RuntimeNativeCtx<'_>, args: &[RuntimeValue], _c: &[RuntimeValue]| {
            socket_address(ctx, args, &address_sockets)
        },
    )?;
    scope.set(object, "address", address)?;

    let close_sockets = sockets.clone();
    let close = scope.native_closure(
        "close",
        1,
        &[],
        move |_ctx: &mut RuntimeNativeCtx<'_>, args: &[RuntimeValue], _c: &[RuntimeValue]| {
            let id = handle_arg(args, 0);
            close_sockets
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(&id);
            Ok(RuntimeValue::undefined())
        },
    )?;
    scope.set(object, "close", close)?;

    let option_sockets = sockets.clone();
    let option = scope.native_closure(
        "setOption",
        3,
        &[],
        move |ctx: &mut RuntimeNativeCtx<'_>, args: &[RuntimeValue], _c: &[RuntimeValue]| {
            set_option(ctx, args, &option_sockets)
        },
    )?;
    scope.set(object, "setOption", option)?;

    let membership_sockets = sockets.clone();
    let membership = scope.native_closure(
        "membership",
        4,
        &[],
        move |ctx: &mut RuntimeNativeCtx<'_>, args: &[RuntimeValue], _c: &[RuntimeValue]| {
            change_membership(ctx, args, &membership_sockets)
        },
    )?;
    scope.set(object, "membership", membership)?;

    // A program may say it is not waiting on a socket, the way it can for
    // a timer, without closing it.
    let hold_sockets = sockets.clone();
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
            let mut table = hold_sockets
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(entry) = table.get_mut(&id)
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

    let resolve_caps = capabilities.clone();
    let resolve = scope.native_closure(
        "resolve",
        3,
        &[],
        move |ctx: &mut RuntimeNativeCtx<'_>, args: &[RuntimeValue], _c: &[RuntimeValue]| {
            resolve_peer(ctx, args, &resolve_caps)
        },
    )?;
    scope.set(object, "resolve", resolve)?;

    // A bound socket can cross a channel to another process. What crosses is
    // a duplicate of the descriptor, so this process keeps its own.
    let dup_sockets = sockets.clone();
    let dup_fd = scope.native_closure(
        "dupFd",
        1,
        &[],
        move |_ctx: &mut RuntimeNativeCtx<'_>, args: &[RuntimeValue], _c: &[RuntimeValue]| {
            use std::os::fd::AsRawFd;
            let id = handle_arg(args, 0);
            let raw = dup_sockets
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get(&id)
                .map(|entry| entry.socket.as_raw_fd());
            let Some(raw) = raw else {
                return Ok(RuntimeValue::number_i32(-1));
            };
            // SAFETY: `raw` names a descriptor this table owns and keeps open
            // for the duration of the call; `dup` only reads it.
            let copy = unsafe { libc::dup(raw) };
            Ok(RuntimeValue::number_i32(copy))
        },
    )?;
    scope.set(object, "dupFd", dup_fd)?;

    // Taking datagrams is something a process asks for. One that holds a
    // socket only to hand it to another must leave them in the kernel.
    let reading_sockets = sockets.clone();
    let set_reading = scope.native_closure(
        "setReading",
        2,
        &[],
        move |_ctx: &mut RuntimeNativeCtx<'_>, args: &[RuntimeValue], _c: &[RuntimeValue]| {
            let id = handle_arg(args, 0);
            let wanted = args
                .get(1)
                .and_then(|value| value.as_boolean())
                .unwrap_or(true);
            let table = reading_sockets
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(entry) = table.get(&id) {
                entry.reading.store(wanted, Ordering::Relaxed);
                if wanted {
                    // A stored permit, not a broadcast: the loop may not have
                    // parked yet, and a wake it never sees is a socket that
                    // never reads.
                    entry.resumed.notify_one();
                }
            }
            Ok(RuntimeValue::undefined())
        },
    )?;
    scope.set(object, "setReading", set_reading)?;

    let adopt_sockets = sockets.clone();
    let adopt_ids = next_id.clone();
    let adopt_spawner = spawner.clone();
    let adopt_fd = scope.native_closure(
        "adoptFd",
        1,
        &[],
        move |_ctx: &mut RuntimeNativeCtx<'_>, args: &[RuntimeValue], _c: &[RuntimeValue]| {
            let raw = args
                .first()
                .and_then(|value| value.as_f64())
                .unwrap_or(-1.0) as std::os::fd::RawFd;
            let Some(spawner) = adopt_spawner.as_ref() else {
                return Ok(RuntimeValue::number_i32(-1));
            };
            if raw < 0 {
                return Ok(RuntimeValue::number_i32(-1));
            }
            // SAFETY: the descriptor arrived from the channel, which handed
            // ownership over with it; nothing else in this process holds it.
            let std_socket =
                unsafe { <std::net::UdpSocket as std::os::fd::FromRawFd>::from_raw_fd(raw) };
            if std_socket.set_nonblocking(true).is_err() {
                return Ok(RuntimeValue::number_i32(-1));
            }
            Ok(adopt_udp(&adopt_sockets, &adopt_ids, spawner, std_socket)
                .map_or(RuntimeValue::number_i32(-1), |id| {
                    RuntimeValue::number_i32(id as i32)
                }))
        },
    )?;
    scope.set(object, "adoptFd", adopt_fd)?;
    Ok(object)
}

/// Carry one bound socket's datagrams onto the isolate thread until it closes.
fn spawn_receive_loop(
    id: u32,
    socket: Arc<tokio::net::UdpSocket>,
    sockets: SocketTable,
    spawner: RuntimeTaskSpawner,
    io: &tokio::runtime::Handle,
    reading: Arc<std::sync::atomic::AtomicBool>,
    resumed: Arc<tokio::sync::Notify>,
) {
    io.spawn(async move {
        let mut buffer = vec![0u8; 65_536];
        loop {
            while !reading.load(Ordering::Relaxed) {
                let still_open = sockets
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .contains_key(&id);
                if !still_open {
                    return;
                }
                resumed.notified().await;
            }
            let received = socket.recv_from(&mut buffer).await;
            let still_open = sockets
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .contains_key(&id);
            if !still_open {
                return;
            }
            match received {
                Ok((length, from)) => {
                    let datagram = received_datagram(&sockets, id, &buffer[..length], from);
                    // Ordered delivery retries on backpressure: a bounded
                    // inbox that dropped would silently lose datagrams the
                    // kernel had already handed over.
                    if !spawner
                        .enqueue_ordered(datagram, RuntimeLiveness::Unref)
                        .await
                    {
                        return;
                    }
                }
                Err(_) => return,
            }
        }
    });
}

/// Take a bound UDP socket over and start carrying its datagrams.
fn adopt_udp(
    sockets: &SocketTable,
    next_id: &Arc<AtomicU32>,
    spawner: &RuntimeTaskSpawner,
    std_socket: std::net::UdpSocket,
) -> Option<u32> {
    let io = spawner.io_handle()?;
    // `from_std` registers with the reactor and needs the runtime in scope.
    let socket = {
        let _guard = io.enter();
        Arc::new(tokio::net::UdpSocket::from_std(std_socket).ok()?)
    };
    let id = next_id.fetch_add(1, Ordering::Relaxed);
    let keep_alive = spawner.retain_keep_alive(RuntimeLiveness::Ref);
    let reading = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let resumed = Arc::new(tokio::sync::Notify::new());
    sockets
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(
            id,
            SocketEntry {
                socket: socket.clone(),
                keep_alive: Some(keep_alive),
                reading: reading.clone(),
                resumed: resumed.clone(),
            },
        );
    spawn_receive_loop(
        id,
        socket,
        sockets.clone(),
        spawner.clone(),
        &io,
        reading,
        resumed,
    );
    Some(id)
}

/// Names the shim may pass to `setOption`, paired with what each one does.
///
/// The socket option is applied through the platform, so an out-of-range value
/// is refused by the kernel rather than by a check of our own — which is what
/// makes `setTTL(1000)` fail with `EINVAL` the way Node's does.
fn set_option(
    ctx: &mut RuntimeNativeCtx<'_>,
    args: &[RuntimeValue],
    sockets: &SocketTable,
) -> Result<RuntimeValue, RuntimeNativeError> {
    let id = handle_arg(args, 0);
    let name = string_arg(ctx, args, 1).unwrap_or_default();
    let number = args.get(2).and_then(|value| value.as_f64()).unwrap_or(0.0);
    let flag = args
        .get(2)
        .and_then(|value| value.as_boolean())
        .unwrap_or(number != 0.0);
    let text = string_arg(ctx, args, 2);

    let socket = lookup_socket(sockets, id);
    let Some(socket) = socket else {
        return Err(option_error(&name, "EBADF", 9));
    };

    let outcome = match name.as_str() {
        "setTTL" => socket.set_ttl(number as u32),
        "setMulticastTTL" => socket.set_multicast_ttl_v4(number as u32),
        "setMulticastLoopback" => socket.set_multicast_loop_v4(flag),
        "setBroadcast" => socket.set_broadcast(flag),
        "setMulticastInterface" => text
            .as_deref()
            .unwrap_or_default()
            .parse::<std::net::Ipv4Addr>()
            .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))
            .and_then(|interface| multicast_interface(&socket, interface)),
        "getRecvBufferSize" | "getSendBufferSize" | "setRecvBufferSize" | "setSendBufferSize" => {
            return buffer_size(&socket, &name, number as i32);
        }
        _ => return Err(option_error(&name, "EINVAL", 22)),
    };
    outcome
        .map_err(|error| option_error(&name, io_code(&error), error.raw_os_error().unwrap_or(0)))?;
    Ok(RuntimeValue::undefined())
}

/// Choose the interface outgoing multicast leaves by. Tokio has no wrapper for
/// `IP_MULTICAST_IF`, so the option is set through the platform directly.
fn multicast_interface(
    socket: &tokio::net::UdpSocket,
    interface: std::net::Ipv4Addr,
) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;

    let value = libc::in_addr {
        s_addr: u32::from_ne_bytes(interface.octets()),
    };
    // SAFETY: `fd` is owned by a live socket and the value is the `in_addr`
    // the option is documented to take.
    let outcome = unsafe {
        libc::setsockopt(
            socket.as_raw_fd(),
            libc::IPPROTO_IP,
            libc::IP_MULTICAST_IF,
            std::ptr::from_ref(&value).cast(),
            std::mem::size_of::<libc::in_addr>() as libc::socklen_t,
        )
    };
    if outcome == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Read or write one of the socket's buffer sizes.
fn buffer_size(
    socket: &tokio::net::UdpSocket,
    name: &str,
    value: i32,
) -> Result<RuntimeValue, RuntimeNativeError> {
    use std::os::fd::AsRawFd;

    let option = if name.ends_with("RecvBufferSize") {
        libc::SO_RCVBUF
    } else {
        libc::SO_SNDBUF
    };
    let fd = socket.as_raw_fd();
    if name.starts_with("set") {
        // SAFETY: `fd` is owned by a live socket and the value is a plain `int`.
        let outcome = unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                option,
                std::ptr::from_ref(&value).cast(),
                std::mem::size_of::<i32>() as libc::socklen_t,
            )
        };
        if outcome != 0 {
            let error = std::io::Error::last_os_error();
            return Err(option_error(
                name,
                io_code(&error),
                error.raw_os_error().unwrap_or(0),
            ));
        }
        return Ok(RuntimeValue::undefined());
    }
    let mut current: i32 = 0;
    let mut length = std::mem::size_of::<i32>() as libc::socklen_t;
    // SAFETY: `fd` is owned by a live socket; the out-parameters are sized to
    // the `int` the option is documented to return.
    let outcome = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            option,
            std::ptr::from_mut(&mut current).cast(),
            &raw mut length,
        )
    };
    if outcome != 0 {
        let error = std::io::Error::last_os_error();
        return Err(option_error(
            name,
            io_code(&error),
            error.raw_os_error().unwrap_or(0),
        ));
    }
    Ok(RuntimeValue::number_i32(current))
}

/// Join or leave a multicast group.
fn change_membership(
    ctx: &mut RuntimeNativeCtx<'_>,
    args: &[RuntimeValue],
    sockets: &SocketTable,
) -> Result<RuntimeValue, RuntimeNativeError> {
    let id = handle_arg(args, 0);
    let operation = string_arg(ctx, args, 1).unwrap_or_default();
    let group = string_arg(ctx, args, 2).unwrap_or_default();
    let interface = string_arg(ctx, args, 3).unwrap_or_default();

    let Some(socket) = lookup_socket(sockets, id) else {
        return Err(option_error(&operation, "EBADF", 9));
    };
    let Ok(group) = group.parse::<std::net::Ipv4Addr>() else {
        return Err(option_error(&operation, "EINVAL", 22));
    };
    let interface = interface
        .parse::<std::net::Ipv4Addr>()
        .unwrap_or(std::net::Ipv4Addr::UNSPECIFIED);
    let outcome = if operation == "addMembership" {
        socket.join_multicast_v4(group, interface)
    } else {
        socket.leave_multicast_v4(group, interface)
    };
    outcome.map_err(|error| {
        option_error(
            &operation,
            io_code(&error),
            error.raw_os_error().unwrap_or(0),
        )
    })?;
    Ok(RuntimeValue::undefined())
}

/// Resolve a peer the way a connect would, without opening anything: the shim
/// needs the resolved address before it can report `remoteAddress()`.
fn resolve_peer(
    ctx: &mut RuntimeNativeCtx<'_>,
    args: &[RuntimeValue],
    capabilities: &CapabilitySet,
) -> Result<RuntimeValue, RuntimeNativeError> {
    let port = args.first().and_then(|value| value.as_f64()).unwrap_or(0.0) as u16;
    let address = string_arg(ctx, args, 1).unwrap_or_default();
    let want_ipv4 = string_arg(ctx, args, 2).as_deref() != Some("udp6");
    let address = if address.is_empty() {
        if want_ipv4 { "127.0.0.1" } else { "::1" }.to_string()
    } else {
        address
    };
    if !capabilities.net.matches(&address) {
        return Err(runtime_type_error(
            "dgram.connect",
            format!("permission denied for '{address}'"),
        ));
    }
    let target = resolve_target(&address, port, want_ipv4)
        .ok_or_else(|| option_error("connect", "ENOTFOUND", 0))?;
    ctx.scope(|mut scope| {
        let result = scope.object()?;
        let address = scope.string(&address_text(&target))?;
        scope.set(result, "address", address)?;
        let port = scope.number(f64::from(target.port()));
        scope.set(result, "port", port)?;
        let family = scope.string(if target.is_ipv4() { "IPv4" } else { "IPv6" })?;
        scope.set(result, "family", family)?;
        Ok(scope.finish(result))
    })
}

fn lookup_socket(sockets: &SocketTable, id: u32) -> Option<Arc<tokio::net::UdpSocket>> {
    sockets
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(&id)
        .map(|entry| entry.socket.clone())
}

/// A name resolves to whatever the platform says; only a candidate of the
/// socket's own family can be used.
fn resolve_target(address: &str, port: u16, want_ipv4: bool) -> Option<std::net::SocketAddr> {
    let mut candidates: Vec<_> = (address, port).to_socket_addrs().ok()?.collect();
    candidates.sort_by_key(|candidate| candidate.is_ipv4() != want_ipv4);
    candidates.into_iter().next()
}

/// Node reports a failed socket option as `Error: <name> <code>`.
fn option_error(name: &str, code: &'static str, errno: i32) -> RuntimeNativeError {
    RuntimeNativeError::Syscall {
        code,
        message: format!("{name} {code}"),
        syscall: "setsockopt",
        path: None,
        dest: None,
        errno,
    }
}

fn io_code(error: &std::io::Error) -> &'static str {
    match error.kind() {
        std::io::ErrorKind::AddrInUse => "EADDRINUSE",
        std::io::ErrorKind::AddrNotAvailable => "EADDRNOTAVAIL",
        std::io::ErrorKind::PermissionDenied => "EACCES",
        std::io::ErrorKind::ConnectionRefused => "ECONNREFUSED",
        // A datagram larger than the socket's send buffer is refused by
        // the kernel, and the caller is told which limit it hit.
        _ => match error.raw_os_error() {
            Some(errno) if errno == libc::EMSGSIZE => "EMSGSIZE",
            Some(errno) if errno == libc::ENOBUFS => "ENOBUFS",
            Some(errno) if errno == libc::EHOSTUNREACH => "EHOSTUNREACH",
            Some(errno) if errno == libc::ENETUNREACH => "ENETUNREACH",
            _ => "EINVAL",
        },
    }
}

/// Bind a socket and start delivering what arrives on it.
fn bind_socket(
    ctx: &mut RuntimeNativeCtx<'_>,
    args: &[RuntimeValue],
    capabilities: &CapabilitySet,
    sockets: &SocketTable,
    next_id: &Arc<AtomicU32>,
    spawner: Option<&RuntimeTaskSpawner>,
) -> Result<RuntimeValue, RuntimeNativeError> {
    let kind = string_arg(ctx, args, 0).unwrap_or_else(|| "udp4".to_string());
    let port = args.get(1).and_then(|value| value.as_f64()).unwrap_or(0.0) as u16;
    let address = string_arg(ctx, args, 2).unwrap_or_default();
    let address = if address.is_empty() {
        if kind == "udp6" { "::" } else { "0.0.0.0" }.to_string()
    } else {
        address
    };

    if !capabilities.net.matches(&address) {
        return Err(runtime_type_error(
            "dgram.bind",
            format!("permission denied for '{address}'"),
        ));
    }
    let Some(spawner) = spawner else {
        return Err(runtime_type_error(
            "dgram.bind",
            "host runtime did not install an IO runtime".to_string(),
        ));
    };
    let Some(io) = spawner.io_handle() else {
        return Err(runtime_type_error(
            "dgram.bind",
            "host runtime did not install an IO runtime".to_string(),
        ));
    };

    // A bind address may be a name, and a name resolves to both families; only
    // the one matching the socket type can be bound.
    let flags = args.get(3).and_then(|value| value.as_f64()).unwrap_or(0.0) as u32;
    let target = resolve_target(&address, port, kind != "udp6")
        .ok_or_else(|| option_error("bind", "ENOTFOUND", 0))?;
    let bound = bind_datagram_socket(target, flags)
        .and_then(|socket| {
            socket.set_nonblocking(true)?;
            Ok(socket)
        })
        .map_err(|error| system_error(&error, "bind", &address))?;
    // `from_std` registers with the reactor and needs the runtime in scope.
    // Entering is the way to get that from a thread already inside the
    // runtime; `block_on` would panic there.
    let socket = {
        let _guard = io.enter();
        tokio::net::UdpSocket::from_std(bound)
            .map_err(|error| system_error(&error, "bind", &address))?
    };
    let socket = Arc::new(socket);
    let local = socket
        .local_addr()
        .map_err(|error| system_error(&error, "bind", &address))?;

    let id = next_id.fetch_add(1, Ordering::Relaxed);
    // A bound socket is work the program is waiting on, so it holds the loop
    // open the way a pending timer does.
    let keep_alive = spawner.retain_keep_alive(RuntimeLiveness::Ref);
    let reading = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let resumed = Arc::new(tokio::sync::Notify::new());
    sockets
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(
            id,
            SocketEntry {
                socket: socket.clone(),
                keep_alive: Some(keep_alive),
                reading: reading.clone(),
                resumed: resumed.clone(),
            },
        );

    spawn_receive_loop(
        id,
        socket,
        sockets.clone(),
        spawner.clone(),
        &io,
        reading,
        resumed,
    );

    ctx.scope(|mut scope| {
        let result = scope.object()?;
        let handle = scope.number(f64::from(id));
        scope.set(result, "handle", handle)?;
        let address = scope.string(&address_text(&local))?;
        scope.set(result, "address", address)?;
        let port = scope.number(f64::from(local.port()));
        scope.set(result, "port", port)?;
        let family = scope.string(if local.is_ipv4() { "IPv4" } else { "IPv6" })?;
        scope.set(result, "family", family)?;
        Ok(scope.finish(result))
    })
}

/// libuv's `uv_udp_bind` flags, as the JS handle passes them through.
const UV_UDP_IPV6ONLY: u32 = 1;
const UV_UDP_REUSEADDR: u32 = 4;
const UV_UDP_REUSEPORT: u32 = 8;

/// Bind a UDP socket, applying the bind-time flags the caller asked for.
///
/// `ipv6Only`, address reuse and port reuse all have to be set on the
/// socket before it is bound, so the socket is built by hand rather than
/// by `UdpSocket::bind`.
fn bind_datagram_socket(
    address: std::net::SocketAddr,
    flags: u32,
) -> std::io::Result<std::net::UdpSocket> {
    if flags == 0 {
        return std::net::UdpSocket::bind(address);
    }
    let domain = if address.is_ipv6() {
        libc::AF_INET6
    } else {
        libc::AF_INET
    };
    // SAFETY: a plain socket creation; the fd is adopted below and closed
    // exactly once by the `UdpSocket` that owns it.
    let fd = unsafe { libc::socket(domain, libc::SOCK_DGRAM, 0) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `fd` is a live socket this function owns from here on.
    let socket = unsafe { <std::net::UdpSocket as std::os::fd::FromRawFd>::from_raw_fd(fd) };
    let set = |level: libc::c_int, option: libc::c_int| -> std::io::Result<()> {
        let enable: libc::c_int = 1;
        // SAFETY: `socket` owns the fd for the call, and `enable` is a
        // valid, initialized option payload of the size passed.
        let outcome = unsafe {
            libc::setsockopt(
                fd,
                level,
                option,
                std::ptr::from_ref(&enable).cast(),
                std::mem::size_of_val(&enable) as libc::socklen_t,
            )
        };
        if outcome != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    };
    if flags & UV_UDP_IPV6ONLY != 0 && address.is_ipv6() {
        set(libc::IPPROTO_IPV6, libc::IPV6_V6ONLY)?;
    }
    if flags & UV_UDP_REUSEADDR != 0 {
        set(libc::SOL_SOCKET, libc::SO_REUSEADDR)?;
    }
    if flags & UV_UDP_REUSEPORT != 0 {
        set(libc::SOL_SOCKET, libc::SO_REUSEPORT)?;
    }
    let (storage, length) = socket_address_bytes(address);
    // SAFETY: `storage` holds a correctly sized `sockaddr_in`/`sockaddr_in6`
    // for `length`, and `fd` is the socket being bound.
    let outcome = unsafe { libc::bind(fd, std::ptr::from_ref(&storage).cast(), length) };
    if outcome != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(socket)
}

/// A socket address in the platform's own layout, with its length.
fn socket_address_bytes(
    address: std::net::SocketAddr,
) -> (libc::sockaddr_storage, libc::socklen_t) {
    let mut storage: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    match address {
        std::net::SocketAddr::V4(v4) => {
            let target = std::ptr::from_mut(&mut storage).cast::<libc::sockaddr_in>();
            // SAFETY: the storage is large enough for `sockaddr_in` and is
            // zeroed, so every field is initialized before the write.
            unsafe {
                (*target).sin_family = libc::AF_INET as libc::sa_family_t;
                (*target).sin_port = v4.port().to_be();
                (*target).sin_addr.s_addr = u32::from_ne_bytes(v4.ip().octets());
            }
            (
                storage,
                std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
            )
        }
        std::net::SocketAddr::V6(v6) => {
            let target = std::ptr::from_mut(&mut storage).cast::<libc::sockaddr_in6>();
            // SAFETY: as above, for the v6 layout.
            unsafe {
                (*target).sin6_family = libc::AF_INET6 as libc::sa_family_t;
                (*target).sin6_port = v6.port().to_be();
                (*target).sin6_addr.s6_addr = v6.ip().octets();
                (*target).sin6_scope_id = v6.scope_id();
            }
            (
                storage,
                std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t,
            )
        }
    }
}

/// The textual form of an address, with the zone id a link-local IPv6
/// address carries. `fe80::1` alone does not name a destination — the
/// interface it is local to is part of the address.
fn address_text(address: &std::net::SocketAddr) -> String {
    let std::net::SocketAddr::V6(v6) = address else {
        return address.ip().to_string();
    };
    let scope = v6.scope_id();
    if scope == 0 || !v6.ip().is_unicast_link_local() {
        return address.ip().to_string();
    }
    let mut name = [0i8; libc::IF_NAMESIZE];
    // SAFETY: `name` is `IF_NAMESIZE` bytes, which is the buffer size
    // `if_indextoname` documents; it writes a NUL-terminated string or null.
    let resolved = unsafe { libc::if_indextoname(scope, name.as_mut_ptr()) };
    if resolved.is_null() {
        return format!("{}%{scope}", v6.ip());
    }
    // SAFETY: `if_indextoname` returned its own buffer, NUL-terminated.
    let text = unsafe { std::ffi::CStr::from_ptr(name.as_ptr()) };
    format!("{}%{}", v6.ip(), text.to_string_lossy())
}

/// One received datagram, handed to the isolate thread.
// Runtime tasks are boxed at enqueue. Boxing the payload again would add an
// allocation without shrinking the task allocation that owns this enum.
#[allow(clippy::large_enum_variant)]
enum Datagram {
    Message {
        id: u32,
        payload: QueuedPayload,
        address: String,
        port: u16,
        family: &'static str,
    },
    ReadFailed {
        id: u32,
        code: &'static str,
    },
}

fn received_datagram(
    sockets: &SocketTable,
    id: u32,
    bytes: &[u8],
    from: std::net::SocketAddr,
) -> Datagram {
    match sockets.payloads.copy_from(bytes) {
        Ok(payload) => Datagram::Message {
            id,
            payload,
            address: address_text(&from),
            port: from.port(),
            family: if from.is_ipv4() { "IPv4" } else { "IPv6" },
        },
        // The kernel has already consumed this datagram. Make the loss
        // observable and continue the receive loop; libuv likewise reports
        // an allocation ENOBUFS and keeps receiving.
        Err(_) => Datagram::ReadFailed {
            id,
            code: "ENOBUFS",
        },
    }
}

impl RuntimeTask for Datagram {
    fn run(self: Box<Self>, runtime: &mut Runtime) -> Result<(), OtterError> {
        let Some(context) = runtime.realm_execution_context() else {
            return Ok(());
        };
        deliver(runtime, &context, *self)
    }
}

/// Hand the datagram to the shim's dispatcher, which owns the JavaScript side
/// of every socket.
fn deliver(
    runtime: &mut Runtime,
    context: &RuntimeExecutionContext,
    datagram: Datagram,
) -> Result<(), OtterError> {
    runtime.run_native_event(context, |ctx| {
        ctx.scope(|mut scope| {
            let globals = scope.global_this();
            let dispatch = scope.get(globals, "__otterDgramDeliver")?;
            if !scope.is_callable(dispatch) {
                return Ok(RuntimeValue::undefined());
            }
            let (id, payload, address, port, family, error) = match &datagram {
                Datagram::Message {
                    id,
                    payload,
                    address,
                    port,
                    family,
                } => {
                    let id = scope.number(f64::from(*id));
                    let payload = scope.string(&bytes_to_latin1(payload.as_slice()))?;
                    let address = scope.string(address)?;
                    let port = scope.number(f64::from(*port));
                    let family = scope.string(family)?;
                    let error = scope.undefined();
                    (id, payload, address, port, family, error)
                }
                Datagram::ReadFailed { id, code } => {
                    let id = scope.number(f64::from(*id));
                    let payload = scope.undefined();
                    let address = scope.undefined();
                    let port = scope.undefined();
                    let family = scope.undefined();
                    let error = scope.string(code)?;
                    (id, payload, address, port, family, error)
                }
            };
            let undefined = scope.undefined();
            let result = scope.call(
                dispatch,
                undefined,
                &[id, payload, address, port, family, error],
            )?;
            Ok(scope.finish(result))
        })
    })
}

fn send_datagram(
    ctx: &mut RuntimeNativeCtx<'_>,
    args: &[RuntimeValue],
    capabilities: &CapabilitySet,
    sockets: &SocketTable,
) -> Result<RuntimeValue, RuntimeNativeError> {
    let id = handle_arg(args, 0);
    let payload = string_arg(ctx, args, 1).unwrap_or_default();
    let port = args.get(2).and_then(|value| value.as_f64()).unwrap_or(0.0) as u16;
    let address = string_arg(ctx, args, 3).unwrap_or_default();

    let socket = lookup_socket(sockets, id);
    let Some(socket) = socket else {
        return Err(runtime_type_error(
            "dgram.send",
            "socket is not bound".to_string(),
        ));
    };

    let bytes = latin1_to_bytes(&payload);
    // `try_send_to` takes a resolved address; a name is resolved first, the
    // same way the platform would. A name like `localhost` resolves to both
    // families, and only the one matching the socket can be sent to.
    let want_ipv4 = socket
        .local_addr()
        .map(|local| local.is_ipv4())
        .unwrap_or(true);
    // An omitted address means the loopback of the socket's own family, which
    // is what Node documents `send` to default to. The unspecified address
    // means the same thing as a destination — it names this host, not a route
    // to nowhere, and sending to it verbatim is what the platform refuses.
    let address = if address.is_empty() || address == "0.0.0.0" || address == "::" {
        if want_ipv4 { "127.0.0.1" } else { "::1" }.to_string()
    } else {
        address
    };
    if !capabilities.net.matches(&address) {
        return Err(runtime_type_error(
            "dgram.send",
            format!("permission denied for '{address}'"),
        ));
    }
    let target = resolve_target(&address, port, want_ipv4)
        .ok_or_else(|| runtime_type_error("dgram.send", format!("cannot resolve '{address}'")))?;
    let sent = socket
        .try_send_to(&bytes, target)
        .map_err(|error| system_error(&error, "send", &address))?;
    Ok(RuntimeValue::number_i32(sent as i32))
}

fn socket_address(
    ctx: &mut RuntimeNativeCtx<'_>,
    args: &[RuntimeValue],
    sockets: &SocketTable,
) -> Result<RuntimeValue, RuntimeNativeError> {
    let id = handle_arg(args, 0);
    let local = {
        let table = sockets
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        table
            .get(&id)
            .and_then(|entry| entry.socket.local_addr().ok())
    };
    let Some(local) = local else {
        return Err(runtime_type_error(
            "dgram.address",
            "socket is not bound".to_string(),
        ));
    };
    ctx.scope(|mut scope| {
        let result = scope.object()?;
        let address = scope.string(&address_text(&local))?;
        scope.set(result, "address", address)?;
        let port = scope.number(f64::from(local.port()));
        scope.set(result, "port", port)?;
        let family = scope.string(if local.is_ipv4() { "IPv4" } else { "IPv6" })?;
        scope.set(result, "family", family)?;
        Ok(scope.finish(result))
    })
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

fn system_error(
    error: &std::io::Error,
    syscall: &'static str,
    address: &str,
) -> RuntimeNativeError {
    let code = io_code(error);
    RuntimeNativeError::Syscall {
        code,
        message: format!("{syscall} {code} {address}"),
        syscall,
        path: None,
        dest: None,
        errno: error.raw_os_error().unwrap_or(0),
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

/// Keep the drop glue explicit: releasing the entry releases the hold on the
/// runtime, which is what lets a program with a closed socket exit.
impl Drop for SocketEntry {
    fn drop(&mut self) {
        self.keep_alive.take();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use otter_runtime::{ResourceAccount, ResourceClass};

    fn current(account: &ResourceAccount, class: ResourceClass) -> u64 {
        account.snapshot().get(class).current()
    }

    #[test]
    fn received_datagram_holds_charge_and_reports_pressure() {
        let runtime = ResourceAccount::default();
        let sockets = Arc::new(DatagramTable {
            entries: Mutex::new(HashMap::new()),
            payloads: TransportPayloadBudget::for_test(runtime.clone(), 1, 3),
        });
        let from = "127.0.0.1:1234".parse().expect("socket address");

        let admitted = received_datagram(&sockets, 7, b"abc", from);
        assert!(matches!(
            admitted,
            Datagram::Message {
                id: 7,
                ref payload,
                ..
            } if payload.as_slice() == b"abc"
        ));
        assert_eq!(current(&runtime, ResourceClass::QueuedMessages), 1);
        assert_eq!(current(&runtime, ResourceClass::QueuedMessageBytes), 3);

        assert!(matches!(
            received_datagram(&sockets, 7, b"x", from),
            Datagram::ReadFailed {
                id: 7,
                code: "ENOBUFS"
            }
        ));
        assert_eq!(current(&runtime, ResourceClass::QueuedMessages), 1);
        assert_eq!(current(&runtime, ResourceClass::QueuedMessageBytes), 3);

        drop(admitted);
        assert_eq!(current(&runtime, ResourceClass::QueuedMessages), 0);
        assert_eq!(current(&runtime, ResourceClass::QueuedMessageBytes), 0);
    }
}
