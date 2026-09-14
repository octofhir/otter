//! Host-facing configuration and diagnostics surface.
//!
//! # Contents
//! Timer scheduler and dynamic-import loader wiring, console sink,
//! microtask queue accessors, logical/native stack-depth limits,
//! machine-visible generated-call accounting, eval hook, tracer, CPU profiler,
//! IC/shape/heap snapshots, interrupt handle, and `global_this`/`set_global`,
//! plus rooted host-construction scopes.
//!
//! # Invariants
//! - Interpreter services here are isolate-owned and never discover the active
//!   execution stack through TLS or raw pointers.
//! - Generated code enforces recursion and native-stack limits through
//!   immutable bounds captured by its outer compiled entry; only host-side
//!   synchronous re-entry mutates `sync_reentry_depth`.
//! - Generated-call depth is derived from canonical native activations. Cold
//!   deoptimization records a temporary materialization transfer so the same
//!   activation is never counted twice.
//! - Live-frame diagnostics belong to [`NativeCtx`](crate::NativeCtx), whose
//!   [`RuntimeTurn`](crate::runtime_cx::RuntimeTurn) carries the explicit
//!   activation-stack borrow.
//! - Snapshot root-slot walks stay VM-private; embedders receive owned snapshot
//!   artifacts rather than raw collector slots.
//!
//! # See also
//! - [`crate::runtime_cx`] — explicit runtime-turn and native binding context.
#![allow(unused_imports)]
use crate::*;

const JIT_NATIVE_STACK_BYTES_LIMIT: usize = 512 * 1024;

impl Interpreter {
    /// Run host/runtime construction work while the complete interpreter root
    /// set is registered with its heap.
    ///
    /// Runtime builders install classes, extensions, and globals after
    /// [`Interpreter::new`] but before the dispatch loop installs its normal
    /// root provider. Any allocation in that interval may collect, so the
    /// interpreter must remain stationary and visible for the entire setup
    /// closure. Keeping the registration internal to this closure makes that
    /// address-stability contract enforceable by the mutable borrow.
    pub fn with_runtime_roots<R>(&mut self, build: impl FnOnce(&mut Self) -> R) -> R {
        let roots = otter_gc::ExtraRoots::new(&*self);
        let guard = self.gc_heap.register_extra_roots(roots);
        let result = build(self);
        drop(guard);
        result
    }

    /// Install the host-side timer scheduler. Called by the
    /// runtime layer at construction time so the JS-visible
    /// `setTimeout` / `setInterval` natives can route through the
    /// event-loop scheduler.
    pub fn set_timer_scheduler(&mut self, scheduler: timers::TimerSchedulerHandle) {
        self.timer_scheduler = Some(scheduler);
    }

    /// Clone the installed timer scheduler, if any. Native-function
    /// implementations of `setTimeout` / `clearTimeout` use this to
    /// schedule and cancel without holding `&mut self` over the
    /// host-side call.
    #[must_use]
    pub fn timer_scheduler(&self) -> Option<timers::TimerSchedulerHandle> {
        self.timer_scheduler.clone()
    }

    /// Install the host completion sink backing async native methods.
    /// Called by the runtime layer at construction time, exactly like
    /// [`Self::set_timer_scheduler`].
    pub fn set_host_completion_sink(
        &mut self,
        sink: std::sync::Arc<dyn crate::host_completion::HostCompletionSink>,
    ) {
        self.host_completion_sink = Some(sink);
    }

    /// Clone the installed host completion sink, if any. The
    /// marshalling layer's pending-promise builders route async
    /// results through it.
    #[must_use]
    pub fn host_completion_sink(
        &self,
    ) -> Option<std::sync::Arc<dyn crate::host_completion::HostCompletionSink>> {
        self.host_completion_sink.clone()
    }

    /// Install a Rust-side Promise rejection observer for this isolate.
    pub fn set_promise_rejection_hook(
        &mut self,
        hook: crate::promise_rejection::PromiseRejectionHookHandle,
    ) {
        self.promise_rejection_hook = Some(hook);
    }

