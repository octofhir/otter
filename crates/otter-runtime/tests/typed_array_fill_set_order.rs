//! Runtime regression coverage for TypedArray `fill` / `set` ordering.
//!
//! # Contents
//! - `fill` snapshots length before value/start/end coercion.
//! - `set` checks source length against the initial target length.
//!
//! # Invariants
//! - Resizable buffers may change during user coercion without changing
//!   already-captured TypedArray lengths.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-%typedarray%.prototype.fill>
//! - <https://tc39.es/ecma262/#sec-%typedarray%.prototype.set>

use otter_runtime::{Runtime, SourceInput};

fn run(source: &str) -> String {
    let mut rt = Runtime::builder().build().expect("runtime");
    rt.run_script(
        SourceInput::from_javascript(source),
        "<typed-array-fill-set-order>",
    )
    .expect("script")
    .completion_string()
    .to_string()
}

#[test]
fn fill_uses_initial_length_when_value_coercion_grows_buffer() {
    let completion = run(r#"
        const rab = new ArrayBuffer(1, { maxByteLength: 4 });
        const ta = new Int8Array(rab);
        const value = { valueOf() { rab.resize(4); return 123; } };
        ta.fill(value);
        Array.from(ta).join(",");
        "#);
    assert_eq!(completion, "123,0,0,0");
}

#[test]
fn set_array_like_rejects_growth_past_initial_target_length() {
    let completion = run(r#"
        const rab = new ArrayBuffer(4, { maxByteLength: 8 });
        const ta = new Int8Array(rab);
        const source = new Proxy({}, {
            get(_target, prop) {
                if (prop === "length") {
                    rab.resize(6);
                    return 6;
                }
                return 1;
            }
        });
        try {
            ta.set(source);
            "no throw";
        } catch (e) {
            e.name + ":" + Array.from(new Int8Array(rab)).join(",");
        }
        "#);
    assert_eq!(completion, "RangeError:0,0,0,0,0,0");
}

#[test]
fn set_array_like_shrink_after_length_snapshot_drops_oob_writes() {
    let completion = run(r#"
        const rab = new ArrayBuffer(4, { maxByteLength: 8 });
        const ta = new Int8Array(rab, 0, 4);
        for (let i = 0; i < 4; ++i) ta[i] = i * 2;
        const source = new Proxy({}, {
            get(_target, prop) {
                if (prop === "length") {
                    rab.resize(3);
                    return 1;
                }
                return 9;
            }
        });
        ta.set(source);
        Array.from(new Int8Array(rab)).join(",");
        "#);
    assert_eq!(completion, "0,2,4");
}
