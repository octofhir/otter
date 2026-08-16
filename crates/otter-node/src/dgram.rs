//! Native half of `node:dgram`: UDP sockets.
//!
//! # Contents
//! - [`dgram_cjs_value`] installs the module's JavaScript shim.
//! - A socket table owned by the natives, keyed by the handle the shim holds.
//! - A receive loop per bound socket, delivering datagrams onto the isolate
//!   thread.
//!
//! # Invariants
//! - Binding and sending are network operations and are gated by the `net`
//!   capability, checked against the address involved.
//! - The receive loop runs on the host's IO runtime and never touches VM
//!   state; it hands owned bytes to a task that re-enters JavaScript on the
//!   isolate thread.
//! - A socket holds the runtime open while it is bound, so a program waiting
//!   for a datagram does not exit early.
//!
//! # See also
//! - `dgram.js`

use std::collections::HashMap;
use std::net::ToSocketAddrs;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use otter_runtime::{
    CapabilitySet, OtterError, Runtime, RuntimeExecutionContext, RuntimeKeepAlive, RuntimeLiveness,
    RuntimeLocal, RuntimeNativeCtx, RuntimeNativeError, RuntimeNativeScope, RuntimeTask,
    RuntimeTaskSpawner, RuntimeValue, runtime_type_error,
};

/// One live socket: the sending half, plus whatever keeps the loop alive.
struct SocketEntry {
    socket: Arc<tokio::net::UdpSocket>,
    keep_alive: Option<RuntimeKeepAlive>,
}

type SocketTable = Arc<Mutex<HashMap<u32, SocketEntry>>>;

/// Build the CommonJS export of `node:dgram`.
///
/// # Errors
/// Returns a native error when the shim fails to allocate or evaluate.
pub fn dgram_cjs_value<'scope>(
    scope: &mut RuntimeNativeScope<'scope, '_>,
    capabilities: &CapabilitySet,
    runtime_task_spawner: Option<RuntimeTaskSpawner>,
    module: RuntimeLocal<'scope>,
    require: RuntimeLocal<'scope>,
) -> Result<RuntimeLocal<'scope>, RuntimeNativeError> {
    let native = build_native(scope, capabilities, runtime_task_spawner)?;
    let globals = scope.global_this();
    scope.set(globals, "__otterDgramNative", native)?;
    otter_runtime::run_builtin_cjs_shim(
        scope,
        "node:dgram",
        include_str!("dgram.js"),
        module,
        require,
    )
}

fn build_native<'scope>(
    scope: &mut RuntimeNativeScope<'scope, '_>,
    capabilities: &CapabilitySet,
    spawner: Option<RuntimeTaskSpawner>,
) -> Result<RuntimeLocal<'scope>, RuntimeNativeError> {
    let object = scope.object()?;
    let sockets: SocketTable = Arc::new(Mutex::new(HashMap::new()));
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
    Ok(object)
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
        let address = scope.string(&target.ip().to_string())?;
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
        _ => "EINVAL",
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
    let target = resolve_target(&address, port, kind != "udp6")
        .ok_or_else(|| option_error("bind", "ENOTFOUND", 0))?;
    let bound = std::net::UdpSocket::bind(target)
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
    sockets
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(
            id,
            SocketEntry {
                socket: socket.clone(),
                keep_alive: Some(keep_alive),
            },
        );

    let delivery_spawner = spawner.clone();
    let delivery_sockets = sockets.clone();
    io.spawn(async move {
        let mut buffer = vec![0u8; 65_536];
        loop {
            let received = socket.recv_from(&mut buffer).await;
            let still_open = delivery_sockets
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .contains_key(&id);
            if !still_open {
                return;
            }
            match received {
                Ok((length, from)) => {
                    let datagram = Datagram {
                        id,
                        payload: buffer[..length].to_vec(),
                        address: from.ip().to_string(),
                        port: from.port(),
                        family: if from.is_ipv4() { "IPv4" } else { "IPv6" },
                    };
                    if delivery_spawner
                        .enqueue(datagram, RuntimeLiveness::Unref)
                        .is_err()
                    {
                        return;
                    }
                }
                Err(_) => return,
            }
        }
    });

    ctx.scope(|mut scope| {
        let result = scope.object()?;
        let handle = scope.number(f64::from(id));
        scope.set(result, "handle", handle)?;
        let address = scope.string(&local.ip().to_string())?;
        scope.set(result, "address", address)?;
        let port = scope.number(f64::from(local.port()));
        scope.set(result, "port", port)?;
        let family = scope.string(if local.is_ipv4() { "IPv4" } else { "IPv6" })?;
        scope.set(result, "family", family)?;
        Ok(scope.finish(result))
    })
}

/// One received datagram, handed to the isolate thread.
struct Datagram {
    id: u32,
    payload: Vec<u8>,
    address: String,
    port: u16,
    family: &'static str,
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
            let id = scope.number(f64::from(datagram.id));
            let payload = scope.string(&bytes_to_latin1(&datagram.payload))?;
            let address = scope.string(&datagram.address)?;
            let port = scope.number(f64::from(datagram.port));
            let family = scope.string(datagram.family)?;
            let undefined = scope.undefined();
            let result = scope.call(dispatch, undefined, &[id, payload, address, port, family])?;
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
    // is what Node documents `send` to default to.
    let address = if address.is_empty() {
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
        let address = scope.string(&local.ip().to_string())?;
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