    /// Clone the installed Promise rejection observer, if any.
    #[must_use]
    pub fn promise_rejection_hook(
        &self,
    ) -> Option<crate::promise_rejection::PromiseRejectionHookHandle> {
        self.promise_rejection_hook.clone()
    }

    /// Mutable handle to the timer-callback registry.
    pub fn timer_callbacks_mut(&mut self) -> &mut timers::TimerCallbacks {
        &mut self.timer_callbacks
    }

    /// Read-only view of the timer-callback registry.
    #[must_use]
    pub fn timer_callbacks(&self) -> &timers::TimerCallbacks {
        &self.timer_callbacks
    }

    /// Cancel one host deadline and forget its callback.
    ///
    /// The callback is removed first so a late host fire cannot re-enter it.
    /// Returns `true` when either side still knew the token.
    #[doc(hidden)]
    pub fn cancel_timer(&mut self, token: u64) -> bool {
        let removed = self.timer_callbacks.remove(token).is_some();
        let host_cancelled = self
            .timer_scheduler()
            .is_some_and(|scheduler| scheduler.cancel(token));
        removed || host_cancelled
    }

    /// Cancel and forget every timer owned by a disposed realm.
    #[doc(hidden)]
    pub fn cancel_timers_for_realm(&mut self, realm_id: u32) -> usize {
        let scheduler = self.timer_scheduler();
        let tokens = self.timer_callbacks.remove_realm(realm_id);
        if let Some(scheduler) = scheduler {
            for token in &tokens {
                let _ = scheduler.cancel(*token);
            }
        }
        tokens.len()
    }

    /// Cancel and forget every timer callback owned by this interpreter.
    ///
    /// Callback entries are drained before their host deadlines are cancelled,
    /// making late fires harmless during process or isolate teardown. Returns
    /// the number of callbacks detached from the interpreter.
    #[doc(hidden)]
    pub fn cancel_all_timers(&mut self) -> usize {
        let scheduler = self.timer_scheduler();
        let mut cancelled = 0usize;
        for token in self.timer_callbacks.drain_tokens() {
            cancelled += 1;
            if let Some(scheduler) = &scheduler {
                let _ = scheduler.cancel(token);
            }
        }
        cancelled
    }

    /// Insert a generic persistent root and return its id.
    pub fn persistent_root_insert(&mut self, value: Value) -> persistent_roots::PersistentRootId {
        self.persistent_roots.insert(value)
    }

    /// Read a generic persistent root.
    #[must_use]
    pub fn persistent_root_get(&self, id: persistent_roots::PersistentRootId) -> Option<Value> {
        self.persistent_roots.get(id, &self.gc_heap)
    }

    /// Insert a collector-managed weak cell into the persistent root table.
    pub(crate) fn persistent_root_insert_weak(
        &mut self,
        weak_ref: crate::JsWeakRef,
    ) -> persistent_roots::PersistentRootId {
        self.persistent_roots.insert_weak(weak_ref)
    }

    /// Remove a generic persistent root.
    pub fn persistent_root_remove(
        &mut self,
        id: persistent_roots::PersistentRootId,
    ) -> Option<Value> {
        self.persistent_roots.remove(id, &self.gc_heap)
    }

    /// Borrow persistent roots for GC tracing.
    #[must_use]
    pub(crate) fn persistent_roots_for_trace(&self) -> &persistent_roots::PersistentRoots {
        &self.persistent_roots
    }

    /// Install the host-side dynamic-import scheduler.
    pub fn set_dynamic_import_loader(&mut self, loader: dynamic_import::DynamicImportLoaderHandle) {
        self.dynamic_import_loader = Some(loader);
    }

    /// Clone the installed dynamic-import scheduler, if any.
    #[must_use]
    pub fn dynamic_import_loader(&self) -> Option<dynamic_import::DynamicImportLoaderHandle> {
        self.dynamic_import_loader.clone()
    }

    /// Read-only view of the dynamic-import registry.
    #[must_use]
    pub fn dynamic_import_registry(&self) -> &dynamic_import::DynamicImportRegistry {
        &self.dynamic_import_registry
    }

