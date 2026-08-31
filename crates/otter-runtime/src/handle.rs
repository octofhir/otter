//! Sendable runtime command handle and isolate runner.
//!
//! [`RuntimeHandle`] is the Layer-B async boundary described in the mdBook
//! event-loop docs. It is
//! cloneable and `Send + Sync`; every command crosses an owned message
//! channel into a dedicated isolate runner thread that constructs and
//! owns the local [`crate::Runtime`].
//!
//! # Contents
//!
//! - [`RuntimeHandle`] — public command API.
//! - [`RuntimeActivityStats`] — cheap aggregate counters.
//! - isolate-runner message loop.
//!
//! # Invariants
//!
//! - VM and GC values never leave the isolate runner.
//! - Module graph preparation produces owned bytecode and metadata on Tokio's
//!   blocking pool; only realm-local instantiation/evaluation runs on the
//!   isolate thread.
//! - Script and module commands carry opaque realm ids; async settlement and
//!   disposal are routed on the owning isolate.
//! - Command replies carry only owned public data.
//! - Dropping a waiting future does not drop the isolate mid-turn; the
//!   runner observes the cancelled reply channel at the completion point.
//! - Public commands never execute recursively. Commands received while the
//!   current turn drains Ref'd work are deferred in FIFO order, and the shared
//!   queue bound accounts for both channel-resident and deferred commands.
//! - Every accepted public command owns one `QueuedTasks` resource lease while
//!   it remains in either queue; execution begins only after releasing it.
//!
//! # See also
//!
//! - [Event loop](../../../docs/book/src/engine/event-loop.md)
//! - [Runtime architecture](../../../docs/book/src/engine/architecture.md)

use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{SyncSender, sync_channel};
use std::sync::{Mutex, RwLock};
use std::time::Duration;

use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{mpsc, oneshot};

use crate::admission::{AdmittedRuntimeConfig, RUNTIME_THREAD_STACK_BYTES, RuntimeAdmissionKind};
use crate::completion_admission::{
    ActiveCompletion, CompletionAdmission, CompletionAdmissionPool, CompletionOrigin,
};
use crate::event_loop::{
    EventLoop, RuntimeLiveness, TimerRequest, TimerToken, TimerWake, TokioEventLoop,
};
use crate::runtime_activity::{
    RuntimeActivityAccounting, RuntimeKeepAlive, RuntimeTask, RuntimeTaskQueue, RuntimeTaskSpawner,
};
use crate::{
    DiagnosticCode, DynamicImportBegin, ExecutionAttempt, ExecutionResult, OtterError,
    ResourceAccount, ResourceClass, ResourceLease, ResourceSnapshot, Runtime, RuntimeConfig,
    RuntimeModuleLoaderState, RuntimePackageManagerHandle, SourceInput, TimerFireOutcome,
};
use otter_vm::{
    DynamicImportAdmission, DynamicImportLoader, RuntimeBudget, RuntimeBudgetTelemetry,
    TimerAdmission, TimerScheduler,
};

const DEFAULT_COMMAND_CAPACITY: usize = 64;

type RunReply = oneshot::Sender<ExecutionAttempt>;
type CheckReply = oneshot::Sender<Result<(), OtterError>>;
type RealmReply = oneshot::Sender<Result<crate::RuntimeRealmId, OtterError>>;

type CommandId = u64;

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct ModuleJobId(u64);

/// Cheap activity counters exposed for tests and diagnostics.
#[derive(Debug, Clone, Default)]
pub struct RuntimeActivityStats {
    /// Commands currently queued from the handle side.
    pub queued_commands: usize,
    /// Commands accepted by the handle.
    pub submitted_commands: u64,
    /// Commands completed successfully.
    pub completed_commands: u64,
    /// Commands completed with an error.
    pub failed_commands: u64,
    /// Commands whose waiter timed out.
    pub timed_out_commands: u64,
    /// Commands whose waiter was dropped before the reply was sent.
    pub cancelled_waiters: u64,
    /// Commands rejected because the bounded queue was full.
    pub backpressure_rejections: u64,
    /// Interrupt requests sent to the isolate.
    pub interrupts: u64,
    /// Referenced host operations still pending.
    pub pending_ref_host_ops: usize,
    /// Unreferenced host operations still pending.
    pub pending_unref_host_ops: usize,
    /// Host operations completed successfully.
    pub completed_host_ops: u64,
    /// Host operations completed with an error.
    pub failed_host_ops: u64,
    /// Host operations cancelled before completion.
    pub cancelled_host_ops: u64,
    /// Referenced timers still pending.
    pub pending_ref_timers: usize,
    /// Unreferenced timers still pending.
    pub pending_unref_timers: usize,
    /// Timers that fired.
    pub fired_timers: u64,
    /// Timers cancelled before firing.
    pub cancelled_timers: u64,
    /// Dynamic module jobs still pending.
    pub pending_dynamic_module_jobs: usize,
    /// Dynamic module jobs completed.
    pub completed_dynamic_module_jobs: u64,
    /// Runtime diagnostics emitted.
    pub diagnostics: u64,
    /// Whether the isolate is currently running a command.
    pub running_command: bool,
    /// Whether VM microtasks were pending at the last runner safepoint.
    pub pending_microtasks: bool,
    /// VM microtask generation observed at the last runner safepoint.
    pub microtask_generation: u64,
    /// `true` after the handle has begun shutdown.
    pub shutdown: bool,
    /// Shared-ledger byte and resource counters captured with the activity
    /// counters: heap external, source/module, generated-code, and queued
    /// byte classes plus worker/task/timer populations, each with current,
    /// peak, rejection, and limit values.
    pub resources: otter_resource::ResourceSnapshot,
}

/// Cloneable, sendable runtime command API.
#[derive(Clone)]
pub struct RuntimeHandle {
    inner: Arc<RuntimeHandleInner>,
}

impl std::fmt::Debug for RuntimeHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuntimeHandle")
            .field("activity_stats", &self.activity_stats())
            .finish_non_exhaustive()
    }
}

struct RuntimeHandleInner {
    resources: ResourceAccount,
    ipc_resources: ResourceAccount,
    completion_pool: CompletionAdmissionPool,
    inbox: InboxSender,
    runner: Mutex<Option<std::thread::JoinHandle<()>>>,
    event_loop: TokioEventLoop,
    module_preparation: ModulePreparation,
    module_cancellation: crate::module_loader::ModuleLoadCancellation,
    interrupt: otter_vm::InterruptFlag,
    atomics_wait_agent: otter_vm::atomics_wait::WaitAgentHandle,
    command_timeout: Duration,
    command_capacity: usize,
    counters: Arc<RuntimeCounters>,
    /// Per-turn execution policy the isolate was built with, paired with the
    /// counters it publishes so one reading carries both.
    budget_limits: RuntimeBudget,
    budget_telemetry: RuntimeBudgetTelemetry,
    exit: Arc<IsolateExit>,
    /// Shared fire-order queue every timer wake is handed to. Only the
    /// host-scheduled test path reaches it from this side; the isolate runner
    /// owns the producing end.
    #[cfg(test)]
    timer_posts: InboxSender,
}

#[derive(Debug, Default)]
struct IsolateExit {
    stopped: AtomicBool,
    notify: tokio::sync::Notify,
}

struct IsolateExitGuard(Arc<IsolateExit>);

impl Drop for IsolateExitGuard {
    fn drop(&mut self) {
        self.0.stopped.store(true, Ordering::Release);
        self.0.notify.notify_waiters();
    }
}

#[derive(Clone)]
struct ModulePreparation {
    loader: RuntimeModuleLoaderState,
    hosted_modules: Vec<crate::HostedModule>,
    package_manager: RuntimePackageManagerHandle,
    capabilities: crate::CapabilitySet,
    hooks: crate::RuntimeHooks,
    capability_evaluator: crate::RuntimeCapabilityEvaluator,
    remote_provider: Arc<dyn crate::module_loader::RemoteModuleProvider>,
    remote_cache: Arc<RemoteModuleCache>,
    /// Ledger remote providers charge while streaming module bodies.
    resource_account: ResourceAccount,
}

#[derive(Debug, Default)]
struct RemoteModuleCache {
    sources: RwLock<HashMap<String, crate::module_loader::RemoteModuleSource>>,
}

impl crate::module_loader::RemoteModuleFetch for RemoteModuleCache {
    fn fetch(&self, url: &str) -> Result<crate::module_loader::RemoteModuleSource, String> {
        self.sources
            .read()
            .map_err(|_| "remote module cache poisoned".to_string())?
            .get(url)
            .cloned()
            .ok_or_else(|| format!("remote module `{url}` was not prefetched"))
    }
}

struct CancelModuleLoadOnDrop(Option<crate::module_loader::ModuleLoadCancellation>);

impl CancelModuleLoadOnDrop {
    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for CancelModuleLoadOnDrop {
    fn drop(&mut self) {
        if let Some(cancellation) = &self.0 {
            cancellation.cancel();
        }
    }
}

impl ModulePreparation {
    const MAX_REMOTE_REDIRECTS: usize = 10;

    fn new(config: &RuntimeConfig, event_loop: &TokioEventLoop) -> Self {
        let remote_provider = config
            .remote_module_provider
            .clone()
            .unwrap_or_else(|| event_loop.remote_module_provider());
        Self {
            loader: RuntimeModuleLoaderState::new(config.loader.clone()),
            hosted_modules: config.hosted_modules.clone(),
            package_manager: RuntimePackageManagerHandle::from_loader_config(
                config.loader.as_ref(),
            ),
            capabilities: config.capabilities.clone(),
            hooks: config.hooks.clone(),
            capability_evaluator: crate::RuntimeCapabilityEvaluator::from_refs(
                &config.capabilities,
                &config.hooks,
            ),
            remote_provider,
            remote_cache: Arc::new(RemoteModuleCache::default()),
            resource_account: config.resource_account.clone(),
        }
    }

    fn loader_for_entry(&self, entry: &std::path::Path) -> crate::module_loader::ModuleLoader {
        self.loader
            .for_entry(
                entry,
                &self.hosted_modules,
                &self.package_manager,
                &self.capabilities,
                &self.hooks,
                &self.resource_account,
            )
            .with_remote_fetch(self.remote_cache.clone())
    }

    async fn prepare_source(
        &self,
        source: SourceInput,
        url: String,
        cancellation: crate::module_loader::ModuleLoadCancellation,
    ) -> Result<crate::module_graph::LinkedProgram, crate::module_graph::GraphError> {
        let loader = self.loader_for_entry(std::path::Path::new("."));
        let text = crate::module_loader::admit_source(loader.resource_account(), &url, source.text)
            .map_err(crate::module_graph::GraphError::Loader)?;
        let entry = crate::module_loader::ResolvedSource {
            url,
            kind: source.kind,
            jsx: None,
            text,
        };
        self.prepare_entry(loader, entry, cancellation).await
    }

    async fn prepare_path(
        &self,
        entry: PathBuf,
        cancellation: crate::module_loader::ModuleLoadCancellation,
    ) -> Result<crate::module_graph::LinkedProgram, crate::module_graph::GraphError> {
        let loader = self.loader_for_entry(&entry);
        let load_loader = crate::module_loader::ModuleLoader::with_config(loader.config().clone())
            .with_remote_fetch(self.remote_cache.clone());
        let load_entry = entry.clone();
        let source = tokio::task::spawn_blocking(move || {
            load_loader
                .load(&load_entry.to_string_lossy(), None)
                .map_err(crate::module_graph::GraphError::Loader)
        })
        .await
        .map_err(module_prepare_join_error)??;
        self.prepare_entry(loader, source, cancellation).await
    }

    async fn prepare_remote_entry(
        &self,
        target_url: String,
        cancellation: crate::module_loader::ModuleLoadCancellation,
    ) -> Result<crate::module_graph::LinkedProgram, crate::module_graph::GraphError> {
        let fetched = self
            .fetch_remote(target_url.clone(), cancellation.clone())
            .await?;
        let entry = crate::module_loader::ResolvedSource {
            kind: otter_syntax::remote_source_kind(
                fetched.content_type.as_deref(),
                &fetched.final_url,
            ),
            url: fetched.final_url,
            jsx: None,
            text: fetched.source,
        };
        let loader = self.loader_for_entry(std::path::Path::new("."));
        self.prepare_entry(loader, entry, cancellation).await
    }

    async fn prepare_entry(
        &self,
        loader: crate::module_loader::ModuleLoader,
        entry: crate::module_loader::ResolvedSource,
        cancellation: crate::module_loader::ModuleLoadCancellation,
    ) -> Result<crate::module_graph::LinkedProgram, crate::module_graph::GraphError> {
        let mut cancel_guard = CancelModuleLoadOnDrop(Some(cancellation.clone()));
        self.prefetch_remote_graph(&loader, entry.clone(), &cancellation)
            .await?;
        if cancellation.is_cancelled() {
            return Err(crate::module_graph::GraphError::Interrupted);
        }
        let interrupt = otter_vm::InterruptFlag::new();
        let interrupt_on_cancel = interrupt.clone();
        let cancel_watch = cancellation.clone();
        let watcher = tokio::spawn(async move {
            cancel_watch.cancelled().await;
            interrupt_on_cancel.interrupt();
        });
        let task = tokio::task::spawn_blocking(move || {
            crate::module_graph::load_program_source_interruptible(&loader, entry, interrupt)
        });
        let result = task.await.map_err(module_prepare_join_error)?;
        watcher.abort();
        if result.is_ok() {
            cancel_guard.disarm();
        }
        result
    }

    async fn prefetch_remote_graph(
        &self,
        loader: &crate::module_loader::ModuleLoader,
        entry: crate::module_loader::ResolvedSource,
        cancellation: &crate::module_loader::ModuleLoadCancellation,
    ) -> Result<(), crate::module_graph::GraphError> {
        const MAX_CONCURRENT_FETCHES: usize = 8;
        let mut frontier = vec![entry];
        let mut scanned = HashSet::new();
        let mut requested = HashSet::new();
        while !frontier.is_empty() {
            if cancellation.is_cancelled() {
                return Err(crate::module_graph::GraphError::Interrupted);
            }
            let scan_loader =
                crate::module_loader::ModuleLoader::with_config(loader.config().clone())
                    .with_remote_fetch(self.remote_cache.clone());
            let scan_frontier = std::mem::take(&mut frontier);
            let discovered = tokio::task::spawn_blocking(move || {
                discover_remote_requests(&scan_loader, scan_frontier)
            })
            .await
            .map_err(module_prepare_join_error)??;
            let mut missing = Vec::new();
            for (url, text) in discovered {
                if !requested.insert(url.clone()) {
                    continue;
                }
                let cached = self.cached_remote_source(&url)?;
                if let Some(source) = cached {
                    if !text && scanned.insert(source.final_url.clone()) {
                        frontier.push(remote_resolved_source(source));
                    }
                } else {
                    missing.push((url, text));
                }
            }
            for batch in missing.chunks(MAX_CONCURRENT_FETCHES) {
                let mut tasks = tokio::task::JoinSet::new();
                for (url, text) in batch.iter().cloned() {
                    let preparation = self.clone();
                    let cancellation = cancellation.clone();
                    tasks.spawn(async move {
                        let source = preparation.fetch_remote(url, cancellation).await?;
                        Ok::<_, crate::module_graph::GraphError>((source, text))
                    });
                }
                while let Some(result) = tasks.join_next().await {
                    let (source, text) = result.map_err(module_prepare_join_error)??;
                    if !text && scanned.insert(source.final_url.clone()) {
                        frontier.push(remote_resolved_source(source));
                    }
                }
            }
        }
        Ok(())
    }

    fn cached_remote_source(
        &self,
        url: &str,
    ) -> Result<Option<crate::module_loader::RemoteModuleSource>, crate::module_graph::GraphError>
    {
        let source = self
            .remote_cache
            .sources
            .read()
            .map_err(|_| remote_cache_error(url))?
            .get(url)
            .cloned();
        let Some(source) = source else {
            return Ok(None);
        };
        if source.final_url != url {
            let initiator = url::Url::parse(url).map_err(|error| {
                crate::module_graph::GraphError::Loader(crate::module_loader::LoaderError::Load {
                    url: url.to_string(),
                    message: format!("invalid cached remote module URL: {error}"),
                })
            })?;
            let final_url = url::Url::parse(&source.final_url).map_err(|error| {
                crate::module_graph::GraphError::Loader(crate::module_loader::LoaderError::Load {
                    url: source.final_url.clone(),
                    message: format!("invalid cached remote module final URL: {error}"),
                })
            })?;
            if !self
                .capability_evaluator
                .check_network(&final_url, Some(&initiator))
            {
                return Err(remote_capability_error(final_url.as_str()));
            }
        }
        Ok(Some(source))
    }

