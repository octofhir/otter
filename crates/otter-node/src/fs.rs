//! Minimal `node:fs` / `fs` hosted module.
//!
//! # Contents
//! - Synchronous text file helpers for the Node replacement CLI path.
//! - Capability checks for read and write operations.
//!
//! # Invariants
//! - Filesystem capabilities are checked before host I/O.
//! - Host work uses owned path/string data and does not store VM state.
//! - This slice intentionally exposes text-oriented sync helpers only; Buffer,
//!   streams, file descriptors, and async promises belong to later Node API
//!   slices.

use std::path::{Path, PathBuf};

use otter_runtime::{
    CapabilitySet, HostedModuleCtx, HostedNativeCall, RuntimeNativeCtx as NativeCtx,
    RuntimeNativeError as NativeError, RuntimeValue as Value, runtime_optional_arg_to_string,
};

/// Errors produced by the active `node:fs` slice.
#[derive(Debug, thiserror::Error)]
pub enum FsError {
    /// Filesystem permission denied.
    #[error("permission denied for `{path}`")]
    PermissionDenied {
        /// Path that was rejected.
        path: PathBuf,
    },
    /// Unsupported text encoding.
    #[error("unsupported encoding `{0}`")]
    UnsupportedEncoding(String),
    /// Filesystem I/O error.
    #[error("io error for `{path}`: {message}")]
    Io {
        /// Path involved in the failed operation.
        path: PathBuf,
        /// Underlying error message.
        message: String,
    },
}

/// Result alias for `node:fs`.
pub type FsResult<T> = Result<T, FsError>;

/// Install the `node:fs` / `fs` namespace object.
pub fn install_fs_module(ctx: &mut HostedModuleCtx<'_>) -> Result<(), String> {
    let caps = ctx.capabilities().clone();
    let read_file_sync = std::sync::Arc::new(
        move |ctx: &mut NativeCtx<'_>, args: &[Value], _captures: &[Value]| {
            read_file_sync(ctx, args, &caps)
        },
    );
    let caps = ctx.capabilities().clone();
    let write_file_sync = std::sync::Arc::new(
        move |_ctx: &mut NativeCtx<'_>, args: &[Value], _captures: &[Value]| {
            write_file_sync(args, &caps)
        },
    );
    let caps = ctx.capabilities().clone();
    let exists_sync = std::sync::Arc::new(
        move |_ctx: &mut NativeCtx<'_>, args: &[Value], _captures: &[Value]| {
            exists_sync(args, &caps)
        },
    );
    let caps = ctx.capabilities().clone();
    let mkdir_sync = std::sync::Arc::new(
        move |_ctx: &mut NativeCtx<'_>, args: &[Value], _captures: &[Value]| {
            mkdir_sync(args, &caps)
        },
    );

    ctx.method("readFileSync", 2, HostedNativeCall::dynamic(read_file_sync))?
        .method(
            "writeFileSync",
            3,
            HostedNativeCall::dynamic(write_file_sync),
        )?
        .method("existsSync", 1, HostedNativeCall::dynamic(exists_sync))?
        .method("mkdirSync", 2, HostedNativeCall::dynamic(mkdir_sync))?;
    Ok(())
}

fn read_file_sync(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    capabilities: &CapabilitySet,
) -> Result<Value, NativeError> {
    let path = path_arg(args, 0, "fs.readFileSync")?;
    require_read(&path, capabilities).map_err(fs_error)?;
    let encoding = runtime_optional_arg_to_string(args, 1);
    validate_text_encoding(encoding.as_deref()).map_err(fs_error)?;
    let text = std::fs::read_to_string(&path).map_err(|err| {
        fs_error(FsError::Io {
            path: path.clone(),
            message: err.to_string(),
        })
    })?;
    crate::string_value(ctx, &text)
}

fn write_file_sync(args: &[Value], capabilities: &CapabilitySet) -> Result<Value, NativeError> {
    let path = path_arg(args, 0, "fs.writeFileSync")?;
    require_write(&path, capabilities).map_err(fs_error)?;
    let data = crate::arg_string(args, 1, "fs.writeFileSync")?;
    let encoding = runtime_optional_arg_to_string(args, 2);
    validate_text_encoding(encoding.as_deref()).map_err(fs_error)?;
    std::fs::write(&path, data).map_err(|err| {
        fs_error(FsError::Io {
            path: path.clone(),
            message: err.to_string(),
        })
    })?;
    Ok(Value::Undefined)
}

fn exists_sync(args: &[Value], capabilities: &CapabilitySet) -> Result<Value, NativeError> {
    let path = path_arg(args, 0, "fs.existsSync")?;
    if !capabilities.read.matches_path(&path) {
        return Ok(Value::Boolean(false));
    }
    Ok(Value::Boolean(path.exists()))
}

fn mkdir_sync(args: &[Value], capabilities: &CapabilitySet) -> Result<Value, NativeError> {
    let path = path_arg(args, 0, "fs.mkdirSync")?;
    require_write(&path, capabilities).map_err(fs_error)?;
    std::fs::create_dir(&path).map_err(|err| {
        fs_error(FsError::Io {
            path: path.clone(),
            message: err.to_string(),
        })
    })?;
    Ok(Value::Undefined)
}

fn path_arg(args: &[Value], index: usize, name: &'static str) -> Result<PathBuf, NativeError> {
    let path = crate::arg_string(args, index, name)?;
    if path.is_empty() {
        return Err(crate::type_error(name, "path is required"));
    }
    Ok(PathBuf::from(path))
}

fn validate_text_encoding(encoding: Option<&str>) -> FsResult<()> {
    match encoding {
        None | Some("") => Ok(()),
        Some(value) if value.eq_ignore_ascii_case("utf8") => Ok(()),
        Some(value) if value.eq_ignore_ascii_case("utf-8") => Ok(()),
        Some(value) => Err(FsError::UnsupportedEncoding(value.to_string())),
    }
}

fn require_read(path: &Path, capabilities: &CapabilitySet) -> FsResult<()> {
    if capabilities.read.matches_path(path) {
        Ok(())
    } else {
        Err(FsError::PermissionDenied {
            path: path.to_path_buf(),
        })
    }
}

fn require_write(path: &Path, capabilities: &CapabilitySet) -> FsResult<()> {
    if capabilities.write.matches_path(path) {
        Ok(())
    } else {
        Err(FsError::PermissionDenied {
            path: path.to_path_buf(),
        })
    }
}

fn fs_error(err: FsError) -> NativeError {
    crate::type_error("fs", err.to_string())
}
