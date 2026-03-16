//! Native `node:net` extension.
//!
//! This implementation keeps the hot transport path in Rust: listener bind,
//! accept loop, connect, EOF detection, and ref/unref handle tracking are all
//! native. JS work is limited to EventEmitter delivery and surface object state.

use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, SocketAddr, TcpListener as StdTcpListener, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use otter_macros::{js_class, js_constructor, js_method};
use otter_vm_core::context::{JsJobQueueTrait, NativeContext};
use otter_vm_core::error::VmError;
use otter_vm_core::gc::GcRef;
use otter_vm_core::memory::MemoryManager;
use otter_vm_core::object::{JsObject, PropertyKey};
use otter_vm_core::promise::{JsPromiseJob, JsPromiseJobKind};
use otter_vm_core::string::JsString;
use otter_vm_core::value::{Symbol, Value};
use otter_vm_runtime::extension_v2::{OtterExtension, Profile};
use otter_vm_runtime::registration::RegistrationContext;
use tokio::io::AsyncReadExt;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;

const NORMALIZED_ARGS_SYMBOL_GLOBAL: &str = "__NetNormalizedArgsSymbol";
const SOCKET_CTOR_GLOBAL: &str = "__NetSocketCtor";
const SERVER_CTOR_GLOBAL: &str = "__NetServerCtor";
const SOCKET_ID_KEY: &str = "__net_socket_id";
const SERVER_ID_KEY: &str = "__net_server_id";
const SOCKET_CONNECTED_KEY: &str = "__net_socket_connected";
const SOCKET_CONNECTING_KEY: &str = "__net_socket_connecting";
const SOCKET_DESTROYED_KEY: &str = "__net_socket_destroyed";
const SOCKET_REFED_KEY: &str = "__net_socket_refed";
const SERVER_LISTENING_KEY: &str = "__net_server_listening";
const SERVER_CLOSING_KEY: &str = "__net_server_closing";
const SERVER_REFED_KEY: &str = "__net_server_refed";
const SERVER_PORT_KEY: &str = "__net_server_port";
const SERVER_HOST_KEY: &str = "__net_server_host";
const SERVER_FAMILY_KEY: &str = "__net_server_family";
const SERVER_PATH_KEY: &str = "__net_server_path";
const SERVER_CONN_KEY: &str = "_connectionKey";
const DEFAULT_AUTO_SELECT_TIMEOUT_MS: u64 = 250;

static AUTO_SELECT_TIMEOUT: AtomicU64 = AtomicU64::new(DEFAULT_AUTO_SELECT_TIMEOUT_MS);
static AUTO_SELECT_FAMILY: AtomicBool = AtomicBool::new(true);
static NEXT_SYMBOL_ID: AtomicU64 = AtomicU64::new(10_000);
static NEXT_SERVER_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_SOCKET_ID: AtomicU64 = AtomicU64::new(1);
static NET_STATE: OnceLock<Mutex<NetState>> = OnceLock::new();

#[derive(Default)]
struct NetState {
    servers: HashMap<u64, Arc<ServerRuntime>>,
    sockets: HashMap<u64, Arc<SocketRuntime>>,
}

struct NetHandleRef {
    counter: Option<Arc<AtomicU64>>,
    active: AtomicBool,
    refed: AtomicBool,
}

impl NetHandleRef {
    fn new(counter: Option<Arc<AtomicU64>>) -> Self {
        if let Some(counter) = &counter {
            counter.fetch_add(1, Ordering::Relaxed);
        }
        Self {
            counter,
            active: AtomicBool::new(true),
            refed: AtomicBool::new(true),
        }
    }

