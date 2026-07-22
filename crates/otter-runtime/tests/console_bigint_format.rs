//! `console` renders a BigInt argument as an inspected value, not a coerced
//! string: the `n` suffix is what distinguishes `1n` from `1` on the wire.
//!
//! Explicit `ToString` conversions must stay unaffected — `String(1n)`,
//! template interpolation, and `Array.prototype.join` all yield bare digits.

use std::sync::{Arc, Mutex};

use otter_runtime::{ConsoleLevel, ConsoleSink, Otter, OtterError, SourceInput};

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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn console_keeps_the_bigint_suffix_without_changing_to_string() -> Result<(), OtterError> {
    let capture = Arc::new(LogCapture::default());
    let otter = Otter::builder()
        .console_sink(capture.clone())
        .build()
        .expect("otter");

    otter
        .handle()
        .eval(SourceInput::from_javascript(
            r#"
            console.log(1n + 2n);
            console.log(0n, -5n, 2n ** 64n);
            console.log(String(3n), `${3n}`, (3n).toString(), 3n + "");
            console.log([1n, 2n].join(","));
            console.log(1, "s", true, null, undefined);
            "#,
        ))
        .await?;

    assert_eq!(
        capture.events.lock().expect("log mutex").clone(),
        vec![
            "3n".to_string(),
            "0n -5n 18446744073709551616n".to_string(),
            "3 3 3 3".to_string(),
            "1,2".to_string(),
            "1 s true null undefined".to_string(),
        ]
    );
    Ok(())
}
