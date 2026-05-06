//! `Value::NativeFunction` — host-implemented callable values.
//!
//! Native callables are GC-managed handles. Production builtins use
//! a static function-pointer dispatch path; dynamic closures remain
//! available for host/embedder cases that need captured Rust state.
//! Any JS values a dynamic closure owns must also be listed in the
//! body's capture list so tracing can keep those values alive.
//!
//! # Contents
//! - [`NativeFunction`] — cheap-to-clone GC handle.
//! - [`NativeFunctionBody`] — name, closure payload, and traced
//!   captured values.
//! - [`NativeFastFn`] / [`NativeCall`] — static and dynamic native
//!   dispatch targets.
//! - [`NativeFn`] — the dynamic closure signature.
//! - [`NativeError`] — failure outcome the dispatcher converts to
//!   `VmError`.
//!
//! # Invariants
//! - Every allocation receives an explicit [`otter_gc::GcHeap`].
//! - The call signature receives an explicit [`crate::NativeCtx`].
//!   Host async work must copy owned, non-GC data out before any
//!   `.await`; `NativeCtx`, `Value`, and GC handles are
//!   isolate-local.
//! - Static builtins carry a plain function pointer and no captured
//!   payload.
//! - Public dynamic native constructors require `Send + Sync`
//!   closures and pass traced JS captures as an explicit slice at
//!   call time. That keeps embedders from hiding isolate-local
//!   `Gc<T>` / `Value` handles inside a long-lived closure.
//! - Crate-internal unchecked constructors are reserved for audited
//!   isolate-local VM helpers whose payload-specific trace hook covers
//!   every hidden JS value.
//!
//! # See also
//! - [GC architecture plan §4.1](../../../docs/new-engine/gc-architecture.md)
//! - [Task 83](../../../docs/new-engine/tasks/83-migrate-bound-native-regexp.md)

use std::rc::Rc;
use std::sync::Arc;

use smallvec::SmallVec;

use crate::{NativeCtx, Value};
use otter_gc::raw::{RawGc, SlotVisitor};

/// Reserved [`otter_gc::Traceable::TYPE_TAG`] for
/// [`NativeFunctionBody`].
pub const NATIVE_FUNCTION_BODY_TYPE_TAG: u8 = 0x1d;

/// Function-pointer signature for native callables.
///
/// `ctx` is the isolate-bound native view. Native bodies enqueue
/// work but **must not** synchronously re-enter the dispatch loop.
/// JS-side callbacks that need to run in turn (e.g. promise
/// reactions) flow through the microtask queue.
///
/// `args` is the JS argument list (post-coercion of any `apply`
/// expansion). Implementations return `Ok(value)` to write into
/// the call-site destination register, or `Err` to surface as a
/// runtime error.
pub type NativeFn = dyn for<'rt> Fn(&mut NativeCtx<'rt>, &[Value], &[Value]) -> Result<Value, NativeError>
    + Send
    + Sync;

type LocalNativeFn =
    dyn for<'rt> Fn(&mut NativeCtx<'rt>, &[Value], &[Value]) -> Result<Value, NativeError>;

/// Function-pointer signature for static builtin callables.
///
/// This is the production fast path for spec-declared builtins and
/// future macro-generated surfaces: invoking it requires no closure
/// allocation, capture clone, or dynamic dispatch.
pub type NativeFastFn = for<'rt> fn(&mut NativeCtx<'rt>, &[Value]) -> Result<Value, NativeError>;

/// Native callable storage.
///
/// Static specs should use [`NativeCall::Static`]. Dynamic closures
/// are reserved for embedder cases that need captured Rust state.
#[derive(Clone)]
pub enum NativeCall {
    /// Plain function-pointer dispatch with no captured payload.
    Static(NativeFastFn),
    /// Dynamic closure dispatch. Captured JS values still live in
    /// [`NativeFunctionBody::captures`] so the GC can trace them.
    Dynamic(Arc<NativeFn>),
}

#[derive(Clone)]
enum NativeCallStorage {
    Static(NativeFastFn),
    Dynamic(Arc<NativeFn>),
    LocalDynamic(Rc<LocalNativeFn>),
}

impl From<NativeCall> for NativeCallStorage {
    fn from(value: NativeCall) -> Self {
        match value {
            NativeCall::Static(call) => Self::Static(call),
            NativeCall::Dynamic(call) => Self::Dynamic(call),
        }
    }
}

impl std::fmt::Debug for NativeCall {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Static(_) => f.write_str("NativeCall::Static(..)"),
            Self::Dynamic(_) => f.write_str("NativeCall::Dynamic(..)"),
        }
    }
}