    fn ref_handle(&self) {
        if !self.active.load(Ordering::Acquire) {
            return;
        }
        if !self.refed.swap(true, Ordering::AcqRel) && let Some(counter) = &self.counter {
            counter.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn unref_handle(&self) {
        if !self.active.load(Ordering::Acquire) {
            return;
        }
        if self.refed.swap(false, Ordering::AcqRel) && let Some(counter) = &self.counter {
            counter.fetch_sub(1, Ordering::Relaxed);
        }
    }

    fn deactivate(&self) {
        if self.active.swap(false, Ordering::AcqRel) && self.refed.load(Ordering::Acquire)
            && let Some(counter) = &self.counter
        {
            counter.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

struct ServerRuntime {
    handle_ref: NetHandleRef,
    shutdown_tx: Mutex<Option<oneshot::Sender<()>>>,
    accept_stopped: AtomicBool,
    close_emitted: AtomicBool,
    active_connections: AtomicU64,
    server_value: Value,
    queue: Arc<dyn JsJobQueueTrait + Send + Sync>,
    memory_manager: Arc<MemoryManager>,
}

impl ServerRuntime {
    fn new(
        pending_ops: Option<Arc<AtomicU64>>,
        shutdown_tx: oneshot::Sender<()>,
        server_value: Value,
        queue: Arc<dyn JsJobQueueTrait + Send + Sync>,
        memory_manager: Arc<MemoryManager>,
    ) -> Self {
        Self {
            handle_ref: NetHandleRef::new(pending_ops),
            shutdown_tx: Mutex::new(Some(shutdown_tx)),
            accept_stopped: AtomicBool::new(false),
            close_emitted: AtomicBool::new(false),
            active_connections: AtomicU64::new(0),
            server_value,
            queue,
            memory_manager,
        }
    }

    fn emit(&self, event_name: &'static str, args: Vec<Value>) {
        enqueue_emit(
            &self.queue,
            &self.memory_manager,
            self.server_value.clone(),
            event_name,
            args,
        );
    }
}

struct SocketRuntime {
    handle_ref: NetHandleRef,
    writer: Mutex<Option<OwnedWriteHalf>>,
    close_emitted: AtomicBool,
    owner_server_id: Option<u64>,
}

impl SocketRuntime {
    fn new(pending_ops: Option<Arc<AtomicU64>>, owner_server_id: Option<u64>) -> Self {
        Self {
            handle_ref: NetHandleRef::new(pending_ops),
            writer: Mutex::new(None),
            close_emitted: AtomicBool::new(false),
            owner_server_id,
        }
    }

    fn take_writer(&self) -> Option<OwnedWriteHalf> {
        self.writer.lock().ok().and_then(|mut guard| guard.take())
    }

    fn replace_writer(&self, writer: OwnedWriteHalf) {
        if let Ok(mut guard) = self.writer.lock() {
            *guard = Some(writer);
        }
    }

    fn mark_close_emitted(&self) -> bool {
        !self.close_emitted.swap(true, Ordering::AcqRel)
    }
}

struct ConnectedStream {
    stream: TcpStream,
    local_addr: SocketAddr,
    remote_addr: SocketAddr,
}

struct ListenBinding {
    listener: StdTcpListener,
    local_addr: SocketAddr,
}

fn net_state() -> &'static Mutex<NetState> {
    NET_STATE.get_or_init(|| {
        Mutex::new(NetState {
            servers: HashMap::new(),
            sockets: HashMap::new(),
        })
    })
}

fn get_server_runtime(server_id: u64) -> Option<Arc<ServerRuntime>> {
    net_state()
        .lock()
        .ok()
        .and_then(|state| state.servers.get(&server_id).cloned())
}

fn insert_server_runtime(server_id: u64, runtime: Arc<ServerRuntime>) {
    if let Ok(mut state) = net_state().lock() {
        state.servers.insert(server_id, runtime);
    }
}

fn remove_server_runtime(server_id: u64) -> Option<Arc<ServerRuntime>> {
    net_state()
        .lock()
        .ok()
        .and_then(|mut state| state.servers.remove(&server_id))
}

fn get_socket_runtime(socket_id: u64) -> Option<Arc<SocketRuntime>> {
    net_state()
        .lock()
        .ok()
        .and_then(|state| state.sockets.get(&socket_id).cloned())
}

fn insert_socket_runtime(socket_id: u64, runtime: Arc<SocketRuntime>) {
    if let Ok(mut state) = net_state().lock() {
        state.sockets.insert(socket_id, runtime);
    }
}

fn remove_socket_runtime(socket_id: u64) -> Option<Arc<SocketRuntime>> {
    net_state()
        .lock()
        .ok()
        .and_then(|mut state| state.sockets.remove(&socket_id))
}

pub struct NodeNetExtension;

impl OtterExtension for NodeNetExtension {
    fn name(&self) -> &str {
        "node_net"
    }

    fn profiles(&self) -> &[Profile] {
        static P: [Profile; 1] = [Profile::Full];
        &P
    }

    fn deps(&self) -> &[&str] {
        &["node_events"]
    }

    fn module_specifiers(&self) -> &[&str] {
        static S: [&str; 4] = ["node:net", "net", "node:internal/net", "internal/net"];
        &S
    }

    fn install(&self, ctx: &mut RegistrationContext) -> Result<(), VmError> {
        let normalized_args_symbol = Value::symbol(GcRef::new(Symbol {
            description: Some("normalizedArgs".to_string()),
            id: NEXT_SYMBOL_ID.fetch_add(1, Ordering::Relaxed),
        }));
        ctx.global_value(NORMALIZED_ARGS_SYMBOL_GLOBAL, normalized_args_symbol);

        let emitter_ctor = ctx
            .global()
            .get(&PropertyKey::string("__EventEmitter"))
            .ok_or_else(|| VmError::type_error("node:net requires node:events"))?;
        let emitter_proto = emitter_ctor
            .as_object()
            .and_then(|o| o.get(&PropertyKey::string("prototype")))
            .and_then(|v| v.as_object())
            .ok_or_else(|| VmError::type_error("node:net requires EventEmitter.prototype"))?;

        let socket_ctor = build_socket_class(ctx, emitter_proto);
        let server_ctor = build_server_class(ctx, emitter_proto);
        ctx.global_value(SOCKET_CTOR_GLOBAL, socket_ctor);
        ctx.global_value(SERVER_CTOR_GLOBAL, server_ctor);
        Ok(())
    }

    fn load_module(
        &self,
        specifier: &str,
        ctx: &mut RegistrationContext,
    ) -> Option<GcRef<JsObject>> {
        if specifier == "node:internal/net" || specifier == "internal/net" {
            let normalized_args_symbol = ctx.global().get(&PropertyKey::string(
                NORMALIZED_ARGS_SYMBOL_GLOBAL,
            ))?;
            let ns = ctx
                .module_namespace()
                .property("normalizedArgsSymbol", normalized_args_symbol)
                .function("isLoopback", Arc::new(internal_is_loopback), 1)
                .build();
            return Some(ns);
        }

        let socket_ctor = ctx.global().get(&PropertyKey::string(SOCKET_CTOR_GLOBAL))?;
        let server_ctor = ctx.global().get(&PropertyKey::string(SERVER_CTOR_GLOBAL))?;

        let ns = ctx
            .module_namespace()
            .property("Socket", socket_ctor)
            .property("Server", server_ctor)
            .function(
                "setDefaultAutoSelectFamilyAttemptTimeout",
                Arc::new(NetModule::set_default_auto_select_family_attempt_timeout),
                1,
            )
            .function(
                "getDefaultAutoSelectFamilyAttemptTimeout",
                Arc::new(NetModule::get_default_auto_select_family_attempt_timeout),
                0,
            )
            .function(
                "setDefaultAutoSelectFamily",
                Arc::new(NetModule::set_default_auto_select_family),
                1,
            )
            .function(
                "getDefaultAutoSelectFamily",
                Arc::new(NetModule::get_default_auto_select_family),
                0,
            )
            .function("createServer", Arc::new(NetModule::create_server), 2)
            .function("createConnection", Arc::new(NetModule::create_connection), 0)
            .function("connect", Arc::new(NetModule::create_connection), 0)
            .function("_normalizeArgs", Arc::new(NetModule::normalize_args), 1)
            .function("isIP", Arc::new(NetModule::is_ip), 1)
            .function("isIPv4", Arc::new(NetModule::is_ipv4), 1)
            .function("isIPv6", Arc::new(NetModule::is_ipv6), 1)
            .build();

        Some(ns)
    }
}

pub fn node_net_extension() -> Box<dyn OtterExtension> {
    Box::new(NodeNetExtension)
}

#[js_class(name = "Socket")]
pub struct Socket;

#[js_class]
impl Socket {
    #[js_constructor(name = "Socket", length = 1)]
    pub fn constructor(
        this: &Value,
        args: &[Value],
        ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        if !ncx.is_construct() {
            let ctor = ncx
                .global()
                .get(&PropertyKey::string(SOCKET_CTOR_GLOBAL))
                .ok_or_else(|| VmError::type_error("Socket constructor unavailable"))?;
            return ncx.call_function_construct(&ctor, Value::undefined(), args);
        }

        let options = args.first().and_then(|v| v.as_object());
        if let Some(options) = options
            && let Some(fd) = options.get(&PropertyKey::string("fd"))
        {
            if let Some(n) = fd.as_number() {
                if !n.is_finite() || n.fract() != 0.0 || n < 0.0 {
                    return throw_node_error(
                        ncx,
                        "RangeError",
                        "The value of \"options.fd\" is out of range. It must be >= 0.",
                        Some("ERR_OUT_OF_RANGE"),
                    );
                }
            } else {
                return throw_node_error(
                    ncx,
                    "TypeError",
                    "The \"options.fd\" property must be of type number",
                    Some("ERR_INVALID_ARG_TYPE"),
                );
            }
        }

        init_event_emitter(this, ncx)?;
        let socket_id = NEXT_SOCKET_ID.fetch_add(1, Ordering::Relaxed);
        let obj = this
            .as_object()
            .ok_or_else(|| VmError::type_error("Socket constructor requires object receiver"))?;
        let readable = options
            .and_then(|o| o.get(&PropertyKey::string("readable")))
            .map(|v| v.to_boolean())
            .unwrap_or(true);
        let writable = options
            .and_then(|o| o.get(&PropertyKey::string("writable")))
            .map(|v| v.to_boolean())
            .unwrap_or(true);

        let _ = obj.set(PropertyKey::string(SOCKET_ID_KEY), Value::number(socket_id as f64));
        let _ = obj.set(PropertyKey::string(SOCKET_CONNECTED_KEY), Value::boolean(false));
        let _ = obj.set(PropertyKey::string(SOCKET_CONNECTING_KEY), Value::boolean(false));
        let _ = obj.set(PropertyKey::string(SOCKET_DESTROYED_KEY), Value::boolean(false));
        let _ = obj.set(PropertyKey::string(SOCKET_REFED_KEY), Value::boolean(true));
        let _ = obj.set(PropertyKey::string("bytesWritten"), Value::number(0.0));
        let _ = obj.set(PropertyKey::string("bytesRead"), Value::number(0.0));
        let _ = obj.set(PropertyKey::string("bufferSize"), Value::number(0.0));
        let _ = obj.set(PropertyKey::string("_handle"), Value::null());
        let _ = obj.set(PropertyKey::string("remoteAddress"), Value::undefined());
        let _ = obj.set(PropertyKey::string("remotePort"), Value::undefined());
        let _ = obj.set(PropertyKey::string("localAddress"), Value::undefined());
        let _ = obj.set(PropertyKey::string("localPort"), Value::undefined());
        let _ = obj.set(PropertyKey::string("readable"), Value::boolean(readable));
        let _ = obj.set(PropertyKey::string("writable"), Value::boolean(writable));
        Ok(Value::undefined())
    }

    #[js_method(name = "connect", length = 0)]
    pub fn connect(
        this: &Value,
        args: &[Value],
        ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        let (options, callback) = normalize_connection_args(args, ncx)?;
        validate_connect_options(&options, ncx)?;

        if let Some(cb) = callback {
            add_once_listener(this, "connect", cb, ncx)?;
        }

        let target = extract_connect_target(&options, ncx)?;
        let handle = tokio::runtime::Handle::try_current().map_err(|_| {
            VmError::exception(create_node_error_value(
                ncx,
                "Error",
                "No async runtime available for socket.connect()",
                None,
            ))
        })?;
        let socket_id = socket_id_of(this)?;
        let js_queue = ncx
            .js_job_queue()
            .ok_or_else(|| VmError::type_error("No JS job queue available for socket.connect"))?;
        let mm = ncx.memory_manager().clone();

        if let Some(obj) = this.as_object() {
            let _ = obj.set(PropertyKey::string(SOCKET_CONNECTING_KEY), Value::boolean(true));
            let _ = obj.set(PropertyKey::string(SOCKET_CONNECTED_KEY), Value::boolean(false));
            let _ = obj.set(PropertyKey::string(SOCKET_DESTROYED_KEY), Value::boolean(false));
        }

        let runtime = Arc::new(SocketRuntime::new(ncx.pending_async_ops(), None));
        insert_socket_runtime(socket_id, Arc::clone(&runtime));

        let completion_slot: Arc<Mutex<Option<Result<ConnectedStream, String>>>> =
            Arc::new(Mutex::new(None));
        let completion_slot_for_task = Arc::clone(&completion_slot);
        let completion_callback = make_connect_completion_callback(
            mm.clone(),
            this.clone(),
            socket_id,
            Arc::clone(&runtime),
            Arc::clone(&completion_slot),
        );

        handle.spawn(async move {
            let outcome = connect_stream(target).await.map_err(|err| err.to_string());
            if let Ok(mut guard) = completion_slot_for_task.lock() {
                *guard = Some(outcome);
            }
            enqueue_callback(&js_queue, completion_callback, Vec::new());
        });

        Ok(this.clone())
    }

    #[js_method(name = "destroy", length = 0)]
    pub fn destroy(
        this: &Value,
        _args: &[Value],
        ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        let socket_id = socket_id_of(this)?;
        if let Some(runtime) = remove_socket_runtime(socket_id) {
            let _ = runtime.take_writer();
            runtime.handle_ref.deactivate();
            finish_owned_server_connection(runtime.owner_server_id);
            if runtime.mark_close_emitted() {
                apply_socket_closed_state(this);
                schedule_emit(this.clone(), "close", Vec::new(), ncx);
            }
        } else {
            apply_socket_closed_state(this);
            schedule_emit(this.clone(), "close", Vec::new(), ncx);
        }
        Ok(this.clone())
    }

    #[js_method(name = "write", length = 1)]
    pub fn write(
        this: &Value,
        args: &[Value],
        ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        let callback = args.iter().rev().find(|v| v.is_callable()).cloned();
        if socket_id_of(this)
            .ok()
            .and_then(get_socket_runtime)
            .is_none()
        {
            if let Some(cb) = callback {
                let err = create_node_error_value(
                    ncx,
                    "Error",
                    "This socket is not connected",
                    Some("ERR_SOCKET_CLOSED"),
                );
                schedule_callback(cb, vec![err], ncx);
            }
            return Ok(Value::boolean(false));
        }

        if let Some(cb) = callback {
            schedule_callback(cb, vec![Value::null()], ncx);
        }
        Ok(Value::boolean(true))
    }

    #[js_method(name = "end", length = 0)]
    pub fn end(this: &Value, _args: &[Value], ncx: &mut NativeContext) -> Result<Value, VmError> {
        let socket_id = socket_id_of(this)?;
        if let Some(runtime) = get_socket_runtime(socket_id) {
            if let Some(writer) = runtime.take_writer() {
                if let Ok(handle) = tokio::runtime::Handle::try_current() {
                    handle.spawn(async move {
                        let mut writer = writer;
                        let _ = tokio::io::AsyncWriteExt::shutdown(&mut writer).await;
                    });
                }
            }
        } else {
            apply_socket_closed_state(this);
            schedule_emit(this.clone(), "end", Vec::new(), ncx);
            schedule_emit(this.clone(), "close", Vec::new(), ncx);
        }
        Ok(this.clone())
    }

    #[js_method(name = "setNoDelay", length = 0)]
    pub fn set_no_delay(
        this: &Value,
        _args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        Ok(this.clone())
    }

    #[js_method(name = "setKeepAlive", length = 0)]
    pub fn set_keep_alive(
        this: &Value,
        _args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        Ok(this.clone())
    }

    #[js_method(name = "pause", length = 0)]
    pub fn pause(
        this: &Value,
        _args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        Ok(this.clone())
    }

    #[js_method(name = "resume", length = 0)]
    pub fn resume(
        this: &Value,
        _args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        Ok(this.clone())
    }

    #[js_method(name = "setEncoding", length = 1)]
    pub fn set_encoding(
        this: &Value,
        _args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        Ok(this.clone())
    }

    #[js_method(name = "address", length = 0)]
    pub fn address(
        this: &Value,
        _args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        let Some(obj) = this.as_object() else {
            return Ok(Value::null());
        };
        let Some(address) = obj.get(&PropertyKey::string("localAddress")) else {
            return Ok(Value::null());
        };
        if address.is_undefined() {
            return Ok(Value::null());
        }
        let out = GcRef::new(JsObject::new(Value::null()));
        let _ = out.set(PropertyKey::string("address"), address);
        if let Some(port) = obj.get(&PropertyKey::string("localPort")) {
            let _ = out.set(PropertyKey::string("port"), port);
        }
        let family = obj
            .get(&PropertyKey::string("localAddress"))
            .and_then(|v| v.as_string())
            .map(|s| s.as_str().contains(':'))
            .unwrap_or(false);
        let _ = out.set(
            PropertyKey::string("family"),
            Value::string(JsString::intern(if family { "IPv6" } else { "IPv4" })),
        );
        Ok(Value::object(out))
    }

    #[js_method(name = "cork", length = 0)]
    pub fn cork(
        this: &Value,
        _args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        Ok(this.clone())
    }

    #[js_method(name = "uncork", length = 0)]
    pub fn uncork(
        this: &Value,
        _args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        Ok(this.clone())
    }

    #[js_method(name = "ref", length = 0)]
    pub fn ref_socket(
        this: &Value,
        _args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        if let Ok(socket_id) = socket_id_of(this)
            && let Some(runtime) = get_socket_runtime(socket_id)
        {
            runtime.handle_ref.ref_handle();
        }
        if let Some(obj) = this.as_object() {
            let _ = obj.set(PropertyKey::string(SOCKET_REFED_KEY), Value::boolean(true));
        }
        Ok(this.clone())
    }

    #[js_method(name = "unref", length = 0)]
    pub fn unref_socket(
        this: &Value,
        _args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        if let Ok(socket_id) = socket_id_of(this)
            && let Some(runtime) = get_socket_runtime(socket_id)
        {
            runtime.handle_ref.unref_handle();
        }
        if let Some(obj) = this.as_object() {
            let _ = obj.set(PropertyKey::string(SOCKET_REFED_KEY), Value::boolean(false));
        }
        Ok(this.clone())
    }
}

#[js_class(name = "Server")]
pub struct Server;

#[js_class]
impl Server {
    #[js_constructor(name = "Server", length = 1)]
    pub fn constructor(
        this: &Value,
        args: &[Value],
        ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        if !ncx.is_construct() {
            let ctor = ncx
                .global()
                .get(&PropertyKey::string(SERVER_CTOR_GLOBAL))
                .ok_or_else(|| VmError::type_error("Server constructor unavailable"))?;
            return ncx.call_function_construct(&ctor, Value::undefined(), args);
        }

        init_event_emitter(this, ncx)?;
        let server_id = NEXT_SERVER_ID.fetch_add(1, Ordering::Relaxed);
        let obj = this
            .as_object()
            .ok_or_else(|| VmError::type_error("Server constructor requires object receiver"))?;
        let _ = obj.set(PropertyKey::string(SERVER_ID_KEY), Value::number(server_id as f64));
        let _ = obj.set(PropertyKey::string(SERVER_LISTENING_KEY), Value::boolean(false));
        let _ = obj.set(PropertyKey::string(SERVER_CLOSING_KEY), Value::boolean(false));
        let _ = obj.set(PropertyKey::string(SERVER_REFED_KEY), Value::boolean(true));
        let _ = obj.set(PropertyKey::string("listening"), Value::boolean(false));
        let _ = obj.set(PropertyKey::string(SERVER_PORT_KEY), Value::undefined());
        let _ = obj.set(
            PropertyKey::string(SERVER_HOST_KEY),
            Value::string(JsString::intern("0.0.0.0")),
        );
        let _ = obj.set(
            PropertyKey::string(SERVER_FAMILY_KEY),
            Value::string(JsString::intern("IPv4")),
        );
        let _ = obj.set(PropertyKey::string(SERVER_PATH_KEY), Value::undefined());
        let _ = obj.set(
            PropertyKey::string(SERVER_CONN_KEY),
            Value::string(JsString::intern("4:0.0.0.0:0")),
        );

        if let Some(listener) = args.first().filter(|v| v.is_callable()).cloned() {
            add_listener(this, "connection", listener, ncx)?;
        }

        Ok(Value::undefined())
    }

    #[js_method(name = "listen", length = 0)]
    pub fn listen(this: &Value, args: &[Value], ncx: &mut NativeContext) -> Result<Value, VmError> {
        let obj = this
            .as_object()
            .ok_or_else(|| VmError::type_error("Server.listen called on non-object"))?;

        if obj
            .get(&PropertyKey::string(SERVER_LISTENING_KEY))
            .is_some_and(|v| v.to_boolean())
        {
            return throw_node_error(
                ncx,
                "Error",
                "Listen method has been called more than once without closing.",
                Some("ERR_SERVER_ALREADY_LISTEN"),
            );
        }

        let parsed = parse_listen_args(args, ncx)?;
        if parsed.path.is_some() {
            let err = create_node_error_value(
                ncx,
                "Error",
                "Unix sockets are not supported by this runtime yet",
                Some("ERR_INVALID_ARG_VALUE"),
            );
            schedule_emit(this.clone(), "error", vec![err], ncx);
            return Ok(this.clone());
        }

        if let Some(cb) = parsed.callback.clone() {
            add_once_listener(this, "listening", cb, ncx)?;
        }

        let binding = match bind_listener(&parsed, ncx) {
            Ok(binding) => binding,
            Err(err) => {
                schedule_emit(this.clone(), "error", vec![err], ncx);
                return Ok(this.clone());
            }
        };

        let handle = tokio::runtime::Handle::try_current().map_err(|_| {
            VmError::exception(create_node_error_value(
                ncx,
                "Error",
                "No async runtime available for server.listen()",
                None,
            ))
        })?;
        let server_id = server_id_of(this)?;
        let js_queue = ncx
            .js_job_queue()
            .ok_or_else(|| VmError::type_error("No JS job queue available for server.listen"))?;
        let mm = ncx.memory_manager().clone();
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let runtime = Arc::new(ServerRuntime::new(
            ncx.pending_async_ops(),
            shutdown_tx,
            this.clone(),
            Arc::clone(&js_queue),
            mm.clone(),
        ));
        insert_server_runtime(server_id, Arc::clone(&runtime));

        let listener = TcpListener::from_std(binding.listener).map_err(|e| {
            VmError::exception(create_node_error_value(
                ncx,
                "Error",
                &format!("Failed to create async listener: {e}"),
                None,
            ))
        })?;

        apply_server_listening_state(this, binding.local_addr, parsed.path.as_deref());
        schedule_if_listening(this.clone(), "listening", ncx);

        handle.spawn(run_accept_loop(
            server_id,
            listener,
            runtime,
            shutdown_rx,
        ));

        Ok(this.clone())
    }

    #[js_method(name = "close", length = 0)]
    pub fn close(this: &Value, args: &[Value], ncx: &mut NativeContext) -> Result<Value, VmError> {
        if let Some(cb) = args.first().filter(|v| v.is_callable()).cloned() {
            add_once_listener(this, "close", cb, ncx)?;
        }

        let obj = this
            .as_object()
            .ok_or_else(|| VmError::type_error("Server.close called on non-object"))?;
        let server_id = server_id_of(this)?;

        let _ = obj.set(PropertyKey::string(SERVER_LISTENING_KEY), Value::boolean(false));
        let _ = obj.set(PropertyKey::string("listening"), Value::boolean(false));
        let _ = obj.set(PropertyKey::string(SERVER_CLOSING_KEY), Value::boolean(true));

        if let Some(runtime) = get_server_runtime(server_id) {
            if let Ok(mut guard) = runtime.shutdown_tx.lock()
                && let Some(tx) = guard.take()
            {
                let _ = tx.send(());
            }
            maybe_finish_server_close(server_id);
        }

        Ok(this.clone())
    }

    #[js_method(name = "address", length = 0)]
    pub fn address(
        this: &Value,
        _args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        let Some(obj) = this.as_object() else {
            return Ok(Value::null());
        };

        if !obj
            .get(&PropertyKey::string(SERVER_LISTENING_KEY))
            .is_some_and(|v| v.to_boolean())
        {
            return Ok(Value::null());
        }

        let address = GcRef::new(JsObject::new(Value::null()));
        if let Some(host) = obj.get(&PropertyKey::string(SERVER_HOST_KEY)) {
            let _ = address.set(PropertyKey::string("address"), host);
        }
        if let Some(port) = obj.get(&PropertyKey::string(SERVER_PORT_KEY)) {
            let _ = address.set(PropertyKey::string("port"), port);
        }
        if let Some(family) = obj.get(&PropertyKey::string(SERVER_FAMILY_KEY)) {
            let _ = address.set(PropertyKey::string("family"), family);
        }

        Ok(Value::object(address))
    }

    #[js_method(name = "ref", length = 0)]
    pub fn ref_server(
        this: &Value,
        _args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        if let Ok(server_id) = server_id_of(this)
            && let Some(runtime) = get_server_runtime(server_id)
        {
            runtime.handle_ref.ref_handle();
        }
        if let Some(obj) = this.as_object() {
            let _ = obj.set(PropertyKey::string(SERVER_REFED_KEY), Value::boolean(true));
        }
        Ok(this.clone())
    }

    #[js_method(name = "unref", length = 0)]
    pub fn unref_server(
        this: &Value,
        _args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        if let Ok(server_id) = server_id_of(this)
            && let Some(runtime) = get_server_runtime(server_id)
        {
            runtime.handle_ref.unref_handle();
        }
        if let Some(obj) = this.as_object() {
            let _ = obj.set(PropertyKey::string(SERVER_REFED_KEY), Value::boolean(false));
        }
        Ok(this.clone())
    }
}

struct NetModule;

impl NetModule {
    fn set_default_auto_select_family_attempt_timeout(
        _this: &Value,
        args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        if let Some(ms) = args.first().and_then(|v| v.as_number()) {
            AUTO_SELECT_TIMEOUT.store(ms as u64, Ordering::Relaxed);
        }
        Ok(Value::undefined())
    }

    fn get_default_auto_select_family_attempt_timeout(
        _this: &Value,
        _args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        Ok(Value::number(
            AUTO_SELECT_TIMEOUT.load(Ordering::Relaxed) as f64,
        ))
    }

    fn set_default_auto_select_family(
        _this: &Value,
        args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        if let Some(flag) = args.first() {
            AUTO_SELECT_FAMILY.store(flag.to_boolean(), Ordering::Relaxed);
        }
        Ok(Value::undefined())
    }

    fn get_default_auto_select_family(
        _this: &Value,
        _args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        Ok(Value::boolean(AUTO_SELECT_FAMILY.load(Ordering::Relaxed)))
    }

    fn create_server(this: &Value, args: &[Value], ncx: &mut NativeContext) -> Result<Value, VmError> {
        let ctor = ncx
            .global()
            .get(&PropertyKey::string(SERVER_CTOR_GLOBAL))
            .ok_or_else(|| VmError::type_error("Server constructor unavailable"))?;
        ncx.call_function_construct(&ctor, this.clone(), args)
    }

    fn create_connection(
        _this: &Value,
        args: &[Value],
        ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        let ctor = ncx
            .global()
            .get(&PropertyKey::string(SOCKET_CTOR_GLOBAL))
            .ok_or_else(|| VmError::type_error("Socket constructor unavailable"))?;
        let socket = ncx.call_function_construct(&ctor, Value::undefined(), &[])?;
        let connect = socket
            .as_object()
            .and_then(|o| o.get(&PropertyKey::string("connect")))
            .ok_or_else(|| VmError::type_error("Socket.connect unavailable"))?;
        ncx.call_function(&connect, socket.clone(), args)?;
        Ok(socket)
    }

    fn normalize_args(
        _this: &Value,
        args: &[Value],
        ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        let input = args
            .first()
            .and_then(|v| v.as_object())
            .ok_or_else(|| VmError::type_error("_normalizeArgs expects an array"))?;
        let len = input.array_length();
        let mut values = Vec::with_capacity(len);
        for i in 0..len {
            values.push(
                input.get(&PropertyKey::Index(i as u32))
                    .unwrap_or_else(Value::undefined),
            );
        }
        build_normalized_args_array(&values, ncx)
    }

    fn is_ip(_this: &Value, args: &[Value], ncx: &mut NativeContext) -> Result<Value, VmError> {
        let text = ip_input_string(args.first(), ncx)?;
        let value = match parse_ip_input(text.as_deref()) {
            Some(IpKind::V4) => 4,
            Some(IpKind::V6) => 6,
            None => 0,
        };
        Ok(Value::int32(value))
    }

    fn is_ipv4(_this: &Value, args: &[Value], ncx: &mut NativeContext) -> Result<Value, VmError> {
        let text = ip_input_string(args.first(), ncx)?;
        Ok(Value::boolean(matches!(
            parse_ip_input(text.as_deref()),
            Some(IpKind::V4)
        )))
    }

    fn is_ipv6(_this: &Value, args: &[Value], ncx: &mut NativeContext) -> Result<Value, VmError> {
        let text = ip_input_string(args.first(), ncx)?;
        Ok(Value::boolean(matches!(
            parse_ip_input(text.as_deref()),
            Some(IpKind::V6)
        )))
    }
}

enum IpKind {
    V4,
    V6,
}

struct ParsedListenArgs {
    port: Option<u16>,
    host: String,
    path: Option<String>,
    callback: Option<Value>,
}

struct ConnectTarget {
    host: String,
    port: u16,
}

fn build_socket_class(ctx: &RegistrationContext, emitter_proto: GcRef<JsObject>) -> Value {
    type DeclFn = fn() -> (
        &'static str,
        Arc<dyn Fn(&Value, &[Value], &mut NativeContext) -> Result<Value, VmError> + Send + Sync>,
        u32,
    );

    let methods: &[DeclFn] = &[
        Socket::connect_decl,
        Socket::destroy_decl,
        Socket::write_decl,
        Socket::end_decl,
        Socket::set_no_delay_decl,
        Socket::set_keep_alive_decl,
        Socket::pause_decl,
        Socket::resume_decl,
        Socket::set_encoding_decl,
        Socket::address_decl,
        Socket::cork_decl,
        Socket::uncork_decl,
        Socket::ref_socket_decl,
        Socket::unref_socket_decl,
    ];

    let mut builder = ctx
        .builtin_fresh("Socket")
        .inherits(emitter_proto)
        .constructor_fn(Socket::constructor, 1);

    for decl in methods {
        let (name, func, length) = decl();
        builder = builder.method_native(name, func, length);
    }

    builder.build()
}

fn build_server_class(ctx: &RegistrationContext, emitter_proto: GcRef<JsObject>) -> Value {
    type DeclFn = fn() -> (
        &'static str,
        Arc<dyn Fn(&Value, &[Value], &mut NativeContext) -> Result<Value, VmError> + Send + Sync>,
        u32,
    );

    let methods: &[DeclFn] = &[
        Server::listen_decl,
        Server::close_decl,
        Server::address_decl,
        Server::ref_server_decl,
        Server::unref_server_decl,
    ];

    let mut builder = ctx
        .builtin_fresh("Server")
        .inherits(emitter_proto)
        .constructor_fn(Server::constructor, 1);

    for decl in methods {
        let (name, func, length) = decl();
        builder = builder.method_native(name, func, length);
    }

    builder.build()
}

fn init_event_emitter(this: &Value, ncx: &mut NativeContext) -> Result<(), VmError> {
    let emitter_ctor = ncx
        .global()
        .get(&PropertyKey::string("__EventEmitter"))
        .ok_or_else(|| VmError::type_error("EventEmitter is not available"))?;
    ncx.call_function(&emitter_ctor, this.clone(), &[])?;
    Ok(())
}

fn normalized_args_symbol(ncx: &NativeContext) -> Result<GcRef<Symbol>, VmError> {
    ncx.global()
        .get(&PropertyKey::string(NORMALIZED_ARGS_SYMBOL_GLOBAL))
        .and_then(|v| v.as_symbol())
        .ok_or_else(|| VmError::type_error("normalizedArgsSymbol not initialized"))
}

fn build_normalized_args_array(args: &[Value], ncx: &mut NativeContext) -> Result<Value, VmError> {
    let arr = GcRef::new(JsObject::array(0));
    let options = GcRef::new(JsObject::new(Value::null()));

    if let Some(first) = args.first() {
        if let Some(obj) = first.as_object() {
            if !obj.is_array() {
                copy_known_connect_options(&options, &first.clone());
            }
        } else if let Some(text) = first.as_string() {
            let _ = options.set(
                PropertyKey::string("path"),
                Value::string(JsString::new_gc(text.as_str())),
            );
        } else if !first.is_undefined() {
            let _ = options.set(PropertyKey::string("port"), first.clone());
            if let Some(host) = args.get(1).and_then(|v| v.as_string()) {
                let _ = options.set(
                    PropertyKey::string("host"),
                    Value::string(JsString::new_gc(host.as_str())),
                );
            }
        }
    }

    let callback = args.last().filter(|v| v.is_callable()).cloned().unwrap_or_else(Value::null);
    arr.array_push(Value::object(options));
    arr.array_push(callback);
    let sym = normalized_args_symbol(ncx)?;
    let _ = arr.set(PropertyKey::Symbol(sym), Value::boolean(true));
    Ok(Value::object(arr))
}

fn normalize_connection_args(
    args: &[Value],
    ncx: &mut NativeContext,
) -> Result<(Value, Option<Value>), VmError> {
    if args.is_empty() {
        return throw_node_error(
            ncx,
            "TypeError",
            "The \"options\" or \"port\" or \"path\" argument must be specified",
            Some("ERR_MISSING_ARGS"),
        );
    }

    if let Some(first_obj) = args.first().and_then(|v| v.as_object()) {
        if first_obj.is_array() {
            let sym = normalized_args_symbol(ncx)?;
            if !first_obj
                .get(&PropertyKey::Symbol(sym))
                .is_some_and(|v| v.to_boolean())
            {
                return throw_node_error(
                    ncx,
                    "TypeError",
                    "The \"options\" or \"port\" or \"path\" argument must be specified",
                    Some("ERR_MISSING_ARGS"),
                );
            }

            let options = first_obj
                .get(&PropertyKey::Index(0))
                .unwrap_or_else(Value::undefined);
            let callback = first_obj
                .get(&PropertyKey::Index(1))
                .filter(|v| v.is_callable());
            return Ok((options, callback));
        }

        if first_obj
            .get(&PropertyKey::string("port"))
            .is_none()
            && first_obj.get(&PropertyKey::string("path")).is_none()
        {
            return throw_node_error(
                ncx,
                "TypeError",
                "The \"options\" or \"port\" or \"path\" argument must be specified",
                Some("ERR_MISSING_ARGS"),
            );
        }
    }

    let normalized = build_normalized_args_array(args, ncx)?;
    let normalized_obj = normalized
        .as_object()
        .ok_or_else(|| VmError::type_error("normalized args must be an array"))?;
    let options = normalized_obj
        .get(&PropertyKey::Index(0))
        .unwrap_or_else(Value::undefined);
    let callback = normalized_obj
        .get(&PropertyKey::Index(1))
        .filter(|v| v.is_callable());
    Ok((options, callback))
}

fn validate_connect_options(options: &Value, ncx: &mut NativeContext) -> Result<(), VmError> {
    let options_obj = options
        .as_object()
        .ok_or_else(|| VmError::type_error("connect options must be an object"))?;

    for invalid_key in ["objectMode", "readableObjectMode", "writableObjectMode"] {
        if let Some(value) = options_obj.get(&PropertyKey::string(invalid_key))
            && value.to_boolean()
        {
            let received = value_display(&value);
            return throw_node_error(
                ncx,
                "TypeError",
                &format!(
                    "The property 'options.{invalid_key}' is not supported. Received {}",
                    received
                ),
                Some("ERR_INVALID_ARG_VALUE"),
            );
        }
    }

    if let Some(host) = options_obj.get(&PropertyKey::string("host"))
        && !(host.is_undefined() || host.is_null() || host.as_string().is_some())
    {
        return throw_node_error(
            ncx,
            "TypeError",
            "The \"host\" argument must be of type string",
            Some("ERR_INVALID_ARG_TYPE"),
        );
    }

    if let Some(auto_select_family) = options_obj.get(&PropertyKey::string("autoSelectFamily"))
        && !(auto_select_family.is_undefined()
            || auto_select_family.is_null()
            || auto_select_family.is_boolean())
    {
        return throw_node_error(
            ncx,
            "TypeError",
            "The \"options.autoSelectFamily\" property must be of type boolean",
            Some("ERR_INVALID_ARG_TYPE"),
        );
    }

    if options_obj.get(&PropertyKey::string("path")).is_some() {
        return Ok(());
    }

    if let Some(port) = options_obj.get(&PropertyKey::string("port")) {
        validate_port(&port, ncx)?;
        return Ok(());
    }

    throw_node_error(
        ncx,
        "TypeError",
        "The \"options\" or \"port\" or \"path\" argument must be specified",
        Some("ERR_MISSING_ARGS"),
    )
}

fn validate_port(port: &Value, ncx: &mut NativeContext) -> Result<u16, VmError> {
    if port.is_boolean() || port.as_object().is_some() || port.as_symbol().is_some() {
        return throw_node_error(
            ncx,
            "TypeError",
            "The \"port\" argument must be of type number or string",
            Some("ERR_INVALID_ARG_TYPE"),
        );
    }

    if port.is_null() || port.is_undefined() {
        return throw_node_error(
            ncx,
            "TypeError",
            "The \"port\" argument must be of type number or string",
            Some("ERR_INVALID_ARG_TYPE"),
        );
    }

    let value = if let Some(n) = port.as_number() {
        n
    } else if let Some(text) = port.as_string() {
        parse_port_string(text.as_str()).ok_or_else(|| {
            VmError::exception(create_node_error_value(
                ncx,
                "RangeError",
                &format!("Port should be >= 0 and < 65536. Received {}", text.as_str()),
                Some("ERR_SOCKET_BAD_PORT"),
            ))
        })?
    } else {
        return throw_node_error(
            ncx,
            "TypeError",
            "The \"port\" argument must be of type number or string",
            Some("ERR_INVALID_ARG_TYPE"),
        );
    };

    if !value.is_finite() || value.fract() != 0.0 || !(0.0..=65535.0).contains(&value) {
        return throw_node_error(
            ncx,
            "RangeError",
            &format!("Port should be >= 0 and < 65536. Received {value}"),
            Some("ERR_SOCKET_BAD_PORT"),
        );
    }

    Ok(value as u16)
}

fn parse_port_string(text: &str) -> Option<f64> {
    if text.trim().is_empty() {
        return None;
    }

    if let Some(hex) = text.strip_prefix("0x") {
        return u16::from_str_radix(hex, 16).ok().map(|n| n as f64);
    }

    text.parse::<f64>().ok()
}

fn parse_listen_args(args: &[Value], ncx: &mut NativeContext) -> Result<ParsedListenArgs, VmError> {
    let callback = args.iter().rev().find(|v| v.is_callable()).cloned();
    let positional: Vec<Value> = args
        .iter()
        .filter(|v| !v.is_callable())
        .cloned()
        .collect();

    if positional.is_empty() {
        return Ok(ParsedListenArgs {
            port: Some(0),
            host: "0.0.0.0".to_string(),
            path: None,
            callback,
        });
    }

    if let Some(options) = positional.first().and_then(|v| v.as_object()) {
        let has_port = options.get(&PropertyKey::string("port")).is_some();
        let has_path = options.get(&PropertyKey::string("path")).is_some();
        if !has_port && !has_path {
            let received = value_display(&positional[0]);
            return throw_node_error(
                ncx,
                "TypeError",
                &format!(
                    "The argument 'options' must have the property \"port\" or \"path\". Received {}",
                    received
                ),
                Some("ERR_INVALID_ARG_VALUE"),
            );
        }

        let path = options
            .get(&PropertyKey::string("path"))
            .filter(|v| !v.is_undefined())
            .map(|v| {
                if let Some(text) = v.as_string() {
                    Ok(text.as_str().to_string())
                } else {
                    let received = value_display(&positional[0]);
                    Err(VmError::exception(create_node_error_value(
                        ncx,
                        "TypeError",
                        &format!("The argument 'options' is invalid. Received {}", received),
                        Some("ERR_INVALID_ARG_VALUE"),
                    )))
                }
            })
            .transpose()?;

        let port = match options.get(&PropertyKey::string("port")) {
            Some(v) if v.is_undefined() || v.is_null() => Some(0),
            Some(v) if v.is_boolean() || v.as_object().is_some() || v.as_symbol().is_some() => {
                let received = value_display(&positional[0]);
                return throw_node_error(
                    ncx,
                    "TypeError",
                    &format!("The argument 'options' is invalid. Received {}", received),
                    Some("ERR_INVALID_ARG_VALUE"),
                );
            }
            Some(v) => Some(validate_port(&v, ncx)?),
            None => None,
        };

        let host = options
            .get(&PropertyKey::string("host"))
            .and_then(|v| v.as_string())
            .map(|s| s.as_str().to_string())
            .unwrap_or_else(|| "0.0.0.0".to_string());

        return Ok(ParsedListenArgs {
            port,
            host,
            path,
            callback,
        });
    }

    let port = match positional.first() {
        Some(v) if v.is_undefined() || v.is_null() => 0,
        Some(v) => validate_port(v, ncx)?,
        None => 0,
    };
    let host = positional
        .get(1)
        .and_then(|v| v.as_string())
        .map(|s| s.as_str().to_string())
        .unwrap_or_else(|| "0.0.0.0".to_string());

    Ok(ParsedListenArgs {
        port: Some(port),
        host,
        path: None,
        callback,
    })
}

fn extract_connect_target(options: &Value, ncx: &mut NativeContext) -> Result<ConnectTarget, VmError> {
    let options_obj = options
        .as_object()
        .ok_or_else(|| VmError::type_error("connect options must be an object"))?;
    let port = options_obj
        .get(&PropertyKey::string("port"))
        .map(|v| validate_port(&v, ncx))
        .transpose()?
        .unwrap_or(0);

    let host = options_obj
        .get(&PropertyKey::string("host"))
        .and_then(|v| v.as_string())
        .map(|s| s.as_str().to_string())
        .or_else(|| {
            options_obj
                .get(&PropertyKey::string("address"))
                .and_then(|v| v.as_string())
                .map(|s| s.as_str().to_string())
        })
        .unwrap_or_else(|| {
            let prefers_v6 = options_obj
                .get(&PropertyKey::string("family"))
                .and_then(|v| v.as_number())
                .is_some_and(|n| n == 6.0);
            if prefers_v6 {
                "::1".to_string()
            } else {
                "127.0.0.1".to_string()
            }
        });

    Ok(ConnectTarget { host, port })
}

fn bind_listener(parsed: &ParsedListenArgs, ncx: &mut NativeContext) -> Result<ListenBinding, Value> {
    let requested_port = parsed.port.unwrap_or(0);
    let bind_host = normalize_bind_host(&parsed.host);
    let bind_addr = resolve_socket_addr(&bind_host, requested_port).map_err(|err| {
        create_node_error_value(
            ncx,
            "Error",
            &format!("listen {err}: {}:{}", parsed.host, requested_port),
            None,
        )
    })?;

    match StdTcpListener::bind(bind_addr) {
        Ok(listener) => {
            let local_addr = listener.local_addr().map_err(|err| {
                create_node_error_value(
                    ncx,
                    "Error",
                    &format!("listen {err}: {}:{}", parsed.host, requested_port),
                    None,
                )
            })?;
            listener.set_nonblocking(true).map_err(|err| {
                create_node_error_value(
                    ncx,
                    "Error",
                    &format!("listen {err}: {}:{}", parsed.host, requested_port),
                    None,
                )
            })?;
            Ok(ListenBinding {
                listener,
                local_addr,
            })
        }
        Err(err) => {
            let code = if err.kind() == io::ErrorKind::AddrInUse {
                Some("EADDRINUSE")
            } else {
                None
            };
            Err(create_node_error_value(
                ncx,
                "Error",
                &format!("listen {err}: {}:{}", parsed.host, requested_port),
                code,
            ))
        }
    }
}

async fn connect_stream(target: ConnectTarget) -> io::Result<ConnectedStream> {
    let addr = resolve_socket_addr(&normalize_connect_host(&target.host), target.port)?;
    let stream = TcpStream::connect(addr).await?;
    let local_addr = stream.local_addr()?;
    let remote_addr = stream.peer_addr()?;
    Ok(ConnectedStream {
        stream,
        local_addr,
        remote_addr,
    })
}

async fn run_accept_loop(
    server_id: u64,
    listener: TcpListener,
    runtime: Arc<ServerRuntime>,
    mut shutdown_rx: oneshot::Receiver<()>,
) {
    loop {
        tokio::select! {
            _ = &mut shutdown_rx => {
                break;
            }
            outcome = listener.accept() => {
                match outcome {
                    Ok((stream, remote_addr)) => {
                        runtime.active_connections.fetch_add(1, Ordering::Relaxed);
                        let local_addr = match stream.local_addr() {
                            Ok(addr) => addr,
                            Err(_) => remote_addr,
                        };
                        enqueue_accept_event(server_id, &runtime, stream, local_addr, remote_addr);
                    }
                    Err(err) => {
                        if !runtime.accept_stopped.load(Ordering::Acquire) {
                            runtime.emit(
                                "error",
                                vec![create_node_error_from_runtime(
                                    &runtime,
                                    "Error",
                                    &format!("accept {err}"),
                                    None,
                                )],
                            );
                        }
                        break;
                    }
                }
            }
        }
    }

    runtime.accept_stopped.store(true, Ordering::Release);
    runtime.handle_ref.deactivate();
    maybe_finish_server_close(server_id);
}

fn enqueue_accept_event(
    server_id: u64,
    runtime: &Arc<ServerRuntime>,
    stream: TcpStream,
    local_addr: SocketAddr,
    remote_addr: SocketAddr,
) {
    let slot = Arc::new(Mutex::new(Some(ConnectedStream {
        stream,
        local_addr,
        remote_addr,
    })));
    let slot_for_callback = Arc::clone(&slot);
    let server_value = runtime.server_value.clone();
    let mm = runtime.memory_manager.clone();
    let callback = Value::native_function(
        move |_this, _args, ncx| {
            let connected = slot_for_callback
                .lock()
                .ok()
                .and_then(|mut guard| guard.take());
            let Some(connected) = connected else {
                return Ok(Value::undefined());
            };

            let socket = construct_socket_for_server_connection(
                ncx,
                server_id,
                connected,
                server_value.clone(),
            )?;
            emit_now(
                &server_value,
                "connection",
                &[socket],
                ncx,
            )?;
            Ok(Value::undefined())
        },
        mm,
    );
    enqueue_callback(&runtime.queue, callback, Vec::new());
}

fn construct_socket_for_server_connection(
    ncx: &mut NativeContext,
    server_id: u64,
    connected: ConnectedStream,
    server_value: Value,
) -> Result<Value, VmError> {
    let ctor = ncx
        .global()
        .get(&PropertyKey::string(SOCKET_CTOR_GLOBAL))
        .ok_or_else(|| VmError::type_error("Socket constructor unavailable"))?;
    let socket = ncx.call_function_construct(&ctor, Value::undefined(), &[])?;
    let socket_id = socket_id_of(&socket)?;
    let runtime = Arc::new(SocketRuntime::new(ncx.pending_async_ops(), Some(server_id)));
    insert_socket_runtime(socket_id, Arc::clone(&runtime));
    attach_connected_stream(
        ncx,
        socket.clone(),
        socket_id,
        runtime,
        connected,
        Some(server_value),
    )?;
    Ok(socket)
}

fn make_connect_completion_callback(
    mm: Arc<MemoryManager>,
    socket_value: Value,
    socket_id: u64,
    runtime: Arc<SocketRuntime>,
    slot: Arc<Mutex<Option<Result<ConnectedStream, String>>>>,
) -> Value {
    Value::native_function(
        move |_this, _args, ncx| {
            let outcome = slot.lock().ok().and_then(|mut guard| guard.take());
            let Some(outcome) = outcome else {
                return Ok(Value::undefined());
            };

            match outcome {
                Ok(connected) => {
                    attach_connected_stream(
                        ncx,
                        socket_value.clone(),
                        socket_id,
                        Arc::clone(&runtime),
                        connected,
                        None,
                    )?;
                    emit_now(&socket_value, "connect", &[], ncx)?;
                    emit_now(&socket_value, "ready", &[], ncx)?;
                }
                Err(message) => {
                    let _ = remove_socket_runtime(socket_id);
                    runtime.handle_ref.deactivate();
                    apply_socket_closed_state(&socket_value);
                    let err = create_node_error_value(ncx, "Error", &message, None);
                    emit_now(&socket_value, "error", &[err], ncx)?;
                    emit_now(&socket_value, "close", &[], ncx)?;
                }
            }

            Ok(Value::undefined())
        },
        mm,
    )
}

fn attach_connected_stream(
    ncx: &mut NativeContext,
    socket_value: Value,
    socket_id: u64,
    runtime: Arc<SocketRuntime>,
    connected: ConnectedStream,
    server_value: Option<Value>,
) -> Result<(), VmError> {
    let (read_half, write_half) = connected.stream.into_split();
    runtime.replace_writer(write_half);
    apply_socket_connected_state(&socket_value, connected.local_addr, connected.remote_addr);
    spawn_socket_reader(
        ncx,
        socket_value,
        socket_id,
        runtime,
        read_half,
        server_value,
    )?;
    Ok(())
}

fn spawn_socket_reader(
    ncx: &mut NativeContext,
    socket_value: Value,
    socket_id: u64,
    runtime: Arc<SocketRuntime>,
    mut read_half: OwnedReadHalf,
    server_value: Option<Value>,
) -> Result<(), VmError> {
    let handle = tokio::runtime::Handle::try_current().map_err(|_| {
        VmError::exception(create_node_error_value(
            ncx,
            "Error",
            "No async runtime available for socket reader",
            None,
        ))
    })?;
    let queue = ncx
        .js_job_queue()
        .ok_or_else(|| VmError::type_error("No JS job queue available for socket reader"))?;
    let mm = ncx.memory_manager().clone();

    handle.spawn(async move {
        let mut buf = [0_u8; 8192];
        let outcome = loop {
            match read_half.read(&mut buf).await {
                Ok(0) => break Ok(()),
                Ok(_) => continue,
                Err(err) => break Err(err.to_string()),
            }
        };

        let _ = remove_socket_runtime(socket_id);
        runtime.handle_ref.deactivate();
        let emit_close = runtime.mark_close_emitted();
        let owner_server_id = runtime.owner_server_id;
        let socket_for_callback = socket_value.clone();
        let server_for_callback = server_value.clone();
        let callback = Value::native_function(
            move |_this, _args, callback_ncx| {
                apply_socket_closed_state(&socket_for_callback);
                match &outcome {
                    Ok(()) => {
                        emit_now(&socket_for_callback, "end", &[], callback_ncx)?;
                    }
                    Err(message) => {
                        let err = create_node_error_value(callback_ncx, "Error", message, None);
                        emit_now(&socket_for_callback, "error", &[err], callback_ncx)?;
                    }
                }
                if emit_close {
                    emit_now(&socket_for_callback, "close", &[], callback_ncx)?;
                }
                finish_owned_server_connection_with_value(owner_server_id, server_for_callback.clone());
                Ok(Value::undefined())
            },
            mm,
        );
        enqueue_callback(&queue, callback, Vec::new());
    });
    Ok(())
}

fn maybe_finish_server_close(server_id: u64) {
    let Some(runtime) = get_server_runtime(server_id) else {
        return;
    };

    if !runtime.accept_stopped.load(Ordering::Acquire) {
        return;
    }
    if runtime.active_connections.load(Ordering::Acquire) != 0 {
        return;
    }
    if runtime.close_emitted.swap(true, Ordering::AcqRel) {
        return;
    }

    let runtime = remove_server_runtime(server_id).unwrap_or(runtime);
    runtime.emit("close", Vec::new());
}

fn finish_owned_server_connection(server_id: Option<u64>) {
    finish_owned_server_connection_with_value(server_id, None);
}

fn finish_owned_server_connection_with_value(server_id: Option<u64>, server_value: Option<Value>) {
    let Some(server_id) = server_id else {
        return;
    };
    let Some(runtime) = get_server_runtime(server_id) else {
        return;
    };
    runtime.active_connections.fetch_sub(1, Ordering::Relaxed);
    if let Some(server_value) = server_value {
        let _ = server_value;
    }
    maybe_finish_server_close(server_id);
}

fn apply_server_listening_state(server: &Value, addr: SocketAddr, path: Option<&str>) {
    let Some(obj) = server.as_object() else {
        return;
    };
    let family = if addr.is_ipv6() { "IPv6" } else { "IPv4" };
    let connection_key = format!(
        "{}:{}:{}",
        if addr.is_ipv6() { 6 } else { 4 },
        addr.ip(),
        addr.port()
    );
    let _ = obj.set(PropertyKey::string(SERVER_LISTENING_KEY), Value::boolean(true));
    let _ = obj.set(PropertyKey::string("listening"), Value::boolean(true));
    let _ = obj.set(PropertyKey::string(SERVER_CLOSING_KEY), Value::boolean(false));
    let _ = obj.set(PropertyKey::string(SERVER_PORT_KEY), Value::number(addr.port() as f64));
    let _ = obj.set(
        PropertyKey::string(SERVER_HOST_KEY),
        Value::string(JsString::new_gc(&addr.ip().to_string())),
    );
    let _ = obj.set(
        PropertyKey::string(SERVER_FAMILY_KEY),
        Value::string(JsString::intern(family)),
    );
    let _ = obj.set(
        PropertyKey::string(SERVER_CONN_KEY),
        Value::string(JsString::new_gc(&connection_key)),
    );
    if let Some(path) = path {
        let _ = obj.set(
            PropertyKey::string(SERVER_PATH_KEY),
            Value::string(JsString::new_gc(path)),
        );
    } else {
        let _ = obj.set(PropertyKey::string(SERVER_PATH_KEY), Value::undefined());
    }
}

fn apply_socket_connected_state(socket: &Value, local_addr: SocketAddr, remote_addr: SocketAddr) {
    let Some(obj) = socket.as_object() else {
        return;
    };
    let _ = obj.set(PropertyKey::string(SOCKET_CONNECTING_KEY), Value::boolean(false));
    let _ = obj.set(PropertyKey::string(SOCKET_CONNECTED_KEY), Value::boolean(true));
    let _ = obj.set(PropertyKey::string(SOCKET_DESTROYED_KEY), Value::boolean(false));
    let _ = obj.set(PropertyKey::string("_handle"), Value::object(GcRef::new(JsObject::new(Value::null()))));
    let _ = obj.set(
        PropertyKey::string("remoteAddress"),
        Value::string(JsString::new_gc(&remote_addr.ip().to_string())),
    );
    let _ = obj.set(
        PropertyKey::string("remotePort"),
        Value::number(remote_addr.port() as f64),
    );
    let _ = obj.set(
        PropertyKey::string("localAddress"),
        Value::string(JsString::new_gc(&local_addr.ip().to_string())),
    );
    let _ = obj.set(
        PropertyKey::string("localPort"),
        Value::number(local_addr.port() as f64),
    );
}

fn apply_socket_closed_state(socket: &Value) {
    let Some(obj) = socket.as_object() else {
        return;
    };
    let _ = obj.set(PropertyKey::string(SOCKET_CONNECTING_KEY), Value::boolean(false));
    let _ = obj.set(PropertyKey::string(SOCKET_CONNECTED_KEY), Value::boolean(false));
    let _ = obj.set(PropertyKey::string(SOCKET_DESTROYED_KEY), Value::boolean(true));
    let _ = obj.set(PropertyKey::string("_handle"), Value::null());
}

fn socket_id_of(value: &Value) -> Result<u64, VmError> {
    value
        .as_object()
        .and_then(|o| o.get(&PropertyKey::string(SOCKET_ID_KEY)))
        .and_then(|v| v.as_number())
        .map(|n| n as u64)
        .ok_or_else(|| VmError::type_error("Socket object is missing native id"))
}

fn server_id_of(value: &Value) -> Result<u64, VmError> {
    value
        .as_object()
        .and_then(|o| o.get(&PropertyKey::string(SERVER_ID_KEY)))
        .and_then(|v| v.as_number())
        .map(|n| n as u64)
        .ok_or_else(|| VmError::type_error("Server object is missing native id"))
}

fn normalize_bind_host(host: &str) -> String {
    match host {
        "" | "localhost" => "127.0.0.1".to_string(),
        _ => host.to_string(),
    }
}

fn normalize_connect_host(host: &str) -> String {
    match host {
        "" | "localhost" => "127.0.0.1".to_string(),
        _ => host.to_string(),
    }
}

fn resolve_socket_addr(host: &str, port: u16) -> io::Result<SocketAddr> {
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(SocketAddr::new(ip, port));
    }
    let mut addrs = (host, port).to_socket_addrs()?;
    addrs.next().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::AddrNotAvailable,
            format!("could not resolve {host}:{port}"),
        )
    })
}

