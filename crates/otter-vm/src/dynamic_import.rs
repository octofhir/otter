//! Per-isolate dynamic-import bridge owned by [`crate::Interpreter`].
//!
//! ECMA-262 §16.2.1.7 ImportCall is the spec entry point for
//! `import(expr)`. When the linker has not pre-resolved the
//! specifier, the runtime needs to load + compile + link +
//! evaluate a brand-new module before the awaiting frame can
//! resume. That host work cannot run inline inside the dispatch
//! loop (the interpreter is busy executing the current frame), so
//! `Op::ImportNamespaceDynamic` registers a fresh pending
//! [`crate::JsPromiseHandle`] in this module's [`DynamicImportRegistry`]
//! and hands the host-issued token to the runtime-layer scheduler
//! ([`DynamicImportLoader`]).
//!
//! The runtime layer's scheduler implementation posts an inbox
//! message to the isolate runner. On the next runner tick, the
//! runner re-enters the runtime, synchronously loads the module,
//! and settles the registered promise — at which point the
//! awaiting frame's microtask reaction fires.
//!
//! # Contents
//!
//! - [`DynamicImportLoader`] — host scheduler trait.
//! - [`DynamicImportRegistry`] — per-isolate `u64 → pending entry`
//!   map for dynamic-import promises, origin contexts, and realm ids.
//!
//! # Invariants
//!
//! - Tokens are host-issued and monotonic; the VM only stores
//!   handles under tokens the host hands it.
//! - The registry only holds *pending* promises. Settlement
//!   consumes the entry, so a late or duplicate
//!   [`Interpreter::settle_dynamic_import`] call is a silent
//!   no-op rather than a spec-illegal double-settlement.
//! - Cross-thread payloads on this trait are `Send + 'static`
//!   (strings + a `u64` token); no [`crate::Value`] or
//!   [`crate::JsPromiseHandle`] crosses the boundary.
//! - Settlement re-enters the scalar origin realm. Disposing that realm
//!   removes its pending entries, so a late host result is a no-op.
//!
//! # See also
//!
//! - <https://tc39.es/ecma262/#sec-import-call-runtime-semantics-evaluation>
//! - [`crate::microtask`] — reaction-microtask queue the
//!   settlement enqueues onto.

use std::collections::HashMap;
use std::sync::Arc;

use otter_gc::raw::RawGc;

use crate::JsPromiseHandle;
use crate::execution_context::ExecutionContext;

/// Host-side scheduler the runtime layer plugs in. Lives behind
/// an [`Arc<dyn DynamicImportLoader>`] on [`crate::Interpreter`].
pub trait DynamicImportLoader: Send + Sync {
    /// Schedule an on-demand module load. The VM has already
    /// registered a pending promise under `token`; the host posts
    /// a runtime inbox message that, on its next tick, drives the
    /// `load + compile + link + evaluate` for `specifier` (relative
    /// to `referrer`) and settles the promise through
    /// [`crate::Interpreter::settle_dynamic_import`].
    ///
    /// `referrer` is empty for entry-script callers; otherwise it
    /// is the canonical URL of the module that ran the
    /// `import(expr)` call.
    fn schedule(&self, token: u64, specifier: String, referrer: String);
}

/// Cloneable handle the VM uses to talk to the host scheduler.
pub type DynamicImportLoaderHandle = Arc<dyn DynamicImportLoader>;

/// Pending dynamic-import state keyed by host-issued token.
#[derive(Debug, Clone)]
pub struct DynamicImportEntry {
    /// Promise handle returned by `import(expr)`.
    pub promise: JsPromiseHandle,
    /// Execution context that ran the import call. The runtime
    /// drains reactions against this context when the host load
    /// settles.
    pub context: ExecutionContext,
    /// Scalar id of the realm that executed `import()`.
    pub realm_id: u32,
}

/// Per-interpreter map keyed by host-issued token.
#[derive(Debug, Default)]
pub struct DynamicImportRegistry {
    entries: HashMap<u64, DynamicImportEntry>,
    next_token: u64,
}

impl DynamicImportRegistry {
    /// Empty registry. The first token issued is `1`; `0` is
    /// reserved as a sentinel for "no registration".
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
            next_token: 1,
        }
    }

    /// Register a fresh pending promise and return its token.
    /// Token allocation is monotonic — reuse is impossible inside
    /// one interpreter's lifetime.
    pub fn insert(
        &mut self,
        handle: JsPromiseHandle,
        context: ExecutionContext,
        realm_id: u32,
    ) -> u64 {
        let token = self.next_token;
        self.next_token = self
            .next_token
            .checked_add(1)
            .expect("dynamic-import token overflow within one interpreter lifetime");
        self.entries.insert(
            token,
            DynamicImportEntry {
                promise: handle,
                context,
                realm_id,
            },
        );
        token
    }

    /// Pop the entry matching `token` (consumes — settlement is
    /// one-shot per §27.2.1.{4,7}).
    pub fn take(&mut self, token: u64) -> Option<DynamicImportEntry> {
        self.entries.remove(&token)
    }

    /// Realm that owns a pending import token.
    #[must_use]
    pub fn realm_id(&self, token: u64) -> Option<u32> {
        self.entries.get(&token).map(|entry| entry.realm_id)
    }

    /// Drop pending imports owned by a disposed realm.
    pub fn remove_realm(&mut self, realm_id: u32) -> usize {
        let before = self.entries.len();
        self.entries.retain(|_, entry| entry.realm_id != realm_id);
        before - self.entries.len()
    }

    /// Number of pending dynamic-import promises — diagnostic only.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// `true` when no entries are registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Trace every entry's GC slot. Called from
    /// [`crate::runtime_state::RuntimeState::trace_roots`] so the
    /// pending promise survives any GC between scheduling and
    /// settlement.
    pub(crate) fn trace_gc_slots(&self, visitor: &mut dyn FnMut(*mut RawGc)) {
        for entry in self.entries.values() {
            // JsPromiseHandle exposes its body slot via Value::trace.
            let value = crate::Value::promise(entry.promise);
            value.trace_value_slots(visitor);
        }
    }
}