    /// Mutable handle to the dynamic-import registry.
    pub fn dynamic_import_registry_mut(&mut self) -> &mut dynamic_import::DynamicImportRegistry {
        &mut self.dynamic_import_registry
    }

    /// Scalar realm identity for a pending dynamic-import token.
    #[doc(hidden)]
    #[must_use]
    pub fn dynamic_import_realm_id(&self, token: u64) -> Option<u32> {
        self.dynamic_import_registry.realm_id(token)
    }

    /// Origin execution context of one pending dynamic import, cloned.
    #[must_use]
    pub fn dynamic_import_context(&self, token: u64) -> Option<ExecutionContext> {
        self.dynamic_import_registry.context(token)
    }

    /// Remove one pending dynamic-import registry entry without settling it.
    /// Hosts use this only when process exit or a fatal infrastructure error
    /// makes JavaScript delivery impossible; taking the entry releases the
    /// registry's GC root deterministically.
    #[doc(hidden)]
    pub fn cancel_dynamic_import(&mut self, token: u64) -> bool {
        self.dynamic_import_registry.take(token).is_some()
    }

    /// Forget pending dynamic imports owned by a disposed realm.
    #[doc(hidden)]
    pub fn remove_dynamic_imports_for_realm(&mut self, realm_id: u32) -> usize {
        self.dynamic_import_registry.remove_realm(realm_id)
    }
}

impl Interpreter {
    /// Replace the sink used by `console.*` methods.
    pub fn set_console_sink(&mut self, sink: console::ConsoleSinkHandle) {
        self.console_sink = sink;
    }

    /// Clone the sink used by `console.*` methods.
    #[must_use]
    pub fn console_sink(&self) -> console::ConsoleSinkHandle {
        self.console_sink.clone()
    }
}

impl Interpreter {
    /// Mutable handle to the isolate-local microtask queue.
    /// Host-side async callbacks must re-enter the isolate before
    /// enqueueing GC-bearing [`Microtask`] values.
    pub fn microtasks_mut(&mut self) -> &mut MicrotaskQueue {
        &mut self.microtasks
    }

    /// Read-only view of the microtask queue.
    #[must_use]
    pub fn microtasks(&self) -> &MicrotaskQueue {
        &self.microtasks
    }

    /// Override the stack-depth limit. `0` is treated as the
    /// configured default (foundation slice rejects an explicit
    /// `0` limit at the `RuntimeBuilder` boundary, so this
    /// fall-through is defensive).
    pub fn set_max_stack_depth(&mut self, depth: u32) {
        self.max_stack_depth = if depth == 0 {
            DEFAULT_MAX_STACK_DEPTH
        } else {
            depth
        };
    }

    pub(crate) fn enter_sync_reentry(&mut self) -> Result<(), VmError> {
        let limit = self.jit_sync_reentry_limit();
        if self.sync_reentry_depth >= limit {
            return Err(VmError::StackOverflow { limit });
        }
        self.sync_reentry_depth += 1;
        Ok(())
    }

    pub(crate) fn leave_sync_reentry(&mut self) {
        debug_assert!(self.sync_reentry_depth > 0);
        self.sync_reentry_depth = self.sync_reentry_depth.saturating_sub(1);
    }

    /// Address of the active realm's rooted `globalThis` compressed offset.
    ///
    /// `JsObject` is a transparent four-byte `Gc<ObjectBody>`. The collector
    /// and realm-switch machinery both rewrite this exact interpreter field,
    /// so generated sloppy-call linkage always reads the current rooted handle
    /// immediately before publishing the callee frame.
    pub fn jit_global_this_offset_addr(&self) -> *const u32 {
        const _: [(); std::mem::size_of::<u32>()] = [(); std::mem::size_of::<crate::JsObject>()];
        const _: [(); std::mem::align_of::<u32>()] = [(); std::mem::align_of::<crate::JsObject>()];
        std::ptr::from_ref(&self.global_this).cast()
    }