fn internal_is_loopback(
    _this: &Value,
    args: &[Value],
    _ncx: &mut NativeContext,
) -> Result<Value, VmError> {
    let text = args
        .first()
        .and_then(|v| v.as_string())
        .map(|s| s.as_str().to_string())
        .unwrap_or_default();
    let normalized = text.trim_matches(|c| c == '[' || c == ']');
    let is_loopback = normalized.eq_ignore_ascii_case("localhost")
        || normalized
            .parse::<IpAddr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(false);
    Ok(Value::boolean(is_loopback))
}

fn add_listener(
    target: &Value,
    event_name: &str,
    listener: Value,
    ncx: &mut NativeContext,
) -> Result<(), VmError> {
    let on = target
        .as_object()
        .and_then(|o| o.get(&PropertyKey::string("on")))
        .ok_or_else(|| VmError::type_error("Target has no on() method"))?;
    ncx.call_function(
        &on,
        target.clone(),
        &[Value::string(JsString::intern(event_name)), listener],
    )?;
    Ok(())
}

fn add_once_listener(
    target: &Value,
    event_name: &str,
    listener: Value,
    ncx: &mut NativeContext,
) -> Result<(), VmError> {
    let once = target
        .as_object()
        .and_then(|o| o.get(&PropertyKey::string("once")))
        .ok_or_else(|| VmError::type_error("Target has no once() method"))?;
    ncx.call_function(
        &once,
        target.clone(),
        &[Value::string(JsString::intern(event_name)), listener],
    )?;
    Ok(())
}

