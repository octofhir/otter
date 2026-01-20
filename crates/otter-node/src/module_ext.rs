//! Node.js module extension.
//!
//! Provides createRequire and other module-related APIs.

use otter_runtime::Extension;

/// Create the module extension.
pub fn extension() -> Extension {
    Extension::new("module").with_js(include_str!("module.js"))
}