    async fn fetch_remote(
        &self,
        url: String,
        cancellation: crate::module_loader::ModuleLoadCancellation,
    ) -> Result<crate::module_loader::RemoteModuleSource, crate::module_graph::GraphError> {
        if cancellation.is_cancelled() {
            return Err(crate::module_graph::GraphError::Interrupted);
        }
        let requested_url = url;
        let mut current_url = requested_url.clone();
        let mut aliases = Vec::new();
        let mut followed = 0usize;
        loop {
            if cancellation.is_cancelled() {
                return Err(crate::module_graph::GraphError::Interrupted);
            }
            let cached = self.cached_remote_source(&current_url)?;
            if let Some(source) = cached {
                if !aliases.is_empty() {
                    let mut cache = self
                        .remote_cache
                        .sources
                        .write()
                        .map_err(|_| remote_cache_error(&current_url))?;
                    for alias in aliases {
                        cache.insert(alias, source.clone());
                    }
                }
                return Ok(source);
            }
            let response = self
                .remote_provider
                .fetch(crate::module_loader::RemoteModuleRequest {
                    url: current_url.clone(),
                    account: self.resource_account.clone(),
                    cancellation: cancellation.clone(),
                })
                .await;
            if cancellation.is_cancelled() {
                return Err(crate::module_graph::GraphError::Interrupted);
            }
            let response = response.map_err(|error| match error {
                crate::module_loader::RemoteModuleError::Cancelled => {
                    crate::module_graph::GraphError::Interrupted
                }
                other => crate::module_graph::GraphError::Loader(
                    crate::module_loader::LoaderError::Load {
                        url: current_url.clone(),
                        message: other.to_string(),
                    },
                ),
            })?;
            match response {
                crate::module_loader::RemoteModuleResponse::Source {
                    source,
                    content_type,
                } => {
                    let source = crate::module_loader::RemoteModuleSource {
                        source,
                        content_type,
                        final_url: current_url.clone(),
                    };
                    let mut cache = self
                        .remote_cache
                        .sources
                        .write()
                        .map_err(|_| remote_cache_error(&current_url))?;
                    cache.insert(current_url, source.clone());
                    for alias in aliases {
                        cache.insert(alias, source.clone());
                    }
                    return Ok(source);
                }
                crate::module_loader::RemoteModuleResponse::Redirect { location } => {
                    if followed >= Self::MAX_REMOTE_REDIRECTS {
                        return Err(crate::module_graph::GraphError::Loader(
                            crate::module_loader::LoaderError::Load {
                                url: current_url,
                                message: format!(
                                    "remote module exceeded {} redirects",
                                    Self::MAX_REMOTE_REDIRECTS
                                ),
                            },
                        ));
                    }
                    let previous = url::Url::parse(&current_url).map_err(|error| {
                        crate::module_graph::GraphError::Loader(
                            crate::module_loader::LoaderError::Load {
                                url: current_url.clone(),
                                message: format!("invalid remote module URL: {error}"),
                            },
                        )
                    })?;
                    let next = previous.join(&location).map_err(|error| {
                        crate::module_graph::GraphError::Loader(
                            crate::module_loader::LoaderError::Load {
                                url: current_url.clone(),
                                message: format!(
                                    "invalid remote module redirect `{location}`: {error}"
                                ),
                            },
                        )
                    })?;
                    if !crate::module_loader::is_http_url(next.as_str()) {
                        return Err(crate::module_graph::GraphError::Loader(
                            crate::module_loader::LoaderError::Load {
                                url: next.to_string(),
                                message: "remote module redirect must use http or https"
                                    .to_string(),
                            },
                        ));
                    }
                    if !self
                        .capability_evaluator
                        .check_network(&next, Some(&previous))
                    {
                        return Err(remote_capability_error(next.as_str()));
                    }
                    aliases.push(current_url);
                    current_url = next.to_string();
                    followed += 1;
                }
            }
        }
    }
}

fn remote_capability_error(target: &str) -> crate::module_graph::GraphError {
    let resource = url::Url::parse(target)
        .map(|url| crate::module_loader::network_resource(&url))
        .unwrap_or_else(|_| target.to_string());
    crate::module_graph::GraphError::Loader(crate::module_loader::LoaderError::CapabilityDenied {
        specifier: target.to_string(),
        capability: "net".to_string(),
        resource,
    })
}

fn module_prepare_join_error(error: tokio::task::JoinError) -> crate::module_graph::GraphError {
    crate::module_graph::GraphError::Loader(crate::module_loader::LoaderError::Load {
        url: "<module-prepare>".to_string(),
        message: error.to_string(),
    })
}

fn remote_cache_error(url: &str) -> crate::module_graph::GraphError {
    crate::module_graph::GraphError::Loader(crate::module_loader::LoaderError::Load {
        url: url.to_string(),
        message: "remote module cache poisoned".to_string(),
    })
}

fn remote_resolved_source(
    source: crate::module_loader::RemoteModuleSource,
) -> crate::module_loader::ResolvedSource {
    crate::module_loader::ResolvedSource {
        kind: otter_syntax::remote_source_kind(source.content_type.as_deref(), &source.final_url),
        url: source.final_url,
        jsx: None,
        text: source.source,
    }
}

fn discover_remote_requests(
    loader: &crate::module_loader::ModuleLoader,
    sources: Vec<crate::module_loader::ResolvedSource>,
) -> Result<Vec<(String, bool)>, crate::module_graph::GraphError> {
    let mut queue = sources;
    let mut visited = HashSet::new();
    let mut remote = Vec::new();
    while let Some(source) = queue.pop() {
        if !visited.insert(source.url.clone()) {
            continue;
        }
        for request in crate::module_graph::scan_module_requests(source.text, source.kind)? {
            let target = loader.resolve(&request.specifier, Some(&source.url))?;
            if crate::module_loader::is_http_url(&target) {
                remote.push((target, request.data));
            } else if !request.data && !loader.is_hosted_url(&target) {
                queue.push(loader.load_resolved(target)?);
            }
        }
    }
    Ok(remote)
}

struct RuntimeCounters {
    queued_commands: AtomicUsize,
    submitted_commands: AtomicU64,
    completed_commands: AtomicU64,
    failed_commands: AtomicU64,
    timed_out_commands: AtomicU64,
    cancelled_waiters: AtomicU64,
    backpressure_rejections: AtomicU64,
    interrupts: AtomicU64,
    pending_ref_host_ops: AtomicUsize,
    pending_unref_host_ops: AtomicUsize,
    completed_host_ops: AtomicU64,
    failed_host_ops: AtomicU64,
    cancelled_host_ops: AtomicU64,
    pending_ref_timers: AtomicUsize,
    pending_unref_timers: AtomicUsize,
    /// Current liveness class per pending JS timer token. Fire, cancel, and
    /// ref/unref all consult this so the class moved by `set_ref` is the one
    /// decremented, not the class the timer was scheduled with.
    timers: Mutex<HashMap<u64, OwnedTimer>>,
    fired_timers: AtomicU64,
    cancelled_timers: AtomicU64,
    pending_dynamic_module_jobs: AtomicUsize,
    completed_dynamic_module_jobs: AtomicU64,
    diagnostics: AtomicU64,
    running_command: AtomicBool,
    pending_microtasks: AtomicBool,
    microtask_generation: AtomicU64,
    next_command_id: AtomicU64,
    #[cfg(test)]
    next_module_job_id: AtomicU64,
    shutdown: AtomicBool,
}

enum TimerResource {
    OneShot(CompletionAdmission),
    Repeat { _active: ActiveCompletion },
}

struct OwnedTimer {
    liveness: RuntimeLiveness,
    resource: TimerResource,
}

enum PendingTimerAdmission {
    OneShot(CompletionAdmission),
    Repeat(ActiveCompletion),
}

impl PendingTimerAdmission {
    fn belongs_to(&self, pool: &CompletionAdmissionPool) -> bool {
        match self {
            Self::OneShot(admission) => admission.belongs_to(pool),
            Self::Repeat(active) => active.belongs_to(pool),
        }
    }
}

#[derive(Clone, Copy)]
enum CompletionAccounting {
    Host(RuntimeLiveness),
    Timer(RuntimeLiveness),
}

/// Unique guaranteed-completion carrier. Its queue and origin credits stay
/// attached to the payload through the overflow FIFO, Tokio channel, async
/// dynamic-import preparation, dispatch, and shutdown cancellation.
struct QueuedCompletion {
    admission: Option<CompletionAdmission>,
    accounting: Option<CompletionAccounting>,
    counters: Arc<RuntimeCounters>,
}

impl QueuedCompletion {
    fn admit_host(
        pool: &CompletionAdmissionPool,
        counters: Arc<RuntimeCounters>,
        liveness: RuntimeLiveness,
    ) -> Result<Self, OtterError> {
        let admission = pool.admit(CompletionOrigin::HostOperation)?;
        counters.retain_host_activity(liveness);
        Ok(Self {
            admission: Some(admission),
            accounting: Some(CompletionAccounting::Host(liveness)),
            counters,
        })
    }

    fn from_one_shot_timer(
        admission: CompletionAdmission,
        counters: Arc<RuntimeCounters>,
        liveness: RuntimeLiveness,
    ) -> Self {
        Self {
            admission: Some(admission),
            accounting: Some(CompletionAccounting::Timer(liveness)),
            counters,
        }
    }

    fn begin_dispatch(mut self) -> ActiveTrackedCompletion {
        let active = self
            .admission
            .take()
            .expect("queued completion owns one admission")
            .begin_dispatch();
        ActiveTrackedCompletion {
            active: Some(active),
            accounting: self.accounting.take(),
            counters: self.counters.clone(),
        }
    }

    fn belongs_to(&self, pool: &CompletionAdmissionPool) -> bool {
        self.admission
            .as_ref()
            .is_some_and(|admission| admission.belongs_to(pool))
    }
}

impl Drop for QueuedCompletion {
    fn drop(&mut self) {
        if let Some(accounting) = self.accounting.take() {
            cancel_completion_accounting(&self.counters, accounting);
        }
    }
}

struct ActiveTrackedCompletion {
    active: Option<ActiveCompletion>,
    accounting: Option<CompletionAccounting>,
    counters: Arc<RuntimeCounters>,
}

impl ActiveTrackedCompletion {
    fn complete(mut self) {
        if let Some(accounting) = self.accounting.take() {
            complete_completion_accounting(&self.counters, accounting);
        }
        drop(self.active.take());
    }

