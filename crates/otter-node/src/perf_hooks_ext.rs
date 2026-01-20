//! Node.js perf_hooks module extension.
//!
//! Provides performance measurement APIs compatible with Node.js.

use otter_runtime::Extension;

/// Create the perf_hooks extension.
pub fn extension() -> Extension {
    Extension::new("perf_hooks").with_js(include_str!("perf_hooks.js"))
}