    /// Address of the global declarative record's monotonic name-set epoch.
    ///
    /// Generated object-record global loads read this cell immediately before
    /// their shape guard. The interpreter is exclusively borrowed and cannot
    /// move for the complete compiled activation.
    pub fn jit_global_lexical_epoch_addr(&self) -> *const u64 {
        std::ptr::addr_of!(self.global_lexical_epoch)
    }

    /// Current logical JavaScript frame depth across materialized interpreter
    /// frames and generated frames that still live only on the native stack.
    ///
    /// A stack-call cold deopt temporarily marks its still-published native
    /// frame as materialized, so this sum never counts that activation twice.
    pub(crate) fn logical_call_depth(&self, stack: &ActivationStack) -> u32 {
        u32::try_from(stack.len())
            .unwrap_or(u32::MAX)
            .saturating_add(self.jit_generated_call_depth())
    }

    /// Temporarily transfer the current generated frame into interpreter
    /// ownership while `operation` runs.
    ///
    /// Native publication remains intact for GC. Exact frame identity transfers
    /// logical-depth and diagnostic ownership around the cold operation.
    pub(crate) fn with_materialized_native_frame<T>(
        &mut self,
        native_address: usize,
        materialized_index: usize,
        operation: impl FnOnce(&mut Self) -> T,
    ) -> Result<T, VmError> {
        if self.jit_generated_call_depth() == 0 {
            return Err(VmError::InvalidOperand);
        }
        self.jit_materialized_generated_calls
            .push((native_address, materialized_index));
        let result = operation(self);
        self.jit_materialized_generated_calls.pop();
        Ok(result)
    }

    /// Maximum synchronous JavaScript re-entry depth accepted by generated
    /// calls and [`Self::enter_sync_reentry`].
    #[must_use]
    pub fn jit_sync_reentry_limit(&self) -> u32 {
        self.max_stack_depth.min(DEFAULT_MAX_SYNC_REENTRY_DEPTH)
    }

    /// Lowest native-stack address generated code may reserve beneath the
    /// outer compiled entry's stack marker.
    ///
    /// AArch64 stacks grow downward. Comparing prospective callee `sp` against
    /// this immutable address accounts for every caller linkage and compiled
    /// prologue without shared mutable byte accounting.
    #[must_use]
    pub const fn jit_native_stack_limit(&self, outer_stack_marker: usize) -> usize {
        outer_stack_marker.saturating_sub(JIT_NATIVE_STACK_BYTES_LIMIT)
    }

    /// Install the parse + compile callback used by `Op::Eval` and
    /// `Op::NewFunction`. The runtime layer hooks the otter-compiler
    /// in here at construction time. Pass `None` (the default) to
    /// disable dynamic code; both opcodes will raise SyntaxError
    /// when invoked without a hook.
    pub fn set_eval_hook(&mut self, hook: Option<EvalHook>) {
        self.eval_hook = hook;
    }

    /// Install (or clear) the per-instruction step tracer.
    ///
    /// When `Some`, every dispatched instruction routes through the
    /// observer. When `None` (the default), the dispatch loop pays a
    /// single `Option` discriminant check per instruction and never
    /// touches the tracer slot. The trace format is documented at
    /// [`crate::inspect`] and `docs/book/src/engine/step-trace.md`.
    pub fn set_tracer(&mut self, tracer: Option<Box<dyn inspect::StepTracer>>) {
        self.tracer = tracer;
    }

    /// Register a module's verbatim source text so the VM can resolve a
    /// frame's byte span to a `(line, column)` for `Error.prototype.stack`
    /// and `util.getCallSites`. The runtime module loader calls this as it
    /// loads each module fragment; replays simply rebuild the line index.
    pub fn register_module_source(
        &mut self,
        module_url: impl Into<String>,
        text: otter_resource::SharedSource,
    ) {
        self.module_sources.register(module_url, text);
    }

    /// Admit a host- or VM-synthesized source against the registry's
    /// account and register it.
    ///
    /// # Errors
    /// Returns the admission failure without retaining anything.
    pub fn register_module_source_owned(
        &mut self,
        module_url: impl Into<String>,
        text: String,
    ) -> Result<(), otter_resource::SharedSourceError> {
        self.module_sources.register_owned(module_url, text)
    }

