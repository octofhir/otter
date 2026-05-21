//! ECMA-262 §28.2 `Proxy` object — meta-programming hook between
//! `[[Get]]` / `[[Set]]` / etc. and a user-defined handler.
//!
//! A proxy carries a `(target, handler)` pair. Each property
//! operation consults the corresponding handler trap; when the trap
//! is missing or the proxy is revoked, the operation falls through
//! to the target object.
//!
//! # Contents
//! - [`JsProxy`] — cheap-to-clone handle.
//! - [`ProxyBody`] — internal storage.
//!
//! # Invariants
//! - `target` is any Object-like [`Value`] accepted by §7.2.4
//!   `IsConstructor` / §7.2.3 `IsCallable` (`Value::Object`,
//!   `Value::Array`, the callable variants, and nested
//!   `Value::Proxy`). The constructor coerces callables so the
//!   `apply` / `construct` trap fallback can invoke the underlying
//!   function directly.
//! - `revoked` flips from `false` to `true` once and never back; a
//!   revoked proxy raises `TypeError` from every trap dispatch.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-proxy-objects>

use std::cell::Cell;
use std::rc::Rc;

use crate::Value;
use otter_gc::raw::SlotVisitor;

/// Reserved [`otter_gc::Traceable::TYPE_TAG`] for [`ProxyBodyGc`].
pub const PROXY_BODY_TYPE_TAG: u8 = 0x29;

/// GC body for [`crate::Value::Proxy`].
///
/// Mutators flip `revoked` through [`otter_gc::GcHeap::with_payload`]
/// (no interior mutability in GC bodies).
#[derive(Debug)]
pub struct ProxyBodyGc {
    /// Target value trap-less operations fall through to. ECMA-262
    /// §28.2 accepts any object, including callables.
    pub target: Value,
    /// Handler object — trap properties live here.
    pub handler: Value,
    /// `true` once `Proxy.revocable().revoke()` has fired.
    pub revoked: bool,
}

impl otter_gc::SafeTraceable for ProxyBodyGc {
    const TYPE_TAG: u8 = PROXY_BODY_TYPE_TAG;

    fn trace_slots_safe(&self, visitor: &mut SlotVisitor<'_>) {
        self.target.trace_value_slots(visitor);
        self.handler.trace_value_slots(visitor);
    }
}

/// 4-byte compressed GC handle to a [`ProxyBodyGc`]. `Copy`.
pub type ProxyHandle = otter_gc::Gc<ProxyBodyGc>;

/// Allocate a Proxy body on the GC heap.
///
/// Lives in old-space because the scavenger does not yet rewrite
/// embedded `Value` slots.
///
/// # Errors
///
/// Surfaces [`otter_gc::OutOfMemory`] verbatim.
pub fn alloc_proxy(
    heap: &mut otter_gc::GcHeap,
    target: Value,
    handler: Value,
) -> Result<ProxyHandle, otter_gc::OutOfMemory> {
    heap.alloc_old(ProxyBodyGc {
        target,
        handler,
        revoked: false,
    })
}

/// Cheap-to-clone Proxy handle.
#[derive(Debug, Clone)]
pub struct JsProxy {
    inner: Rc<ProxyBody>,
}

/// Internal storage for a Proxy.
#[derive(Debug)]
pub struct ProxyBody {
    /// Target value every trap-less operation falls through to.
    /// Spec accepts any object, including callables — Foundation
    /// stores the original [`Value`] so trap fallback can re-call
    /// the underlying function directly.
    target: Value,
    /// Handler object trap properties live on.
    handler: Value,
    /// `true` once `Proxy.revocable().revoke()` has fired.
    revoked: Cell<bool>,
}

impl JsProxy {
    /// Construct a proxy over `target` with `handler`.
    #[must_use]
    pub fn new(target: Value, handler: Value) -> Self {
        Self {
            inner: Rc::new(ProxyBody {
                target,
                handler,
                revoked: Cell::new(false),
            }),
        }
    }

    /// Target value.
    #[must_use]
    pub fn target(&self) -> Value {
        self.inner.target.clone()
    }

    /// Handler object.
    #[must_use]
    pub fn handler(&self) -> Value {
        self.inner.handler.clone()
    }

    /// `true` once revoked.
    #[must_use]
    pub fn is_revoked(&self) -> bool {
        self.inner.revoked.get()
    }

    /// Revoke the proxy. Idempotent; subsequent calls are no-ops.
    pub fn revoke(&self) {
        self.inner.revoked.set(true);
    }

    /// Identity comparison.
    #[must_use]
    pub fn ptr_eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.inner, &other.inner)
    }

    /// `Rc` data-pointer for cycle / identity sets.
    #[must_use]
    pub fn identity_addr(&self) -> *const () {
        Rc::as_ptr(&self.inner).cast()
    }

    /// Trace GC handles reachable from the proxy's target and
    /// handler slots.
    pub(crate) fn trace_value_slots(&self, visitor: &mut SlotVisitor<'_>) {
        self.inner.target.trace_value_slots(visitor);
        self.inner.handler.trace_value_slots(visitor);
    }
}

impl PartialEq for JsProxy {
    fn eq(&self, other: &Self) -> bool {
        self.ptr_eq(other)
    }
}
