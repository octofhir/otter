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
use crate::object::JsObject;
use otter_gc::raw::{RawGc, SlotVisitor};

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
    handler: JsObject,
    /// `true` once `Proxy.revocable().revoke()` has fired.
    revoked: Cell<bool>,
}

impl JsProxy {
    /// Construct a proxy over `target` with `handler`.
    #[must_use]
    pub fn new(target: Value, handler: JsObject) -> Self {
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

    /// Target object — convenience for trap dispatchers that want
    /// the JsObject form (panics-free: returns a synthetic empty
    /// object when the target is a non-object callable, mirroring
    /// the spec's `[[ProxyTarget]]` slot which always holds an
    /// Object).
    #[must_use]
    pub fn target_object(&self, gc_heap: &mut otter_gc::GcHeap) -> JsObject {
        match &self.inner.target {
            Value::Object(o) => *o,
            // Fallback empty sentinel when target is a callable
            // non-object — never JS-visible because the dispatcher
            // routes those through their own [[Call]] paths first.
            _ => crate::object::alloc_object(gc_heap).unwrap_or_else(|_| otter_gc::Gc::null()),
        }
    }

    /// Handler object.
    #[must_use]
    pub fn handler(&self) -> JsObject {
        self.inner.handler
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
        let p = &self.inner.handler as *const JsObject as *mut RawGc;
        visitor(p);
    }
}

impl PartialEq for JsProxy {
    fn eq(&self, other: &Self) -> bool {
        self.ptr_eq(other)
    }
}
