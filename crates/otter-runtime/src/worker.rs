//! Worker isolates, JavaScript `Worker`, and isolate-pool routing.
//!
//! The runtime worker model is isolate-per-worker: each worker owns a
//! separate managed runtime runner ([`RuntimeHandle`]) and therefore a
//! separate VM, runtime state, GC heap, bounded inbox, timers, host
//! completions, and dynamic-import pump. This module owns both the sendable
//! host handle and the JavaScript-visible message/event surface.
//!
//! # Contents
//!
//! - [`Worker`] — sendable handle to one worker isolate.
//! - [`WorkerBuilder`] — configuration for one worker.
//! - [`OtterPool`] — small round-robin isolate pool prototype.
//! - JavaScript worker construction, phased message admission, typed
//!   [`RuntimeTask`] delivery in both directions, transfer commit, and
//!   deterministic termination.
//!
//! # Invariants
//!
//! - Every worker owns a separate admitted runtime; no ordinary VM value,
//!   moving GC handle, or [`ExecutionContext`] crosses a `Send` boundary.
//!   Parent- and child-side dispatch state lives inside the owning isolate
//!   and is reacquired from `&mut Runtime` or persistent roots.
//! - Delivery is wake-driven: parent and child exchange typed tasks through
//!   their bounded inboxes. There is no polling channel and no poll timer.
//! - Every message passes validate/measure → admission → fallible clone →
//!   enqueue → detach, charging the finite [`WorkerFamily`] ledger and the
//!   shared main ledger atomically per account with checked arithmetic.
//! - The worker's terminal Error/Closed outcome rides a guaranteed credit
//!   reserved at construction and is delivered exactly once.
//! - Worker methods accept only owned public inputs and return
//!   [`crate::ExecutionResult`] / [`crate::OtterError`].
//! - Owned JavaScript message payloads materialize entirely inside one traced
//!   [`NativeScope`]. A transferable detaches only after its destination queue
//!   accepts the corresponding payload.
//!
//! # See also
//!
//! - [Event loop](../../../docs/book/src/engine/event-loop.md)
//! - [Runtime architecture](../../../docs/book/src/engine/architecture.md)

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use otter_gc::raw::RawGc;
use otter_resource::{ResourceClass, ResourceLease, ResourceLeaseSet};
use otter_vm::binary::array_buffer::SharedBody;
use otter_vm::binary::{JsArrayBuffer, TypedArrayKind};
use otter_vm::host_completion::{HostCompletionAdmission, HostCompletionOutcome};
use otter_vm::{
    ExecutionContext, Local, NativeCall, NativeCtx, NativeError, NativeFn, NativeScope,
    PersistentRootId, Value, array, collections, object,
};
use smallvec::smallvec;

use crate::event_loop::RuntimeLiveness;
use crate::module_loader;
use crate::runtime_activity::{RuntimeKeepAlive, RuntimeTask, RuntimeTaskSpawner};
use crate::{
    CapabilitySet, ExecutionResult, OtterError, Permission, ResourceAccount, ResourceLimits,
    ResourceSnapshot, Runtime, RuntimeActivityStats, RuntimeBuilder, RuntimeConfig, RuntimeHandle,
    SourceInput, StructuredCloneNumber, StructuredCloneTransferList, StructuredCloneValue,
    TokioRuntimeHost,
};

static NEXT_WORKER_ID: AtomicU64 = AtomicU64::new(1);
const MAX_JAVASCRIPT_WORKER_ID: u64 = (1_u64 << 53) - 1;

fn next_worker_id() -> Option<WorkerId> {
    NEXT_WORKER_ID
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |next| {
            if next <= MAX_JAVASCRIPT_WORKER_ID {
                next.checked_add(1)
            } else {
                None
            }
        })
        .ok()
        .map(WorkerId)
}

/// Stable host-side worker identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WorkerId(u64);

impl WorkerId {
    /// Numeric worker id. Monotonic within this process.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone)]
enum WorkerPayload {
    Undefined,
    Null,
    Boolean(bool),
    Number(StructuredCloneNumber),
    BigInt(String),
    String(String),
    Array(Vec<WorkerPayload>),
    Object(Vec<(String, WorkerPayload)>),
    Map(Vec<(WorkerPayload, WorkerPayload)>),
    Set(Vec<WorkerPayload>),
    ArrayBuffer(Vec<u8>),
    SharedArrayBuffer(Arc<SharedBody>),
    /// A typed-array view: its kind and range over the cloned `buffer`.
    TypedArray {
        kind: TypedArrayKind,
        buffer: Box<WorkerPayload>,
        byte_offset: usize,
        length: usize,
    },
    /// A `DataView` over the cloned `buffer`.
    DataView {
        buffer: Box<WorkerPayload>,
        byte_offset: usize,
        byte_length: usize,
    },
}

#[derive(Default)]
struct WorkerTransferList {
    buffers: Vec<JsArrayBuffer>,
    set: HashSet<JsArrayBuffer>,
}

enum WorkerEvent {
    Message(WorkerPayload),
    Error(String),
    MessageError(String),
}

/// Hard limits shared by every JavaScript worker reachable from one root
/// runtime, including nested workers. The family ledger is a second, always
/// finite [`ResourceAccount`]: an unlimited main ledger cannot lift these
/// caps because every worker and message reserves on both.
pub(crate) struct WorkerFamily {
    account: ResourceAccount,
}

impl std::fmt::Debug for WorkerFamily {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkerFamily").finish_non_exhaustive()
    }
}

const WORKER_FAMILY_MAX_WORKERS: u64 = 128;
const WORKER_FAMILY_MAX_QUEUED_MESSAGES: u64 = 4096;
const WORKER_FAMILY_MAX_QUEUED_MESSAGE_BYTES: u64 = 64 * 1024 * 1024;
/// Upper bound for one measured message graph.
const WORKER_MAX_MESSAGE_BYTES: u64 = 16 * 1024 * 1024;
/// Flat accounted overhead of one payload node (enum tag, vec headers).
const WORKER_MESSAGE_NODE_BYTES: u64 = 32;
/// Flat accounted overhead of one transfer-list entry.
const WORKER_MESSAGE_TRANSFER_ENTRY_BYTES: u64 = 32;

impl WorkerFamily {
    pub(crate) fn standard() -> Arc<Self> {
        Self::with_limits(
            WORKER_FAMILY_MAX_WORKERS,
            WORKER_FAMILY_MAX_QUEUED_MESSAGES,
            WORKER_FAMILY_MAX_QUEUED_MESSAGE_BYTES,
        )
    }

    pub(crate) fn with_limits(
        workers: u64,
        queued_messages: u64,
        queued_message_bytes: u64,
    ) -> Arc<Self> {
        Arc::new(Self {
            account: ResourceAccount::new(
                ResourceLimits::builder()
                    .limit(ResourceClass::Workers, workers)
                    .limit(ResourceClass::QueuedMessages, queued_messages)
                    .limit(ResourceClass::QueuedMessageBytes, queued_message_bytes)
                    .build(),
            ),
        })
    }
}

/// Parent-isolate dispatch state for one worker: the rooted worker object,
/// the rooted hidden listener store, and the execution context that
/// constructed the worker. Persistent root ids are only dereferenced on the
/// parent isolate thread.
#[derive(Clone)]
struct WorkerParentBinding {
    worker_root: PersistentRootId,
    listeners_root: PersistentRootId,
    context: ExecutionContext,
}

struct WorkerRecord {
    id: WorkerId,
    child: RuntimeHandle,
    child_spawner: RuntimeTaskSpawner,
    child_shared: Arc<WorkerChildShared>,
    wait_agent: otter_vm::atomics_wait::WaitAgentHandle,
    binding: Mutex<Option<WorkerParentBinding>>,
    keep_alive: Mutex<Option<RuntimeKeepAlive>>,
    /// Family worker slot. Released when the record drops.
    _family_worker_lease: ResourceLease,
    terminated: AtomicBool,
}

impl WorkerRecord {
    /// Idempotent termination request: cancel a blocking `Atomics.wait`,
    /// then interrupt and shut the child isolate down. Returns `true` for
    /// the call that performed the transition.
    fn request_terminate(&self) -> bool {
        if self.terminated.swap(true, Ordering::SeqCst) {
            return false;
        }
        self.wait_agent.cancel();
        self.child.shutdown();
        true
    }

    /// Deterministically join the child isolate runner thread.
    fn join(&self) {
        self.child.join_runner_blocking();
    }

    fn release_keep_alive(&self) {
        if let Some(keep_alive) = self
            .keep_alive
            .lock()
            .expect("worker keep-alive mutex poisoned")
            .take()
        {
            keep_alive.close();
        }
    }

    fn take_binding(&self) -> Option<WorkerParentBinding> {
        self.binding
            .lock()
            .expect("worker binding mutex poisoned")
            .take()
    }

    fn binding_view(&self) -> Option<WorkerParentBinding> {
        self.binding
            .lock()
            .expect("worker binding mutex poisoned")
            .clone()
    }
}

impl Drop for WorkerRecord {
    fn drop(&mut self) {
        self.request_terminate();
        self.join();
        self.release_keep_alive();
    }
}

pub(crate) struct WorkerHostState {
    config: RuntimeConfig,
    family: Arc<WorkerFamily>,
    workers: Mutex<HashMap<u64, Arc<WorkerRecord>>>,
}

impl WorkerHostState {
    fn insert(&self, record: Arc<WorkerRecord>) {
        self.workers
            .lock()
            .expect("worker registry poisoned")
            .insert(record.id.get(), record);
    }

    fn get(&self, id: u64) -> Option<Arc<WorkerRecord>> {
        self.workers
            .lock()
            .expect("worker registry poisoned")
            .get(&id)
            .cloned()
    }

    fn remove(&self, id: u64) -> Option<Arc<WorkerRecord>> {
        self.workers
            .lock()
            .expect("worker registry poisoned")
            .remove(&id)
    }
}

impl Drop for WorkerHostState {
    fn drop(&mut self) {
        let workers: Vec<_> = self
            .workers
            .lock()
            .expect("worker registry poisoned")
            .drain()
            .map(|(_, worker)| worker)
            .collect();
        for worker in &workers {
            worker.request_terminate();
        }
        for worker in workers {
            worker.join();
            worker.release_keep_alive();
        }
    }
}

/// Sendable child-side worker state. Lives in the child natives, the entry
/// task, and the parent record; never carries VM values or GC handles.
struct WorkerChildShared {
    id: u64,
    /// Parent isolate inbox for typed delivery tasks.
    parent: RuntimeTaskSpawner,
    /// Parent host registry; `Weak` breaks the
    /// host -> record -> shared -> host cycle.
    host: Weak<WorkerHostState>,
    family: Arc<WorkerFamily>,
    /// Shared main ledger inherited from the parent runtime.
    account: ResourceAccount,
    closed: AtomicBool,
    /// Pre-reserved terminal credit. Taking it is the once-only gate for the
    /// worker's terminal Error/Closed delivery.
    terminal: Mutex<Option<HostCompletionAdmission>>,
}

pub(crate) fn install_main_worker_globals(runtime: &mut Runtime) -> Result<(), OtterError> {
    let family = runtime
        .config
        .worker_family
        .clone()
        .unwrap_or_else(WorkerFamily::standard);
    runtime.config.worker_family = Some(family.clone());
    let parent_spawner = runtime.runtime_task_spawner();
    let host = Arc::new(WorkerHostState {
        config: runtime.config.clone(),
        family,
        workers: Mutex::new(HashMap::new()),
    });
    runtime.install_native_constructor_global_call(
        "Worker",
        2,
        worker_constructor_call(host, parent_spawner),
    )?;
    Ok(())
}

fn worker_constructor_call(
    host: Arc<WorkerHostState>,
    parent_spawner: Option<RuntimeTaskSpawner>,
) -> NativeCall {
    let call: Arc<NativeFn> = Arc::new(move |ctx, args, _captures| {
        // A direct runtime has no managed inbox: fail synchronously before
        // any resource effect.
        let Some(parent_spawner) = parent_spawner.clone() else {
            return Err(type_err(
                "Worker",
                "Worker requires a managed RuntimeHandle/Otter runtime".to_string(),
            ));
        };
        let specifier = value_to_string(ctx, args.first().unwrap_or(&Value::undefined()))?;
        // Validate the narrowing request before any spawn effect.
        let requested_capabilities = parse_worker_capability_request(ctx, args.get(1))?;
        let parent_context = ctx.execution_context().cloned().ok_or_else(|| {
            type_err(
                "Worker",
                "Worker construction requires an execution context".to_string(),
            )
        })?;
        let record =
            spawn_managed_worker(&host, &parent_spawner, specifier, requested_capabilities)?;
        let result = build_worker_object(ctx, &host, &record, parent_context);
        if result.is_err()
            && let Some(record) = host.remove(record.id.get())
        {
            record.request_terminate();
            record.join();
            record.release_keep_alive();
        }
        result
    });
    NativeCall::Dynamic(call)
}

