//! Explicit runtime context per ADR-0005 §3 / task 76A.
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
//! Pre-task-76A the GC heap was reachable through a thread-default
//! escape hatch on `GcHeap`. That helper could not prove which
//! isolate owns a handle once Tokio worker migration enters the
//! picture, and it hid borrow boundaries from the type system. Per
//! ADR-0005, every read / write / write-barrier path must know
//! which isolate owns the object. The explicit-context types are
//! the type-level expression of that rule.
//!
//! # Status (task 77C)
//!
//! The thread-default escape hatch on `GcHeap` was removed in
//! task 77C; every caller now threads `&GcHeap` / `&mut GcHeap` (or
//! `&NativeCtx<'_>` / `&mut NativeCtx<'_>` for native bindings)
//! explicitly. Tasks 78-83 widen the migrated surface to
//! `JsArray` / `JsMap` / promise / iterator / generator and the
//! native-binding entry points.
//!
//! # Spec
//!
//! - <https://tc39.es/ecma262/#sec-agents> (one mutator per agent).
//! - [`docs/new-engine/adr/0005-async-runtime-binding.md`] §3.
//! - [`docs/new-engine/tasks/76a-runtime-binding-explicit-context.md`].
//! - [`docs/new-engine/gc-architecture.md`] §6.2 / §6.3.

use std::marker::PhantomData;

use crate::Interpreter;

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
/// `crates-next/otter-vm/tests/compile_fail/`).
///
/// # Construction
///
/// `RuntimeCx` is `pub(crate)` — only the dispatch loop and a
/// small set of internal helpers may build one. Native bindings
/// receive [`NativeCtx<'rt>`] (a public view) instead.
///
/// # Migration
///
/// Tasks 77-83 progressively replace `JsObject::get(&self, key)`
/// etc. with `JsObject::get(&self, cx: &RuntimeCx<'_>, key)` so
/// every property access threads the heap. The constructor here
/// is the single source of truth for those callers.
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
/// Native bindings under `crates/otter-modules` and the new-engine
/// `crates-next/*` adopt this in tasks 82-83. Until then, native
/// helpers continue to take ad-hoc parameters; the type lands
/// alongside the migration.
pub struct NativeCtx<'rt> {
    pub(crate) cx: RuntimeCx<'rt>,
}

impl<'rt> NativeCtx<'rt> {
    /// Build a native context from an interpreter borrow.
    #[must_use]
    #[allow(dead_code)] // wired in by tasks 82-83 native-binding migration
    pub(crate) fn new(interp: &'rt mut Interpreter) -> Self {
        Self {
            cx: RuntimeCx::new(interp),
        }
    }

    /// Borrow the GC heap immutably.
    #[must_use]
    pub fn heap(&self) -> &otter_gc::GcHeap {
        self.cx.heap()
    }

    /// Borrow the GC heap mutably.
    #[must_use]
    pub fn heap_mut(&mut self) -> &mut otter_gc::GcHeap {
        self.cx.heap_mut()
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
