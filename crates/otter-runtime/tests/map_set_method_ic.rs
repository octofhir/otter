//! Runtime regression coverage for Map/Set method-call inline caches.
//!
//! # Contents
//! - Hot `Map.prototype` / `Set.prototype` method sites that mutate the
//!   prototype after the IC is installed.
//!
//! # Invariants
//! - A cached collection method call is only a guarded fast path. Prototype
//!   replacement or deletion must miss the IC and fall back to full method
//!   resolution.
//!
//! # See also
//! - `otter_vm::method_ops`
//! - `otter_vm::collections`

use otter_runtime::{Runtime, SourceInput};

#[test]
fn map_method_ic_observes_prototype_replacement() {
    let source = r#"
        const m = new Map();
        m.set("x", 1);

        function read() {
            return m.get("x");
        }

        for (let i = 0; i < 100; i++) {
            if (read() !== 1) throw new Error("bad warmup");
        }

        Map.prototype.get = function () {
            return 42;
        };

        if (read() !== 42) throw new Error("stale Map method IC");
        "ok";
    "#;

    let mut runtime = Runtime::builder().build().expect("runtime");
    let completion = runtime
        .run_script(SourceInput::from_javascript(source), "<map-method-ic>")
        .expect("script")
        .completion_string()
        .to_string();

    assert_eq!(completion, "ok");
}

#[test]
fn set_method_ic_observes_prototype_replacement() {
    let source = r#"
        const s = new Set();
        s.add("x");

        function check() {
            return s.has("x");
        }

        for (let i = 0; i < 100; i++) {
            if (check() !== true) throw new Error("bad warmup");
        }

        Set.prototype.has = function () {
            return "patched";
        };

        if (check() !== "patched") throw new Error("stale Set method IC");
        "ok";
    "#;

    let mut runtime = Runtime::builder().build().expect("runtime");
    let completion = runtime
        .run_script(SourceInput::from_javascript(source), "<set-method-ic>")
        .expect("script")
        .completion_string()
        .to_string();

    assert_eq!(completion, "ok");
}
