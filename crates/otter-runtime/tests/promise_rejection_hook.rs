//! A browser can observe Promise rejection checkpoints without a magic global.

use std::sync::{Arc, Mutex};

use otter_runtime::{NativeCtx, NativeError, PromiseRejectionHook, Runtime, SourceInput, Value};

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<(bool, String)>>>);

impl Capture {
    fn snapshot(&self) -> Vec<(bool, String)> {
        self.0.lock().expect("capture mutex").clone()
    }
}

impl PromiseRejectionHook for Capture {
    fn notify(
        &self,
        ctx: &mut NativeCtx<'_>,
        _promise: Value,
        reason: Value,
        handled: bool,
    ) -> Result<(), NativeError> {
        self.0
            .lock()
            .expect("capture mutex")
            .push((handled, reason.display_string(ctx.heap())));
        Ok(())
    }
}

#[test]
fn hook_receives_unhandled_then_late_handled_notifications() {
    let capture = Capture::default();
    let mut runtime = Runtime::builder()
        .promise_rejection_hook(capture.clone())
        .build()
        .expect("runtime");

    runtime
        .eval(SourceInput::from_javascript(
            "globalThis.rejected = Promise.reject('browser-boom')",
        ))
        .expect("rejection is reported, not thrown from eval");
    assert_eq!(
        capture.snapshot(),
        vec![(false, "browser-boom".to_string())]
    );

    runtime
        .eval(SourceInput::from_javascript("rejected.catch(() => {})"))
        .expect("late handler attaches");
    assert_eq!(
        capture.snapshot(),
        vec![
            (false, "browser-boom".to_string()),
            (true, "browser-boom".to_string()),
        ]
    );
}
