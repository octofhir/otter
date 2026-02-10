//! Centralized capability enforcement for Node-compatible native ops.
//!
//! All checks are fail-closed: missing capability context means "deny".

use otter_vm_runtime::capabilities_context;
use std::path::{Path, PathBuf};

const ALLOW_READ_HINT: &str = "Use --allow-read to grant file system read access";
const ALLOW_WRITE_HINT: &str = "Use --allow-write to grant file system write access";
const ALLOW_RUN_HINT: &str = "Use --allow-run to grant subprocess/process-control access";
const ALLOW_HRTIME_HINT: &str = "Use --allow-hrtime to grant high-resolution timers";

fn to_absolute(path: &str) -> PathBuf {
    let p = Path::new(path);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(p)
    }
}

fn normalize_for_capability(path: &str) -> PathBuf {
    let absolute = to_absolute(path);

    if let Ok(canonical) = dunce::canonicalize(&absolute) {
        return canonical;
    }

    if let Some(parent) = absolute.parent()
        && let Ok(canonical_parent) = dunce::canonicalize(parent)
        && let Some(file_name) = absolute.file_name()
    {
        return canonical_parent.join(file_name);
    }

    absolute
}

fn permission_denied(capability: &str, resource: &str, hint: &str) -> String {
    format!(
        "PermissionDenied: {} access to '{}'. {}",
        capability, resource, hint
    )
}

fn with_capabilities_bool<F>(check: F) -> bool
where
    F: FnOnce(&otter_vm_runtime::Capabilities) -> bool,
{
    capabilities_context::with_capabilities(|caps| Some(check(caps))).unwrap_or(false)
}

pub(crate) fn require_fs_read(path: &str) -> Result<(), String> {
    let normalized = normalize_for_capability(path);
    if with_capabilities_bool(|caps| caps.can_read(&normalized)) {
        Ok(())
    } else {
        Err(permission_denied("read", path, ALLOW_READ_HINT))
    }
}

pub(crate) fn require_fs_write(path: &str) -> Result<(), String> {
    let normalized = normalize_for_capability(path);
    if with_capabilities_bool(|caps| caps.can_write(&normalized)) {
        Ok(())
    } else {
        Err(permission_denied("write", path, ALLOW_WRITE_HINT))
    }
}

pub(crate) fn require_subprocess(op: &str) -> Result<(), String> {
    if with_capabilities_bool(|caps| caps.can_subprocess()) {
        Ok(())
    } else {
        Err(permission_denied("run", op, ALLOW_RUN_HINT))
    }
}

pub(crate) fn require_hrtime(op: &str) -> Result<(), String> {
    if with_capabilities_bool(|caps| caps.can_hrtime()) {
        Ok(())
    } else {
        Err(permission_denied("hrtime", op, ALLOW_HRTIME_HINT))
    }
}