fn spawn_managed_worker(
    host: &Arc<WorkerHostState>,
    parent_spawner: &RuntimeTaskSpawner,
    specifier: String,
    requested_capabilities: Option<CapabilitySet>,
) -> Result<Arc<WorkerRecord>, NativeError> {
    let id = next_worker_id()
        .ok_or_else(|| type_err("Worker", "worker id space is exhausted".to_string()))?;
    // Family admission precedes every spawn effect. The main ledger charges
    // its own worker tuple inside the managed spawn below.
    let family_worker_lease = host
        .family
        .account
        .reserve_exact(ResourceClass::Workers, 1)
        .map_err(|err| type_err("Worker", format!("worker family limit: {err}")))?;
    // Terminal credit: reserved before the spawn so the worker's Error/Closed
    // outcome can always be delivered, even through a full parent inbox.
    let terminal = parent_spawner
        .admit_guaranteed(RuntimeLiveness::Unref)
        .map_err(|err| type_err("Worker", err.to_string()))?;
    let child_config = configure_worker_child(
        host.config.clone(),
        parent_spawner.io_handle(),
        requested_capabilities,
    );
    let child = RuntimeHandle::spawn_worker(child_config)
        .map_err(|err| type_err("Worker", err.to_string()))?;
    let child_spawner = child.task_spawner();
    let wait_agent = child.atomics_wait_agent();
    let keep_alive = parent_spawner.retain_keep_alive(RuntimeLiveness::Ref);
    let shared = Arc::new(WorkerChildShared {
        id: id.get(),
        parent: parent_spawner.clone(),
        host: Arc::downgrade(host),
        family: host.family.clone(),
        account: host.config.resource_account.clone(),
        closed: AtomicBool::new(false),
        terminal: Mutex::new(Some(terminal)),
    });
    let record = Arc::new(WorkerRecord {
        id,
        child,
        child_spawner: child_spawner.clone(),
        child_shared: shared.clone(),
        wait_agent,
        binding: Mutex::new(None),
        keep_alive: Mutex::new(Some(keep_alive)),
        _family_worker_lease: family_worker_lease,
        terminated: AtomicBool::new(false),
    });
    host.insert(record.clone());
    if child_spawner
        .enqueue(WorkerEntryTask { shared, specifier }, RuntimeLiveness::Ref)
        .is_err()
    {
        host.remove(id.get());
        record.request_terminate();
        record.join();
        record.release_keep_alive();
        return Err(type_err(
            "Worker",
            "worker entry could not be scheduled".to_string(),
        ));
    }
    Ok(record)
}

/// Prepare the child isolate's configuration: workers may block in
/// `Atomics.wait`, share the parent's Tokio executor, and inherit the
/// parent's capabilities, hooks, module host, resource account, and worker
/// family verbatim through the cloned config.
fn configure_worker_child(
    mut config: RuntimeConfig,
    parent_io_handle: Option<tokio::runtime::Handle>,
    requested_capabilities: Option<CapabilitySet>,
) -> RuntimeConfig {
    config.allow_blocking_atomics_wait = true;
    if config.runtime_host.is_none()
        && let Some(io_handle) = parent_io_handle
    {
        config.runtime_host = Some(TokioRuntimeHost::from_handle(io_handle));
    }
    // Worker-scoped narrowing: the child permits only what both the parent
    // set and the requested subset permit, so a request can never escalate.
    // Nested workers narrow again from the already-narrowed set.
    if let Some(requested) = requested_capabilities {
        config.capabilities = config.capabilities.narrowed(requested);
    }
    config
}

/// Parse the `otter.capabilities` narrowing request from the Worker options
/// argument.
///
/// Shape: `new Worker(url, { otter: { capabilities: { read, write, net,
/// env, run, ffi } } })`, where each class is `false` (deny), `true`
/// (inherit the parent rule set), or an array of pattern strings (allow
/// only those the parent also allows). A missing class inherits. The walk
/// reads plain data properties through the heap — no getters run, keeping
/// validation side-effect free before the spawn effect.
fn parse_worker_capability_request(
    ctx: &NativeCtx<'_>,
    options: Option<&Value>,
) -> Result<Option<CapabilitySet>, NativeError> {
    let Some(options) = options.filter(|value| !value.is_nullish()) else {
        return Ok(None);
    };
    let options = options
        .as_object()
        .ok_or_else(|| type_err("Worker", "options must be an object".to_string()))?;
    let heap = ctx.heap();
    let Some(otter) = object::get(options, heap, "otter").filter(|value| !value.is_nullish())
    else {
        return Ok(None);
    };
    let otter = otter
        .as_object()
        .ok_or_else(|| type_err("Worker", "options.otter must be an object".to_string()))?;
    let Some(capabilities) =
        object::get(otter, heap, "capabilities").filter(|value| !value.is_nullish())
    else {
        return Ok(None);
    };
    let capabilities = capabilities.as_object().ok_or_else(|| {
        type_err(
            "Worker",
            "options.otter.capabilities must be an object".to_string(),
        )
    })?;

    enum ClassRequest {
        /// Missing key or `true`: keep the parent rule set (the
        /// narrowing identity).
        Inherit,
        /// `false`: deny the whole class.
        Deny,
        /// Pattern list: allow only these, bounded by the parent set.
        Allow(Vec<String>),
    }

    let class_request = |key: &'static str| -> Result<ClassRequest, NativeError> {
        let Some(value) = object::get(capabilities, heap, key).filter(|v| !v.is_undefined()) else {
            return Ok(ClassRequest::Inherit);
        };
        if let Some(flag) = value.as_boolean() {
            return Ok(if flag {
                ClassRequest::Inherit
            } else {
                ClassRequest::Deny
            });
        }
        let Some(list) = value.as_array() else {
            return Err(type_err(
                "Worker",
                format!("options.otter.capabilities.{key} must be a boolean or string array"),
            ));
        };
        let len = array::len(list, heap);
        let mut entries = Vec::with_capacity(len);
        for idx in 0..len {
            let element = array::get(list, heap, idx);
            let Some(text) = element.as_string(heap) else {
                return Err(type_err(
                    "Worker",
                    format!("options.otter.capabilities.{key}[{idx}] must be a string"),
                ));
            };
            entries.push(text.to_lossy_string(heap));
        }
        Ok(ClassRequest::Allow(entries))
    };

    let string_permission = |request: ClassRequest| match request {
        ClassRequest::Inherit => Permission::AllowAll,
        ClassRequest::Deny => Permission::Deny,
        ClassRequest::Allow(entries) => Permission::allow(entries),
    };
    let path_permission = |request: ClassRequest| match request {
        ClassRequest::Inherit => Permission::AllowAll,
        ClassRequest::Deny => Permission::Deny,
        ClassRequest::Allow(entries) => Permission::allow(entries.into_iter().map(PathBuf::from)),
    };

    Ok(Some(CapabilitySet {
        read: path_permission(class_request("read")?),
        write: path_permission(class_request("write")?),
        net: string_permission(class_request("net")?),
        env: string_permission(class_request("env")?),
        run: string_permission(class_request("run")?),
        ffi: path_permission(class_request("ffi")?),
    }))
}

fn build_worker_object(
    ctx: &mut NativeCtx<'_>,
    host: &Arc<WorkerHostState>,
    record: &Arc<WorkerRecord>,
    parent_context: ExecutionContext,
) -> Result<Value, NativeError> {
    let id = record.id.get();
    let post_host = host.clone();
    let post: Arc<NativeFn> = Arc::new(move |ctx, args, _captures| {
        ctx.this_value()
            .as_object()
            .ok_or_else(|| type_err("Worker.postMessage", "invalid receiver".to_string()))?;
        let Some(record) = post_host.get(id) else {
            return Err(type_err(
                "Worker.postMessage",
                "worker is not running".to_string(),
            ));
        };
        if record.terminated.load(Ordering::SeqCst) {
            return Err(type_err(
                "Worker.postMessage",
                "worker has been terminated".to_string(),
            ));
        }
        let value = args.first().copied().unwrap_or_else(Value::undefined);
        let transfers = parse_worker_transfer_list(args.get(1), ctx)?;
        // Validate/measure, admit, clone, enqueue, then detach: a rejected or
        // failed message never detaches the sender's transferables.
        let measured = measure_worker_message(&value, ctx.heap(), &transfers)?;
        let leases = admit_worker_message(
            &post_host.family,
            &post_host.config.resource_account,
            measured,
            "Worker.postMessage",
        )?;
        let payload = clone_worker_value(&value, ctx.heap(), &transfers)?;
        record
            .child_spawner
            .enqueue(
                WorkerChildMessageTask {
                    shared: record.child_shared.clone(),
                    payload,
                    leases,
                },
                RuntimeLiveness::Ref,
            )
            .map_err(|err| type_err("Worker.postMessage", err.to_string()))?;
        detach_worker_transfers(&transfers, ctx.heap_mut());
        Ok(Value::undefined())
    });

    let terminate_host = host.clone();
    let terminate: Arc<NativeFn> = Arc::new(move |ctx, _args, _captures| {
        ctx.this_value()
            .as_object()
            .ok_or_else(|| type_err("Worker.terminate", "invalid receiver".to_string()))?;
        if let Some(record) = terminate_host.remove(id) {
            record.request_terminate();
            record.join();
            record.release_keep_alive();
            if let Some(binding) = record.take_binding() {
                ctx.persistent_root_remove(binding.worker_root);
                ctx.persistent_root_remove(binding.listeners_root);
            }
        }
        Ok(Value::undefined())
    });

    let worker = ctx.scope(|mut scope| {
        let worker = scope.object()?;
        let null = scope.null();
        scope.set(worker, "onmessage", null)?;
        scope.set(worker, "onerror", null)?;
        scope.set(worker, "onmessageerror", null)?;

        for (name, length, call) in [
            ("postMessage", 1, NativeCall::Dynamic(post)),
            ("terminate", 0, NativeCall::Dynamic(terminate)),
        ] {
            let function = scope.native_call(name, length, call)?;
            scope.set(worker, name, function)?;
        }
        Ok::<Value, NativeError>(scope.finish(worker))
    })?;
    let worker_root = ctx.persistent_root_insert(worker);
    let listeners = match ctx.scope(|mut scope| {
        let listeners = scope.object()?;
        Ok::<Value, NativeError>(scope.finish(listeners))
    }) {
        Ok(listeners) => listeners,
        Err(err) => {
            ctx.persistent_root_remove(worker_root);
            return Err(err);
        }
    };
    let listeners_root = ctx.persistent_root_insert(listeners);
    let result = (|| {
        install_worker_event_methods(ctx, worker_root, listeners_root)?;
        worker_persistent_value(ctx, worker_root)
    })();
    match result {
        Ok(worker_value) => {
            // The binding owns both persistent roots from here on; terminate
            // or the terminal task releases them.
            record
                .binding
                .lock()
                .expect("worker binding mutex poisoned")
                .replace(WorkerParentBinding {
                    worker_root,
                    listeners_root,
                    context: parent_context,
                });
            Ok(worker_value)
        }
        Err(err) => {
            ctx.persistent_root_remove(worker_root);
            ctx.persistent_root_remove(listeners_root);
            Err(err)
        }
    }
}

/// Leases held while one message occupies the parent or child queue.
/// Dropping them — on dispatch, cancellation, or enqueue failure — returns
/// the family and main-ledger charges atomically per account.
struct WorkerMessageLeases {
    _family: ResourceLeaseSet,
    _main: ResourceLeaseSet,
}

fn admit_worker_message(
    family: &WorkerFamily,
    account: &ResourceAccount,
    bytes: u64,
    api: &'static str,
) -> Result<WorkerMessageLeases, NativeError> {
    if bytes > WORKER_MAX_MESSAGE_BYTES {
        return Err(type_err(
            api,
            format!("message of {bytes} bytes exceeds the {WORKER_MAX_MESSAGE_BYTES}-byte limit"),
        ));
    }
    let family_leases = family
        .account
        .reserve_exact_many(&[
            (ResourceClass::QueuedMessages, 1),
            (ResourceClass::QueuedMessageBytes, bytes),
        ])
        .map_err(|err| type_err(api, format!("worker message queue limit: {err}")))?;
    let main_leases = account
        .reserve_exact_many(&[
            (ResourceClass::QueuedTasks, 1),
            (ResourceClass::QueuedMessages, 1),
            (ResourceClass::QueuedMessageBytes, bytes),
        ])
        .map_err(|err| type_err(api, format!("runtime message budget: {err}")))?;
    Ok(WorkerMessageLeases {
        _family: family_leases,
        _main: main_leases,
    })
}

/// Deliver the worker's terminal outcome exactly once through the
/// pre-reserved guaranteed credit. Later calls find the credit consumed and
/// do nothing.
fn post_worker_terminal(shared: &WorkerChildShared, error: Option<String>) {
    let Some(admission) = shared
        .terminal
        .lock()
        .expect("worker terminal mutex poisoned")
        .take()
    else {
        return;
    };
    let Some(host) = shared.host.upgrade() else {
        return;
    };
    let task = WorkerTerminalTask {
        host,
        id: shared.id,
        error,
    };
    let _ = shared
        .parent
        .enqueue_guaranteed(admission, task, HostCompletionOutcome::Completed);
}

/// Post one non-terminal child event (an uncaught handler error or a payload
/// materialization failure) to the parent through ordinary admission. A full
/// queue drops the event; the rejection stays visible on both ledgers.
fn post_worker_event(shared: &WorkerChildShared, event: WorkerEvent) {
    let bytes = match &event {
        WorkerEvent::Error(message) | WorkerEvent::MessageError(message) => {
            let Some(bytes) = (message.len() as u64)
                .checked_mul(2)
                .and_then(|b| b.checked_add(WORKER_MESSAGE_NODE_BYTES))
            else {
                return;
            };
            bytes
        }
        WorkerEvent::Message(_) => return,
    };
    let Ok(leases) = admit_worker_message(&shared.family, &shared.account, bytes, "Worker") else {
        return;
    };
    let Some(host) = shared.host.upgrade() else {
        return;
    };
    let _ = shared.parent.enqueue(
        WorkerParentDeliverTask {
            host,
            id: shared.id,
            event,
            leases,
        },
        RuntimeLiveness::Ref,
    );
}