fn emit_now(
    target: &Value,
    event_name: &'static str,
    args: &[Value],
    ncx: &mut NativeContext,
) -> Result<(), VmError> {
    let emit = target
        .as_object()
        .and_then(|o| o.get(&PropertyKey::string("emit")))
        .ok_or_else(|| VmError::type_error("Target has no emit() method"))?;
    let mut event_args = Vec::with_capacity(args.len() + 1);
    event_args.push(Value::string(JsString::intern(event_name)));
    event_args.extend(args.iter().cloned());
    ncx.call_function(&emit, target.clone(), &event_args)?;
    Ok(())
}

fn schedule_emit(target: Value, event_name: &'static str, args: Vec<Value>, ncx: &mut NativeContext) {
    let mm = ncx.memory_manager().clone();
    let callback = Value::native_function(
        move |_this, call_args, callback_ncx| {
            let Some(target) = call_args.first().cloned() else {
                return Ok(Value::undefined());
            };
            let emit = target
                .as_object()
                .and_then(|o| o.get(&PropertyKey::string("emit")))
                .ok_or_else(|| VmError::type_error("Target has no emit() method"))?;
            let mut event_args = Vec::with_capacity(call_args.len());
            event_args.push(Value::string(JsString::intern(event_name)));
            event_args.extend(call_args.iter().skip(1).cloned());
            callback_ncx.call_function(&emit, target, &event_args)?;
            Ok(Value::undefined())
        },
        mm,
    );

    let mut next_tick_args = Vec::with_capacity(args.len() + 1);
    next_tick_args.push(target);
    next_tick_args.extend(args);
    let _ = ncx.enqueue_next_tick(callback, next_tick_args);
}

