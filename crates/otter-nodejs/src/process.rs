//! Process module - Node.js-compatible process object
//!
//! Provides process.env, process.cwd(), process.exit(), etc.
//! Note: process.env uses existing __env_* ops from otter_runtime.

use otter_vm_runtime::extension::{op_sync, Op};
use serde_json::{json, Value as JsonValue};

/// Create process native operations
pub fn process_ops() -> Vec<Op> {
    vec![
        op_sync("__process_cwd", process_cwd),
        op_sync("__process_chdir", process_chdir),
        op_sync("__process_exit", process_exit),
        op_sync("__process_hrtime", process_hrtime),
        op_sync("__process_pid", process_pid),
        op_sync("__process_platform", process_platform),
        op_sync("__process_arch", process_arch),
        op_sync("__process_version", process_version),
    ]
}

/// Get current working directory
fn process_cwd(_args: &[JsonValue]) -> Result<JsonValue, String> {
    std::env::current_dir()
        .map(|p| json!(p.to_string_lossy()))
        .map_err(|e| format!("Failed to get cwd: {}", e))
}

/// Change current working directory
fn process_chdir(args: &[JsonValue]) -> Result<JsonValue, String> {
    let dir = args
        .first()
        .and_then(|v| v.as_str())
        .ok_or("process.chdir requires directory argument")?;

    std::env::set_current_dir(dir)
        .map(|_| json!(null))
        .map_err(|e| format!("ENOENT: no such file or directory, chdir '{}'", e))
}

/// Exit the process with given code
fn process_exit(args: &[JsonValue]) -> Result<JsonValue, String> {
    let code = args.first().and_then(|v| v.as_i64()).unwrap_or(0) as i32;

    // Note: In a sandboxed environment, we may want to throw instead
    std::process::exit(code);
}

/// High-resolution time (nanoseconds)
fn process_hrtime(args: &[JsonValue]) -> Result<JsonValue, String> {
    use std::time::{SystemTime, UNIX_EPOCH};

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| format!("Time error: {}", e))?;

    // If previous time provided, return difference
    if let Some(prev) = args.first().and_then(|v| v.as_array()) {
        if prev.len() >= 2 {
            let prev_secs = prev[0].as_u64().unwrap_or(0);
            let prev_nanos = prev[1].as_u64().unwrap_or(0);
            let prev_total = prev_secs * 1_000_000_000 + prev_nanos;
            let now_total = now.as_nanos() as u64;

            let diff = now_total.saturating_sub(prev_total);
            let diff_secs = diff / 1_000_000_000;
            let diff_nanos = diff % 1_000_000_000;

            return Ok(json!([diff_secs, diff_nanos]));
        }
    }

    Ok(json!([now.as_secs(), now.subsec_nanos()]))
}

/// Get process ID
fn process_pid(_args: &[JsonValue]) -> Result<JsonValue, String> {
    Ok(json!(std::process::id()))
}

/// Get platform name (like Node.js process.platform)
fn process_platform(_args: &[JsonValue]) -> Result<JsonValue, String> {
    let platform = if cfg!(target_os = "windows") {
        "win32"
    } else if cfg!(target_os = "macos") {
        "darwin"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else if cfg!(target_os = "freebsd") {
        "freebsd"
    } else {
        "unknown"
    };
    Ok(json!(platform))
}

/// Get architecture (like Node.js process.arch)
fn process_arch(_args: &[JsonValue]) -> Result<JsonValue, String> {
    let arch = if cfg!(target_arch = "x86_64") {
        "x64"
    } else if cfg!(target_arch = "aarch64") {
        "arm64"
    } else if cfg!(target_arch = "x86") {
        "ia32"
    } else if cfg!(target_arch = "arm") {
        "arm"
    } else {
        "unknown"
    };
    Ok(json!(arch))
}

/// Get Node.js-like version string
fn process_version(_args: &[JsonValue]) -> Result<JsonValue, String> {
    // Return Otter version in Node.js format
    Ok(json!("v0.1.0-otter"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_process_cwd() {
        let result = process_cwd(&[]).unwrap();
        assert!(result.as_str().is_some());
    }

    #[test]
    fn test_process_pid() {
        let result = process_pid(&[]).unwrap();
        assert!(result.as_u64().is_some());
        assert!(result.as_u64().unwrap() > 0);
    }

    #[test]
    fn test_process_platform() {
        let result = process_platform(&[]).unwrap();
        let platform = result.as_str().unwrap();
        assert!(["darwin", "linux", "win32", "freebsd", "unknown"].contains(&platform));
    }

    #[test]
    fn test_process_arch() {
        let result = process_arch(&[]).unwrap();
        let arch = result.as_str().unwrap();
        assert!(["x64", "arm64", "ia32", "arm", "unknown"].contains(&arch));
    }

    #[test]
    fn test_process_hrtime() {
        let result = process_hrtime(&[]).unwrap();
        let arr = result.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert!(arr[0].as_u64().is_some());
        assert!(arr[1].as_u64().is_some());
    }
}