/// Optional tracing hook for native payloads whose Rust-side state
/// owns JS values outside the fixed capture list.
pub type NativeTraceFn = dyn Fn(&mut SlotVisitor<'_>);

/// Heap payload for [`Value::NativeFunction`].
pub struct NativeFunctionBody {
    /// Display name (used in stack traces and `Function.prototype.
    /// toString` once that lands).
    name: &'static str,
    /// ECMAScript `.length` metadata.
    length: u8,
    /// Static function pointer or dynamic closure payload.
    call: NativeCallStorage,
    /// JS values owned by the native payload and therefore traced
    /// strongly while this function is reachable.
    captures: SmallVec<[Value; 4]>,
    /// Optional trace hook for native-owned state such as shared
    /// Promise combinator slots.
    trace: Option<Rc<NativeTraceFn>>,
}

impl otter_gc::SafeTraceable for NativeFunctionBody {
    const TYPE_TAG: u8 = NATIVE_FUNCTION_BODY_TYPE_TAG;

    fn trace_slots_safe(&self, visitor: &mut SlotVisitor<'_>) {
        for value in &self.captures {
            value.trace_value_slots(visitor);
        }
        if let Some(trace) = &self.trace {
            trace(visitor);
        }
    }
}

/// Cheap-to-clone native-function handle.
#[repr(transparent)]
#[derive(Clone, Copy)]
pub struct NativeFunction {
    inner: otter_gc::Gc<NativeFunctionBody>,
}

impl std::fmt::Debug for NativeFunction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeFunction")
            .field("inner", &self.inner)
            .finish()
    }
}

impl NativeFunction {
    /// Build a native function with a static name and an `Fn`
    /// payload.
    pub fn new<F>(
        heap: &mut otter_gc::GcHeap,
        name: &'static str,
        call: F,
    ) -> Result<Self, otter_gc::OutOfMemory>
    where
        F: for<'rt> Fn(&mut NativeCtx<'rt>, &[Value], &[Value]) -> Result<Value, NativeError>
            + Send
            + Sync
            + 'static,
    {
        Self::with_length_and_captures(heap, name, 0, call, SmallVec::new())
    }

    /// Build a static native function with explicit `.length`.
    pub fn new_static(
        heap: &mut otter_gc::GcHeap,
        name: &'static str,
        length: u8,
        call: NativeFastFn,
    ) -> Result<Self, otter_gc::OutOfMemory> {
        Ok(Self {
            inner: heap.alloc_old(NativeFunctionBody {
                name,
                length,
                call: NativeCallStorage::Static(call),
                captures: SmallVec::new(),
                trace: None,
            })?,
        })
    }

    /// Build a native function from an already-classified call
    /// target.
    pub fn from_call(
        heap: &mut otter_gc::GcHeap,
        name: &'static str,
        length: u8,
        call: NativeCall,
    ) -> Result<Self, otter_gc::OutOfMemory> {
        Ok(Self {
            inner: heap.alloc_old(NativeFunctionBody {
                name,
                length,
                call: call.into(),
                captures: SmallVec::new(),
                trace: None,
            })?,
        })
    }

    /// Build a native function with explicit traced JS captures.
    pub fn with_captures<F>(
        heap: &mut otter_gc::GcHeap,
        name: &'static str,
        call: F,
        captures: SmallVec<[Value; 4]>,
    ) -> Result<Self, otter_gc::OutOfMemory>
    where
        F: for<'rt> Fn(&mut NativeCtx<'rt>, &[Value], &[Value]) -> Result<Value, NativeError>
            + Send
            + Sync
            + 'static,
    {
        Self::with_length_and_captures(heap, name, 0, call, captures)
    }

    /// Build a dynamic native function with explicit `.length` and
    /// explicit traced JS captures.
    pub fn with_length_and_captures<F>(
        heap: &mut otter_gc::GcHeap,
        name: &'static str,
        length: u8,
        call: F,
        captures: SmallVec<[Value; 4]>,
    ) -> Result<Self, otter_gc::OutOfMemory>
    where
        F: for<'rt> Fn(&mut NativeCtx<'rt>, &[Value], &[Value]) -> Result<Value, NativeError>
            + Send
            + Sync
            + 'static,
    {
        Ok(Self {
            inner: heap.alloc_old(NativeFunctionBody {
                name,
                length,
                call: NativeCallStorage::Dynamic(Arc::new(call)),
                captures,
                trace: None,
            })?,
        })
    }

    /// Raw handle used by root tracing and write barriers.
    #[must_use]
    pub(crate) fn raw(&self) -> RawGc {
        self.inner.raw()
    }

    /// Stable identity token.
    #[must_use]
    pub fn identity_addr(&self) -> *const () {
        self.inner.as_header_ptr() as *const ()
    }

    /// Identity comparison.
    #[must_use]
    pub fn ptr_eq(&self, other: &Self) -> bool {
        self.inner == other.inner
    }

    /// Read display metadata.
    #[must_use]
    pub fn name(&self, heap: &otter_gc::GcHeap) -> &'static str {
        heap.read_payload(self.inner, |body| body.name)
    }

    /// Read ECMAScript `.length` metadata.
    #[must_use]
    pub fn length(&self, heap: &otter_gc::GcHeap) -> u8 {
        heap.read_payload(self.inner, |body| body.length)
    }

    /// Clone the call target and captures so the caller can invoke
    /// it after releasing the heap borrow.
    #[must_use]
    pub(crate) fn call_target(&self, heap: &otter_gc::GcHeap) -> NativeCallTarget {
        heap.read_payload(self.inner, |body| match &body.call {
            NativeCallStorage::Static(call) => NativeCallTarget::Static(*call),
            NativeCallStorage::Dynamic(call) => NativeCallTarget::Dynamic {
                call: call.clone(),
                captures: body.captures.clone(),
            },
            NativeCallStorage::LocalDynamic(call) => NativeCallTarget::LocalDynamic {
                call: call.clone(),
                captures: body.captures.clone(),
            },
        })
    }

    /// `true` when this callable uses the static function-pointer
    /// fast path.
    #[must_use]
    pub fn is_static_call(&self, heap: &otter_gc::GcHeap) -> bool {
        heap.read_payload(self.inner, |body| {
            matches!(body.call, NativeCallStorage::Static(_))
        })
    }

    /// Trace this handle as a root slot.
    pub(crate) fn trace_value_slots(&self, visitor: &mut SlotVisitor<'_>) {
        let p = self as *const NativeFunction as *mut RawGc;
        visitor(p);
    }
}

