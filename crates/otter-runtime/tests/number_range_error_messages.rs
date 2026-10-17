//! The `RangeError`s of the `Number.prototype` formatting methods say what
//! V8 and SpiderMonkey's descendants say.
//!
//! Pages tell engines apart by these messages: detectIncognito, which CNN
//! runs, reads the length of `(-1).toFixed(-1)`'s message (51 in Chromium,
//! 44 in Safari, 25 in Firefox) and gives up on an engine it cannot place.

use otter_runtime::{Runtime, SourceInput};

fn run(source: &str) -> String {
    let mut runtime = Runtime::builder().build().expect("runtime");
    runtime
        .run_script(
            SourceInput::from_javascript(source),
            "https://site.test/run.js",
        )
        .expect("script runs")
        .completion_string()
        .to_owned()
}

#[test]
fn formatting_range_errors_read_as_v8_writes_them() {
    let result = run(r#"
        const out = [];
        for (const f of [() => (-1).toFixed(-1), () => (1).toExponential(101), () => (1).toPrecision(0), () => (1).toString(1)]) {
            try { f(); out.push('no error'); } catch (error) { out.push((error instanceof RangeError) + ':' + error.message); }
        }
        out.join('|')
    "#);
    assert_eq!(
        result,
        "true:toFixed() digits argument must be between 0 and 100|\
         true:toExponential() argument must be between 0 and 100|\
         true:toPrecision() argument must be between 1 and 100|\
         true:toString() radix argument must be between 2 and 36"
    );
}
