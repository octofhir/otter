//! JIT regression coverage for owner-scoped constant resolution.
//!
//! # Contents
//! - Hot functions whose constant tables differ from their caller.
//!
//! # Invariants
//! - JIT runtime bridges resolve string/property constants against the executing
//!   function's chunk, not the caller or ambient module.
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
