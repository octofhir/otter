//! `Value::NativeFunction` — host-implemented callable values.
//!
//! Foundation slice 34 introduces this so the `Promise` constructor
//! can hand `resolve` / `reject` closures to user code without
//! going through `Op::MakeFunction` (which only knows how to wrap
//! a bytecode function). Future Phase F surfaces (`fetch`, timers,
//! `Promise.all` aggregator-functions) reuse the same pipe.
//!
//! # Contents
//! - [`NativeFunction`] — heap struct with a name + the actual
//!   `Fn` payload.
//! - [`NativeFn`] — the function-pointer signature.
//! - [`NativeError`] — failure outcome the dispatcher converts to
//!   `VmError`.
//!
//! # Invariants
//! - The closure is `Fn` (not `FnMut`) so multiple call sites can
//!   share one [`NativeFunction`] handle. State that needs
//!   mutation goes through interior cells inside the captured
//!   environment (e.g. an `Rc<RefCell<...>>` shared with the
//!   originating object).
//! - Calls run **inline** — no frame is pushed, no stack-depth
//!   bookkeeping. The closure is responsible for not recursing
//!   into another VM call without the dispatcher's help.
//! - Native callables are **not** GC roots in the foundation; the
//!   `Rc` payload keeps them alive as long as a `Value` holds the
//!   handle.
//!
//! # See also
//! - [`docs/new-engine/tasks/34-promise-value.md`](
//!     ../../../docs/new-engine/tasks/34-promise-value.md
//!   )

use std::rc::Rc;

use crate::{Interpreter, Value};

/// Function-pointer signature for native callables.
///
/// `interp` is the interpreter holding the microtask queue and
/// other side-effecting state; native bodies enqueue work but
/// **must not** synchronously re-enter the dispatch loop. JS-side
/// callbacks that need to run in turn (e.g. promise reactions)
/// flow through the microtask queue.
///
/// `args` is the JS argument list (post-coercion of any `apply`
/// expansion). Implementations return `Ok(value)` to write into
/// the call-site destination register, or `Err` to surface as a
/// runtime error.
pub type NativeFn = dyn Fn(&mut Interpreter, &[Value]) -> Result<Value, NativeError>;

/// Heap struct for [`Value::NativeFunction`]. Cheap to clone — the
/// inner `Rc` is shared.
pub struct NativeFunction {
    /// Display name (used in stack traces and `Function.prototype.
    /// toString` once that lands).
    pub name: &'static str,
    /// Captured `Fn` payload.
    pub call: Box<NativeFn>,
}

impl std::fmt::Debug for NativeFunction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeFunction")
            .field("name", &self.name)
            .finish()
    }
}

impl NativeFunction {
    /// Build a native function with a static name and an `Fn`
    /// payload. Use [`new_rc`] for the common path of producing a
    /// `Value::NativeFunction` directly.
    #[must_use]
    pub fn new<F>(name: &'static str, call: F) -> Self
    where
        F: Fn(&mut Interpreter, &[Value]) -> Result<Value, NativeError> + 'static,
    {
        Self {
            name,
            call: Box::new(call),
        }
    }
}

/// Convenience: produce a `Value::NativeFunction` from a closure.
#[must_use]
pub fn native_value<F>(name: &'static str, call: F) -> Value
where
    F: Fn(&mut Interpreter, &[Value]) -> Result<Value, NativeError> + 'static,
{
    Value::NativeFunction(Rc::new(NativeFunction::new(name, call)))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::number::NumberValue;

    #[test]
    fn native_value_dispatches() {
        let f = native_value("identity", |_, args| {
            Ok(args.first().cloned().unwrap_or(Value::Undefined))
        });
        let Value::NativeFunction(rc) = &f else {
            panic!("expected NativeFunction")
        };
        let mut interp = Interpreter::new();
        let r = (rc.call)(&mut interp, &[Value::Number(NumberValue::from_i32(7))]).unwrap();
        assert_eq!(r.display_string(), "7");
    }

    #[test]
    fn rejects_arity_via_typeerror() {
        let f = native_value("require_one_arg", |_, args| {
            if args.len() != 1 {
                return Err(NativeError::TypeError {
                    name: "require_one_arg",
                    reason: format!("expected 1 arg, got {}", args.len()),
                });
            }
            Ok(args[0].clone())
        });
        let Value::NativeFunction(rc) = &f else {
            panic!()
        };
        let mut interp = Interpreter::new();
        let err = (rc.call)(&mut interp, &[]).unwrap_err();
        assert!(matches!(err, NativeError::TypeError { .. }));
    }
}
