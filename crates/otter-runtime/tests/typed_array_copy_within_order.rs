//! Runtime regression coverage for TypedArray `copyWithin` ordering.
//!
//! # Contents
//! - Length is snapshotted before target/start/end coercion.
//! - Detachment during coercion is rejected before raw copy.
//!
//! # Invariants
//! - `copyWithin` preserves spec ordering around resizable/detachable buffers.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-%typedarray%.prototype.copywithin>

use otter_runtime::{Runtime, SourceInput};

fn run(source: &str) -> String {
    let mut rt = Runtime::builder().build().expect("runtime");
    rt.run_script(
        SourceInput::from_javascript(source),
        "<typed-array-copy-within-order>",
    )
    .expect("script")
    .completion_string()
    .to_string()
}

#[test]
fn copy_within_uses_initial_length_when_coercion_grows_buffer() {
    let completion = run(r#"
        const rab = new ArrayBuffer(4, { maxByteLength: 8 });
        const ta = new Int8Array(rab);
        for (let i = 0; i < 4; ++i) ta[i] = i;
        const target = { valueOf() { rab.resize(6); ta[4] = 4; ta[5] = 5; return 0; } };
        ta.copyWithin(target, 2);
        Array.from(ta).join(",");
        "#);
    assert_eq!(completion, "2,3,2,3,4,5");
}

#[test]
fn copy_within_rejects_detach_during_start_coercion() {
    let completion = run(r#"
        const ta = new Uint8Array([1, 2, 3, 4]);
        const start = { valueOf() { ta.buffer.transfer(); return 1; } };
        try {
            ta.copyWithin(0, start, 3);
            "no throw";
        } catch (e) {
            e.name;
        }
        "#);
    assert_eq!(completion, "TypeError");
}
