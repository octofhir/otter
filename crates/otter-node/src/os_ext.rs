//! OS extension module using lazy evaluation.
//!
//! This module provides the node:os extension with lazy-loaded OS information.
//! Values are only computed when accessed from JavaScript, not at startup.
//!
//! ## Architecture
//!
//! - `os.rs` - Rust implementation of OS functions
//! - `os_ext.rs` - Extension creation with lazy ops
//! - `os.js` - JavaScript os module wrapper

use otter_runtime::extension::op_sync;
use otter_runtime::Extension;
use serde_json::json;

use crate::os;

/// Create the os extension with lazy-loaded values.
///
/// This extension provides Node.js-compatible operating system utilities.
/// OS information is loaded lazily via native ops when first accessed.
pub fn extension() -> Extension {
    // Only compute static/cheap values at startup
    let platform = os::platform();
    let arch = os::arch();
    let os_type = os::os_type().as_str();
    let endianness = os::endianness();
    let eol = os::eol();
    let devnull = if cfg!(windows) { "\\\\.\\nul" } else { "/dev/null" };

    // Setup code with only static values - dynamic values fetched via ops
    let setup_js = format!(
        r#"
globalThis.__os_platform = {platform:?};
globalThis.__os_arch = {arch:?};
globalThis.__os_type = {os_type:?};
globalThis.__os_endianness = {endianness:?};
globalThis.__os_eol = {eol:?};
globalThis.__os_devnull = {devnull:?};
"#,
        platform = platform,
        arch = arch,
        os_type = os_type,
        endianness = endianness,
        eol = eol,
        devnull = devnull,
    );

    // Combine setup with module code
    let full_js = format!("{}\n{}", setup_js, include_str!("os.js"));

    Extension::new("os")
        .with_ops(vec![
            op_sync("__otter_os_hostname", |_ctx, _args| {
                Ok(json!(os::hostname()))
            }),
            op_sync("__otter_os_homedir", |_ctx, _args| {
                Ok(json!(os::homedir()))
            }),
            op_sync("__otter_os_tmpdir", |_ctx, _args| {
                Ok(json!(os::tmpdir()))
            }),
            op_sync("__otter_os_release", |_ctx, _args| {
                Ok(json!(os::release()))
            }),
            op_sync("__otter_os_version", |_ctx, _args| {
                Ok(json!(os::version()))
            }),
            op_sync("__otter_os_totalmem", |_ctx, _args| {
                Ok(json!(os::totalmem()))
            }),
            op_sync("__otter_os_freemem", |_ctx, _args| {
                Ok(json!(os::freemem()))
            }),
            op_sync("__otter_os_uptime", |_ctx, _args| {
                Ok(json!(os::uptime()))
            }),
            op_sync("__otter_os_cpus", |_ctx, _args| {
                Ok(json!(os::cpus()))
            }),
            op_sync("__otter_os_loadavg", |_ctx, _args| {
                Ok(json!(os::loadavg()))
            }),
            op_sync("__otter_os_userinfo", |_ctx, _args| {
                Ok(json!(os::userinfo()))
            }),
            op_sync("__otter_os_machine", |_ctx, _args| {
                Ok(json!(os::machine()))
            }),
        ])
        .with_js(&full_js)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extension_creation() {
        let ext = extension();
        assert_eq!(ext.name(), "os");
        assert!(ext.js_code().is_some());
        let js = ext.js_code().unwrap();
        assert!(js.contains("__os_platform"));
        assert!(js.contains("osModule"));
    }
}