fn dispatch_worker_event_on_parent(
    runtime: &mut Runtime,
    binding: &WorkerParentBinding,
    event: WorkerEvent,
) -> Result<(), OtterError> {
    let worker_root = binding.worker_root;
    let listeners_root = binding.listeners_root;
    runtime.run_native_event(&binding.context, move |ctx| {
        // Materialize first; the root re-reads below stay fresh because no
        // allocation happens between them and the dispatch call.
        let event_value = worker_event_to_value(ctx, event)?;
        let Some(event_obj) = event_value.as_object() else {
            return Ok(Value::undefined());
        };
        let Some(worker) = ctx
            .persistent_root_get(worker_root)
            .and_then(|value| value.as_object())
        else {
            return Ok(Value::undefined());
        };
        let listeners = ctx
            .persistent_root_get(listeners_root)
            .and_then(|value| value.as_object());
        dispatch_event_object(ctx, worker, listeners, event_obj)?;
        Ok(Value::undefined())
    })
}

/// First task on a fresh worker isolate: installs the worker globals, runs
/// the entry, and retains the entry context for later message dispatch. The
/// bounded child inbox is FIFO, so a `postMessage` issued right after the
/// constructor is dispatched only after the entry completed.
struct WorkerEntryTask {
    shared: Arc<WorkerChildShared>,
    specifier: String,
}

impl RuntimeTask for WorkerEntryTask {
    fn run(self: Box<Self>, runtime: &mut Runtime) -> Result<(), OtterError> {
        let Self { shared, specifier } = *self;
        if let Err(err) = install_worker_scope_natives(runtime, shared.clone()) {
            post_worker_terminal(&shared, Some(err.to_string()));
            return Ok(());
        }
        match run_worker_entry(runtime, &specifier) {
            Ok((_result, context)) => {
                runtime.worker_child_context = Some(context);
            }
            Err(err) => {
                post_worker_terminal(&shared, Some(err.to_string()));
            }
        }
        Ok(())
    }

    fn cancel(self: Box<Self>, _runtime: &mut Runtime) {
        post_worker_terminal(&self.shared, None);
    }
}

/// Parent-to-child message. Runs on the child isolate; leases drop when the
/// message leaves the queue on every dispatch, cancel, and drop path.
struct WorkerChildMessageTask {
    shared: Arc<WorkerChildShared>,
    payload: WorkerPayload,
    leases: WorkerMessageLeases,
}

impl RuntimeTask for WorkerChildMessageTask {
    fn run(self: Box<Self>, runtime: &mut Runtime) -> Result<(), OtterError> {
        let Self {
            shared,
            payload,
            leases,
        } = *self;
        drop(leases);
        if shared.closed.load(Ordering::SeqCst) {
            return Ok(());
        }
        let Some(context) = runtime.worker_child_context.clone() else {
            return Ok(());
        };
        if let Err(err) = runtime.dispatch_worker_message_event(&context, |ctx| {
            materialize_worker_payload(ctx, &payload)
        }) {
            let event = match err {
                crate::MessageEventDispatchError::Materialize(err) => {
                    WorkerEvent::MessageError(err.to_string())
                }
                crate::MessageEventDispatchError::Handler(err) => {
                    WorkerEvent::Error(err.to_string())
                }
            };
            post_worker_event(&shared, event);
        }
        Ok(())
    }
}

/// Child-to-parent delivery. Runs on the parent isolate and dispatches the
/// event on the rooted worker object.
struct WorkerParentDeliverTask {
    host: Arc<WorkerHostState>,
    id: u64,
    event: WorkerEvent,
    leases: WorkerMessageLeases,
}

impl RuntimeTask for WorkerParentDeliverTask {
    fn run(self: Box<Self>, runtime: &mut Runtime) -> Result<(), OtterError> {
        let Self {
            host,
            id,
            event,
            leases,
        } = *self;
        drop(leases);
        let Some(record) = host.get(id) else {
            return Ok(());
        };
        if record.terminated.load(Ordering::SeqCst) {
            return Ok(());
        }
        let Some(binding) = record.binding_view() else {
            return Ok(());
        };
        // A throwing event handler must not take the parent runner down; the
        // dispatch error is already routed through diagnostics mapping.
        let _ = dispatch_worker_event_on_parent(runtime, &binding, event);
        Ok(())
    }
}

/// The worker's terminal outcome on the parent isolate: dispatch a fatal
/// error event when one exists, then release the binding roots and tear the
/// child isolate down. Reached exactly once through the terminal credit.
struct WorkerTerminalTask {
    host: Arc<WorkerHostState>,
    id: u64,
    error: Option<String>,
}

impl WorkerTerminalTask {
    fn cleanup(record: &WorkerRecord, runtime: &mut Runtime) {
        if let Some(binding) = record.take_binding() {
            runtime.interp.persistent_root_remove(binding.worker_root);
            runtime
                .interp
                .persistent_root_remove(binding.listeners_root);
        }
        record.request_terminate();
        record.join();
        record.release_keep_alive();
    }
}

impl RuntimeTask for WorkerTerminalTask {
    fn run(self: Box<Self>, runtime: &mut Runtime) -> Result<(), OtterError> {
        let Self { host, id, error } = *self;
        let Some(record) = host.remove(id) else {
            return Ok(());
        };
        if let Some(error) = error
            && !record.terminated.load(Ordering::SeqCst)
            && let Some(binding) = record.binding_view()
        {
            let _ = dispatch_worker_event_on_parent(runtime, &binding, WorkerEvent::Error(error));
        }
        Self::cleanup(&record, runtime);
        Ok(())
    }

    fn cancel(self: Box<Self>, runtime: &mut Runtime) {
        let Self { host, id, .. } = *self;
        if let Some(record) = host.remove(id) {
            Self::cleanup(&record, runtime);
        }
    }
}

fn worker_persistent_value(
    ctx: &NativeCtx<'_>,
    root: otter_vm::PersistentRootId,
) -> Result<Value, NativeError> {
    ctx.persistent_root_get(root)
        .ok_or_else(|| type_err("Worker", "worker root was lost".to_string()))
}

/// Installs `dispatchEvent`/`addEventListener`/`removeEventListener` on the
/// worker object. The listener store rides each function's traced capture
/// vector; it is deliberately not an own property of the worker, so script
/// cannot observe or replace the routing state.
fn install_worker_event_methods(
    ctx: &mut NativeCtx<'_>,
    worker_root: otter_vm::PersistentRootId,
    listeners_root: otter_vm::PersistentRootId,
) -> Result<(), NativeError> {
    let listeners = worker_persistent_value(ctx, listeners_root)?;
    let dispatch = ctx.native_value_with_length(
        "dispatchEvent",
        1,
        smallvec![listeners],
        |ctx, args, captures| {
            let worker = ctx
                .this_value()
                .as_object()
                .ok_or_else(|| type_err("Worker.dispatchEvent", "invalid receiver".to_string()))?;
            let event = args.first().copied().unwrap_or(Value::undefined());
            let event_obj = event.as_object().ok_or_else(|| {
                type_err(
                    "Worker.dispatchEvent",
                    "event must be an object".to_string(),
                )
            })?;
            let listeners = captures.first().and_then(|value| value.as_object());
            dispatch_event_object(ctx, worker, listeners, event_obj)?;
            Ok(Value::boolean(true))
        },
    )?;
    set_worker_method(ctx, worker_root, "dispatchEvent", dispatch)?;

    let listeners = worker_persistent_value(ctx, listeners_root)?;
    let add = ctx.native_value_with_length(
        "addEventListener",
        2,
        smallvec![listeners],
        |ctx, args, captures| {
            ctx.this_value().as_object().ok_or_else(|| {
                type_err("Worker.addEventListener", "invalid receiver".to_string())
            })?;
            let ty = value_to_string(ctx, args.first().unwrap_or(&Value::undefined()))?;
            // The capture slab is old-space and rewritten in place by a moving
            // collection, so this read is fresh even after `value_to_string`
            // allocated.
            let store = captures.first().copied().unwrap_or(Value::undefined());
            if let Some(listener) = args.get(1)
                && listener.is_callable()
            {
                add_worker_event_listener(ctx, store, &ty, *listener)?;
            }
            Ok(Value::undefined())
        },
    )?;
    set_worker_method(ctx, worker_root, "addEventListener", add)?;

    let listeners = worker_persistent_value(ctx, listeners_root)?;
    let remove = ctx.native_value_with_length(
        "removeEventListener",
        2,
        smallvec![listeners],
        |ctx, args, captures| {
            ctx.this_value().as_object().ok_or_else(|| {
                type_err("Worker.removeEventListener", "invalid receiver".to_string())
            })?;
            let ty = value_to_string(ctx, args.first().unwrap_or(&Value::undefined()))?;
            let store = captures.first().copied().unwrap_or(Value::undefined());
            if let Some(listener) = args.get(1) {
                remove_worker_event_listener(ctx, store, &ty, *listener)?;
            }
            Ok(Value::undefined())
        },
    )?;
    set_worker_method(ctx, worker_root, "removeEventListener", remove)
}

fn set_worker_method(
    ctx: &mut NativeCtx<'_>,
    worker_root: otter_vm::PersistentRootId,
    name: &'static str,
    function: Value,
) -> Result<(), NativeError> {
    let worker = worker_persistent_value(ctx, worker_root)?;
    ctx.scope(|mut scope| {
        let worker = scope.value(worker);
        let function = scope.value(function);
        scope.set(worker, name, function)
    })
}

fn dispatch_event_object(
    ctx: &mut NativeCtx<'_>,
    worker: object::JsObject,
    listeners_store: Option<object::JsObject>,
    event: object::JsObject,
) -> Result<(), NativeError> {
    let ty = object::get(event, ctx.heap(), "type")
        .and_then(|value| value.as_string(ctx.heap()))
        .map(|s| s.to_lossy_string(ctx.heap()))
        .unwrap_or_default();
    let handler_key = format!("on{ty}");
    let handler = object::get(worker, ctx.heap(), &handler_key).unwrap_or(Value::undefined());
    let listeners = listeners_store
        .map(|store| worker_event_listeners(ctx, store, &ty))
        .unwrap_or_default();
    ctx.scope(|mut scope| {
        let worker = scope.value(Value::object(worker));
        let event = scope.value(Value::object(event));
        let handler = scope.value(handler);
        let listeners: Vec<Local<'_>> = listeners
            .into_iter()
            .map(|listener| scope.value(listener))
            .collect();
        if scope.is_callable(handler) {
            scope.call(handler, worker, &[event])?;
        }
        for listener in listeners {
            if scope.is_callable(listener) {
                scope.call(listener, worker, &[event])?;
            }
        }
        Ok(())
    })
}

fn add_worker_event_listener(
    ctx: &mut NativeCtx<'_>,
    store_value: Value,
    ty: &str,
    listener: Value,
) -> Result<(), NativeError> {
    ctx.scope(|mut scope| {
        let store = scope.value(store_value);
        let listener = scope.value(listener);
        if !scope.is_object(store) {
            return Ok(());
        }
        let existing_list = scope.get(store, ty)?;
        let list = if scope.is_array(existing_list)? {
            existing_list
        } else {
            let list = scope.array(0)?;
            scope.set(store, ty, list)?;
            list
        };
        let len = scope.array_length(list)?;
        for index in 0..len {
            let existing = scope.index(list, index)?;
            if scope.strict_equals(existing, listener) {
                return Ok(());
            }
        }
        scope.set_index(list, len, listener)
    })
}

fn remove_worker_event_listener(
    ctx: &mut NativeCtx<'_>,
    store_value: Value,
    ty: &str,
    listener: Value,
) -> Result<(), NativeError> {
    ctx.scope(|mut scope| {
        let store = scope.value(store_value);
        let listener = scope.value(listener);
        if !scope.is_object(store) {
            return Ok(());
        }
        let list = scope.get(store, ty)?;
        if !scope.is_array(list)? {
            return Ok(());
        }
        let len = scope.array_length(list)?;
        let mut kept = Vec::with_capacity(len);
        for index in 0..len {
            let existing = scope.index(list, index)?;
            if !scope.strict_equals(existing, listener) {
                kept.push(existing);
            }
        }
        let next = scope.array(kept.len())?;
        for (index, value) in kept.into_iter().enumerate() {
            scope.set_index(next, index, value)?;
        }
        scope.set(store, ty, next)
    })
}

fn worker_event_listeners(ctx: &NativeCtx<'_>, store: object::JsObject, ty: &str) -> Vec<Value> {
    let Some(list) = object::get(store, ctx.heap(), ty).and_then(|value| value.as_array()) else {
        return Vec::new();
    };
    let len = array::len(list, ctx.heap());
    (0..len)
        .map(|idx| array::get(list, ctx.heap(), idx))
        .collect()
}

fn run_worker_entry(
    runtime: &mut Runtime,
    specifier: &str,
) -> Result<(ExecutionResult, ExecutionContext), OtterError> {
    // A `file:` URL names the same file its path form names. Path-shaped
    // specifiers run as files; everything else resolves as a module. The
    // choice is syntactic: probing the filesystem before the capability
    // boundary would leak existence information and race the actual open.
    if let Some(path) = file_url_path(specifier) {
        return runtime.run_file_with_context(path);
    }
    let path = Path::new(specifier);
    if path.is_absolute() || specifier.starts_with("./") || specifier.starts_with("../") {
        runtime.run_file_with_context(PathBuf::from(specifier))
    } else {
        runtime.run_module_with_context(PathBuf::from(specifier))
    }
}

/// The filesystem path a `file:` URL specifier names, or `None` for any other
/// specifier (including a `file:` URL with a host or a malformed path).
fn file_url_path(specifier: &str) -> Option<PathBuf> {
    let url = url::Url::parse(specifier).ok()?;
    (url.scheme() == "file").then(|| url.to_file_path().ok())?
}

