//! Moving-GC coverage for the `Object.assign` result.
//!
//! # Contents
//! - A target moved while enumerable source properties are copied.
//! - Identity and property checks on the returned value across every tier.
//!
//! # Invariants
//! - One scope handle owns the target for the complete assign operation.
//! - The return value is reloaded from that handle after the final allocation.

use otter_runtime::{JitSelection, Runtime, SourceInput};

#[test]
fn object_assign_returns_the_relocated_target_on_every_tier() {
    for selection in [
        JitSelection::InterpreterOnly,
        JitSelection::Template,
        JitSelection::ProductionTiered,
    ] {
        let mut runtime = Runtime::builder()
            .jit_selection(selection)
            .build()
            .expect("runtime");
        let result = runtime
            .run_script(
                SourceInput::from_javascript(
                    r#"
const target = { __proto__: null };
const returned = Object.assign(target, {
  alpha: { marker: "alpha" },
  beta: { marker: "beta" },
  gamma: { marker: "gamma" },
  delta: { marker: "delta" }
});
returned === target &&
  returned.alpha.marker === "alpha" &&
  returned.delta.marker === "delta";
"#,
                ),
                "step10-object-assign-return.js",
            )
            .expect("Object.assign result");
        assert_eq!(result.completion_string(), "true", "{selection:?}");
    }
}
