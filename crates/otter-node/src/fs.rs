//! `node:fs` native core.
//!
//! Provides the capability-gated synchronous primitives that the `fs.js` shim
//! builds the full Node `fs` surface on top of. Raw file bytes cross the
//! native/JS boundary as latin1 strings (each byte maps 1:1 to a code unit), so
//! the JS layer can wrap them in a `Buffer` and apply any encoding.
//!
//! # Contents
//! - [`install_fs_module`] - the legacy ESM namespace (text sync helpers).
//! - [`fs_native_value`] - the raw sync core consumed by `fs.js`.
//!
//! # Invariants
//! - Every operation checks `read`/`write` capabilities at the Rust boundary
//!   before touching the host filesystem.
//! - No VM state is retained across host I/O.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use otter_runtime::module_scope::ModuleScope;
use otter_runtime::{
    CapabilitySet, HostedModuleCtx, HostedNativeCall, RuntimeAttr, RuntimeNativeCtx as NativeCtx,
    RuntimeNativeError as NativeError, RuntimeObjectBuilder, RuntimeValue as Value,
    runtime_arg_to_string, runtime_native_dynamic, runtime_optional_arg_to_string,
};

/// Errors produced by the native `node:fs` core.
#[derive(Debug, thiserror::Error)]
pub enum FsError {
    /// Filesystem permission denied.
    #[error("permission denied for `{path}`")]
    PermissionDenied {
        /// Path that was rejected.
        path: PathBuf,
    },
    /// Filesystem I/O error.
    #[error("{message}")]
    Io {
        /// Path involved in the failed operation.
        path: PathBuf,
        /// Underlying error message.
        message: String,
        /// Node-style error code (e.g. `ENOENT`).
        code: &'static str,
    },
}

/// Result alias for `node:fs`.
pub type FsResult<T> = Result<T, FsError>;

/// Install the legacy `node:fs` / `fs` namespace (text sync helpers). Retained
/// for the ESM install path; the CommonJS surface is built by `fs.js`.
pub fn install_fs_module(ctx: &mut HostedModuleCtx<'_>) -> Result<(), String> {
    let caps = ctx.capabilities().clone();
    let read_file_sync = Arc::new(
        move |ctx: &mut NativeCtx<'_>, args: &[Value], _c: &[Value]| {
            let path = path_arg(ctx, args, 0, "fs.readFileSync")?;
            require_read(&path, &caps).map_err(fs_error)?;
            let encoding = runtime_optional_arg_to_string(args, 1, ctx.heap());
            let bytes = std::fs::read(&path).map_err(|e| fs_error(io_error(&path, &e)))?;
            if encoding.as_deref().is_some_and(|e| !e.is_empty()) {
                crate::string_value(ctx, &String::from_utf8_lossy(&bytes))
            } else {
                crate::string_value(ctx, &bytes_to_latin1(&bytes))
            }
        },
    );
    let caps = ctx.capabilities().clone();
    let write_file_sync = Arc::new(
        move |ctx: &mut NativeCtx<'_>, args: &[Value], _c: &[Value]| {
            let path = path_arg(ctx, args, 0, "fs.writeFileSync")?;
            require_write(&path, &caps).map_err(fs_error)?;
            let data = runtime_arg_to_string(args, 1, ctx.heap());
            std::fs::write(&path, data.as_bytes()).map_err(|e| fs_error(io_error(&path, &e)))?;
            Ok(Value::undefined())
        },
    );
    let caps = ctx.capabilities().clone();
    let exists_sync = Arc::new(move |ctx: &mut NativeCtx<'_>, args: &[Value], _c: &[Value]| {
        let path = path_arg(ctx, args, 0, "fs.existsSync")?;
        Ok(Value::boolean(caps.read.matches_path(&path) && path.exists()))
    });
    let caps = ctx.capabilities().clone();
    let mkdir_sync = Arc::new(move |ctx: &mut NativeCtx<'_>, args: &[Value], _c: &[Value]| {
        let path = path_arg(ctx, args, 0, "fs.mkdirSync")?;
        require_write(&path, &caps).map_err(fs_error)?;
        let recursive = args.get(1).is_some_and(truthy);
        let r = if recursive {
            std::fs::create_dir_all(&path)
        } else {
            std::fs::create_dir(&path)
        };
        r.map_err(|e| fs_error(io_error(&path, &e)))?;
        Ok(Value::undefined())
    });
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