fn install_worker_scope_natives(
    runtime: &mut Runtime,
    shared: Arc<WorkerChildShared>,
) -> Result<(), OtterError> {
    let post_shared = shared.clone();
    let post: Arc<NativeFn> = Arc::new(move |ctx, args, _captures| {
        if post_shared.closed.load(Ordering::SeqCst) {
            return Err(type_err("postMessage", "worker is closed".to_string()));
        }
        let value = args.first().copied().unwrap_or_else(Value::undefined);
        let transfers = parse_worker_transfer_list(args.get(1), ctx)?;
        let measured = measure_worker_message(&value, ctx.heap(), &transfers)?;
        let leases = admit_worker_message(
            &post_shared.family,
            &post_shared.account,
            measured,
            "postMessage",
        )?;
        let payload = clone_worker_value(&value, ctx.heap(), &transfers)?;
        let Some(host) = post_shared.host.upgrade() else {
            return Err(type_err(
                "postMessage",
                "parent runtime is gone".to_string(),
            ));
        };
        post_shared
            .parent
            .enqueue(
                WorkerParentDeliverTask {
                    host,
                    id: post_shared.id,
                    event: WorkerEvent::Message(payload),
                    leases,
                },
                RuntimeLiveness::Ref,
            )
            .map_err(|err| type_err("postMessage", err.to_string()))?;
        // Preserve the transferable on enqueue failure. Detaching before the
        // parent queue accepts ownership would turn a reported failure into
        // silent data loss in the worker.
        detach_worker_transfers(&transfers, ctx.heap_mut());
        Ok(Value::undefined())
    });
    runtime.install_native_global_call("postMessage", 2, NativeCall::Dynamic(post))?;

    let close_shared = shared;
    let close: Arc<NativeFn> = Arc::new(move |_ctx, _args, _captures| {
        if !close_shared.closed.swap(true, Ordering::SeqCst) {
            post_worker_terminal(&close_shared, None);
        }
        Ok(Value::undefined())
    });
    runtime.install_native_global_call("close", 0, NativeCall::Dynamic(close))?;
    runtime.set_global("self", runtime.global_this_value());
    runtime.set_global("onmessage", Value::null());
    runtime.set_global("onerror", Value::null());
    Ok(())
}

/// Measure the accounted size of one message graph before any admission or
/// clone. Mirrors [`clone_worker_value`]'s structure: an unsupported value or
/// a cycle fails here, before any resource effect. All arithmetic is checked.
fn measure_worker_message(
    value: &Value,
    heap: &otter_gc::GcHeap,
    transfers: &WorkerTransferList,
) -> Result<u64, NativeError> {
    let mut active = HashSet::new();
    let body = measure_worker_value(value, heap, "$".to_string(), 0, &mut active)?;
    let transfer_entries =
        u64::try_from(transfers.buffers.len()).map_err(|_| measure_overflow())?;
    let transfer_cost = transfer_entries
        .checked_mul(WORKER_MESSAGE_TRANSFER_ENTRY_BYTES)
        .ok_or_else(measure_overflow)?;
    body.checked_add(transfer_cost).ok_or_else(measure_overflow)
}

fn measure_overflow() -> NativeError {
    type_err(
        "structuredClone",
        "message size overflows the accounting range".to_string(),
    )
}

fn measure_checked_sum(total: u64, add: u64) -> Result<u64, NativeError> {
    total.checked_add(add).ok_or_else(measure_overflow)
}

fn measure_worker_value(
    value: &Value,
    heap: &otter_gc::GcHeap,
    path: String,
    depth: usize,
    active: &mut HashSet<RawGc>,
) -> Result<u64, NativeError> {
    if depth > crate::structured_clone::DEFAULT_STRUCTURED_CLONE_MAX_DEPTH {
        return Err(type_err(
            "structuredClone",
            format!("depth limit exceeded at {path}"),
        ));
    }
    if value.is_undefined()
        || value.is_null()
        || value.as_boolean().is_some()
        || value.as_number().is_some()
    {
        return Ok(WORKER_MESSAGE_NODE_BYTES);
    }
    if let Some(b) = value.as_big_int() {
        let digits =
            u64::try_from(b.to_decimal_string(heap).len()).map_err(|_| measure_overflow())?;
        return measure_checked_sum(WORKER_MESSAGE_NODE_BYTES, digits);
    }
    if let Some(s) = value.as_string(heap) {
        let bytes = u64::from(s.len())
            .checked_mul(2)
            .ok_or_else(measure_overflow)?;
        return measure_checked_sum(WORKER_MESSAGE_NODE_BYTES, bytes);
    }
    if let Some(view) = value.as_typed_array(heap) {
        let buffer = Value::array_buffer(view.buffer(heap));
        let buffer =
            measure_worker_value(&buffer, heap, format!("{path}.buffer"), depth + 1, active)?;
        return measure_checked_sum(WORKER_MESSAGE_NODE_BYTES, buffer);
    }
    if let Some(view) = value.as_data_view() {
        let buffer = Value::array_buffer(view.buffer(heap));
        let buffer =
            measure_worker_value(&buffer, heap, format!("{path}.buffer"), depth + 1, active)?;
        return measure_checked_sum(WORKER_MESSAGE_NODE_BYTES, buffer);
    }
    if let Some(buf) = value.as_array_buffer() {
        if buf.as_shared_arc(heap).is_some() {
            return Ok(WORKER_MESSAGE_NODE_BYTES);
        }
        let bytes = buf.with_bytes(heap, |bytes| bytes.len());
        let bytes = u64::try_from(bytes).map_err(|_| measure_overflow())?;
        return measure_checked_sum(WORKER_MESSAGE_NODE_BYTES, bytes);
    }
    if let Some(arr) = value.as_array() {
        if !active.insert(arr.raw()) {
            return Err(type_err(
                "structuredClone",
                format!("cycle detected at {path}"),
            ));
        }
        let len = array::len(arr, heap);
        let mut total = WORKER_MESSAGE_NODE_BYTES;
        for idx in 0..len {
            let element = array::get(arr, heap, idx);
            let child =
                measure_worker_value(&element, heap, format!("{path}[{idx}]"), depth + 1, active)?;
            total = measure_checked_sum(total, child)?;
        }
        active.remove(&arr.raw());
        return Ok(total);
    }
    if let Some(map) = value.as_map() {
        if !active.insert(map.raw()) {
            return Err(type_err(
                "structuredClone",
                format!("cycle detected at {path}"),
            ));
        }
        let entries = collections::map_entries(map, heap);
        let mut total = WORKER_MESSAGE_NODE_BYTES;
        for (idx, (key, entry)) in entries.iter().enumerate() {
            let key_bytes = measure_worker_value(
                key,
                heap,
                format!("{path}<map-key:{idx}>"),
                depth + 1,
                active,
            )?;
            total = measure_checked_sum(total, key_bytes)?;
            let value_bytes = measure_worker_value(
                entry,
                heap,
                format!("{path}<map-value:{idx}>"),
                depth + 1,
                active,
            )?;
            total = measure_checked_sum(total, value_bytes)?;
        }
        active.remove(&map.raw());
        return Ok(total);
    }
    if let Some(set) = value.as_set() {
        if !active.insert(set.raw()) {
            return Err(type_err(
                "structuredClone",
                format!("cycle detected at {path}"),
            ));
        }
        let values = collections::set_values(set, heap);
        let mut total = WORKER_MESSAGE_NODE_BYTES;
        for (idx, element) in values.iter().enumerate() {
            let child = measure_worker_value(
                element,
                heap,
                format!("{path}<set-value:{idx}>"),
                depth + 1,
                active,
            )?;
            total = measure_checked_sum(total, child)?;
        }
        active.remove(&set.raw());
        return Ok(total);
    }
    if let Some(obj) = value.as_object() {
        if !active.insert(obj.raw()) {
            return Err(type_err(
                "structuredClone",
                format!("cycle detected at {path}"),
            ));
        }
        let properties: Vec<(String, Value)> = object::with_properties(obj, heap, |properties| {
            properties
                .enumerable_data_iter()
                .map(|(key, entry)| (key.to_string(), entry))
                .collect()
        });
        let mut total = WORKER_MESSAGE_NODE_BYTES;
        for (key, entry) in properties {
            let key_bytes = u64::try_from(key.len()).map_err(|_| measure_overflow())?;
            total = measure_checked_sum(total, key_bytes)?;
            total = measure_checked_sum(total, WORKER_MESSAGE_NODE_BYTES)?;
            let child =
                measure_worker_value(&entry, heap, format!("{path}.{key}"), depth + 1, active)?;
            total = measure_checked_sum(total, child)?;
        }
        active.remove(&obj.raw());
        return Ok(total);
    }
    Err(type_err(
        "structuredClone",
        format!("unsupported value at {path}: {:?}", value.kind()),
    ))
}

fn worker_event_to_value(
    ctx: &mut NativeCtx<'_>,
    event: WorkerEvent,
) -> Result<Value, NativeError> {
    ctx.scope(|mut scope| {
        let object = materialize_worker_event_in_scope(&mut scope, event)?;
        Ok(scope.finish(object))
    })
}

fn materialize_worker_event_in_scope<'scope, 'rt>(
    scope: &mut NativeScope<'scope, 'rt>,
    event: WorkerEvent,
) -> Result<Local<'scope>, NativeError> {
    let object = scope.object()?;
    match event {
        WorkerEvent::Message(payload) => {
            let data = materialize_worker_payload_in_scope(scope, &payload)?;
            let ty = scope.string("message")?;
            scope.set(object, "type", ty)?;
            scope.set(object, "data", data)?;
        }
        WorkerEvent::Error(message) => {
            let ty = scope.string("error")?;
            let message = scope.string(&message)?;
            scope.set(object, "type", ty)?;
            scope.set(object, "message", message)?;
        }
        WorkerEvent::MessageError(message) => {
            let ty = scope.string("messageerror")?;
            let message = scope.string(&message)?;
            scope.set(object, "type", ty)?;
            scope.set(object, "message", message)?;
        }
    }
    Ok(object)
}

fn parse_worker_transfer_list(
    value: Option<&Value>,
    ctx: &mut NativeCtx<'_>,
) -> Result<WorkerTransferList, NativeError> {
    let Some(value) = value else {
        return Ok(WorkerTransferList::default());
    };
    if value.is_undefined() || value.is_null() {
        return Ok(WorkerTransferList::default());
    }
    let array = value.as_array().ok_or_else(|| {
        type_err(
            "Worker.postMessage",
            "transferList must be an Array".to_string(),
        )
    })?;
    let mut out = WorkerTransferList::default();
    let len = array::len(array, ctx.heap());
    for idx in 0..len {
        let item = array::get(array, ctx.heap(), idx);
        let buffer = item.as_array_buffer().ok_or_else(|| {
            type_err(
                "Worker.postMessage",
                format!("transferList[{idx}] is not an ArrayBuffer"),
            )
        })?;
        if buffer.is_shared() {
            return Err(type_err(
                "Worker.postMessage",
                format!("transferList[{idx}] is a SharedArrayBuffer"),
            ));
        }
        if buffer.is_detached(ctx.heap()) {
            return Err(type_err(
                "Worker.postMessage",
                format!("transferList[{idx}] is detached"),
            ));
        }
        if !out.set.insert(buffer) {
            return Err(type_err(
                "Worker.postMessage",
                format!("duplicate transferable ArrayBuffer at transferList[{idx}]"),
            ));
        }
        out.buffers.push(buffer);
    }
    Ok(out)
}

fn detach_worker_transfers(transfers: &WorkerTransferList, heap: &mut otter_gc::GcHeap) {
    for buffer in &transfers.buffers {
        buffer.detach(heap);
    }
}

fn clone_worker_value(
    value: &Value,
    heap: &otter_gc::GcHeap,
    transfers: &WorkerTransferList,
) -> Result<WorkerPayload, NativeError> {
    let mut active = HashSet::new();
    clone_worker_value_inner(value, heap, transfers, "$".to_string(), 0, &mut active)
}

