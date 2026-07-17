//! Moving-GC invariants for async promise-reaction entry.
//!
//! # Contents
//! - An async bytecode reaction that allocates before and after its first
//!   `await`, then publishes its result through the downstream reaction.
//!
//! # Invariants
//! - The top-level reaction's async result promise remains rooted after its
//!   activation parks and while the dispatcher settles the downstream promise.

use std::sync::{Arc, Mutex};

use otter_runtime::{ConsoleLevel, ConsoleSink, Otter};

#[derive(Debug, Default)]
struct LogCapture {
    events: Mutex<Vec<String>>,
}

impl ConsoleSink for LogCapture {
    fn write(&self, level: ConsoleLevel, fields: &[String]) {
        if matches!(level, ConsoleLevel::Log) {
            self.events
                .lock()
                .expect("log mutex")
                .push(fields.join(" "));
        }
    }
}

#[test]
fn async_reaction_result_survives_activation_parking_and_relocation() {
    let capture = Arc::new(LogCapture::default());
    let otter = Otter::builder()
        .console_sink(capture.clone())
        .build()
        .expect("otter build");

    otter
        .blocking_run_typescript(
            r#"
                Promise.resolve("start")
                    .then(async () => {
                        const before = [];
                        for (let i = 0; i < 96; i++) {
                            before.push({ i, text: "before-" + i });
                        }
                        await Promise.resolve("resume");
                        const after = [];
                        for (let i = 0; i < 96; i++) {
                            after.push({ i, text: "after-" + i });
                        }
                        return {
                            marker: "async-root-ok",
                            count: before.length + after.length,
                        };
                    })
                    .then((value) => console.log(value.marker, value.count));
            "#,
        )
        .expect("async reaction fixture");

    assert_eq!(
        capture.events.lock().expect("log mutex").as_slice(),
        ["async-root-ok 192"]
    );
}
