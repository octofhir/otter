//! Node.js timers module extension
//!
//! Provides node:timers and node:timers/promises exports.

use otter_runtime::Extension;

/// Create the timers extension
pub fn extension() -> Extension {
    Extension::new("node_timers").with_js(include_str!("timers.js"))
}