const FS_SHIM: &str = include_str!("fs.js");
const FS_PROMISES_SHIM: &str = "'use strict';\nmodule.exports = require('fs').promises;\n";

/// CommonJS export: the full `fs` namespace, built by `fs.js` on the native core.
pub fn fs_cjs_value(ctx: &mut NativeCtx<'_>, caps: &CapabilitySet) -> Result<Value, String> {
    let native = fs_native_value(ctx, caps)?;
    let buffer = crate::buffer::buffer_cjs_value(ctx, caps)?;
    let stream = crate::stream::stream_cjs_value(ctx, caps)?;
    otter_runtime::run_builtin_cjs_shim(
        ctx,
        "node:fs",
        FS_SHIM,
        &[
            ("__fsnative", native),
            ("buffer", buffer),
            ("stream", stream),
        ],
    )
}

/// CommonJS export: `fs/promises` (= `require('fs').promises`).
pub fn fs_promises_cjs_value(
    ctx: &mut NativeCtx<'_>,
    caps: &CapabilitySet,
) -> Result<Value, String> {
    let fs = fs_cjs_value(ctx, caps)?;
    otter_runtime::run_builtin_cjs_shim(ctx, "node:fs/promises", FS_PROMISES_SHIM, &[("fs", fs)])
}

/// Build the raw synchronous core as a value: `{ readRaw, writeRaw, stat, … }`.
/// Each method captures a clone of the capability set.
pub fn fs_native_value(ctx: &mut NativeCtx<'_>, caps: &CapabilitySet) -> Result<Value, String> {
    let object = otter_runtime::runtime_alloc_object(ctx).map_err(|e| e.to_string())?;
    let mut builder = RuntimeObjectBuilder::from_object(ctx, object);

    macro_rules! m {
        ($name:literal, $len:expr, $f:ident) => {{
            let caps = caps.clone();
            builder
                .method(
                    $name,
                    $len,
                    runtime_native_dynamic(Arc::new(
                        move |ctx: &mut NativeCtx<'_>, args: &[Value], _c: &[Value]| {
                            $f(ctx, args, &caps)
                        },
                    )),
                    RuntimeAttr::builtin_function(),
                )
                .map_err(|e| e.to_string())?;
        }};
    }

    m!("readRaw", 1, fs_read_raw);
    m!("writeRaw", 3, fs_write_raw);
    m!("existsRaw", 1, fs_exists);
    m!("statRaw", 2, fs_stat_raw);
    m!("readdirRaw", 1, fs_readdir_raw);
    m!("readdirTypes", 1, fs_readdir_types);
    m!("mkdir", 2, fs_mkdir);
    m!("rm", 3, fs_rm);
    m!("rmdir", 1, fs_rmdir);
    m!("unlink", 1, fs_unlink);
    m!("realpath", 1, fs_realpath);
    m!("copyFile", 2, fs_copy_file);
    m!("access", 2, fs_access);
    m!("rename", 2, fs_rename);
    m!("readlink", 1, fs_readlink);
    m!("chmod", 2, fs_chmod);
    m!("truncate", 2, fs_truncate);

    Ok(Value::object(builder.build()))
}

// ---- raw byte/latin1 bridging ----

fn truthy(v: &Value) -> bool {
    v.as_boolean().unwrap_or(false)
}

fn bytes_to_latin1(bytes: &[u8]) -> String {
    bytes.iter().map(|&b| b as char).collect()
}

fn latin1_to_bytes(s: &str) -> Vec<u8> {
    s.chars().map(|c| c as u32 as u8).collect()
}

// ---- native operations ----

fn fs_read_raw(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    caps: &CapabilitySet,
) -> Result<Value, NativeError> {
    let path = path_arg(ctx, args, 0, "fs.readFile")?;
    require_read(&path, caps).map_err(fs_error)?;
    let bytes = std::fs::read(&path).map_err(|e| fs_error(io_error(&path, &e)))?;
    crate::string_value(ctx, &bytes_to_latin1(&bytes))
}