fn clone_worker_value_inner(
    value: &Value,
    heap: &otter_gc::GcHeap,
    transfers: &WorkerTransferList,
    path: String,
    depth: usize,
    active: &mut HashSet<RawGc>,
) -> Result<WorkerPayload, NativeError> {
    if depth > crate::structured_clone::DEFAULT_STRUCTURED_CLONE_MAX_DEPTH {
        return Err(type_err(
            "structuredClone",
            format!("depth limit exceeded at {path}"),
        ));
    }
    if value.is_undefined() {
        return Ok(WorkerPayload::Undefined);
    }
    if value.is_null() {
        return Ok(WorkerPayload::Null);
    }
    if let Some(b) = value.as_boolean() {
        return Ok(WorkerPayload::Boolean(b));
    }
    if let Some(n) = value.as_number() {
        return Ok(WorkerPayload::Number(n.into()));
    }
    if let Some(b) = value.as_big_int() {
        return Ok(WorkerPayload::BigInt(b.to_decimal_string(heap)));
    }
    if let Some(s) = value.as_string(heap) {
        return Ok(WorkerPayload::String(s.to_lossy_string(heap)));
    }
    if let Some(view) = value.as_typed_array(heap) {
        let buffer = clone_worker_value_inner(
            &Value::array_buffer(view.buffer(heap)),
            heap,
            transfers,
            format!("{path}.buffer"),
            depth + 1,
            active,
        )?;
        return Ok(WorkerPayload::TypedArray {
            kind: view.kind(),
            buffer: Box::new(buffer),
            byte_offset: view.raw_byte_offset(heap),
            length: view.raw_length(heap),
        });
    }
    if let Some(view) = value.as_data_view() {
        let buffer = clone_worker_value_inner(
            &Value::array_buffer(view.buffer(heap)),
            heap,
            transfers,
            format!("{path}.buffer"),
            depth + 1,
            active,
        )?;
        return Ok(WorkerPayload::DataView {
            buffer: Box::new(buffer),
            byte_offset: view.byte_offset(heap),
            byte_length: view.byte_length(heap),
        });
    }
    if let Some(buf) = value.as_array_buffer() {
        if let Some(shared) = buf.as_shared_arc(heap) {
            return Ok(WorkerPayload::SharedArrayBuffer(shared));
        }
        if transfers.set.contains(&buf) && buf.is_detached(heap) {
            return Err(type_err(
                "structuredClone",
                format!("cannot transfer detached ArrayBuffer at {path}"),
            ));
        }
        return Ok(WorkerPayload::ArrayBuffer(
            buf.with_bytes(heap, |bytes| bytes.to_vec()),
        ));
    }
    if let Some(arr) = value.as_array() {
        if !active.insert(arr.raw()) {
            return Err(type_err(
                "structuredClone",
                format!("cycle detected at {path}"),
            ));
        }
        let len = array::len(arr, heap);
        let values: Vec<Value> = (0..len).map(|idx| array::get(arr, heap, idx)).collect();
        let mut cloned = Vec::with_capacity(values.len());
        for (idx, value) in values.iter().enumerate() {
            cloned.push(clone_worker_value_inner(
                value,
                heap,
                transfers,
                format!("{path}[{idx}]"),
                depth + 1,
                active,
            )?);
        }
        active.remove(&arr.raw());
        return Ok(WorkerPayload::Array(cloned));
    }
    if let Some(map) = value.as_map() {
        if !active.insert(map.raw()) {
            return Err(type_err(
                "structuredClone",
                format!("cycle detected at {path}"),
            ));
        }
        let entries = collections::map_entries(map, heap);
        let mut cloned = Vec::with_capacity(entries.len());
        for (idx, (key, value)) in entries.iter().enumerate() {
            cloned.push((
                clone_worker_value_inner(
                    key,
                    heap,
                    transfers,
                    format!("{path}<map-key:{idx}>"),
                    depth + 1,
                    active,
                )?,
                clone_worker_value_inner(
                    value,
                    heap,
                    transfers,
                    format!("{path}<map-value:{idx}>"),
                    depth + 1,
                    active,
                )?,
            ));
        }
        active.remove(&map.raw());
        return Ok(WorkerPayload::Map(cloned));
    }
    if let Some(set) = value.as_set() {
        if !active.insert(set.raw()) {
            return Err(type_err(
                "structuredClone",
                format!("cycle detected at {path}"),
            ));
        }
        let values = collections::set_values(set, heap);
        let mut cloned = Vec::with_capacity(values.len());
        for (idx, value) in values.iter().enumerate() {
            cloned.push(clone_worker_value_inner(
                value,
                heap,
                transfers,
                format!("{path}<set-value:{idx}>"),
                depth + 1,
                active,
            )?);
        }
        active.remove(&set.raw());
        return Ok(WorkerPayload::Set(cloned));
    }
    if let Some(obj) = value.as_object() {
        if !active.insert(obj.raw()) {
            return Err(type_err(
                "structuredClone",
                format!("cycle detected at {path}"),
            ));
        }
        let properties: Vec<(String, Value)> = object::with_properties(obj, heap, |properties| {
            properties
                .enumerable_data_iter()
                .map(|(key, value)| (key.to_string(), value))
                .collect()
        });
        let mut cloned = Vec::with_capacity(properties.len());
        for (key, value) in properties {
            cloned.push((
                key.clone(),
                clone_worker_value_inner(
                    &value,
                    heap,
                    transfers,
                    format!("{path}.{key}"),
                    depth + 1,
                    active,
                )?,
            ));
        }
        active.remove(&obj.raw());
        return Ok(WorkerPayload::Object(cloned));
    }
    Err(type_err(
        "structuredClone",
        format!("unsupported value at {path}: {:?}", value.kind()),
    ))
}

fn materialize_worker_payload(
    ctx: &mut NativeCtx<'_>,
    payload: &WorkerPayload,
) -> Result<Value, NativeError> {
    ctx.scope(|mut scope| {
        let value = materialize_worker_payload_in_scope(&mut scope, payload)?;
        Ok(scope.finish(value))
    })
}

fn materialize_worker_payload_in_scope<'scope, 'rt>(
    scope: &mut NativeScope<'scope, 'rt>,
    payload: &WorkerPayload,
) -> Result<Local<'scope>, NativeError> {
    match payload {
        WorkerPayload::Undefined => Ok(scope.undefined()),
        WorkerPayload::Null => Ok(scope.null()),
        WorkerPayload::Boolean(value) => Ok(scope.boolean(*value)),
        WorkerPayload::Number(value) => Ok(scope.number(value.as_f64())),
        WorkerPayload::BigInt(value) => scope.bigint_decimal(value),
        WorkerPayload::String(value) => scope.string(value),
        WorkerPayload::Array(values) => {
            let array = scope.array(values.len())?;
            for (index, value) in values.iter().enumerate() {
                let value = materialize_worker_payload_in_scope(scope, value)?;
                scope.set_index(array, index, value)?;
            }
            Ok(array)
        }
        WorkerPayload::Object(properties) => {
            let object = scope.object()?;
            for (key, value) in properties {
                let value = materialize_worker_payload_in_scope(scope, value)?;
                scope.set(object, key, value)?;
            }
            Ok(object)
        }
        WorkerPayload::Map(entries) => {
            let map = scope.map_collection()?;
            for (key, value) in entries {
                let key = materialize_worker_payload_in_scope(scope, key)?;
                let value = materialize_worker_payload_in_scope(scope, value)?;
                scope.map_set(map, key, value)?;
            }
            Ok(map)
        }
        WorkerPayload::Set(values) => {
            let set = scope.set_collection()?;
            for value in values {
                let value = materialize_worker_payload_in_scope(scope, value)?;
                scope.set_add(set, value)?;
            }
            Ok(set)
        }
        WorkerPayload::ArrayBuffer(bytes) => scope.array_buffer_from_bytes(bytes.to_vec()),
        WorkerPayload::SharedArrayBuffer(body) => scope.shared_array_buffer(body.clone()),
        WorkerPayload::TypedArray {
            kind,
            buffer,
            byte_offset,
            length,
        } => {
            let buffer = materialize_worker_payload_in_scope(scope, buffer)?;
            scope.typed_array_view(buffer, *kind, *byte_offset, *length)
        }
        WorkerPayload::DataView {
            buffer,
            byte_offset,
            byte_length,
        } => {
            let buffer = materialize_worker_payload_in_scope(scope, buffer)?;
            scope.data_view(buffer, *byte_offset, *byte_length)
        }
    }
}

fn value_to_string(ctx: &mut NativeCtx<'_>, value: &Value) -> Result<String, NativeError> {
    if let Some(s) = value.as_string(ctx.heap()) {
        Ok(s.to_lossy_string(ctx.heap()))
    } else if value.is_undefined() {
        Ok("undefined".to_string())
    } else {
        Ok(value.display_string(ctx.heap()))
    }
}

fn type_err(name: &'static str, reason: String) -> NativeError {
    NativeError::TypeError { name, reason }
}

/// Sendable handle to one worker isolate.
#[derive(Clone, Debug)]
pub struct Worker {
    id: WorkerId,
    handle: RuntimeHandle,
}

/// Worker shutdown / leak diagnostic snapshot.
#[derive(Debug, Clone)]
pub struct WorkerShutdownReport {
    /// Worker id.
    pub worker_id: WorkerId,
    /// Runtime handle references still pointing at this isolate.
    pub live_runtime_handles: usize,
    /// Commands queued from the handle side at report time.
    pub queued_messages: usize,
    /// Runtime activity at the time of the report.
    pub activity: RuntimeActivityStats,
    /// Transferable resources still owned by the worker boundary.
    ///
    /// This is always zero until message ports and ArrayBuffer
    /// transfer ownership land; keeping it in the report now fixes
    /// the public diagnostic shape.
    pub leaked_transferables: usize,
}

impl WorkerShutdownReport {
    /// `true` if any shutdown-relevant work/resource is still live.
    #[must_use]
    pub fn has_leaks(&self) -> bool {
        self.activity.queued_commands > 0
            || self.queued_messages > 0
            || self.live_runtime_handles > 1
            || self.activity.pending_ref_host_ops > 0
            || self.activity.pending_unref_host_ops > 0
            || self.activity.pending_ref_timers > 0
            || self.activity.pending_unref_timers > 0
            || self.activity.pending_dynamic_module_jobs > 0
            || self.leaked_transferables > 0
    }
}

impl Worker {
    /// Start configuring a worker isolate.
    #[must_use]
    pub fn builder() -> WorkerBuilder {
        WorkerBuilder::default()
    }

    /// Build a worker with default runtime configuration.
    ///
    /// # Errors
    /// Returns [`OtterError`] if the runtime isolate cannot start.
    pub fn new() -> Result<Self, OtterError> {
        Self::builder().build()
    }

    /// Host-side worker id.
    #[must_use]
    pub const fn id(&self) -> WorkerId {
        self.id
    }

    /// Run a file from disk on this worker isolate.
    ///
    /// # Errors
    /// See [`OtterError`].
    pub async fn run_file(&self, path: impl AsRef<Path>) -> Result<ExecutionResult, OtterError> {
        self.handle.run_file(path.as_ref().to_path_buf()).await
    }

    /// Run an ES module entry file on this worker isolate.
    ///
    /// # Errors
    /// See [`OtterError`].
    pub async fn run_module(&self, path: impl AsRef<Path>) -> Result<ExecutionResult, OtterError> {
        self.handle.run_module(path.as_ref().to_path_buf()).await
    }

    /// Run JavaScript source on this worker isolate.
    ///
    /// # Errors
    /// See [`OtterError`].
    pub async fn run_script(&self, source: &str) -> Result<ExecutionResult, OtterError> {
        self.handle
            .run_script(
                SourceInput::from_javascript(source).with_top_level_await(),
                worker_specifier(self.id),
            )
            .await
    }

    /// Run TypeScript source on this worker isolate.
    ///
    /// # Errors
    /// See [`OtterError`].
    pub async fn run_typescript(&self, source: &str) -> Result<ExecutionResult, OtterError> {
        self.handle
            .run_script(
                SourceInput::from_typescript(source).with_top_level_await(),
                worker_specifier(self.id),
            )
            .await
    }

    /// Evaluate JavaScript source on this worker isolate.
    ///
    /// # Errors
    /// See [`OtterError`].
    pub async fn eval(&self, source: &str) -> Result<ExecutionResult, OtterError> {
        self.handle
            .eval(SourceInput::from_javascript(source).with_top_level_await())
            .await
    }

    /// Validate that a message already crossed the structured-clone
    /// boundary. Full JS delivery lands with message ports in a later
    /// task-92 slice.
    #[must_use]
    pub fn accepts_message(&self, _message: &StructuredCloneValue) -> bool {
        true
    }

    /// Validate transfer-list metadata for a future message send.
    #[must_use]
    pub fn accepts_transfer_list(&self, transfers: &StructuredCloneTransferList) -> bool {
        transfers.validate().is_ok()
    }

    /// Cooperative cancellation for this worker isolate.
    pub fn interrupt(&self) {
        self.handle.interrupt();
    }

    /// Snapshot worker activity counters.
    #[must_use]
    pub fn activity_stats(&self) -> RuntimeActivityStats {
        self.handle.activity_stats()
    }

    /// Clone the resource account shared by this worker's runtime family.
    #[must_use]
    pub fn resource_account(&self) -> ResourceAccount {
        self.handle.resource_account()
    }

    /// Capture deterministic resource usage for this worker's shared account.
    #[must_use]
    pub fn resource_snapshot(&self) -> ResourceSnapshot {
        self.handle.resource_snapshot()
    }

    /// Snapshot shutdown diagnostics without tearing down the worker.
    #[must_use]
    pub fn shutdown_report(&self) -> WorkerShutdownReport {
        let activity = self.activity_stats();
        WorkerShutdownReport {
            worker_id: self.id,
            live_runtime_handles: self.handle.live_handle_count(),
            queued_messages: activity.queued_commands,
            activity,
            leaked_transferables: 0,
        }
    }

    /// Drop down to the sendable runtime handle.
    #[must_use]
    pub fn handle(&self) -> &RuntimeHandle {
        &self.handle
    }
}

/// Builder for one worker isolate.
#[derive(Debug, Clone, Default)]
pub struct WorkerBuilder {
    runtime: RuntimeBuilder,
}

impl WorkerBuilder {
    /// Replace the shared resource ledger with a fresh account using `limits`.
    #[must_use]
    pub fn resource_limits(mut self, limits: ResourceLimits) -> Self {
        self.runtime = self.runtime.resource_limits(limits);
        self
    }

    /// Use an existing account shared with sibling runtime builders.
    #[must_use]
    pub fn resource_account(mut self, account: ResourceAccount) -> Self {
        self.runtime = self.runtime.resource_account(account);
        self
    }

    /// Set finite per-isolate capacities for guaranteed terminal work,
    /// in-flight host operations, and live timers. Zero disables admission for
    /// that class.
    #[must_use]
    pub fn completion_capacities(
        mut self,
        guaranteed: usize,
        host_operations: usize,
        timers: usize,
    ) -> Self {
        self.runtime = self
            .runtime
            .completion_capacities(guaranteed, host_operations, timers);
        self
    }

    /// Replace the capability set.
    #[must_use]
    pub fn capabilities(mut self, caps: CapabilitySet) -> Self {
        self.runtime = self.runtime.capabilities(caps);
        self
    }

    /// Hard heap cap. `0` disables the cap.
    #[must_use]
    pub fn max_heap_bytes(mut self, bytes: u64) -> Self {
        self.runtime = self.runtime.max_heap_bytes(bytes);
        self
    }

