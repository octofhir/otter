//! Process module - Node.js-compatible process object.
//!
//! Security model:
//! - `process.chdir` and `process.exit` require subprocess/process-control capability.
//! - `process.hrtime` requires explicit `hrtime` capability.
//! - `process.exit` is fail-closed: direct host termination is disabled.

use crate::security;
use serde_json::{Value as JsonValue, json};
use std::sync::OnceLock;
use std::time::Instant;

static START_INSTANT: OnceLock<Instant> = OnceLock::new();

fn uptime_seconds() -> f64 {
    let start = START_INSTANT.get_or_init(Instant::now);
    start.elapsed().as_secs_f64()
}

/// Get current working directory.
fn process_cwd(_args: &[JsonValue]) -> Result<JsonValue, String> {
    std::env::current_dir()
        .map(|p| json!(p.to_string_lossy()))
        .map_err(|e| format!("Failed to get cwd: {e}"))
}

/// Change current working directory.
fn process_chdir(args: &[JsonValue]) -> Result<JsonValue, String> {
    let dir = args
        .first()
        .and_then(|v| v.as_str())
        .ok_or("process.chdir requires directory argument")?;

    security::require_subprocess("process.chdir")?;
    std::env::set_current_dir(dir)
        .map_err(|e| format!("ENOENT: no such file or directory, chdir '{dir}': {e}"))?;
    Ok(json!(null))
}

/// Request process termination.
///
/// Direct host termination is intentionally disabled for embedded safety.
fn process_exit(args: &[JsonValue]) -> Result<JsonValue, String> {
    let code = args.first().and_then(|v| v.as_i64()).unwrap_or(0) as i32;
    security::require_subprocess("process.exit")?;
    Err(format!(
        "ProcessExit: code={code}. Host termination is disabled in this runtime."
    ))
}

/// High-resolution time (nanoseconds).
fn process_hrtime(args: &[JsonValue]) -> Result<JsonValue, String> {
    security::require_hrtime("process.hrtime")?;

    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| format!("Time error: {e}"))?;

    // If previous time provided, return difference.
    if let Some(prev) = args.first().and_then(|v| v.as_array())
        && prev.len() >= 2
    {
        let prev_secs = prev[0].as_u64().unwrap_or(0);
        let prev_nanos = prev[1].as_u64().unwrap_or(0);
        let prev_total = prev_secs.saturating_mul(1_000_000_000) + prev_nanos;
        let now_total = now.as_nanos() as u64;

        let diff = now_total.saturating_sub(prev_total);
        let diff_secs = diff / 1_000_000_000;
        let diff_nanos = diff % 1_000_000_000;

        return Ok(json!([diff_secs, diff_nanos]));
    }

    Ok(json!([now.as_secs(), now.subsec_nanos()]))
}

/// Get process ID.
fn process_pid(_args: &[JsonValue]) -> Result<JsonValue, String> {
    Ok(json!(std::process::id()))
}

/// Get platform name (like Node.js `process.platform`).
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

/// Get architecture (like Node.js `process.arch`).
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

/// Get Node.js-like version string.
fn process_version(_args: &[JsonValue]) -> Result<JsonValue, String> {
    Ok(json!("v0.1.0-otter"))
}

/// Best-effort executable path.
fn process_exec_path(_args: &[JsonValue]) -> Result<JsonValue, String> {
    let path = std::env::current_exe()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    Ok(json!(path))
}

/// First argv entry.
fn process_argv0(_args: &[JsonValue]) -> Result<JsonValue, String> {
    Ok(json!(std::env::args().next().unwrap_or_default()))
}

/// Process uptime in seconds.
fn process_uptime(_args: &[JsonValue]) -> Result<JsonValue, String> {
    Ok(json!(uptime_seconds()))
}

/// Node-like memory usage shape.
fn process_memory_usage(_args: &[JsonValue]) -> Result<JsonValue, String> {
    // Runtime-level memory stats are not fully wired yet; keep shape-compatible values.
    Ok(json!({
        "rss": 0_u64,
        "heapTotal": 0_u64,
        "heapUsed": 0_u64,
        "external": 0_u64,
        "arrayBuffers": 0_u64,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use otter_vm_runtime::{CapabilitiesBuilder, CapabilitiesGuard};

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
    fn test_process_hrtime_requires_capability() {
        let err = process_hrtime(&[]).unwrap_err();
        assert!(err.contains("PermissionDenied"));

        let _guard = CapabilitiesGuard::new(CapabilitiesBuilder::new().allow_hrtime().build());
        let result = process_hrtime(&[]).unwrap();
        let arr = result.as_array().unwrap();
        assert_eq!(arr.len(), 2);
    }

    #[test]
    fn test_process_chdir_requires_subprocess() {
        let err = process_chdir(&[json!(".")]).unwrap_err();
        assert!(err.contains("PermissionDenied"));

        let _guard = CapabilitiesGuard::new(CapabilitiesBuilder::new().allow_subprocess().build());
        let result = process_chdir(&[json!(".")]);
        assert!(result.is_ok());
    }

    #[test]
    fn test_process_exit_is_fail_closed() {
        let _guard = CapabilitiesGuard::new(CapabilitiesBuilder::new().allow_subprocess().build());
        let err = process_exit(&[json!(7)]).unwrap_err();
        assert!(err.contains("ProcessExit"));
        assert!(err.contains("disabled"));
    }
}