fn fs_write_raw(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    caps: &CapabilitySet,
) -> Result<Value, NativeError> {
    let path = path_arg(ctx, args, 0, "fs.writeFile")?;
    require_write(&path, caps).map_err(fs_error)?;
    let data = runtime_arg_to_string(args, 1, ctx.heap());
    let append = args.get(2).is_some_and(truthy);
    let bytes = latin1_to_bytes(&data);
    let result = if append {
        use std::io::Write;
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .and_then(|mut f| f.write_all(&bytes))
    } else {
        std::fs::write(&path, &bytes)
    };
    result.map_err(|e| fs_error(io_error(&path, &e)))?;
    Ok(Value::undefined())
}

fn fs_exists(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    caps: &CapabilitySet,
) -> Result<Value, NativeError> {
    let path = path_arg(ctx, args, 0, "fs.exists")?;
    if !caps.read.matches_path(&path) {
        return Ok(Value::boolean(false));
    }
    Ok(Value::boolean(path.exists()))
}

fn fs_stat_raw(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    caps: &CapabilitySet,
) -> Result<Value, NativeError> {
    let path = path_arg(ctx, args, 0, "fs.stat")?;
    require_read(&path, caps).map_err(fs_error)?;
    let lstat = args.get(1).is_some_and(truthy);
    let meta = if lstat {
        std::fs::symlink_metadata(&path)
    } else {
        std::fs::metadata(&path)
    }
    .map_err(|e| fs_error(io_error(&path, &e)))?;

    let to_ms = |t: Option<std::time::SystemTime>| -> f64 {
        t.and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs_f64() * 1000.0)
            .unwrap_or(0.0)
    };
    let mtime = to_ms(meta.modified().ok());
    let atime = to_ms(meta.accessed().ok());
    let ctime = mtime;
    let birthtime = to_ms(meta.created().ok());
    let file_type = meta.file_type();

    let mut scope = ModuleScope::new(ctx);
    let obj = scope.object().map_err(oom)?;
    scope.set_number(obj, "size", meta.len() as f64);
    scope.set_number(obj, "mode", file_mode(&meta) as f64);
    scope.set_number(obj, "mtimeMs", mtime);
    scope.set_number(obj, "atimeMs", atime);
    scope.set_number(obj, "ctimeMs", ctime);
    scope.set_number(obj, "birthtimeMs", birthtime);
    set_bool(&mut scope, obj, "isFile", file_type.is_file());
    set_bool(&mut scope, obj, "isDirectory", file_type.is_dir());
    set_bool(&mut scope, obj, "isSymbolicLink", file_type.is_symlink());
    let (blksize, blocks, dev, ino, nlink, uid, gid, rdev) = stat_extra(&meta);
    scope.set_number(obj, "blksize", blksize);
    scope.set_number(obj, "blocks", blocks);
    scope.set_number(obj, "dev", dev);
    scope.set_number(obj, "ino", ino);
    scope.set_number(obj, "nlink", nlink);
    scope.set_number(obj, "uid", uid);
    scope.set_number(obj, "gid", gid);
    scope.set_number(obj, "rdev", rdev);
    Ok(scope.finish(obj))
}

fn fs_readdir_raw(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    caps: &CapabilitySet,
) -> Result<Value, NativeError> {
    let path = path_arg(ctx, args, 0, "fs.readdir")?;
    require_read(&path, caps).map_err(fs_error)?;
    let entries = read_dir_names(&path).map_err(|e| fs_error(io_error(&path, &e)))?;
    let mut scope = ModuleScope::new(ctx);
    let mut items = Vec::with_capacity(entries.len());
    for name in &entries {
        items.push(scope.string(name).map_err(oom)?);
    }
    let arr = scope.array(&items).map_err(oom)?;
    Ok(scope.finish(arr))
}

fn fs_readdir_types(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    caps: &CapabilitySet,
) -> Result<Value, NativeError> {
    let path = path_arg(ctx, args, 0, "fs.readdir")?;
    require_read(&path, caps).map_err(fs_error)?;
    let dir = std::fs::read_dir(&path).map_err(|e| fs_error(io_error(&path, &e)))?;
    let mut rows: Vec<(String, bool, bool, bool)> = Vec::new();
    for entry in dir.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let ft = entry.file_type().ok();
        rows.push((
            name,
            ft.as_ref().is_some_and(std::fs::FileType::is_dir),
            ft.as_ref().is_some_and(std::fs::FileType::is_file),
            ft.as_ref().is_some_and(std::fs::FileType::is_symlink),
        ));
    }
    let mut scope = ModuleScope::new(ctx);
    let mut items = Vec::with_capacity(rows.len());
    for (name, is_dir, is_file, is_link) in &rows {
        let row = scope.object().map_err(oom)?;
        scope.set_string(row, "name", name).map_err(oom)?;
        set_bool(&mut scope, row, "isDir", *is_dir);
        set_bool(&mut scope, row, "isFile", *is_file);
        set_bool(&mut scope, row, "isSymlink", *is_link);
        items.push(row);
    }
    let arr = scope.array(&items).map_err(oom)?;
    Ok(scope.finish(arr))
}

