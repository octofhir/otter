//! `Value::NativeFunction` — host-implemented callable values.
//!
//! Native callables are GC-managed handles. Their Rust closure
//! payload is a leaf from the GC's point of view; any JS values the
//! closure owns must also be listed in the body's capture list so
//! tracing can keep those values alive.
//!
//! # Contents
//! - [`NativeFunction`] — cheap-to-clone GC handle.
//! - [`NativeFunctionBody`] — name, closure payload, and traced
//!   captured values.
//! - [`NativeFn`] — the function-pointer signature.
//! - [`NativeError`] — failure outcome the dispatcher converts to
//!   `VmError`.
//!
//! # Invariants
//! - Every allocation receives an explicit [`otter_gc::GcHeap`].
//! - The call signature receives an explicit [`crate::NativeCtx`].
//!   Host async work must copy owned, non-GC data out before any
//!   `.await`; `NativeCtx`, `Value`, and GC handles are
//!   isolate-local.
//! - The body traces only `captures`; the Rust closure must not hide
//!   additional GC-bearing values without mirroring them there.
//!
//! # See also
//! - [GC architecture plan §4.1](../../../docs/new-engine/gc-architecture.md)
//! - [Task 83](../../../docs/new-engine/tasks/83-migrate-bound-native-regexp.md)

use std::rc::Rc;

use smallvec::SmallVec;

use crate::{NativeCtx, Value};

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
pub type NativeFn = dyn for<'rt> Fn(&mut NativeCtx<'rt>, &[Value]) -> Result<Value, NativeError>;

/// Optional tracing hook for native payloads whose Rust-side state
/// owns JS values outside the fixed capture list.
pub type NativeTraceFn = dyn Fn(&mut otter_gc::SlotVisitor<'_>);

/// Heap payload for [`Value::NativeFunction`].
pub struct NativeFunctionBody {
    /// Display name (used in stack traces and `Function.prototype.
    /// toString` once that lands).
    pub name: &'static str,
    /// Captured `Fn` payload.
    pub call: Rc<NativeFn>,
    /// JS values owned by the native payload and therefore traced
    /// strongly while this function is reachable.
    pub captures: SmallVec<[Value; 4]>,
    /// Optional trace hook for native-owned state such as shared
    /// Promise combinator slots.
    pub trace: Option<Rc<NativeTraceFn>>,
}

impl otter_gc::SafeTraceable for NativeFunctionBody {
    const TYPE_TAG: u8 = NATIVE_FUNCTION_BODY_TYPE_TAG;

    fn trace_slots_safe(&self, visitor: &mut otter_gc::SlotVisitor<'_>) {
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
        F: for<'rt> Fn(&mut NativeCtx<'rt>, &[Value]) -> Result<Value, NativeError> + 'static,
    {
        Self::with_captures(heap, name, call, SmallVec::new())
    }

    /// Build a native function with explicit traced JS captures.
    pub fn with_captures<F>(
        heap: &mut otter_gc::GcHeap,
        name: &'static str,
        call: F,
        captures: SmallVec<[Value; 4]>,
    ) -> Result<Self, otter_gc::OutOfMemory>
    where
        F: for<'rt> Fn(&mut NativeCtx<'rt>, &[Value]) -> Result<Value, NativeError> + 'static,
    {
        Ok(Self {
            inner: heap.alloc_old(NativeFunctionBody {
                name,
                call: Rc::new(call),
                captures,
                trace: None,
            })?,
        })
    }

    /// Build a native function with explicit captures plus an
    /// additional payload-specific trace hook.
    pub fn with_trace<F>(
        heap: &mut otter_gc::GcHeap,
        name: &'static str,
        call: F,
        captures: SmallVec<[Value; 4]>,
        trace: Rc<NativeTraceFn>,
    ) -> Result<Self, otter_gc::OutOfMemory>
    where
        F: for<'rt> Fn(&mut NativeCtx<'rt>, &[Value]) -> Result<Value, NativeError> + 'static,
    {
        Ok(Self {
            inner: heap.alloc_old(NativeFunctionBody {
                name,
                call: Rc::new(call),
                captures,
                trace: Some(trace),
            })?,
        })
    }

    /// Raw handle used by root tracing and write barriers.
    #[must_use]
    pub fn raw(&self) -> otter_gc::RawGc {
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

    /// Clone the Rust closure payload so the caller can invoke it
    /// after releasing the heap borrow.
    #[must_use]
    pub fn call(&self, heap: &otter_gc::GcHeap) -> Rc<NativeFn> {
        heap.read_payload(self.inner, |body| body.call.clone())
    }

    /// Trace this handle as a root slot.
    pub(crate) fn trace_value_slots(&self, visitor: &mut otter_gc::SlotVisitor<'_>) {
        let p = self as *const NativeFunction as *mut otter_gc::RawGc;
        visitor(p);
    }
}

/// Convenience: produce a `Value::NativeFunction` from a closure.
pub fn native_value<F>(
    heap: &mut otter_gc::GcHeap,
    name: &'static str,
    call: F,
) -> Result<Value, otter_gc::OutOfMemory>
where
    F: for<'rt> Fn(&mut NativeCtx<'rt>, &[Value]) -> Result<Value, NativeError> + 'static,
{
    Ok(Value::NativeFunction(NativeFunction::new(
        heap, name, call,
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
    F: for<'rt> Fn(&mut NativeCtx<'rt>, &[Value]) -> Result<Value, NativeError> + 'static,
{
    Ok(Value::NativeFunction(NativeFunction::with_captures(
        heap, name, call, captures,
    )?))
}

/// Convenience: produce a native function with explicit captures
/// and a payload-specific trace hook.
pub fn native_value_with_trace<F>(
    heap: &mut otter_gc::GcHeap,
    name: &'static str,
    captures: SmallVec<[Value; 4]>,
    trace: Rc<NativeTraceFn>,
    call: F,
) -> Result<Value, otter_gc::OutOfMemory>
where
    F: for<'rt> Fn(&mut NativeCtx<'rt>, &[Value]) -> Result<Value, NativeError> + 'static,
{
    Ok(Value::NativeFunction(NativeFunction::with_trace(
        heap, name, call, captures, trace,
    )?))
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
        let f = native_value(interp.gc_heap_mut(), "identity", |_, args| {
            Ok(args.first().cloned().unwrap_or(Value::Undefined))
        })
        .expect("native");
        let Value::NativeFunction(native) = &f else {
            panic!("expected NativeFunction")
        };
        let call = native.call(interp.gc_heap());
        let mut ctx = NativeCtx::new(&mut interp);
        let r = call(&mut ctx, &[Value::Number(NumberValue::from_i32(7))]).unwrap();
        assert_eq!(r.display_string(), "7");
    }

    #[test]
    fn rejects_arity_via_typeerror() {
        let mut interp = crate::Interpreter::new();
        let f = native_value(interp.gc_heap_mut(), "require_one_arg", |_, args| {
            if args.len() != 1 {
                return Err(NativeError::TypeError {
                    name: "require_one_arg",
                    reason: format!("expected 1 arg, got {}", args.len()),
                });
            }
            Ok(args[0].clone())
        })
        .expect("native");
        let Value::NativeFunction(native) = &f else {
            panic!()
        };
        let call = native.call(interp.gc_heap());
        let mut ctx = NativeCtx::new(&mut interp);
        let err = call(&mut ctx, &[]).unwrap_err();
        assert!(matches!(err, NativeError::TypeError { .. }));
    }
}