    /// Install the shared ledger charged for VM-retained allocations:
    /// synthesized sources, linked code-space chunks, installed generated
    /// code, and the heap's outstanding external backing-store bytes.
    /// Chunks linked after installation charge this account; earlier
    /// bootstrap chunks keep their construction-time charge.
    ///
    /// # Errors
    /// Returns the ledger refusal when the account cannot admit the heap's
    /// already-outstanding `ExternalBytes`; the previous account remains
    /// installed everywhere.
    pub fn set_resource_account(
        &mut self,
        account: otter_resource::ResourceAccount,
    ) -> Result<(), otter_resource::ResourceError> {
        self.gc_heap.set_external_bytes_account(&account)?;
        self.module_sources.set_account(account.clone());
        self.jit_code_registry.set_account(account);
        Ok(())
    }

    /// The ledger charged for VM-synthesized retained sources.
    #[must_use]
    pub fn source_account(&self) -> otter_resource::ResourceAccount {
        self.module_sources.account().clone()
    }

    /// Resolve a `(module_url, byte_offset)` to a 1-based `(line, column)`
    /// position when the module's source has been registered.
    pub(crate) fn source_line_col(&self, module_url: &str, byte_offset: u32) -> Option<(u32, u32)> {
        self.module_sources.line_col(module_url, byte_offset)
    }

    pub(crate) fn source_line_text(&self, module_url: &str, line_number: u32) -> Option<&str> {
        self.module_sources.line_text(module_url, line_number)
    }

    /// Read the live `Error.stackTraceLimit` and translate it to a frame
    /// cap, matching V8's coercion: a finite `>= 1` number caps the
    /// count, `+Infinity` keeps every frame, a missing property falls
    /// back to the default 10, and anything else (`<= 0`, `NaN`, a
    /// non-number) disables capture.
    pub(crate) fn current_stack_trace_limit(&self) -> usize {
        let ctor = self
            .error_classes
            .constructor(error_classes::ErrorKind::Error);
        match crate::object::get(ctor, &self.gc_heap, "stackTraceLimit") {
            None => error_classes::DEFAULT_STACK_TRACE_LIMIT,
            Some(v) => match v.as_f64() {
                Some(n) if n.is_infinite() && n > 0.0 => usize::MAX,
                Some(n) if n.is_finite() && n >= 1.0 => n as usize,
                _ => 0,
            },
        }
    }

    /// Enable the VM stack profiler, sampling every `interval` bytecode ticks.
    pub fn enable_cpu_profiler(&mut self, interval: u64) {
        self.cpu_profiler = Some(cpu_profile::CpuProfiler::new(interval));
    }

    /// Disable the VM stack profiler without returning its samples.
    pub fn disable_cpu_profiler(&mut self) {
        self.cpu_profiler = None;
    }

    /// Take the samples collected so far, leaving the profiler installed.
    #[must_use]
    pub fn drain_cpu_profile(&mut self) -> Option<CpuProfile> {
        self.cpu_profiler
            .as_mut()
            .map(cpu_profile::CpuProfiler::drain)
    }

    /// Take and clear the current CPU profile, if profiling was enabled.
    #[must_use]
    pub fn take_cpu_profile(&mut self) -> Option<CpuProfile> {
        self.cpu_profiler
            .take()
            .map(cpu_profile::CpuProfiler::finish)
    }

    /// Whether a step tracer is installed.
    #[must_use]
    pub fn has_tracer(&self) -> bool {
        self.tracer.is_some()
    }

    /// Install (or clear) the shape-transition observer. The
    /// observer fires on every hidden-class transition the VM
    /// takes — both fresh allocations and cached lookups. See
    /// [`inspect::ShapeTransitionEvent`].
    pub fn set_shape_transition_observer(
        &mut self,
        observer: Option<Box<dyn inspect::ShapeTransitionObserver>>,
    ) {
        self.shape_runtime.set_observer(observer);
    }

