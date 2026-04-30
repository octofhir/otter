//! ECMA-262 §27 Generator objects.
//!
//! A generator value carries a paused [`crate::Frame`] plus the
//! bookkeeping needed to resume it: the destination register the
//! current `Op::Yield` paused on (so a follow-up `.next(arg)` can
//! deposit `arg` there), and a `done` flag. Cloning a `JsGenerator`
//! shares the same body — every clone observes the same suspension
//! state.
//!
//! # Contents
//! - [`JsGenerator`] — cheap-to-clone handle.
//! - [`GeneratorBody`] — internal storage.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-generator-objects>
//! - <https://tc39.es/ecma262/#sec-generator.prototype.next>

use std::cell::RefCell;
use std::rc::Rc;

use crate::Frame;

/// Cheap-to-clone generator handle.
#[derive(Debug, Clone)]
pub struct JsGenerator {
    inner: Rc<RefCell<GeneratorBody>>,
}

/// Internal storage. Holds the suspended frame and a few resume
/// hints.
#[derive(Debug)]
pub struct GeneratorBody {
    /// `Some(frame)` when the generator can still resume; `None`
    /// once the body returned (or threw past the top of the call).
    pub frame: Option<Frame>,
    /// Register slot that the most recent `Op::Yield` paused on.
    /// `gen.next(arg)` writes `arg` into this slot before
    /// re-entering the dispatch loop. `0` is a sentinel before
    /// the first yield (the entry resume drops its argument per
    /// §27.5.1.3 step 5).
    pub resume_dst: u16,
    /// `true` once the body has returned, thrown, or had `.return()`
    /// invoked. Subsequent `.next()` calls short-circuit to
    /// `{value: undefined, done: true}`.
    pub done: bool,
    /// Most recent value yielded by the body. Drained by
    /// [`crate::Interpreter::resume_generator`] after each
    /// dispatch turn.
    pub yielded: Option<crate::Value>,
    /// `true` for `async function*` generators. The runtime wraps
    /// each `.next` / `.return` / `.throw` call in a Promise per
    /// §27.6 and routes `Op::Await` inside the body through the
    /// generator's pending-request slot.
    pub is_async: bool,
    /// Pending Promise capability for an in-flight `.next` /
    /// `.return` / `.throw` on an async generator. The body's
    /// `Op::Yield` / completion / unhandled throw settles this
    /// promise before yielding control back to the caller.
    pub pending_request: Option<crate::promise::PromiseCapability>,
}

impl JsGenerator {
    /// Allocate a fresh generator over `frame`.
    #[must_use]
    pub fn new(frame: Frame) -> Self {
        Self {
            inner: Rc::new(RefCell::new(GeneratorBody {
                frame: Some(frame),
                resume_dst: 0,
                done: false,
                yielded: None,
                is_async: false,
                pending_request: None,
            })),
        }
    }

    /// Borrow the body for reads.
    #[must_use]
    pub fn borrow(&self) -> std::cell::Ref<'_, GeneratorBody> {
        self.inner.borrow()
    }

    /// Borrow the body mutably.
    #[must_use]
    pub fn borrow_mut(&self) -> std::cell::RefMut<'_, GeneratorBody> {
        self.inner.borrow_mut()
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
}

impl PartialEq for JsGenerator {
    fn eq(&self, other: &Self) -> bool {
        self.ptr_eq(other)
    }
}