    /// Per-command timeout. `Duration::ZERO` disables the timeout.
    #[must_use]
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.runtime = self.runtime.timeout(timeout);
        self
    }

    /// JS call-stack depth cap.
    #[must_use]
    pub fn max_stack_depth(mut self, depth: u32) -> Self {
        self.runtime = self.runtime.max_stack_depth(depth);
        self
    }

    /// Override the module-loader configuration.
    #[must_use]
    pub fn module_loader(mut self, loader: module_loader::LoaderConfig) -> Self {
        self.runtime = self.runtime.module_loader(loader);
        self
    }

    /// Construct a worker isolate.
    ///
    /// # Errors
    /// Returns [`OtterError`] when config validation or isolate
    /// startup fails.
    pub fn build(self) -> Result<Worker, OtterError> {
        let id = next_worker_id().ok_or_else(|| OtterError::Internal {
            code: "WORKER_ID_EXHAUSTED".to_string(),
            message: "worker id space is exhausted".to_string(),
        })?;
        let handle = self.runtime.build_worker_handle()?;
        Ok(Worker { id, handle })
    }
}

/// Round-robin pool of independent worker isolates.
#[derive(Clone, Debug)]
pub struct OtterPool {
    workers: Arc<[Worker]>,
    next: Arc<AtomicUsize>,
}

impl OtterPool {
    /// Start configuring an isolate pool.
    #[must_use]
    pub fn builder() -> OtterPoolBuilder {
        OtterPoolBuilder::default()
    }

    /// Number of workers in the pool.
    #[must_use]
    pub fn len(&self) -> usize {
        self.workers.len()
    }

    /// `true` when the pool has no workers.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.workers.is_empty()
    }

    /// Borrow the worker slice for diagnostics/tests.
    #[must_use]
    pub fn workers(&self) -> &[Worker] {
        &self.workers
    }

    /// Clone the resource account shared by every worker in this pool.
    #[must_use]
    pub fn resource_account(&self) -> ResourceAccount {
        self.workers
            .first()
            .expect("pool construction rejects an empty worker set")
            .resource_account()
    }

    /// Capture aggregate resource usage for all workers in this pool.
    #[must_use]
    pub fn resource_snapshot(&self) -> ResourceSnapshot {
        self.workers
            .first()
            .expect("pool construction rejects an empty worker set")
            .resource_snapshot()
    }

    /// Snapshot shutdown diagnostics for every worker.
    #[must_use]
    pub fn shutdown_reports(&self) -> Vec<WorkerShutdownReport> {
        self.workers.iter().map(Worker::shutdown_report).collect()
    }

    /// Pick the next worker using round-robin routing.
    #[must_use]
    pub fn next_worker(&self) -> Worker {
        let idx = self.next.fetch_add(1, Ordering::Relaxed) % self.workers.len();
        self.workers[idx].clone()
    }

    /// Run JavaScript on the next worker.
    ///
    /// # Errors
    /// See [`OtterError`].
    pub async fn run_script(&self, source: &str) -> Result<ExecutionResult, OtterError> {
        self.next_worker().run_script(source).await
    }

    /// Run TypeScript on the next worker.
    ///
    /// # Errors
    /// See [`OtterError`].
    pub async fn run_typescript(&self, source: &str) -> Result<ExecutionResult, OtterError> {
        self.next_worker().run_typescript(source).await
    }

    /// Run a file on the next worker.
    ///
    /// # Errors
    /// See [`OtterError`].
    pub async fn run_file(&self, path: impl AsRef<Path>) -> Result<ExecutionResult, OtterError> {
        self.next_worker().run_file(path.as_ref()).await
    }
}

/// Builder for [`OtterPool`].
#[derive(Debug, Clone)]
pub struct OtterPoolBuilder {
    runtime: RuntimeBuilder,
    workers: usize,
}

impl Default for OtterPoolBuilder {
    fn default() -> Self {
        Self {
            runtime: RuntimeBuilder::default(),
            workers: 1,
        }
    }
}

impl OtterPoolBuilder {
    /// Number of worker isolates to spawn. Values below one are
    /// rejected at [`Self::build`].
    #[must_use]
    pub fn workers(mut self, workers: usize) -> Self {
        self.workers = workers;
        self
    }

    /// Replace the pool's shared resource ledger with a fresh limited account.
    #[must_use]
    pub fn resource_limits(mut self, limits: ResourceLimits) -> Self {
        self.runtime = self.runtime.resource_limits(limits);
        self
    }

    /// Use an existing account shared by every worker in the pool.
    #[must_use]
    pub fn resource_account(mut self, account: ResourceAccount) -> Self {
        self.runtime = self.runtime.resource_account(account);
        self
    }

    /// Set finite per-isolate capacities for guaranteed terminal work,
    /// in-flight host operations, and live timers. Zero disables admission for
    /// that class.
    #[must_use]
    pub fn completion_capacities(
        mut self,
        guaranteed: usize,
        host_operations: usize,
        timers: usize,
    ) -> Self {
        self.runtime = self
            .runtime
            .completion_capacities(guaranteed, host_operations, timers);
        self
    }

    /// Replace the capability set for every worker.
    #[must_use]
    pub fn capabilities(mut self, caps: CapabilitySet) -> Self {
        self.runtime = self.runtime.capabilities(caps);
        self
    }

    /// Hard heap cap per worker. `0` disables each cap.
    #[must_use]
    pub fn max_heap_bytes(mut self, bytes: u64) -> Self {
        self.runtime = self.runtime.max_heap_bytes(bytes);
        self
    }

    /// Per-command timeout for every worker.
    #[must_use]
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.runtime = self.runtime.timeout(timeout);
        self
    }

    /// JS call-stack depth cap for every worker.
    #[must_use]
    pub fn max_stack_depth(mut self, depth: u32) -> Self {
        self.runtime = self.runtime.max_stack_depth(depth);
        self
    }

    /// Override the module-loader configuration for every worker.
    #[must_use]
    pub fn module_loader(mut self, loader: module_loader::LoaderConfig) -> Self {
        self.runtime = self.runtime.module_loader(loader);
        self
    }

    /// Construct an isolate pool.
    ///
    /// # Errors
    /// Returns [`OtterError`] when config validation or isolate
    /// startup fails. A zero-worker pool is rejected as a config
    /// error because routing could not make progress.
    pub fn build(self) -> Result<OtterPool, OtterError> {
        if self.workers == 0 {
            return Err(OtterError::Config {
                reason: crate::ConfigError::ConflictingCapabilities {
                    message: "worker pool must contain at least one worker".to_string(),
                },
            });
        }

        let mut workers = Vec::with_capacity(self.workers);
        for _ in 0..self.workers {
            workers.push(
                WorkerBuilder {
                    runtime: self.runtime.clone(),
                }
                .build()?,
            );
        }

        Ok(OtterPool {
            workers: workers.into(),
            next: Arc::new(AtomicUsize::new(0)),
        })
    }
}