    /// Snapshot every property inline-cache site in dense site-id
    /// order. The snapshot is built without disturbing the live IC
    /// state and can be called from anywhere with a `&self`
    /// borrow.
    #[must_use]
    pub fn ic_snapshot(&self) -> Vec<inspect::IcSiteSnapshot> {
        self.code_space.property_ic_snapshots()
    }

    /// Snapshot the active hidden-class transition tree. Nodes
    /// appear in deterministic order: root first, then transitions
    /// sorted by `(parent_shape_id, transition_key)`.
    #[must_use]
    pub fn shape_transition_snapshot(&self) -> inspect::ShapeTransitionSnapshot {
        inspect::build_shape_transition_snapshot(&self.shape_runtime, &self.gc_heap)
    }

    /// Type-count summary of every live GC body. Walks the heap
    /// without holding allocator paths open — safe to call from
    /// any mutator-turn boundary.
    #[must_use]
    pub fn heap_snapshot_summary(&self) -> inspect::HeapSnapshotSummary {
        let census = self.gc_heap.census();
        inspect::HeapSnapshotSummary::from_census(&census)
    }

    /// Per-space, per-type-tag census of the live heap. Cheaper than
    /// [`Self::heap_snapshot_summary`] — a linear page walk with no
    /// edge graph — and it separates old space from the nursery, which
    /// lets the opaque in-process snapshot path size its retained image.
    #[must_use]
    pub fn heap_census(&self) -> otter_gc::HeapCensus {
        self.gc_heap.census()
    }

    /// Census of every live native callable by dispatch storage. See
    /// [`crate::native_census`].
    #[must_use]
    pub fn native_census(&self) -> crate::native_census::NativeCensus {
        crate::native_census::native_census(&self.gc_heap)
    }

    /// Which live bodies still hold GC references outside their own
    /// cell, and so cannot survive a page image. See
    /// [`otter_gc::SelfContainmentAudit`].
    #[must_use]
    pub fn self_containment_audit(&mut self) -> otter_gc::SelfContainmentAudit {
        self.gc_heap.audit_self_containment()
    }

    /// Capture this isolate's old generation as an opaque relocatable image.
    ///
    /// # Errors
    /// Propagates [`otter_gc::ImageError`]; a build leaves the nursery
    /// empty, so a capture taken right after one succeeds.
    pub(crate) fn capture_heap_image(&self) -> Result<otter_gc::HeapImage, otter_gc::ImageError> {
        self.flush_constructor_observations(&self.gc_heap);
        self.gc_heap.capture_old_space()
    }

