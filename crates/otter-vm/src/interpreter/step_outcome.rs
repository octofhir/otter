//! `StepOutcome`, `Completion`, `TailCallPayload`, `YieldStarResult`, and
//! `ToPrimitiveHint` — enums returned from a single interpreter step.

use crate::frame::RegisterIndex;
use crate::module::Module;
use crate::object::ObjectHandle;
use crate::value::RegisterValue;

use super::Activation;

#[derive(Debug, Clone, PartialEq)]
pub(super) struct TailCallPayload {
    pub(super) module: Module,
    pub(super) activation: Activation,
}

#[derive(Debug, Clone, PartialEq)]
pub(super) enum StepOutcome {
    Continue,
    Return(RegisterValue),
    Throw(RegisterValue),
    /// §14.6 Tail call — replace the current activation with the callee's.
    /// The execution loop swaps module/activation/function in-place instead
    /// of recursing into `run_completion_with_runtime`.
    /// Spec: <https://tc39.es/ecma262/#sec-tail-position-calls>
    TailCall(Box<TailCallPayload>),
    /// The interpreter should suspend at an `await` on a pending promise.
    /// The caller captures the frame state and enqueues a resume job.
    Suspend {
        /// The promise being awaited.
        awaited_promise: ObjectHandle,
        /// The register where the await result should be written on resume.
        resume_register: RegisterIndex,
    },
    /// The generator should yield a value and suspend.
    GeneratorYield {
        /// The value being yielded.
        yielded_value: RegisterValue,
        /// The register where the sent value should be written on resume.
        resume_register: u16,
    },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum Completion {
    Return(RegisterValue),
    Throw(RegisterValue),
}

/// §14.4.4 yield* delegation result — used internally by resume_generator_impl.
pub(super) enum YieldStarResult {
    /// Inner iterator yielded a value — yield it to the outer caller.
    Yield(RegisterValue),
    /// Inner iterator completed — the `yield*` expression evaluates to this value.
    Done(RegisterValue),
    /// Inner iterator completed via `.return()` forwarding — complete the outer generator.
    Return(RegisterValue),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToPrimitiveHint {
    String,
    Number,
}
