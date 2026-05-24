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

use crate::Value;
use otter_gc::raw::SlotVisitor;
use otter_macros::Pelt;

/// Reserved [`otter_gc::Traceable::TYPE_TAG`] for [`ProxyBodyGc`].
pub const PROXY_BODY_TYPE_TAG: u8 = 0x29;

/// GC body for [`crate::Value::Proxy`].
///
/// Mutators flip `revoked` through [`otter_gc::GcHeap::with_payload`]
/// (no interior mutability in GC bodies).
#[derive(Debug, Pelt)]
#[pelt(tag = PROXY_BODY_TYPE_TAG)]
pub struct ProxyBodyGc {
    /// Target value trap-less operations fall through to. ECMA-262
    /// §28.2 accepts any object, including callables.
    pub target: Value,
    /// Handler object — trap properties live here.
    pub handler: Value,
    /// `true` once `Proxy.revocable().revoke()` has fired.
    #[pelt(skip)]
    pub revoked: bool,
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

/// Cheap-to-copy Proxy wrapper carrying a [`ProxyHandle`].
///
/// ECMA-262 §28.2 Proxy state lives in the GC body. All reader /
/// mutator entry points thread the heap explicitly — no off-heap
/// cache, no `Cell` / `RefCell`.
#[derive(Debug, Clone, Copy)]
pub struct JsProxy {
    handle: ProxyHandle,
}

impl JsProxy {
    /// Construct a proxy over `target` with `handler`.
    ///
    /// # Errors
    ///
    /// Surfaces [`otter_gc::OutOfMemory`] from the underlying
    /// `alloc_proxy` call.
    pub fn new(
        heap: &mut otter_gc::GcHeap,
        target: Value,
        handler: Value,
    ) -> Result<Self, otter_gc::OutOfMemory> {
        Ok(Self {
            handle: alloc_proxy(heap, target, handler)?,
        })
    }

    /// Wrap an existing GC handle (e.g. after a downcast from
    /// [`crate::Value`]).
    #[must_use]
    pub fn from_handle(handle: ProxyHandle) -> Self {
        Self { handle }
    }

    /// Raw GC handle.
    #[must_use]
    pub fn handle(self) -> ProxyHandle {
        self.handle
    }

    /// Target value.
    #[must_use]
    pub fn target(self, heap: &otter_gc::GcHeap) -> Value {
        heap.read_payload(self.handle, |body| body.target)
    }

    /// Handler object.
    #[must_use]
    pub fn handler(self, heap: &otter_gc::GcHeap) -> Value {
        heap.read_payload(self.handle, |body| body.handler)
    }

    /// `true` once revoked.
    #[must_use]
    pub fn is_revoked(self, heap: &otter_gc::GcHeap) -> bool {
        heap.read_payload(self.handle, |body| body.revoked)
    }

    /// Revoke the proxy. Idempotent; subsequent calls are no-ops.
    /// Spec §28.2.2.1 RevokeProxy step 4 clears target/handler to
    /// `null` so trap dispatch can detect revocation without an
    /// extra heap read.
    pub fn revoke(self, heap: &mut otter_gc::GcHeap) {
        heap.with_payload(self.handle, |body| {
            body.revoked = true;
            body.target = Value::null();
            body.handler = Value::null();
        });
    }

    /// Identity comparison via the underlying handle offset.
    #[must_use]
    pub fn ptr_eq(self, other: Self) -> bool {
        self.handle.offset() == other.handle.offset()
    }

    /// Stable identity address for cycle / identity sets.
    #[must_use]
    pub fn identity_addr(self) -> *const () {
        self.handle.offset() as usize as *const ()
    }

    /// Trace the embedded GC handle slot.
    pub(crate) fn trace_value_slots(&self, visitor: &mut SlotVisitor<'_>) {
        let p = &self.handle as *const ProxyHandle as *mut otter_gc::raw::RawGc;
        visitor(p);
    }
}

impl PartialEq for JsProxy {
    fn eq(&self, other: &Self) -> bool {
        self.ptr_eq(*other)
    }
}

impl Eq for JsProxy {}

impl std::hash::Hash for JsProxy {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.handle.offset().hash(state);
    }
}