fn schedule_if_listening(target: Value, event_name: &'static str, ncx: &mut NativeContext) {
    let mm = ncx.memory_manager().clone();
    let callback = Value::native_function(
        move |_this, call_args, callback_ncx| {
            let Some(target) = call_args.first().cloned() else {
                return Ok(Value::undefined());
            };
            let Some(obj) = target.as_object() else {
                return Ok(Value::undefined());
            };
            if !obj
                .get(&PropertyKey::string(SERVER_LISTENING_KEY))
                .is_some_and(|v| v.to_boolean())
            {
                return Ok(Value::undefined());
            }
            let emit = obj
                .get(&PropertyKey::string("emit"))
                .ok_or_else(|| VmError::type_error("Server has no emit() method"))?;
            callback_ncx.call_function(
                &emit,
                target,
                &[Value::string(JsString::intern(event_name))],
            )?;
            Ok(Value::undefined())
        },
        mm,
    );
    let _ = ncx.enqueue_next_tick(callback, vec![target]);
}

fn schedule_callback(callback: Value, args: Vec<Value>, ncx: &mut NativeContext) {
    let _ = ncx.enqueue_next_tick(callback, args);
}

fn enqueue_emit(
    queue: &Arc<dyn JsJobQueueTrait + Send + Sync>,
    memory_manager: &Arc<MemoryManager>,
    target: Value,
    event_name: &'static str,
    args: Vec<Value>,
) {
    let callback = Value::native_function(
        move |_this, _args, callback_ncx| {
            emit_now(&target, event_name, &args, callback_ncx)?;
            Ok(Value::undefined())
        },
        memory_manager.clone(),
    );
    enqueue_callback(queue, callback, Vec::new());
}

