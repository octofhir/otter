//! JIT regression coverage for owner-scoped constant resolution.
//!
//! # Contents
//! - Hot functions whose constant tables differ from their caller.
//! - Typed variadic JIT metadata for arrays, closures, and Math calls.
//! - Frameless shared-context JIT-to-JIT method calls.
//! - Store IC misses preserving inherited accessor semantics.
//!
//! # Invariants
//! - JIT runtime bridges resolve string/property constants against the executing
//!   function's chunk, not the caller or ambient module.
//! - A shaped store cache never overrides the receiver's current `[[Set]]`
//!   outcome.
//!
//! # See also
//! - `otter_vm::global_ops`
//! - `otter_vm::property_dispatch`

use otter_runtime::{Runtime, SourceInput};

#[test]
fn hot_global_and_property_loads_use_owner_constants() {
    let mut literals = String::new();
    for i in 0..160 {
        literals.push_str(&format!("const s{i} = \"lit{i}\";\n"));
    }
    let source = format!(
        r#"
        function lookup() {{
            {literals}
            return Uint8Array.prototype.subarray;
        }}

        for (let i = 0; i < 80; i++) {{
            if (typeof lookup() !== "function") throw new Error("bad lookup");
        }}
        "ok";
        "#
    );

    let mut runtime = Runtime::builder().build().expect("runtime");
    let completion = runtime
        .run_script(SourceInput::from_javascript(source), "<jit-constant-owner>")
        .expect("script")
        .completion_string()
        .to_string();
    assert_eq!(completion, "ok");
}

#[test]
fn hot_typed_array_computed_loads_route_to_prototype() {
    let source = r#"
        Uint8Array.prototype.total = function () {
            return this[0] + this[1];
        };

        const bytes = new Uint8Array([20, 22]);
        function lookup(name) {
            return bytes[name]();
        }

        for (let i = 0; i < 80; i++) {
            if (lookup("total") !== 42) throw new Error("bad typed array lookup");
        }
        "ok";
        "#;

    let mut runtime = Runtime::builder().build().expect("runtime");
    let completion = runtime
        .run_script(
            SourceInput::from_javascript(source),
            "<jit-typed-array-computed-load>",
        )
        .expect("script")
        .completion_string()
        .to_string();
    assert_eq!(completion, "ok");
}

#[test]
fn hot_variadic_ops_use_compiled_metadata_without_bytecode_redecode() {
    let source = r#"
        function make(base) {
            let offset = 2;
            return function (input) {
                const values = [base, offset, input];
                return Math.max(values[0] + values[1], values[2]);
            };
        }

        for (let i = 0; i < 160; i++) {
            const closure = make(40);
            if (closure(i % 10) !== 42) throw new Error("bad variadic JIT result");
        }
        "ok";
        "#;

    let mut runtime = Runtime::builder().build().expect("runtime");
    let completion = runtime
        .run_script(SourceInput::from_javascript(source), "<jit-typed-variadic>")
        .expect("script")
        .completion_string()
        .to_string();
    assert_eq!(completion, "ok");
}

#[test]
fn hot_zero_arg_method_uses_rooted_frameless_register_window() {
    let source = r#"
        function Task(value) {
            this.value = value;
        }
        Task.prototype.run = function () {
            return this.value + 1;
        };

        const task = new Task(41);
        function dispatch(receiver) {
            return receiver.run();
        }

        let sum = 0;
        for (let i = 0; i < 400; i++) {
            sum += dispatch(task);
        }
        if (sum !== 16800) throw new Error("bad direct method result");
        "ok";
        "#;

    let mut runtime = Runtime::builder().build().expect("runtime");
    let completion = runtime
        .run_script(
            SourceInput::from_javascript(source),
            "<jit-frameless-method>",
        )
        .expect("script")
        .completion_string()
        .to_string();
    assert_eq!(completion, "ok");
}

#[test]
fn hot_store_property_does_not_bypass_inherited_setter() {
    let source = r#"
        let setterSum = 0;
        const proto = {
            set value(input) {
                setterSum += input;
            },
        };

        function write(input) {
            const object = Object.create(proto);
            object.value = input;
        }

        for (let i = 0; i < 200; i++) write(i);
        if (setterSum !== 19900) throw new Error("store IC bypassed inherited setter");
        "ok";
    "#;

    let mut runtime = Runtime::builder().build().expect("runtime");
    let completion = runtime
        .run_script(SourceInput::from_javascript(source), "<jit-store-setter>")
        .expect("script")
        .completion_string()
        .to_string();
    assert_eq!(completion, "ok");
}

#[test]
fn frameless_native_recursion_respects_stack_limit() {
    let source = r#"
        function recurse(depth) {
            if (depth <= 0) return 0;
            return recurse(depth - 1);
        }
        for (let i = 0; i < 160; i++) recurse(2);
        recurse(100);
        "#;

    let mut runtime = Runtime::builder()
        .max_stack_depth(8)
        .build()
        .expect("runtime");
    let error = runtime
        .run_script(
            SourceInput::from_javascript(source),
            "<jit-frameless-depth>",
        )
        .expect_err("deep native recursion must overflow");
    assert!(
        error
            .to_string()
            .to_ascii_lowercase()
            .contains("maximum call stack size exceeded"),
        "unexpected error: {error}"
    );
}
