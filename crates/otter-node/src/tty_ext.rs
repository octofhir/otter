//! Node.js tty module extension
//!
//! Provides TTY detection and stream classes.

use otter_runtime::Extension;

/// Create the tty extension
pub fn extension() -> Extension {
    Extension::new("node_tty").with_js(include_str!("tty.js"))
}