fn enqueue_callback(
    queue: &Arc<dyn JsJobQueueTrait + Send + Sync>,
    callback: Value,
    args: Vec<Value>,
) {
    queue.enqueue(
        JsPromiseJob {
            kind: JsPromiseJobKind::Fulfill,
            callback,
            this_arg: Value::undefined(),
            result_promise: None,
        },
        args,
    );
}

fn create_node_error_from_runtime(
    _runtime: &ServerRuntime,
    ctor_name: &str,
    message: &str,
    code: Option<&str>,
) -> Value {
    let value = Value::object(GcRef::new(JsObject::new(Value::null())));
    if let Some(obj) = value.as_object() {
        let _ = obj.set(
            PropertyKey::string("name"),
            Value::string(JsString::new_gc(ctor_name)),
        );
        let _ = obj.set(
            PropertyKey::string("message"),
            Value::string(JsString::new_gc(message)),
        );
        if let Some(code) = code {
            let _ = obj.set(
                PropertyKey::string("code"),
                Value::string(JsString::new_gc(code)),
            );
        }
    }
    value
}

fn create_node_error_value(
    ncx: &mut NativeContext,
    ctor_name: &str,
    message: &str,
    code: Option<&str>,
) -> Value {
    let msg = Value::string(JsString::new_gc(message));
    let value = if let Some(ctor) = ncx.global().get(&PropertyKey::string(ctor_name)) {
        if ctor.is_callable() {
            ncx.call_function_construct(&ctor, Value::undefined(), &[msg])
                .unwrap_or_else(|_| Value::undefined())
        } else {
            Value::undefined()
        }
    } else {
        Value::undefined()
    };

    let value = if value.is_object() {
        value
    } else {
        Value::object(GcRef::new(JsObject::new(Value::null())))
    };

    if let Some(obj) = value.as_object() {
        let _ = obj.set(
            PropertyKey::string("name"),
            Value::string(JsString::new_gc(ctor_name)),
        );
        let _ = obj.set(
            PropertyKey::string("message"),
            Value::string(JsString::new_gc(message)),
        );
        if let Some(code) = code {
            let _ = obj.set(
                PropertyKey::string("code"),
                Value::string(JsString::new_gc(code)),
            );
        }
    }

    value
}