/// Cloned native target ready for invocation after the heap borrow
/// has ended.
pub(crate) enum NativeCallTarget {
    /// Static fast path.
    Static(NativeFastFn),
    /// Dynamic closure path with traced captures.
    Dynamic {
        /// Closure payload.
        call: Arc<NativeFn>,
        /// Traced JS captures.
        captures: SmallVec<[Value; 4]>,
    },
    /// Local VM-only closure path.
    LocalDynamic {
        /// Closure payload.
        call: Rc<LocalNativeFn>,
        /// Traced JS captures.
        captures: SmallVec<[Value; 4]>,
    },
}

impl NativeCallTarget {
    /// Invoke the target.
    pub(crate) fn invoke(
        self,
        ctx: &mut NativeCtx<'_>,
        args: &[Value],
    ) -> Result<Value, NativeError> {
        match self {
            Self::Static(call) => call(ctx, args),
            Self::Dynamic { call, captures } => call(ctx, args, &captures),
            Self::LocalDynamic { call, captures } => call(ctx, args, &captures),
        }
    }
}

/// Convenience: produce a `Value::NativeFunction` from a closure.
pub fn native_value<F>(
    heap: &mut otter_gc::GcHeap,
    name: &'static str,
    call: F,
) -> Result<Value, otter_gc::OutOfMemory>
where
    F: for<'rt> Fn(&mut NativeCtx<'rt>, &[Value], &[Value]) -> Result<Value, NativeError>
        + Send
        + Sync
        + 'static,
{
    Ok(Value::NativeFunction(NativeFunction::new(
        heap, name, call,
    )?))
}

/// Convenience: produce a static native function value.
pub fn native_value_static(
    heap: &mut otter_gc::GcHeap,
    name: &'static str,
    length: u8,
    call: NativeFastFn,
) -> Result<Value, otter_gc::OutOfMemory> {
    Ok(Value::NativeFunction(NativeFunction::new_static(
        heap, name, length, call,
    )?))
}

/// Convenience: produce a native function with explicit traced JS
/// captures.
pub fn native_value_with_captures<F>(
    heap: &mut otter_gc::GcHeap,
    name: &'static str,
    captures: SmallVec<[Value; 4]>,
    call: F,
) -> Result<Value, otter_gc::OutOfMemory>
where
    F: for<'rt> Fn(&mut NativeCtx<'rt>, &[Value], &[Value]) -> Result<Value, NativeError>
        + Send
        + Sync
        + 'static,
{
    Ok(Value::NativeFunction(NativeFunction::with_captures(
        heap, name, call, captures,
    )?))
}

pub(crate) fn native_value_unchecked<F>(
    heap: &mut otter_gc::GcHeap,
    name: &'static str,
    call: F,
) -> Result<Value, otter_gc::OutOfMemory>
where
    F: for<'rt> Fn(&mut NativeCtx<'rt>, &[Value], &[Value]) -> Result<Value, NativeError> + 'static,
{
    native_value_with_captures_unchecked(heap, name, SmallVec::new(), call)
}

