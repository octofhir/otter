//! Scoped preservation of an incoming IteratorClose throw completion.
//!
//! # Contents
//! - The single extent shared by ordinary, helper and close-all throw paths.
//!
//! # Invariants
//! The incoming thrown value is a collector-traced handle. Owned detail and
//! source frames are removed before observable cleanup. Catchable cleanup is
//! suppressed and restores the exact incoming state, including absence. Fatal,
//! control and completed terminal allocation failure keep the cleanup's actual
//! state. Local source errors retain their JavaScript disposition.
//! Normal-close semantics remain in the caller and do not enter this extent.
//!
//! # See also
//! - `super::Interpreter::iterator_close_discarding_completion`.
//! - `super::Interpreter::iterator_zip_close_all`.

use crate::{Interpreter, runtime_activation::CommittedValueError};

impl Interpreter {
    pub(super) fn preserving_iterator_throw_completion(
        &mut self,
        close: impl FnOnce(&mut Self) -> Result<(), CommittedValueError>,
    ) -> Result<(), CommittedValueError> {
        self.with_handle_scope(|interp, scope| {
            let thrown = interp
                .take_pending_uncaught_throw()
                .map(|value| interp.scoped_value(scope, value));
            let detail = interp.take_error_detail();
            let frames = interp.pending_uncaught_frames.take();
            let from_rejection = interp.take_uncaught_from_promise_rejection();
            let result = close(interp);
            if let Err(error @ CommittedValueError::Fatal(_)) = result {
                return Err(error);
            }
            let _ = interp.take_pending_uncaught_throw();
            if let Some(thrown) = thrown {
                interp.set_pending_uncaught_throw(interp.escape_scoped(thrown));
            }
            *interp.pending_error_detail.borrow_mut() = detail;
            interp.pending_uncaught_frames = frames;
            interp.uncaught_from_promise_rejection = from_rejection;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests;