fn throw_node_error<T>(
    ncx: &mut NativeContext,
    ctor_name: &str,
    message: &str,
    code: Option<&str>,
) -> Result<T, VmError> {
    Err(VmError::exception(create_node_error_value(
        ncx, ctor_name, message, code,
    )))
}

fn value_display(value: &Value) -> String {
    if value.is_null() {
        return "null".to_string();
    }
    if value.is_undefined() {
        return "undefined".to_string();
    }
    if let Some(text) = value.as_string() {
        return text.as_str().to_string();
    }
    if let Some(n) = value.as_number() {
        return n.to_string();
    }
    if let Some(b) = value.as_boolean() {
        return b.to_string();
    }
    if let Some(obj) = value.as_object() {
        if obj.is_array() {
            return "[object Array]".to_string();
        }
        return "[object Object]".to_string();
    }
    if value.as_symbol().is_some() {
        return "Symbol()".to_string();
    }
    format!("{value:?}")
}

fn copy_known_connect_options(dst: &GcRef<JsObject>, source: &Value) {
    let Some(src) = source.as_object() else {
        return;
    };
    for key in [
        "port",
        "host",
        "address",
        "path",
        "family",
        "hints",
        "lookup",
        "autoSelectFamily",
        "autoSelectFamilyAttemptTimeout",
        "objectMode",
        "readableObjectMode",
        "writableObjectMode",
    ] {
        if let Some(value) = src.get(&PropertyKey::string(key)) {
            let _ = dst.set(PropertyKey::string(key), value);
        }
    }
}