    /// Fixed-shape walk over the root slots a snapshot has to carry.
    ///
    /// Unlike the collector's root walk this visits every slot whether or
    /// not it is filled, so the sequence has the same length and order on
    /// a built isolate and on an empty one. That is what lets a capture
    /// collect the slots into a list and a restore write them back.
    pub(crate) fn visit_snapshot_roots(&self, visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)) {
        let global =
            &self.global_this as *const crate::object::JsObject as *mut otter_gc::raw::RawGc;
        visitor(global);
        self.realm_intrinsics.visit_slots(visitor);
        // The well-known symbol set is fixed by the spec, so its entry
        // order is identical on every isolate. Only the body handle is
        // walked — one slot per symbol, state-independent; the wrapper's
        // description cache is re-read from the restored body.
        for symbol in self.well_known_symbols.entries() {
            symbol.visit_handle_slot(visitor);
        }
        // The error class registry's sixteen prototype/constructor
        // handles are realm-authoritative, fixed in count and order.
        self.error_classes.trace_gc_roots(visitor);
        // The shape runtime's side tables are caches a restore rebuilds
        // from the heap; the root shape is the one authoritative handle.
        self.shape_runtime.visit_root_slot(visitor);
    }

    /// Shared handle to this isolate's code space, for the in-process
    /// snapshot.
    #[must_use]
    pub(crate) fn snapshot_code_space(&self) -> std::sync::Arc<crate::code_space::CodeSpace> {
        self.code_space.clone()
    }

    /// The property-name atom table, in id order. Restoring the list
    /// through the interner re-mints identical atom ids.
    #[must_use]
    pub fn snapshot_atom_names(&self) -> Vec<Box<str>> {
        self.names.snapshot_names()
    }

    /// Global lexical bindings: name, cell, `is_const`. Unordered; the
    /// snapshot layer sorts.
    #[must_use]
    pub fn snapshot_global_lexicals(&self) -> Vec<(Box<str>, crate::UpvalueCell, bool)> {
        self.global_lexicals
            .iter()
            .map(|(name, (cell, is_const))| (name.clone(), *cell, *is_const))
            .collect()
    }

    /// Collect the snapshot root slots in walk order.
    #[must_use]
    pub(crate) fn capture_snapshot_roots(&self) -> Vec<otter_gc::raw::RawGc> {
        let mut out = Vec::new();
        // SAFETY: every slot address the walk yields is a live field of
        // this interpreter, read while nothing else holds it.
        self.visit_snapshot_roots(&mut |slot| out.push(unsafe { *slot }));
        out
    }

    /// Write a Chrome DevTools `.heapsnapshot` JSON document for the
    /// current heap state. The output matches the format documented
    /// at
    /// <https://developer.chrome.com/docs/devtools/memory-problems/heap-snapshots>
    /// and can be loaded straight into the DevTools "Memory" panel.
    ///
    /// # Errors
    /// Propagates I/O errors from `writer`.
    pub fn write_chrome_heap_snapshot<W: std::io::Write>(
        &self,
        writer: &mut W,
    ) -> std::io::Result<()> {
        otter_gc::devtools_snapshot::write_heap_snapshot(&self.gc_heap, &mut *writer)?;
        writer.write_all(b"\n")?;
        Ok(())
    }

    /// Cloneable handle for cooperative cancellation.
    #[must_use]
    pub fn interrupt_handle(&self) -> InterruptFlag {
        self.interrupt.clone()
    }

    /// Cloneable lifecycle handle for this isolate's `Atomics.wait` agent.
    ///
    /// Unlike [`Self::interrupt_handle`], cancellation through this handle is
    /// permanent for the interpreter lifetime and wakes only waits registered
    /// by this isolate.
    #[must_use]
    pub fn atomics_wait_agent_handle(&self) -> atomics_wait::WaitAgentHandle {
        self.atomics_wait_agent.handle()
    }

    /// Configure whether this isolate may block in `Atomics.wait`.
    ///
    /// Main/direct runtimes keep this disabled so an infinite wait cannot
    /// stall the host thread. Worker runtimes enable it because their owning
    /// host can interrupt and terminate the isolate thread.
    pub fn set_allow_blocking_atomics_wait(&mut self, allow: bool) {
        self.allow_blocking_atomics_wait = allow;
    }

    /// Whether this isolate may block in `Atomics.wait`.
    #[must_use]
    pub fn allow_blocking_atomics_wait(&self) -> bool {
        self.allow_blocking_atomics_wait
    }

    /// Clone-out the error-class registry. Used by native closures
    /// (e.g. `Promise.any`) that need to build error instances from
    /// a deferred microtask.
    #[must_use]
    pub fn error_classes_clone(&self) -> ErrorClassRegistry {
        self.error_classes.clone()
    }

    /// Borrow the shared `globalThis` object. Used by the GC
    /// root walker (task 75) and by any embedder reading the
    /// foundation seed identity (`globalThis.globalThis ===
    /// globalThis`).
    #[must_use]
    pub fn global_this(&self) -> &JsObject {
        &self.global_this
    }

    /// Install `value` as the `name` property on `globalThis` with
    /// the standard `{ writable: true, enumerable: false,
    /// configurable: true }` data-descriptor attributes used by
    /// every default-global binding (§17 + §19). Public entry for
    /// embedders that need to inject a runtime-side value into
    /// scripts (e.g. host-bound promises, capability tokens).
    pub fn set_global(&mut self, name: &str, value: Value) {
        let descriptor = crate::object::PropertyDescriptor::data(value, true, false, true);
        let _ = crate::object::define_own_property(
            self.global_this,
            &mut self.gc_heap,
            name,
            descriptor,
        );
    }
}
