//! Explicit runtime context for VM dispatch and native bindings.
//!
//! [`RuntimeCx<'rt>`] is the internal context handed to VM dispatch
//! and built-in helpers; it bundles the borrow set every algorithm
//! needs (`&mut RuntimeState`, `&mut GcHeap`, intrinsics) so callers
//! never reach for thread-local heap lookup. [`NativeCtx<'rt>`] is
//! the public-to-native binding view used by `#[dive]` /
//! `#[js_namespace]` style entry points.
//!
//! Both types are `!Send + !Sync` (enforced by static assertions in
//! [`crate::lib`]) and never cross `.await` — the lifetime parameter
//! `'rt` is what the borrow checker uses to keep the context tied
//! to a single mutator turn.
//!
//! # Why explicit context?
//!
//! The GC heap used to be reachable through a thread-default escape hatch on
//! `GcHeap`. That helper could not prove which isolate owns a handle once
//! Tokio worker migration enters the picture, and it hid borrow boundaries from
//! the type system. Every read / write / write-barrier path must know which
//! isolate owns the object. The explicit-context types are the type-level
//! expression of that rule.
//!
//! # Status
//!
//! The thread-default escape hatch on `GcHeap` was removed; every caller now
//! threads `&GcHeap` / `&mut GcHeap` (or
//! `&NativeCtx<'_>` / `&mut NativeCtx<'_>` for native bindings)
//! explicitly.
//!
//! # Spec
//!
//! - <https://tc39.es/ecma262/#sec-agents> (one mutator per agent).
//! - [Event loop](../../../docs/book/src/engine/event-loop.md).
//! - [GC API](../../../docs/book/src/engine/gc-api.md).

use std::marker::PhantomData;

use otter_bytecode::BytecodeModule;

use crate::{Interpreter, Value};

/// Internal VM context. Carried explicitly through the dispatch
/// loop and built-in helper signatures so every algorithm sees the
/// `&mut GcHeap` it allocates against and the `&mut Interpreter`
/// it reads / mutates.
///
/// # Lifetime contract
///
/// `'rt` is the lifetime of the enclosing mutator turn — the
/// dispatch loop's `&mut self` borrow. The borrow checker prevents
/// `RuntimeCx` from crossing `.await`, escaping into a
/// `'static + Send` future, or being captured by `tokio::spawn`
/// (see compile-fail tests under
/// `crates/otter-vm/tests/compile_fail/`).
///
/// # Construction
///
/// `RuntimeCx` is `pub(crate)` — only the dispatch loop and a
/// small set of internal helpers may build one. Native bindings
/// receive [`NativeCtx<'rt>`] (a public view) instead.
///
pub(crate) struct RuntimeCx<'rt> {
    /// The interpreter owns the GC heap and every other isolate
    /// resource (string heap, microtask queue, intrinsic
    /// registries). One isolate has one mutator (ECMA-262 §16.6),
    /// so `&mut Interpreter` is the right shape for the context.
    pub(crate) interp: &'rt mut Interpreter,
    /// PhantomData carries the `'rt` lifetime so callers cannot
    /// store the context past the mutator turn even if `interp`
    /// is later split out.
    _marker: PhantomData<&'rt mut ()>,
}

impl<'rt> RuntimeCx<'rt> {
    /// Build a fresh context from an interpreter borrow.
    ///
    /// `pub(crate)` — only the dispatch loop / internal helpers
    /// build a [`RuntimeCx`].
    #[must_use]
    #[allow(dead_code)] // wired in by tasks 77-83 caller migration
    pub(crate) fn new(interp: &'rt mut Interpreter) -> Self {
        Self {
            interp,
            _marker: PhantomData,
        }
    }

    /// Borrow the GC heap immutably.
    #[must_use]
    pub(crate) fn heap(&self) -> &otter_gc::GcHeap {
        self.interp.gc_heap_for_cx()
    }

    /// Borrow the GC heap mutably.
    #[must_use]
    pub(crate) fn heap_mut(&mut self) -> &mut otter_gc::GcHeap {
        self.interp.gc_heap_for_cx_mut()
    }
}

/// Call-site metadata for native bindings.
///
/// The values are snapshots for the active call only. Native code may inspect
/// them synchronously, but must not store them or move them into async work.
#[derive(Debug, Clone)]
pub struct NativeCallInfo {
    this_value: Value,
    new_target: Option<Value>,
}

impl NativeCallInfo {
    /// Ordinary function/method call metadata.
    #[must_use]
    pub fn call(this_value: Value) -> Self {
        Self {
            this_value,
            new_target: None,
        }
    }

    /// Constructor call metadata.
    #[must_use]
    pub fn construct(this_value: Value, new_target: Option<Value>) -> Self {
        Self {
            this_value,
            new_target,
        }
    }

    /// Foundation default for legacy callers.
    #[must_use]
    pub fn default_call() -> Self {
        Self::call(Value::Undefined)
    }
}

/// Public-to-native binding context. Handed to `#[dive]` /
/// `#[js_namespace]` entry points so native code allocates and
/// mutates against the right isolate without reaching for
/// thread-local state.
///
/// `NativeCtx<'rt>` is `!Send + !Sync` and never crosses `.await`.
/// The lifetime `'rt` is the mutator turn — the same constraint
/// that applies to [`RuntimeCx<'rt>`].
///
/// # Migration
///
/// Native bindings under `crates/otter-modules` and active product
/// crates use this instead of ad-hoc runtime handles.
pub struct NativeCtx<'rt> {
    pub(crate) cx: RuntimeCx<'rt>,
    call_info: NativeCallInfo,
    module: Option<&'rt BytecodeModule>,
}

