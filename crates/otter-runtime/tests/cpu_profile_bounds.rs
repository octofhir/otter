//! End-to-end bounds for VM CPU-profile capture.
//!
//! # Contents
//! - Deep-stack sampling through the public runtime builder/result surface.
//!
//! # Invariants
//! - A retained sample never owns more than 255 JavaScript frames.
//! - Omitted deep frames are visible in profile telemetry.
//! - Sample and time-delta arrays remain aligned.

use otter_runtime::{JitSelection, Runtime, SourceInput};

#[test]
fn deep_samples_are_bounded_and_report_truncated_frames() {
    let mut runtime = Runtime::builder()
        .jit_selection(JitSelection::InterpreterOnly)
        .max_stack_depth(512)
        .cpu_profile_interval(Some(1))
        .build()
        .expect("runtime");
    let result = runtime
        .run_script(
            SourceInput::from_javascript(
                r#"
function dive(depth) {
  if (depth === 0) return 7;
  return dive(depth - 1);
}
dive(260);
"#,
            ),
            "<cpu-profile-bounds>",
        )
        .expect("deep recursion");
    let profile = result.cpu_profile().expect("profile");

    assert!(!profile.samples.is_empty(), "expected retained samples");
    assert!(
        profile.samples.iter().all(|sample| sample.len() <= 255),
        "sample escaped the frame ceiling"
    );
    assert!(
        profile.truncated_frames > 0,
        "deep samples must report omitted frames"
    );
    assert_eq!(profile.samples.len(), profile.time_deltas_us.len());
}