fn fs_mkdir(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    caps: &CapabilitySet,
) -> Result<Value, NativeError> {
    let path = path_arg(ctx, args, 0, "fs.mkdir")?;
    require_write(&path, caps).map_err(fs_error)?;
    let recursive = args.get(1).is_some_and(truthy);
    let result = if recursive {
        std::fs::create_dir_all(&path)
    } else {
        std::fs::create_dir(&path)
    };
    result.map_err(|e| fs_error(io_error(&path, &e)))?;
    Ok(Value::undefined())
}

fn fs_rm(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    caps: &CapabilitySet,
) -> Result<Value, NativeError> {
    let path = path_arg(ctx, args, 0, "fs.rm")?;
    require_write(&path, caps).map_err(fs_error)?;
    let recursive = args.get(1).is_some_and(truthy);
    let force = args.get(2).is_some_and(truthy);
    let result = if path.is_dir() {
        if recursive {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_dir(&path)
        }
    } else {
        std::fs::remove_file(&path)
    };
    match result {
        Ok(()) => Ok(Value::undefined()),
        Err(e) if force && e.kind() == std::io::ErrorKind::NotFound => Ok(Value::undefined()),
        Err(e) => Err(fs_error(io_error(&path, &e))),
    }
}

fn fs_rmdir(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    caps: &CapabilitySet,
) -> Result<Value, NativeError> {
    let path = path_arg(ctx, args, 0, "fs.rmdir")?;
    require_write(&path, caps).map_err(fs_error)?;
    let recursive = args.get(1).is_some_and(truthy);
    let result = if recursive {
        std::fs::remove_dir_all(&path)
    } else {
        std::fs::remove_dir(&path)
    };
    result.map_err(|e| fs_error(io_error(&path, &e)))?;
    Ok(Value::undefined())
}

fn fs_unlink(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    caps: &CapabilitySet,
) -> Result<Value, NativeError> {
    let path = path_arg(ctx, args, 0, "fs.unlink")?;
    require_write(&path, caps).map_err(fs_error)?;
    std::fs::remove_file(&path).map_err(|e| fs_error(io_error(&path, &e)))?;
    Ok(Value::undefined())
}

fn fs_realpath(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    caps: &CapabilitySet,
) -> Result<Value, NativeError> {
    let path = path_arg(ctx, args, 0, "fs.realpath")?;
    require_read(&path, caps).map_err(fs_error)?;
    let real = std::fs::canonicalize(&path).map_err(|e| fs_error(io_error(&path, &e)))?;
    crate::string_value(ctx, &real.to_string_lossy())
}

fn fs_copy_file(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    caps: &CapabilitySet,
) -> Result<Value, NativeError> {
    let src = path_arg(ctx, args, 0, "fs.copyFile")?;
    let dest = path_arg(ctx, args, 1, "fs.copyFile")?;
    require_read(&src, caps).map_err(fs_error)?;
    require_write(&dest, caps).map_err(fs_error)?;
    std::fs::copy(&src, &dest).map_err(|e| fs_error(io_error(&src, &e)))?;
    Ok(Value::undefined())
}

fn fs_access(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    caps: &CapabilitySet,
) -> Result<Value, NativeError> {
    let path = path_arg(ctx, args, 0, "fs.access")?;
    require_read(&path, caps).map_err(fs_error)?;
    if path.exists() {
        Ok(Value::undefined())
    } else {
        Err(fs_error(FsError::Io {
            path: path.clone(),
            message: format!(
                "ENOENT: no such file or directory, access '{}'",
                path.display()
            ),
            code: "ENOENT",
        }))
    }
}