fn ip_input_string(arg: Option<&Value>, ncx: &mut NativeContext) -> Result<Option<String>, VmError> {
    let Some(arg) = arg else {
        return Ok(None);
    };
    if arg.is_null() || arg.is_undefined() {
        return Ok(None);
    }
    if let Some(text) = arg.as_string() {
        return Ok(Some(text.as_str().to_string()));
    }
    if let Some(obj) = arg.as_object()
        && let Some(to_string) = obj.get(&PropertyKey::string("toString"))
        && to_string.is_callable()
    {
        let value = ncx.call_function(&to_string, *arg, &[])?;
        if let Some(text) = value.as_string() {
            return Ok(Some(text.as_str().to_string()));
        }
        return Ok(Some(value_display(&value)));
    }
    Ok(Some(value_display(arg)))
}

fn parse_ip_input(text: Option<&str>) -> Option<IpKind> {
    let text = text?;
    if let Ok(addr) = text.parse::<IpAddr>() {
        return Some(match addr {
            IpAddr::V4(_) => IpKind::V4,
            IpAddr::V6(_) => IpKind::V6,
        });
    }

    if let Some((base, zone)) = text.split_once('%')
        && !base.is_empty()
        && base.contains(':')
        && !zone.is_empty()
        && zone
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.')
        && matches!(base.parse::<IpAddr>(), Ok(IpAddr::V6(_)))
    {
        return Some(IpKind::V6);
    }

    None
}
