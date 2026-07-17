//! Cross-thread JavaScript-promise settlement registry.
//!
//! Connects host-side async work to a collector-rooted Promise without
//! crossing the VM/JavaScript thread boundary with VM state: the host owns
//! only the [`PromiseId`] token, while the actual promise stays in the
//! isolate's persistent-root table. When the host posts
//! [`crate::handle::RuntimeMessage::SettlePromise`], the runner
//! resolves the entry through the standard promise dispatch path
//! so reactions land on the per-isolate microtask queue.
//!
//! # Contents
//!
//! - [`PromiseId`] — opaque, monotonic token, `Send + Sync`.
//! - [`PromiseRegistry`] — token → persistent-root map owned by
//!   [`crate::Runtime`].
//! - [`HostSettleOutcome`] — owned host payload that crosses the
//!   inbox hop (`Send + 'static`); converted to a JS [`Value`] on
//!   the runner side.
//!
//! # Invariants
//!
//! - Tokens are `u64` monotonic; reuse is impossible.
//! - The registry only stores pending promises. A settled entry
//!   is removed before the resolve / reject closure runs so a
//!   redundant `SettlePromise` posted by the host (lost the race)
//!   becomes a no-op rather than running spec-illegal double
//!   settlement.
//! - Registry entries are opaque [`otter_vm::PersistentRootId`]s. The moving
//!   collector rewrites the corresponding promise values in the
//!   interpreter-owned root table.
//! - The host payload type is `Send + 'static` and never carries a
//!   GC handle. Conversion to `Value` happens in
//!   [`crate::Runtime::settle_pending_promise`] on the runner
//!   thread.
//!
//! # See also
//!
//! - [Promise §27.2](https://tc39.es/ecma262/#sec-promise-objects)
//! - [Microtask queue](otter_vm::microtask)

use std::collections::HashMap;

use otter_vm::PersistentRootId;

/// Opaque per-runtime promise token. `Send + Sync + Copy` so the
/// embedder may safely move it onto a Tokio worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PromiseId(pub u64);

/// Owned host payload that crosses the runtime inbox hop. Kept
/// intentionally small: any complex JS shape an embedder needs to
/// hand back must be reconstructed inside a runner-side native
/// function before the matching `register_pending_promise` call
/// returns. Richer payloads (Map, Array, ArrayBuffer transfer) are
/// layered on top of structured clone.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum HostSettleOutcome {
    /// Resolve the promise with `undefined`.
    ResolveUndefined,
    /// Resolve the promise with `null`.
    ResolveNull,
    /// Resolve the promise with a boolean.
    ResolveBoolean(bool),
    /// Resolve the promise with an IEEE-754 double.
    ResolveNumber(f64),
    /// Resolve the promise with a string. Allocated on the runner
    /// thread inside `Runtime::settle_pending_promise`.
    ResolveString(String),
    /// Reject the promise with a string reason.
    RejectString(String),
}

/// Per-runtime token → persistent-root map. Owned by
/// [`crate::Runtime`]; mutated only on the isolate runner thread.
#[derive(Debug, Default)]
pub struct PromiseRegistry {
    entries: HashMap<u64, PersistentRootId>,
    next_id: u64,
}

impl PromiseRegistry {
    /// Empty registry. The first issued id is `1`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
            next_id: 1,
        }
    }

    /// Associate a fresh token with an already-published persistent root.
    ///
    /// The caller must insert the Promise value into the interpreter-owned
    /// root table before calling this method.
    pub fn register(&mut self, root: PersistentRootId) -> PromiseId {
        let id = self.next_id;
        self.next_id = self
            .next_id
            .checked_add(1)
            .expect("promise id overflow is impossible inside one runtime lifetime");
        self.entries.insert(id, root);
        PromiseId(id)
    }

    /// Pop the entry matching `id`, returning the stored persistent
    /// root (consumes the entry — settlement is one-shot per spec
    /// §27.2.1.{4,7}).
    pub fn take(&mut self, id: PromiseId) -> Option<PersistentRootId> {
        self.entries.remove(&id.0)
    }

    /// Number of registered promises — diagnostic only.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// `true` when no entries are registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}
