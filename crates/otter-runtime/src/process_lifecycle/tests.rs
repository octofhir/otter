//! Observable lifecycle lookup and collecting idle-listener regressions.
//!
//! # Contents
//! - Exact getter order, lexical precedence, null/callable guards and TDZ.
//! - Actual listener child relocation followed by timer revival and exit.
//!
//! # Invariants
//! Assertions execute in Rust after the native callback returns. JavaScript
//! operands use the normal call/handle owners; only scalar offsets are retained
//! by the observer. The child is born inside the listener after bootstrap and
//! already owns its marker/self slots before the explicit collection.
//!
//! # See also
//! - `super` implements lifecycle delivery.
//! - Existing `process_timer_teardown` and `fatal_checkpoints` cover teardown.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use crate::{ConsoleLevel, ConsoleSink, Otter, RuntimeExtensionInstaller};
use otter_vm::{NativeCall, NativeCtx, NativeError, Value};

#[derive(Debug, Default)]
struct Capture(Mutex<Vec<String>>);

impl ConsoleSink for Capture {
    fn write(&self, level: ConsoleLevel, fields: &[String]) {
        if matches!(level, ConsoleLevel::Log) {
            self.0
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(fields.join(" "));
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn collecting_before_exit_listener_retains_current_children_and_revives_a_timer() {
    let capture = Arc::new(Capture::default());
    let offsets = Arc::new(Mutex::new(Vec::<u32>::new()));
    let collections = Arc::new(AtomicUsize::new(0));
    let observed = offsets.clone();
    let collected = collections.clone();
    let otter = Otter::builder()
        .console_sink(capture.clone())
        .extension_installer(RuntimeExtensionInstaller::new(move |realm| {
            let observed = observed.clone();
            realm.install_native_global_call(
                "lifecycleOffset",
                1,
                NativeCall::Dynamic(Arc::new(
                    move |_ctx: &mut NativeCtx<'_>,
                          args: &[Value],
                          _captures: &[Value]|
                          -> Result<Value, NativeError> {
                        let child = args
                            .first()
                            .copied()
                            .and_then(Value::as_object)
                            .ok_or_else(|| NativeError::Error {
                                message: "lifecycle observer expected a live child".into(),
                            })?;
                        let offset = child.offset();
                        observed
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .push(offset);
                        Ok(Value::number_i32(offset as i32))
                    },
                )),
            )?;
            let collected = collected.clone();
            realm.install_native_global_call(
                "lifecycleCollect",
                0,
                NativeCall::Dynamic(Arc::new(
                    move |ctx: &mut NativeCtx<'_>,
                          _args: &[Value],
                          _captures: &[Value]|
                          -> Result<Value, NativeError> {
                        ctx.interp_mut().force_gc().map_err(NativeError::from)?;
                        collected.fetch_add(1, Ordering::SeqCst);
                        Ok(Value::undefined())
                    },
                )),
            )?;
            Ok(())
        }))
        .build()
        .expect("collecting lifecycle runtime");
    let result = otter.run_script(r#"
        globalThis.lifecycleMotionChild = undefined;
        let beforeCount = 0;
        process.on('beforeExit', function (code) {
            if (this !== process) throw new Error('beforeExit receiver');
            console.log('before', code);
            if (++beforeCount === 1) {
                const child = { marker: 17, self: null };
                child.self = child;
                const alias = child;
                lifecycleMotionChild = child;
                const before = lifecycleOffset(child);
                lifecycleCollect();
                const after = lifecycleOffset(child);
                if (before === after || child !== alias || child !== lifecycleMotionChild || child.self !== child || child.marker !== 17) {
                    throw new Error('listener child motion or alias');
                }
                process.exitCode = 6;
                setTimeout(() => { console.log('timer'); }, 1);
            }
        });
        process.on('exit', function (code) {
            if (this !== process || lifecycleMotionChild.self !== lifecycleMotionChild || lifecycleMotionChild.marker !== 17) {
                throw new Error('exit receiver or retained child');
            }
            console.log('exit', code);
        });
        'entry';
    "#).await.expect("collecting listener and timer revival complete");
    assert_eq!(result.completion_string(), "entry");
    assert_eq!(result.exit_code(), 6);
    assert_eq!(collections.load(Ordering::SeqCst), 1);
    let offsets = offsets.lock().expect("owned observations");
    assert_eq!(offsets.len(), 2);
    assert_ne!(
        offsets[0], offsets[1],
        "the already-born listener child actually relocated"
    );
    assert_eq!(
        *capture.0.lock().expect("console capture"),
        ["before 0", "timer", "before 6", "exit 6"]
    );
    assert_eq!(otter.activity_stats().pending_ref_timers, 0);
    assert_eq!(otter.activity_stats().pending_unref_timers, 0);
}