fn worker_specifier(id: WorkerId) -> String {
    format!("<worker:{}>", id.get())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Otter;
    use std::fs;

    fn assert_send_sync_static<T: Send + Sync + 'static>() {}

    #[test]
    fn worker_handles_are_send_sync_static() {
        assert_send_sync_static::<Worker>();
        assert_send_sync_static::<OtterPool>();
        assert_send_sync_static::<WorkerShutdownReport>();
    }

    #[test]
    fn a_child_isolate_inherits_the_parent_work_budget() {
        let parent_budget = otter_vm::WorkBudget {
            on_exceeded: otter_vm::WorkBudgetExceededAction::Yield,
            max_work_units_per_turn: Some(64),
            ..otter_vm::WorkBudget::default()
        };
        let config = RuntimeConfig {
            work_budget: parent_budget,
            ..RuntimeConfig::default()
        };
        let child = configure_worker_child(config, None, None);

        // Capabilities narrow on the way down; the CPU policy does not, so a
        // worker cannot buy itself a longer slice than its parent runs under.
        assert_eq!(child.work_budget, parent_budget);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn two_workers_run_concurrently_with_separate_globals() {
        let left = Worker::new().unwrap();
        let right = Worker::new().unwrap();

        let (left_result, right_result) = tokio::join!(
            left.run_script("globalThis.workerSlot = 7; workerSlot"),
            right.run_script("typeof globalThis.workerSlot"),
        );

        assert_eq!(left_result.unwrap().completion_string(), "7");
        assert_eq!(right_result.unwrap().completion_string(), "undefined");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pool_routes_round_robin_across_isolates() {
        let pool = OtterPool::builder().workers(2).build().unwrap();

        let first = pool.next_worker();
        let second = pool.next_worker();
        let third = pool.next_worker();

        assert_eq!(pool.len(), 2);
        assert_ne!(first.id(), second.id());
        assert_eq!(first.id(), third.id());

        first.run_script("globalThis.onlyFirst = 1").await.unwrap();
        let second_read = second
            .run_script("typeof globalThis.onlyFirst")
            .await
            .unwrap();

        assert_eq!(second_read.completion_string(), "undefined");
    }

    #[test]
    fn zero_worker_pool_is_rejected() {
        let err = OtterPool::builder().workers(0).build().unwrap_err();
        assert!(matches!(err, OtterError::Config { .. }));
    }

    #[test]
    fn worker_message_boundary_accepts_only_structured_clone_payload() {
        let worker = Worker::new().unwrap();
        let message = StructuredCloneValue::Object(vec![crate::StructuredCloneProperty {
            key: "ok".to_string(),
            value: StructuredCloneValue::Boolean(true),
        }]);
        let transfers = StructuredCloneTransferList::empty();

        assert!(worker.accepts_message(&message));
        assert!(worker.accepts_transfer_list(&transfers));
    }

    #[test]
    fn shutdown_report_marks_pending_timer_as_leak() {
        use crate::event_loop::TimerRequest;

        let worker = Worker::new().unwrap();
        let token = worker.handle().schedule_timer(TimerRequest {
            delay: Duration::from_secs(60),
            repeat: None,
        });

        let report = worker.shutdown_report();

        assert_eq!(report.worker_id, worker.id());
        assert_eq!(report.live_runtime_handles, 1);
        assert_eq!(report.queued_messages, report.activity.queued_commands);
        assert!(report.has_leaks());
        assert_eq!(report.activity.pending_ref_timers, 1);
        assert!(worker.handle().cancel_timer(token));
    }

    #[test]
    fn shutdown_report_tracks_live_handle_refs() {
        let worker = Worker::new().unwrap();
        let cloned = worker.clone();

        let report = worker.shutdown_report();

        assert_eq!(report.live_runtime_handles, 2);
        assert!(report.has_leaks());
        drop(cloned);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn global_worker_receives_worker_message() {
        let dir = tempfile::tempdir().unwrap();
        let worker_path = dir.path().join("worker.js");
        fs::write(&worker_path, "postMessage('ready');").unwrap();
        let entry = dir.path().join("entry.js");
        fs::write(
            &entry,
            format!(
                r#"
                let got = "pending";
                const w = new Worker({:?});
                w.onerror = (event) => {{
                  got = "ERR:" + event.message;
                  w.terminate();
                }};
                w.onmessage = (event) => {{
                  got = event.data;
                  w.terminate();
                }};
                setTimeout(() => {{
                  if (got !== "ready") throw "bad worker message: " + got;
                }}, 20);
                "#,
                worker_path.to_string_lossy()
            ),
        )
        .unwrap();

        let otter = Otter::builder()
            .capabilities(CapabilitySet::allow_all())
            .build()
            .unwrap();
        otter.run_file(&entry).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn global_worker_event_listener_surface_dispatches_and_removes() {
        let dir = tempfile::tempdir().unwrap();
        let worker_path = dir.path().join("worker.js");
        fs::write(&worker_path, "postMessage('ready');").unwrap();
        let entry = dir.path().join("entry.js");
        fs::write(
            &entry,
            format!(
                r#"
                let count = 0;
                let got = "pending";
                const w = new Worker({:?});
                const removed = () => {{ count += 100; }};
                w.addEventListener("message", removed);
                w.removeEventListener("message", removed);
                w.onmessage = () => {{ count += 10; }};
                w.addEventListener("message", (event) => {{
                  count += 1;
                  got = event.data;
                  w.terminate();
                }});
                setTimeout(() => {{
                  if (got !== "ready") throw "listener did not receive message";
                  if (count !== 11) throw "bad listener count: " + count;
                }}, 20);
                "#,
                worker_path.to_string_lossy()
            ),
        )
        .unwrap();

        let otter = Otter::builder()
            .capabilities(CapabilitySet::allow_all())
            .build()
            .unwrap();
        otter.run_file(&entry).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn global_worker_parent_to_worker_message() {
        let dir = tempfile::tempdir().unwrap();
        let worker_path = dir.path().join("worker.js");
        fs::write(
            &worker_path,
            "globalThis.onmessage = (event) => postMessage(event.data.answer + 1);",
        )
        .unwrap();
        let entry = dir.path().join("entry.js");
        fs::write(
            &entry,
            format!(
                r#"
                let got = 0;
                const w = new Worker({:?});
                w.onerror = (event) => {{
                  got = -1;
                  w.terminate();
                }};
                w.onmessage = (event) => {{
                  got = event.data;
                  w.terminate();
                }};
                w.postMessage({{ answer: 41 }});
                setTimeout(() => {{
                  if (got !== 42) throw "bad response: " + got;
                }}, 20);
                "#,
                worker_path.to_string_lossy()
            ),
        )
        .unwrap();

        let otter = Otter::builder()
            .capabilities(CapabilitySet::allow_all())
            .build()
            .unwrap();
        otter.run_file(&entry).await.unwrap();
    }

    /// Run with `OTTER_GC_STRESS=1..16` to force relocation throughout nested
    /// Array/Object/Map/Set/BigInt materialization and event allocation.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn worker_message_event_rooting_survives_gc_relocation() {
        let dir = tempfile::tempdir().unwrap();
        let worker_path = dir.path().join("worker.js");
        fs::write(
            &worker_path,
            r#"
            globalThis.onmessage = (event) => {
              if (event.type !== "message") throw "bad event type";
              const arrayValue = event.data.array[0].nested.value;
              const mapValue = event.data.map.get("key").value;
              const setValue = event.data.set.values().next().value.value;
              if (event.data.big !== 123456789012345678901234567890n) {
                throw "bad BigInt payload";
              }
              postMessage(event.type + ":" + arrayValue + ":" + mapValue + ":" + setValue);
            };
            "#,
        )
        .unwrap();
        let entry = dir.path().join("entry.js");
        fs::write(
            &entry,
            format!(
                r#"
                let got = "pending";
                const w = new Worker({:?});
                w.onerror = (event) => {{
                  got = "ERR:" + event.message;
                  w.terminate();
                }};
                w.onmessage = (event) => {{
                  got = event.data;
                  w.terminate();
                }};
                w.postMessage({{
                  array: [{{ nested: {{ value: "array" }} }}],
                  map: new Map([["key", {{ value: "map" }}]]),
                  set: new Set([{{ value: "set" }}]),
                  big: 123456789012345678901234567890n,
                }});
                setTimeout(() => {{
                  if (got !== "message:array:map:set") {{
                    throw "bad rooted worker event: " + got;
                  }}
                }}, 20);
                "#,
                worker_path.to_string_lossy()
            ),
        )
        .unwrap();

        let otter = Otter::builder()
            .capabilities(CapabilitySet::allow_all())
            .build()
            .unwrap();
        otter.run_file(&entry).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn global_worker_terminate_interrupts_infinite_loop() {
        let dir = tempfile::tempdir().unwrap();
        let worker_path = dir.path().join("worker.js");
        fs::write(&worker_path, "while (true) {}").unwrap();
        let entry = dir.path().join("entry.js");
        fs::write(
            &entry,
            format!(
                r#"
                const w = new Worker({:?});
                w.terminate();
                "terminated";
                "#,
                worker_path.to_string_lossy()
            ),
        )
        .unwrap();

        let otter = Otter::builder()
            .capabilities(CapabilitySet::allow_all())
            .build()
            .unwrap();
        otter.run_file(&entry).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn global_worker_terminate_interrupts_atomics_wait() {
        let dir = tempfile::tempdir().unwrap();
        let worker_path = dir.path().join("worker.js");
        fs::write(
            &worker_path,
            r#"
            globalThis.onmessage = (event) => {
              const view = new Int32Array(event.data);
              Atomics.wait(view, 0, 0);
              postMessage("after-wait");
            };
            "#,
        )
        .unwrap();
        let entry = dir.path().join("entry.js");
        fs::write(
            &entry,
            format!(
                r#"
                const sab = new SharedArrayBuffer(4);
                let got = "pending";
                const w = new Worker({:?});
                w.onmessage = (event) => {{
                  got = event.data;
                }};
                w.onerror = () => {{
                  got = "interrupted";
                }};
                w.postMessage(sab);
                setTimeout(() => w.terminate(), 5);
                setTimeout(() => {{
                  if (got === "after-wait") throw "Atomics.wait was not cancelled";
                }}, 20);
                "#,
                worker_path.to_string_lossy()
            ),
        )
        .unwrap();

        let otter = Otter::builder()
            .capabilities(CapabilitySet::allow_all())
            .build()
            .unwrap();
        otter.run_file(&entry).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn terminating_one_worker_does_not_cancel_another_atomics_agent() {
        let dir = tempfile::tempdir().unwrap();
        let worker_path = dir.path().join("worker.js");
        fs::write(
            &worker_path,
            r#"
            globalThis.onmessage = (event) => {
              const view = new Int32Array(event.data);
              postMessage("ready");
              const outcome = Atomics.wait(view, 0, 0);
              postMessage("wait:" + outcome);
            };
            "#,
        )
        .unwrap();
        let entry = dir.path().join("entry.js");
        fs::write(
            &entry,
            format!(
                r#"
                const sab = new SharedArrayBuffer(4);
                const view = new Int32Array(sab);
                const cancelled = new Worker({0:?});
                const survivor = new Worker({0:?});
                let ready = 0;
                let survivorOutcome = "pending";

                const sawReady = () => {{
                  ready++;
                  if (ready === 2) {{
                    cancelled.terminate();
                    setTimeout(() => {{
                      const woken = Atomics.notify(view, 0, 1);
                      if (woken !== 1) throw "survivor waiter was cancelled";
                    }}, 5);
                  }}
                }};
                cancelled.onmessage = (event) => {{
                  if (event.data === "ready") sawReady();
                }};
                survivor.onmessage = (event) => {{
                  if (event.data === "ready") sawReady();
                  else {{
                    survivorOutcome = event.data;
                    survivor.terminate();
                  }}
                }};
                survivor.onerror = (event) => {{
                  survivorOutcome = "ERR:" + event.message;
                }};
                cancelled.postMessage(sab);
                survivor.postMessage(sab);
                setTimeout(() => {{
                  if (survivorOutcome !== "wait:ok") {{
                    survivor.terminate();
                    throw "bad survivor outcome: " + survivorOutcome;
                  }}
                }}, 80);
                "#,
                worker_path.to_string_lossy()
            ),
        )
        .unwrap();

        let otter = Otter::builder()
            .capabilities(CapabilitySet::allow_all())
            .build()
            .unwrap();
        otter.run_file(&entry).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn global_worker_shares_shared_array_buffer() {
        let dir = tempfile::tempdir().unwrap();
        let worker_path = dir.path().join("worker.js");
        fs::write(
            &worker_path,
            r#"
            globalThis.onmessage = (event) => {
              const view = new Int32Array(event.data);
              Atomics.store(view, 0, 7);
              Atomics.notify(view, 0, 1);
              postMessage("stored");
            };
            "#,
        )
        .unwrap();
        let entry = dir.path().join("entry.js");
        fs::write(
            &entry,
            format!(
                r#"
                const sab = new SharedArrayBuffer(4);
                const view = new Int32Array(sab);
                let got = "pending";
                const w = new Worker({:?});
                w.onerror = (event) => {{
                  got = "ERR:" + event.message;
                  w.terminate();
                }};
                w.onmessage = (event) => {{
                  got = event.data;
                  w.terminate();
                }};
                w.postMessage(sab);
                setTimeout(() => {{
                  if (got !== "stored") throw "bad response: " + got;
                  if (Atomics.load(view, 0) !== 7) throw "shared write missing";
                }}, 20);
                "#,
                worker_path.to_string_lossy()
            ),
        )
        .unwrap();

        let otter = Otter::builder()
            .capabilities(CapabilitySet::allow_all())
            .build()
            .unwrap();
        otter.run_file(&entry).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn global_worker_transfers_array_buffer_and_detaches_sender() {
        let dir = tempfile::tempdir().unwrap();
        let worker_path = dir.path().join("worker.js");
        fs::write(
            &worker_path,
            r#"
            globalThis.onmessage = (event) => {
              const view = new Uint8Array(event.data);
              postMessage([event.data.byteLength, view[0], view[1], view[2]]);
            };
            "#,
        )
        .unwrap();
        let entry = dir.path().join("entry.js");
        fs::write(
            &entry,
            format!(
                r#"
                const buffer = new ArrayBuffer(3);
                const view = new Uint8Array(buffer);
                view[0] = 4;
                view[1] = 5;
                view[2] = 6;
                let got = null;
                const w = new Worker({:?});
                w.onerror = (event) => {{
                  got = "ERR:" + event.message;
                  w.terminate();
                }};
                w.onmessage = (event) => {{
                  got = event.data.join(",");
                  w.terminate();
                }};
                w.postMessage(buffer, [buffer]);
                if (buffer.byteLength !== 0) throw "sender buffer was not detached";
                setTimeout(() => {{
                  if (got !== "3,4,5,6") throw "bad transfer result: " + got;
                }}, 20);
                "#,
                worker_path.to_string_lossy()
            ),
        )
        .unwrap();

        let otter = Otter::builder()
            .capabilities(CapabilitySet::allow_all())
            .build()
            .unwrap();
        otter.run_file(&entry).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn typed_array_and_data_view_messages_keep_their_view_ranges() {
        let dir = tempfile::tempdir().unwrap();
        let worker_path = dir.path().join("worker.js");
        fs::write(
            &worker_path,
            r#"
            globalThis.onmessage = (event) => {
              const { words, view } = event.data;
              postMessage([
                words.constructor.name, words.byteOffset, words.length, words[0], words[2],
                view.constructor.name, view.byteOffset, view.byteLength, view.getUint8(0),
                words.buffer.byteLength,
              ]);
            };
            "#,
        )
        .unwrap();
        let entry = dir.path().join("entry.js");
        fs::write(
            &entry,
            format!(
                r#"
                const buffer = new ArrayBuffer(10);
                const bytes = new Uint8Array(buffer);
                for (let i = 0; i < bytes.length; i++) bytes[i] = i + 1;
                const words = new Uint16Array(buffer, 2, 3);
                const view = new DataView(buffer, 1, 4);
                let got = null;
                const w = new Worker({:?});
                w.onerror = (event) => {{
                  got = "ERR:" + event.message;
                  w.terminate();
                }};
                w.onmessage = (event) => {{
                  got = event.data.join(",");
                  w.terminate();
                }};
                w.postMessage({{ words, view }});
                setTimeout(() => {{
                  const expected = ["Uint16Array", 2, 3, words[0], words[2], "DataView", 1, 4, 2, 10].join(",");
                  if (got !== expected) throw "bad view clone: " + got + " expected " + expected;
                }}, 20);
                "#,
                worker_path.to_string_lossy()
            ),
        )
        .unwrap();

        let otter = Otter::builder()
            .capabilities(CapabilitySet::allow_all())
            .build()
            .unwrap();
        otter.run_file(&entry).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn worker_entry_accepts_a_file_url() {
        let dir = tempfile::tempdir().unwrap();
        let worker_path = dir.path().join("worker.js");
        fs::write(&worker_path, "postMessage('ready');").unwrap();
        let worker_url = url::Url::from_file_path(&worker_path).unwrap();
        let entry = dir.path().join("entry.js");
        fs::write(
            &entry,
            format!(
                r#"
                let got = "pending";
                const w = new Worker({:?});
                w.onerror = (event) => {{
                  got = "ERR:" + event.message;
                  w.terminate();
                }};
                w.onmessage = (event) => {{
                  got = event.data;
                  w.terminate();
                }};
                setTimeout(() => {{
                  if (got !== "ready") throw "bad worker message: " + got;
                }}, 20);
                "#,
                worker_url.as_str()
            ),
        )
        .unwrap();

        let otter = Otter::builder()
            .capabilities(CapabilitySet::allow_all())
            .build()
            .unwrap();
        otter.run_file(&entry).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn failed_worker_transfer_keeps_the_sender_buffer_attached() {
        let dir = tempfile::tempdir().unwrap();
        let worker_path = dir.path().join("worker.js");
        fs::write(&worker_path, "close();").unwrap();
        let entry = dir.path().join("entry.js");
        fs::write(
            &entry,
            format!(
                r#"
                const worker = new Worker({:?});
                let attempts = 0;
                const waitForClosedChannel = () => {{
                  try {{
                    worker.postMessage("probe");
                  }} catch (_) {{
                    const buffer = new ArrayBuffer(4);
                    new Uint8Array(buffer)[0] = 17;
                    let transferThrew = false;
                    try {{
                      worker.postMessage(buffer, [buffer]);
                    }} catch (_) {{
                      transferThrew = true;
                    }}
                    if (!transferThrew) throw "closed worker accepted transfer";
                    if (buffer.byteLength !== 4 || new Uint8Array(buffer)[0] !== 17) {{
                      throw "failed transfer detached or changed sender buffer";
                    }}
                    worker.terminate();
                    return;
                  }}
                  if (++attempts > 1000) throw "worker channel did not close";
                  setTimeout(waitForClosedChannel, 0);
                }};
                setTimeout(waitForClosedChannel, 0);
                "#,
                worker_path.to_string_lossy()
            ),
        )
        .unwrap();

        let otter = Otter::builder()
            .capabilities(CapabilitySet::allow_all())
            .build()
            .unwrap();
        otter.run_file(&entry).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn worker_timer_ownership_ignores_mutable_timer_globals() {
        let dir = tempfile::tempdir().unwrap();
        let worker_path = dir.path().join("worker.js");
        fs::write(&worker_path, "globalThis.onmessage = () => {};").unwrap();
        let entry = dir.path().join("entry.js");
        fs::write(
            &entry,
            format!(
                r#"
                const originalSetTimeout = setTimeout;
                const originalSetInterval = setInterval;
                const originalClearInterval = clearInterval;
                let victimFired = false;
                const victim = originalSetTimeout(() => {{ victimFired = true; }}, 0);

                try {{
                  globalThis.setInterval = () => victim;
                  globalThis.clearInterval = () => {{ throw "must not re-enter clearInterval"; }};

                  const worker = new Worker({:?});
                  if (Object.getOwnPropertyNames(worker).some((key) => key.startsWith("__otter"))) {{
                    throw "Worker exposed internal routing state";
                  }}
                  worker.terminate();
                }} finally {{
                  globalThis.setInterval = originalSetInterval;
                  globalThis.clearInterval = originalClearInterval;
                }}

                let attempts = 0;
                const waitForVictim = () => {{
                  if (victimFired) return;
                  if (++attempts > 128) throw "Worker cancelled a foreign timer";
                  originalSetTimeout(waitForVictim, 0);
                }};
                originalSetTimeout(waitForVictim, 0);
                "#,
                worker_path.to_string_lossy()
            ),
        )
        .unwrap();

        let otter = Otter::builder()
            .capabilities(CapabilitySet::allow_all())
            .build()
            .unwrap();
        otter.run_file(&entry).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn global_worker_post_message_rejects_unsupported_value() {
        let dir = tempfile::tempdir().unwrap();
        let worker_path = dir.path().join("worker.js");
        fs::write(&worker_path, "globalThis.onmessage = () => {};").unwrap();
        let entry = dir.path().join("entry.js");
        fs::write(
            &entry,
            format!(
                r#"
                const w = new Worker({:?});
                let threw = false;
                try {{
                  w.postMessage(() => 1);
                }} catch (err) {{
                  threw = String(err).includes("structuredClone");
                }}
                w.terminate();
                if (!threw) throw "unsupported value did not throw";
                "#,
                worker_path.to_string_lossy()
            ),
        )
        .unwrap();

        let otter = Otter::builder()
            .capabilities(CapabilitySet::allow_all())
            .build()
            .unwrap();
        otter.run_file(&entry).await.unwrap();
    }
    fn family_otter(family: Arc<WorkerFamily>) -> Otter {
        let mut builder = Otter::builder().capabilities(CapabilitySet::allow_all());
        builder.runtime = builder.runtime.worker_family(family);
        builder.build().unwrap()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn child_worker_runs_timers_microtasks_and_dynamic_import() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("dep.js"), "export const tag = 'dep';").unwrap();
        let worker_path = dir.path().join("worker.js");
        fs::write(
            &worker_path,
            r#"
            const order = [];
            queueMicrotask(() => order.push("microtask"));
            setTimeout(async () => {
              order.push("timer");
              const dep = await import("./dep.js");
              order.push(dep.tag);
              postMessage(order.join(","));
            }, 0);
            "#,
        )
        .unwrap();
        let entry = dir.path().join("entry.js");
        fs::write(
            &entry,
            format!(
                r#"
                const w = new Worker({:?});
                w.onerror = (event) => {{ throw "worker error: " + event.message; }};
                w.onmessage = (event) => {{
                  if (event.data !== "microtask,timer,dep") throw "bad order: " + event.data;
                  w.terminate();
                }};
                "#,
                worker_path.to_string_lossy()
            ),
        )
        .unwrap();

        let otter = Otter::builder()
            .capabilities(CapabilitySet::allow_all())
            .build()
            .unwrap();
        otter.run_file(&entry).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unlimited_main_ledger_does_not_bypass_family_worker_limit() {
        let dir = tempfile::tempdir().unwrap();
        let worker_path = dir.path().join("worker.js");
        fs::write(&worker_path, "globalThis.onmessage = () => {};").unwrap();
        let entry = dir.path().join("entry.js");
        fs::write(
            &entry,
            format!(
                r#"
                const first = new Worker({0:?});
                let rejected = "";
                try {{
                  new Worker({0:?});
                }} catch (error) {{
                  rejected = String(error);
                }}
                first.terminate();
                if (!rejected.includes("worker family limit")) {{
                  throw "second worker was not rejected: " + rejected;
                }}
                "#,
                worker_path.to_string_lossy()
            ),
        )
        .unwrap();

        // The main ledger is the unlimited default; only the family is finite.
        let otter = family_otter(WorkerFamily::with_limits(
            1,
            WORKER_FAMILY_MAX_QUEUED_MESSAGES,
            WORKER_FAMILY_MAX_QUEUED_MESSAGE_BYTES,
        ));
        otter.run_file(&entry).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn nested_workers_share_the_family_limits() {
        let dir = tempfile::tempdir().unwrap();
        let inner_path = dir.path().join("inner.js");
        fs::write(&inner_path, "globalThis.onmessage = () => {};").unwrap();
        let worker_path = dir.path().join("worker.js");
        fs::write(
            &worker_path,
            format!(
                r#"
                let outcome = "spawned";
                try {{
                  new Worker({:?});
                }} catch (error) {{
                  outcome = String(error);
                }}
                postMessage(outcome);
                "#,
                inner_path.to_string_lossy()
            ),
        )
        .unwrap();
        let entry = dir.path().join("entry.js");
        fs::write(
            &entry,
            format!(
                r#"
                const w = new Worker({:?});
                w.onerror = (event) => {{ throw "worker error: " + event.message; }};
                w.onmessage = (event) => {{
                  if (!String(event.data).includes("worker family limit")) {{
                    throw "nested worker escaped the family limit: " + event.data;
                  }}
                  w.terminate();
                }};
                "#,
                worker_path.to_string_lossy()
            ),
        )
        .unwrap();

        let otter = family_otter(WorkerFamily::with_limits(
            1,
            WORKER_FAMILY_MAX_QUEUED_MESSAGES,
            WORKER_FAMILY_MAX_QUEUED_MESSAGE_BYTES,
        ));
        otter.run_file(&entry).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn family_rejected_message_does_not_detach_the_transfer() {
        let dir = tempfile::tempdir().unwrap();
        let worker_path = dir.path().join("worker.js");
        fs::write(&worker_path, "globalThis.onmessage = () => {};").unwrap();
        let entry = dir.path().join("entry.js");
        fs::write(
            &entry,
            format!(
                r#"
                const w = new Worker({:?});
                const buffer = new ArrayBuffer(64);
                let threw = "";
                try {{
                  w.postMessage(buffer, [buffer]);
                }} catch (error) {{
                  threw = String(error);
                }}
                w.terminate();
                if (!threw.includes("worker message queue limit")) {{
                  throw "message was not rejected: " + threw;
                }}
                if (buffer.byteLength !== 64) {{
                  throw "rejected transfer detached the sender buffer";
                }}
                "#,
                worker_path.to_string_lossy()
            ),
        )
        .unwrap();

        let otter = family_otter(WorkerFamily::with_limits(WORKER_FAMILY_MAX_WORKERS, 0, 0));
        otter.run_file(&entry).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn oversized_message_graph_is_a_typed_error_before_any_effect() {
        let dir = tempfile::tempdir().unwrap();
        let worker_path = dir.path().join("worker.js");
        fs::write(
            &worker_path,
            "globalThis.onmessage = (event) => postMessage(event.data);",
        )
        .unwrap();
        let entry = dir.path().join("entry.js");
        fs::write(
            &entry,
            format!(
                r#"
                const w = new Worker({:?});
                const giant = "x".repeat(9 * 1024 * 1024);
                let threw = "";
                try {{
                  w.postMessage(giant);
                }} catch (error) {{
                  threw = String(error);
                }}
                if (!threw.includes("exceeds")) {{
                  throw "giant graph was not rejected: " + threw;
                }}
                // The worker remains usable after the rejection.
                w.onmessage = (event) => {{
                  if (event.data !== "ok") throw "bad echo: " + event.data;
                  w.terminate();
                }};
                w.postMessage("ok");
                "#,
                worker_path.to_string_lossy()
            ),
        )
        .unwrap();

        let otter = Otter::builder()
            .capabilities(CapabilitySet::allow_all())
            .build()
            .unwrap();
        otter.run_file(&entry).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn terminate_error_close_race_yields_one_terminal_outcome() {
        let dir = tempfile::tempdir().unwrap();
        let worker_path = dir.path().join("worker.js");
        fs::write(&worker_path, "postMessage('ready'); close();").unwrap();
        let entry = dir.path().join("entry.js");
        fs::write(
            &entry,
            format!(
                r#"
                const w = new Worker({:?});
                globalThis.errors = 0;
                w.onerror = () => {{ globalThis.errors += 1; }};
                w.onmessage = () => {{
                  // Race the terminal Closed delivery against terminate().
                  w.terminate();
                  w.terminate();
                }};
                "#,
                worker_path.to_string_lossy()
            ),
        )
        .unwrap();

        let account = ResourceAccount::default();
        let otter = Otter::builder()
            .capabilities(CapabilitySet::allow_all())
            .resource_account(account.clone())
            .build()
            .unwrap();
        otter.run_file(&entry).await.unwrap();
        let errors = otter.eval("globalThis.errors").await.unwrap();
        assert_eq!(errors.completion_string(), "0");
        // Census returns to baseline: the parent handle is the only isolate
        // and every queued-message charge was released.
        let snapshot = account.snapshot();
        assert_eq!(
            snapshot
                .get(otter_resource::ResourceClass::Workers)
                .current(),
            0
        );
        assert_eq!(
            snapshot
                .get(otter_resource::ResourceClass::QueuedMessages)
                .current(),
            0
        );
        assert_eq!(
            snapshot
                .get(otter_resource::ResourceClass::QueuedMessageBytes)
                .current(),
            0
        );
        assert_eq!(
            snapshot
                .get(otter_resource::ResourceClass::Isolates)
                .current(),
            1
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn worker_lifecycle_returns_resource_census_to_baseline() {
        let dir = tempfile::tempdir().unwrap();
        let worker_path = dir.path().join("worker.js");
        fs::write(
            &worker_path,
            r#"
            globalThis.onmessage = (event) => {
              for (let i = 0; i < 32; i += 1) postMessage(event.data + i);
              close();
            };
            "#,
        )
        .unwrap();
        let entry = dir.path().join("entry.js");
        fs::write(
            &entry,
            format!(
                r#"
                const w = new Worker({:?});
                globalThis.received = 0;
                w.onmessage = () => {{ globalThis.received += 1; }};
                w.postMessage(100);
                "#,
                worker_path.to_string_lossy()
            ),
        )
        .unwrap();

        let account = ResourceAccount::default();
        let otter = Otter::builder()
            .capabilities(CapabilitySet::allow_all())
            .resource_account(account.clone())
            .build()
            .unwrap();
        otter.run_file(&entry).await.unwrap();
        let received = otter.eval("globalThis.received").await.unwrap();
        assert_eq!(received.completion_string(), "32");
        let snapshot = account.snapshot();
        for class in [
            otter_resource::ResourceClass::Workers,
            otter_resource::ResourceClass::QueuedMessages,
            otter_resource::ResourceClass::QueuedMessageBytes,
        ] {
            assert_eq!(
                snapshot.get(class).current(),
                0,
                "class {class:?} did not return to baseline"
            );
        }
        // The worker's 16 MiB stack charge is gone; what remains is the
        // parent handle's own runner stack.
        assert_eq!(
            snapshot
                .get(otter_resource::ResourceClass::WorkerStackBytes)
                .current(),
            crate::admission::RUNTIME_THREAD_STACK_BYTES as u64
        );
        assert_eq!(
            snapshot
                .get(otter_resource::ResourceClass::Isolates)
                .current(),
            1
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn worker_terminal_survives_a_busy_parent_inbox() {
        let dir = tempfile::tempdir().unwrap();
        let worker_path = dir.path().join("worker.js");
        // A burst larger than the parent's bounded inbox: excess ordinary
        // messages may be rejected with a typed error, but the terminal
        // Closed always lands through its pre-reserved guaranteed credit.
        fs::write(
            &worker_path,
            r#"
            for (let i = 0; i < 256; i += 1) {
              try { postMessage(i); } catch (error) { /* backpressure */ }
            }
            close();
            "#,
        )
        .unwrap();
        let entry = dir.path().join("entry.js");
        fs::write(
            &entry,
            format!(
                r#"
                const w = new Worker({:?});
                globalThis.received = 0;
                w.onmessage = () => {{ globalThis.received += 1; }};
                "#,
                worker_path.to_string_lossy()
            ),
        )
        .unwrap();

        let account = ResourceAccount::default();
        let otter = Otter::builder()
            .capabilities(CapabilitySet::allow_all())
            .resource_account(account.clone())
            .build()
            .unwrap();
        // run_file returning proves the worker's keep-alive was released by
        // the terminal task; a lost terminal would hang the parent drain.
        otter.run_file(&entry).await.unwrap();
        let snapshot = account.snapshot();
        assert_eq!(
            snapshot
                .get(otter_resource::ResourceClass::Workers)
                .current(),
            0
        );
        assert_eq!(
            snapshot
                .get(otter_resource::ResourceClass::QueuedMessages)
                .current(),
            0
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn idle_worker_holds_no_timer_wakeups() {
        let dir = tempfile::tempdir().unwrap();
        let worker_path = dir.path().join("worker.js");
        fs::write(&worker_path, "globalThis.onmessage = () => {};").unwrap();
        let entry = dir.path().join("entry.js");
        fs::write(
            &entry,
            format!(
                r#"
                const w = new Worker({:?});
                setTimeout(() => {{ w.terminate(); }}, 30);
                "#,
                worker_path.to_string_lossy()
            ),
        )
        .unwrap();

        let otter = Otter::builder()
            .capabilities(CapabilitySet::allow_all())
            .build()
            .unwrap();
        otter.run_file(&entry).await.unwrap();
        // The only timer in this program is the parent's one-shot terminate
        // timer. An idle worker adds no repeating wakeup: after the run the
        // parent's timer census is empty.
        let stats = otter.handle().activity_stats();
        assert_eq!(stats.pending_ref_timers, 0);
        assert_eq!(stats.pending_unref_timers, 0);
    }

    /// Static source gate: the managed worker path must not regress to
    /// thread-channel polling. The needles are split so this test does not
    /// match itself.
    #[test]
    fn worker_source_gate_forbids_polling_primitives() {
        let source = include_str!("worker.rs");
        for needle in [
            concat!("std::sync::", "mpsc"),
            concat!("recv_", "timeout"),
            concat!("WORKER_COMMAND_", "POLL_INTERVAL"),
            concat!("schedule_", "interval"),
            concat!("std::thread::", "spawn"),
        ] {
            assert!(
                !source.contains(needle),
                "worker.rs regressed to polling primitive {needle:?}"
            );
        }
    }
}