fn fs_rename(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    caps: &CapabilitySet,
) -> Result<Value, NativeError> {
    let from = path_arg(ctx, args, 0, "fs.rename")?;
    let to = path_arg(ctx, args, 1, "fs.rename")?;
    require_write(&from, caps).map_err(fs_error)?;
    require_write(&to, caps).map_err(fs_error)?;
    std::fs::rename(&from, &to).map_err(|e| fs_error(io_error(&from, &e)))?;
    Ok(Value::undefined())
}

fn fs_readlink(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    caps: &CapabilitySet,
) -> Result<Value, NativeError> {
    let path = path_arg(ctx, args, 0, "fs.readlink")?;
    require_read(&path, caps).map_err(fs_error)?;
    let target = std::fs::read_link(&path).map_err(|e| fs_error(io_error(&path, &e)))?;
    crate::string_value(ctx, &target.to_string_lossy())
}

fn fs_chmod(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    caps: &CapabilitySet,
) -> Result<Value, NativeError> {
    let path = path_arg(ctx, args, 0, "fs.chmod")?;
    require_write(&path, caps).map_err(fs_error)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = args.get(1).and_then(|v| v.as_f64()).unwrap_or(0.0) as u32;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode))
            .map_err(|e| fs_error(io_error(&path, &e)))?;
    }
    Ok(Value::undefined())
}

fn fs_truncate(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    caps: &CapabilitySet,
) -> Result<Value, NativeError> {
    let path = path_arg(ctx, args, 0, "fs.truncate")?;
    require_write(&path, caps).map_err(fs_error)?;
    let len = args.get(1).and_then(|v| v.as_f64()).unwrap_or(0.0) as u64;
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .map_err(|e| fs_error(io_error(&path, &e)))?;
    file.set_len(len)
        .map_err(|e| fs_error(io_error(&path, &e)))?;
    Ok(Value::undefined())
}

// ---- helpers ----

fn read_dir_names(path: &Path) -> std::io::Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in std::fs::read_dir(path)? {
        names.push(entry?.file_name().to_string_lossy().into_owned());
    }
    Ok(names)
}

fn set_bool(
    scope: &mut ModuleScope<'_, '_>,
    obj: otter_runtime::module_scope::Rooted,
    key: &str,
    b: bool,
) {
    let v = scope.boolean(b);
    scope.set(obj, key, v);
}

#[cfg(unix)]
fn file_mode(meta: &std::fs::Metadata) -> u32 {
    use std::os::unix::fs::MetadataExt;
    meta.mode()
}
#[cfg(not(unix))]
fn file_mode(meta: &std::fs::Metadata) -> u32 {
    if meta.permissions().readonly() {
        0o444
    } else {
        0o666
    }
}

#[cfg(unix)]
#[allow(clippy::type_complexity)]
fn stat_extra(meta: &std::fs::Metadata) -> (f64, f64, f64, f64, f64, f64, f64, f64) {
    use std::os::unix::fs::MetadataExt;
    (
        meta.blksize() as f64,
        meta.blocks() as f64,
        meta.dev() as f64,
        meta.ino() as f64,
        meta.nlink() as f64,
        meta.uid() as f64,
        meta.gid() as f64,
        meta.rdev() as f64,
    )
}
#[cfg(not(unix))]
fn stat_extra(_meta: &std::fs::Metadata) -> (f64, f64, f64, f64, f64, f64, f64, f64) {
    (4096.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0)
}

fn oom(err: String) -> NativeError {
    crate::type_error("fs", err)
}

fn path_arg(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    index: usize,
    name: &'static str,
) -> Result<PathBuf, NativeError> {
    let path = crate::arg_string(args, index, name, ctx.heap())?;
    if path.is_empty() {
        return Err(crate::type_error(name, "path is required"));
    }
    Ok(PathBuf::from(path))
}

fn io_error(path: &Path, err: &std::io::Error) -> FsError {
    let code = match err.kind() {
        std::io::ErrorKind::NotFound => "ENOENT",
        std::io::ErrorKind::PermissionDenied => "EACCES",
        std::io::ErrorKind::AlreadyExists => "EEXIST",
        _ => "EIO",
    };
    FsError::Io {
        path: path.to_path_buf(),
        message: format!("{code}: {err}, '{}'", path.display()),
        code,
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
    let code = match &err {
        FsError::PermissionDenied { .. } => "EACCES",
        FsError::Io { code, .. } => code,
    };
    NativeError::Coded {
        kind: otter_vm::ErrorKind::Error,
        code,
        message: err.to_string(),
    }
}