impl<'rt> NativeCtx<'rt> {
    /// Build a native context from an interpreter borrow.
    #[must_use]
    #[allow(dead_code)] // wired in by tasks 82-83 native-binding migration
    pub(crate) fn new(interp: &'rt mut Interpreter) -> Self {
        Self::new_with_call_info(interp, NativeCallInfo::default_call())
    }

    /// Build a native context with explicit call-site metadata.
    #[must_use]
    pub(crate) fn new_with_call_info(
        interp: &'rt mut Interpreter,
        call_info: NativeCallInfo,
    ) -> Self {
        Self::new_with_call_info_and_module(interp, call_info, None)
    }

    /// Build a native context with explicit call-site metadata and
    /// current bytecode module. Builtins that need to re-enter JS
    /// observable algorithms (for example Proxy traps) use the
    /// module to invoke callbacks with the same function table as
    /// the caller.
    #[must_use]
    pub(crate) fn new_with_call_info_and_module(
        interp: &'rt mut Interpreter,
        call_info: NativeCallInfo,
        module: Option<&'rt BytecodeModule>,
    ) -> Self {
        Self {
            cx: RuntimeCx::new(interp),
            call_info,
            module,
        }
    }

    /// Current bytecode module for native builtins that re-enter
    /// JS callbacks.
    #[must_use]
    pub(crate) fn current_module(&self) -> Option<&'rt BytecodeModule> {
        self.module
    }

    /// Borrow the owning interpreter together with the current
    /// bytecode module. Use this when a native needs to re-enter VM
    /// code that also needs the caller module for observable coercions.
    pub(crate) fn interp_mut_and_current_module(
        &mut self,
    ) -> (&mut Interpreter, Option<&'rt BytecodeModule>) {
        (self.cx.interp, self.module)
    }

    /// Return the JavaScript receiver for the active native call.
    #[must_use]
    pub fn this_value(&self) -> &Value {
        &self.call_info.this_value
    }

    /// Return `new.target` for constructor calls.
    #[must_use]
    pub fn new_target(&self) -> Option<&Value> {
        self.call_info.new_target.as_ref()
    }

    /// Whether this native call is executing as a constructor.
    #[must_use]
    pub fn is_construct_call(&self) -> bool {
        self.call_info.new_target.is_some()
    }

    /// Borrow the GC heap immutably.
    #[must_use]
    pub fn heap(&self) -> &otter_gc::GcHeap {
        self.cx.heap()
    }

    /// Borrow the GC heap mutably.
    #[must_use]
    pub(crate) fn heap_mut(&mut self) -> &mut otter_gc::GcHeap {
        self.cx.heap_mut()
    }

    /// Allocate a GC payload through the owning isolate.
    ///
    /// This is the safe allocation path for native/builtin authors:
    /// allocation stays tied to the active [`NativeCtx`] instead of a
    /// thread-local heap lookup or a raw `GcHeap` borrow.
    pub fn alloc<T: otter_gc::Traceable>(
        &mut self,
        value: T,
    ) -> Result<otter_gc::Gc<T>, otter_gc::OutOfMemory> {
        self.heap_mut().alloc(value)
    }

    /// Allocate a long-lived GC payload directly in old-space.
    ///
    /// This mirrors the VM's current migration constraints for
    /// handles stored in non-moving Rust containers.
    pub fn alloc_old<T: otter_gc::Traceable>(
        &mut self,
        value: T,
    ) -> Result<otter_gc::Gc<T>, otter_gc::OutOfMemory> {
        self.heap_mut().alloc_old(value)
    }

    /// Record a GC-bearing value store into `parent`.
    pub fn record_write<T: ?Sized, V: otter_gc::GcStore + ?Sized>(
        &mut self,
        parent: otter_gc::Gc<T>,
        value: &V,
    ) {
        self.heap_mut().record_write(parent, value);
    }

    /// Reserve native/off-object memory with RAII release.
    pub fn reserve_external(
        &mut self,
        bytes: u64,
    ) -> Result<otter_gc::ExternalMemory, otter_gc::OutOfMemory> {
        self.heap_mut().reserve_external(bytes)
    }

    /// Enter a branded GC session for root/weak operations.
    ///
    /// Persistent roots and weak handles created inside the closure
    /// carry the fresh isolate brand and can only be read/upgraded
    /// through a matching session.
    pub fn with_gc_session<R>(
        &mut self,
        f: impl for<'iso> FnOnce(otter_gc::GcSession<'iso, '_>) -> R,
    ) -> R {
        otter_gc::with_gc_session(self.heap_mut(), f)
    }

    /// Borrow the owning interpreter for native functions that need
    /// isolate services outside the heap (microtasks, string tables,
    /// intrinsic registries).
    #[must_use]
    pub fn interp_mut(&mut self) -> &mut Interpreter {
        self.cx.interp
    }
}

// `RuntimeCx` and `NativeCtx` are `!Send + !Sync` because they
// hold a `&mut Interpreter` (which is `!Send + !Sync` by virtue
// of holding a `GcHeap`, and reinforced by the static_assertions
// in `lib.rs`).