    fn fail(mut self) {
        if let Some(accounting) = self.accounting.take() {
            fail_completion_accounting(&self.counters, accounting);
        }
        drop(self.active.take());
    }
}

impl Drop for ActiveTrackedCompletion {
    fn drop(&mut self) {
        if let Some(accounting) = self.accounting.take() {
            cancel_completion_accounting(&self.counters, accounting);
        }
    }
}

/// Cloneable producer side of the isolate's bounded inbox.
///
/// Tokio owns channel-capacity permits and wakes ordered senders when the
/// isolate receives an item. Engine-owned completions that originate from a
/// synchronous callback use one shared FIFO and at most one async drain task;
/// they never allocate a retry task per saturated message.
#[derive(Clone)]
struct InboxSender {
    shared: Arc<InboxShared>,
}

struct InboxShared {
    tx: mpsc::Sender<RuntimeMessage>,
    io_handle: tokio::runtime::Handle,
    counters: Arc<RuntimeCounters>,
    pending: Mutex<PendingPosts>,
    #[cfg(test)]
    drain_spawns: AtomicU64,
    #[cfg(test)]
    drain_runs: AtomicU64,
    #[cfg(test)]
    handoff_hook: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

#[derive(Default)]
struct PendingPosts {
    fifo: VecDeque<RuntimeMessage>,
    draining: bool,
}

/// Cancels a pump's shared FIFO if its executor task is aborted while awaiting
/// a channel permit. Posts stay in that FIFO until a permit is owned.
struct GuaranteedDrainDropGuard {
    inbox: InboxSender,
    armed: bool,
}

impl GuaranteedDrainDropGuard {
    fn new(inbox: InboxSender) -> Self {
        Self { inbox, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for GuaranteedDrainDropGuard {
    fn drop(&mut self) {
        if self.armed {
            self.inbox.cancel_pending();
        }
    }
}

impl InboxSender {
    fn new(
        tx: mpsc::Sender<RuntimeMessage>,
        io_handle: tokio::runtime::Handle,
        counters: Arc<RuntimeCounters>,
    ) -> Self {
        Self {
            shared: Arc::new(InboxShared {
                tx,
                io_handle,
                counters,
                pending: Mutex::new(PendingPosts::default()),
                #[cfg(test)]
                drain_spawns: AtomicU64::new(0),
                #[cfg(test)]
                drain_runs: AtomicU64::new(0),
                #[cfg(test)]
                handoff_hook: Mutex::new(None),
            }),
        }
    }

    fn try_send(&self, message: RuntimeMessage) -> Result<(), TrySendError<RuntimeMessage>> {
        self.shared.tx.try_send(message)
    }

    async fn send_ordered(&self, message: RuntimeMessage) -> Result<(), RuntimeMessage> {
        if self.shared.counters.shutdown.load(Ordering::Acquire) {
            return Err(message);
        }
        self.shared.tx.send(message).await.map_err(|error| error.0)
    }

    /// Queue a completion that must be delivered while the isolate is alive.
    ///
    /// The `pending` mutex serializes the direct-send/backlog boundary. Once a
    /// message encounters pressure, every later guaranteed post joins the same
    /// FIFO until its single drain catches up, so later posts cannot overtake
    /// the first overflow.
    fn post_guaranteed(&self, post: GuaranteedPost) {
        let message = RuntimeMessage::Guaranteed(post);
        if self.shared.counters.shutdown.load(Ordering::Acquire) {
            drop(message);
            return;
        }

        let mut pending = self
            .shared
            .pending
            .lock()
            .expect("runtime inbox pending queue poisoned");
        if self.shared.counters.shutdown.load(Ordering::Acquire) {
            drop(pending);
            drop(message);
            return;
        }

        let message = if pending.draining || !pending.fifo.is_empty() {
            message
        } else {
            match self.shared.tx.try_send(message) {
                Ok(()) => return,
                Err(TrySendError::Full(message)) => message,
                Err(TrySendError::Closed(_)) => {
                    drop(pending);
                    return;
                }
            }
        };

        pending.fifo.push_back(message);
        let start_drain = !pending.draining;
        if start_drain {
            pending.draining = true;
            #[cfg(test)]
            self.shared.drain_spawns.fetch_add(1, Ordering::Relaxed);
        }
        drop(pending);

        if start_drain {
            let inbox = self.clone();
            self.shared
                .io_handle
                .spawn(async move { inbox.drain_guaranteed().await });
        }
    }

    /// Post one repeat-timer tick without accumulating work under pressure.
    ///
    /// A pending guaranteed post counts as pressure too: a repeat tick may not
    /// bypass an earlier one-shot timer or completion waiting in the FIFO.
    fn post_coalescing(&self, message: RuntimeMessage) {
        if self.shared.counters.shutdown.load(Ordering::Acquire) {
            return;
        }
        let pending = self
            .shared
            .pending
            .lock()
            .expect("runtime inbox pending queue poisoned");
        if pending.draining || !pending.fifo.is_empty() {
            return;
        }
        let _ = self.shared.tx.try_send(message);
    }

    async fn drain_guaranteed(self) {
        #[cfg(test)]
        self.shared.drain_runs.fetch_add(1, Ordering::Release);
        let mut drain_guard = GuaranteedDrainDropGuard::new(self.clone());
        loop {
            // Keep the accounted post inside the shared FIFO while waiting.
            // Shutdown can then synchronously cancel every queued item; the
            // capacity wait itself owns no message or accounting state.
            let permit = match self.shared.tx.reserve().await {
                Ok(permit) => permit,
                Err(_) => {
                    self.cancel_pending();
                    drain_guard.disarm();
                    return;
                }
            };
            let mut pending = self
                .shared
                .pending
                .lock()
                .expect("runtime inbox pending queue poisoned");
            if self.shared.counters.shutdown.load(Ordering::Acquire) {
                let cancelled = std::mem::take(&mut pending.fifo);
                pending.draining = false;
                drain_guard.disarm();
                drop(pending);
                drop(cancelled);
                return;
            }
            let Some(message) = pending.fifo.pop_front() else {
                pending.draining = false;
                drain_guard.disarm();
                return;
            };
            #[cfg(test)]
            if let Some(hook) = self
                .shared
                .handoff_hook
                .lock()
                .expect("runtime inbox handoff hook")
                .clone()
            {
                hook();
            }
            permit.send(message);
            // Shutdown takes this mutex before draining the receiver. Keeping
            // it through the synchronous handoff makes the message visible to
            // that drain before shutdown can observe the FIFO as empty.
            drop(pending);
        }
    }

    fn cancel_pending(&self) {
        let cancelled = {
            let mut pending = self
                .shared
                .pending
                .lock()
                .expect("runtime inbox pending queue poisoned");
            pending.draining = false;
            std::mem::take(&mut pending.fifo)
        };
        drop(cancelled);
    }

    #[cfg(test)]
    fn drain_spawns(&self) -> u64 {
        self.shared.drain_spawns.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    fn drain_runs(&self) -> u64 {
        self.shared.drain_runs.load(Ordering::Acquire)
    }

    #[cfg(test)]
    fn set_handoff_hook(&self, hook: Arc<dyn Fn() + Send + Sync>) {
        *self
            .shared
            .handoff_hook
            .lock()
            .expect("runtime inbox handoff hook") = Some(hook);
    }
}

struct InboxRuntimeTaskQueue {
    inbox: InboxSender,
    counters: Arc<RuntimeCounters>,
    completion_pool: CompletionAdmissionPool,
}

impl InboxRuntimeTaskQueue {
    fn take_completion(
        &self,
        admission: otter_vm::host_completion::HostCompletionAdmission,
    ) -> Result<QueuedCompletion, OtterError> {
        let completion = admission
            .try_into_inner::<QueuedCompletion>()
            .map_err(|_| OtterError::Internal {
                code: DiagnosticCode::RuntimeClosed.as_str().to_string(),
                message: "foreign host-completion admission carrier".to_string(),
            })?;
        if !completion.belongs_to(&self.completion_pool) {
            return Err(OtterError::Internal {
                code: DiagnosticCode::RuntimeClosed.as_str().to_string(),
                message: "host-completion admission belongs to another isolate".to_string(),
            });
        }
        Ok(*completion)
    }
}

/// Cancellation-safe accounting for a task waiting on inbox capacity.
///
/// An ordered producer may be aborted at any `.await`. Until the task is
/// handed to the channel, this guard owns its liveness hold and releases that
/// hold as cancelled work when the producer future is dropped.
struct PendingHostActivity {
    counters: Arc<RuntimeCounters>,
    liveness: RuntimeLiveness,
    armed: bool,
}

impl PendingHostActivity {
    fn retain(counters: Arc<RuntimeCounters>, liveness: RuntimeLiveness) -> Self {
        counters.retain_host_activity(liveness);
        Self {
            counters,
            liveness,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for PendingHostActivity {
    fn drop(&mut self) {
        if self.armed {
            self.counters.cancel_host_activity(self.liveness);
        }
    }
}

impl RuntimeTaskQueue for InboxRuntimeTaskQueue {
    fn enqueue_boxed(
        &self,
        task: Box<dyn RuntimeTask>,
        liveness: RuntimeLiveness,
    ) -> Result<(), OtterError> {
        if self.counters.shutdown.load(Ordering::Acquire) {
            return Err(OtterError::Internal {
                code: DiagnosticCode::RuntimeShutdown.as_str().to_string(),
                message: "runtime isolate is shutting down".to_string(),
            });
        }
        self.counters.retain_host_activity(liveness);
        match self
            .inbox
            .try_send(RuntimeMessage::RuntimeTask { task, liveness })
        {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => {
                self.counters.cancel_host_activity(liveness);
                self.counters
                    .backpressure_rejections
                    .fetch_add(1, Ordering::Relaxed);
                Err(OtterError::Internal {
                    code: DiagnosticCode::RuntimeBackpressure.as_str().to_string(),
                    message: "runtime inbox is full".to_string(),
                })
            }
            Err(TrySendError::Closed(_)) => {
                self.counters.cancel_host_activity(liveness);
                Err(OtterError::Internal {
                    code: DiagnosticCode::RuntimeShutdown.as_str().to_string(),
                    message: "runtime isolate is no longer accepting tasks".to_string(),
                })
            }
        }
    }

    fn enqueue_boxed_ordered(
        &self,
        task: Box<dyn RuntimeTask>,
        liveness: RuntimeLiveness,
    ) -> std::pin::Pin<Box<dyn Future<Output = bool> + Send + 'static>> {
        let inbox = self.inbox.clone();
        let counters = self.counters.clone();
        Box::pin(async move {
            if counters.shutdown.load(Ordering::Acquire) {
                return false;
            }
            let mut activity = PendingHostActivity::retain(counters, liveness);
            let message = RuntimeMessage::RuntimeTask { task, liveness };
            match inbox.send_ordered(message).await {
                Ok(()) => {
                    activity.disarm();
                    true
                }
                Err(_) => false,
            }
        })
    }

    fn admit_boxed_guaranteed(
        &self,
        liveness: RuntimeLiveness,
    ) -> Result<otter_vm::host_completion::HostCompletionAdmission, OtterError> {
        if self.counters.shutdown.load(Ordering::Acquire) {
            return Err(OtterError::Internal {
                code: DiagnosticCode::RuntimeShutdown.as_str().to_string(),
                message: "runtime isolate is shutting down".to_string(),
            });
        }
        let completion =
            QueuedCompletion::admit_host(&self.completion_pool, self.counters.clone(), liveness)?;
        Ok(otter_vm::host_completion::HostCompletionAdmission::new(
            Box::new(completion),
        ))
    }

    fn enqueue_boxed_guaranteed(
        &self,
        admission: otter_vm::host_completion::HostCompletionAdmission,
        task: Box<dyn RuntimeTask>,
        outcome: otter_vm::host_completion::HostCompletionOutcome,
    ) -> Result<(), OtterError> {
        let completion = self.take_completion(admission)?;
        self.inbox.post_guaranteed(GuaranteedPost {
            payload: GuaranteedPayload::RuntimeTask { task, outcome },
            completion,
        });
        Ok(())
    }

    fn finish_boxed_guaranteed(
        &self,
        admission: otter_vm::host_completion::HostCompletionAdmission,
        outcome: otter_vm::host_completion::HostCompletionOutcome,
    ) -> Result<(), OtterError> {
        let active = self.take_completion(admission)?.begin_dispatch();
        match outcome {
            otter_vm::host_completion::HostCompletionOutcome::Completed => active.complete(),
            otter_vm::host_completion::HostCompletionOutcome::Failed => active.fail(),
            otter_vm::host_completion::HostCompletionOutcome::Cancelled => drop(active),
        }
        Ok(())
    }
}

impl RuntimeActivityAccounting for RuntimeCounters {
    fn retain_host_activity(&self, liveness: RuntimeLiveness) {
        increment_liveness(
            liveness,
            &self.pending_ref_host_ops,
            &self.pending_unref_host_ops,
        );
    }

    fn complete_host_activity(&self, liveness: RuntimeLiveness) {
        decrement_liveness(
            liveness,
            &self.pending_ref_host_ops,
            &self.pending_unref_host_ops,
        );
        self.completed_host_ops.fetch_add(1, Ordering::Relaxed);
    }

    fn cancel_host_activity(&self, liveness: RuntimeLiveness) {
        decrement_liveness(
            liveness,
            &self.pending_ref_host_ops,
            &self.pending_unref_host_ops,
        );
        self.cancelled_host_ops.fetch_add(1, Ordering::Relaxed);
    }

    fn move_host_activity(&self, from: RuntimeLiveness, to: RuntimeLiveness) {
        decrement_liveness(
            from,
            &self.pending_ref_host_ops,
            &self.pending_unref_host_ops,
        );
        increment_liveness(to, &self.pending_ref_host_ops, &self.pending_unref_host_ops);
    }
}

impl RuntimeCounters {
    fn new() -> Self {
        Self {
            queued_commands: AtomicUsize::new(0),
            submitted_commands: AtomicU64::new(0),
            completed_commands: AtomicU64::new(0),
            failed_commands: AtomicU64::new(0),
            timed_out_commands: AtomicU64::new(0),
            cancelled_waiters: AtomicU64::new(0),
            backpressure_rejections: AtomicU64::new(0),
            interrupts: AtomicU64::new(0),
            pending_ref_host_ops: AtomicUsize::new(0),
            pending_unref_host_ops: AtomicUsize::new(0),
            completed_host_ops: AtomicU64::new(0),
            failed_host_ops: AtomicU64::new(0),
            cancelled_host_ops: AtomicU64::new(0),
            pending_ref_timers: AtomicUsize::new(0),
            pending_unref_timers: AtomicUsize::new(0),
            timers: Mutex::new(HashMap::new()),
            fired_timers: AtomicU64::new(0),
            cancelled_timers: AtomicU64::new(0),
            pending_dynamic_module_jobs: AtomicUsize::new(0),
            completed_dynamic_module_jobs: AtomicU64::new(0),
            diagnostics: AtomicU64::new(0),
            running_command: AtomicBool::new(false),
            pending_microtasks: AtomicBool::new(false),
            microtask_generation: AtomicU64::new(0),
            next_command_id: AtomicU64::new(1),
            #[cfg(test)]
            next_module_job_id: AtomicU64::new(1),
            shutdown: AtomicBool::new(false),
        }
    }

    /// Current class of a timer owned by this isolate.
    fn timer_class(&self, token: u64) -> Option<RuntimeLiveness> {
        self.timers
            .lock()
            .expect("timer ownership lock")
            .get(&token)
            .map(|timer| timer.liveness)
    }

    /// Forget a completed or cancelled timer, returning its final ownership.
    fn timer_take(&self, token: u64) -> Option<OwnedTimer> {
        self.timers
            .lock()
            .expect("timer ownership lock")
            .remove(&token)
    }

    /// Atomically detach every timer owned by this isolate for teardown.
    fn timer_take_all(&self) -> HashMap<u64, OwnedTimer> {
        let mut timers = self.timers.lock().expect("timer ownership lock");
        std::mem::take(&mut *timers)
    }

    /// Move a pending timer between liveness classes. `false` when the
    /// token is unknown (already fired or cancelled).
    fn timer_set_ref(&self, token: u64, refed: bool) -> bool {
        let desired = if refed {
            RuntimeLiveness::Ref
        } else {
            RuntimeLiveness::Unref
        };
        let mut map = self.timers.lock().expect("timer ownership lock");
        let Some(timer) = map.get_mut(&token) else {
            return false;
        };
        if timer.liveness != desired {
            decrement_liveness(
                timer.liveness,
                &self.pending_ref_timers,
                &self.pending_unref_timers,
            );
            increment_liveness(
                desired,
                &self.pending_ref_timers,
                &self.pending_unref_timers,
            );
            timer.liveness = desired;
        }
        true
    }
}

enum RuntimeMessage {
    Command(QueuedCommand),
    Guaranteed(GuaranteedPost),
    RuntimeTask {
        task: Box<dyn RuntimeTask>,
        liveness: RuntimeLiveness,
    },
    TimerFired {
        token: TimerToken,
        expects_js_callback: bool,
    },
    #[cfg(test)]
    DynamicModuleReady(ModuleJobId),
    #[cfg(test)]
    Diagnostic(RuntimeDiagnostic),
    Interrupt,
    Shutdown,
}

struct GuaranteedPost {
    payload: GuaranteedPayload,
    completion: QueuedCompletion,
}

enum GuaranteedPayload {
    RuntimeTask {
        task: Box<dyn RuntimeTask>,
        outcome: otter_vm::host_completion::HostCompletionOutcome,
    },
    TimerFired {
        token: TimerToken,
        expects_js_callback: bool,
    },
    DynamicImportLoad {
        token: u64,
        specifier: String,
        referrer: String,
        attr_type: Option<String>,
    },
    DynamicImportGraphPrepared {
        token: u64,
        target_url: String,
        /// Boxed: a linked program embeds a whole bytecode module and its
        /// shared source snapshot, far larger than the sibling variants.
        result: Result<Box<crate::module_graph::LinkedProgram>, String>,
    },
}

/// One admitted public command waiting to enter the isolate.
///
/// The lease follows the command through both the bounded inbox and the
/// runner's deferred FIFO. It is released only when the command stops being
/// queued, immediately before execution begins, or when teardown drops the
/// envelope without executing it.
struct QueuedCommand {
    command: RuntimeCommand,
    lease: ResourceLease,
}

enum RuntimeCommand {
    CheckFile {
        id: CommandId,
        path: PathBuf,
        reply: CheckReply,
    },
    RunFile {
        id: CommandId,
        path: PathBuf,
        reply: RunReply,
    },
    RunScript {
        id: CommandId,
        source: SourceInput,
        specifier: String,
        reply: RunReply,
    },
    CreateRealm {
        id: CommandId,
        reply: RealmReply,
    },
    DisposeRealm {
        id: CommandId,
        realm: crate::RuntimeRealmId,
        reply: CheckReply,
    },
    RunScriptInRealm {
        id: CommandId,
        realm: crate::RuntimeRealmId,
        source: SourceInput,
        specifier: String,
        reply: RunReply,
    },
    RunModule {
        id: CommandId,
        linked: crate::module_graph::LinkedProgram,
        reply: RunReply,
    },
    RunModuleSource {
        id: CommandId,
        linked: crate::module_graph::LinkedProgram,
        reply: RunReply,
    },
    RunModuleInRealm {
        id: CommandId,
        realm: crate::RuntimeRealmId,
        linked: crate::module_graph::LinkedProgram,
        reply: RunReply,
    },
    Eval {
        id: CommandId,
        source: SourceInput,
        /// Directory whose CommonJS scope the snippet runs in, if any: the
        /// `-e`/`-p` snippet sees `require`, `module` and the builtin
        /// modules the way a Node one does.
        commonjs_scope: Option<std::path::PathBuf>,
        reply: RunReply,
    },
}

#[cfg(test)]
struct RuntimeDiagnostic {
    _origin: String,
    _message: String,
}

impl RuntimeHandle {
    /// Spawn an isolate runner with the default command capacity.
    ///
    /// # Errors
    /// Returns [`OtterError`] if the runtime config is invalid or the
    /// default Tokio runtime cannot be created.
    pub(crate) fn spawn(config: RuntimeConfig) -> Result<Self, OtterError> {
        Self::spawn_with_capacity(config, DEFAULT_COMMAND_CAPACITY)
    }

    /// Spawn a host worker whose role admission includes a worker slot.
    pub(crate) fn spawn_worker(config: RuntimeConfig) -> Result<Self, OtterError> {
        let admitted = Runtime::admit_config(config, RuntimeAdmissionKind::WorkerThread)?;
        Self::spawn_admitted_with_capacity(admitted, DEFAULT_COMMAND_CAPACITY)
    }

    pub(crate) fn spawn_admitted(admitted: AdmittedRuntimeConfig) -> Result<Self, OtterError> {
        Self::spawn_admitted_with_capacity(admitted, DEFAULT_COMMAND_CAPACITY)
    }

    /// Spawn an isolate runner with an explicit bounded queue size.
    ///
    /// # Errors
    /// Returns [`OtterError`] if the runtime config is invalid or the
    /// default Tokio runtime cannot be created.
    pub(crate) fn spawn_with_capacity(
        config: RuntimeConfig,
        capacity: usize,
    ) -> Result<Self, OtterError> {
        if capacity == 0 {
            return Err(OtterError::Internal {
                code: DiagnosticCode::RuntimeBackpressure.as_str().to_string(),
                message: "runtime inbox capacity must be at least one".to_string(),
            });
        }
        let admitted = Runtime::admit_config(config, RuntimeAdmissionKind::HandleThread)?;
        Self::spawn_admitted_with_capacity(admitted, capacity)
    }

    fn spawn_admitted_with_capacity(
        admitted: AdmittedRuntimeConfig,
        capacity: usize,
    ) -> Result<Self, OtterError> {
        if capacity == 0 {
            return Err(OtterError::Internal {
                code: DiagnosticCode::RuntimeBackpressure.as_str().to_string(),
                message: "runtime inbox capacity must be at least one".to_string(),
            });
        }
        let resources = admitted.config.resource_account.clone();
        let ipc_resources = crate::ipc::standard_ipc_resource_account();
        let completion_pool = CompletionAdmissionPool::new(
            resources.clone(),
            admitted.config.completion_capacities(),
        );
        let command_timeout = admitted.config.timeout();
        let budget_limits = admitted.config.runtime_budget();
        let event_loop = match admitted.config.runtime_host() {
            Some(host) => host.event_loop(),
            None => TokioEventLoop::current_or_owned().map_err(|e| OtterError::Internal {
                code: DiagnosticCode::TokioRuntimeCreate.as_str().to_string(),
                message: e.to_string(),
            })?,
        };
        let module_preparation = ModulePreparation::new(&admitted.config, &event_loop);
        let module_cancellation = crate::module_loader::ModuleLoadCancellation::new();
        let (interrupt_tx, interrupt_rx) = sync_channel(1);
        let counters = Arc::new(RuntimeCounters::new());
        let (tx, rx) = mpsc::channel(capacity);
        let inbox = InboxSender::new(tx, event_loop.handle(), counters.clone());
        #[cfg(test)]
        let handle_timer_posts = inbox.clone();
        let runner_counters = counters.clone();
        let runner_inbox = inbox.clone();
        let scheduler_event_loop = event_loop.clone();
        let runner_module_preparation = module_preparation.clone();
        let runner_module_cancellation = module_cancellation.clone();
        let runner_completion_pool = completion_pool.clone();
        let runner_ipc_resources = ipc_resources.clone();
        let exit = Arc::new(IsolateExit::default());
        let runner_exit = exit.clone();
        let runner = std::thread::Builder::new()
            .name("otter-isolate".to_string())
            .stack_size(RUNTIME_THREAD_STACK_BYTES)
            .spawn(move || {
                let _exit_guard = IsolateExitGuard(runner_exit);
                run_isolate(
                    admitted,
                    rx,
                    runner_counters,
                    interrupt_tx,
                    runner_inbox,
                    scheduler_event_loop,
                    runner_module_preparation,
                    runner_module_cancellation,
                    runner_completion_pool,
                    runner_ipc_resources,
                )
            })
            .map_err(|e| OtterError::Internal {
                code: DiagnosticCode::IsolateSpawn.as_str().to_string(),
                message: e.to_string(),
            })?;
        let (interrupt, atomics_wait_agent, budget_telemetry) = match interrupt_rx.recv() {
            Ok(handles) => handles,
            Err(_) => {
                // Bootstrap failed before publishing the interrupt handle.
                // Join the finished runner so a failed construction never
                // leaves a detached isolate thread behind.
                let _ = runner.join();
                return Err(OtterError::Internal {
                    code: DiagnosticCode::IsolateStart.as_str().to_string(),
                    message: "runtime isolate stopped before exposing its interrupt handle"
                        .to_string(),
                });
            }
        };
        let inner = Arc::new(RuntimeHandleInner {
            resources,
            ipc_resources,
            budget_limits,
            budget_telemetry,
            completion_pool,
            inbox,
            runner: Mutex::new(Some(runner)),
            event_loop,
            module_preparation,
            module_cancellation,
            interrupt,
            atomics_wait_agent,
            command_timeout,
            command_capacity: capacity,
            counters,
            exit,
            #[cfg(test)]
            timer_posts: handle_timer_posts,
        });
        Ok(Self { inner })
    }

    /// Run a file through the isolate runner.
    ///
    /// # Errors
    /// See [`OtterError`].
    pub async fn run_file(&self, path: impl Into<PathBuf>) -> Result<ExecutionResult, OtterError> {
        self.run_file_with_diagnostics(path).await.into_result()
    }

    /// Run a file and retain partial JIT diagnostics on abrupt failure.
    pub async fn run_file_with_diagnostics(&self, path: impl Into<PathBuf>) -> ExecutionAttempt {
        let (reply, rx) = oneshot::channel();
        let id = self.next_command_id();
        if let Err(error) = self.submit(RuntimeCommand::RunFile {
            id,
            path: path.into(),
            reply,
        }) {
            return ExecutionAttempt::from_result(Err(error), None, None);
        }
        self.await_run_reply(rx).await
    }

    /// Parse and compile a file through the isolate runner without executing
    /// user code.
    ///
    /// # Errors
    /// See [`OtterError`].
    pub async fn check_file(&self, path: impl Into<PathBuf>) -> Result<(), OtterError> {
        let (reply, rx) = oneshot::channel();
        let id = self.next_command_id();
        self.submit(RuntimeCommand::CheckFile {
            id,
            path: path.into(),
            reply,
        })?;
        self.await_check_reply(rx).await
    }

    /// Run a JavaScript or TypeScript source bundle through the
    /// isolate runner.
    ///
    /// # Errors
    /// See [`OtterError`].
    pub async fn run_script(
        &self,
        source: SourceInput,
        specifier: impl Into<String>,
    ) -> Result<ExecutionResult, OtterError> {
        self.run_script_with_diagnostics(source, specifier)
            .await
            .into_result()
    }

    /// Run a source bundle and retain partial JIT diagnostics on failure.
    pub async fn run_script_with_diagnostics(
        &self,
        source: SourceInput,
        specifier: impl Into<String>,
    ) -> ExecutionAttempt {
        let (reply, rx) = oneshot::channel();
        let id = self.next_command_id();
        if let Err(error) = self.submit(RuntimeCommand::RunScript {
            id,
            source,
            specifier: specifier.into(),
            reply,
        }) {
            return ExecutionAttempt::from_result(Err(error), None, None);
        }
        self.await_run_reply(rx).await
    }

    /// Run a module entry file through the isolate runner.
    ///
    /// # Errors
    /// See [`OtterError`].
    pub async fn run_module(
        &self,
        path: impl Into<PathBuf>,
    ) -> Result<ExecutionResult, OtterError> {
        self.run_module_with_diagnostics(path).await.into_result()
    }

    /// Run a module and retain partial JIT diagnostics on abrupt failure.
    pub async fn run_module_with_diagnostics(&self, path: impl Into<PathBuf>) -> ExecutionAttempt {
        let preparation = self.inner.module_preparation.clone();
        let path = path.into();
        let cancellation =
            crate::module_loader::ModuleLoadCancellation::linked(&self.inner.module_cancellation);
        let linked = match self
            .await_module_preparation(
                cancellation.clone(),
                preparation.prepare_path(path, cancellation),
            )
            .await
        {
            Ok(linked) => linked,
            Err(error) => return ExecutionAttempt::from_result(Err(error), None, None),
        };
        let (reply, rx) = oneshot::channel();
        let id = self.next_command_id();
        if let Err(error) = self.submit(RuntimeCommand::RunModule { id, linked, reply }) {
            return ExecutionAttempt::from_result(Err(error), None, None);
        }
        self.await_run_reply(rx).await
    }

    /// Run an in-memory ES module graph rooted at an absolute URL.
    ///
    /// # Errors
    /// See [`OtterError`].
    pub async fn run_module_source(
        &self,
        source: SourceInput,
        url: impl Into<String>,
    ) -> Result<ExecutionResult, OtterError> {
        self.run_module_source_with_diagnostics(source, url)
            .await
            .into_result()
    }

    /// Run an in-memory module and retain partial JIT diagnostics on failure.
    pub async fn run_module_source_with_diagnostics(
        &self,
        source: SourceInput,
        url: impl Into<String>,
    ) -> ExecutionAttempt {
        let preparation = self.inner.module_preparation.clone();
        let url = url.into();
        let cancellation =
            crate::module_loader::ModuleLoadCancellation::linked(&self.inner.module_cancellation);
        let linked = match self
            .await_module_preparation(
                cancellation.clone(),
                preparation.prepare_source(source, url, cancellation),
            )
            .await
        {
            Ok(linked) => linked,
            Err(error) => return ExecutionAttempt::from_result(Err(error), None, None),
        };
        let (reply, rx) = oneshot::channel();
        let id = self.next_command_id();
        if let Err(error) = self.submit(RuntimeCommand::RunModuleSource { id, linked, reply }) {
            return ExecutionAttempt::from_result(Err(error), None, None);
        }
        self.await_run_reply(rx).await
    }

    /// Run an in-memory ES module graph in an additional realm.
    pub async fn run_module_source_in_realm(
        &self,
        realm: crate::RuntimeRealmId,
        source: SourceInput,
        url: impl Into<String>,
    ) -> Result<ExecutionResult, OtterError> {
        self.run_module_source_in_realm_with_diagnostics(realm, source, url)
            .await
            .into_result()
    }

    /// Run a realm-targeted module and retain partial JIT diagnostics.
    pub async fn run_module_source_in_realm_with_diagnostics(
        &self,
        realm: crate::RuntimeRealmId,
        source: SourceInput,
        url: impl Into<String>,
    ) -> ExecutionAttempt {
        let preparation = self.inner.module_preparation.clone();
        let url = url.into();
        let cancellation =
            crate::module_loader::ModuleLoadCancellation::linked(&self.inner.module_cancellation);
        let linked = match self
            .await_module_preparation(
                cancellation.clone(),
                preparation.prepare_source(source, url, cancellation),
            )
            .await
        {
            Ok(linked) => linked,
            Err(error) => return ExecutionAttempt::from_result(Err(error), None, None),
        };
        let (reply, rx) = oneshot::channel();
        let id = self.next_command_id();
        if let Err(error) = self.submit(RuntimeCommand::RunModuleInRealm {
            id,
            realm,
            linked,
            reply,
        }) {
            return ExecutionAttempt::from_result(Err(error), None, None);
        }
        self.await_run_reply(rx).await
    }

    /// Evaluate a source bundle through the isolate runner.
    ///
    /// # Errors
    /// See [`OtterError`].
    pub async fn eval(&self, source: SourceInput) -> Result<ExecutionResult, OtterError> {
        self.eval_with_diagnostics(source).await.into_result()
    }

    /// Evaluate a source bundle and retain partial JIT diagnostics on failure.
    pub async fn eval_with_diagnostics(&self, source: SourceInput) -> ExecutionAttempt {
        self.eval_in_commonjs_scope(source, None).await
    }

    /// Evaluate a snippet with the CommonJS scope of `commonjs_scope`
    /// installed first, which is what makes `-e`/`-p` a Node snippet.
    pub async fn eval_in_commonjs_scope(
        &self,
        source: SourceInput,
        commonjs_scope: Option<std::path::PathBuf>,
    ) -> ExecutionAttempt {
        let (reply, rx) = oneshot::channel();
        let id = self.next_command_id();
        if let Err(error) = self.submit(RuntimeCommand::Eval {
            id,
            source,
            commonjs_scope,
            reply,
        }) {
            return ExecutionAttempt::from_result(Err(error), None, None);
        }
        self.await_run_reply(rx).await
    }

    /// Create and bootstrap an additional realm on this isolate.
    pub async fn create_realm(&self) -> Result<crate::RuntimeRealmId, OtterError> {
        let (reply, rx) = oneshot::channel();
        let id = self.next_command_id();
        self.submit(RuntimeCommand::CreateRealm { id, reply })?;
        self.await_realm_reply(rx).await
    }

    /// Dispose an additional realm on its owning isolate.
    pub async fn dispose_realm(&self, realm: crate::RuntimeRealmId) -> Result<(), OtterError> {
        let (reply, rx) = oneshot::channel();
        let id = self.next_command_id();
        self.submit(RuntimeCommand::DisposeRealm { id, realm, reply })?;
        self.await_check_reply(rx).await
    }

    /// Execute a classic script in an additional realm.
    pub async fn run_script_in_realm(
        &self,
        realm: crate::RuntimeRealmId,
        source: SourceInput,
        specifier: impl Into<String>,
    ) -> Result<ExecutionResult, OtterError> {
        let (reply, rx) = oneshot::channel();
        let id = self.next_command_id();
        self.submit(RuntimeCommand::RunScriptInRealm {
            id,
            realm,
            source,
            specifier: specifier.into(),
            reply,
        })?;
        self.await_run_reply(rx).await.into_result()
    }

    /// Request cooperative cancellation.
    pub fn interrupt(&self) {
        self.inner
            .counters
            .interrupts
            .fetch_add(1, Ordering::Relaxed);
        self.inner.interrupt.interrupt();
        let _ = self.inner.inbox.try_send(RuntimeMessage::Interrupt);
    }

    /// Begin shutting down this isolate and invalidate every clone of the
    /// handle.
    ///
    /// This is the page/worker lifecycle boundary for embedders that need
    /// deterministic teardown before all handle clones naturally drop. The
    /// cooperative interrupt wakes a running script; the shutdown message then
    /// makes later commands and runtime-task delivery fail instead of reaching
    /// a replacement or already-destroyed document.
    pub fn shutdown(&self) {
        self.inner.completion_pool.close();
        if self.inner.counters.shutdown.swap(true, Ordering::AcqRel) {
            return;
        }
        self.inner.interrupt.interrupt();
        self.inner.module_cancellation.cancel();
        // Never park the caller (often a browser UI thread) behind a full
        // bounded isolate inbox. A full queue already guarantees the runner is
        // awake; it observes the atomic shutdown flag before its next tick.
        let _ = self.inner.inbox.try_send(RuntimeMessage::Shutdown);
    }

    /// Shut the isolate down and asynchronously wait for complete teardown.
    ///
    /// This is the deterministic disposal path for latency-sensitive async
    /// hosts. It never joins the isolate thread on the caller and remains safe
    /// on a current-thread Tokio runtime. After it resolves, VM pages and
    /// traced host payloads have been released.
    pub async fn shutdown_and_wait(&self) {
        self.shutdown();
        loop {
            let notified = self.inner.exit.notify.notified();
            if self.inner.exit.stopped.load(Ordering::Acquire) {
                return;
            }
            notified.await;
        }
    }

    /// `true` after explicit shutdown or isolate teardown has begun.
    #[must_use]
    pub fn is_shutdown(&self) -> bool {
        self.inner.counters.shutdown.load(Ordering::Acquire)
    }

    /// Schedule a timer wake through the runtime inbox.
    #[must_use]
    #[cfg(test)]
    pub(crate) fn schedule_timer(&self, request: TimerRequest) -> TimerToken {
        let admission = self
            .inner
            .completion_pool
            .admit(CompletionOrigin::OneShotTimer)
            .expect("test timer fits the configured completion capacity");
        increment_liveness(
            RuntimeLiveness::Ref,
            &self.inner.counters.pending_ref_timers,
            &self.inner.counters.pending_unref_timers,
        );
        let wake = Arc::new(RuntimeTimerWake {
            inbox: self.inner.timer_posts.clone(),
            counters: self.inner.counters.clone(),
            repeat: request.repeat.is_some(),
            expects_js_callback: false,
        });
        schedule_owned_timer(
            &self.inner.event_loop,
            &self.inner.counters,
            request,
            wake,
            OwnedTimer {
                liveness: RuntimeLiveness::Ref,
                resource: TimerResource::OneShot(admission),
            },
        )
    }

    /// Retain one long-lived host resource in the runtime liveness counters.
    ///
    /// The returned guard must be closed when the host resource closes. Dropping
    /// the last guard also releases the hold, which covers error paths.
    #[must_use]
    pub fn retain_keep_alive(&self, liveness: RuntimeLiveness) -> RuntimeKeepAlive {
        let accounting: Arc<dyn RuntimeActivityAccounting> = self.inner.counters.clone();
        RuntimeKeepAlive::retain(accounting, liveness)
    }

    /// Enqueue an owned task to run on the isolate event-loop thread.
    ///
    /// The task is accounted as one host activity until the runner executes it.
    /// Feature crates should use this for cross-thread callbacks instead of
    /// calling into VM/JS from worker threads.
    ///
    /// # Errors
    /// Returns [`OtterError`] when the runtime inbox is full or shutting down.
    pub fn enqueue_runtime_task(
        &self,
        task: impl RuntimeTask,
        liveness: RuntimeLiveness,
    ) -> Result<(), OtterError> {
        self.task_spawner().enqueue(task, liveness)
    }

    /// Cross-thread canceller for this isolate's blocking `Atomics.wait`
    /// agent. `Worker.terminate` uses it to wake a worker parked in a
    /// blocking wait before the cooperative interrupt can land.
    #[must_use]
    pub(crate) fn atomics_wait_agent(&self) -> otter_vm::atomics_wait::WaitAgentHandle {
        self.inner.atomics_wait_agent.clone()
    }

    /// Join the isolate runner thread. Deterministic teardown for
    /// `Worker.terminate`: callers must have already requested shutdown and
    /// cancelled blocking waits, so the runner exits promptly.
    pub(crate) fn join_runner_blocking(&self) {
        let runner = self
            .inner
            .runner
            .lock()
            .expect("isolate runner mutex poisoned")
            .take();
        if let Some(runner) = runner {
            let _ = runner.join();
        }
    }

    /// Clone a sender for scheduling typed runtime tasks.
    #[must_use]
    pub fn task_spawner(&self) -> RuntimeTaskSpawner {
        RuntimeTaskSpawner::new(
            Arc::new(InboxRuntimeTaskQueue {
                inbox: self.inner.inbox.clone(),
                counters: self.inner.counters.clone(),
                completion_pool: self.inner.completion_pool.clone(),
            }),
            self.inner.counters.clone(),
            self.inner.resources.clone(),
            self.inner.ipc_resources.clone(),
            Some(self.inner.event_loop.handle()),
        )
    }

    /// Cancel a pending timer.
    #[cfg(test)]
    pub(crate) fn cancel_timer(&self, token: TimerToken) -> bool {
        let Some(timer) = self.inner.counters.timer_take(token.0) else {
            return false;
        };
        let _ = self.inner.event_loop.cancel_timer(token);
        decrement_liveness(
            timer.liveness,
            &self.inner.counters.pending_ref_timers,
            &self.inner.counters.pending_unref_timers,
        );
        self.inner
            .counters
            .cancelled_timers
            .fetch_add(1, Ordering::Relaxed);
        true
    }

    /// Register and complete a synthetic dynamic module job.
    ///
    /// This keeps the task-85 inbox shape exercised until module
    /// graph loading itself grows asynchronous host work.
    #[doc(hidden)]
    #[cfg(test)]
    pub(crate) fn complete_dynamic_module_job_for_tests(&self) {
        self.inner
            .counters
            .pending_dynamic_module_jobs
            .fetch_add(1, Ordering::Relaxed);
        if self
            .inner
            .inbox
            .try_send(RuntimeMessage::DynamicModuleReady(ModuleJobId(
                self.inner
                    .counters
                    .next_module_job_id
                    .fetch_add(1, Ordering::Relaxed),
            )))
            .is_err()
        {
            self.inner
                .counters
                .pending_dynamic_module_jobs
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| v.checked_sub(1))
                .ok();
        }
    }

    /// Emit a diagnostic wake through the event-loop abstraction.
    /// Wake the runtime and emit a diagnostic inbox item.
    #[cfg(test)]
    pub(crate) fn wake_runtime(&self, origin: impl Into<String>) {
        let origin = origin.into();
        let _ = self
            .inner
            .inbox
            .try_send(RuntimeMessage::Diagnostic(RuntimeDiagnostic {
                _origin: origin,
                _message: "runtime wake".to_string(),
            }));
    }

    fn next_command_id(&self) -> CommandId {
        self.inner
            .counters
            .next_command_id
            .fetch_add(1, Ordering::Relaxed)
    }

    async fn await_module_preparation<F>(
        &self,
        cancellation: crate::module_loader::ModuleLoadCancellation,
        task: F,
    ) -> Result<crate::module_graph::LinkedProgram, OtterError>
    where
        F: Future<
            Output = Result<crate::module_graph::LinkedProgram, crate::module_graph::GraphError>,
        >,
    {
        let timeout = self.inner.command_timeout;
        let outcome = if timeout == Duration::ZERO {
            task.await
        } else {
            tokio::pin!(task);
            tokio::select! {
                outcome = &mut task => outcome,
                () = tokio::time::sleep(timeout) => {
                    cancellation.cancel();
                    // Give an async provider one scheduler turn to observe its
                    // cancellation token before its future is dropped.
                    tokio::task::yield_now().await;
                    self.inner
                        .counters
                        .timed_out_commands
                        .fetch_add(1, Ordering::Relaxed);
                    self.inner
                        .counters
                        .failed_commands
                        .fetch_add(1, Ordering::Relaxed);
                    return Err(OtterError::timeout_after(timeout));
                }
            }
        };
        outcome.map_err(crate::map_graph_error)
    }

    /// Read the isolate's CPU policy, its published counters, and the shared
    /// resource ledger as one consistent picture.
    ///
    /// No command crosses the inbox: the counters are published by the isolate
    /// at its own turn boundaries, so a busy or blocked isolate still reports.
    #[must_use]
    pub fn budget_report(&self) -> crate::RuntimeBudgetReport {
        crate::RuntimeBudgetReport {
            limits: self.inner.budget_limits,
            execution: self.inner.budget_telemetry.snapshot(),
            resources: self.inner.resources.snapshot(),
        }
    }

    /// Snapshot cheap activity counters.
    #[must_use]
    pub fn activity_stats(&self) -> RuntimeActivityStats {
        RuntimeActivityStats {
            queued_commands: self.inner.counters.queued_commands.load(Ordering::Relaxed),
            submitted_commands: self
                .inner
                .counters
                .submitted_commands
                .load(Ordering::Relaxed),
            completed_commands: self
                .inner
                .counters
                .completed_commands
                .load(Ordering::Relaxed),
            failed_commands: self.inner.counters.failed_commands.load(Ordering::Relaxed),
            timed_out_commands: self
                .inner
                .counters
                .timed_out_commands
                .load(Ordering::Relaxed),
            cancelled_waiters: self
                .inner
                .counters
                .cancelled_waiters
                .load(Ordering::Relaxed),
            backpressure_rejections: self
                .inner
                .counters
                .backpressure_rejections
                .load(Ordering::Relaxed),
            interrupts: self.inner.counters.interrupts.load(Ordering::Relaxed),
            pending_ref_host_ops: self
                .inner
                .counters
                .pending_ref_host_ops
                .load(Ordering::Relaxed),
            pending_unref_host_ops: self
                .inner
                .counters
                .pending_unref_host_ops
                .load(Ordering::Relaxed),
            completed_host_ops: self
                .inner
                .counters
                .completed_host_ops
                .load(Ordering::Relaxed),
            failed_host_ops: self.inner.counters.failed_host_ops.load(Ordering::Relaxed),
            cancelled_host_ops: self
                .inner
                .counters
                .cancelled_host_ops
                .load(Ordering::Relaxed),
            pending_ref_timers: self
                .inner
                .counters
                .pending_ref_timers
                .load(Ordering::Relaxed),
            pending_unref_timers: self
                .inner
                .counters
                .pending_unref_timers
                .load(Ordering::Relaxed),
            fired_timers: self.inner.counters.fired_timers.load(Ordering::Relaxed),
            cancelled_timers: self.inner.counters.cancelled_timers.load(Ordering::Relaxed),
            pending_dynamic_module_jobs: self
                .inner
                .counters
                .pending_dynamic_module_jobs
                .load(Ordering::Relaxed),
            completed_dynamic_module_jobs: self
                .inner
                .counters
                .completed_dynamic_module_jobs
                .load(Ordering::Relaxed),
            diagnostics: self.inner.counters.diagnostics.load(Ordering::Relaxed),
            running_command: self.inner.counters.running_command.load(Ordering::Relaxed),
            pending_microtasks: self
                .inner
                .counters
                .pending_microtasks
                .load(Ordering::Relaxed),
            microtask_generation: self
                .inner
                .counters
                .microtask_generation
                .load(Ordering::Relaxed),
            shutdown: self.inner.counters.shutdown.load(Ordering::Relaxed),
            resources: self.inner.resources.snapshot(),
        }
    }

    /// Number of public handle clones still referencing this isolate.
    #[must_use]
    pub fn live_handle_count(&self) -> usize {
        Arc::strong_count(&self.inner)
    }

    /// Clone the shared resource account used by this isolate and its children.
    #[must_use]
    pub fn resource_account(&self) -> ResourceAccount {
        self.inner.resources.clone()
    }

    /// Capture deterministic current, peak, rejection, and limit counters.
    #[must_use]
    pub fn resource_snapshot(&self) -> ResourceSnapshot {
        self.inner.resources.snapshot()
    }

    pub(crate) fn block_on<F: std::future::Future>(&self, future: F) -> F::Output {
        self.inner.event_loop.block_on(future)
    }

    fn submit(&self, command: RuntimeCommand) -> Result<(), OtterError> {
        if self.inner.counters.shutdown.load(Ordering::Relaxed) {
            return Err(OtterError::Internal {
                code: DiagnosticCode::RuntimeShutdown.as_str().to_string(),
                message: "runtime handle is shut down".to_string(),
            });
        }
        let lease = self
            .inner
            .resources
            .reserve_exact(ResourceClass::QueuedTasks, 1)?;
        if self
            .inner
            .counters
            .queued_commands
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |queued| {
                (queued < self.inner.command_capacity).then_some(queued + 1)
            })
            .is_err()
        {
            self.inner
                .counters
                .backpressure_rejections
                .fetch_add(1, Ordering::Relaxed);
            return Err(OtterError::Internal {
                code: DiagnosticCode::RuntimeBackpressure.as_str().to_string(),
                message: "runtime command queue is full".to_string(),
            });
        }
        match self
            .inner
            .inbox
            .try_send(RuntimeMessage::Command(QueuedCommand { command, lease }))
        {
            Ok(()) => {
                self.inner
                    .counters
                    .submitted_commands
                    .fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
            Err(TrySendError::Full(_)) => {
                self.inner
                    .counters
                    .queued_commands
                    .fetch_sub(1, Ordering::Relaxed);
                self.inner
                    .counters
                    .backpressure_rejections
                    .fetch_add(1, Ordering::Relaxed);
                Err(OtterError::Internal {
                    code: DiagnosticCode::RuntimeBackpressure.as_str().to_string(),
                    message: "runtime command queue is full".to_string(),
                })
            }
            Err(TrySendError::Closed(_)) => {
                self.inner
                    .counters
                    .queued_commands
                    .fetch_sub(1, Ordering::Relaxed);
                Err(OtterError::Internal {
                    code: DiagnosticCode::RuntimeClosed.as_str().to_string(),
                    message: "runtime isolate has stopped".to_string(),
                })
            }
        }
    }

    async fn await_run_reply(&self, rx: oneshot::Receiver<ExecutionAttempt>) -> ExecutionAttempt {
        let timeout = self.inner.command_timeout;
        let outcome = if timeout == Duration::ZERO {
            rx.await
        } else {
            match tokio::time::timeout(timeout, rx).await {
                Ok(outcome) => outcome,
                Err(_) => {
                    self.inner
                        .counters
                        .timed_out_commands
                        .fetch_add(1, Ordering::Relaxed);
                    self.inner
                        .counters
                        .failed_commands
                        .fetch_add(1, Ordering::Relaxed);
                    self.interrupt();
                    return ExecutionAttempt::from_result(
                        Err(OtterError::timeout_after(timeout)),
                        None,
                        None,
                    );
                }
            }
        };
        match outcome {
            Ok(attempt) => {
                if attempt.result().is_ok() {
                    self.inner
                        .counters
                        .completed_commands
                        .fetch_add(1, Ordering::Relaxed);
                } else {
                    self.inner
                        .counters
                        .failed_commands
                        .fetch_add(1, Ordering::Relaxed);
                }
                attempt
            }
            Err(_) => ExecutionAttempt::from_result(
                Err(OtterError::Internal {
                    code: DiagnosticCode::RuntimeReplyDropped.as_str().to_string(),
                    message: "runtime isolate dropped command reply".to_string(),
                }),
                None,
                None,
            ),
        }
    }

    async fn await_check_reply(
        &self,
        rx: oneshot::Receiver<Result<(), OtterError>>,
    ) -> Result<(), OtterError> {
        let timeout = self.inner.command_timeout;
        let outcome = if timeout == Duration::ZERO {
            rx.await
        } else {
            match tokio::time::timeout(timeout, rx).await {
                Ok(outcome) => outcome,
                Err(_) => {
                    self.inner
                        .counters
                        .timed_out_commands
                        .fetch_add(1, Ordering::Relaxed);
                    self.inner
                        .counters
                        .failed_commands
                        .fetch_add(1, Ordering::Relaxed);
                    self.interrupt();
                    return Err(OtterError::timeout_after(timeout));
                }
            }
        };
        match outcome {
            Ok(Ok(())) => {
                self.inner
                    .counters
                    .completed_commands
                    .fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
            Ok(Err(err)) => {
                self.inner
                    .counters
                    .failed_commands
                    .fetch_add(1, Ordering::Relaxed);
                Err(err)
            }
            Err(_) => Err(OtterError::Internal {
                code: DiagnosticCode::RuntimeReplyDropped.as_str().to_string(),
                message: "runtime isolate dropped command reply".to_string(),
            }),
        }
    }

    async fn await_realm_reply(
        &self,
        rx: oneshot::Receiver<Result<crate::RuntimeRealmId, OtterError>>,
    ) -> Result<crate::RuntimeRealmId, OtterError> {
        let timeout = self.inner.command_timeout;
        let outcome = if timeout == Duration::ZERO {
            rx.await
        } else {
            match tokio::time::timeout(timeout, rx).await {
                Ok(outcome) => outcome,
                Err(_) => {
                    self.inner
                        .counters
                        .timed_out_commands
                        .fetch_add(1, Ordering::Relaxed);
                    self.inner
                        .counters
                        .failed_commands
                        .fetch_add(1, Ordering::Relaxed);
                    self.interrupt();
                    return Err(OtterError::timeout_after(timeout));
                }
            }
        };
        match outcome {
            Ok(Ok(realm)) => {
                self.inner
                    .counters
                    .completed_commands
                    .fetch_add(1, Ordering::Relaxed);
                Ok(realm)
            }
            Ok(Err(error)) => {
                self.inner
                    .counters
                    .failed_commands
                    .fetch_add(1, Ordering::Relaxed);
                Err(error)
            }
            Err(_) => Err(OtterError::Internal {
                code: DiagnosticCode::RuntimeReplyDropped.as_str().to_string(),
                message: "runtime isolate dropped command reply".to_string(),
            }),
        }
    }
}

impl Drop for RuntimeHandleInner {
    fn drop(&mut self) {
        self.completion_pool.close();
        self.counters.shutdown.store(true, Ordering::Release);
        self.interrupt.interrupt();
        self.module_cancellation.cancel();
        let _ = self.inbox.try_send(RuntimeMessage::Shutdown);
        // The final handle owns the isolate lifecycle. Joining here makes
        // runtime disposal a deterministic resource boundary: VM pages and
        // traced host payloads are gone when the last handle has dropped.
        // Explicit `shutdown()` remains the non-blocking signal for callers
        // that must initiate teardown from a latency-sensitive thread.
        if let Some(runner) = self.runner.lock().expect("runner mutex poisoned").take() {
            let _ = runner.join();
        }
    }
}

fn run_isolate(
    admitted: AdmittedRuntimeConfig,
    rx: mpsc::Receiver<RuntimeMessage>,
    counters: Arc<RuntimeCounters>,
    interrupt_tx: SyncSender<(
        otter_vm::InterruptFlag,
        otter_vm::atomics_wait::WaitAgentHandle,
        RuntimeBudgetTelemetry,
    )>,
    inbox: InboxSender,
    event_loop: TokioEventLoop,
    module_preparation: ModulePreparation,
    module_cancellation: crate::module_loader::ModuleLoadCancellation,
    completion_pool: CompletionAdmissionPool,
    ipc_resources: ResourceAccount,
) {
    let module_task_handle = event_loop.handle();
    let resources = admitted.config.resource_account.clone();
    let runtime_task_spawner = RuntimeTaskSpawner::new(
        Arc::new(InboxRuntimeTaskQueue {
            inbox: inbox.clone(),
            counters: counters.clone(),
            completion_pool: completion_pool.clone(),
        }),
        counters.clone(),
        resources,
        ipc_resources,
        Some(event_loop.handle()),
    );
    let mut runtime =
        match Runtime::from_config_with_task_spawner(admitted, Some(runtime_task_spawner)) {
            Ok(runtime) => runtime,
            Err(_) => return,
        };
    let timer_scheduler = Arc::new(InboxTimerScheduler {
        inbox: inbox.clone(),
        event_loop,
        counters: counters.clone(),
        completion_pool: completion_pool.clone(),
    });
    runtime.install_timer_scheduler(timer_scheduler);
    runtime.install_host_completion_sink(Arc::new(
        crate::runtime_activity::SpawnerCompletionSink {
            spawner: runtime
                .runtime_task_spawner()
                .expect("isolate runner constructs the runtime with a task spawner"),
        },
    ));
    let dynamic_import_loader = Arc::new(InboxDynamicImportLoader {
        inbox: inbox.clone(),
        counters: counters.clone(),
        completion_pool: completion_pool.clone(),
    });
    runtime.install_dynamic_import_loader(dynamic_import_loader);
    let _ = interrupt_tx.send((
        runtime.interrupt_handle().raw_flag(),
        runtime.atomics_wait_agent_handle(),
        runtime.budget_telemetry(),
    ));
    let mut runner = IsolateRunner {
        runtime,
        rx,
        inbox,
        counters,
        module_preparation,
        module_cancellation,
        module_task_handle,
        deferred_commands: VecDeque::new(),
        fatal_task_error: None,
        exit_finalized: false,
        shutdown: false,
        completion_pool: completion_pool.clone(),
    };
    runner.run_until_idle();
}

/// Timer scheduler installed on the [`crate::Runtime`] inside the
/// isolate runner thread. Each `setTimeout` / `setInterval` native
/// call lands here, schedules a Tokio sleep through the event
/// loop, and posts back a [`RuntimeMessage::TimerFired`] when the
/// delay elapses so the runner re-enters the VM and runs the JS
/// callback.
///
/// The struct is `Send + Sync` because the
/// [`otter_vm::TimerSchedulerHandle`] alias requires both. The
/// fields satisfy that: [`InboxSender`] and `TokioEventLoop` are
/// `Clone + Send + Sync`; `RuntimeCounters` is wrapped in `Arc`.
/// No VM state crosses this boundary — the schedule callback only
/// ships the host-issued [`TimerToken`] back to the runner, which
/// is then resolved against the per-isolate
/// [`otter_vm::TimerCallbacks`] table.
struct InboxTimerScheduler {
    inbox: InboxSender,
    event_loop: TokioEventLoop,
    counters: Arc<RuntimeCounters>,
    completion_pool: CompletionAdmissionPool,
}

struct RuntimeTimerWake {
    inbox: InboxSender,
    counters: Arc<RuntimeCounters>,
    repeat: bool,
    expects_js_callback: bool,
}

impl TimerWake for RuntimeTimerWake {
    fn timer_fired(&self, token: TimerToken) {
        if self.repeat {
            // A wake that lost a cancellation race no longer belongs to this
            // isolate and must not enter its inbox. Repeating ticks are
            // coalesced and retain only their finite live-timer origin credit.
            if self.counters.timer_class(token.0).is_some() {
                self.inbox.post_coalescing(RuntimeMessage::TimerFired {
                    token,
                    expects_js_callback: self.expects_js_callback,
                });
            }
        } else {
            // Move one-shot ownership out of the active timer table before
            // publishing the wake. Cancellation racing this take either owns
            // the complete resource or observes it already attached to the
            // guaranteed message; neither path can double-release it.
            let Some(timer) = self.counters.timer_take(token.0) else {
                return;
            };
            let liveness = timer.liveness;
            let admission = match timer.resource {
                TimerResource::OneShot(admission) => admission,
                TimerResource::Repeat { .. } => {
                    // A malformed embedder schedule must not leak liveness or
                    // panic on the timer-driver thread.
                    decrement_liveness(
                        liveness,
                        &self.counters.pending_ref_timers,
                        &self.counters.pending_unref_timers,
                    );
                    self.counters
                        .cancelled_timers
                        .fetch_add(1, Ordering::Relaxed);
                    return;
                }
            };
            self.inbox.post_guaranteed(GuaranteedPost {
                payload: GuaranteedPayload::TimerFired {
                    token,
                    expects_js_callback: self.expects_js_callback,
                },
                completion: QueuedCompletion::from_one_shot_timer(
                    admission,
                    self.counters.clone(),
                    liveness,
                ),
            });
        }
    }
}

/// Dynamic-import scheduler installed on [`crate::Runtime`] from
/// inside the isolate runner. The VM-thread opcode hands us a
/// host-issued token + the resolved specifier; we post a
/// [`GuaranteedPayload::DynamicImportLoad`] inbox message that, on
/// the next runner tick, resolves the request. Remote fetch plus graph
/// preparation runs off-isolate; the resulting owned graph returns through the
/// inbox for realm-local evaluation and promise settlement.
///
/// Both fields are `Send + Sync`: [`InboxSender`] clones, `Arc`
/// wraps the counters. The `String` payloads carry no VM state.
struct InboxDynamicImportLoader {
    inbox: InboxSender,
    counters: Arc<RuntimeCounters>,
    completion_pool: CompletionAdmissionPool,
}

/// Abort-safe owner for a dynamic-import preparation completion. Tokio drops
/// task futures on abort and while unwinding a panic; this guard converts that
/// drop into the same admitted Prepared message so the isolate can reject (or
/// cancel after process exit) the registry token instead of stranding it.
struct PendingDynamicImportPreparation {
    inbox: InboxSender,
    post: Option<(u64, String, QueuedCompletion)>,
    cancellation_error: String,
}

impl PendingDynamicImportPreparation {
    fn new(
        inbox: InboxSender,
        token: u64,
        target_url: String,
        completion: QueuedCompletion,
    ) -> Self {
        Self {
            inbox,
            post: Some((token, target_url, completion)),
            cancellation_error: "dynamic import preparation was cancelled".to_string(),
        }
    }

    fn finish(mut self, result: Result<crate::module_graph::LinkedProgram, String>) {
        self.send(result.map(Box::new));
    }

    fn send(&mut self, result: Result<Box<crate::module_graph::LinkedProgram>, String>) {
        let Some((token, target_url, completion)) = self.post.take() else {
            return;
        };
        self.inbox.post_guaranteed(GuaranteedPost {
            payload: GuaranteedPayload::DynamicImportGraphPrepared {
                token,
                target_url,
                result,
            },
            completion,
        });
    }
}

impl Drop for PendingDynamicImportPreparation {
    fn drop(&mut self) {
        if self.post.is_some() {
            let error = std::mem::take(&mut self.cancellation_error);
            self.send(Err(error));
        }
    }
}

impl DynamicImportLoader for InboxDynamicImportLoader {
    fn admit(&self) -> Result<DynamicImportAdmission, String> {
        let liveness = RuntimeLiveness::Ref;
        let completion =
            QueuedCompletion::admit_host(&self.completion_pool, self.counters.clone(), liveness)
                .map_err(|error| error.to_string())?;
        Ok(DynamicImportAdmission::new(Box::new(completion)))
    }

    fn schedule(
        &self,
        admission: DynamicImportAdmission,
        token: u64,
        specifier: String,
        referrer: String,
        attr_type: Option<String>,
    ) -> Result<(), String> {
        let completion = admission
            .try_into_inner::<QueuedCompletion>()
            .map_err(|_| "foreign dynamic-import admission carrier".to_string())?;
        if !completion.belongs_to(&self.completion_pool) {
            return Err("dynamic-import admission belongs to another isolate".to_string());
        }
        self.inbox.post_guaranteed(GuaranteedPost {
            payload: GuaranteedPayload::DynamicImportLoad {
                token,
                specifier,
                referrer,
                attr_type,
            },
            completion: *completion,
        });
        Ok(())
    }
}

/// Arm a driver timer and publish its isolate ownership as one linearized
/// operation. A zero-delay wake may run immediately on another Tokio worker,
/// so it must block on the ownership mutex until the token is registered.
fn schedule_owned_timer(
    event_loop: &TokioEventLoop,
    counters: &RuntimeCounters,
    request: TimerRequest,
    wake: Arc<dyn TimerWake>,
    timer: OwnedTimer,
) -> TimerToken {
    let mut timers = counters.timers.lock().expect("timer ownership lock");
    let token = event_loop.schedule_timer(request, wake);
    timers.insert(token.0, timer);
    token
}

impl TimerScheduler for InboxTimerScheduler {
    fn admit(&self, repeat: bool) -> Result<TimerAdmission, String> {
        let admission = if repeat {
            PendingTimerAdmission::Repeat(
                self.completion_pool
                    .admit_origin_only(ResourceClass::Timers)
                    .map_err(|error| error.to_string())?,
            )
        } else {
            PendingTimerAdmission::OneShot(
                self.completion_pool
                    .admit(CompletionOrigin::OneShotTimer)
                    .map_err(|error| error.to_string())?,
            )
        };
        Ok(TimerAdmission::new(Box::new(admission)))
    }

    fn schedule(
        &self,
        admission: TimerAdmission,
        delay_ms: u64,
        repeat_ms: Option<u64>,
    ) -> Result<u64, String> {
        let admission = admission
            .try_into_inner::<PendingTimerAdmission>()
            .map_err(|_| "foreign timer admission carrier".to_string())?;
        if !admission.belongs_to(&self.completion_pool) {
            return Err("timer admission belongs to another isolate".to_string());
        }
        let resource = match (*admission, repeat_ms) {
            (PendingTimerAdmission::OneShot(admission), None) => TimerResource::OneShot(admission),
            (PendingTimerAdmission::Repeat(active), Some(_)) => {
                TimerResource::Repeat { _active: active }
            }
            (PendingTimerAdmission::OneShot(_), Some(_)) => {
                return Err("one-shot timer admission cannot arm a repeating deadline".to_string());
            }
            (PendingTimerAdmission::Repeat(_), None) => {
                return Err("repeating timer admission cannot arm a one-shot deadline".to_string());
            }
        };
        let liveness = RuntimeLiveness::Ref;
        increment_liveness(
            liveness,
            &self.counters.pending_ref_timers,
            &self.counters.pending_unref_timers,
        );
        let request = TimerRequest {
            delay: Duration::from_millis(delay_ms),
            repeat: repeat_ms.map(Duration::from_millis),
        };
        let wake = Arc::new(RuntimeTimerWake {
            inbox: self.inbox.clone(),
            counters: self.counters.clone(),
            repeat: repeat_ms.is_some(),
            expects_js_callback: true,
        });
        let token = schedule_owned_timer(
            &self.event_loop,
            &self.counters,
            request,
            wake,
            OwnedTimer { liveness, resource },
        );
        Ok(token.0)
    }

    fn set_ref(&self, token: u64, refed: bool) -> bool {
        self.counters.timer_set_ref(token, refed)
    }

    fn cancel(&self, token: u64) -> bool {
        // The per-isolate ownership map is authoritative. Consulting the
        // shared driver first could cancel another isolate's timer when an
        // arbitrary numeric handle happens to name its token.
        let Some(timer) = self.counters.timer_take(token) else {
            return false;
        };
        let _ = self.event_loop.cancel_timer(TimerToken(token));
        // Taking ownership above turns an already-posted wake into a no-op,
        // so release the hold even if the driver had already fired it.
        decrement_liveness(
            timer.liveness,
            &self.counters.pending_ref_timers,
            &self.counters.pending_unref_timers,
        );
        self.counters
            .cancelled_timers
            .fetch_add(1, Ordering::Relaxed);
        true
    }
}

impl Drop for InboxTimerScheduler {
    fn drop(&mut self) {
        for (token, timer) in self.counters.timer_take_all() {
            let _ = self.event_loop.cancel_timer(TimerToken(token));
            decrement_liveness(
                timer.liveness,
                &self.counters.pending_ref_timers,
                &self.counters.pending_unref_timers,
            );
            self.counters
                .cancelled_timers
                .fetch_add(1, Ordering::Relaxed);
        }
    }
}

struct IsolateRunner {
    runtime: Runtime,
    rx: mpsc::Receiver<RuntimeMessage>,
    inbox: InboxSender,
    counters: Arc<RuntimeCounters>,
    module_preparation: ModulePreparation,
    module_cancellation: crate::module_loader::ModuleLoadCancellation,
    module_task_handle: tokio::runtime::Handle,
    deferred_commands: VecDeque<QueuedCommand>,
    /// First unhandled error thrown by a runtime task or timer callback in
    /// this turn. `process` and any active domain have already refused it by
    /// the time it lands here; the in-flight command's waiter receives it as
    /// the run's failure, exactly as Node fails the process on an uncaught
    /// exception. Without a waiter it is reported to stderr instead of being
    /// dropped.
    fatal_task_error: Option<OtterError>,
    /// Set once the `'exit'` event has been emitted for the current run.
    /// Node's process is gone after its exit listeners return, so callbacks a
    /// listener scheduled — or timers still in flight — must never run.
    /// Cleared when the next command starts a fresh run.
    exit_finalized: bool,
    shutdown: bool,
    completion_pool: CompletionAdmissionPool,
}

enum TickOutcome {
    Processed,
    Idle,
    Shutdown,
}

impl IsolateRunner {
    fn poll_one_tick(&mut self) -> TickOutcome {
        if self.counters.shutdown.load(Ordering::Acquire) {
            self.shutdown();
            return TickOutcome::Shutdown;
        }
        if let Some(command) = self.deferred_commands.pop_front() {
            return self.process_message(RuntimeMessage::Command(command));
        }
        let msg = match self.rx.try_recv() {
            Ok(msg) => msg,
            Err(mpsc::error::TryRecvError::Empty) => return TickOutcome::Idle,
            Err(mpsc::error::TryRecvError::Disconnected) => {
                self.shutdown();
                return TickOutcome::Shutdown;
            }
        };
        self.process_message(msg)
    }

    /// Remember the first unhandled task/timer error of the turn; the
    /// in-flight command's `drive_event_loop_to_idle` takes it as the run's
    /// failure. Later errors of the same turn lose to the first, as they do
    /// in Node, where the first uncaught exception ends the process.
    fn record_fatal_task_error(&mut self, error: OtterError) {
        if self.fatal_task_error.is_none() {
            self.fatal_task_error = Some(error);
        }
    }

    /// Surface a fatal task error that no command waiter will ever receive.
    /// Reaching this outside a driven turn means the embedder is ticking the
    /// inbox directly; stderr is the only remaining sink.
    fn report_stray_fatal_task_error(&mut self) {
        if let Some(error) = self.fatal_task_error.take() {
            eprintln!("{error}");
        }
    }

    fn run_until_idle(&mut self) {
        loop {
            match self.poll_one_tick() {
                TickOutcome::Processed | TickOutcome::Idle => {}
                TickOutcome::Shutdown => return,
            }
            self.report_stray_fatal_task_error();
            // Every poll consumes a message, so every outcome must be
            // honoured here. Discarding a consumed `Shutdown` would re-enter
            // the blocking receive even though the atomic shutdown signal has
            // already asked this detached isolate thread to terminate.
            match self.poll_one_tick() {
                TickOutcome::Processed => {}
                TickOutcome::Shutdown => return,
                TickOutcome::Idle => match self.rx.blocking_recv() {
                    Some(msg) => {
                        if matches!(self.process_message(msg), TickOutcome::Shutdown) {
                            return;
                        }
                    }
                    None => {
                        self.shutdown();
                        return;
                    }
                },
            }
        }
    }

    fn shutdown(&mut self) {
        if self.shutdown {
            return;
        }
        self.shutdown = true;
        self.completion_pool.close();
        self.counters.shutdown.store(true, Ordering::Release);
        self.module_cancellation.cancel();
        // Closing the sole receiver wakes every ordered sender and the one
        // guaranteed-delivery pump. Nothing accepted after this point can be
        // stranded behind a dead isolate.
        self.rx.close();
        self.inbox.cancel_pending();

        let deferred = self.deferred_commands.len();
        self.deferred_commands.clear();
        decrement_queued_commands(&self.counters, deferred);
        while let Ok(message) = self.rx.try_recv() {
            cancel_runtime_message(message, &self.counters);
        }
    }

    fn process_message(&mut self, msg: RuntimeMessage) -> TickOutcome {
        // Shutdown can race the gap between a receive/poll and this dispatch.
        // A command also performs a definitive check after clearing only a
        // stale interrupt in `prepare_command_dispatch`.
        if self.counters.shutdown.load(Ordering::Acquire)
            && !matches!(&msg, RuntimeMessage::Shutdown)
        {
            cancel_runtime_message(msg, &self.counters);
            self.shutdown();
            return TickOutcome::Shutdown;
        }
        match msg {
            RuntimeMessage::Command(queued) => {
                let command = match self.prepare_command_dispatch(queued) {
                    Ok(command) => command,
                    Err(outcome) => return outcome,
                };
                let id = command.id();
                self.run_command(command);
                self.record_microtask_snapshot();
                if self.shutdown {
                    return TickOutcome::Shutdown;
                }
                if id == 0 {
                    return TickOutcome::Idle;
                }
                TickOutcome::Processed
            }
            RuntimeMessage::Guaranteed(post) => self.process_guaranteed(post),
            RuntimeMessage::RuntimeTask { task, liveness } => {
                if self.exit_finalized {
                    // The 'exit' event already ran; a task landing now belongs
                    // to a process that no longer exists in Node terms.
                    task.cancel(&mut self.runtime);
                    self.counters.cancel_host_activity(liveness);
                    return TickOutcome::Processed;
                }
                let result = task.run(&mut self.runtime);
                decrement_liveness(
                    liveness,
                    &self.counters.pending_ref_host_ops,
                    &self.counters.pending_unref_host_ops,
                );
                match result {
                    Ok(()) => {
                        self.counters
                            .completed_host_ops
                            .fetch_add(1, Ordering::Relaxed);
                    }
                    Err(error) => {
                        self.counters
                            .failed_host_ops
                            .fetch_add(1, Ordering::Relaxed);
                        // See the timer branch: shutdown's cooperative
                        // interrupt tearing down an in-flight task is not a
                        // run failure.
                        let teardown_interrupt = matches!(error, OtterError::Interrupted)
                            && self.counters.shutdown.load(Ordering::Acquire);
                        if !teardown_interrupt {
                            self.record_fatal_task_error(error);
                        }
                    }
                }
                self.record_microtask_snapshot();
                TickOutcome::Processed
            }
            RuntimeMessage::TimerFired {
                token,
                expects_js_callback,
            } => {
                if self.exit_finalized && expects_js_callback {
                    // See the RuntimeTask branch: after the 'exit' event no
                    // scheduled callback may run. An unknown token is stale
                    // and owns no aggregate hold in this isolate.
                    self.cancel_timer_ownership(token);
                    return TickOutcome::Processed;
                }
                if !expects_js_callback {
                    if let Some(timer) = self.counters.timer_take(token.0) {
                        self.counters.fired_timers.fetch_add(1, Ordering::Relaxed);
                        decrement_liveness(
                            timer.liveness,
                            &self.counters.pending_ref_timers,
                            &self.counters.pending_unref_timers,
                        );
                    }
                    return TickOutcome::Processed;
                }
                // Drive the JS callback associated with `token` through the
                // runtime. An `Err` here means `process` and any active
                // domain already refused the throw inside `fire_timer`, so
                // it is fatal for the run.
                match self.runtime.fire_timer(token.0) {
                    Ok(TimerFireOutcome::Missing) => {
                        // The VM has no callback under this token any more, so
                        // nothing will fire it again. Whoever dropped the
                        // callback did not necessarily reach the scheduler's
                        // cancel path, and a hold nobody will ever release
                        // keeps the loop waiting for a timer that no longer
                        // exists. A token the cancel path already released is
                        // gone from the map and takes nothing here.
                        self.cancel_timer_ownership(token);
                    }
                    Ok(TimerFireOutcome::Fired { repeat }) => {
                        self.counters.fired_timers.fetch_add(1, Ordering::Relaxed);
                        // `None` means the callback itself cleared the timer
                        // and the cancel path already released the hold; a
                        // second decrement would free a hold someone else
                        // still counts on.
                        if !repeat && let Some(timer) = self.counters.timer_take(token.0) {
                            decrement_liveness(
                                timer.liveness,
                                &self.counters.pending_ref_timers,
                                &self.counters.pending_unref_timers,
                            );
                        }
                    }
                    Err(error) => {
                        self.counters.fired_timers.fetch_add(1, Ordering::Relaxed);
                        // An uncaught timer callback ends this hosted process.
                        // Cancel through the VM/scheduler boundary while the
                        // isolate still owns the token: merely dropping its
                        // liveness entry would leave a repeating driver armed.
                        self.cancel_timer_ownership(token);
                        // A timer that fires between shutdown's cooperative
                        // interrupt and the Shutdown message observing it is
                        // torn down on purpose — that interruption is not a
                        // run failure.
                        let teardown_interrupt = matches!(error, OtterError::Interrupted)
                            && self.counters.shutdown.load(Ordering::Acquire);
                        if !teardown_interrupt {
                            self.record_fatal_task_error(error);
                        }
                    }
                }
                self.record_microtask_snapshot();
                TickOutcome::Processed
            }
            #[cfg(test)]
            RuntimeMessage::DynamicModuleReady(_id) => {
                self.counters
                    .pending_dynamic_module_jobs
                    .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| v.checked_sub(1))
                    .ok();
                self.counters
                    .completed_dynamic_module_jobs
                    .fetch_add(1, Ordering::Relaxed);
                TickOutcome::Processed
            }
            #[cfg(test)]
            RuntimeMessage::Diagnostic(diagnostic) => {
                let _ = diagnostic;
                self.counters.diagnostics.fetch_add(1, Ordering::Relaxed);
                TickOutcome::Processed
            }
            RuntimeMessage::Interrupt => {
                self.runtime.interrupt_handle().interrupt();
                TickOutcome::Processed
            }
            RuntimeMessage::Shutdown => {
                self.shutdown();
                TickOutcome::Shutdown
            }
        }
    }

    fn process_guaranteed(&mut self, post: GuaranteedPost) -> TickOutcome {
        let GuaranteedPost {
            payload,
            completion,
        } = post;
        match payload {
            GuaranteedPayload::RuntimeTask { task, outcome } => {
                let active = completion.begin_dispatch();
                if self.exit_finalized {
                    task.cancel(&mut self.runtime);
                    drop(active);
                    return TickOutcome::Processed;
                }
                match task.run(&mut self.runtime) {
                    Ok(()) => match outcome {
                        otter_vm::host_completion::HostCompletionOutcome::Completed => {
                            active.complete();
                        }
                        otter_vm::host_completion::HostCompletionOutcome::Failed => {
                            active.fail();
                        }
                        otter_vm::host_completion::HostCompletionOutcome::Cancelled => {
                            drop(active);
                        }
                    },
                    Err(error) => {
                        active.fail();
                        let teardown_interrupt = matches!(error, OtterError::Interrupted)
                            && self.counters.shutdown.load(Ordering::Acquire);
                        if !teardown_interrupt {
                            self.record_fatal_task_error(error);
                        }
                    }
                }
                self.record_microtask_snapshot();
                TickOutcome::Processed
            }
            GuaranteedPayload::TimerFired {
                token,
                expects_js_callback,
            } => {
                let active = completion.begin_dispatch();
                if self.exit_finalized && expects_js_callback {
                    let _ = self.runtime.cancel_timer_callback(token.0);
                    drop(active);
                    return TickOutcome::Processed;
                }
                if !expects_js_callback {
                    self.counters.fired_timers.fetch_add(1, Ordering::Relaxed);
                    active.complete();
                    return TickOutcome::Processed;
                }
                match self.runtime.fire_timer(token.0) {
                    Ok(TimerFireOutcome::Missing) => drop(active),
                    Ok(TimerFireOutcome::Fired { .. }) => {
                        self.counters.fired_timers.fetch_add(1, Ordering::Relaxed);
                        active.complete();
                    }
                    Err(error) => {
                        self.counters.fired_timers.fetch_add(1, Ordering::Relaxed);
                        active.fail();
                        let _ = self.runtime.cancel_timer_callback(token.0);
                        let teardown_interrupt = matches!(error, OtterError::Interrupted)
                            && self.counters.shutdown.load(Ordering::Acquire);
                        if !teardown_interrupt {
                            self.record_fatal_task_error(error);
                        }
                    }
                }
                self.record_microtask_snapshot();
                TickOutcome::Processed
            }
            GuaranteedPayload::DynamicImportLoad {
                token,
                specifier,
                referrer,
                attr_type,
            } => {
                if self.exit_finalized {
                    let _ = self.runtime.cancel_dynamic_import(token);
                    drop(completion);
                    return TickOutcome::Processed;
                }
                match self.runtime.begin_dynamic_import(
                    token,
                    &specifier,
                    &referrer,
                    attr_type.as_deref(),
                ) {
                    Ok(DynamicImportBegin::Settled) => completion.begin_dispatch().complete(),
                    Err(error) => {
                        let _ = self.runtime.cancel_dynamic_import(token);
                        completion.begin_dispatch().fail();
                        self.record_fatal_task_error(error);
                    }
                    Ok(DynamicImportBegin::FetchHttps { target_url }) => {
                        let preparation = self.module_preparation.clone();
                        let task_handle = self.module_task_handle.clone();
                        let inbox = self.inbox.clone();
                        let cancellation = crate::module_loader::ModuleLoadCancellation::linked(
                            &self.module_cancellation,
                        );
                        let preparation_target = target_url.clone();
                        let pending = PendingDynamicImportPreparation::new(
                            inbox, token, target_url, completion,
                        );
                        task_handle.spawn(async move {
                            let result = preparation
                                .prepare_remote_entry(preparation_target, cancellation)
                                .await
                                .map_err(|error| error.to_string());
                            pending.finish(result);
                        });
                    }
                }
                self.record_microtask_snapshot();
                TickOutcome::Processed
            }
            GuaranteedPayload::DynamicImportGraphPrepared {
                token,
                target_url,
                result,
            } => {
                if self.exit_finalized {
                    let _ = self.runtime.cancel_dynamic_import(token);
                    drop(completion);
                    return TickOutcome::Processed;
                }
                let active = completion.begin_dispatch();
                match self.runtime.complete_dynamic_import_prepared(
                    token,
                    &target_url,
                    result.map(|linked| *linked),
                ) {
                    Ok(_) => active.complete(),
                    Err(error) => {
                        let _ = self.runtime.cancel_dynamic_import(token);
                        active.fail();
                        self.record_fatal_task_error(error);
                    }
                }
                self.record_microtask_snapshot();
                TickOutcome::Processed
            }
        }
    }

    fn cancel_timer_ownership(&mut self, token: TimerToken) {
        if self.runtime.cancel_timer_callback(token.0) {
            return;
        }
        if let Some(timer) = self.counters.timer_take(token.0) {
            decrement_liveness(
                timer.liveness,
                &self.counters.pending_ref_timers,
                &self.counters.pending_unref_timers,
            );
            self.counters
                .cancelled_timers
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Clear a stale interrupt before the final shutdown linearization point.
    /// Any shutdown starting after the Acquire check will publish a fresh
    /// interrupt that command execution never resets.
    fn prepare_command_dispatch(
        &mut self,
        queued: QueuedCommand,
    ) -> Result<RuntimeCommand, TickOutcome> {
        self.runtime.interrupt_handle().reset();
        if self.counters.shutdown.load(Ordering::Acquire) {
            cancel_runtime_message(RuntimeMessage::Command(queued), &self.counters);
            self.shutdown();
            return Err(TickOutcome::Shutdown);
        }
        self.counters
            .queued_commands
            .fetch_sub(1, Ordering::Relaxed);
        let QueuedCommand { command, lease } = queued;
        drop(lease);
        Ok(command)
    }

    fn run_command(&mut self, command: RuntimeCommand) {
        self.counters.running_command.store(true, Ordering::Relaxed);
        // A new command is a fresh run; its callbacks are live again.
        self.exit_finalized = false;
        match command {
            RuntimeCommand::CheckFile { path, reply, .. } => {
                // Compile-only, no event loop driving needed.
                send_check_reply(reply, self.runtime.check_file(path), &self.counters);
            }
            RuntimeCommand::RunFile { path, reply, .. } => {
                let result = self.runtime.run_file(path);
                let result = self.drive_event_loop_to_idle(result);
                let (result, exit_override) = self.finalize_process_exit(result);
                let attempt = self
                    .runtime
                    .finish_jit_debug_attempt(result)
                    .with_exit_code_override(exit_override);
                send_run_reply(reply, attempt, &self.counters);
            }
            RuntimeCommand::RunScript {
                source,
                specifier,
                reply,
                ..
            } => {
                let result = self.runtime.run_script(source, &specifier);
                let result = self.drive_event_loop_to_idle(result);
                let (result, exit_override) = self.finalize_process_exit(result);
                let attempt = self
                    .runtime
                    .finish_jit_debug_attempt(result)
                    .with_exit_code_override(exit_override);
                send_run_reply(reply, attempt, &self.counters);
            }
            RuntimeCommand::CreateRealm { reply, .. } => {
                let result = self.runtime.create_realm();
                send_realm_reply(reply, result, &self.counters);
            }
            RuntimeCommand::DisposeRealm { realm, reply, .. } => {
                let result = self.runtime.dispose_realm(realm);
                send_check_reply(reply, result, &self.counters);
            }
            RuntimeCommand::RunScriptInRealm {
                realm,
                source,
                specifier,
                reply,
                ..
            } => {
                let result = self.runtime.run_script_in_realm(realm, source, &specifier);
                let result = self.drive_event_loop_to_idle(result);
                let attempt = self.runtime.finish_jit_debug_attempt(result);
                send_run_reply(reply, attempt, &self.counters);
            }
            RuntimeCommand::RunModule { linked, reply, .. } => {
                let result = self.runtime.run_prepared_module(linked);
                let result = self.drive_event_loop_to_idle(result);
                let (result, exit_override) = self.finalize_process_exit(result);
                let attempt = self
                    .runtime
                    .finish_jit_debug_attempt(result)
                    .with_exit_code_override(exit_override);
                send_run_reply(reply, attempt, &self.counters);
            }
            RuntimeCommand::RunModuleSource { linked, reply, .. } => {
                let result = self.runtime.run_prepared_module(linked);
                let result = self.drive_event_loop_to_idle(result);
                let (result, exit_override) = self.finalize_process_exit(result);
                let attempt = self
                    .runtime
                    .finish_jit_debug_attempt(result)
                    .with_exit_code_override(exit_override);
                send_run_reply(reply, attempt, &self.counters);
            }
            RuntimeCommand::RunModuleInRealm {
                realm,
                linked,
                reply,
                ..
            } => {
                let result = self.runtime.run_prepared_module_in_realm(realm, linked);
                let result = self.drive_event_loop_to_idle(result);
                let attempt = self.runtime.finish_jit_debug_attempt(result);
                send_run_reply(reply, attempt, &self.counters);
            }
            RuntimeCommand::Eval {
                source,
                commonjs_scope,
                reply,
                ..
            } => {
                let result = match commonjs_scope {
                    Some(cwd) => self
                        .runtime
                        .install_commonjs_eval_scope(&cwd)
                        .and_then(|()| self.runtime.eval(source)),
                    None => self.runtime.eval(source),
                };
                let result = self.drive_event_loop_to_idle(result);
                let (result, exit_override) = self.finalize_process_exit(result);
                let attempt = self
                    .runtime
                    .finish_jit_debug_attempt(result)
                    .with_exit_code_override(exit_override);
                send_run_reply(reply, attempt, &self.counters);
            }
        }
        self.counters
            .running_command
            .store(false, Ordering::Relaxed);
        self.runtime.interrupt_handle().reset();
    }

    /// Drive the inbox until pending Ref'd timers / host ops drop
    /// to zero. Mirrors the Node / Deno run-loop semantics: a
    /// command's reply is held until the event loop is idle so
    /// `await otter.run_script(\"setTimeout(...)\")` observes the
    /// timer callback before resolving.
    ///
    /// On script error, the loop is short-circuited; pending
    /// timers are not run because the reply already carries the
    /// failure. [`Self::finalize_process_exit`] cancels the command's
    /// leftover JavaScript timers before its reply is sent.
    /// Emit `'beforeExit'` with the current `process.exitCode`. Answers
    /// `Ok(Some(code))` when a listener called `process.exit(code)` (the run
    /// completes with that code), `Ok(None)` on a normal return, and `Err`
    /// when a listener threw — an uncaught exception, exactly as in Node.
    fn emit_before_exit(&mut self) -> Result<Option<u8>, OtterError> {
        let script = "typeof process === 'object' && typeof process.emit === 'function' \
            ? (process.emit('beforeExit', typeof process.exitCode === 'number' ? process.exitCode : 0), 0) \
            : 0";
        match self.runtime.eval(SourceInput::from_javascript(script)) {
            Ok(result) if result.explicit_exit() => Ok(Some(result.exit_code())),
            Ok(_) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Fire the process `'exit'` event exactly once after a run completes and
    /// fold a listener's replacement code into the result. Runs the hook as a
    /// bare script — never through [`Self::drive_event_loop_to_idle`] — so a
    /// handle the run left open cannot stall completion.
    ///
    /// A failed run still fires the event, exactly as Node's uncaught path
    /// does: the error is preserved for rendering, and the second element
    /// carries the exit code the listeners settled on (default: the error's
    /// own recommended code). After listeners finish (or immediately for a
    /// teardown-shaped failure), every JavaScript timer is detached and its
    /// host deadline cancelled before the command reply becomes observable.
    fn finalize_process_exit(
        &mut self,
        result: Result<ExecutionResult, OtterError>,
    ) -> (Result<ExecutionResult, OtterError>, Option<u8>) {
        // From here on the run is over in Node terms. Setting the guard before
        // evaluating listeners also suppresses any wake that raced teardown;
        // callbacks scheduled by a listener are collected immediately after
        // the listener returns.
        self.exit_finalized = true;
        let finalized = self.emit_process_exit(result);
        let _ = self.runtime.cancel_all_timers();
        finalized
    }

    fn emit_process_exit(
        &mut self,
        result: Result<ExecutionResult, OtterError>,
    ) -> (Result<ExecutionResult, OtterError>, Option<u8>) {
        let inner = match result {
            Ok(inner) => inner,
            Err(error) => {
                // Teardown-shaped failures never reach user listeners.
                if matches!(
                    error,
                    OtterError::Interrupted
                        | OtterError::Timeout { .. }
                        | OtterError::OutOfMemory { .. }
                ) {
                    return (Err(error), None);
                }
                let code = u8::try_from(error.exit_code().clamp(0, 255)).unwrap_or(1);
                let script = format!(
                    "typeof process === 'object' && typeof process.__otterEmitExit === 'function' ? process.__otterEmitExit({code}, true) : {code}"
                );
                let final_code = match self.runtime.eval(SourceInput::from_javascript(script)) {
                    Ok(emit_result) => emit_result
                        .completion_string()
                        .parse::<i64>()
                        .ok()
                        .map(|value| value.clamp(0, 255) as u8)
                        .unwrap_or_else(|| emit_result.exit_code()),
                    // A second failure inside an exit listener cannot improve
                    // on the original error; the run keeps its code.
                    Err(_) => code,
                };
                // Only a listener that actually chose a different code
                // overrides the failure. Reporting the unchanged code as an
                // override would hide the error itself from the caller, which
                // is what renders the diagnostic.
                let override_code = (final_code != code).then_some(final_code);
                return (Err(error), override_code);
            }
        };
        let code = inner.exit_code();
        let script = format!(
            "typeof process === 'object' && typeof process.__otterEmitExit === 'function' ? process.__otterEmitExit({code}, false) : {code}"
        );
        match self.runtime.eval(SourceInput::from_javascript(script)) {
            Ok(emit_result) => {
                // Normal completion answers the hook's final code as the
                // completion value; a nested `process.exit(newCode)` in a
                // listener surfaces as an exit-shaped result whose own code
                // is the replacement.
                let final_code = emit_result
                    .completion_string()
                    .parse::<i64>()
                    .ok()
                    .map(|value| value.clamp(0, 255) as u8)
                    .unwrap_or_else(|| emit_result.exit_code());
                (Ok(inner.with_exit_code(final_code)), None)
            }
            // An uncaught throw in an exit listener fails the run the way
            // Node's does.
            Err(error) => (Err(error), None),
        }
    }

    fn drive_event_loop_to_idle(
        &mut self,
        initial: Result<ExecutionResult, OtterError>,
    ) -> Result<ExecutionResult, OtterError> {
        // Everything a server does happens while the loop drains, long after
        // entry evaluation handed back its result, so the samples taken during
        // the drain are collected here rather than at the entry's return.
        match self.drain_event_loop(initial) {
            Ok(result) => Ok(self.runtime.attach_cpu_profile(result)),
            Err(error) => Err(error),
        }
    }

    fn drain_event_loop(
        &mut self,
        initial: Result<ExecutionResult, OtterError>,
    ) -> Result<ExecutionResult, OtterError> {
        // Clippy `question_mark` suggests `as_ref()?` but the
        // function returns `Result<T, OtterError>` while `as_ref`
        // gives `Result<&T, &OtterError>`; rewriting would force
        // an extra clone path that does not pay back.
        #[allow(clippy::question_mark)]
        if initial.is_err() {
            return initial;
        }
        // An explicit `process.exit` during entry evaluation terminates the
        // run before any queued work: live listeners or timers must not hold
        // the loop open past a requested exit.
        if initial
            .as_ref()
            .map(ExecutionResult::explicit_exit)
            .unwrap_or(false)
        {
            return initial;
        }
        loop {
            // `shutdown()` is deliberately non-blocking and its wake message
            // may lose a `try_send` race to a full inbox. A full inbox already
            // wakes this driven turn; observing the atomic here makes that
            // first received item sufficient to finish teardown.
            if self.counters.shutdown.load(Ordering::Acquire) {
                self.shutdown();
                return initial;
            }
            // An exit requested from a task or timer callback completes the
            // run with that code, exactly as an exit during entry evaluation
            // does.
            if let Some(code) = self.runtime.take_pending_exit_code() {
                let duration = initial
                    .as_ref()
                    .map(|result| result.duration)
                    .unwrap_or_default();
                return Ok(ExecutionResult::from_exit_code(code, duration));
            }
            // An unhandled throw from a task or timer callback fails the run:
            // the entry value is gone the way Node's is when an uncaught
            // exception ends the process.
            if let Some(error) = self.fatal_task_error.take() {
                return Err(error);
            }
            let pending_ref_timers = self.counters.pending_ref_timers.load(Ordering::Relaxed);
            let pending_ref_host_ops = self.counters.pending_ref_host_ops.load(Ordering::Relaxed);
            if pending_ref_timers == 0 && pending_ref_host_ops == 0 {
                // The loop drained without an explicit exit: Node emits
                // `'beforeExit'` here, and a listener may revive the loop by
                // scheduling new work. An exit-shaped completion skips it.
                let explicit = initial
                    .as_ref()
                    .map(ExecutionResult::explicit_exit)
                    .unwrap_or(true);
                if explicit || self.exit_finalized {
                    return initial;
                }
                match self.emit_before_exit() {
                    Ok(Some(code)) => {
                        let duration = initial
                            .as_ref()
                            .map(|result| result.duration)
                            .unwrap_or_default();
                        return Ok(ExecutionResult::from_exit_code(code, duration));
                    }
                    Ok(None) => {}
                    Err(error) => return Err(error),
                }
                if let Some(code) = self.runtime.take_pending_exit_code() {
                    let duration = initial
                        .as_ref()
                        .map(|result| result.duration)
                        .unwrap_or_default();
                    return Ok(ExecutionResult::from_exit_code(code, duration));
                }
                let timers = self.counters.pending_ref_timers.load(Ordering::Relaxed);
                let host_ops = self.counters.pending_ref_host_ops.load(Ordering::Relaxed);
                if timers == 0 && host_ops == 0 {
                    return initial;
                }
                // A listener scheduled new work — keep driving; the event
                // re-fires on the next drain, exactly as Node's does.
                continue;
            }
            // Block on the next inbox item. A later public command is deferred
            // until this command's Ref'd work finishes: recursively running it
            // would interleave isolate state and diagnostics batches.
            let msg = match self.rx.blocking_recv() {
                Some(msg) => msg,
                None => {
                    self.shutdown();
                    return initial;
                }
            };
            let msg = match msg {
                RuntimeMessage::Command(command) => {
                    self.deferred_commands.push_back(command);
                    continue;
                }
                other => other,
            };
            if matches!(self.process_message(msg), TickOutcome::Shutdown) {
                return initial;
            }
        }
    }

    fn record_microtask_snapshot(&self) {
        let stats = self.runtime.microtask_stats();
        self.counters
            .pending_microtasks
            .store(stats.pending, Ordering::Relaxed);
        self.counters
            .microtask_generation
            .store(stats.generation, Ordering::Relaxed);
    }
}

impl RuntimeCommand {
    fn id(&self) -> CommandId {
        match self {
            RuntimeCommand::CheckFile { id, .. }
            | RuntimeCommand::RunFile { id, .. }
            | RuntimeCommand::RunScript { id, .. }
            | RuntimeCommand::CreateRealm { id, .. }
            | RuntimeCommand::DisposeRealm { id, .. }
            | RuntimeCommand::RunScriptInRealm { id, .. }
            | RuntimeCommand::RunModule { id, .. }
            | RuntimeCommand::RunModuleSource { id, .. }
            | RuntimeCommand::RunModuleInRealm { id, .. }
            | RuntimeCommand::Eval { id, .. } => *id,
        }
    }
}

fn send_run_reply(reply: RunReply, result: ExecutionAttempt, counters: &RuntimeCounters) {
    if reply.send(result).is_err() {
        counters.cancelled_waiters.fetch_add(1, Ordering::Relaxed);
    }
}

fn send_check_reply(reply: CheckReply, result: Result<(), OtterError>, counters: &RuntimeCounters) {
    if reply.send(result).is_err() {
        counters.cancelled_waiters.fetch_add(1, Ordering::Relaxed);
    }
}

fn send_realm_reply(
    reply: RealmReply,
    result: Result<crate::RuntimeRealmId, OtterError>,
    counters: &RuntimeCounters,
) {
    if reply.send(result).is_err() {
        counters.cancelled_waiters.fetch_add(1, Ordering::Relaxed);
    }
}

fn increment_liveness(
    liveness: RuntimeLiveness,
    ref_counter: &AtomicUsize,
    unref_counter: &AtomicUsize,
) {
    match liveness {
        RuntimeLiveness::Ref => {
            ref_counter.fetch_add(1, Ordering::Relaxed);
        }
        RuntimeLiveness::Unref => {
            unref_counter.fetch_add(1, Ordering::Relaxed);
        }
    }
}

fn decrement_liveness(
    liveness: RuntimeLiveness,
    ref_counter: &AtomicUsize,
    unref_counter: &AtomicUsize,
) {
    let counter = match liveness {
        RuntimeLiveness::Ref => ref_counter,
        RuntimeLiveness::Unref => unref_counter,
    };
    let _ = counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| v.checked_sub(1));
}

fn cancel_completion_accounting(counters: &RuntimeCounters, accounting: CompletionAccounting) {
    match accounting {
        CompletionAccounting::Host(liveness) => counters.cancel_host_activity(liveness),
        CompletionAccounting::Timer(liveness) => {
            decrement_liveness(
                liveness,
                &counters.pending_ref_timers,
                &counters.pending_unref_timers,
            );
            counters.cancelled_timers.fetch_add(1, Ordering::Relaxed);
        }
    }
}

fn complete_completion_accounting(counters: &RuntimeCounters, accounting: CompletionAccounting) {
    match accounting {
        CompletionAccounting::Host(liveness) => counters.complete_host_activity(liveness),
        CompletionAccounting::Timer(liveness) => {
            decrement_liveness(
                liveness,
                &counters.pending_ref_timers,
                &counters.pending_unref_timers,
            );
        }
    }
}

fn fail_completion_accounting(counters: &RuntimeCounters, accounting: CompletionAccounting) {
    match accounting {
        CompletionAccounting::Host(liveness) => {
            decrement_liveness(
                liveness,
                &counters.pending_ref_host_ops,
                &counters.pending_unref_host_ops,
            );
            counters.failed_host_ops.fetch_add(1, Ordering::Relaxed);
        }
        CompletionAccounting::Timer(liveness) => {
            decrement_liveness(
                liveness,
                &counters.pending_ref_timers,
                &counters.pending_unref_timers,
            );
        }
    }
}

fn decrement_queued_commands(counters: &RuntimeCounters, amount: usize) {
    if amount == 0 {
        return;
    }
    let _ = counters
        .queued_commands
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |queued| {
            Some(queued.saturating_sub(amount))
        });
}

/// Release accounting for an accepted inbox item that shutdown will not run.
fn cancel_runtime_message(message: RuntimeMessage, counters: &RuntimeCounters) {
    match message {
        RuntimeMessage::Command(_) => decrement_queued_commands(counters, 1),
        RuntimeMessage::Guaranteed(_) => {}
        RuntimeMessage::RuntimeTask { liveness, .. } => {
            counters.cancel_host_activity(liveness);
        }
        RuntimeMessage::TimerFired { token, .. } => {
            if let Some(timer) = counters.timer_take(token.0) {
                decrement_liveness(
                    timer.liveness,
                    &counters.pending_ref_timers,
                    &counters.pending_unref_timers,
                );
                counters.cancelled_timers.fetch_add(1, Ordering::Relaxed);
            }
        }
        #[cfg(test)]
        RuntimeMessage::DynamicModuleReady(_) => {
            let _ = counters.pending_dynamic_module_jobs.fetch_update(
                Ordering::Relaxed,
                Ordering::Relaxed,
                |pending| pending.checked_sub(1),
            );
        }
        #[cfg(test)]
        RuntimeMessage::Diagnostic(_) => {}
        RuntimeMessage::Interrupt | RuntimeMessage::Shutdown => {}
    }
}

#[cfg(test)]
mod inbox_tests {
    use super::*;

    struct MarkExecuted(Arc<AtomicBool>);

    impl RuntimeTask for MarkExecuted {
        fn run(self: Box<Self>, _runtime: &mut Runtime) -> Result<(), OtterError> {
            self.0.store(true, Ordering::Release);
            Ok(())
        }
    }

    fn inbox(
        capacity: usize,
    ) -> (
        InboxSender,
        mpsc::Receiver<RuntimeMessage>,
        Arc<RuntimeCounters>,
    ) {
        let counters = Arc::new(RuntimeCounters::new());
        let (tx, rx) = mpsc::channel(capacity);
        let inbox = InboxSender::new(tx, tokio::runtime::Handle::current(), counters.clone());
        (inbox, rx, counters)
    }

    fn completion_pool() -> CompletionAdmissionPool {
        let config = RuntimeConfig::default();
        CompletionAdmissionPool::new(
            config.resource_account.clone(),
            config.completion_capacities(),
        )
    }

    fn bounded_completion_pool(account: ResourceAccount) -> CompletionAdmissionPool {
        CompletionAdmissionPool::new(
            account,
            crate::completion_admission::CompletionCapacities {
                guaranteed: 1,
                host_operations: 1,
                timers: 1,
            },
        )
    }

    fn isolate_runner(capacity: usize) -> IsolateRunner {
        let config = RuntimeConfig::default();
        let event_loop = TokioEventLoop::current_or_owned().expect("test event loop");
        let module_preparation = ModulePreparation::new(&config, &event_loop);
        let module_cancellation = crate::module_loader::ModuleLoadCancellation::new();
        let module_task_handle = event_loop.handle();
        let runtime = Runtime::builder().build().expect("test runtime");
        let counters = Arc::new(RuntimeCounters::new());
        let (tx, rx) = mpsc::channel(capacity);
        let inbox = InboxSender::new(tx, module_task_handle.clone(), counters.clone());
        IsolateRunner {
            runtime,
            rx,
            inbox,
            counters,
            module_preparation,
            module_cancellation,
            module_task_handle,
            deferred_commands: VecDeque::new(),
            fatal_task_error: None,
            exit_finalized: false,
            shutdown: false,
            completion_pool: completion_pool(),
        }
    }

    fn timer_scheduler(
        inbox: InboxSender,
        event_loop: TokioEventLoop,
        counters: Arc<RuntimeCounters>,
    ) -> InboxTimerScheduler {
        InboxTimerScheduler {
            inbox,
            event_loop,
            counters,
            completion_pool: completion_pool(),
        }
    }

    fn host_completion(
        sequence: u64,
        pool: &CompletionAdmissionPool,
        counters: Arc<RuntimeCounters>,
    ) -> GuaranteedPost {
        GuaranteedPost {
            payload: GuaranteedPayload::DynamicImportGraphPrepared {
                token: sequence,
                target_url: sequence.to_string(),
                result: Err("test completion".to_string()),
            },
            completion: QueuedCompletion::admit_host(pool, counters, RuntimeLiveness::Unref)
                .expect("test host completion admission"),
        }
    }

    fn schedule_test_timer(
        scheduler: &InboxTimerScheduler,
        delay_ms: u64,
        repeat_ms: Option<u64>,
    ) -> u64 {
        let admission = scheduler
            .admit(repeat_ms.is_some())
            .expect("test timer admission");
        scheduler
            .schedule(admission, delay_ms, repeat_ms)
            .expect("test timer scheduling")
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ordered_send_waits_for_capacity_and_preserves_fifo() {
        let (inbox, mut rx, _counters) = inbox(1);
        assert!(
            inbox
                .try_send(RuntimeMessage::Diagnostic(RuntimeDiagnostic {
                    _origin: "first".to_string(),
                    _message: String::new(),
                }))
                .is_ok(),
            "first message fills the inbox"
        );

        let ordered = inbox.clone();
        let join = tokio::spawn(async move {
            ordered
                .send_ordered(RuntimeMessage::Diagnostic(RuntimeDiagnostic {
                    _origin: "second".to_string(),
                    _message: String::new(),
                }))
                .await
        });
        tokio::task::yield_now().await;
        assert!(!join.is_finished(), "ordered sender must wait for capacity");

        let RuntimeMessage::Diagnostic(first) = rx.recv().await.expect("first message") else {
            panic!("unexpected first message");
        };
        assert_eq!(first._origin, "first");
        assert!(
            tokio::time::timeout(Duration::from_secs(2), join)
                .await
                .expect("ordered sender wake")
                .expect("ordered sender task")
                .is_ok()
        );
        let RuntimeMessage::Diagnostic(second) = rx.recv().await.expect("second message") else {
            panic!("unexpected second message");
        };
        assert_eq!(second._origin, "second");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn aborting_ordered_send_releases_pending_activity() {
        let (inbox, mut rx, counters) = inbox(1);
        assert!(
            inbox.try_send(RuntimeMessage::Interrupt).is_ok(),
            "blocker fills the inbox"
        );
        let queue = InboxRuntimeTaskQueue {
            inbox,
            counters: counters.clone(),
            completion_pool: completion_pool(),
        };
        let executed = Arc::new(AtomicBool::new(false));
        let future = queue.enqueue_boxed_ordered(
            Box::new(MarkExecuted(executed.clone())),
            RuntimeLiveness::Ref,
        );
        let join = tokio::spawn(future);

        tokio::time::timeout(Duration::from_secs(2), async {
            while counters.pending_ref_host_ops.load(Ordering::Acquire) != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("ordered task retained its activity while awaiting capacity");
        join.abort();
        assert!(
            join.await
                .expect_err("ordered producer must be aborted")
                .is_cancelled(),
            "producer abort should cancel its capacity wait"
        );

        assert_eq!(counters.pending_ref_host_ops.load(Ordering::Relaxed), 0);
        assert_eq!(counters.cancelled_host_ops.load(Ordering::Relaxed), 1);
        assert!(!executed.load(Ordering::Acquire));
        assert!(matches!(rx.recv().await, Some(RuntimeMessage::Interrupt)));
        assert!(matches!(
            rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
    }

    #[tokio::test]
    async fn foreign_completion_carriers_are_rejected_without_panic_or_retention() {
        let event_loop = TokioEventLoop::current_or_owned().expect("test event loop");
        let (inbox_a, mut rx_a, counters_a) = inbox(4);
        let (inbox_b, mut rx_b, counters_b) = inbox(4);
        let account_a = ResourceAccount::default();
        let account_b = ResourceAccount::default();
        let pool_a = bounded_completion_pool(account_a.clone());
        let pool_b = bounded_completion_pool(account_b.clone());

        let queue_a = InboxRuntimeTaskQueue {
            inbox: inbox_a.clone(),
            counters: counters_a.clone(),
            completion_pool: pool_a.clone(),
        };
        let queue_b = InboxRuntimeTaskQueue {
            inbox: inbox_b.clone(),
            counters: counters_b.clone(),
            completion_pool: pool_b.clone(),
        };
        let admission = queue_a
            .admit_boxed_guaranteed(RuntimeLiveness::Ref)
            .expect("host admission from first isolate");
        assert!(
            queue_b
                .enqueue_boxed_guaranteed(
                    admission,
                    Box::new(MarkExecuted(Arc::new(AtomicBool::new(false)))),
                    otter_vm::host_completion::HostCompletionOutcome::Completed,
                )
                .is_err()
        );
        assert!(
            queue_b
                .enqueue_boxed_guaranteed(
                    otter_vm::host_completion::HostCompletionAdmission::new(Box::new(())),
                    Box::new(MarkExecuted(Arc::new(AtomicBool::new(false)))),
                    otter_vm::host_completion::HostCompletionOutcome::Completed,
                )
                .is_err()
        );
        assert_eq!(counters_a.pending_ref_host_ops.load(Ordering::Relaxed), 0);

        let loader_a = InboxDynamicImportLoader {
            inbox: inbox_a.clone(),
            counters: counters_a.clone(),
            completion_pool: pool_a.clone(),
        };
        let loader_b = InboxDynamicImportLoader {
            inbox: inbox_b.clone(),
            counters: counters_b.clone(),
            completion_pool: pool_b.clone(),
        };
        let admission = loader_a
            .admit()
            .expect("dynamic admission from first isolate");
        assert!(
            loader_b
                .schedule(admission, 1, "x".to_string(), String::new(), None)
                .is_err()
        );
        assert!(
            loader_b
                .schedule(
                    DynamicImportAdmission::new(Box::new(())),
                    2,
                    "x".to_string(),
                    String::new(),
                    None,
                )
                .is_err()
        );
        assert_eq!(counters_a.pending_ref_host_ops.load(Ordering::Relaxed), 0);

        let scheduler_a = InboxTimerScheduler {
            inbox: inbox_a,
            event_loop: event_loop.clone(),
            counters: counters_a.clone(),
            completion_pool: pool_a,
        };
        let scheduler_b = InboxTimerScheduler {
            inbox: inbox_b,
            event_loop,
            counters: counters_b.clone(),
            completion_pool: pool_b,
        };
        let admission = scheduler_a
            .admit(false)
            .expect("timer admission from first isolate");
        assert!(scheduler_b.schedule(admission, 60_000, None).is_err());
        assert!(
            scheduler_b
                .schedule(TimerAdmission::new(Box::new(())), 60_000, None)
                .is_err()
        );
        let repeat = scheduler_b.admit(true).expect("repeating timer admission");
        assert!(scheduler_b.schedule(repeat, 60_000, None).is_err());
        assert_eq!(counters_a.pending_ref_timers.load(Ordering::Relaxed), 0);
        assert_eq!(counters_b.pending_ref_timers.load(Ordering::Relaxed), 0);
        assert!(matches!(
            rx_a.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        assert!(matches!(
            rx_b.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));

        for account in [account_a, account_b] {
            let snapshot = account.snapshot();
            assert_eq!(snapshot.get(ResourceClass::QueuedTasks).current(), 0);
            assert_eq!(snapshot.get(ResourceClass::HostOperations).current(), 0);
            assert_eq!(snapshot.get(ResourceClass::Timers).current(), 0);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn guaranteed_overflow_uses_one_drain_and_preserves_fifo() {
        const POSTS: u64 = 256;
        let (inbox, mut rx, counters) = inbox(1);
        assert!(
            inbox.try_send(RuntimeMessage::Interrupt).is_ok(),
            "blocker fills the inbox"
        );
        let pool = completion_pool();

        for sequence in 0..POSTS {
            inbox.post_guaranteed(host_completion(sequence, &pool, counters.clone()));
        }
        assert_eq!(inbox.drain_spawns(), 1, "one overflow burst, one pump");
        assert!(matches!(rx.recv().await, Some(RuntimeMessage::Interrupt)));

        for expected in 0..POSTS {
            let message = tokio::time::timeout(Duration::from_secs(2), rx.recv())
                .await
                .expect("guaranteed completion wake")
                .expect("guaranteed completion");
            let RuntimeMessage::Guaranteed(GuaranteedPost {
                payload: GuaranteedPayload::DynamicImportGraphPrepared { target_url, .. },
                completion,
            }) = message
            else {
                panic!("unexpected guaranteed message");
            };
            assert_eq!(target_url, expected.to_string());
            completion.begin_dispatch().complete();
        }
        assert_eq!(counters.pending_unref_host_ops.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn aborted_dynamic_import_preparation_posts_one_admitted_cancellation() {
        let (inbox, mut rx, counters) = inbox(2);
        let pool = completion_pool();
        let completion =
            QueuedCompletion::admit_host(&pool, counters.clone(), RuntimeLiveness::Ref)
                .expect("dynamic import admission");
        let pending = PendingDynamicImportPreparation::new(
            inbox,
            17,
            "https://example.invalid/mod.js".to_string(),
            completion,
        );

        drop(pending);

        let message = rx.recv().await.expect("cancellation completion");
        assert!(matches!(
            &message,
            RuntimeMessage::Guaranteed(GuaranteedPost {
                payload: GuaranteedPayload::DynamicImportGraphPrepared {
                    token: 17,
                    result: Err(error),
                    ..
                },
                ..
            }) if error.contains("cancelled")
        ));
        cancel_runtime_message(message, &counters);
        assert_eq!(counters.pending_ref_host_ops.load(Ordering::Relaxed), 0);
        assert_eq!(counters.cancelled_host_ops.load(Ordering::Relaxed), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn aborting_guaranteed_pump_cancels_current_and_tail() {
        let (inbox, mut rx, counters) = inbox(1);
        let pool = completion_pool();
        assert!(
            inbox.try_send(RuntimeMessage::Interrupt).is_ok(),
            "blocker fills the inbox"
        );
        {
            let mut pending = inbox
                .shared
                .pending
                .lock()
                .expect("runtime inbox pending queue");
            pending.draining = true;
            for sequence in 0..2 {
                pending
                    .fifo
                    .push_back(RuntimeMessage::Guaranteed(host_completion(
                        sequence,
                        &pool,
                        counters.clone(),
                    )));
            }
        }
        let pump = tokio::spawn(inbox.clone().drain_guaranteed());
        tokio::time::timeout(Duration::from_secs(2), async {
            while inbox.drain_runs() == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("pump starts its capacity wait");
        assert!(!pump.is_finished());
        assert_eq!(
            inbox
                .shared
                .pending
                .lock()
                .expect("runtime inbox pending queue")
                .fifo
                .len(),
            2,
            "all accounted posts remain synchronously cancellable"
        );

        pump.abort();
        assert!(
            pump.await.expect_err("pump must be aborted").is_cancelled(),
            "pump cancellation should drop its delivery future"
        );
        assert_eq!(counters.pending_unref_host_ops.load(Ordering::Relaxed), 0);
        assert_eq!(counters.cancelled_host_ops.load(Ordering::Relaxed), 2);
        {
            let pending = inbox
                .shared
                .pending
                .lock()
                .expect("runtime inbox pending queue");
            assert!(!pending.draining);
            assert!(pending.fifo.is_empty());
        }
        assert!(matches!(rx.recv().await, Some(RuntimeMessage::Interrupt)));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_drain_observes_a_permitted_handoff() {
        let (inbox, mut rx, counters) = inbox(1);
        let pool = completion_pool();
        {
            let mut pending = inbox
                .shared
                .pending
                .lock()
                .expect("runtime inbox pending queue");
            pending.draining = true;
            pending
                .fifo
                .push_back(RuntimeMessage::Guaranteed(host_completion(
                    1,
                    &pool,
                    counters.clone(),
                )));
        }

        let (entered_tx, entered_rx) = oneshot::channel();
        let entered_tx = Arc::new(Mutex::new(Some(entered_tx)));
        let release = Arc::new(std::sync::Barrier::new(2));
        inbox.set_handoff_hook(Arc::new({
            let entered_tx = entered_tx.clone();
            let release = release.clone();
            move || {
                if let Some(entered_tx) = entered_tx.lock().expect("handoff entered signal").take()
                {
                    let _ = entered_tx.send(());
                }
                release.wait();
            }
        }));
        let pump = tokio::spawn(inbox.clone().drain_guaranteed());
        tokio::time::timeout(Duration::from_secs(2), entered_rx)
            .await
            .expect("pump reaches handoff hook")
            .expect("handoff hook signals");

        // Match IsolateRunner::shutdown ordering while the pump has already
        // popped its message: close, take the shared FIFO mutex, then drain.
        counters.shutdown.store(true, Ordering::Release);
        rx.close();
        let cancel = tokio::task::spawn_blocking({
            let inbox = inbox.clone();
            move || inbox.cancel_pending()
        });
        release.wait();
        cancel.await.expect("shutdown pending cancellation");
        pump.await.expect("pump exits after receiver close");

        let message = rx.try_recv().expect("permitted handoff precedes drain");
        cancel_runtime_message(message, &counters);
        assert_eq!(counters.pending_unref_host_ops.load(Ordering::Relaxed), 0);
        assert_eq!(counters.cancelled_host_ops.load(Ordering::Relaxed), 1);
        assert!(matches!(
            rx.try_recv(),
            Err(mpsc::error::TryRecvError::Disconnected) | Err(mpsc::error::TryRecvError::Empty)
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn repeat_timer_coalesces_and_cannot_bypass_waiting_one_shot() {
        let (inbox, mut rx, counters) = inbox(1);
        assert!(
            inbox.try_send(RuntimeMessage::Interrupt).is_ok(),
            "blocker fills the inbox"
        );
        let one_shot = TimerToken(11);
        let admission = completion_pool()
            .admit(CompletionOrigin::OneShotTimer)
            .expect("one-shot admission");
        increment_liveness(
            RuntimeLiveness::Ref,
            &counters.pending_ref_timers,
            &counters.pending_unref_timers,
        );
        inbox.post_guaranteed(GuaranteedPost {
            payload: GuaranteedPayload::TimerFired {
                token: one_shot,
                expects_js_callback: true,
            },
            completion: QueuedCompletion::from_one_shot_timer(
                admission,
                counters.clone(),
                RuntimeLiveness::Ref,
            ),
        });
        for token in 100..200 {
            inbox.post_coalescing(RuntimeMessage::TimerFired {
                token: TimerToken(token),
                expects_js_callback: true,
            });
        }

        assert!(matches!(rx.recv().await, Some(RuntimeMessage::Interrupt)));
        let message = rx.recv().await.expect("one-shot wake");
        assert!(matches!(
            &message,
            RuntimeMessage::Guaranteed(GuaranteedPost {
                payload: GuaranteedPayload::TimerFired { token, .. },
                ..
            }) if *token == one_shot
        ));
        assert!(matches!(
            rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        cancel_runtime_message(message, &counters);
        assert_eq!(counters.pending_ref_timers.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn zero_delay_handles_round_trip_clear_exact_target_and_fire_fifo() {
        const MAX_SAFE_INTEGER: u64 = (1u64 << 53) - 1;
        let event_loop = TokioEventLoop::current_or_owned().expect("test event loop");
        let (inbox, mut rx, counters) = inbox(8);
        let scheduler = timer_scheduler(inbox, event_loop, counters.clone());

        let first = schedule_test_timer(&scheduler, 0, None);
        let second = schedule_test_timer(&scheduler, 0, None);
        assert_ne!(first, second);
        assert!(first <= MAX_SAFE_INTEGER && second <= MAX_SAFE_INTEGER);
        assert_eq!((first as f64) as u64, first);
        assert_eq!((second as f64) as u64, second);
        assert!(scheduler.cancel((second as f64) as u64));

        let first_message = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("first zero-delay wake")
            .expect("first zero-delay message");
        assert!(matches!(
            &first_message,
            RuntimeMessage::Guaranteed(GuaranteedPost {
                payload: GuaranteedPayload::TimerFired { token, .. },
                ..
            }) if token.0 == first
        ));
        cancel_runtime_message(first_message, &counters);
        tokio::task::yield_now().await;
        assert!(matches!(
            rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));

        let third = schedule_test_timer(&scheduler, 0, None);
        let fourth = schedule_test_timer(&scheduler, 0, None);
        for expected in [third, fourth] {
            let message = tokio::time::timeout(Duration::from_secs(2), rx.recv())
                .await
                .expect("ordered zero-delay wake")
                .expect("ordered zero-delay message");
            assert!(matches!(
                &message,
                RuntimeMessage::Guaranteed(GuaranteedPost {
                    payload: GuaranteedPayload::TimerFired { token, .. },
                    ..
                }) if token.0 == expected
            ));
            cancel_runtime_message(message, &counters);
        }
        assert_eq!(counters.pending_ref_timers.load(Ordering::Relaxed), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn timer_cancel_is_scoped_to_the_owning_isolate() {
        let event_loop = TokioEventLoop::current_or_owned().expect("shared test event loop");
        let (inbox_a, _rx_a, counters_a) = inbox(2);
        let (inbox_b, _rx_b, counters_b) = inbox(2);
        let scheduler_a = timer_scheduler(inbox_a, event_loop.clone(), counters_a.clone());
        let scheduler_b = timer_scheduler(inbox_b, event_loop, counters_b.clone());
        let owned_by_b = schedule_test_timer(&scheduler_b, 60_000, None);

        assert!(!scheduler_a.cancel(owned_by_b));
        assert_eq!(counters_a.pending_ref_timers.load(Ordering::Relaxed), 0);
        assert_eq!(counters_b.pending_ref_timers.load(Ordering::Relaxed), 1);
        assert!(scheduler_b.cancel(owned_by_b));
        assert_eq!(counters_b.pending_ref_timers.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn dropping_timer_scheduler_disarms_all_owned_timers() {
        let event_loop = TokioEventLoop::current_or_owned().expect("test event loop");
        let (inbox, mut rx, counters) = inbox(4);
        let scheduler = timer_scheduler(inbox, event_loop.clone(), counters.clone());
        let one_shot = schedule_test_timer(&scheduler, 60_000, None);
        let repeat = schedule_test_timer(&scheduler, 60_000, Some(60_000));
        assert_eq!(counters.pending_ref_timers.load(Ordering::Relaxed), 2);

        drop(scheduler);

        assert_eq!(counters.pending_ref_timers.load(Ordering::Relaxed), 0);
        assert_eq!(counters.cancelled_timers.load(Ordering::Relaxed), 2);
        assert!(!event_loop.cancel_timer(TimerToken(one_shot)));
        assert!(!event_loop.cancel_timer(TimerToken(repeat)));
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(matches!(
            rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty) | Err(mpsc::error::TryRecvError::Disconnected)
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn zero_timer_capacity_rejects_without_retaining_callback_or_ledger_state() {
        let otter = crate::Otter::builder()
            .completion_capacities(1, 1, 0)
            .build()
            .expect("zero timer capacity is a valid fail-closed runtime");
        let result = otter
            .handle()
            .run_script(
                SourceInput::from_javascript(
                    r#"
                    let outcome;
                    try {
                        setInterval(() => {}, 1);
                        outcome = "unexpected-success";
                    } catch (error) {
                        outcome = error.name + ":" + /capacity/.test(error.message);
                    }
                    outcome;
                    "#,
                ),
                "<timer-admission-exhausted>",
            )
            .await
            .expect("timer admission failure is a catchable JavaScript error");

        assert_eq!(result.completion_string(), "RangeError:true");
        assert_eq!(otter.activity_stats().pending_ref_timers, 0);
        let snapshot = otter.resource_snapshot();
        assert_eq!(snapshot.get(ResourceClass::Timers).current(), 0);
        assert_eq!(snapshot.get(ResourceClass::QueuedTasks).current(), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stale_timer_wakes_do_not_release_another_timer_hold() {
        let mut runner = isolate_runner(2);
        let live = TimerToken(17);
        let active = completion_pool()
            .admit_origin_only(ResourceClass::Timers)
            .expect("live timer admission");
        runner
            .counters
            .timers
            .lock()
            .expect("timer ownership lock")
            .insert(
                live.0,
                OwnedTimer {
                    liveness: RuntimeLiveness::Ref,
                    resource: TimerResource::Repeat { _active: active },
                },
            );
        increment_liveness(
            RuntimeLiveness::Ref,
            &runner.counters.pending_ref_timers,
            &runner.counters.pending_unref_timers,
        );
        runner.exit_finalized = true;

        for expects_js_callback in [true, false] {
            let outcome = runner.process_message(RuntimeMessage::TimerFired {
                token: TimerToken(u64::MAX),
                expects_js_callback,
            });
            assert!(matches!(outcome, TickOutcome::Processed));
            assert_eq!(
                runner.counters.pending_ref_timers.load(Ordering::Relaxed),
                1
            );
            assert_eq!(
                runner.counters.timer_class(live.0),
                Some(RuntimeLiveness::Ref)
            );
        }

        cancel_runtime_message(
            RuntimeMessage::TimerFired {
                token: live,
                expects_js_callback: true,
            },
            &runner.counters,
        );
        assert_eq!(
            runner.counters.pending_ref_timers.load(Ordering::Relaxed),
            0
        );
    }

    #[tokio::test]
    async fn finalized_process_cancels_late_dynamic_import_load_and_prepared_messages() {
        let mut runner = isolate_runner(4);
        let (_, context) = runner
            .runtime
            .run_script_with_context(SourceInput::from_javascript("0"), "<late-dynamic-import>")
            .expect("establish dynamic-import context");
        let load_promise = otter_vm::promise_dispatch::pending_runtime_rooted(
            &mut runner.runtime.interp,
            &[],
            &[],
        )
        .expect("load promise");
        let load_token = runner.runtime.interp.dynamic_import_registry_mut().insert(
            load_promise,
            context.clone(),
            0,
        );
        let prepared_promise = otter_vm::promise_dispatch::pending_runtime_rooted(
            &mut runner.runtime.interp,
            &[],
            &[],
        )
        .expect("prepared promise");
        let prepared_token = runner.runtime.interp.dynamic_import_registry_mut().insert(
            prepared_promise,
            context,
            0,
        );

        let directory = tempfile::tempdir().expect("temporary module directory");
        let module_path = directory.path().join("late.mjs");
        std::fs::write(
            &module_path,
            "globalThis.lateImportEffect = true; export {};",
        )
        .expect("write late module");
        let module_url = url::Url::from_file_path(&module_path)
            .expect("module file URL")
            .to_string();
        runner.exit_finalized = true;

        let load = GuaranteedPost {
            payload: GuaranteedPayload::DynamicImportLoad {
                token: load_token,
                specifier: module_url,
                referrer: String::new(),
                attr_type: None,
            },
            completion: QueuedCompletion::admit_host(
                &runner.completion_pool,
                runner.counters.clone(),
                RuntimeLiveness::Ref,
            )
            .expect("load admission"),
        };
        let prepared = GuaranteedPost {
            payload: GuaranteedPayload::DynamicImportGraphPrepared {
                token: prepared_token,
                target_url: "file:///unused.mjs".to_string(),
                result: Err("late prepared result".to_string()),
            },
            completion: QueuedCompletion::admit_host(
                &runner.completion_pool,
                runner.counters.clone(),
                RuntimeLiveness::Ref,
            )
            .expect("prepared admission"),
        };

        assert!(matches!(
            runner.process_guaranteed(load),
            TickOutcome::Processed
        ));
        assert!(matches!(
            runner.process_guaranteed(prepared),
            TickOutcome::Processed
        ));
        assert!(runner.runtime.interp.dynamic_import_registry().is_empty());
        assert_eq!(
            runner.counters.pending_ref_host_ops.load(Ordering::Relaxed),
            0
        );
        assert_eq!(
            runner.counters.cancelled_host_ops.load(Ordering::Relaxed),
            2
        );
        let effect = runner
            .runtime
            .eval(SourceInput::from_javascript("typeof lateImportEffect"))
            .expect("inspect late module effect");
        assert_eq!(effect.completion_string(), "undefined");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn receiver_close_wakes_ordered_sender_and_cancels_guaranteed_tail() {
        let (inbox, mut rx, counters) = inbox(1);
        let pool = completion_pool();
        assert!(
            inbox.try_send(RuntimeMessage::Interrupt).is_ok(),
            "blocker fills the inbox"
        );
        let ordered = inbox.clone();
        let ordered_join = tokio::spawn(async move {
            ordered
                .send_ordered(RuntimeMessage::Diagnostic(RuntimeDiagnostic {
                    _origin: "ordered".to_string(),
                    _message: String::new(),
                }))
                .await
        });
        for sequence in 0..2 {
            inbox.post_guaranteed(host_completion(sequence, &pool, counters.clone()));
        }
        tokio::task::yield_now().await;
        counters.shutdown.store(true, Ordering::Release);
        rx.close();
        while let Ok(message) = rx.try_recv() {
            cancel_runtime_message(message, &counters);
        }

        assert!(
            tokio::time::timeout(Duration::from_secs(2), ordered_join)
                .await
                .expect("ordered close wake")
                .expect("ordered sender task")
                .is_err()
        );
        tokio::time::timeout(Duration::from_secs(2), async {
            while counters.pending_unref_host_ops.load(Ordering::Acquire) != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("guaranteed tail cancellation");
        assert_eq!(counters.cancelled_host_ops.load(Ordering::Relaxed), 2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_race_cancels_received_task_and_drains_queued_command() {
        let mut runner = isolate_runner(2);
        let counters = runner.counters.clone();
        let account = ResourceAccount::default();
        let lease = account
            .reserve_exact(ResourceClass::QueuedTasks, 1)
            .expect("queued command lease");
        let (reply, mut reply_rx) = oneshot::channel();
        counters.queued_commands.store(1, Ordering::Relaxed);
        assert!(
            runner
                .inbox
                .try_send(RuntimeMessage::Command(QueuedCommand {
                    command: RuntimeCommand::Eval {
                        id: 1,
                        source: SourceInput::from_javascript("1;"),
                        commonjs_scope: None,
                        reply,
                    },
                    lease,
                }))
                .is_ok(),
            "queued command enters the inbox before shutdown"
        );
        let executed = Arc::new(AtomicBool::new(false));
        counters.retain_host_activity(RuntimeLiveness::Ref);
        counters.shutdown.store(true, Ordering::Release);

        let outcome = runner.process_message(RuntimeMessage::RuntimeTask {
            task: Box::new(MarkExecuted(executed.clone())),
            liveness: RuntimeLiveness::Ref,
        });

        assert!(matches!(outcome, TickOutcome::Shutdown));
        assert!(runner.shutdown);
        assert!(!executed.load(Ordering::Acquire));
        assert_eq!(counters.pending_ref_host_ops.load(Ordering::Relaxed), 0);
        assert_eq!(counters.cancelled_host_ops.load(Ordering::Relaxed), 1);
        assert_eq!(counters.queued_commands.load(Ordering::Relaxed), 0);
        assert_eq!(
            account.snapshot().get(ResourceClass::QueuedTasks).current(),
            0
        );
        assert!(matches!(
            reply_rx.try_recv(),
            Err(oneshot::error::TryRecvError::Closed)
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_between_initial_poll_and_command_prepare_cancels_command() {
        let mut runner = isolate_runner(1);
        let counters = runner.counters.clone();
        let account = ResourceAccount::default();
        let lease = account
            .reserve_exact(ResourceClass::QueuedTasks, 1)
            .expect("queued command lease");
        let (reply, mut reply_rx) = oneshot::channel();
        let queued = QueuedCommand {
            command: RuntimeCommand::Eval {
                id: 1,
                source: SourceInput::from_javascript("for (;;) {}"),
                commonjs_scope: None,
                reply,
            },
            lease,
        };
        counters.queued_commands.store(1, Ordering::Relaxed);

        assert!(!counters.shutdown.load(Ordering::Acquire));
        runner.runtime.interrupt_handle().interrupt();
        counters.shutdown.store(true, Ordering::Release);
        let prepared = runner.prepare_command_dispatch(queued);

        assert!(matches!(prepared, Err(TickOutcome::Shutdown)));
        assert!(runner.shutdown);
        assert_eq!(counters.queued_commands.load(Ordering::Relaxed), 0);
        assert_eq!(
            account.snapshot().get(ResourceClass::QueuedTasks).current(),
            0
        );
        assert!(matches!(
            reply_rx.try_recv(),
            Err(oneshot::error::TryRecvError::Closed)
        ));
    }
}

#[cfg(test)]
mod shutdown_tests {
    use super::*;
    use crate::{ResourceError, ResourceLimits};

    fn queued_task_account(limit: u64) -> ResourceAccount {
        ResourceAccount::new(
            ResourceLimits::builder()
                .limit(ResourceClass::QueuedTasks, limit)
                .build(),
        )
    }

    fn config_with_account(account: ResourceAccount) -> RuntimeConfig {
        RuntimeConfig {
            resource_account: account,
            ..RuntimeConfig::default()
        }
    }

    fn queued_tasks(account: &ResourceAccount) -> crate::ResourceSnapshotEntry {
        *account.snapshot().get(ResourceClass::QueuedTasks)
    }

    fn submit_script(
        handle: &RuntimeHandle,
        source: &'static str,
        specifier: &'static str,
    ) -> oneshot::Receiver<ExecutionAttempt> {
        let (reply, reply_rx) = oneshot::channel();
        let id = handle.next_command_id();
        handle
            .submit(RuntimeCommand::RunScript {
                id,
                source: SourceInput::from_javascript(source),
                specifier: specifier.to_string(),
                reply,
            })
            .expect("submit command");
        reply_rx
    }

    #[derive(Clone)]
    struct NotifyTask(std::sync::mpsc::Sender<()>);

    impl RuntimeTask for NotifyTask {
        fn run(self: Box<Self>, _runtime: &mut Runtime) -> Result<(), OtterError> {
            let _ = self.0.send(());
            Ok(())
        }
    }

    fn wait_until_running(handle: &RuntimeHandle) {
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !handle.activity_stats().running_command {
            assert!(
                std::time::Instant::now() < deadline,
                "command never entered the isolate"
            );
            std::thread::yield_now();
        }
    }

    /// Queue a second command behind an in-flight run and execute a barrier
    /// task after the runner has moved that command into its deferred FIFO.
    fn defer_second_command(
        handle: &RuntimeHandle,
    ) -> (
        oneshot::Receiver<ExecutionAttempt>,
        oneshot::Receiver<ExecutionAttempt>,
    ) {
        let _timer = handle.schedule_timer(TimerRequest {
            delay: Duration::from_secs(60),
            repeat: None,
        });
        let first = submit_script(handle, "1;", "<queued-task-first>");
        wait_until_running(handle);
        let second = submit_script(handle, "2;", "<queued-task-deferred>");

        let (barrier_tx, barrier_rx) = std::sync::mpsc::channel();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            match handle
                .enqueue_runtime_task(NotifyTask(barrier_tx.clone()), RuntimeLiveness::Unref)
            {
                Ok(()) => break,
                Err(error) if error.is_backpressure() => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "runner never accepted the deferred-command barrier"
                    );
                    std::thread::yield_now();
                }
                Err(error) => panic!("barrier enqueue failed: {error}"),
            }
        }
        barrier_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("barrier must run after the command is deferred");
        (first, second)
    }

    #[test]
    fn zero_queued_task_limit_rejects_before_enqueue() {
        let account = queued_task_account(0);
        let handle =
            RuntimeHandle::spawn(config_with_account(account.clone())).expect("runtime handle");
        let (reply, _reply_rx) = oneshot::channel();
        let error = handle
            .submit(RuntimeCommand::RunScript {
                id: handle.next_command_id(),
                source: SourceInput::from_javascript("1;"),
                specifier: "<queued-task-zero-limit>".to_string(),
                reply,
            })
            .expect_err("zero queued-task limit must reject admission");

        assert!(matches!(
            error,
            OtterError::Resource {
                error: ResourceError::Exhausted {
                    class: ResourceClass::QueuedTasks,
                    requested: 1,
                    in_use: 0,
                    limit: 0,
                }
            }
        ));
        let entry = queued_tasks(&account);
        assert_eq!(
            (entry.current(), entry.peak(), entry.rejections()),
            (0, 0, 1)
        );
        let stats = handle.activity_stats();
        assert_eq!(stats.queued_commands, 0);
        assert_eq!(stats.submitted_commands, 0);
        assert_eq!(stats.backpressure_rejections, 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn queued_task_lease_releases_before_normal_and_error_execution() {
        let account = queued_task_account(1);
        let handle =
            RuntimeHandle::spawn(config_with_account(account.clone())).expect("runtime handle");

        let result = handle
            .run_script(SourceInput::from_javascript("1 + 1;"), "<queued-task-ok>")
            .await
            .expect("normal command");
        assert_eq!(result.completion_string(), "2");
        let entry = queued_tasks(&account);
        assert_eq!((entry.current(), entry.peak()), (0, 1));

        handle
            .run_script(
                SourceInput::from_javascript("throw new Error('expected');"),
                "<queued-task-error>",
            )
            .await
            .expect_err("throwing command");
        let entry = queued_tasks(&account);
        assert_eq!(
            (entry.current(), entry.peak(), entry.rejections()),
            (0, 1, 0)
        );

        handle.shutdown_and_wait().await;
        assert_eq!(queued_tasks(&account).current(), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn deferred_command_retains_one_lease_until_shutdown_teardown() {
        // Budget: one lease for the 60s one-shot timer admission that keeps
        // the first command draining, one for the deferred command.
        let account = queued_task_account(2);
        let handle = RuntimeHandle::spawn_with_capacity(config_with_account(account.clone()), 1)
            .expect("runtime handle");
        let (first, second) = defer_second_command(&handle);

        let entry = queued_tasks(&account);
        assert_eq!(
            (entry.current(), entry.peak(), entry.rejections()),
            (2, 2, 0)
        );
        assert!(handle.activity_stats().running_command);
        assert_eq!(handle.activity_stats().queued_commands, 1);

        handle.shutdown_and_wait().await;
        assert_eq!(queued_tasks(&account).current(), 0);
        drop(first);
        assert!(second.await.is_err(), "deferred command must be dropped");
    }

    #[test]
    fn dropping_last_handle_during_referenced_work_does_not_deadlock() {
        // One lease for the pending one-shot timer, one for the deferred
        // command.
        let account = queued_task_account(2);
        let handle = RuntimeHandle::spawn_with_capacity(config_with_account(account.clone()), 1)
            .expect("runtime handle");
        let (first, second) = defer_second_command(&handle);
        assert_eq!(queued_tasks(&account).current(), 2);
        drop(first);
        drop(second);

        let (dropped_tx, dropped_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            drop(handle);
            let _ = dropped_tx.send(());
        });
        dropped_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("last handle drop must return without blocking on the isolate");
        assert_eq!(queued_tasks(&account).current(), 0);
    }
}
