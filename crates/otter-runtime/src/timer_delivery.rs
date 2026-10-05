//! Scoped timer invocation and uncaught reporting in its admitted realm.
//!
//! # Contents
//! - Invocation of the existing TimerEntry callback and arguments.
//! - Scoped restoration of the ambient async value after collecting callbacks.
//! - Pure fatal disposition before the existing process/domain reporter.
//!
//! # Invariants
//! The caller installs TimerEntry.realm_id before invocation. Both the ambient
//! async value and every entry value enter the production handle arena before
//! a call can collect. Reporting occurs in that same extent. Structural/control
//! and escaping OOM never enter an uncaught JavaScript handler; authored native
//! OOM is catchable at ordinary calls and terminates only if it escapes here.
//! No timer, queue, source, exception or cancellation owner is duplicated.
//!
//! # See also
//! - crate::Runtime::fire_timer owns admission, removal and checkpoint entry.
//! - crate::uncaught owns process/domain reporting.

use otter_vm::{Interpreter, NativeCallInfo, NativeCtx, NativeError, TimerEntry};

pub(crate) fn invoke(interp: &mut Interpreter, entry: &TimerEntry) -> Result<(), NativeError> {
    NativeCtx::with_host_context(
        interp,
        NativeCallInfo::default_call(),
        entry.context.as_ref(),
        |ctx| {
            ctx.scope(|mut scope| {
                let origin = scope.value(entry.async_context);
                let callback = scope.value(entry.callback);
                let arguments: Vec<_> = entry.extra_args.iter().map(|v| scope.value(*v)).collect();
                scope.with_async_context(origin, |ctx| {
                    let called = ctx.scope(|mut child| {
                        let receiver = child.undefined();
                        child.call(callback, receiver, &arguments).map(|_| ())
                    });
                    match called {
                        Ok(()) => Ok(()),
                        Err(error)
                            if error.is_fatal()
                                || matches!(error, NativeError::OutOfMemory { .. }) =>
                        {
                            Err(error)
                        }
                        Err(error) => match crate::uncaught::dispatch(ctx) {
                            Ok(true) => Ok(()),
                            Ok(false) => Err(error),
                            Err(handler) => Err(handler),
                        },
                    }
                })
            })
        },
    )
}
