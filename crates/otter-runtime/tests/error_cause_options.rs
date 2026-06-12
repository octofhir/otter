//! Runtime regression coverage for `Error` constructor `cause` options.
//!
//! # Contents
//! - `InstallErrorCause` property lookup and descriptor shape.
//! - Observable ordering between message `ToString` and cause access.
//!
//! # Invariants
//! - `options.cause` is read through `HasProperty` then `Get`.
//! - User-thrown values during those operations propagate unchanged.
//! - The installed `cause` property is writable, non-enumerable, and
//!   configurable.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-installerrorcause>

use otter_runtime::{Runtime, SourceInput};

fn run(source: &str) -> String {
    let mut rt = Runtime::builder().build().expect("runtime");
    rt.run_script(
        SourceInput::from_javascript(source),
        "<error-cause-options>",
    )
    .expect("script")
    .completion_string()
    .to_string()
}

#[test]
fn error_constructor_installs_non_enumerable_cause() {
    let completion = run(r#"
        const cause = {};
        const error = new Error("msg", { cause });
        const desc = Object.getOwnPropertyDescriptor(error, "cause");
        String(desc.value === cause) + ":" + desc.writable + ":" +
            desc.enumerable + ":" + desc.configurable;
        "#);
    assert_eq!(completion, "true:true:false:true");
}

#[test]
fn error_constructor_reads_message_before_cause() {
    let completion = run(r#"
        const sequence = [];
        new Error({
            toString() {
                sequence.push("toString");
                return "msg";
            }
        }, {
            get cause() {
                sequence.push("cause");
                return {};
            }
        });
        sequence.join(",");
        "#);
    assert_eq!(completion, "toString,cause");
}

#[test]
fn error_constructor_propagates_abrupt_cause_has() {
    let completion = run(r#"
        const marker = {};
        const options = new Proxy({}, {
            has(_target, key) {
                if (key === "cause") throw marker;
                return false;
            }
        });
        try {
            new Error("msg", options);
            "no throw";
        } catch (e) {
            String(e === marker);
        }
        "#);
    assert_eq!(completion, "true");
}
