//! CPU-profile coverage of native JIT execution.
//!
//! # Contents
//! - Hot-loop sampling through the public runtime builder/result surface on
//!   both compiled tiers.
//!
//! # Invariants
//! - Compiled back-edge polls retain samples naming the executing source
//!   function, so native execution is not a sampling blind spot.
//! - Retained frames resolve through their owning code chunk, never the
//!   canonical unknown marker.

#![cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]

use otter_runtime::{JitSelection, Runtime, SourceInput};

const HOT_LOOP: &str = r#"
function hotLoop(n) {
  let s = 0;
  for (let i = 0; i < n; i++) {
    s += i % 7;
  }
  return s;
}
hotLoop(4000000);
"#;

#[test]
fn compiled_execution_is_sampled_by_source_name() {
    for selection in [JitSelection::ProductionTiered, JitSelection::Template] {
        let mut runtime = Runtime::builder()
            .jit_selection(selection)
            .cpu_profile_interval(Some(200))
            .build()
            .expect("runtime");
        let result = runtime
            .run_script(
                SourceInput::from_javascript(HOT_LOOP),
                "<cpu-profile-native>",
            )
            .expect("hot loop");
        let profile = result.cpu_profile().expect("profile");
        let hot_leaves = profile
            .samples
            .iter()
            .filter(|sample| {
                sample
                    .first()
                    .is_some_and(|frame| frame.function_name == "hotLoop")
            })
            .count();
        assert!(
            hot_leaves >= 3,
            "{selection:?}: expected samples naming hotLoop, got {hot_leaves} of {}",
            profile.samples.len()
        );
        assert!(
            profile.samples.iter().all(|sample| sample
                .iter()
                .all(|frame| frame.function_name != "<unknown>")),
            "{selection:?}: every sampled frame resolves a source name"
        );
        assert_eq!(profile.samples.len(), profile.time_deltas_us.len());
    }
}