pub(crate) fn native_value_with_captures_unchecked<F>(
    heap: &mut otter_gc::GcHeap,
    name: &'static str,
    captures: SmallVec<[Value; 4]>,
    call: F,
) -> Result<Value, otter_gc::OutOfMemory>
where
    F: for<'rt> Fn(&mut NativeCtx<'rt>, &[Value], &[Value]) -> Result<Value, NativeError> + 'static,
{
    Ok(Value::NativeFunction(NativeFunction {
        inner: heap.alloc_old(NativeFunctionBody {
            name,
            length: 0,
            call: NativeCallStorage::LocalDynamic(Rc::new(call)),
            captures,
            trace: None,
        })?,
    }))
}

pub(crate) fn native_value_with_trace_unchecked<F>(
    heap: &mut otter_gc::GcHeap,
    name: &'static str,
    captures: SmallVec<[Value; 4]>,
    trace: Rc<NativeTraceFn>,
    call: F,
) -> Result<Value, otter_gc::OutOfMemory>
where
    F: for<'rt> Fn(&mut NativeCtx<'rt>, &[Value], &[Value]) -> Result<Value, NativeError> + 'static,
{
    Ok(Value::NativeFunction(NativeFunction {
        inner: heap.alloc_old(NativeFunctionBody {
            name,
            length: 0,
            call: NativeCallStorage::LocalDynamic(Rc::new(call)),
            captures,
            trace: Some(trace),
        })?,
    }))
}

/// Failure outcome from a native call. Mirrors the
/// [`crate::IntrinsicError`] / [`crate::math::MathError`] shape so
/// the runtime mapper can route everything through one path.
#[derive(Debug, Clone, thiserror::Error)]
#[non_exhaustive]
pub enum NativeError {
    /// A user-thrown JS value escaped the native body. The
    /// dispatcher will route this through the same path as
    /// `Op::Throw` — i.e. into the catchable handler stack.
    #[error("native function {name} threw")]
    Thrown {
        /// Display name of the offending native (for diagnostics).
        name: &'static str,
        /// The thrown value. Foundation: rendered to a string.
        message: String,
    },
    /// Type or value error inside the native body that does not
    /// originate as a `throw` (e.g. wrong arity). Surfaces as
    /// `VmError::TypeMismatch`.
    #[error("native function {name}: {reason}")]
    TypeError {
        /// Display name of the native.
        name: &'static str,
        /// Short reason.
        reason: String,
    },
}

impl From<otter_gc::OutOfMemory> for NativeError {
    fn from(_: otter_gc::OutOfMemory) -> Self {
        Self::TypeError {
            name: "native",
            reason: "out of memory".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::number::NumberValue;

    #[test]
    fn native_value_dispatches() {
        let mut interp = crate::Interpreter::new();
        let f = native_value(interp.gc_heap_mut(), "identity", |_, args, _captures| {
            Ok(args.first().cloned().unwrap_or(Value::Undefined))
        })
        .expect("native");
        let Value::NativeFunction(native) = &f else {
            panic!("expected NativeFunction")
        };
        let call = native.call_target(interp.gc_heap());
        let mut ctx = NativeCtx::new(&mut interp);
        let r = call
            .invoke(&mut ctx, &[Value::Number(NumberValue::from_i32(7))])
            .unwrap();
        assert_eq!(r.display_string(), "7");
    }

    #[test]
    fn rejects_arity_via_typeerror() {
        let mut interp = crate::Interpreter::new();
        let f = native_value(
            interp.gc_heap_mut(),
            "require_one_arg",
            |_, args, _captures| {
                if args.len() != 1 {
                    return Err(NativeError::TypeError {
                        name: "require_one_arg",
                        reason: format!("expected 1 arg, got {}", args.len()),
                    });
                }
                Ok(args[0].clone())
            },
        )
        .expect("native");
        let Value::NativeFunction(native) = &f else {
            panic!()
        };
        let call = native.call_target(interp.gc_heap());
        let mut ctx = NativeCtx::new(&mut interp);
        let err = call.invoke(&mut ctx, &[]).unwrap_err();
        assert!(matches!(err, NativeError::TypeError { .. }));
    }

    #[test]
    fn static_native_value_uses_fast_path_and_length() {
        fn id(_: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
            Ok(args.first().cloned().unwrap_or(Value::Undefined))
        }

        let mut interp = crate::Interpreter::new();
        let f = native_value_static(interp.gc_heap_mut(), "id", 1, id).expect("native");
        let Value::NativeFunction(native) = &f else {
            panic!("expected NativeFunction")
        };
        assert!(native.is_static_call(interp.gc_heap()));
        assert_eq!(native.length(interp.gc_heap()), 1);
    }
}
